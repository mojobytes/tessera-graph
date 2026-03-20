// Copyright 2026 BelowZero Security OU. All rights reserved.

/// Errors that can occur during bulk data import.
#[derive(Debug, thiserror::Error)]
pub enum ImportError {
    #[error("CSV parse error at row {row}: {reason}")]
    CsvParse { row: usize, reason: String },

    #[error("invalid JSON: {0}")]
    JsonInvalid(String),

    #[error("missing required JSON field: {0}")]
    JsonMissingField(String),

    #[error("node not found for edge endpoint — label={label}, {prop}={value}")]
    NodeNotFoundForEdge {
        label: String,
        prop: String,
        value: String,
    },

    #[error("GQL statement error at line {line}: {reason}")]
    GqlStatement { line: usize, reason: String },

    #[error("graph write error: {0}")]
    GraphWrite(String),

    #[error("invalid property key '{0}': keys must match [a-zA-Z_][a-zA-Z0-9_]*")]
    InvalidPropertyKey(String),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// Errors that can occur during bulk data export.
#[derive(Debug, thiserror::Error)]
pub enum ExportError {
    #[error("graph read error: {0}")]
    GraphRead(String),

    #[error("serialization error: {0}")]
    Serialize(String),

    #[error("{context}: property type '{type_name}' is not supported for this format")]
    UnsupportedType { context: String, type_name: String },

    #[error("invalid property key '{0}': must match [a-zA-Z_][a-zA-Z0-9_]*")]
    InvalidPropertyKey(String),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

pub type ImportResult<T> = std::result::Result<T, ImportError>;
pub type ExportResult<T> = std::result::Result<T, ExportError>;
