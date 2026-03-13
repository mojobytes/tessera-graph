use std::collections::HashSet;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::RwLock;

use tessera_graph::{WalRecord, WalWriter};

use super::handle::{IsolationLevel, TransactionHandle, TxnState};
use super::snapshot::Snapshot;
use crate::error::{EnterpriseError, Result};

/// Manages transaction lifecycle: begin, commit, rollback.
///
/// Thread-safe: uses `AtomicU64` for ID generation and `RwLock` for
/// the commit log. Multiple threads can begin/commit concurrently.
pub struct TransactionManager {
    next_txn_id: AtomicU64,
    committed: RwLock<HashSet<u64>>,
}

impl Default for TransactionManager {
    fn default() -> Self {
        Self::new()
    }
}

impl TransactionManager {
    /// Creates a new transaction manager with empty commit log.
    #[must_use]
    pub fn new() -> Self {
        Self {
            next_txn_id: AtomicU64::new(1),
            committed: RwLock::new(HashSet::new()),
        }
    }

    /// Begins a new transaction with the given isolation level.
    ///
    /// For `SnapshotIsolation`, a snapshot of the current committed set
    /// is captured at this point. For `ReadCommitted`, no snapshot is taken.
    ///
    /// # Panics
    ///
    /// Panics if the commit log lock is poisoned.
    pub fn begin(&self, isolation: IsolationLevel) -> TransactionHandle {
        let txn_id = self.next_txn_id.fetch_add(1, Ordering::Relaxed);

        let snapshot = match isolation {
            IsolationLevel::SnapshotIsolation => {
                let committed = self
                    .committed
                    .read()
                    .expect("commit log lock poisoned")
                    .clone();
                Some(Snapshot::new(committed, txn_id))
            }
            IsolationLevel::ReadCommitted => None,
        };

        TransactionHandle {
            txn_id,
            isolation,
            state: TxnState::Active,
            snapshot,
        }
    }

    /// Commits the transaction: writes a `Commit` record to the WAL
    /// and marks it in the commit log.
    ///
    /// # Errors
    ///
    /// Returns [`EnterpriseError::TransactionNotActive`] if the handle
    /// is not in `Active` state. Returns I/O errors from WAL writes.
    ///
    /// # Panics
    ///
    /// Panics if the commit log lock is poisoned.
    pub fn commit(
        &self,
        handle: &mut TransactionHandle,
        wal_path: &Path,
    ) -> Result<()> {
        if handle.state != TxnState::Active {
            return Err(EnterpriseError::TransactionNotActive(handle.txn_id));
        }

        let mut wal = WalWriter::open(wal_path)?;
        wal.append(WalRecord::Commit {
            lsn: 0,
            txn_id: handle.txn_id,
        })
        .map_err(EnterpriseError::Graph)?;
        wal.sync().map_err(EnterpriseError::Graph)?;

        self.committed
            .write()
            .expect("commit log lock poisoned")
            .insert(handle.txn_id);

        handle.state = TxnState::Committed;
        Ok(())
    }

    /// Rolls back the transaction: writes a `Rollback` record to the WAL
    /// without adding it to the commit log.
    ///
    /// # Errors
    ///
    /// Returns [`EnterpriseError::TransactionNotActive`] if the handle
    /// is not in `Active` state. Returns I/O errors from WAL writes.
    pub fn rollback(
        &self,
        handle: &mut TransactionHandle,
        wal_path: &Path,
    ) -> Result<()> {
        if handle.state != TxnState::Active {
            return Err(EnterpriseError::TransactionNotActive(handle.txn_id));
        }

        let mut wal = WalWriter::open(wal_path)?;
        wal.append(WalRecord::Rollback {
            lsn: 0,
            txn_id: handle.txn_id,
        })
        .map_err(EnterpriseError::Graph)?;
        wal.sync().map_err(EnterpriseError::Graph)?;

        handle.state = TxnState::RolledBack;
        Ok(())
    }

    /// Returns `true` if the given transaction has been committed.
    ///
    /// # Panics
    ///
    /// Panics if the commit log lock is poisoned.
    #[must_use]
    pub fn is_committed(&self, txn_id: u64) -> bool {
        self.committed
            .read()
            .expect("commit log lock poisoned")
            .contains(&txn_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    #[test]
    fn begin_returns_handle_with_unique_id() {
        let mgr = TransactionManager::new();
        let t1 = mgr.begin(IsolationLevel::ReadCommitted);
        let t2 = mgr.begin(IsolationLevel::ReadCommitted);
        assert_ne!(t1.txn_id(), t2.txn_id());
    }

    #[test]
    fn commit_transitions_state_and_marks_committed() {
        let tmp = NamedTempFile::new().unwrap();
        let mgr = TransactionManager::new();
        let mut handle = mgr.begin(IsolationLevel::ReadCommitted);
        let id = handle.txn_id();

        assert!(!mgr.is_committed(id));
        mgr.commit(&mut handle, tmp.path()).unwrap();
        assert_eq!(handle.state(), TxnState::Committed);
        assert!(mgr.is_committed(id));
    }

    #[test]
    fn rollback_transitions_state_and_not_committed() {
        let tmp = NamedTempFile::new().unwrap();
        let mgr = TransactionManager::new();
        let mut handle = mgr.begin(IsolationLevel::ReadCommitted);
        let id = handle.txn_id();

        mgr.rollback(&mut handle, tmp.path()).unwrap();
        assert_eq!(handle.state(), TxnState::RolledBack);
        assert!(!mgr.is_committed(id));
    }

    #[test]
    fn double_commit_returns_error() {
        let tmp = NamedTempFile::new().unwrap();
        let mgr = TransactionManager::new();
        let mut handle = mgr.begin(IsolationLevel::ReadCommitted);
        mgr.commit(&mut handle, tmp.path()).unwrap();
        assert!(mgr.commit(&mut handle, tmp.path()).is_err());
    }

    #[test]
    fn commit_after_rollback_returns_error() {
        let tmp = NamedTempFile::new().unwrap();
        let mgr = TransactionManager::new();
        let mut handle = mgr.begin(IsolationLevel::ReadCommitted);
        mgr.rollback(&mut handle, tmp.path()).unwrap();
        assert!(mgr.commit(&mut handle, tmp.path()).is_err());
    }

    #[test]
    fn uncommitted_txn_not_visible() {
        let mgr = TransactionManager::new();
        let t1 = mgr.begin(IsolationLevel::ReadCommitted);
        assert!(!mgr.is_committed(t1.txn_id()));
    }

    #[test]
    fn snapshot_captures_committed_set_at_begin_time() {
        let tmp = NamedTempFile::new().unwrap();
        let mgr = TransactionManager::new();

        // T1 commits before T2 begins
        let mut t1 = mgr.begin(IsolationLevel::SnapshotIsolation);
        let t1_id = t1.txn_id();
        mgr.commit(&mut t1, tmp.path()).unwrap();

        // T2 begins — its snapshot MUST include T1
        let t2 = mgr.begin(IsolationLevel::SnapshotIsolation);
        let snap = t2.snapshot().expect("SI handle must have snapshot");
        assert!(snap.is_visible(t1_id));

        // T3 begins after T2 — T3 NOT visible in T2's snapshot
        let t3 = mgr.begin(IsolationLevel::SnapshotIsolation);
        assert!(!snap.is_visible(t3.txn_id()));
    }

    #[test]
    fn read_committed_has_no_snapshot() {
        let mgr = TransactionManager::new();
        let t = mgr.begin(IsolationLevel::ReadCommitted);
        assert!(t.snapshot().is_none());
    }

    #[test]
    fn manager_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<TransactionManager>();
    }
}
