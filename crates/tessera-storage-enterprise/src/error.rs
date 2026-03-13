/// Errors specific to the enterprise storage layer.
#[derive(Debug, thiserror::Error)]
pub enum EnterpriseError {
    /// Attempted to commit/rollback a transaction that is not active.
    #[error("transaction {0} is not active")]
    TransactionNotActive(u64),

    /// I/O error from the underlying storage or WAL.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// Error propagated from the tessera-graph core.
    #[error("graph error: {0}")]
    Graph(#[from] tessera_graph::Error),
}

/// Convenience alias for enterprise storage results.
pub type Result<T> = std::result::Result<T, EnterpriseError>;
