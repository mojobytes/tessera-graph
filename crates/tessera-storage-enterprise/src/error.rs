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

    /// Backup operation failed.
    #[error("backup failed: {reason}")]
    BackupFailed { reason: String },

    /// Restore operation failed.
    #[error("restore failed: {reason}")]
    RestoreFailed { reason: String },

    /// Backup manifest is missing, unreadable, or has a checksum mismatch.
    #[error("manifest corrupt: {0}")]
    ManifestCorrupt(String),

    /// A backup already exists at the target path.
    #[error("backup already exists at: {}", _0.display())]
    BackupAlreadyExists(std::path::PathBuf),
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

    #[test]
    fn backup_failed_formats_reason() {
        let msg = format!("{}", EnterpriseError::BackupFailed { reason: "disk full".into() });
        assert!(msg.contains("disk full"));
    }

    #[test]
    fn restore_failed_formats_reason() {
        let msg = format!("{}", EnterpriseError::RestoreFailed { reason: "corrupt".into() });
        assert!(msg.contains("corrupt"));
    }

    #[test]
    fn manifest_corrupt_formats_detail() {
        let msg = format!("{}", EnterpriseError::ManifestCorrupt("missing lsn".into()));
        assert!(msg.contains("missing lsn"));
    }

    #[test]
    fn backup_already_exists_formats_path() {
        use std::path::PathBuf;
        let msg = format!("{}", EnterpriseError::BackupAlreadyExists(PathBuf::from("/tmp/bk")));
        assert!(msg.contains("/tmp/bk"));
    }
}
