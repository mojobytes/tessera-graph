use std::collections::BTreeSet;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use tessera_graph::{WalRecord, WalWriter};

use super::handle::{IsolationLevel, TransactionHandle, TxnState};
use super::snapshot::Snapshot;
use crate::error::{EnterpriseError, Result};

/// Placeholder LSN passed to `WalRecord` constructors. The real LSN
/// is assigned by [`WalWriter::append()`](tessera_graph::WalWriter).
const LSN_PLACEHOLDER: u64 = 0;

/// Manages transaction lifecycle: begin, commit, rollback.
///
/// Thread-safe: uses `AtomicU64` for ID generation, `RwLock` for
/// the commit log, and `Mutex<WalWriter>` for serialised WAL access.
/// Multiple threads can begin/commit concurrently without WAL corruption.
pub struct TransactionManager {
    next_txn_id: AtomicU64,
    committed: RwLock<Arc<BTreeSet<u64>>>,
    pub(crate) wal: Mutex<WalWriter>,
}

impl TransactionManager {
    /// Opens or creates a `TransactionManager` backed by a WAL at `path`.
    ///
    /// # Errors
    ///
    /// Returns an error if the WAL file cannot be opened or created.
    pub fn open(path: &Path) -> Result<Self> {
        let wal = WalWriter::open(path).map_err(EnterpriseError::Graph)?;
        Ok(Self {
            next_txn_id: AtomicU64::new(1),
            committed: RwLock::new(Arc::new(BTreeSet::new())),
            wal: Mutex::new(wal),
        })
    }

    /// Begins a new transaction with the given isolation level.
    ///
    /// For `SnapshotIsolation`, a snapshot of the current committed set
    /// is captured at this point. For `ReadCommitted`, no snapshot is taken.
    ///
    /// # Errors
    ///
    /// Returns [`EnterpriseError::LockPoisoned`] if the commit log lock
    /// was poisoned by a panicking thread.
    pub fn begin(&self, isolation: IsolationLevel) -> Result<TransactionHandle> {
        let txn_id = self.next_txn_id.fetch_add(1, Ordering::Relaxed);

        // Write Begin record before capturing snapshot (atomicity boundary).
        // No sync — Begin is advisory; durability comes from Commit/Rollback sync.
        {
            let mut wal = self
                .wal
                .lock()
                .map_err(|_| EnterpriseError::LockPoisoned("wal"))?;
            wal.append(WalRecord::Begin {
                lsn: LSN_PLACEHOLDER,
                txn_id,
            })
            .map_err(EnterpriseError::Graph)?;
        }

        let snapshot = match isolation {
            IsolationLevel::SnapshotIsolation => {
                let guard = self
                    .committed
                    .read()
                    .map_err(|_| EnterpriseError::LockPoisoned("commit log"))?;
                let arc = Arc::clone(&guard);
                drop(guard);
                Some(Snapshot::new(arc, txn_id))
            }
            IsolationLevel::ReadCommitted => None,
        };

        Ok(TransactionHandle::new(
            txn_id,
            isolation,
            TxnState::Active,
            snapshot,
        ))
    }

    /// Commits the transaction: writes a `Commit` record to the WAL
    /// and marks it in the commit log.
    ///
    /// # Errors
    ///
    /// Returns [`EnterpriseError::TransactionNotActive`] if the handle
    /// is not in `Active` state. Returns [`EnterpriseError::LockPoisoned`]
    /// if a lock was poisoned. Returns I/O errors from WAL writes.
    pub fn commit(&self, handle: &mut TransactionHandle) -> Result<()> {
        if handle.state() != TxnState::Active {
            return Err(EnterpriseError::TransactionNotActive(
                handle.txn_id(),
                handle.state(),
            ));
        }

        let mut wal = self
            .wal
            .lock()
            .map_err(|_| EnterpriseError::LockPoisoned("wal"))?;
        wal.append(WalRecord::Commit {
            lsn: LSN_PLACEHOLDER,
            txn_id: handle.txn_id(),
        })
        .map_err(EnterpriseError::Graph)?;
        wal.sync().map_err(EnterpriseError::Graph)?;
        // ATOMICITY GAP: The Commit WAL record is now durable on disk, but the
        // in-memory `committed` set has not yet been updated. A crash here will
        // leave the WAL with a Commit entry whose txn_id is absent from the
        // committed set.
        //
        // Recovery path (not yet implemented): on open, scan WAL records; for
        // any `Commit { txn_id }` absent from the meta-log, replay it into the
        // in-memory set before accepting new transactions.
        drop(wal);

        let mut guard = self
            .committed
            .write()
            .map_err(|_| EnterpriseError::LockPoisoned("commit log"))?;
        let mut new_set = (**guard).clone();
        new_set.insert(handle.txn_id());
        *guard = Arc::new(new_set);
        drop(guard);

        handle.set_state(TxnState::Committed);
        Ok(())
    }

    /// Rolls back the transaction: writes a `Rollback` record to the WAL
    /// without adding it to the commit log.
    ///
    /// # Errors
    ///
    /// Returns [`EnterpriseError::TransactionNotActive`] if the handle
    /// is not in `Active` state. Returns [`EnterpriseError::LockPoisoned`]
    /// if a lock was poisoned. Returns I/O errors from WAL writes.
    pub fn rollback(&self, handle: &mut TransactionHandle) -> Result<()> {
        if handle.state() != TxnState::Active {
            return Err(EnterpriseError::TransactionNotActive(
                handle.txn_id(),
                handle.state(),
            ));
        }

        let mut wal = self
            .wal
            .lock()
            .map_err(|_| EnterpriseError::LockPoisoned("wal"))?;
        wal.append(WalRecord::Rollback {
            lsn: LSN_PLACEHOLDER,
            txn_id: handle.txn_id(),
        })
        .map_err(EnterpriseError::Graph)?;
        // Sync ensures the Rollback record is durable before we return.
        // Without recovery scanning WAL for Commit/Rollback, this sync is
        // conservative — an unsynced rollback is harmless because recovery
        // treats any transaction without a Commit record as aborted. Kept
        // for forward compatibility with future recovery implementation.
        wal.sync().map_err(EnterpriseError::Graph)?;
        drop(wal);

        handle.set_state(TxnState::RolledBack);
        Ok(())
    }

    /// Returns the number of committed transactions.
    ///
    /// # Errors
    ///
    /// Returns [`EnterpriseError::LockPoisoned`] if the commit log lock
    /// was poisoned by a panicking thread.
    pub fn committed_count(&self) -> Result<usize> {
        Ok(self
            .committed
            .read()
            .map_err(|_| EnterpriseError::LockPoisoned("commit log"))?
            .len())
    }

    /// Remove all committed transaction IDs strictly below `min_txn_id`.
    ///
    /// Call this periodically (e.g., after every N commits) with the lowest
    /// `txn_id` that any active snapshot still references. IDs below that
    /// threshold are no longer needed for visibility checks.
    ///
    /// # Errors
    ///
    /// Returns [`EnterpriseError::LockPoisoned`] if the commit log lock
    /// was poisoned by a panicking thread.
    #[allow(clippy::significant_drop_tightening)]
    pub fn prune_below(&self, min_txn_id: u64) -> Result<usize> {
        let mut guard = self
            .committed
            .write()
            .map_err(|_| EnterpriseError::LockPoisoned("commit log"))?;
        let before = guard.len();
        let mut new_set = (**guard).clone();
        // BTreeSet is ordered — split_off returns all elements >= min_txn_id.
        new_set = new_set.split_off(&min_txn_id);
        let pruned = before - new_set.len();
        *guard = Arc::new(new_set);
        Ok(pruned)
    }

    /// Returns `true` if the given transaction has been committed.
    ///
    /// # Errors
    ///
    /// Returns [`EnterpriseError::LockPoisoned`] if the commit log lock
    /// was poisoned by a panicking thread.
    pub fn is_committed(&self, txn_id: u64) -> Result<bool> {
        Ok(self
            .committed
            .read()
            .map_err(|_| EnterpriseError::LockPoisoned("commit log"))?
            .contains(&txn_id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    #[test]
    fn begin_returns_handle_with_unique_id() {
        let tmp = NamedTempFile::new().unwrap();
        let mgr = TransactionManager::open(tmp.path()).unwrap();
        let t1 = mgr.begin(IsolationLevel::ReadCommitted).unwrap();
        let t2 = mgr.begin(IsolationLevel::ReadCommitted).unwrap();
        assert_ne!(t1.txn_id(), t2.txn_id());
    }

    #[test]
    fn commit_transitions_state_and_marks_committed() {
        let tmp = NamedTempFile::new().unwrap();
        let mgr = TransactionManager::open(tmp.path()).unwrap();
        let mut handle = mgr.begin(IsolationLevel::ReadCommitted).unwrap();
        let id = handle.txn_id();

        assert!(!mgr.is_committed(id).unwrap());
        mgr.commit(&mut handle).unwrap();
        assert_eq!(handle.state(), TxnState::Committed);
        assert!(mgr.is_committed(id).unwrap());
    }

    #[test]
    fn rollback_transitions_state_and_not_committed() {
        let tmp = NamedTempFile::new().unwrap();
        let mgr = TransactionManager::open(tmp.path()).unwrap();
        let mut handle = mgr.begin(IsolationLevel::ReadCommitted).unwrap();
        let id = handle.txn_id();

        mgr.rollback(&mut handle).unwrap();
        assert_eq!(handle.state(), TxnState::RolledBack);
        assert!(!mgr.is_committed(id).unwrap());
    }

    #[test]
    fn double_commit_returns_error() {
        let tmp = NamedTempFile::new().unwrap();
        let mgr = TransactionManager::open(tmp.path()).unwrap();
        let mut handle = mgr.begin(IsolationLevel::ReadCommitted).unwrap();
        mgr.commit(&mut handle).unwrap();
        assert!(mgr.commit(&mut handle).is_err());
    }

    #[test]
    fn commit_after_rollback_returns_error() {
        let tmp = NamedTempFile::new().unwrap();
        let mgr = TransactionManager::open(tmp.path()).unwrap();
        let mut handle = mgr.begin(IsolationLevel::ReadCommitted).unwrap();
        mgr.rollback(&mut handle).unwrap();
        assert!(mgr.commit(&mut handle).is_err());
    }

    #[test]
    fn uncommitted_txn_not_visible() {
        let tmp = NamedTempFile::new().unwrap();
        let mgr = TransactionManager::open(tmp.path()).unwrap();
        let t1 = mgr.begin(IsolationLevel::ReadCommitted).unwrap();
        assert!(!mgr.is_committed(t1.txn_id()).unwrap());
    }

    #[test]
    fn snapshot_captures_committed_set_at_begin_time() {
        let tmp = NamedTempFile::new().unwrap();
        let mgr = TransactionManager::open(tmp.path()).unwrap();

        // T1 commits before T2 begins
        let mut t1 = mgr.begin(IsolationLevel::SnapshotIsolation).unwrap();
        let t1_id = t1.txn_id();
        mgr.commit(&mut t1).unwrap();

        // T2 begins — its snapshot MUST include T1
        let t2 = mgr.begin(IsolationLevel::SnapshotIsolation).unwrap();
        let snap = t2.snapshot().expect("SI handle must have snapshot");
        assert!(snap.is_visible(t1_id));

        // T3 begins after T2 — T3 NOT visible in T2's snapshot
        let t3 = mgr.begin(IsolationLevel::SnapshotIsolation).unwrap();
        assert!(!snap.is_visible(t3.txn_id()));
    }

    #[test]
    fn read_committed_has_no_snapshot() {
        let tmp = NamedTempFile::new().unwrap();
        let mgr = TransactionManager::open(tmp.path()).unwrap();
        let t = mgr.begin(IsolationLevel::ReadCommitted).unwrap();
        assert!(t.snapshot().is_none());
    }

    #[test]
    fn manager_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<TransactionManager>();
    }

    // --- Cycle E-C1: WAL as Mutex<WalWriter> field ---

    #[test]
    fn open_constructs_manager_with_wal() {
        let tmp = NamedTempFile::new().unwrap();
        let mgr = TransactionManager::open(tmp.path()).unwrap();
        let mut h = mgr.begin(IsolationLevel::ReadCommitted).unwrap();
        mgr.commit(&mut h).unwrap();
    }

    #[test]
    fn commit_without_wal_path_arg() {
        let tmp = NamedTempFile::new().unwrap();
        let mgr = TransactionManager::open(tmp.path()).unwrap();
        let mut h = mgr.begin(IsolationLevel::ReadCommitted).unwrap();
        assert!(mgr.commit(&mut h).is_ok());
    }

    #[test]
    fn rollback_without_wal_path_arg() {
        let tmp = NamedTempFile::new().unwrap();
        let mgr = TransactionManager::open(tmp.path()).unwrap();
        let mut h = mgr.begin(IsolationLevel::ReadCommitted).unwrap();
        assert!(mgr.rollback(&mut h).is_ok());
    }

    #[test]
    fn concurrent_commits_are_serialised() {
        use std::sync::Arc;
        use std::thread;

        let tmp = NamedTempFile::new().unwrap();
        let mgr = Arc::new(TransactionManager::open(tmp.path()).unwrap());

        let threads: Vec<_> = (0..16)
            .map(|_| {
                let mgr = Arc::clone(&mgr);
                thread::spawn(move || {
                    let mut h = mgr.begin(IsolationLevel::ReadCommitted).unwrap();
                    mgr.commit(&mut h).unwrap();
                })
            })
            .collect();

        for t in threads {
            t.join().unwrap();
        }

        assert_eq!(mgr.committed_count().unwrap(), 16);
    }

    // --- Cycle E-R5: LockPoisoned error variant ---

    #[test]
    fn poisoned_wal_lock_returns_error() {
        use std::sync::Arc;
        use std::thread;

        let tmp = NamedTempFile::new().unwrap();
        let mgr = Arc::new(TransactionManager::open(tmp.path()).unwrap());
        let mgr2 = Arc::clone(&mgr);

        // Poison the WAL lock by panicking while holding it.
        let _ = thread::spawn(move || {
            let _guard = mgr2.wal.lock().unwrap();
            panic!("intentional poison");
        })
        .join();

        // begin() now also acquires the WAL lock, so it returns LockPoisoned too.
        let result = mgr.begin(IsolationLevel::ReadCommitted);
        assert!(matches!(result, Err(EnterpriseError::LockPoisoned(_))));
    }

    // --- Cycle E-C3: Arc<HashSet> for O(1) snapshot clone ---

    #[test]
    fn arc_snapshot_isolation_sees_correct_commits() {
        let tmp = NamedTempFile::new().unwrap();
        let mgr = TransactionManager::open(tmp.path()).unwrap();

        // Commit 1000 transactions.
        for _ in 0..1_000 {
            let mut h = mgr.begin(IsolationLevel::ReadCommitted).unwrap();
            mgr.commit(&mut h).unwrap();
        }

        // Snapshot begins — sees all 1000 committed txns.
        let t_snap = mgr.begin(IsolationLevel::SnapshotIsolation).unwrap();
        let committed_count_at_begin = t_snap.snapshot().unwrap().committed_count();
        assert_eq!(committed_count_at_begin, 1_000);

        // New commits after snapshot must NOT be visible.
        let mut t_after = mgr.begin(IsolationLevel::ReadCommitted).unwrap();
        mgr.commit(&mut t_after).unwrap();

        assert_eq!(
            t_snap.snapshot().unwrap().committed_count(),
            1_000,
            "snapshot must be immutable after begin"
        );
    }

    // --- R4: prune_below prevents unbounded growth ---

    #[test]
    fn prune_below_removes_old_txn_ids() {
        let tmp = NamedTempFile::new().unwrap(); // OK: test
        let mgr = TransactionManager::open(tmp.path()).unwrap(); // OK: test

        for _ in 0..10 {
            let mut h = mgr.begin(IsolationLevel::ReadCommitted).unwrap(); // OK: test
            mgr.commit(&mut h).unwrap(); // OK: test
        }
        assert_eq!(mgr.committed_count().unwrap(), 10); // OK: test

        let pruned = mgr.prune_below(6).unwrap(); // OK: test
        assert_eq!(pruned, 5);
        assert_eq!(mgr.committed_count().unwrap(), 5); // OK: test
        assert!(!mgr.is_committed(1).unwrap()); // OK: test
        assert!(!mgr.is_committed(5).unwrap()); // OK: test
        assert!(mgr.is_committed(6).unwrap()); // OK: test
        assert!(mgr.is_committed(10).unwrap()); // OK: test
    }

    #[test]
    fn prune_below_zero_is_noop() {
        let tmp = NamedTempFile::new().unwrap(); // OK: test
        let mgr = TransactionManager::open(tmp.path()).unwrap(); // OK: test
        let mut h = mgr.begin(IsolationLevel::ReadCommitted).unwrap(); // OK: test
        mgr.commit(&mut h).unwrap(); // OK: test
        let pruned = mgr.prune_below(0).unwrap(); // OK: test
        assert_eq!(pruned, 0);
        assert_eq!(mgr.committed_count().unwrap(), 1); // OK: test
    }

    #[test]
    fn prune_does_not_affect_active_snapshots() {
        let tmp = NamedTempFile::new().unwrap(); // OK: test
        let mgr = TransactionManager::open(tmp.path()).unwrap(); // OK: test
        let mut t1 = mgr.begin(IsolationLevel::ReadCommitted).unwrap(); // OK: test
        let t1_id = t1.txn_id();
        mgr.commit(&mut t1).unwrap(); // OK: test
        let t2 = mgr.begin(IsolationLevel::SnapshotIsolation).unwrap(); // OK: test
        let snap = t2.snapshot().unwrap(); // OK: test
        assert!(snap.is_visible(t1_id));
        mgr.prune_below(t1_id + 1).unwrap(); // OK: test
        assert!(!mgr.is_committed(t1_id).unwrap()); // OK: test
        // Snapshot is an Arc clone — unaffected by prune.
        assert!(snap.is_visible(t1_id));
    }

    // --- Cycle E-C4: Write WalRecord::Begin in begin() ---

    #[test]
    fn begin_writes_wal_begin_record() {
        use tessera_graph::WalReader;

        let tmp = NamedTempFile::new().unwrap();
        let mgr = TransactionManager::open(tmp.path()).unwrap();
        let h = mgr.begin(IsolationLevel::ReadCommitted).unwrap();
        let txn_id = h.txn_id();

        // Drop manager to flush WalWriter (handle drop has no WAL effect).
        drop(mgr);

        let reader = WalReader::open(tmp.path()).unwrap();
        let records: Vec<_> = reader.records().collect();

        let found = records
            .iter()
            .any(|r| matches!(r, WalRecord::Begin { txn_id: id, .. } if *id == txn_id));
        assert!(found, "WAL must contain a Begin record for txn {txn_id}");
    }
}
