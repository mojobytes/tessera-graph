// Copyright 2026 BelowZero Security OU. All rights reserved.

//! `SecureGraph` — LBAC enforcement wrapper over any `GraphAccess` implementation.

use std::collections::HashSet;
use tessera_auth::lbac::{Clearance, SecurityLabel, SecurityPolicy};
use tessera_graph::{Edge, EdgeId, Error, GraphAccess, Node, NodeId, Properties};

/// Shared pure filtering helpers used by both `SecureGraph` and `SecureGraphRef`.
pub mod filter {
    use tessera_auth::lbac::{Clearance, SecurityPolicy};
    use tessera_graph::{Edge, GraphAccess, Node, Properties};

    /// Returns `true` iff `clearance` dominates the security label encoded in `props`.
    #[must_use]
    pub fn can_read_props(clearance: &Clearance, props: &Properties) -> bool {
        let label = SecurityPolicy::extract_label(props);
        clearance.dominates(&label)
    }

    /// Return a copy of `node` with all reserved security properties removed.
    #[must_use]
    pub fn strip_node(mut node: Node) -> Node {
        SecurityPolicy::strip_security_properties(node.properties_mut());
        node
    }

    /// Return a copy of `edge` with all reserved security properties removed.
    #[must_use]
    pub fn strip_edge(mut edge: Edge) -> Edge {
        SecurityPolicy::strip_security_properties(edge.properties_mut());
        edge
    }

    /// Returns `true` iff edge and both endpoint nodes are visible to `clearance`.
    ///
    /// Checks: edge label dominated AND src node label dominated AND tgt node label
    /// dominated. Any missing node or graph error is treated as invisible (fail-safe).
    #[must_use]
    pub fn edge_visible_for<G: GraphAccess>(
        graph: &G,
        clearance: &Clearance,
        edge: &Edge,
    ) -> bool {
        if !can_read_props(clearance, edge.properties()) {
            return false;
        }
        let src_ok = graph
            .node(edge.source())
            .map(|n| can_read_props(clearance, n.properties()))
            .unwrap_or(false);
        let tgt_ok = graph
            .node(edge.target())
            .map(|n| can_read_props(clearance, n.properties()))
            .unwrap_or(false);
        src_ok && tgt_ok
    }
}

/// A security-enforcing wrapper over any `GraphAccess` implementation.
///
/// All read operations filter results by the caller's `Clearance`.
/// All write operations enforce that the caller can write to the
/// target resource's security label.
/// Security properties are stripped from all returned nodes and edges.
///
/// # Fail-safe
///
/// Any error during clearance extraction results in denial (the resource
/// is treated as if the clearance check failed).
pub struct SecureGraph<'g, G: GraphAccess> {
    inner: &'g mut G,
    clearance: Clearance,
}

impl<'g, G: GraphAccess> SecureGraph<'g, G> {
    /// Create a new `SecureGraph` wrapping `inner` with the given `clearance`.
    pub const fn new(inner: &'g mut G, clearance: Clearance) -> Self {
        Self { inner, clearance }
    }

    /// Create a node with an explicit security label.
    ///
    /// # Errors
    ///
    /// Returns `Error::GqlMutationError` if the caller's clearance does not dominate
    /// the requested label.
    pub fn add_node_with_label(
        &mut self,
        graph_label: &str,
        mut properties: Properties,
        security_label: &SecurityLabel,
    ) -> tessera_graph::Result<NodeId> {
        if !self.clearance.dominates(security_label) {
            return Err(Error::GqlMutationError(
                "insufficient clearance to create resource with requested security label"
                    .to_string(),
            ));
        }
        // Strip any user-supplied security properties and inject the explicit label
        SecurityPolicy::strip_security_properties(&mut properties);
        SecurityPolicy::inject_label(&mut properties, security_label);
        self.inner.add_node(graph_label, properties)
    }

    /// Create an edge with an explicit security label.
    ///
    /// # Errors
    ///
    /// Returns `Error::GqlMutationError` if the caller's clearance does not dominate
    /// the requested label, or if either endpoint is not visible.
    pub fn add_edge_with_label(
        &mut self,
        label: &str,
        source: NodeId,
        target: NodeId,
        mut properties: Properties,
        security_label: &SecurityLabel,
    ) -> tessera_graph::Result<EdgeId> {
        if !self.clearance.dominates(security_label) {
            return Err(Error::GqlMutationError(
                "insufficient clearance to create edge with requested security label".to_string(),
            ));
        }
        // Verify both endpoints are visible to the caller
        let src_node = self.inner.node(source)?;
        if !filter::can_read_props(&self.clearance, src_node.properties()) {
            return Err(Error::NodeNotFound(source));
        }
        let tgt_node = self.inner.node(target)?;
        if !filter::can_read_props(&self.clearance, tgt_node.properties()) {
            return Err(Error::NodeNotFound(target));
        }
        SecurityPolicy::strip_security_properties(&mut properties);
        SecurityPolicy::inject_label(&mut properties, security_label);
        self.inner.add_edge(label, source, target, properties)
    }
}

impl<G: GraphAccess> GraphAccess for SecureGraph<'_, G> {
    fn node_ids(&self) -> Vec<NodeId> {
        self.inner
            .node_ids()
            .into_iter()
            .filter(|&id| {
                self.inner
                    .node(id)
                    .map(|n| filter::can_read_props(&self.clearance, n.properties()))
                    .unwrap_or(false)
            })
            .collect()
    }

    fn nodes_by_label(&self, label: &str) -> Vec<NodeId> {
        self.inner
            .nodes_by_label(label)
            .into_iter()
            .filter(|&id| {
                self.inner
                    .node(id)
                    .map(|n| filter::can_read_props(&self.clearance, n.properties()))
                    .unwrap_or(false)
            })
            .collect()
    }

    fn node(&self, id: NodeId) -> tessera_graph::Result<Node> {
        let node = self.inner.node(id)?;
        if filter::can_read_props(&self.clearance, node.properties()) {
            Ok(filter::strip_node(node))
        } else {
            Err(Error::NodeNotFound(id))
        }
    }

    fn node_exists(&self, id: NodeId) -> bool {
        self.inner
            .node(id)
            .map(|n| filter::can_read_props(&self.clearance, n.properties()))
            .unwrap_or(false)
    }

    fn node_count(&self) -> usize {
        self.node_ids().len()
    }

    fn edges_by_label(&self, label: &str) -> Vec<EdgeId> {
        self.inner
            .edges_by_label(label)
            .into_iter()
            .filter(|&id| {
                self.inner
                    .edge(id)
                    .map(|e| filter::edge_visible_for(self.inner, &self.clearance, &e))
                    .unwrap_or(false)
            })
            .collect()
    }

    fn edge(&self, id: EdgeId) -> tessera_graph::Result<Edge> {
        let edge = self.inner.edge(id)?;
        if filter::edge_visible_for(self.inner, &self.clearance, &edge) {
            Ok(filter::strip_edge(edge))
        } else {
            Err(Error::EdgeNotFound(id))
        }
    }

    fn edge_count(&self) -> usize {
        // GraphAccess has no all_edge_ids(); scan visible nodes and collect unique edges.
        let mut seen = HashSet::new();
        for &nid in &self.node_ids() {
            if let Ok(edges) = self.inner.outgoing_edges(nid) {
                for e in edges {
                    if filter::edge_visible_for(self.inner, &self.clearance, &e) {
                        seen.insert(e.id());
                    }
                }
            }
        }
        seen.len()
    }

    fn outgoing_edges(&self, node: NodeId) -> tessera_graph::Result<Vec<Edge>> {
        let node_val = self.inner.node(node)?;
        if !filter::can_read_props(&self.clearance, node_val.properties()) {
            return Err(Error::NodeNotFound(node));
        }
        let edges = self.inner.outgoing_edges(node)?;
        Ok(edges
            .into_iter()
            .filter(|e| filter::edge_visible_for(self.inner, &self.clearance, e))
            .map(filter::strip_edge)
            .collect())
    }

    fn incoming_edges(&self, node: NodeId) -> tessera_graph::Result<Vec<Edge>> {
        let node_val = self.inner.node(node)?;
        if !filter::can_read_props(&self.clearance, node_val.properties()) {
            return Err(Error::NodeNotFound(node));
        }
        let edges = self.inner.incoming_edges(node)?;
        Ok(edges
            .into_iter()
            .filter(|e| filter::edge_visible_for(self.inner, &self.clearance, e))
            .map(filter::strip_edge)
            .collect())
    }

    // --- Mutations ---

    fn add_node(
        &mut self,
        label: &str,
        mut properties: Properties,
    ) -> tessera_graph::Result<NodeId> {
        // Strip any user-supplied security properties; inject default label (public)
        SecurityPolicy::strip_security_properties(&mut properties);
        SecurityPolicy::inject_label(&mut properties, &SecurityLabel::default());
        self.inner.add_node(label, properties)
    }

    fn update_node(&mut self, id: NodeId, node: &Node) -> tessera_graph::Result<()> {
        // Verify caller can see the existing node
        let existing = self.inner.node(id)?;
        if !filter::can_read_props(&self.clearance, existing.properties()) {
            return Err(Error::NodeNotFound(id));
        }
        // Preserve the existing security label (user cannot change it via update_node)
        let existing_label = SecurityPolicy::extract_label(existing.properties());
        let mut updated = node.clone();
        SecurityPolicy::strip_security_properties(updated.properties_mut());
        SecurityPolicy::inject_label(updated.properties_mut(), &existing_label);
        self.inner.update_node(id, &updated)
    }

    fn remove_node(&mut self, id: NodeId) -> tessera_graph::Result<Node> {
        let existing = self.inner.node(id)?;
        if !filter::can_read_props(&self.clearance, existing.properties()) {
            return Err(Error::NodeNotFound(id));
        }
        self.inner.remove_node(id).map(filter::strip_node)
    }

    fn add_edge(
        &mut self,
        label: &str,
        source: NodeId,
        target: NodeId,
        mut properties: Properties,
    ) -> tessera_graph::Result<EdgeId> {
        // Verify both endpoints are visible
        let src_node = self.inner.node(source)?;
        if !filter::can_read_props(&self.clearance, src_node.properties()) {
            return Err(Error::NodeNotFound(source));
        }
        let tgt_node = self.inner.node(target)?;
        if !filter::can_read_props(&self.clearance, tgt_node.properties()) {
            return Err(Error::NodeNotFound(target));
        }
        // Strip user-supplied security properties; inject default label (public)
        SecurityPolicy::strip_security_properties(&mut properties);
        SecurityPolicy::inject_label(&mut properties, &SecurityLabel::default());
        self.inner.add_edge(label, source, target, properties)
    }

    fn update_edge(&mut self, id: EdgeId, edge: &Edge) -> tessera_graph::Result<()> {
        let existing = self.inner.edge(id)?;
        if !filter::edge_visible_for(self.inner, &self.clearance, &existing) {
            return Err(Error::EdgeNotFound(id));
        }
        // Preserve existing security label
        let existing_label = SecurityPolicy::extract_label(existing.properties());
        let mut updated = edge.clone();
        SecurityPolicy::strip_security_properties(updated.properties_mut());
        SecurityPolicy::inject_label(updated.properties_mut(), &existing_label);
        self.inner.update_edge(id, &updated)
    }

    fn remove_edge(&mut self, id: EdgeId) -> tessera_graph::Result<Edge> {
        let existing = self.inner.edge(id)?;
        if !filter::edge_visible_for(self.inner, &self.clearance, &existing) {
            return Err(Error::EdgeNotFound(id));
        }
        self.inner.remove_edge(id).map(filter::strip_edge)
    }
}

/// A read-only security-enforcing wrapper over any `GraphAccess` implementation.
///
/// Holds an immutable borrow `&'g G`, allowing use with `RwLockReadGuard<Graph>`.
/// All read operations filter results by the caller's `Clearance`.
/// All mutation methods return `Error::GqlMutationError` — this wrapper is read-only.
///
/// # Fail-safe
///
/// Any error during clearance extraction results in denial.
pub struct SecureGraphRef<'g, G: GraphAccess> {
    inner: &'g G,
    clearance: Clearance,
}

impl<'g, G: GraphAccess> SecureGraphRef<'g, G> {
    /// Create a new read-only `SecureGraphRef` wrapping an immutable borrow of `inner`
    /// with the given `clearance`.
    pub const fn new(inner: &'g G, clearance: Clearance) -> Self {
        Self { inner, clearance }
    }
}

/// Error message for all mutation attempts on a read-only secure graph.
const READ_ONLY_ERROR: &str = "read-only secure graph: mutations are not permitted";

impl<G: GraphAccess> GraphAccess for SecureGraphRef<'_, G> {
    fn node_ids(&self) -> Vec<NodeId> {
        self.inner
            .node_ids()
            .into_iter()
            .filter(|&id| {
                self.inner
                    .node(id)
                    .map(|n| filter::can_read_props(&self.clearance, n.properties()))
                    .unwrap_or(false)
            })
            .collect()
    }

    fn nodes_by_label(&self, label: &str) -> Vec<NodeId> {
        self.inner
            .nodes_by_label(label)
            .into_iter()
            .filter(|&id| {
                self.inner
                    .node(id)
                    .map(|n| filter::can_read_props(&self.clearance, n.properties()))
                    .unwrap_or(false)
            })
            .collect()
    }

    fn node(&self, id: NodeId) -> tessera_graph::Result<Node> {
        let node = self.inner.node(id)?;
        if filter::can_read_props(&self.clearance, node.properties()) {
            Ok(filter::strip_node(node))
        } else {
            Err(Error::NodeNotFound(id))
        }
    }

    fn node_exists(&self, id: NodeId) -> bool {
        self.inner
            .node(id)
            .map(|n| filter::can_read_props(&self.clearance, n.properties()))
            .unwrap_or(false)
    }

    fn node_count(&self) -> usize {
        self.node_ids().len()
    }

    fn edges_by_label(&self, label: &str) -> Vec<EdgeId> {
        self.inner
            .edges_by_label(label)
            .into_iter()
            .filter(|&id| {
                self.inner
                    .edge(id)
                    .map(|e| filter::edge_visible_for(self.inner, &self.clearance, &e))
                    .unwrap_or(false)
            })
            .collect()
    }

    fn edge(&self, id: EdgeId) -> tessera_graph::Result<Edge> {
        let edge = self.inner.edge(id)?;
        if filter::edge_visible_for(self.inner, &self.clearance, &edge) {
            Ok(filter::strip_edge(edge))
        } else {
            Err(Error::EdgeNotFound(id))
        }
    }

    fn edge_count(&self) -> usize {
        let mut seen = HashSet::new();
        for &nid in &self.node_ids() {
            if let Ok(edges) = self.inner.outgoing_edges(nid) {
                for e in edges {
                    if filter::edge_visible_for(self.inner, &self.clearance, &e) {
                        seen.insert(e.id());
                    }
                }
            }
        }
        seen.len()
    }

    fn outgoing_edges(&self, node: NodeId) -> tessera_graph::Result<Vec<Edge>> {
        let node_val = self.inner.node(node)?;
        if !filter::can_read_props(&self.clearance, node_val.properties()) {
            return Err(Error::NodeNotFound(node));
        }
        let edges = self.inner.outgoing_edges(node)?;
        Ok(edges
            .into_iter()
            .filter(|e| filter::edge_visible_for(self.inner, &self.clearance, e))
            .map(filter::strip_edge)
            .collect())
    }

    fn incoming_edges(&self, node: NodeId) -> tessera_graph::Result<Vec<Edge>> {
        let node_val = self.inner.node(node)?;
        if !filter::can_read_props(&self.clearance, node_val.properties()) {
            return Err(Error::NodeNotFound(node));
        }
        let edges = self.inner.incoming_edges(node)?;
        Ok(edges
            .into_iter()
            .filter(|e| filter::edge_visible_for(self.inner, &self.clearance, e))
            .map(filter::strip_edge)
            .collect())
    }

    // --- Mutations — always denied on a read-only wrapper ---

    fn add_node(
        &mut self,
        _label: &str,
        _properties: Properties,
    ) -> tessera_graph::Result<NodeId> {
        Err(Error::GqlMutationError(READ_ONLY_ERROR.to_string()))
    }

    fn update_node(&mut self, _id: NodeId, _node: &Node) -> tessera_graph::Result<()> {
        Err(Error::GqlMutationError(READ_ONLY_ERROR.to_string()))
    }

    fn remove_node(&mut self, _id: NodeId) -> tessera_graph::Result<Node> {
        Err(Error::GqlMutationError(READ_ONLY_ERROR.to_string()))
    }

    fn add_edge(
        &mut self,
        _label: &str,
        _source: NodeId,
        _target: NodeId,
        _properties: Properties,
    ) -> tessera_graph::Result<EdgeId> {
        Err(Error::GqlMutationError(READ_ONLY_ERROR.to_string()))
    }

    fn update_edge(&mut self, _id: EdgeId, _edge: &Edge) -> tessera_graph::Result<()> {
        Err(Error::GqlMutationError(READ_ONLY_ERROR.to_string()))
    }

    fn remove_edge(&mut self, _id: EdgeId) -> tessera_graph::Result<Edge> {
        Err(Error::GqlMutationError(READ_ONLY_ERROR.to_string()))
    }
}
