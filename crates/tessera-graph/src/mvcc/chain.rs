// SPDX-License-Identifier: Apache-2.0

//! The delta chain for a single record: newest delta first.

use super::delta::Delta;

/// The head of a record's delta chain, ordered newest-first.
///
/// [`push`](Self::push) prepends, so [`iter`](Self::iter) yields the most
/// recent delta first — the order visibility resolution ([`super::visibility`])
/// relies on. Chains stay short in practice because the vacuum (Phase 5) prunes
/// versions no live transaction can still see.
#[derive(Debug, Default, Clone)]
pub struct DeltaChainHead(Vec<Delta>);

impl DeltaChainHead {
    /// Creates an empty chain.
    ///
    /// Production code builds chains via `Default` (`DeltaTable::push_delta`'s
    /// `or_default`); this explicit constructor is used by tests and kept as the
    /// canonical entry point.
    #[allow(dead_code)]
    pub const fn new() -> Self {
        Self(Vec::new())
    }

    /// Prepends `delta`, making it the newest (first-yielded) entry.
    pub fn push(&mut self, delta: Delta) {
        self.0.insert(0, delta);
    }

    /// Iterates the chain newest-first.
    pub fn iter(&self) -> impl Iterator<Item = &Delta> {
        self.0.iter()
    }

    /// Returns the number of deltas in the chain.
    ///
    /// Consumed by the Phase 5 vacuum (pruning short chains); `is_empty` is
    /// already used by rollback's chain cleanup.
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Returns `true` when the chain holds no deltas.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Stamps every uncommitted delta authored by `txn_id` with `commit_ts`,
    /// making them visible. Used by commit.
    pub fn stamp_txn(&mut self, txn_id: u64, commit_ts: u64) {
        for delta in &mut self.0 {
            if delta.txn_id() == txn_id && delta.commit_ts().is_none() {
                delta.stamp_commit(commit_ts);
            }
        }
    }

    /// Removes every delta authored by `txn_id` from the chain. Used by
    /// rollback to discard an aborted transaction's writes.
    pub fn remove_txn(&mut self, txn_id: u64) {
        self.0.retain(|delta| delta.txn_id() != txn_id);
    }

    /// Returns a clone of the newest delta authored by `txn_id`, or `None` if
    /// the transaction wrote nothing on this chain. Used by commit to build the
    /// durable WAL redo from the version the transaction is committing.
    pub fn newest_delta_of_txn(&self, txn_id: u64) -> Option<Delta> {
        self.0.iter().find(|d| d.txn_id() == txn_id).cloned()
    }

    /// Returns a clone of the OLDEST delta authored by `txn_id` (the first the
    /// transaction wrote on this chain), or `None` if it wrote nothing here.
    ///
    /// Commit's category-B reconciliation uses it to find the transaction's net
    /// effect versus the committed base: the oldest delta's `op` and `prior`
    /// say whether the transaction, as a whole, inserted / updated / deleted the
    /// record — independent of how many times it rewrote it afterward. The chain
    /// is newest-first, so the oldest is the last matching element.
    pub fn oldest_delta_of_txn(&self, txn_id: u64) -> Option<Delta> {
        self.0.iter().rev().find(|d| d.txn_id() == txn_id).cloned()
    }

    /// Returns the newest delta (first-yielded), or `None` if the chain is
    /// empty. Used by the vacuum to source the version to materialize once the
    /// whole chain is safe to free.
    pub fn newest(&self) -> Option<&Delta> {
        self.0.first()
    }

    /// Returns the oldest delta (last-yielded), or `None` if empty. Used by the
    /// vacuum to source the committed base a materialized chain replaced.
    pub fn oldest(&self) -> Option<&Delta> {
        self.0.last()
    }

    /// Whether every delta in the chain is safe to vacuum for `watermark`
    /// (see [`Delta::safe_to_vacuum`](crate::mvcc::delta::Delta::safe_to_vacuum)).
    /// An empty chain is trivially safe. Used by the vacuum: a chain is only
    /// materialized-and-freed when nothing in it a live reader could still need.
    pub fn all_safe_to_vacuum(&self, watermark: Option<u64>) -> bool {
        self.0.iter().all(|d| d.safe_to_vacuum(watermark))
    }
}

#[cfg(test)]
mod tests {
    use super::DeltaChainHead;
    use crate::mvcc::delta::{Delta, DeltaOp, EntitySnapshot};

    #[test]
    fn delta_chain_push_prepends_newest_first() {
        let mut head = DeltaChainHead::new();
        head.push(Delta::new(1, None, None, DeltaOp::Insert));
        head.push(Delta::new(
            2,
            Some(EntitySnapshot::Deleted),
            None,
            DeltaOp::Update,
        ));
        let ids: Vec<u64> = head.iter().map(Delta::txn_id).collect();
        assert_eq!(ids, vec![2, 1]);
    }

    #[test]
    fn delta_chain_is_empty_when_new() {
        let head = DeltaChainHead::new();
        assert!(head.is_empty());
        assert!(head.iter().next().is_none());
    }

    #[test]
    fn oldest_delta_of_txn_returns_the_first_written() {
        // A txn that inserts then updates the same record: the OLDEST delta
        // (the insert) determines the net op vs the committed base.
        let mut head = DeltaChainHead::new();
        head.push(Delta::new(5, None, None, DeltaOp::Insert)); // written first
        head.push(Delta::new(5, None, None, DeltaOp::Update)); // written second (newest)
        let oldest = head.oldest_delta_of_txn(5).unwrap();
        assert_eq!(oldest.op(), DeltaOp::Insert);
    }

    #[test]
    fn oldest_delta_of_txn_absent_returns_none() {
        let mut head = DeltaChainHead::new();
        head.push(Delta::new(1, None, None, DeltaOp::Insert));
        assert!(head.oldest_delta_of_txn(2).is_none());
    }

    #[test]
    fn delta_chain_len_counts_pushes() {
        let mut head = DeltaChainHead::new();
        assert_eq!(head.len(), 0);
        head.push(Delta::new(1, None, None, DeltaOp::Insert));
        head.push(Delta::new(2, None, None, DeltaOp::Insert));
        head.push(Delta::new(3, None, None, DeltaOp::Insert));
        assert_eq!(head.len(), 3);
        assert!(!head.is_empty());
        let ids: Vec<u64> = head.iter().map(Delta::txn_id).collect();
        assert_eq!(ids, vec![3, 2, 1]);
    }

    #[test]
    fn oldest_returns_last_pushed() {
        let mut head = DeltaChainHead::new();
        head.push(Delta::new(1, None, None, DeltaOp::Insert));
        head.push(Delta::new(2, None, None, DeltaOp::Update));
        assert_eq!(head.oldest().unwrap().txn_id(), 1);
    }
}
