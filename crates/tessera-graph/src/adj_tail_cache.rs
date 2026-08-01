// SPDX-License-Identifier: Apache-2.0

//! Internal cache of adjacency-chain tail state, keyed by `(node_id,
//! AdjDirection)`.
//!
//! Stores the [`AdjChainState`](crate::storage::codec::adjacency_codec::AdjChainState)
//! of each node's adjacency chain so the write path can append edges without
//! re-walking the whole chain to find its tail (the O(N²) fan-in bug, issue
//! #33). It is a pure optimization: a miss (or an evicted entry) falls back to
//! recomputing the state via `read_adj_chain_state`, so correctness never
//! depends on retention. This type is deliberately internal — unlike
//! [`AdjacencyPointer`](crate::storage::codec::adjacency_codec::AdjacencyPointer)
//! it encodes internal chain layout and must never cross the `GraphAccess`
//! public boundary.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::RwLock;

use crate::storage::codec::adjacency_codec::{AdjChainState, AdjDirection};

/// Minimum capacity, mirroring `AdjCache`'s guard against a degenerate size.
const MIN_CAPACITY: usize = 16;

const POISON: &str = "adj tail cache lock poisoned";

type Key = (u64, AdjDirection);

struct Inner {
    map: HashMap<Key, AdjChainState>,
    /// Insertion order of currently-present keys, for FIFO eviction. A key is
    /// pushed only when first inserted; updates to an existing key do not
    /// re-enqueue it, so a hot node rewritten repeatedly does not evict the rest.
    order: VecDeque<Key>,
}

/// Bounded, internal cache mapping `(node_id, direction)` to the chain's tail
/// [`AdjChainState`]. Eviction is FIFO on overflow; correctness never depends on
/// retention, so the simplest bounded policy suffices (a miss recomputes the
/// state safely).
pub struct AdjTailCache {
    inner: RwLock<Inner>,
    capacity: usize,
}

impl AdjTailCache {
    /// Creates a cache with the given capacity, clamped to [`MIN_CAPACITY`].
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        let capacity = capacity.max(MIN_CAPACITY);
        Self {
            inner: RwLock::new(Inner {
                map: HashMap::with_capacity(capacity),
                order: VecDeque::with_capacity(capacity),
            }),
            capacity,
        }
    }

    /// Returns the cached tail state for `(node_id, direction)`, or `None` on a
    /// miss.
    #[must_use]
    pub fn get(&self, node_id: u64, direction: AdjDirection) -> Option<AdjChainState> {
        let inner = self.inner.read().expect(POISON);
        let state = inner.map.get(&(node_id, direction)).copied();
        drop(inner);
        state
    }

    /// Inserts or updates the tail state for `(node_id, direction)`. On overflow
    /// evicts the oldest-inserted entry. Updating an existing key does not change
    /// its eviction order.
    pub fn insert(&self, node_id: u64, direction: AdjDirection, state: AdjChainState) {
        let key = (node_id, direction);
        let mut inner = self.inner.write().expect(POISON);
        if inner.map.insert(key, state).is_none() {
            // New key: enqueue and evict if over capacity.
            inner.order.push_back(key);
            while inner.order.len() > self.capacity {
                if let Some(evicted) = inner.order.pop_front() {
                    inner.map.remove(&evicted);
                }
            }
        }
        drop(inner);
    }

    /// Removes both directions of `node_id` from the cache (used on node
    /// deletion or when a chain is rewritten by a delete).
    pub fn remove(&self, node_id: u64) {
        let mut inner = self.inner.write().expect(POISON);
        for direction in [AdjDirection::Outgoing, AdjDirection::Incoming] {
            let key = (node_id, direction);
            if inner.map.remove(&key).is_some() {
                inner.order.retain(|k| *k != key);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(first: u32, total: usize) -> AdjChainState {
        AdjChainState {
            first_page_id: first,
            last_page_id: first,
            total_edges: total,
            last_page_used_slots: total,
            is_single: true,
        }
    }

    #[test]
    fn insert_and_get_returns_state() {
        let cache = AdjTailCache::new(16);
        cache.insert(5, AdjDirection::Outgoing, state(1, 3));
        let got = cache.get(5, AdjDirection::Outgoing).unwrap();
        assert_eq!(got.total_edges, 3);
        assert_eq!(got.first_page_id, 1);
    }

    #[test]
    fn get_miss_returns_none() {
        let cache = AdjTailCache::new(16);
        assert!(cache.get(99, AdjDirection::Outgoing).is_none());
    }

    #[test]
    fn separate_entries_per_direction() {
        let cache = AdjTailCache::new(16);
        cache.insert(7, AdjDirection::Outgoing, state(1, 10));
        cache.insert(7, AdjDirection::Incoming, state(2, 20));
        assert_eq!(cache.get(7, AdjDirection::Outgoing).unwrap().total_edges, 10);
        assert_eq!(cache.get(7, AdjDirection::Incoming).unwrap().total_edges, 20);
    }

    #[test]
    fn remove_deletes_both_directions() {
        let cache = AdjTailCache::new(16);
        cache.insert(3, AdjDirection::Outgoing, state(1, 5));
        cache.insert(3, AdjDirection::Incoming, state(2, 6));
        cache.remove(3);
        assert!(cache.get(3, AdjDirection::Outgoing).is_none());
        assert!(cache.get(3, AdjDirection::Incoming).is_none());
    }

    #[test]
    fn eviction_when_at_capacity_drops_oldest() {
        let cache = AdjTailCache::new(16); // clamped to MIN_CAPACITY = 16
        // Insert one more than capacity; the first-inserted key is evicted.
        for i in 0..17u64 {
            // Test fixture: `i` runs over a literal range.
            #[allow(clippy::cast_possible_truncation)]
            cache.insert(i, AdjDirection::Outgoing, state(1, i as usize));
        }
        // Key 0 (oldest) evicted; key 16 (newest) present.
        assert!(cache.get(0, AdjDirection::Outgoing).is_none());
        assert!(cache.get(16, AdjDirection::Outgoing).is_some());
    }

    #[test]
    fn reinsert_same_key_does_not_grow_eviction_queue() {
        // Updating an existing key must not count as a new entry for eviction,
        // else a hot node rewritten many times would evict everything else.
        let cache = AdjTailCache::new(16);
        for _ in 0..100 {
            cache.insert(1, AdjDirection::Outgoing, state(1, 1));
        }
        cache.insert(2, AdjDirection::Outgoing, state(1, 1));
        assert!(cache.get(1, AdjDirection::Outgoing).is_some());
        assert!(cache.get(2, AdjDirection::Outgoing).is_some());
    }
}
