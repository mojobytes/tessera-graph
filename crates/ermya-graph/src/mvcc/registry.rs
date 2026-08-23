// SPDX-License-Identifier: MIT

//! The active-transaction registry: tracks which transactions are live and the
//! `start_ts` snapshot each one holds.

use std::collections::{HashMap, HashSet};
use std::sync::RwLock;
use std::sync::atomic::{AtomicU64, Ordering};

use super::clock::TxnClock;
use super::delta_table::EntityKey;
use crate::storage::codec::adjacency_codec::AdjDirection;

/// Tracks live transactions and their `start_ts` snapshots.
///
/// `txn_id` (transaction identity) is drawn from a counter distinct from the
/// visibility [`TxnClock`]: identity is never used for visibility comparisons —
/// only `start_ts`/`commit_ts` are — so the two concerns stay decoupled.
///
/// The oldest live `start_ts` ([`oldest_active_start_ts`](Self::oldest_active_start_ts))
/// is the watermark the vacuum uses to decide which committed versions are safe
/// to materialize and free.
///
/// `written` is the reverse index `txn_id -> keys touched`, which commit and
/// rollback walk to stamp or discard exactly the transaction's own deltas
/// without scanning the whole delta table.
///
/// Each active entry also tracks `bytes_used`, the running estimate of the
/// memory the transaction's uncommitted deltas hold, which the per-transaction
/// memory cap ([`add_bytes`](Self::add_bytes)) charges against.
#[derive(Debug)]
pub struct TxnRegistry {
    active: RwLock<HashMap<u64, TxnState>>,
    written: RwLock<HashMap<u64, Vec<EntityKey>>>,
    overlays: RwLock<HashMap<u64, TxnOverlay>>,
    next_txn_id: AtomicU64,
}

/// Per-transaction overlay of pending inserts not yet reflected in the
/// committed `node_exists` / label index / adjacency pages, so a transaction's
/// own reads-after-writes see them by enumeration and traversal without a
/// delta-chain scan.
///
/// Born in [`begin`](TxnRegistry::begin), dropped in [`end`](TxnRegistry::end)
/// — the single cleanup point shared by commit and rollback — so the "always
/// cleared" guarantee is not duplicated across two paths.
#[derive(Debug, Default)]
struct TxnOverlay {
    /// Node ids created in this transaction, pending commit.
    node_ids: HashSet<u64>,
    /// `label` -> node ids with that label, created in this transaction.
    nodes_by_label: HashMap<String, HashSet<u64>>,
    /// `(node_id, direction)` -> edge ids pending in this transaction,
    /// adjacency-style (mirrors `Graph::adj_pending`'s shape, txn-scoped).
    adjacency: HashMap<(u64, AdjDirection), Vec<u64>>,
}

/// Per-transaction state held while a transaction is active: its snapshot
/// `start_ts` and the running estimate of memory its uncommitted deltas hold.
#[derive(Debug, Clone, Copy)]
struct TxnState {
    start_ts: u64,
    bytes_used: u64,
}

impl TxnRegistry {
    /// Creates an empty registry. The first [`begin`](Self::begin) yields
    /// `txn_id` 1.
    pub fn new() -> Self {
        Self {
            active: RwLock::new(HashMap::new()),
            written: RwLock::new(HashMap::new()),
            overlays: RwLock::new(HashMap::new()),
            next_txn_id: AtomicU64::new(1),
        }
    }

    /// Opens a transaction: allocates a fresh `txn_id`, snapshots a `start_ts`
    /// from `clock`, records the pair as active, and returns the `txn_id`.
    pub fn begin(&self, clock: &TxnClock) -> u64 {
        let txn_id = self.next_txn_id.fetch_add(1, Ordering::SeqCst);
        let start_ts = clock.next();
        self.active
            .write()
            .expect("txn registry lock poisoned")
            .insert(
                txn_id,
                TxnState {
                    start_ts,
                    bytes_used: 0,
                },
            );
        self.overlays
            .write()
            .expect("txn registry lock poisoned")
            .insert(txn_id, TxnOverlay::default());
        txn_id
    }

    /// Returns `true` while `txn_id` remains active (begun, not yet ended).
    pub fn is_active(&self, txn_id: u64) -> bool {
        self.active
            .read()
            .expect("txn registry lock poisoned")
            .contains_key(&txn_id)
    }

    /// Returns the `start_ts` of `txn_id`, or `None` if it is not active.
    pub fn start_ts(&self, txn_id: u64) -> Option<u64> {
        self.active
            .read()
            .expect("txn registry lock poisoned")
            .get(&txn_id)
            .map(|s| s.start_ts)
    }

    /// Charges `delta_bytes` against `txn_id`'s running memory estimate and
    /// returns the new total, or `None` if `txn_id` is not active.
    ///
    /// The caller (the `Graph` write path) compares the returned total against
    /// the configured cap and aborts the transaction if it is exceeded. Charging
    /// happens BEFORE the delta is pushed, so a transaction that would breach the
    /// cap never grows the delta table past the limit.
    pub fn add_bytes(&self, txn_id: u64, delta_bytes: u64) -> Option<u64> {
        let mut guard = self.active.write().expect("txn registry lock poisoned");
        let state = guard.get_mut(&txn_id)?;
        state.bytes_used = state.bytes_used.saturating_add(delta_bytes);
        let total = state.bytes_used;
        drop(guard);
        Some(total)
    }

    /// Records that `txn_id` wrote a delta for `key`, appending to its reverse
    /// index. Called from the write path after each delta is pushed, so commit
    /// and rollback can find exactly the keys this transaction touched.
    ///
    /// A key may appear more than once if the transaction wrote it repeatedly;
    /// commit/rollback dedup implicitly by operating on the chain, and rollback
    /// removing all of a txn's deltas for a key is idempotent.
    pub fn record_write(&self, txn_id: u64, key: EntityKey) {
        self.written
            .write()
            .expect("txn registry lock poisoned")
            .entry(txn_id)
            .or_default()
            .push(key);
    }

    /// Returns the keys `txn_id` wrote, in insertion order, or an empty vec if
    /// it wrote nothing.
    pub fn keys_written_by(&self, txn_id: u64) -> Vec<EntityKey> {
        self.written
            .read()
            .expect("txn registry lock poisoned")
            .get(&txn_id)
            .cloned()
            .unwrap_or_default()
    }

    /// Removes `txn_id` from the active set and drops its write index (on commit
    /// or rollback).
    pub fn end(&self, txn_id: u64) {
        self.written
            .write()
            .expect("txn registry lock poisoned")
            .remove(&txn_id);
        self.overlays
            .write()
            .expect("txn registry lock poisoned")
            .remove(&txn_id);
        self.active
            .write()
            .expect("txn registry lock poisoned")
            .remove(&txn_id);
    }

    /// Records that `txn_id` created node `id` with `label`, pending commit, so
    /// the transaction's own enumeration reads (`node_ids_in_txn`,
    /// `nodes_by_label_in_txn`) see it before it reaches the committed indexes.
    ///
    /// Silent no-op if `txn_id` is not active: the caller
    /// (`Graph::push_txn_delta`) only reaches here for a live transaction, so a
    /// missing overlay entry cannot happen in practice.
    pub fn mark_node_pending(&self, txn_id: u64, id: u64, label: &str) {
        let mut guard = self.overlays.write().expect("txn registry lock poisoned");
        let Some(overlay) = guard.get_mut(&txn_id) else {
            return;
        };
        overlay.node_ids.insert(id);
        overlay
            .nodes_by_label
            .entry(label.to_owned())
            .or_default()
            .insert(id);
        drop(guard);
    }

    /// Returns the node ids `txn_id` created and has pending, in ascending
    /// order, or an empty vec if it created none (or is not active).
    pub fn pending_node_ids(&self, txn_id: u64) -> Vec<u64> {
        let guard = self.overlays.read().expect("txn registry lock poisoned");
        let Some(overlay) = guard.get(&txn_id) else {
            return Vec::new();
        };
        let mut ids: Vec<u64> = overlay.node_ids.iter().copied().collect();
        drop(guard);
        ids.sort_unstable();
        ids
    }

    /// Returns the pending node ids `txn_id` created with `label`, in ascending
    /// order, or an empty vec if it created none with that label (or is not
    /// active).
    pub fn pending_node_ids_by_label(&self, txn_id: u64, label: &str) -> Vec<u64> {
        let guard = self.overlays.read().expect("txn registry lock poisoned");
        let Some(overlay) = guard.get(&txn_id) else {
            return Vec::new();
        };
        let Some(label_ids) = overlay.nodes_by_label.get(label) else {
            return Vec::new();
        };
        let mut ids: Vec<u64> = label_ids.iter().copied().collect();
        drop(guard);
        ids.sort_unstable();
        ids
    }

    /// Records that `txn_id` created an edge `edge_id` pending on `node_id` in
    /// `direction`, so the transaction's own traversal reads see it before it
    /// reaches the committed adjacency pages. Called once per direction
    /// (`Outgoing` on the source, `Incoming` on the target), mirroring the
    /// committed adjacency's per-direction split.
    ///
    /// Silent no-op if `txn_id` is not active (see [`mark_node_pending`](Self::mark_node_pending)).
    pub fn mark_edge_pending(
        &self,
        txn_id: u64,
        node_id: u64,
        direction: AdjDirection,
        edge_id: u64,
    ) {
        let mut guard = self.overlays.write().expect("txn registry lock poisoned");
        let Some(overlay) = guard.get_mut(&txn_id) else {
            return;
        };
        overlay
            .adjacency
            .entry((node_id, direction))
            .or_default()
            .push(edge_id);
        drop(guard);
    }

    /// Returns the edge ids `txn_id` has pending on `node_id` in `direction`, in
    /// insertion order, or an empty vec if none (or not active). The caller
    /// resolves each id through the delta chain for visibility, so ordering here
    /// only needs to be stable, not filtered.
    pub fn pending_edges_for(
        &self,
        txn_id: u64,
        node_id: u64,
        direction: AdjDirection,
    ) -> Vec<u64> {
        let guard = self.overlays.read().expect("txn registry lock poisoned");
        let Some(overlay) = guard.get(&txn_id) else {
            return Vec::new();
        };
        let edges = overlay
            .adjacency
            .get(&(node_id, direction))
            .cloned()
            .unwrap_or_default();
        drop(guard);
        edges
    }

    /// Returns the smallest `start_ts` among live transactions, or `None` if
    /// none are active. This is the vacuum watermark.
    ///
    /// Consumed by the Phase 5 vacuum to decide which committed versions no live
    /// transaction can still see.
    pub fn oldest_active_start_ts(&self) -> Option<u64> {
        self.active
            .read()
            .expect("txn registry lock poisoned")
            .values()
            .map(|s| s.start_ts)
            .min()
    }
}

#[cfg(test)]
mod tests {
    use super::TxnRegistry;
    use crate::mvcc::clock::TxnClock;

    #[test]
    fn registry_tracks_active_start_timestamps() {
        let clock = TxnClock::new();
        let registry = TxnRegistry::new();
        let txn_id = registry.begin(&clock);
        assert!(registry.is_active(txn_id));
        assert!(registry.start_ts(txn_id).is_some());
    }

    #[test]
    fn registry_end_removes_from_active() {
        let clock = TxnClock::new();
        let registry = TxnRegistry::new();
        let txn_id = registry.begin(&clock);
        registry.end(txn_id);
        assert!(!registry.is_active(txn_id));
        assert_eq!(registry.start_ts(txn_id), None);
    }

    #[test]
    fn add_bytes_accumulates_and_reports_running_total() {
        let clock = TxnClock::new();
        let registry = TxnRegistry::new();
        let txn = registry.begin(&clock);
        assert_eq!(registry.add_bytes(txn, 100), Some(100));
        assert_eq!(registry.add_bytes(txn, 50), Some(150));
        // An inactive transaction charges nothing and reports None.
        registry.end(txn);
        assert_eq!(registry.add_bytes(txn, 10), None);
    }

    #[test]
    fn overlay_is_empty_on_begin_and_removed_on_end() {
        let clock = TxnClock::new();
        let registry = TxnRegistry::new();
        let txn = registry.begin(&clock);

        assert!(registry.pending_node_ids(txn).is_empty());

        registry.mark_node_pending(txn, 1, "Person");
        assert_eq!(registry.pending_node_ids(txn), vec![1]);

        registry.end(txn);
        assert!(registry.pending_node_ids(txn).is_empty());
    }

    #[test]
    fn registry_oldest_active_start_ts_is_minimum() {
        let clock = TxnClock::new();
        let registry = TxnRegistry::new();
        let t1 = registry.begin(&clock);
        let t2 = registry.begin(&clock);
        let first = registry.start_ts(t1).unwrap();
        assert_eq!(registry.oldest_active_start_ts(), Some(first));
        registry.end(t1);
        assert_eq!(registry.oldest_active_start_ts(), registry.start_ts(t2));
        registry.end(t2);
        assert_eq!(registry.oldest_active_start_ts(), None);
    }
}
