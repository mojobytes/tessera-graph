// SPDX-License-Identifier: MIT

use std::collections::HashMap;
use std::sync::RwLock;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::storage::codec::adjacency_codec::AdjacencyPointer;

/// Minimum cache capacity to avoid degenerate behavior.
const MIN_CAPACITY: usize = 8;

/// Lock-poison message for the cache's `RwLock`.
const LOCK_POISON_MSG: &str = "adj_cache lock poisoned";

/// A single slot in the clock-hand ring buffer.
///
/// `AdjacencyPointer` is `Copy`, so no `Arc` wrapping is needed.
/// The `AtomicBool` can be set by readers holding only a read lock,
/// avoiding write-lock contention on lookups.
struct CacheSlot {
    node_id: u64,
    ptr: AdjacencyPointer,
    recently_used: AtomicBool,
}

/// Internal mutable state of the adjacency cache.
struct CacheInner {
    /// Maps `node_id` → slot index for O(1) lookup.
    map: HashMap<u64, usize>,
    /// Clock-hand ring buffer of slots.
    slots: Vec<Option<CacheSlot>>,
    /// Number of occupied slots.
    count: usize,
    /// Current clock-hand position for eviction scanning.
    clock_hand: usize,
}

/// Bounded clock-hand cache for adjacency pointers.
///
/// Uses a read lock for `get()` and a write lock only for `insert()`,
/// `remove()`, and `clear()`. The clock-hand eviction approximates LRU
/// with lower contention than a true LRU queue because reads only need
/// a shared lock to set the `recently_used` flag atomically.
pub struct AdjCache {
    inner: RwLock<CacheInner>,
    capacity: usize,
}

impl AdjCache {
    /// Creates a new cache with the given capacity (clamped to [`MIN_CAPACITY`]).
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        let capacity = capacity.max(MIN_CAPACITY);
        Self {
            inner: RwLock::new(CacheInner {
                map: HashMap::with_capacity(capacity),
                slots: Vec::with_capacity(capacity),
                count: 0,
                clock_hand: 0,
            }),
            capacity,
        }
    }

    /// Looks up a node's adjacency pointer, marking it as recently used.
    ///
    /// Only acquires a **read lock**, so concurrent readers never block
    /// each other. Returns `None` on cache miss.
    pub fn get(&self, node_id: u64) -> Option<AdjacencyPointer> {
        let inner = self.inner.read().expect(LOCK_POISON_MSG);
        if let Some(&slot_idx) = inner.map.get(&node_id)
            && let Some(slot) = &inner.slots[slot_idx]
        {
            slot.recently_used.store(true, Ordering::Relaxed);
            return Some(slot.ptr);
        }
        drop(inner);
        None
    }

    /// Inserts or updates an adjacency pointer. Evicts via clock-hand if at capacity.
    pub fn insert(&self, node_id: u64, ptr: AdjacencyPointer) {
        let mut inner = self.inner.write().expect(LOCK_POISON_MSG);

        // Update existing entry.
        if let Some(&slot_idx) = inner.map.get(&node_id) {
            inner.slots[slot_idx] = Some(CacheSlot {
                node_id,
                ptr,
                recently_used: AtomicBool::new(true),
            });
            return;
        }

        // Find a free slot or evict via clock hand.
        let slot_idx = if inner.count < self.capacity {
            if inner.slots.len() < self.capacity {
                // Room to grow the ring buffer.
                inner.slots.push(None);
                inner.slots.len() - 1
            } else {
                // Ring buffer is full-sized but has None holes from remove().
                inner.slots.iter().position(Option::is_none).expect(
                    "free slot must exist when count < capacity and slots.len() == capacity",
                )
            }
        } else {
            // At capacity — clock-hand eviction.
            Self::clock_sweep(&mut inner)
        };

        inner.slots[slot_idx] = Some(CacheSlot {
            node_id,
            ptr,
            recently_used: AtomicBool::new(false),
        });
        inner.map.insert(node_id, slot_idx);
        inner.count += 1;
    }

    /// Removes an entry from the cache (used when a node is deleted).
    pub fn remove(&self, node_id: u64) {
        let mut inner = self.inner.write().expect(LOCK_POISON_MSG);
        if let Some(slot_idx) = inner.map.remove(&node_id) {
            inner.slots[slot_idx] = None;
            inner.count -= 1;
        }
    }

    /// Clears all entries.
    #[cfg(test)]
    pub fn clear(&self) {
        let mut inner = self.inner.write().expect(LOCK_POISON_MSG);
        inner.map.clear();
        inner.slots.clear();
        inner.count = 0;
        inner.clock_hand = 0;
    }

    /// Returns the number of entries currently in the cache.
    ///
    /// Useful for monitoring cache occupancy. No production callers yet —
    /// will be wired when monitoring integration is implemented.
    #[must_use]
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.inner.read().expect(LOCK_POISON_MSG).count
    }

    /// Returns true if the cache has no entries.
    #[must_use]
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.inner.read().expect(LOCK_POISON_MSG).count == 0
    }

    /// Returns the configured capacity.
    #[must_use]
    pub const fn capacity(&self) -> usize {
        self.capacity
    }

    /// Clock-hand sweep: find a slot to evict. Entries marked `recently_used`
    /// get a second chance (flag cleared, hand moves on). The first entry
    /// with `recently_used == false` is evicted.
    ///
    /// Guarantees termination: at most `2 * len` iterations. The first pass
    /// clears all `recently_used` flags; the second pass finds a victim.
    fn clock_sweep(inner: &mut CacheInner) -> usize {
        let len = inner.slots.len();
        for _ in 0..2 * len {
            let hand = inner.clock_hand % len;
            inner.clock_hand = (hand + 1) % len;
            if let Some(slot) = &inner.slots[hand] {
                if slot.recently_used.swap(false, Ordering::Relaxed) {
                    continue; // Second chance — spare this entry.
                }
                // Evict this slot.
                inner.map.remove(&slot.node_id);
                inner.count -= 1;
                return hand;
            }
        }
        unreachable!("clock_sweep: no evictable slot found after 2 full passes")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ptr(out: Option<u32>, inc: Option<u32>) -> AdjacencyPointer {
        AdjacencyPointer {
            outgoing_page: out,
            incoming_page: inc,
        }
    }

    #[test]
    fn insert_and_get_returns_pointer() {
        let cache = AdjCache::new(16);
        let p = ptr(Some(0), Some(1));
        cache.insert(42, p);
        let got = cache.get(42).unwrap();
        assert_eq!(got.outgoing_page, Some(0));
        assert_eq!(got.incoming_page, Some(1));
    }

    #[test]
    fn get_miss_returns_none() {
        let cache = AdjCache::new(16);
        assert!(cache.get(999).is_none());
    }

    #[test]
    fn eviction_when_at_capacity() {
        let cache = AdjCache::new(8);
        for i in 0..8 {
            // Test fixture: `i` runs over a literal range far below u32.
            #[allow(clippy::cast_possible_truncation)]
            cache.insert(i, ptr(Some(i as u32), None));
        }
        assert_eq!(cache.len(), 8);

        // Insert one more — should evict something
        cache.insert(100, ptr(Some(100), None));
        assert_eq!(cache.len(), 8);
        assert!(cache.get(100).is_some());
    }

    #[test]
    fn capacity_zero_is_clamped_to_minimum() {
        let cache = AdjCache::new(0);
        assert_eq!(cache.capacity(), MIN_CAPACITY);
    }

    #[test]
    fn evicted_entry_on_get_returns_none() {
        let cache = AdjCache::new(8);
        for i in 0..8 {
            // Test fixture: `i` runs over a literal range far below u32.
            #[allow(clippy::cast_possible_truncation)]
            cache.insert(i, ptr(Some(i as u32), None));
        }

        // Access node_id=0 to give it a second chance
        cache.get(0);

        // Insert one more — node_id=0 should survive (recently used)
        cache.insert(99, ptr(Some(99), None));
        assert!(
            cache.get(0).is_some(),
            "node 0 was recently used, should survive"
        );
        assert!(cache.get(99).is_some());
    }

    #[test]
    fn remove_deletes_entry() {
        let cache = AdjCache::new(16);
        cache.insert(1, ptr(Some(0), None));
        cache.remove(1);
        assert!(cache.get(1).is_none());
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn remove_nonexistent_is_noop() {
        let cache = AdjCache::new(16);
        cache.remove(999); // should not panic
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn clear_empties_cache() {
        let cache = AdjCache::new(16);
        cache.insert(1, ptr(Some(0), None));
        cache.insert(2, ptr(Some(1), None));
        cache.clear();
        assert!(cache.is_empty());
    }

    #[test]
    fn insert_same_key_updates_value() {
        let cache = AdjCache::new(16);
        cache.insert(1, ptr(Some(0), None));
        cache.insert(1, ptr(Some(99), Some(88)));
        let got = cache.get(1).unwrap();
        assert_eq!(got.outgoing_page, Some(99));
        assert_eq!(got.incoming_page, Some(88));
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn adj_cache_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<AdjCache>();
    }

    #[test]
    fn get_takes_shared_ref() {
        let cache = AdjCache::new(16);
        cache.insert(1, ptr(Some(0), None));
        let cache_ref: &AdjCache = &cache;
        assert!(cache_ref.get(1).is_some());
    }

    // --- Cycle G-C1: Clock-hand eviction, read lock for get() ---

    #[test]
    fn concurrent_reads_do_not_block() {
        use std::sync::Arc;
        use std::thread;

        let cache = Arc::new(AdjCache::new(64));
        cache.insert(1, ptr(Some(0), None));
        cache.insert(2, ptr(Some(1), None));

        let c1 = Arc::clone(&cache);
        let c2 = Arc::clone(&cache);

        let t1 = thread::spawn(move || {
            for _ in 0..1_000 {
                c1.get(1);
            }
        });
        let t2 = thread::spawn(move || {
            for _ in 0..1_000 {
                c2.get(2);
            }
        });

        t1.join().unwrap();
        t2.join().unwrap();
    }

    #[test]
    fn clock_hand_evicts_unreferenced_entry() {
        let cache = AdjCache::new(8);
        for i in 0..8 {
            // Test fixture: `i` runs over a literal range far below u32.
            #[allow(clippy::cast_possible_truncation)]
            cache.insert(i, ptr(Some(i as u32), None));
        }

        // Access nodes 1-7 so they are marked recently_used; node 0 is not.
        for i in 1..8 {
            cache.get(i);
        }

        // Insert 9th entry — clock hand should evict node 0 (not recently used).
        cache.insert(100, ptr(Some(100), None));

        assert!(cache.get(0).is_none(), "node 0 should have been evicted");
        assert!(cache.get(100).is_some());
    }
}
