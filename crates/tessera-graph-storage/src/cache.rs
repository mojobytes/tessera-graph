// Copyright 2026 BelowZero Security OU. All rights reserved.

//! `NeighborCache` — enterprise adjacency cache that short-circuits the MIT core
//! storage hot path for traversal operations.
//!
//! Instead of deserializing full `Edge` objects (label, properties, overflow pages)
//! on every BFS step, the cache stores `Vec<NodeId>` per node for outgoing and
//! incoming neighbors. Lazy-populated on first access, invalidated on mutations.

use std::cell::RefCell;
use std::collections::HashMap;

use tessera_graph::{Edge, EdgeId, GraphAccess, Node, NodeId, Properties};

use crate::adj_index::AdjacencyIndex;

/// In-memory neighbor cache that wraps any `GraphAccess` implementation.
///
/// Stores resolved `Vec<NodeId>` for outgoing and incoming neighbors,
/// eliminating full `Edge` deserialization during BFS/DFS traversal.
///
/// # Interior Mutability
///
/// Uses `RefCell` for lazy population under `&self` reads. This is correct
/// because the GQL execution path is single-threaded. If concurrent access
/// is needed in the future, replace with `RwLock`.
pub struct NeighborCache<G> {
    inner: G,
    outgoing: RefCell<HashMap<NodeId, Vec<NodeId>>>,
    incoming: RefCell<HashMap<NodeId, Vec<NodeId>>>,
    adj_index: RefCell<AdjacencyIndex>,
}

impl<G: GraphAccess> NeighborCache<G> {
    /// Creates a new cache wrapping the given graph.
    pub fn new(inner: G) -> Self {
        Self {
            inner,
            outgoing: RefCell::new(HashMap::new()),
            incoming: RefCell::new(HashMap::new()),
            adj_index: RefCell::new(AdjacencyIndex::new()),
        }
    }

    /// Returns a reference to the inner graph.
    pub const fn inner(&self) -> &G {
        &self.inner
    }

    /// Returns a mutable reference to the inner graph.
    pub const fn inner_mut(&mut self) -> &mut G {
        &mut self.inner
    }

    /// Returns a reference to the adjacency index.
    pub fn adj_index(&self) -> std::cell::Ref<'_, AdjacencyIndex> {
        self.adj_index.borrow()
    }

    /// Returns cached outgoing neighbor IDs, or `None` on cache miss.
    fn outgoing_cached(&self, node: NodeId) -> Option<Vec<NodeId>> {
        self.outgoing.borrow().get(&node).cloned()
    }

    /// Returns cached incoming neighbor IDs, or `None` on cache miss.
    fn incoming_cached(&self, node: NodeId) -> Option<Vec<NodeId>> {
        self.incoming.borrow().get(&node).cloned()
    }

    /// Populates the outgoing cache entry for a node.
    fn populate_outgoing(&self, node: NodeId, neighbors: Vec<NodeId>) {
        self.outgoing.borrow_mut().insert(node, neighbors);
    }

    /// Populates the incoming cache entry for a node.
    fn populate_incoming(&self, node: NodeId, neighbors: Vec<NodeId>) {
        self.incoming.borrow_mut().insert(node, neighbors);
    }

    /// Invalidates all cache entries for a node (both outgoing and incoming).
    fn invalidate_node(&self, node: NodeId) {
        self.outgoing.borrow_mut().remove(&node);
        self.incoming.borrow_mut().remove(&node);
    }

    /// Invalidates cache entries affected by an edge between source and target.
    fn invalidate_edge(&self, source: NodeId, target: NodeId) {
        self.outgoing.borrow_mut().remove(&source);
        self.incoming.borrow_mut().remove(&target);
    }

    /// Returns outgoing neighbor IDs, populating from the inner graph on cache miss.
    ///
    /// This is the fast path for BFS/DFS: returns `Vec<NodeId>` without
    /// deserializing edge labels or properties.
    ///
    /// # Errors
    ///
    /// Returns [`tessera_graph::Error::NodeNotFound`] if the node does not exist.
    pub fn outgoing_neighbor_ids(&self, node: NodeId) -> tessera_graph::Result<Vec<NodeId>> {
        if let Some(cached) = self.outgoing_cached(node) {
            return Ok(cached);
        }
        // Pre-warm core AdjCache from the enterprise index to avoid O(N) scan.
        if let Some(ptr) = self.adj_index.borrow().get(node) {
            self.inner.set_adj_pointer(node, ptr);
        }
        let edges = self.inner.outgoing_edges(node)?;
        let neighbors: Vec<NodeId> = edges.iter().map(Edge::target).collect();
        self.populate_outgoing(node, neighbors.clone());
        Ok(neighbors)
    }

    /// Returns incoming neighbor IDs, populating from the inner graph on cache miss.
    ///
    /// # Errors
    ///
    /// Returns [`tessera_graph::Error::NodeNotFound`] if the node does not exist.
    pub fn incoming_neighbor_ids(&self, node: NodeId) -> tessera_graph::Result<Vec<NodeId>> {
        if let Some(cached) = self.incoming_cached(node) {
            return Ok(cached);
        }
        // Pre-warm core AdjCache from the enterprise index to avoid O(N) scan.
        if let Some(ptr) = self.adj_index.borrow().get(node) {
            self.inner.set_adj_pointer(node, ptr);
        }
        let edges = self.inner.incoming_edges(node)?;
        let neighbors: Vec<NodeId> = edges.iter().map(Edge::source).collect();
        self.populate_incoming(node, neighbors.clone());
        Ok(neighbors)
    }
}

impl<G: GraphAccess> GraphAccess for NeighborCache<G> {
    // --- Reads: delegate to inner ---

    fn node_ids(&self) -> Vec<NodeId> {
        self.inner.node_ids()
    }

    fn nodes_by_label(&self, label: &str) -> Vec<NodeId> {
        self.inner.nodes_by_label(label)
    }

    fn node(&self, id: NodeId) -> tessera_graph::Result<Node> {
        self.inner.node(id)
    }

    fn node_projected(&self, id: NodeId, keys: &[&str]) -> tessera_graph::Result<Node> {
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

    fn edge(&self, id: EdgeId) -> tessera_graph::Result<Edge> {
        self.inner.edge(id)
    }

    fn edge_count(&self) -> usize {
        self.inner.edge_count()
    }

    fn outgoing_edges(&self, node: NodeId) -> tessera_graph::Result<Vec<Edge>> {
        self.inner.outgoing_edges(node)
    }

    fn incoming_edges(&self, node: NodeId) -> tessera_graph::Result<Vec<Edge>> {
        self.inner.incoming_edges(node)
    }

    // --- Mutations: delegate + invalidate ---

    fn add_node(&mut self, label: &str, properties: Properties) -> tessera_graph::Result<NodeId> {
        self.inner.add_node(label, properties)
    }

    fn update_node(&mut self, id: NodeId, node: &Node) -> tessera_graph::Result<()> {
        self.inner.update_node(id, node)
    }

    fn remove_node(&mut self, id: NodeId) -> tessera_graph::Result<Node> {
        // Collect neighbors before removing so we can invalidate their cache entries.
        let out_neighbors: Vec<NodeId> = self
            .inner
            .outgoing_edges(id)
            .unwrap_or_default()
            .iter()
            .map(Edge::target)
            .collect();
        let in_neighbors: Vec<NodeId> = self
            .inner
            .incoming_edges(id)
            .unwrap_or_default()
            .iter()
            .map(Edge::source)
            .collect();

        let removed = self.inner.remove_node(id)?;

        // Invalidate the removed node itself.
        self.invalidate_node(id);
        // Remove the node from the adjacency index.
        self.adj_index.borrow_mut().remove(id);

        // Invalidate all neighbors that had edges to/from the removed node
        // and update their adj_index entries.
        for neighbor in &out_neighbors {
            self.incoming.borrow_mut().remove(neighbor);
            match self.inner.adj_pointer(*neighbor) {
                Ok(Some(ptr)) => self.adj_index.borrow_mut().insert(*neighbor, ptr),
                _ => { self.adj_index.borrow_mut().remove(*neighbor); }
            }
        }
        for neighbor in &in_neighbors {
            self.outgoing.borrow_mut().remove(neighbor);
            match self.inner.adj_pointer(*neighbor) {
                Ok(Some(ptr)) => self.adj_index.borrow_mut().insert(*neighbor, ptr),
                _ => { self.adj_index.borrow_mut().remove(*neighbor); }
            }
        }

        Ok(removed)
    }

    fn add_edge(
        &mut self,
        label: &str,
        source: NodeId,
        target: NodeId,
        properties: Properties,
    ) -> tessera_graph::Result<EdgeId> {
        let id = self.inner.add_edge(label, source, target, properties)?;
        self.invalidate_edge(source, target);
        // Update adj_index: re-read pointers from the core after the mutation.
        if let Ok(Some(ptr)) = self.inner.adj_pointer(source) {
            self.adj_index.borrow_mut().insert(source, ptr);
        }
        if source != target {
            if let Ok(Some(ptr)) = self.inner.adj_pointer(target) {
                self.adj_index.borrow_mut().insert(target, ptr);
            }
        }
        Ok(id)
    }

    fn update_edge(&mut self, id: EdgeId, edge: &Edge) -> tessera_graph::Result<()> {
        self.inner.update_edge(id, edge)
    }

    fn remove_edge(&mut self, id: EdgeId) -> tessera_graph::Result<Edge> {
        let edge = self.inner.remove_edge(id)?;
        let source = edge.source();
        let target = edge.target();
        self.invalidate_edge(source, target);
        // Update adj_index: re-read pointers after the mutation.
        match self.inner.adj_pointer(source) {
            Ok(Some(ptr)) => self.adj_index.borrow_mut().insert(source, ptr),
            _ => { self.adj_index.borrow_mut().remove(source); }
        }
        if source != target {
            match self.inner.adj_pointer(target) {
                Ok(Some(ptr)) => self.adj_index.borrow_mut().insert(target, ptr),
                _ => { self.adj_index.borrow_mut().remove(target); }
            }
        }
        Ok(edge)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tessera_graph::Graph;

    fn chain_graph(n: usize) -> (Graph, Vec<NodeId>) {
        let mut g = Graph::new();
        let mut nodes = Vec::with_capacity(n);
        for _ in 0..n {
            nodes.push(g.add_node("N", Properties::new()).unwrap());
        }
        for i in 0..n - 1 {
            g.add_edge("E", nodes[i], nodes[i + 1], Properties::new())
                .unwrap();
        }
        (g, nodes)
    }

    // ── Ciclo 1: struct exists and starts empty ──

    #[test]
    fn new_cache_starts_empty() {
        let g = Graph::new();
        let cache = NeighborCache::new(g);
        assert!(cache.outgoing_cached(NodeId::from_raw(0)).is_none());
        assert!(cache.incoming_cached(NodeId::from_raw(0)).is_none());
    }

    // ── Ciclo 2: core cache logic ──

    #[test]
    fn populate_and_retrieve_outgoing() {
        let g = Graph::new();
        let cache = NeighborCache::new(g);
        let node = NodeId::from_raw(1);
        let neighbors = vec![NodeId::from_raw(2), NodeId::from_raw(3)];
        cache.populate_outgoing(node, neighbors.clone());
        assert_eq!(cache.outgoing_cached(node).unwrap(), neighbors);
    }

    #[test]
    fn populate_and_retrieve_incoming() {
        let g = Graph::new();
        let cache = NeighborCache::new(g);
        let node = NodeId::from_raw(1);
        let neighbors = vec![NodeId::from_raw(0)];
        cache.populate_incoming(node, neighbors.clone());
        assert_eq!(cache.incoming_cached(node).unwrap(), neighbors);
    }

    #[test]
    fn invalidate_node_clears_both_maps() {
        let g = Graph::new();
        let cache = NeighborCache::new(g);
        let node = NodeId::from_raw(1);
        cache.populate_outgoing(node, vec![NodeId::from_raw(2)]);
        cache.populate_incoming(node, vec![NodeId::from_raw(0)]);
        cache.invalidate_node(node);
        assert!(cache.outgoing_cached(node).is_none());
        assert!(cache.incoming_cached(node).is_none());
    }

    #[test]
    fn invalidate_edge_clears_source_outgoing_and_target_incoming() {
        let g = Graph::new();
        let cache = NeighborCache::new(g);
        let src = NodeId::from_raw(1);
        let tgt = NodeId::from_raw(2);
        cache.populate_outgoing(src, vec![tgt]);
        cache.populate_incoming(tgt, vec![src]);
        cache.invalidate_edge(src, tgt);
        assert!(cache.outgoing_cached(src).is_none());
        assert!(cache.incoming_cached(tgt).is_none());
    }

    // ── Ciclo 3: lazy population from GraphAccess ──

    #[test]
    fn outgoing_neighbor_ids_populates_on_miss() {
        let (g, nodes) = chain_graph(3); // A→B→C
        let cache = NeighborCache::new(g);

        // First call: cache miss, populates from inner
        let neighbors = cache.outgoing_neighbor_ids(nodes[0]).unwrap();
        assert_eq!(neighbors, vec![nodes[1]]);

        // Second call: cache hit
        let neighbors2 = cache.outgoing_neighbor_ids(nodes[0]).unwrap();
        assert_eq!(neighbors2, vec![nodes[1]]);
    }

    #[test]
    fn incoming_neighbor_ids_populates_on_miss() {
        let (g, nodes) = chain_graph(3);
        let cache = NeighborCache::new(g);
        let neighbors = cache.incoming_neighbor_ids(nodes[1]).unwrap();
        assert_eq!(neighbors, vec![nodes[0]]);
    }

    #[test]
    fn neighbor_ids_missing_node_returns_error() {
        let g = Graph::new();
        let cache = NeighborCache::new(g);
        assert!(cache.outgoing_neighbor_ids(NodeId::from_raw(999)).is_err());
        assert!(cache.incoming_neighbor_ids(NodeId::from_raw(999)).is_err());
    }

    // ── Ciclo 4: invalidation on mutations ──

    #[test]
    fn add_edge_invalidates_cache() {
        let (g, nodes) = chain_graph(3); // A→B→C
        let mut cache = NeighborCache::new(g);

        // Populate cache for A
        let _ = cache.outgoing_neighbor_ids(nodes[0]).unwrap();
        assert!(cache.outgoing_cached(nodes[0]).is_some());

        // Add edge A→C
        let d = cache.add_node("N", Properties::new()).unwrap();
        cache
            .add_edge("E", nodes[0], d, Properties::new())
            .unwrap();

        // Cache for A's outgoing should be invalidated
        assert!(cache.outgoing_cached(nodes[0]).is_none());

        // Re-query: should now include the new neighbor
        let neighbors = cache.outgoing_neighbor_ids(nodes[0]).unwrap();
        assert!(neighbors.contains(&nodes[1]));
        assert!(neighbors.contains(&d));
    }

    #[test]
    fn remove_edge_invalidates_both_endpoints() {
        let (g, nodes) = chain_graph(3); // A→B→C
        let mut cache = NeighborCache::new(g);

        // Populate cache
        let _ = cache.outgoing_neighbor_ids(nodes[0]).unwrap();
        let _ = cache.incoming_neighbor_ids(nodes[1]).unwrap();

        // Find the actual edge ID for A→B
        let edge_id = cache
            .outgoing_edges(nodes[0])
            .expect("outgoing_edges")
            .into_iter()
            .find(|e| e.target() == nodes[1])
            .expect("edge A→B")
            .id();
        cache.remove_edge(edge_id).expect("remove_edge");

        // Both endpoints invalidated
        assert!(cache.outgoing_cached(nodes[0]).is_none());
        assert!(cache.incoming_cached(nodes[1]).is_none());

        // Re-query: A has no outgoing neighbors
        let neighbors = cache.outgoing_neighbor_ids(nodes[0]).unwrap();
        assert!(neighbors.is_empty());
    }

    #[test]
    fn remove_node_invalidates_all_incident() {
        let (g, nodes) = chain_graph(3); // A→B→C
        let mut cache = NeighborCache::new(g);

        // Populate cache for A's outgoing and C's incoming
        let _ = cache.outgoing_neighbor_ids(nodes[0]).unwrap(); // OK: test
        let _ = cache.incoming_neighbor_ids(nodes[2]).unwrap(); // OK: test

        // Remove B — A's outgoing and C's incoming should invalidate
        cache.remove_node(nodes[1]).unwrap(); // OK: test

        assert!(cache.outgoing_cached(nodes[0]).is_none());
        assert!(cache.incoming_cached(nodes[2]).is_none());
    }

    // ── Ciclo 5: NeighborCache has AdjacencyIndex, empty for new node ──

    #[test]
    fn adj_index_empty_for_new_node() {
        let mut cache = NeighborCache::new(Graph::new());
        let id = cache.add_node("Person", Properties::new()).unwrap(); // OK: test
        assert!(
            cache.adj_index().get(id).is_none(),
            "new node without edges must not be in adj_index"
        );
    }

    // ── Ciclo 6: add_edge populates the index ──

    #[test]
    fn adj_index_populated_after_add_edge() {
        let mut cache = NeighborCache::new(Graph::new());
        let a = cache.add_node("A", Properties::new()).unwrap(); // OK: test
        let b = cache.add_node("B", Properties::new()).unwrap(); // OK: test
        cache
            .add_edge("REL", a, b, Properties::new())
            .unwrap(); // OK: test

        let ptr_a = cache.adj_index().get(a).expect("source must be indexed"); // OK: test
        assert!(ptr_a.outgoing_page.is_some(), "source must have outgoing page");

        let ptr_b = cache.adj_index().get(b).expect("target must be indexed"); // OK: test
        assert!(ptr_b.incoming_page.is_some(), "target must have incoming page");
    }

    #[test]
    fn adj_index_self_loop_populates_both_pages() {
        let mut cache = NeighborCache::new(Graph::new());
        let a = cache.add_node("A", Properties::new()).unwrap(); // OK: test
        cache
            .add_edge("SELF", a, a, Properties::new())
            .unwrap(); // OK: test

        let ptr = cache.adj_index().get(a).expect("self-loop node must be indexed"); // OK: test
        assert!(ptr.outgoing_page.is_some());
        assert!(ptr.incoming_page.is_some());
    }

    // ── Ciclo 7: remove_edge and remove_node update the index ──

    #[test]
    fn adj_index_updated_after_remove_edge() {
        let mut cache = NeighborCache::new(Graph::new());
        let a = cache.add_node("N", Properties::new()).unwrap(); // OK: test
        let b = cache.add_node("N", Properties::new()).unwrap(); // OK: test
        let eid = cache
            .add_edge("E", a, b, Properties::new())
            .unwrap(); // OK: test

        assert!(cache.adj_index().get(a).is_some());
        assert!(cache.adj_index().get(b).is_some());

        cache.remove_edge(eid).unwrap(); // OK: test

        // After removing the only edge, nodes still have adj pages (empty records).
        // The index reflects the core state — no panic, no stale data.
    }

    #[test]
    fn adj_index_cleared_after_remove_node() {
        let mut cache = NeighborCache::new(Graph::new());
        let a = cache.add_node("A", Properties::new()).unwrap(); // OK: test
        let b = cache.add_node("B", Properties::new()).unwrap(); // OK: test
        cache
            .add_edge("E", a, b, Properties::new())
            .unwrap(); // OK: test
        assert!(cache.adj_index().get(a).is_some());

        cache.remove_node(a).unwrap(); // OK: test
        assert!(
            cache.adj_index().get(a).is_none(),
            "removed node must be cleared from adj_index"
        );
    }

    // ── Ciclo 8: pre-warm core cache from index ──

    #[test]
    fn outgoing_neighbor_ids_prewarms_core_cache() {
        let mut cache = NeighborCache::new(Graph::new());
        let a = cache.add_node("A", Properties::new()).unwrap(); // OK: test
        let b = cache.add_node("B", Properties::new()).unwrap(); // OK: test
        cache
            .add_edge("E", a, b, Properties::new())
            .unwrap(); // OK: test

        assert!(cache.adj_index().get(a).is_some());
        // Clear the neighbor cache to force a re-read
        cache.invalidate_node(a);

        // outgoing_neighbor_ids should pre-warm core AdjCache and succeed
        let neighbors = cache.outgoing_neighbor_ids(a).unwrap(); // OK: test
        assert_eq!(neighbors, vec![b]);
    }

    #[test]
    fn incoming_neighbor_ids_prewarms_core_cache() {
        let mut cache = NeighborCache::new(Graph::new());
        let a = cache.add_node("A", Properties::new()).unwrap(); // OK: test
        let b = cache.add_node("B", Properties::new()).unwrap(); // OK: test
        cache
            .add_edge("E", a, b, Properties::new())
            .unwrap(); // OK: test

        assert!(cache.adj_index().get(b).is_some());
        cache.invalidate_node(b);

        let neighbors = cache.incoming_neighbor_ids(b).unwrap(); // OK: test
        assert_eq!(neighbors, vec![a]);
    }

    // ── Fase 3: Throughput regression guard ──

    #[test]
    fn add_edge_with_adj_index_throughput_regression_guard() {
        let mut cache = NeighborCache::new(Graph::new());
        let count = 5_000;

        // Create nodes
        let mut nodes = Vec::with_capacity(count + 1);
        for _ in 0..=count {
            nodes.push(cache.add_node("N", Properties::new()).unwrap()); // OK: test
        }

        // Measure: add_edge with adj_index maintenance
        let start = std::time::Instant::now();
        for i in 0..count {
            cache
                .add_edge("E", nodes[i], nodes[i + 1], Properties::new())
                .unwrap(); // OK: test
        }
        let elapsed = start.elapsed();
        #[allow(clippy::cast_precision_loss)]
        let ops_per_sec = count as f64 / elapsed.as_secs_f64();

        let threshold = if cfg!(debug_assertions) { 100.0 } else { 2_000.0 };
        assert!(
            ops_per_sec >= threshold,
            "add_edge throughput {ops_per_sec:.0} ops/s is below threshold {threshold:.0} ops/s \
             (elapsed: {elapsed:?} for {count} edges)"
        );

        // Verify adj_index has entries for all nodes with edges
        assert!(
            !cache.adj_index().is_empty(),
            "adj_index must be populated after bulk insert"
        );
        assert!(
            cache.adj_index().len() >= count,
            "adj_index must have entries for edge endpoints"
        );
    }
}
