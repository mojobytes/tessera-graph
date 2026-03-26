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

    #[error("packstream buffer underflow: need {needed} bytes, got {available}")]
    PackStreamUnderflow { needed: usize, available: usize },

    #[error("packstream unknown marker byte: 0x{marker:02X}")]
    PackStreamUnknownMarker { marker: u8 },

    #[error("packstream invalid UTF-8 in string field")]
    PackStreamInvalidUtf8,

    #[error("packstream dict key must be a string")]
    PackStreamDictKeyNotString,

    #[error("packstream decode depth limit exceeded (max {max})")]
    PackStreamDepthLimitExceeded { max: usize },

    #[error("packstream float value must be finite (NaN and Infinity are not allowed)")]
    PackStreamInvalidFloat,

    #[error("bolt: unexpected struct tag (expected 0x{expected:02X}, got 0x{got:02X})")]
    BoltUnexpectedTag { expected: u8, got: u8 },

    #[error("bolt: missing field '{field}' in {message} message")]
    BoltMissingField {
        message: &'static str,
        field: &'static str,
    },

    #[error("bolt: invalid handshake — {reason}")]
    BoltInvalidHandshake { reason: &'static str },

    #[error("bolt: authentication failed — {message}")]
    BoltAuthFailure { message: String },

    #[error("bolt: query failed — {message}")]
    BoltQueryFailure { message: String },
}

/// Convenience result type for protocol operations.
pub type Result<T> = std::result::Result<T, ProtocolError>;
