// SPDX-License-Identifier: MIT

//! A `GraphAccess` delegating wrapper for testing that builders work with non-`Graph` types.
//!
//! `DelegatingGraph` wraps a real `Graph` but goes through the trait interface,
//! proving that any `G: GraphAccess` can substitute `Graph` in query builders.

use tessera_graph::{Edge, EdgeId, Graph, GraphAccess, Node, NodeId, Properties, Result};

/// A thin wrapper around [`Graph`] that implements [`GraphAccess`].
///
/// This is NOT a mock with fake data — it delegates to a real graph.
/// Its purpose is to verify that builders and the compiler work with
/// any `G: GraphAccess`, not just `Graph` directly.
// TODO(O3): add call counters (node_reads, edge_writes, etc.) to enable
// interception tests without a dedicated mock framework.
pub struct DelegatingGraph {
    inner: Graph,
}

impl DelegatingGraph {
    /// Creates a new empty `DelegatingGraph`.
    pub fn new() -> Self {
        Self {
            inner: Graph::new(),
        }
    }
}

impl Default for DelegatingGraph {
    fn default() -> Self {
        Self::new()
    }
}

impl GraphAccess for DelegatingGraph {
    fn node_ids(&self) -> Vec<NodeId> {
        self.inner.node_ids()
    }

    fn nodes_by_label(&self, label: &str) -> Vec<NodeId> {
        self.inner.nodes_by_label(label)
    }

    fn node(&self, id: NodeId) -> Result<Node> {
        self.inner.node(id)
    }

    fn node_label(&self, id: NodeId) -> Result<String> {
        self.inner.node_label(id)
    }

    fn node_projected(&self, id: NodeId, keys: &[&str]) -> Result<Node> {
        self.inner.node_projected(id, keys)
    }

    fn node_exists(&self, id: NodeId) -> bool {
        self.inner.node_exists(id)
    }

    fn node_count(&self) -> usize {
        self.inner.node_count()
    }

    fn edges_by_label(&self, label: &str) -> Vec<EdgeId> {
        self.inner.edges_by_label(label)
    }

    fn edge(&self, id: EdgeId) -> Result<Edge> {
        self.inner.edge(id)
    }

    fn edge_count(&self) -> usize {
        self.inner.edge_count()
    }

    fn outgoing_edges(&self, node: NodeId) -> Result<Vec<Edge>> {
        self.inner.outgoing_edges(node)
    }

    fn incoming_edges(&self, node: NodeId) -> Result<Vec<Edge>> {
        self.inner.incoming_edges(node)
    }

    fn outgoing_edges_by_label(&self, node: NodeId, label: &str) -> Result<Vec<Edge>> {
        self.inner.outgoing_edges_by_label(node, label)
    }

    fn incoming_edges_by_label(&self, node: NodeId, label: &str) -> Result<Vec<Edge>> {
        self.inner.incoming_edges_by_label(node, label)
    }

    fn add_node(&mut self, label: &str, properties: Properties) -> Result<NodeId> {
        GraphAccess::add_node(&mut self.inner, label, properties)
    }

    fn update_node(&mut self, id: NodeId, node: &Node) -> Result<()> {
        self.inner.update_node(id, node)
    }

    fn remove_node(&mut self, id: NodeId) -> Result<Node> {
        self.inner.remove_node(id)
    }

    fn add_edge(
        &mut self,
        label: &str,
        source: NodeId,
        target: NodeId,
        properties: Properties,
    ) -> Result<EdgeId> {
        GraphAccess::add_edge(&mut self.inner, label, source, target, properties)
    }

    fn update_edge(&mut self, id: EdgeId, edge: &Edge) -> Result<()> {
        self.inner.update_edge(id, edge)
    }

    fn remove_edge(&mut self, id: EdgeId) -> Result<Edge> {
        self.inner.remove_edge(id)
    }
}
