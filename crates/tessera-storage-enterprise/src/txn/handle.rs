use super::snapshot::Snapshot;

/// Transaction isolation level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IsolationLevel {
    /// Each statement sees the latest committed data.
    ReadCommitted,
    /// The transaction sees a consistent snapshot taken at `BEGIN` time.
    SnapshotIsolation,
}

/// State of a transaction in its lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxnState {
    /// Transaction is open and accepting operations.
    Active,
    /// Transaction has been committed.
    Committed,
    /// Transaction has been rolled back.
    RolledBack,
}

/// Opaque handle to an in-flight transaction.
///
/// Created by [`TransactionManager::begin`](super::TransactionManager::begin).
/// The caller must eventually call `commit` or `rollback` on the manager,
/// passing this handle back.
pub struct TransactionHandle {
    pub(crate) txn_id: u64,
    pub(crate) isolation: IsolationLevel,
    pub(crate) state: TxnState,
    pub(crate) snapshot: Option<Snapshot>,
}

impl TransactionHandle {
    /// Returns the unique transaction ID.
    #[must_use]
    pub const fn txn_id(&self) -> u64 {
        self.txn_id
    }

    /// Returns the current state of this transaction.
    #[must_use]
    pub const fn state(&self) -> TxnState {
        self.state
    }

    /// Returns the isolation level of this transaction.
    #[must_use]
    pub const fn isolation(&self) -> IsolationLevel {
        self.isolation
    }

    /// Returns the snapshot for `SnapshotIsolation` transactions.
    /// Returns `None` for `ReadCommitted`.
    #[must_use]
    pub const fn snapshot(&self) -> Option<&Snapshot> {
        self.snapshot.as_ref()
    }
}
