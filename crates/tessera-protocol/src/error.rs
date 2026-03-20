// Copyright 2026 BelowZero Security OU. All rights reserved.

/// Error type for protocol operations.
#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("certificate load error: {0}")]
    CertificateLoad(String),

    #[error("key load error: {0}")]
    KeyLoad(String),

    #[error("TLS configuration error: {0}")]
    TlsConfig(String),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error(
        "frame too large: declared {declared} bytes (max {})",
        crate::frame::MAX_FRAME_SIZE
    )]
    FrameTooLarge { declared: u32 },

    #[error("invalid message: {0}")]
    InvalidMessage(#[from] serde_json::Error),
}

/// Convenience result type for protocol operations.
pub type Result<T> = std::result::Result<T, ProtocolError>;
