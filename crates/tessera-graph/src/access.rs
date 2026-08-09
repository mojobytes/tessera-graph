// SPDX-License-Identifier: MIT

//! The `GraphAccess` trait — abstracts read and write access to a property graph.
//!
//! `Graph` implements this trait directly. External crates (e.g. `tessera-graph-enterprise`)
//! can implement it to intercept reads/writes for LBAC filtering, caching, auditing,
//! federation, or on-read transformations.

use crate::edge::Edge;
use crate::error::{EdgeId, NodeId, Result};
use crate::graph::Graph;
use crate::node::Node;
use crate::property::{Properties, Property};
use crate::storage::codec::adjacency_codec::AdjacencyPointer;

/// Abstraction over property-graph data access.
///
/// All query builders (`NeighborQuery`, `TraversalBuilder`, `SubgraphQuery`,
/// `PatternBuilder`) and the GQL compiler are generic over this trait, so any
/// implementation can be used in place of [`Graph`](crate::Graph).
///
/// # Object Safety
///
/// This trait is object-safe: `&dyn GraphAccess` and `&mut dyn GraphAccess`
/// are valid.
///
/// # Future Considerations
///
/// This trait combines read and write access. A future version may split it
/// into `ReadAccess` and `WriteAccess` to enable read-only views (e.g. for
/// query builders that do not need mutation). This split is deferred until a
/// concrete consumer (e.g. an enterprise read-replica adapter) requires it.
pub trait GraphAccess {
    // --- Node reads ---

    /// Returns all node IDs in the graph.
    fn node_ids(&self) -> Vec<NodeId>;

    /// Returns node IDs whose label matches exactly.
    fn nodes_by_label(&self, label: &str) -> Vec<NodeId>;

    /// Returns a clone of the node with the given ID.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NodeNotFound`] if the ID does not exist.
    fn node(&self, id: NodeId) -> Result<Node>;

    /// Returns only the label of a node without decoding properties.
    ///
    /// The default implementation falls back to a full `node()` read. Implementations
    /// backed by a slot-level codec should override this to read only the label bytes,
    /// skipping property deserialization entirely.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NodeNotFound`] if the ID does not exist.
    fn node_label(&self, id: NodeId) -> Result<String> {
        Ok(self.node(id)?.label().to_owned())
    }

    /// Returns a node with only the projected properties decoded.
    ///
    /// Properties whose keys are not in `keys` are skipped without allocating.
    /// If `keys` is empty, no properties are decoded (label and id are always available).
    ///
    /// # Errors
    ///
    /// Returns [`Error::NodeNotFound`] if the ID does not exist.
    fn node_projected(&self, id: NodeId, keys: &[&str]) -> Result<Node>;

    /// Returns `true` if a node with the given ID exists.
    fn node_exists(&self, id: NodeId) -> bool;

    /// Whether `id` is visible to the caller's read snapshot. The default is
    /// existence (`node_exists`); `Graph` overrides it with MVCC visibility.
    fn node_visible(&self, id: NodeId) -> bool {
        self.node_exists(id)
    }

    /// Returns the total number of nodes.
    ///
    /// Implementations should maintain this as a counter (O(1)), not
    /// compute it by scanning.
    fn node_count(&self) -> usize;

    // --- Edge reads ---

    /// Returns edge IDs whose label matches exactly.
    fn edges_by_label(&self, label: &str) -> Vec<EdgeId>;

    /// Returns a clone of the edge with the given ID.
    ///
    /// # Errors
    ///
    /// Returns [`Error::EdgeNotFound`] if the ID does not exist.
    fn edge(&self, id: EdgeId) -> Result<Edge>;

    /// Returns the total number of edges.
    ///
    /// Implementations should maintain this as a counter (O(1)), not
    /// compute it by scanning.
    fn edge_count(&self) -> usize;

    /// Returns outgoing edges from the given node.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NodeNotFound`] if the node does not exist.
    fn outgoing_edges(&self, node: NodeId) -> Result<Vec<Edge>>;

    /// Returns incoming edges to the given node.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NodeNotFound`] if the node does not exist.
    fn incoming_edges(&self, node: NodeId) -> Result<Vec<Edge>>;

    // --- Node mutations ---

    /// Adds a new node with the given label and properties.
    ///
    /// # Errors
    ///
    /// Returns a storage error if the write fails.
    fn add_node(&mut self, label: &str, properties: Properties) -> Result<NodeId>;

    /// Updates an existing node.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NodeNotFound`] if the ID does not exist.
    fn update_node(&mut self, id: NodeId, node: &Node) -> Result<()>;

    /// Removes a node and all its connected edges.
    ///
    /// # Postconditions
    ///
    /// After a successful call with `id`:
    /// - [`node_exists`](Self::node_exists) returns `false` for `id`.
    /// - [`outgoing_edges`](Self::outgoing_edges) and
    ///   [`incoming_edges`](Self::incoming_edges) return
    ///   [`Error::NodeNotFound`] for `id`.
    /// - Every edge that had `id` as source or target is removed; those
    ///   edge IDs are no longer valid.
    ///
    /// Implementors of this trait **must** uphold these postconditions.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NodeNotFound`] if the ID does not exist.
    fn remove_node(&mut self, id: NodeId) -> Result<Node>;

    // --- Edge mutations ---

    /// Adds a directed edge from `source` to `target`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NodeNotFound`] if either endpoint does not exist.
    fn add_edge(
        &mut self,
        label: &str,
        source: NodeId,
        target: NodeId,
        properties: Properties,
    ) -> Result<EdgeId>;

    /// Updates an existing edge.
    ///
    /// # Errors
    ///
    /// Returns [`Error::EdgeNotFound`] if the ID does not exist.
    fn update_edge(&mut self, id: EdgeId, edge: &Edge) -> Result<()>;

    /// Removes an edge.
    ///
    /// # Errors
    ///
    /// Returns [`Error::EdgeNotFound`] if the ID does not exist.
    fn remove_edge(&mut self, id: EdgeId) -> Result<Edge>;

    // --- Property index query ---

    /// Returns all node IDs whose label is `label` and have property `key` equal
    /// to `value`.
    ///
    /// The default implementation falls back to a label-scanned filter (O(N) on
    /// matching nodes). Implementations backed by a `PropertyIndex` should
    /// override this to return an O(1) index lookup.
    fn nodes_by_label_and_property(&self, label: &str, key: &str, value: &Property) -> Vec<NodeId> {
        self.nodes_by_label(label)
            .into_iter()
            .filter(|&id| {
                self.node(id)
                    .ok()
                    .is_some_and(|n| n.properties().get(key) == Some(value))
            })
            .collect()
    }

    /// Returns node IDs whose `I64` property `(label, key)` lies in `[lo, hi)`
    /// (`None` bounds are unbounded on that side). Default is a label scan +
    /// in-memory range test (O(N)); a `PropertyIndex`-backed implementation
    /// overrides this with an ordered range scan (issue #41).
    fn nodes_by_label_and_property_range(
        &self,
        label: &str,
        key: &str,
        lo: Option<i64>,
        hi: Option<i64>,
    ) -> Vec<NodeId> {
        self.nodes_by_label(label)
            .into_iter()
            .filter(|&id| {
                self.node(id)
                    .ok()
                    .is_some_and(|n| match n.properties().get(key) {
                        Some(Property::I64(v)) => {
                            lo.is_none_or(|l| *v >= l) && hi.is_none_or(|h| *v < h)
                        }
                        _ => false,
                    })
            })
            .collect()
    }

    /// Returns the `NodeId` with the highest `I64` value for `(label, key)`, or
    /// `None`. Default is a label scan tracking the max (O(N)); an indexed
    /// implementation overrides with an O(log N) ordered lookup (issue #40).
    fn max_node_by_property(&self, label: &str, key: &str) -> Option<NodeId> {
        self.nodes_by_label(label)
            .into_iter()
            .filter_map(|id| {
                self.node(id)
                    .ok()
                    .and_then(|n| match n.properties().get(key) {
                        Some(Property::I64(v)) => Some((*v, id)),
                        _ => None,
                    })
            })
            .max_by(|a, b| a.0.cmp(&b.0).then(b.1.0.cmp(&a.1.0)))
            .map(|(_, id)| id)
    }

    /// Returns the `NodeId`s of `label` that do NOT have property `key`. Default
    /// is a label scan + presence test (O(N)); an indexed implementation
    /// overrides with a set difference (issue #42 substitute).
    fn nodes_by_label_without_property(&self, label: &str, key: &str) -> Vec<NodeId> {
        self.nodes_by_label(label)
            .into_iter()
            .filter(|&id| {
                self.node(id)
                    .ok()
                    .is_some_and(|n| n.properties().get(key).is_none())
            })
            .collect()
    }

    // --- Label-filtered edge reads ---

    /// Returns outgoing edges from the given node that match the specified label.
    ///
    /// The default implementation loads all outgoing edges and retains only those
    /// whose label matches. Implementations backed by a label-hash index should
    /// override this to skip deserializing non-matching edges.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NodeNotFound`] if the node does not exist.
    fn outgoing_edges_by_label(&self, node: NodeId, label: &str) -> Result<Vec<Edge>> {
        let mut edges = self.outgoing_edges(node)?;
        edges.retain(|e| e.label() == label);
        Ok(edges)
    }

    /// Returns incoming edges to the given node that match the specified label.
    ///
    /// The default implementation loads all incoming edges and retains only those
    /// whose label matches. Implementations backed by a label-hash index should
    /// override this to skip deserializing non-matching edges.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NodeNotFound`] if the node does not exist.
    fn incoming_edges_by_label(&self, node: NodeId, label: &str) -> Result<Vec<Edge>> {
        let mut edges = self.incoming_edges(node)?;
        edges.retain(|e| e.label() == label);
        Ok(edges)
    }

    // --- Adjacency internals (for enterprise index integration) ---

    /// Returns the adjacency pointer for a node, if it has adjacency pages.
    ///
    /// Returns `Ok(None)` for isolated nodes, non-existent nodes, or when the
    /// implementation does not expose adjacency internals. The default
    /// implementation returns `Ok(None)`.
    ///
    /// # Errors
    ///
    /// Returns a storage error if adjacency page I/O fails.
    fn adj_pointer(&self, _node: NodeId) -> Result<Option<AdjacencyPointer>> {
        Ok(None)
    }

    /// Pre-warms the internal adjacency cache with the given pointer.
    ///
    /// Enterprise crates use this to inject index-resolved pointers into the
    /// core cache, avoiding the O(N) page scan in `resolve_adj_pointer`.
    /// The default implementation is a no-op.
    fn set_adj_pointer(&self, _node: NodeId, _ptr: AdjacencyPointer) {}
}

// ── Blanket delegation for Graph ─────────────────────────────────────────
//
// Every method delegates 1:1 to the corresponding `Graph` inherent method.
// The two exceptions are `add_node` (delegates to `add_node_str`, which
// accepts `&str` rather than `impl Into<String>`, matching the trait
// signature) and `add_edge` (same: delegates to `add_edge_str`).
// This split keeps the trait object-safe (`&str`) while the Graph sugar
// API continues to accept `impl Into<String>`.
impl GraphAccess for Graph {
    fn node_ids(&self) -> Vec<NodeId> {
        self.node_ids()
    }

    fn nodes_by_label(&self, label: &str) -> Vec<NodeId> {
        self.nodes_by_label(label)
    }

    fn node(&self, id: NodeId) -> Result<Node> {
        self.node(id)
    }

    fn node_label(&self, id: NodeId) -> Result<String> {
        self.node_label(id)
    }

    fn node_exists(&self, id: NodeId) -> bool {
        self.node_exists(id)
    }

    fn node_visible(&self, id: NodeId) -> bool {
        self.node_visible(id)
    }

    fn node_projected(&self, id: NodeId, keys: &[&str]) -> Result<Node> {
        self.node_projected(id, keys)
    }

    fn node_count(&self) -> usize {
        self.node_count()
    }

    fn edges_by_label(&self, label: &str) -> Vec<EdgeId> {
        self.edges_by_label(label)
    }

    fn edge(&self, id: EdgeId) -> Result<Edge> {
        self.edge(id)
    }

    fn edge_count(&self) -> usize {
        self.edge_count()
    }

    fn outgoing_edges(&self, node: NodeId) -> Result<Vec<Edge>> {
        self.outgoing_edges(node)
    }

    fn incoming_edges(&self, node: NodeId) -> Result<Vec<Edge>> {
        self.incoming_edges(node)
    }

    fn outgoing_edges_by_label(&self, node: NodeId, label: &str) -> Result<Vec<Edge>> {
        self.outgoing_edges_by_label(node, label)
    }

    fn incoming_edges_by_label(&self, node: NodeId, label: &str) -> Result<Vec<Edge>> {
        self.incoming_edges_by_label(node, label)
    }

    fn add_node(&mut self, label: &str, properties: Properties) -> Result<NodeId> {
        self.add_node_str(label, properties)
    }

    fn update_node(&mut self, id: NodeId, node: &Node) -> Result<()> {
        self.update_node(id, node)
    }

    fn remove_node(&mut self, id: NodeId) -> Result<Node> {
        self.remove_node(id)
    }

    fn add_edge(
        &mut self,
        label: &str,
        source: NodeId,
        target: NodeId,
        properties: Properties,
    ) -> Result<EdgeId> {
        self.add_edge_str(label, source, target, properties)
    }

    fn update_edge(&mut self, id: EdgeId, edge: &Edge) -> Result<()> {
        self.update_edge(id, edge)
    }

    fn remove_edge(&mut self, id: EdgeId) -> Result<Edge> {
        self.remove_edge(id)
    }

    fn nodes_by_label_and_property(&self, label: &str, key: &str, value: &Property) -> Vec<NodeId> {
        self.nodes_by_label_and_property(label, key, value)
    }

    fn nodes_by_label_and_property_range(
        &self,
        label: &str,
        key: &str,
        lo: Option<i64>,
        hi: Option<i64>,
    ) -> Vec<NodeId> {
        self.nodes_by_label_and_property_range(label, key, lo, hi)
    }

    fn max_node_by_property(&self, label: &str, key: &str) -> Option<NodeId> {
        self.max_node_by_property(label, key)
    }

    fn nodes_by_label_without_property(&self, label: &str, key: &str) -> Vec<NodeId> {
        self.nodes_by_label_without_property(label, key)
    }

    fn adj_pointer(&self, node: NodeId) -> Result<Option<AdjacencyPointer>> {
        self.adj_pointer(node)
    }

    fn set_adj_pointer(&self, node: NodeId, ptr: AdjacencyPointer) {
        self.set_adj_pointer(node, ptr);
    }
}
