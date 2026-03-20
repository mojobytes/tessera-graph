use std::collections::HashSet;
use std::sync::Arc;

/// Immutable snapshot of the committed transaction set at a point in time.
///
/// A transaction with `SnapshotIsolation` sees exactly the transactions that
/// were committed when its snapshot was taken, plus its own writes.
///
/// Uses `Arc<HashSet<u64>>` internally so that creating a snapshot is O(1)
/// (a single atomic increment) regardless of how many transactions have
/// committed.
#[derive(Debug, Clone)]
pub struct Snapshot {
    committed_at_begin: Arc<HashSet<u64>>,
    owner_txn_id: u64,
}

impl Snapshot {
    /// Creates a new snapshot capturing the given committed set.
    pub(crate) const fn new(committed_at_begin: Arc<HashSet<u64>>, owner_txn_id: u64) -> Self {
        Self {
            committed_at_begin,
            owner_txn_id,
        }
    }

    /// Returns `true` if data written by `writer_txn_id` is visible
    /// to this snapshot.
    #[must_use]
    pub fn is_visible(&self, writer_txn_id: u64) -> bool {
        writer_txn_id == self.owner_txn_id || self.committed_at_begin.contains(&writer_txn_id)
    }

    /// Returns the number of committed transactions visible to this snapshot.
    #[must_use]
    pub fn committed_count(&self) -> usize {
        self.committed_at_begin.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn own_writes_visible() {
        let snap = Snapshot::new(Arc::new(HashSet::new()), 10);
        assert!(snap.is_visible(10));
    }

    #[test]
    fn committed_writes_visible() {
        let committed = Arc::new(HashSet::from([1, 2, 3]));
        let snap = Snapshot::new(committed, 10);
        assert!(snap.is_visible(1));
        assert!(snap.is_visible(2));
        assert!(snap.is_visible(3));
    }

    #[test]
    fn unknown_writes_not_visible() {
        let committed = Arc::new(HashSet::from([1, 2]));
        let snap = Snapshot::new(committed, 10);
        assert!(!snap.is_visible(99));
    }

    #[test]
    fn snapshot_debug_does_not_panic() {
        let snap = Snapshot::new(Arc::new(HashSet::new()), 42);
        let _ = format!("{snap:?}");
    }

    #[test]
    fn committed_count_matches() {
        let committed = Arc::new(HashSet::from([1, 2, 3]));
        let snap = Snapshot::new(committed, 10);
        assert_eq!(snap.committed_count(), 3);
    }
}
