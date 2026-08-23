// SPDX-License-Identifier: BSL-1.1

//! Server error types.

/// Alias for `Result<T, ServerError>`.
pub type Result<T> = std::result::Result<T, ServerError>;

/// Errors that can occur during server operation.
#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    /// I/O error (TCP, TLS, file).
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// Bolt protocol error (framing, encoding, decoding).
    #[error("protocol error: {0}")]
    Protocol(#[from] ermya_graph_protocol::ProtocolError),

    /// Graph engine error (storage, WAL, query execution).
    #[error("graph error: {0}")]
    Graph(#[from] ermya_graph::Error),

    /// Client has not authenticated.
    #[error("not authenticated")]
    NotAuthenticated,

    /// No database selected.
    #[error("no database selected")]
    NoDatabase,

    /// Lock poisoned.
    #[error("lock poisoned: {0}")]
    LockPoisoned(&'static str),

    /// Schema-version guard failure during startup. Wraps
    /// [`crate::migration::MigrationError`] verbatim so callers
    /// (and tests) can match on the concrete variant rather than
    /// the textual representation.
    #[error("migration: {0}")]
    Migration(crate::migration::MigrationError),

    /// Registry construction or runtime failure. Wraps
    /// [`crate::registry::RegistryError`] verbatim — same rationale
    /// as [`Self::Migration`].
    #[error("registry: {0}")]
    Registry(crate::registry::RegistryError),

    /// The configured persistent data directory could not be created or
    /// written. Carries an actionable message naming the path and both escape
    /// hatches, so a deployment hitting the file-backed default on an
    /// unwritable path fails loudly instead of with a cryptic OS error.
    #[error(
        "cannot create or write the data directory '{path}': {source}. \
         Set ERMYA_DATA_DIR to a writable path, or use ERMYA_DATA_DIR=:memory: \
         for a non-persistent in-memory instance."
    )]
    DataDir {
        /// The data directory path that could not be prepared.
        path: std::path::PathBuf,
        /// The underlying I/O cause.
        source: std::io::Error,
    },
}
