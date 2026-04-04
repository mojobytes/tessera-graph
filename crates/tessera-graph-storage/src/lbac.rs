// Copyright 2026 BelowZero Security OU. All rights reserved.

//! `SecureGraph` — LBAC enforcement wrapper over any `GraphAccess` implementation.

use tessera_graph_auth::lbac::{Clearance, SecurityLabel, SecurityPolicy};
use tessera_graph::{Edge, EdgeId, Error, GraphAccess, Node, NodeId, Properties};

/// Shared pure filtering helpers used by both `SecureGraph` and `SecureGraphRef`.
///
/// # Visibility note
///
/// This module is `pub` rather than `pub(crate)` only because the project
/// convention places integration tests in `crates/<crate>/tests/`, which are
/// external crates and cannot access `pub(crate)` items. These functions are
/// internal implementation helpers and are **not part of the public API**.
/// Do not depend on them from outside this crate.
pub mod filter {
    use tessera_graph_auth::lbac::{Clearance, SecurityPolicy};
    use tessera_graph::{Edge, EdgeId, GraphAccess, Node, NodeId, Properties};

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
    pub fn edge_visible_for<G: GraphAccess>(graph: &G, clearance: &Clearance, edge: &Edge) -> bool {
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

    // --- Shared read implementations used by both SecureGraph and SecureGraphRef ---

    #[must_use]
    pub fn secure_node_ids<G: GraphAccess>(inner: &G, clearance: &Clearance) -> Vec<NodeId> {
        inner
            .node_ids()
            .into_iter()
            .filter(|&id| {
                inner
                    .node(id)
                    .map(|n| can_read_props(clearance, n.properties()))
                    .unwrap_or(false)
            })
            .collect()
    }

    #[must_use]
    pub fn secure_nodes_by_label<G: GraphAccess>(
        inner: &G,
        clearance: &Clearance,
        label: &str,
    ) -> Vec<NodeId> {
        inner
            .nodes_by_label(label)
            .into_iter()
            .filter(|&id| {
                inner
                    .node(id)
                    .map(|n| can_read_props(clearance, n.properties()))
                    .unwrap_or(false)
            })
            .collect()
    }

    /// Returns the node at `id` if the caller's clearance dominates its label.
    ///
    /// # Errors
    ///
    /// Returns [`tessera_graph::Error::NodeNotFound`] if the node does not exist
    /// or the caller lacks sufficient clearance.
    pub fn secure_node<G: GraphAccess>(
        inner: &G,
        clearance: &Clearance,
        id: NodeId,
    ) -> tessera_graph::Result<Node> {
        let node = inner.node(id)?;
        if can_read_props(clearance, node.properties()) {
            Ok(strip_node(node))
        } else {
            Err(tessera_graph::Error::NodeNotFound(id))
        }
    }

    /// Returns a projected node (only `keys` properties) if the caller's
    /// clearance dominates its security label.
    ///
    /// The full node is always fetched first so the security label is
    /// available for the clearance check, regardless of which `keys` were
    /// requested. The in-memory `Graph` has no I/O distinction; in a
    /// page-level storage backend the page containing the security
    /// properties is loaded, but this is the minimum required to enforce
    /// the access control decision.
    ///
    /// Keys that do not exist in the node's properties are silently ignored.
    ///
    /// # Errors
    ///
    /// Returns [`tessera_graph::Error::NodeNotFound`] if the node does not
    /// exist or the caller lacks sufficient clearance.
    pub fn secure_node_projected<G: GraphAccess>(
        inner: &G,
        clearance: &Clearance,
        id: NodeId,
        keys: &[&str],
    ) -> tessera_graph::Result<Node> {
        let mut node = secure_node(inner, clearance, id)?;
        // `secure_node` already stripped security properties. Now project.
        let key_set: std::collections::HashSet<&str> = keys.iter().copied().collect();
        node.properties_mut()
            .retain(|k, _| key_set.contains(k.as_str()));
        Ok(node)
    }

    #[must_use]
    pub fn secure_node_exists<G: GraphAccess>(
        inner: &G,
        clearance: &Clearance,
        id: NodeId,
    ) -> bool {
        inner
            .node(id)
            .map(|n| can_read_props(clearance, n.properties()))
            .unwrap_or(false)
    }

    #[must_use]
    pub fn secure_edges_by_label<G: GraphAccess>(
        inner: &G,
        clearance: &Clearance,
        label: &str,
    ) -> Vec<EdgeId> {
        inner
            .edges_by_label(label)
            .into_iter()
            .filter(|&id| {
                inner
                    .edge(id)
                    .map(|e| edge_visible_for(inner, clearance, &e))
                    .unwrap_or(false)
            })
            .collect()
    }

    /// Returns the edge at `id` if both endpoints are visible to the caller.
    ///
    /// # Errors
    ///
    /// Returns [`tessera_graph::Error::EdgeNotFound`] if the edge does not exist
    /// or the caller lacks sufficient clearance for either endpoint.
    pub fn secure_edge<G: GraphAccess>(
        inner: &G,
        clearance: &Clearance,
        id: EdgeId,
    ) -> tessera_graph::Result<Edge> {
        let edge = inner.edge(id)?;
        if edge_visible_for(inner, clearance, &edge) {
            Ok(strip_edge(edge))
        } else {
            Err(tessera_graph::Error::EdgeNotFound(id))
        }
    }

    #[must_use]
    pub fn secure_edge_count<G: GraphAccess>(inner: &G, clearance: &Clearance) -> usize {
        let mut seen = std::collections::HashSet::new();
        for &nid in &secure_node_ids(inner, clearance) {
            if let Ok(edges) = inner.outgoing_edges(nid) {
                for e in edges {
                    if edge_visible_for(inner, clearance, &e) {
                        seen.insert(e.id());
                    }
                }
            }
        }
        seen.len()
    }

    /// Returns outgoing edges of `node` whose target is visible to the caller.
    ///
    /// # Errors
    ///
    /// Returns [`tessera_graph::Error::NodeNotFound`] if the source node does
    /// not exist or the caller lacks sufficient clearance for it.
    pub fn secure_outgoing_edges<G: GraphAccess>(
        inner: &G,
        clearance: &Clearance,
        node: NodeId,
    ) -> tessera_graph::Result<Vec<Edge>> {
        let node_val = inner.node(node)?;
        if !can_read_props(clearance, node_val.properties()) {
            return Err(tessera_graph::Error::NodeNotFound(node));
        }
        let edges = inner.outgoing_edges(node)?;
        Ok(edges
            .into_iter()
            .filter(|e| edge_visible_for(inner, clearance, e))
            .map(strip_edge)
            .collect())
    }

    /// Returns incoming edges of `node` whose source is visible to the caller.
    ///
    /// # Errors
    ///
    /// Returns [`tessera_graph::Error::NodeNotFound`] if the target node does
    /// not exist or the caller lacks sufficient clearance for it.
    pub fn secure_incoming_edges<G: GraphAccess>(
        inner: &G,
        clearance: &Clearance,
        node: NodeId,
    ) -> tessera_graph::Result<Vec<Edge>> {
        let node_val = inner.node(node)?;
        if !can_read_props(clearance, node_val.properties()) {
            return Err(tessera_graph::Error::NodeNotFound(node));
        }
        let edges = inner.incoming_edges(node)?;
        Ok(edges
            .into_iter()
            .filter(|e| edge_visible_for(inner, clearance, e))
            .map(strip_edge)
            .collect())
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
        filter::secure_node_ids(self.inner, &self.clearance)
    }
    fn nodes_by_label(&self, label: &str) -> Vec<NodeId> {
        filter::secure_nodes_by_label(self.inner, &self.clearance, label)
    }
    fn node(&self, id: NodeId) -> tessera_graph::Result<Node> {
        filter::secure_node(self.inner, &self.clearance, id)
    }
    fn node_projected(&self, id: NodeId, keys: &[&str]) -> tessera_graph::Result<Node> {
        filter::secure_node_projected(self.inner, &self.clearance, id, keys)
    }
    fn node_exists(&self, id: NodeId) -> bool {
        filter::secure_node_exists(self.inner, &self.clearance, id)
    }
    fn node_count(&self) -> usize {
        // TODO(perf): replace with a counting iterator to avoid Vec allocation
        self.node_ids().len()
    }
    fn edges_by_label(&self, label: &str) -> Vec<EdgeId> {
        filter::secure_edges_by_label(self.inner, &self.clearance, label)
    }
    fn edge(&self, id: EdgeId) -> tessera_graph::Result<Edge> {
        filter::secure_edge(self.inner, &self.clearance, id)
    }
    fn edge_count(&self) -> usize {
        filter::secure_edge_count(self.inner, &self.clearance)
    }
    fn outgoing_edges(&self, node: NodeId) -> tessera_graph::Result<Vec<Edge>> {
        filter::secure_outgoing_edges(self.inner, &self.clearance, node)
    }
    fn incoming_edges(&self, node: NodeId) -> tessera_graph::Result<Vec<Edge>> {
        filter::secure_incoming_edges(self.inner, &self.clearance, node)
    }

    // --- Mutations ---

    fn add_node(
        &mut self,
        label: &str,
        mut properties: Properties,
    ) -> tessera_graph::Result<NodeId> {
        // Bell-LaPadula no write-down: new resources inherit the caller's clearance.
        SecurityPolicy::strip_security_properties(&mut properties);
        let caller_label =
            SecurityLabel::new(self.clearance.level, self.clearance.compartments.clone());
        SecurityPolicy::inject_label(&mut properties, &caller_label);
        self.inner.add_node(label, properties)
    }

    fn update_node(&mut self, id: NodeId, node: &Node) -> tessera_graph::Result<()> {
        // Explicit write-dominance: caller must dominate the resource's label.
        let existing = self.inner.node(id)?;
        let existing_label = SecurityPolicy::extract_label(existing.properties());
        if !self.clearance.dominates(&existing_label) {
            return Err(Error::NodeNotFound(id));
        }
        // Preserve the existing security label (user cannot change it via update_node)
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
        // Bell-LaPadula no write-down: new edges inherit the caller's clearance.
        SecurityPolicy::strip_security_properties(&mut properties);
        let caller_label =
            SecurityLabel::new(self.clearance.level, self.clearance.compartments.clone());
        SecurityPolicy::inject_label(&mut properties, &caller_label);
        self.inner.add_edge(label, source, target, properties)
    }

    fn update_edge(&mut self, id: EdgeId, edge: &Edge) -> tessera_graph::Result<()> {
        let existing = self.inner.edge(id)?;
        // Explicit write-dominance on the edge's own label
        let existing_label = SecurityPolicy::extract_label(existing.properties());
        if !self.clearance.dominates(&existing_label) {
            return Err(Error::EdgeNotFound(id));
        }
        // Both endpoints must also be visible (cannot reference nodes you cannot read)
        if !filter::edge_visible_for(self.inner, &self.clearance, &existing) {
            return Err(Error::EdgeNotFound(id));
        }
        // Preserve existing security label
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
        filter::secure_node_ids(self.inner, &self.clearance)
    }
    fn nodes_by_label(&self, label: &str) -> Vec<NodeId> {
        filter::secure_nodes_by_label(self.inner, &self.clearance, label)
    }
    fn node(&self, id: NodeId) -> tessera_graph::Result<Node> {
        filter::secure_node(self.inner, &self.clearance, id)
    }
    fn node_projected(&self, id: NodeId, keys: &[&str]) -> tessera_graph::Result<Node> {
        filter::secure_node_projected(self.inner, &self.clearance, id, keys)
    }
    fn node_exists(&self, id: NodeId) -> bool {
        filter::secure_node_exists(self.inner, &self.clearance, id)
    }
    fn node_count(&self) -> usize {
        // TODO(perf): replace with a counting iterator to avoid Vec allocation
        self.node_ids().len()
    }
    fn edges_by_label(&self, label: &str) -> Vec<EdgeId> {
        filter::secure_edges_by_label(self.inner, &self.clearance, label)
    }
    fn edge(&self, id: EdgeId) -> tessera_graph::Result<Edge> {
        filter::secure_edge(self.inner, &self.clearance, id)
    }
    fn edge_count(&self) -> usize {
        filter::secure_edge_count(self.inner, &self.clearance)
    }
    fn outgoing_edges(&self, node: NodeId) -> tessera_graph::Result<Vec<Edge>> {
        filter::secure_outgoing_edges(self.inner, &self.clearance, node)
    }
    fn incoming_edges(&self, node: NodeId) -> tessera_graph::Result<Vec<Edge>> {
        filter::secure_incoming_edges(self.inner, &self.clearance, node)
    }

    // --- Mutations — always denied on a read-only wrapper ---

    fn add_node(&mut self, _label: &str, _properties: Properties) -> tessera_graph::Result<NodeId> {
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
