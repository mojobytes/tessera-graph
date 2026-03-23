// Copyright 2026 BelowZero Security OU. All rights reserved.

/// All errors produced by `tessera-tenant`.
#[derive(Debug, thiserror::Error)]
pub enum TenantError {
    /// The supplied tenant or database name is invalid.
    #[error("invalid name: {0}")]
    InvalidName(String),

    /// No tenant directory exists for the given name.
    #[error("tenant not found: {0}")]
    TenantNotFound(String),

    /// The tenant exists but has no database with that name.
    #[error("database not found: {tenant}/{database}")]
    DatabaseNotFound {
        /// Tenant portion of the address.
        tenant: String,
        /// Database portion of the address.
        database: String,
    },

    /// Attempted to create a database that already exists on disk.
    #[error("database already exists: {tenant}/{database}")]
    DatabaseAlreadyExists {
        /// Tenant portion of the address.
        tenant: String,
        /// Database portion of the address.
        database: String,
    },

    /// The graph for this address has not been loaded into the registry cache.
    #[error("database not loaded: {tenant}/{database}")]
    DatabaseNotLoaded {
        /// Tenant portion of the address.
        tenant: String,
        /// Database portion of the address.
        database: String,
    },

    /// An error from the underlying `tessera-graph` engine.
    #[error("graph error: {0}")]
    Graph(#[from] tessera_graph::Error),

    /// An OS-level I/O error.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// Convenience result alias for this crate.
pub type Result<T> = std::result::Result<T, TenantError>;
