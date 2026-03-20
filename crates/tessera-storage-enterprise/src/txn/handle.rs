use super::snapshot::Snapshot;

/// Transaction isolation level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IsolationLevel {
    /// Each statement sees the latest committed data.
    ReadCommitted,
    /// The transaction sees a consistent snapshot taken at `BEGIN` time.
    SnapshotIsolation,
}

impl std::fmt::Display for IsolationLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ReadCommitted => f.write_str("ReadCommitted"),
            Self::SnapshotIsolation => f.write_str("SnapshotIsolation"),
        }
    }
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

impl std::fmt::Display for TxnState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Active => f.write_str("Active"),
            Self::Committed => f.write_str("Committed"),
            Self::RolledBack => f.write_str("RolledBack"),
        }
    }
}

/// Opaque handle to an in-flight transaction.
///
/// Created by [`TransactionManager::begin`](super::TransactionManager::begin).
/// The caller must eventually call `commit` or `rollback` on the manager,
/// passing this handle back.
pub struct TransactionHandle {
    txn_id: u64,
    isolation: IsolationLevel,
    state: TxnState,
    snapshot: Option<Snapshot>,
}

impl TransactionHandle {
    /// Creates a new handle. Only callable within the crate (by `TransactionManager`).
    pub(crate) const fn new(
        txn_id: u64,
        isolation: IsolationLevel,
        state: TxnState,
        snapshot: Option<Snapshot>,
    ) -> Self {
        Self {
            txn_id,
            isolation,
            state,
            snapshot,
        }
    }

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

    /// Sets the transaction state. Only `TransactionManager` should call this.
    pub(crate) const fn set_state(&mut self, new_state: TxnState) {
        self.state = new_state;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn isolation_level_display() {
        use std::fmt::Write as _;
        let mut s = String::new();
        write!(s, "{}", IsolationLevel::ReadCommitted).unwrap();
        assert_eq!(s, "ReadCommitted");
        s.clear();
        write!(s, "{}", IsolationLevel::SnapshotIsolation).unwrap();
        assert_eq!(s, "SnapshotIsolation");
    }

    #[test]
    fn isolation_level_hashable() {
        use std::collections::HashMap;
        let mut map = HashMap::new();
        map.insert(IsolationLevel::ReadCommitted, 1_u32);
        map.insert(IsolationLevel::SnapshotIsolation, 2_u32);
        assert_eq!(*map.get(&IsolationLevel::ReadCommitted).unwrap(), 1);
    }

    #[test]
    fn txn_state_display() {
        assert_eq!(format!("{}", TxnState::Active), "Active");
        assert_eq!(format!("{}", TxnState::Committed), "Committed");
        assert_eq!(format!("{}", TxnState::RolledBack), "RolledBack");
    }

    #[test]
    fn set_state_transitions_correctly() {
        let mut h =
            TransactionHandle::new(10, IsolationLevel::ReadCommitted, TxnState::Active, None);
        assert_eq!(h.state(), TxnState::Active);
        h.set_state(TxnState::Committed);
        assert_eq!(h.state(), TxnState::Committed);
    }
}
