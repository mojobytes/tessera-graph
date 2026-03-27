// Copyright 2026 BelowZero Security OU. All rights reserved.

//! Activity logging for tessera-graph-enterprise.
//!
//! Provides an append-only, structured audit log in NDJSON format with typed
//! events, non-blocking channel-based writes, and size-based rotation.
//!
//! # Architecture
//!
//! `AuditLog` holds a `tokio::sync::mpsc::Sender<AuditEntry>`. Every
//! `record_event` call is an O(1) non-blocking channel send. A background
//! `AuditWriterTask` receives entries, serializes them to NDJSON, and flushes
//! to disk after draining all immediately available entries (batched flush).
//! This eliminates mutex contention on the query hot path.

use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use tokio::sync::mpsc;

// ── Typed audit events ──────────────────────────────────────────────────────

/// Typed audit event — replaces free-form `operation: String`.
///
/// Every auditable operation has a dedicated variant, ensuring exhaustive
/// coverage at compile time.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum AuditEvent {
    LoginSuccess { username: String },
    LoginFailure { username: String },
    LoginRateLimited { username: String },
    Logout,
    SessionExpired,
    PermissionDenied { permission: String },
    QueryExecuted { query_preview: String },
    MutationExecuted { query_preview: String },
    ExplicitTransactionRejected,
    AdminUserManagement { action: AdminAction },
    SchemaFlush,
}

/// Typed administrative actions for `AuditEvent::AdminUserManagement`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum AdminAction {
    CreateUser,
    DeleteUser,
    ChangePassword,
    AssignRole,
    RevokeRole,
    Other(String),
}

/// Result of an audited operation.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum AuditResult {
    Success,
    Denied { reason: String },
    Error { message: String },
}

/// A single audit log entry.
///
/// `timestamp_ms` is milliseconds since Unix epoch (subsecond resolution
/// for forensic correlation of events within the same second).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AuditEntry {
    pub timestamp_ms: u64,
    pub user_id: Option<u64>,
    pub event: AuditEvent,
    pub result: AuditResult,
}

impl AuditEntry {
    /// Create a success entry with the current timestamp.
    #[must_use]
    pub fn success(user_id: Option<u64>, event: AuditEvent) -> Self {
        Self {
            timestamp_ms: unix_timestamp_ms(),
            user_id,
            event,
            result: AuditResult::Success,
        }
    }

    /// Create a denied entry with the current timestamp.
    #[must_use]
    pub fn denied(user_id: Option<u64>, event: AuditEvent, reason: String) -> Self {
        Self {
            timestamp_ms: unix_timestamp_ms(),
            user_id,
            event,
            result: AuditResult::Denied { reason },
        }
    }

    /// Create an error entry with the current timestamp.
    #[must_use]
    pub fn error(user_id: Option<u64>, event: AuditEvent, message: String) -> Self {
        Self {
            timestamp_ms: unix_timestamp_ms(),
            user_id,
            event,
            result: AuditResult::Error { message },
        }
    }
}

// ── Error ───────────────────────────────────────────────────────────────────

/// Error type for audit operations.
#[derive(Debug, thiserror::Error)]
pub enum AuditError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("audit writer channel full — entry dropped")]
    ChannelFull,

    #[error("audit writer channel closed — writer task has stopped")]
    ChannelClosed,
}

/// Convenience result type.
pub type Result<T> = std::result::Result<T, AuditError>;

// ── AuditLog (sender side) ──────────────────────────────────────────────────

/// Non-blocking audit log handle.
///
/// Each `record_event` call sends the entry through an mpsc channel to the
/// background writer task. The send is O(1) and never blocks the caller.
pub struct AuditLog {
    sender: mpsc::Sender<AuditEntry>,
}

impl AuditLog {
    /// Open (or create) an audit log and return the log handle + writer task.
    ///
    /// The caller must `tokio::spawn(writer_task.run())` to start writing.
    ///
    /// # Errors
    ///
    /// Returns `AuditError::Io` if the file cannot be opened.
    pub fn open(
        path: &Path,
        rotation_max_size_bytes: u64,
        max_rotated_files: usize,
    ) -> Result<(Self, AuditWriterTask)> {
        Self::open_with_capacity(path, rotation_max_size_bytes, max_rotated_files, 4096)
    }

    /// Open with explicit channel capacity. Use `open()` for the default (4096).
    ///
    /// # Errors
    ///
    /// Returns `AuditError::Io` if the file cannot be opened.
    pub fn open_with_capacity(
        path: &Path,
        rotation_max_size_bytes: u64,
        max_rotated_files: usize,
        channel_capacity: usize,
    ) -> Result<(Self, AuditWriterTask)> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        let file_size = file.metadata().map(|m| m.len()).unwrap_or(0);
        let (sender, receiver) = mpsc::channel(channel_capacity);

        let task = AuditWriterTask {
            receiver,
            writer: BufWriter::new(file),
            path: path.to_owned(),
            bytes_written: file_size,
            rotation_max_size_bytes,
            max_rotated_files,
        };

        Ok((Self { sender }, task))
    }

    /// Create a null logger that discards all entries.
    ///
    /// Useful for tests that don't care about audit output. The returned
    /// `AuditLog` silently drops all entries (no background task needed).
    /// All `record_event` calls return `Err(ChannelClosed)` but callers
    /// typically ignore the result with `let _ = ...`.
    #[must_use]
    pub fn new_null() -> Self {
        // Create a channel and immediately drop the receiver.
        // Every try_send will fail with ChannelClosed — entries are discarded.
        let (sender, _receiver) = mpsc::channel(1);
        Self { sender }
    }

    /// Construct an `AuditLog` with an externally provided sender (for testing).
    #[must_use]
    pub fn new_with_sender(sender: mpsc::Sender<AuditEntry>) -> Self {
        Self { sender }
    }

    /// Record an audit entry (non-blocking channel send).
    ///
    /// # Errors
    ///
    /// Returns `AuditError::ChannelFull` if the buffer is exhausted (writer alive
    /// but backpressured). Returns `AuditError::ChannelClosed` if the writer
    /// task has stopped.
    pub fn record_event(&self, entry: AuditEntry) -> Result<()> {
        self.sender.try_send(entry).map_err(|e| match e {
            mpsc::error::TrySendError::Full(_) => {
                eprintln!("audit: channel full — entry dropped");
                AuditError::ChannelFull
            }
            mpsc::error::TrySendError::Closed(_) => AuditError::ChannelClosed,
        })
    }
}

// ── AuditWriterTask (receiver side) ─────────────────────────────────────────

/// Background task that owns the file writer and receives entries via channel.
///
/// Must be spawned with `tokio::spawn(task.run())`.
pub struct AuditWriterTask {
    receiver: mpsc::Receiver<AuditEntry>,
    writer: BufWriter<File>,
    path: PathBuf,
    bytes_written: u64,
    rotation_max_size_bytes: u64,
    max_rotated_files: usize,
}

impl AuditWriterTask {
    /// Run the writer loop until the channel is closed.
    ///
    /// Uses batched flush: blocks on `recv()` for the first entry, then drains
    /// all immediately available entries via `try_recv()`, and flushes once per
    /// batch. This reduces syscall overhead under sustained load.
    pub async fn run(mut self) {
        loop {
            // Block until at least one entry is available.
            let Some(first) = self.receiver.recv().await else {
                // Channel closed — flush and exit.
                let _ = self.writer.flush();
                return;
            };

            if let Err(e) = self.write_entry_no_flush(&first) {
                eprintln!("audit write error: {e}");
            }

            // Drain all immediately available entries (non-blocking).
            while let Ok(entry) = self.receiver.try_recv() {
                if let Err(e) = self.write_entry_no_flush(&entry) {
                    eprintln!("audit write error: {e}");
                }
            }

            // Single flush after the batch.
            if let Err(e) = self.writer.flush() {
                eprintln!("audit flush error: {e}");
            }
        }
    }

    fn write_entry_no_flush(&mut self, entry: &AuditEntry) -> Result<()> {
        let line = serde_json::to_string(entry)?;
        let line_bytes = line.len() as u64 + 1; // +1 for newline
        writeln!(self.writer, "{line}")?;
        self.bytes_written += line_bytes;

        // Rotate if needed.
        if self.rotation_max_size_bytes > 0 && self.bytes_written >= self.rotation_max_size_bytes {
            // Flush before rotation to ensure all buffered data is written.
            self.writer.flush()?;
            self.rotate()?;
        }

        Ok(())
    }

    fn rotate(&mut self) -> std::result::Result<(), std::io::Error> {
        self.writer.flush()?;

        // Generate rotated filename with timestamp (underscore separator for shell compat).
        let now = chrono_timestamp();
        let rotated = self.path.with_extension(format!("{now}.ndjson"));
        fs::rename(&self.path, &rotated)?;

        // Attempt to open fresh file. On failure, revert the rename.
        let file = match OpenOptions::new().create(true).append(true).open(&self.path) {
            Ok(f) => f,
            Err(e) => {
                // Best-effort revert: rename the rotated file back.
                let _ = fs::rename(&rotated, &self.path);
                return Err(e);
            }
        };

        self.writer = BufWriter::new(file);
        self.bytes_written = 0;

        if self.max_rotated_files > 0 {
            self.prune_old_files();
        }

        Ok(())
    }

    fn prune_old_files(&self) {
        let Some(parent) = self.path.parent() else {
            return;
        };
        let Some(stem) = self.path.file_stem().and_then(|s| s.to_str()) else {
            return;
        };

        let mut rotated: Vec<PathBuf> = fs::read_dir(parent)
            .into_iter()
            .flatten()
            .filter_map(std::result::Result::ok)
            .map(|e| e.path())
            .filter(|p| {
                p.to_str()
                    .is_some_and(|s| s.contains(stem) && s.ends_with(".ndjson") && *p != self.path)
            })
            .collect();

        rotated.sort();

        // O(k) drain instead of O(k*n) remove(0) loop.
        if rotated.len() > self.max_rotated_files {
            let excess = rotated.len() - self.max_rotated_files;
            for path in rotated.drain(..excess) {
                let _ = fs::remove_file(&path);
            }
        }
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────────

fn unix_timestamp_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock is before epoch") // OK: fundamental invariant
        .as_millis() as u64
}

fn chrono_timestamp() -> String {
    let dur = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock is before epoch"); // OK: fundamental invariant
    // Underscore separator for shell compatibility (avoids dots in filenames).
    format!("{}_{:09}", dur.as_secs(), dur.subsec_nanos())
}
