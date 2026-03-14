/// Errors specific to the enterprise storage layer.
#[derive(Debug, thiserror::Error)]
pub enum EnterpriseError {
    /// Attempted to commit/rollback a transaction that is not active.
    #[error("transaction {0} is not active (state: {1})")]
    TransactionNotActive(u64, crate::txn::handle::TxnState),

    /// I/O error from the underlying storage or WAL.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// Error propagated from the tessera-graph core.
    #[error("graph error: {0}")]
    Graph(#[from] tessera_graph::Error),

    /// A lock was poisoned by a panicking thread.
    #[error("lock poisoned: {0}")]
    LockPoisoned(&'static str),
}

/// Convenience alias for enterprise storage results.
pub type Result<T> = std::result::Result<T, EnterpriseError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_poisoned_formats_message() {
        use std::fmt::Write as _;
        let e = EnterpriseError::LockPoisoned("commit log");
        let mut s = String::new();
        write!(s, "{e}").unwrap();
        assert!(s.contains("commit log"));
    }
}
