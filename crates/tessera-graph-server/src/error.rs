// Copyright 2026 BelowZero Security OU. All rights reserved.

/// Error type for server operations.
#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    #[error("protocol error: {0}")]
    Protocol(#[from] tessera_graph_protocol::ProtocolError),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Bolt I/O error: {0}")]
    BoltIo(std::io::Error),

    #[error("auth error: {0}")]
    Auth(#[from] tessera_graph_auth::AuthError),

    #[error("storage error: {0}")]
    Storage(#[from] tessera_graph::Error),

    #[error("tenant error: {0}")]
    Tenant(#[from] tessera_graph_tenant::TenantError),
}

/// Convenience result type for server operations.
pub type Result<T> = std::result::Result<T, ServerError>;
