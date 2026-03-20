// Copyright 2026 BelowZero Security OU. All rights reserved.

//! Activity logging for tessera-graph-enterprise.
//!
//! Provides an append-only, structured audit log in NDJSON format.

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::Path;
use std::sync::Mutex;

/// Result of an audited operation.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum AuditResult {
    Success,
    Denied { reason: String },
    Error { message: String },
}

/// A single audit log entry.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AuditEntry {
    pub timestamp_unix: u64,
    pub user_id: Option<u64>,
    pub operation: String,
    pub target: Option<String>,
    pub result: AuditResult,
}

/// Error type for audit operations.
#[derive(Debug, thiserror::Error)]
pub enum AuditError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("lock poisoned")]
    LockPoisoned,
}

/// Convenience result type.
pub type Result<T> = std::result::Result<T, AuditError>;

/// Append-only audit log writing NDJSON (one JSON object per line).
///
/// Thread-safe via internal `Mutex`. The API intentionally provides no
/// truncate, delete, or read methods — the log is write-only.
pub struct AuditLog {
    writer: Mutex<BufWriter<File>>,
}

impl AuditLog {
    /// Open (or create) an audit log file in append mode.
    ///
    /// # Errors
    ///
    /// Returns `AuditError::Io` if the file cannot be opened.
    pub fn open(path: &Path) -> Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;

        Ok(Self {
            writer: Mutex::new(BufWriter::new(file)),
        })
    }

    /// Record an arbitrary audit entry.
    ///
    /// # Errors
    ///
    /// Returns serialization or I/O errors.
    pub fn record(&self, entry: &AuditEntry) -> Result<()> {
        let line = serde_json::to_string(entry)?;
        let mut writer = self.writer.lock().map_err(|_| AuditError::LockPoisoned)?;
        writeln!(writer, "{line}")?;
        writer.flush()?;
        drop(writer);
        Ok(())
    }

    /// Convenience: record a successful operation.
    ///
    /// # Errors
    ///
    /// Returns serialization or I/O errors.
    pub fn record_success(
        &self,
        user_id: Option<u64>,
        operation: &str,
        target: Option<&str>,
    ) -> Result<()> {
        self.record(&AuditEntry {
            timestamp_unix: unix_timestamp(),
            user_id,
            operation: operation.to_owned(),
            target: target.map(str::to_owned),
            result: AuditResult::Success,
        })
    }

    /// Convenience: record a denied operation.
    ///
    /// # Errors
    ///
    /// Returns serialization or I/O errors.
    pub fn record_denied(
        &self,
        user_id: Option<u64>,
        operation: &str,
        target: Option<&str>,
        reason: &str,
    ) -> Result<()> {
        self.record(&AuditEntry {
            timestamp_unix: unix_timestamp(),
            user_id,
            operation: operation.to_owned(),
            target: target.map(str::to_owned),
            result: AuditResult::Denied {
                reason: reason.to_owned(),
            },
        })
    }

    /// Convenience: record an internal error during an operation.
    ///
    /// Use this when the failure is due to an internal system error rather than
    /// a permission denial (e.g., lock poisoned, I/O failure).
    ///
    /// # Errors
    ///
    /// Returns serialization or I/O errors.
    pub fn record_error(
        &self,
        user_id: Option<u64>,
        operation: &str,
        target: Option<&str>,
        message: &str,
    ) -> Result<()> {
        self.record(&AuditEntry {
            timestamp_unix: unix_timestamp(),
            user_id,
            operation: operation.to_owned(),
            target: target.map(str::to_owned),
            result: AuditResult::Error {
                message: message.to_owned(),
            },
        })
    }
}

fn unix_timestamp() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock is before epoch")
        .as_secs()
}
