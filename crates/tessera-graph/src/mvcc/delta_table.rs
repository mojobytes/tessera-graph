// SPDX-License-Identifier: Apache-2.0

//! The sharded delta table: maps each record to its in-memory delta chain.

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::RwLock;

use crate::error::{EdgeId, NodeId};

use super::chain::DeltaChainHead;
use super::delta::Delta;

/// A key into the delta table.
///
/// `Node` and `Edge` occupy distinct namespaces even when their underlying
/// `u64` coincide, so a `NodeId(1)` and an `EdgeId(1)` never collide.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EntityKey {
    /// Keys the delta chain of a node.
    Node(NodeId),
    /// Keys the delta chain of an edge.
    Edge(EdgeId),
}

/// Maps each record ([`EntityKey`]) to its delta chain, sharded to spread lock
/// contention across the key space.
///
/// Sharding uses only `std::sync` primitives (a `Vec` of `RwLock<HashMap>`),
/// keeping the crate's `unsafe_code = "forbid"` guarantee: parallelism comes
/// from partitioning keys into independent shards, not from a lock-free crate.
/// Each shard is held only for the microseconds of a single chain operation.
#[derive(Debug)]
pub struct DeltaTable {
    shards: Vec<RwLock<HashMap<EntityKey, DeltaChainHead>>>,
}

impl DeltaTable {
    /// Creates a table with `shard_count` shards. `shard_count` must be at
    /// least 1; callers pass a power of two (see `MVCC_SHARD_COUNT`).
    pub fn new(shard_count: usize) -> Self {
        let shard_count = shard_count.max(1);
        let mut shards = Vec::with_capacity(shard_count);
        for _ in 0..shard_count {
            shards.push(RwLock::new(HashMap::new()));
        }
        Self { shards }
    }

    /// Prepends `delta` to the chain for `key`, creating the chain if absent.
    pub fn push_delta(&self, key: EntityKey, delta: Delta) {
        let shard = &self.shards[shard_index(key, self.shards.len())];
        shard
            .write()
            .expect("delta table shard lock poisoned")
            .entry(key)
            .or_default()
            .push(delta);
    }

    /// Returns a clone of the chain for `key`, or `None` if the record has no
    /// deltas. Cloning under the read lock keeps the lock's hold to the copy
    /// itself, mirroring how [`crate::storage::buffer_pool::BufferPool`] returns
    /// an owned page rather than a guard.
    pub fn chain_for(&self, key: EntityKey) -> Option<DeltaChainHead> {
        let shard = &self.shards[shard_index(key, self.shards.len())];
        shard
            .read()
            .expect("delta table shard lock poisoned")
            .get(&key)
            .cloned()
    }

    /// Stamps `key`'s uncommitted deltas authored by `txn_id` with `commit_ts`,
    /// making them visible. No-op if `key` has no chain. Used by commit.
    pub fn stamp_commit_for_txn(&self, key: EntityKey, txn_id: u64, commit_ts: u64) {
        let shard = &self.shards[shard_index(key, self.shards.len())];
        if let Some(chain) = shard
            .write()
            .expect("delta table shard lock poisoned")
            .get_mut(&key)
        {
            chain.stamp_txn(txn_id, commit_ts);
        }
    }

    /// Returns the newest delta `txn_id` authored on `key`'s chain, or `None`
    /// if it wrote nothing there. Used by commit to source the durable WAL redo.
    pub fn newest_delta_of_txn(&self, key: EntityKey, txn_id: u64) -> Option<Delta> {
        let shard = &self.shards[shard_index(key, self.shards.len())];
        shard
            .read()
            .expect("delta table shard lock poisoned")
            .get(&key)
            .and_then(|chain| chain.newest_delta_of_txn(txn_id))
    }

    /// Returns the oldest delta `txn_id` authored on `key`'s chain, or `None`
    /// if it wrote nothing there. Used by commit's category-B reconciliation to
    /// determine the transaction's net op versus the committed base.
    pub fn oldest_delta_of_txn(&self, key: EntityKey, txn_id: u64) -> Option<Delta> {
        let shard = &self.shards[shard_index(key, self.shards.len())];
        shard
            .read()
            .expect("delta table shard lock poisoned")
            .get(&key)
            .and_then(|chain| chain.oldest_delta_of_txn(txn_id))
    }

    /// Extracts and removes every chain that is fully safe to vacuum for
    /// `watermark`, returning `(key, newest visible state, oldest prior state)`
    /// for each.
    ///
    /// A chain is drained only when ALL its deltas satisfy
    /// [`Delta::safe_to_vacuum`](super::delta::Delta::safe_to_vacuum): a single
    /// uncommitted delta or a delta with `commit_ts >= watermark` keeps the
    /// whole chain in place (a live reader might still resolve its snapshot
    /// through it). The newest state is the newest delta's `new_state` — a
    /// `Some(EntitySnapshot::Node|Edge)` to materialize, `Some(Deleted)` to
    /// tombstone, or `None` (a delta with no new state, treated as a delete).
    /// The oldest prior is the oldest delta's `prior` — the committed base the
    /// materialized chain replaced, which the vacuum needs to remove the stale
    /// How many delta chains the table currently holds, across every shard.
    ///
    /// The observable size of the memory committed versions still occupy: it
    /// drops as the vacuum materialises chains to their pages. Exposed so a
    /// caller outside the engine can tell a server that reclaims this memory
    /// from one that leaks it — a distinction that has no other symptom until
    /// the process runs out of memory.
    ///
    /// Reads each shard in turn without holding them all at once, so a
    /// concurrent writer may land between shards. It is a diagnostic, not a
    /// synchronisation point.
    pub fn chain_count(&self) -> usize {
        self.shards
            .iter()
            .map(|shard| {
                shard
                    .read()
                    .expect("delta table shard lock poisoned")
                    .len()
            })
            .sum()
    }

    /// category-B entries (a delete's index/exists removal, or an update's
    /// old-value index entries).
    ///
    /// The caller (the vacuum) materializes each returned state to its page and
    /// then applies the category-B baja implied by the pair. Draining under the
    /// shard write lock keeps the extract-and-remove atomic per shard.
    pub fn drain_vacuumable(
        &self,
        watermark: Option<u64>,
    ) -> Vec<(
        EntityKey,
        Option<super::delta::EntitySnapshot>,
        Option<super::delta::EntitySnapshot>,
    )> {
        let mut out = Vec::new();
        for shard in &self.shards {
            let mut guard = shard.write().expect("delta table shard lock poisoned");
            let vacuumable: Vec<EntityKey> = guard
                .iter()
                .filter(|(_, chain)| chain.all_safe_to_vacuum(watermark))
                .map(|(key, _)| *key)
                .collect();
            for key in vacuumable {
                if let Some(chain) = guard.remove(&key) {
                    let newest = chain.newest().and_then(|d| d.new_state().cloned());
                    let oldest_prior = chain.oldest().and_then(|d| d.prior().cloned());
                    out.push((key, newest, oldest_prior));
                }
            }
        }
        out
    }

    /// Removes `key`'s deltas authored by `txn_id`. If the chain becomes empty,
    /// drops the map entry so aborted transactions leave no residue. Used by
    /// rollback.
    pub fn remove_deltas_for_txn(&self, key: EntityKey, txn_id: u64) {
        let shard = &self.shards[shard_index(key, self.shards.len())];
        let mut guard = shard.write().expect("delta table shard lock poisoned");
        if let Some(chain) = guard.get_mut(&key) {
            chain.remove_txn(txn_id);
            if chain.is_empty() {
                guard.remove(&key);
            }
        }
    }
}

/// Maps `key` to a shard index in `[0, len)`. `len` is assumed non-zero.
fn shard_index(key: EntityKey, len: usize) -> usize {
    let mut hasher = DefaultHasher::new();
    key.hash(&mut hasher);
    // Modulo `len` first, so the result is a valid index into a live Vec.
    #[allow(clippy::cast_possible_truncation)]
    let idx = (hasher.finish() % len as u64) as usize;
    idx
}

#[cfg(test)]
mod tests {
    use super::{shard_index, DeltaTable, EntityKey};
    use crate::error::{EdgeId, NodeId};
    use crate::mvcc::delta::{Delta, DeltaOp};

    #[test]
    fn delta_table_roundtrip_insert_and_get() {
        let table = DeltaTable::new(16);
        let key = EntityKey::Node(NodeId(5));
        table.push_delta(key, Delta::new(1, None, None, DeltaOp::Insert));
        let chain = table.chain_for(key);
        assert!(chain.is_some());
        assert_eq!(chain.unwrap().iter().count(), 1);
    }

    #[test]
    fn delta_table_missing_key_returns_none() {
        let table = DeltaTable::new(16);
        assert!(table.chain_for(EntityKey::Node(NodeId(999))).is_none());
    }

    #[test]
    fn delta_table_distinct_keys_do_not_collide() {
        let table = DeltaTable::new(16);
        table.push_delta(
            EntityKey::Node(NodeId(1)),
            Delta::new(1, None, None, DeltaOp::Insert),
        );
        table.push_delta(
            EntityKey::Edge(EdgeId(1)),
            Delta::new(2, None, None, DeltaOp::Insert),
        );
        assert_eq!(
            table
                .chain_for(EntityKey::Node(NodeId(1)))
                .unwrap()
                .iter()
                .count(),
            1
        );
        assert_eq!(
            table
                .chain_for(EntityKey::Edge(EdgeId(1)))
                .unwrap()
                .iter()
                .count(),
            1
        );
    }

    #[test]
    fn shard_index_is_within_bounds() {
        for raw in 0..1000u64 {
            let idx = shard_index(EntityKey::Node(NodeId(raw)), 16);
            assert!(idx < 16);
        }
    }

    use crate::mvcc::delta::EntitySnapshot;
    use crate::node::Node;
    use crate::property::Properties;

    fn committed_node_delta(commit_ts: u64, id: u64, label: &str) -> Delta {
        let node = Node::new(NodeId(id), label, Properties::new());
        let mut d = Delta::new(1, None, Some(EntitySnapshot::Node(node)), DeltaOp::Insert);
        d.stamp_commit(commit_ts);
        d
    }

    #[test]
    fn drain_vacuumable_removes_fully_safe_chain_and_returns_newest_state() {
        let table = DeltaTable::new(16);
        let key = EntityKey::Node(NodeId(5));
        table.push_delta(key, committed_node_delta(2, 5, "Old"));
        table.push_delta(key, committed_node_delta(3, 5, "New")); // newest

        let drained = table.drain_vacuumable(None); // no live txn: all safe
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].0, key);
        match &drained[0].1 {
            Some(EntitySnapshot::Node(n)) => assert_eq!(n.label(), "New"),
            other => panic!("expected newest node state, got {other:?}"),
        }
        // Chain is gone after draining.
        assert!(table.chain_for(key).is_none());
    }

    #[test]
    fn drain_vacuumable_keeps_chain_with_a_delta_at_or_after_watermark() {
        let table = DeltaTable::new(16);
        let key = EntityKey::Node(NodeId(7));
        table.push_delta(key, committed_node_delta(2, 7, "Safe"));
        table.push_delta(key, committed_node_delta(6, 7, "TooNew")); // commit_ts == watermark

        let drained = table.drain_vacuumable(Some(6));
        assert!(drained.is_empty(), "chain with commit_ts >= watermark must be kept");
        assert!(table.chain_for(key).is_some());
    }

    #[test]
    fn drain_vacuumable_keeps_chain_with_uncommitted_delta() {
        let table = DeltaTable::new(16);
        let key = EntityKey::Node(NodeId(9));
        table.push_delta(key, committed_node_delta(2, 9, "Committed"));
        // An uncommitted delta from another still-open txn on the same key.
        table.push_delta(
            key,
            Delta::new(2, None, Some(EntitySnapshot::Deleted), DeltaOp::Delete),
        );

        let drained = table.drain_vacuumable(None);
        assert!(drained.is_empty(), "an uncommitted delta blocks vacuum of the chain");
        assert!(table.chain_for(key).is_some());
    }

    #[test]
    fn drain_vacuumable_returns_deleted_for_tombstone_newest() {
        let table = DeltaTable::new(16);
        let key = EntityKey::Node(NodeId(11));
        let mut del = Delta::new(1, None, Some(EntitySnapshot::Deleted), DeltaOp::Delete);
        del.stamp_commit(3);
        table.push_delta(key, del);

        let drained = table.drain_vacuumable(None);
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].1, Some(EntitySnapshot::Deleted));
    }
}
