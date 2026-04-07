// Copyright 2026 BelowZero Security OU. All rights reserved.

//! `SharedNeighborCache` — thread-safe, LBAC-scoped adjacency cache.
//!
//! Designed for the `TesseraGraph` Enterprise server.
//!
//! Unlike [`NeighborCache`](crate::cache::NeighborCache) (which uses `RefCell`
//! and is single-threaded), this cache uses `RwLock` and is `Send + Sync` for
//! use in the async Bolt handler.
//!
//! ## Security: LBAC-scoped cache keys
//!
//! The cache is keyed by `(NodeId, ClearanceKey)`. Each LBAC clearance level
//! gets its own cache partition. Entries are always populated through a
//! `SecureGraphRef` — the cached neighbor list is already LBAC-filtered.
//! This prevents a low-clearance user from traversing through invisible
//! (high-clearance) nodes.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::RwLock;

use tessera_graph::{GraphAccess, NodeId};
use tessera_graph_auth::lbac::Clearance;

const LOCK_POISON_MSG: &str = "SharedNeighborCache lock poisoned";

/// Hashable, equality-comparable key derived from a [`Clearance`].
///
/// `Clearance` does not implement `Hash` (its `BTreeSet<String>` compartments
/// are not `Hash` by default). This newtype extracts `(level, sorted_compartments)`
/// into a form that is `Hash + Eq`, suitable for use as a `HashMap` key.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct ClearanceKey {
    level: u16,
    /// Compartments sorted lexicographically (`BTreeSet` iteration order).
    compartments: Vec<String>,
}

impl Hash for ClearanceKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.level.hash(state);
        self.compartments.hash(state);
    }
}

impl ClearanceKey {
    /// Creates a new `ClearanceKey` from level and compartments.
    #[must_use]
    pub const fn new(level: u16, compartments: Vec<String>) -> Self {
        Self { level, compartments }
    }
}

impl From<&Clearance> for ClearanceKey {
    fn from(c: &Clearance) -> Self {
        Self {
            level: c.level,
            compartments: c.compartments.iter().cloned().collect(),
        }
    }
}

/// Thread-safe, LBAC-scoped neighbor cache for the enterprise server.
///
/// Stores `Vec<NodeId>` per `(NodeId, ClearanceKey)` for outgoing and incoming
/// neighbors. Populated lazily from a `SecureGraphRef` on cache miss.
/// Invalidated on mutations by removing ALL clearance entries for the affected
/// nodes.
///
/// # Thread Safety
///
/// Uses `RwLock` internally — `Send + Sync`. Read-heavy workloads (BFS
/// traversal) only acquire a read lock on cache hit. Write lock is acquired
/// only on cache miss (populate) and on mutation (invalidate).
pub struct SharedNeighborCache {
    outgoing: RwLock<HashMap<(NodeId, ClearanceKey), Vec<NodeId>>>,
    incoming: RwLock<HashMap<(NodeId, ClearanceKey), Vec<NodeId>>>,
}

impl SharedNeighborCache {
    /// Creates a new empty cache.
    #[must_use]
    pub fn new() -> Self {
        Self {
            outgoing: RwLock::new(HashMap::new()),
            incoming: RwLock::new(HashMap::new()),
        }
    }

    /// Returns cached outgoing neighbor IDs, or `None` on cache miss.
    ///
    /// # Panics
    ///
    /// Panics if the internal `RwLock` is poisoned.
    #[must_use]
    pub fn get_outgoing(&self, node: NodeId, key: &ClearanceKey) -> Option<Vec<NodeId>> {
        let guard = self.outgoing.read().expect(LOCK_POISON_MSG);
        guard.get(&(node, key.clone())).cloned()
    }

    /// Returns cached incoming neighbor IDs, or `None` on cache miss.
    ///
    /// # Panics
    ///
    /// Panics if the internal `RwLock` is poisoned.
    #[must_use]
    pub fn get_incoming(&self, node: NodeId, key: &ClearanceKey) -> Option<Vec<NodeId>> {
        let guard = self.incoming.read().expect(LOCK_POISON_MSG);
        guard.get(&(node, key.clone())).cloned()
    }

    /// Inserts outgoing neighbor IDs into the cache.
    ///
    /// # Panics
    ///
    /// Panics if the internal `RwLock` is poisoned.
    pub fn insert_outgoing(&self, node: NodeId, key: ClearanceKey, neighbors: Vec<NodeId>) {
        self.outgoing
            .write()
            .expect(LOCK_POISON_MSG)
            .insert((node, key), neighbors);
    }

    /// Inserts incoming neighbor IDs into the cache.
    ///
    /// # Panics
    ///
    /// Panics if the internal `RwLock` is poisoned.
    pub fn insert_incoming(&self, node: NodeId, key: ClearanceKey, neighbors: Vec<NodeId>) {
        self.incoming
            .write()
            .expect(LOCK_POISON_MSG)
            .insert((node, key), neighbors);
    }

    /// Invalidates all cache entries for a node across ALL clearance levels.
    ///
    /// Used when a node is removed — all edges to/from it are gone, so every
    /// clearance partition that cached this node's neighbors is stale.
    ///
    /// # Panics
    ///
    /// Panics if the internal `RwLock` is poisoned.
    pub fn invalidate_node(&self, node: NodeId) {
        self.outgoing
            .write()
            .expect(LOCK_POISON_MSG)
            .retain(|&(n, _), _| n != node);
        self.incoming
            .write()
            .expect(LOCK_POISON_MSG)
            .retain(|&(n, _), _| n != node);
    }

    /// Invalidates cache entries affected by an edge between source and target.
    ///
    /// Removes outgoing entries for `source` and incoming entries for `target`
    /// across ALL clearance levels.
    ///
    /// # Panics
    ///
    /// Panics if the internal `RwLock` is poisoned.
    pub fn invalidate_edge(&self, source: NodeId, target: NodeId) {
        self.outgoing
            .write()
            .expect(LOCK_POISON_MSG)
            .retain(|&(n, _), _| n != source);
        self.incoming
            .write()
            .expect(LOCK_POISON_MSG)
            .retain(|&(n, _), _| n != target);
    }

    /// Clears all cached entries across all clearance levels.
    ///
    /// Used as a conservative invalidation when the exact affected node/edge
    /// IDs are not known (e.g. after a bulk mutation).
    ///
    /// # Panics
    ///
    /// Panics if the internal `RwLock` is poisoned.
    pub fn clear(&self) {
        self.outgoing.write().expect(LOCK_POISON_MSG).clear();
        self.incoming.write().expect(LOCK_POISON_MSG).clear();
    }

    /// Returns outgoing neighbor IDs, populating from the graph on cache miss.
    ///
    /// The `graph` parameter should be a `SecureGraphRef` (LBAC-filtered) so
    /// that the cached list only contains neighbors visible to the caller.
    ///
    /// # Errors
    ///
    /// Returns [`tessera_graph::Error::NodeNotFound`] if the node does not exist
    /// or the caller lacks sufficient clearance.
    pub fn outgoing_neighbor_ids<G: GraphAccess + ?Sized>(
        &self,
        graph: &G,
        node: NodeId,
        key: &ClearanceKey,
    ) -> tessera_graph::Result<Vec<NodeId>> {
        if let Some(cached) = self.get_outgoing(node, key) {
            return Ok(cached);
        }
        let edges = graph.outgoing_edges(node)?;
        let neighbors: Vec<NodeId> = edges.iter().map(tessera_graph::Edge::target).collect();
        self.insert_outgoing(node, key.clone(), neighbors.clone());
        Ok(neighbors)
    }

    /// Returns incoming neighbor IDs, populating from the graph on cache miss.
    ///
    /// # Errors
    ///
    /// Returns [`tessera_graph::Error::NodeNotFound`] if the node does not exist
    /// or the caller lacks sufficient clearance.
    pub fn incoming_neighbor_ids<G: GraphAccess + ?Sized>(
        &self,
        graph: &G,
        node: NodeId,
        key: &ClearanceKey,
    ) -> tessera_graph::Result<Vec<NodeId>> {
        if let Some(cached) = self.get_incoming(node, key) {
            return Ok(cached);
        }
        let edges = graph.incoming_edges(node)?;
        let neighbors: Vec<NodeId> = edges.iter().map(tessera_graph::Edge::source).collect();
        self.insert_incoming(node, key.clone(), neighbors.clone());
        Ok(neighbors)
    }
}

impl Default for SharedNeighborCache {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clearance(level: u16, comps: &[&str]) -> Clearance {
        Clearance::new(level, comps.iter().map(|s| (*s).to_string()).collect())
    }

    // ── ClearanceKey ──

    #[test]
    fn clearance_key_same_clearance_equal() {
        let c = clearance(2, &["ALPHA", "BETA"]);
        let k1 = ClearanceKey::from(&c);
        let k2 = ClearanceKey::from(&c);
        assert_eq!(k1, k2);
    }

    #[test]
    fn clearance_key_different_level_not_equal() {
        let k1 = ClearanceKey::from(&clearance(1, &["ALPHA"]));
        let k2 = ClearanceKey::from(&clearance(2, &["ALPHA"]));
        assert_ne!(k1, k2);
    }

    #[test]
    fn clearance_key_different_compartments_not_equal() {
        let k1 = ClearanceKey::from(&clearance(2, &["ALPHA"]));
        let k2 = ClearanceKey::from(&clearance(2, &["BETA"]));
        assert_ne!(k1, k2);
    }

    #[test]
    fn clearance_key_hash_consistent() {
        use std::collections::hash_map::DefaultHasher;
        let c = clearance(3, &["X", "Y"]);
        let k1 = ClearanceKey::from(&c);
        let k2 = ClearanceKey::from(&c);
        let hash = |k: &ClearanceKey| {
            let mut h = DefaultHasher::new();
            k.hash(&mut h);
            h.finish()
        };
        assert_eq!(hash(&k1), hash(&k2));
    }

    // ── SharedNeighborCache core ──

    #[test]
    fn new_cache_starts_empty() {
        let cache = SharedNeighborCache::new();
        let key = ClearanceKey::from(&clearance(0, &[]));
        assert!(cache.get_outgoing(NodeId::from_raw(0), &key).is_none());
        assert!(cache.get_incoming(NodeId::from_raw(0), &key).is_none());
    }

    #[test]
    fn populate_then_hit_same_clearance() {
        let cache = SharedNeighborCache::new();
        let key = ClearanceKey::from(&clearance(1, &["A"]));
        let node = NodeId::from_raw(1);
        let neighbors = vec![NodeId::from_raw(2), NodeId::from_raw(3)];
        cache.insert_outgoing(node, key.clone(), neighbors.clone());
        assert_eq!(cache.get_outgoing(node, &key).expect("hit"), neighbors);
    }

    #[test]
    fn miss_on_different_clearance() {
        let cache = SharedNeighborCache::new();
        let k1 = ClearanceKey::from(&clearance(1, &["A"]));
        let k2 = ClearanceKey::from(&clearance(2, &["A"]));
        let node = NodeId::from_raw(1);
        cache.insert_outgoing(node, k1, vec![NodeId::from_raw(2)]);
        assert!(cache.get_outgoing(node, &k2).is_none());
    }

    #[test]
    fn invalidate_node_removes_all_clearances() {
        let cache = SharedNeighborCache::new();
        let node = NodeId::from_raw(1);
        let k1 = ClearanceKey::from(&clearance(1, &[]));
        let k2 = ClearanceKey::from(&clearance(2, &["X"]));
        cache.insert_outgoing(node, k1.clone(), vec![NodeId::from_raw(2)]);
        cache.insert_outgoing(node, k2.clone(), vec![NodeId::from_raw(2), NodeId::from_raw(3)]);
        cache.insert_incoming(node, k1.clone(), vec![NodeId::from_raw(0)]);

        cache.invalidate_node(node);

        assert!(cache.get_outgoing(node, &k1).is_none());
        assert!(cache.get_outgoing(node, &k2).is_none());
        assert!(cache.get_incoming(node, &k1).is_none());
    }

    #[test]
    fn invalidate_edge_clears_source_outgoing_and_target_incoming() {
        let cache = SharedNeighborCache::new();
        let src = NodeId::from_raw(1);
        let tgt = NodeId::from_raw(2);
        let key = ClearanceKey::from(&clearance(1, &[]));
        cache.insert_outgoing(src, key.clone(), vec![tgt]);
        cache.insert_incoming(tgt, key.clone(), vec![src]);

        cache.invalidate_edge(src, tgt);

        assert!(cache.get_outgoing(src, &key).is_none());
        assert!(cache.get_incoming(tgt, &key).is_none());
    }

    // ── Lazy population ──

    #[test]
    fn outgoing_neighbor_ids_populates_on_miss() {
        use tessera_graph::{Graph, Properties};
        let mut g = Graph::new();
        let a = g.add_node("N", Properties::new()).expect("add a");
        let b = g.add_node("N", Properties::new()).expect("add b");
        g.add_edge("E", a, b, Properties::new()).expect("add edge");

        let cache = SharedNeighborCache::new();
        let key = ClearanceKey::from(&clearance(0, &[]));

        let neighbors = cache.outgoing_neighbor_ids(&g, a, &key).expect("populate");
        assert_eq!(neighbors, vec![b]);

        // Second call is a cache hit
        let neighbors2 = cache.outgoing_neighbor_ids(&g, a, &key).expect("hit");
        assert_eq!(neighbors2, vec![b]);
    }

    #[test]
    fn incoming_neighbor_ids_populates_on_miss() {
        use tessera_graph::{Graph, Properties};
        let mut g = Graph::new();
        let a = g.add_node("N", Properties::new()).expect("add a");
        let b = g.add_node("N", Properties::new()).expect("add b");
        g.add_edge("E", a, b, Properties::new()).expect("add edge");

        let cache = SharedNeighborCache::new();
        let key = ClearanceKey::from(&clearance(0, &[]));

        let neighbors = cache.incoming_neighbor_ids(&g, b, &key).expect("populate");
        assert_eq!(neighbors, vec![a]);
    }

    #[test]
    fn shared_cache_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<SharedNeighborCache>();
    }
}
