// SPDX-License-Identifier: BSL-1.1

//! Exclusive advisory lock over `{data_dir}/system/system.lock`.
//!
//! Three call sites need the same advisory-lock contract on the system
//! graph and were duplicating the `OpenOptions::open()` →
//! `fs2::FileExt::try_lock_exclusive()` → RAII-unlock pattern:
//!
//! 1. `ermya-graph-server` startup — held for the lifetime of the
//!    running server so two server processes can never share identity
//!    state.
//! 2. `ermya-graph-cli admin users …` — held while the offline admin
//!    command opens the system graph; if the server is up, the lock is
//!    contended and the CLI exits with code 3.
//! 3. `ermya-graph-cli migrate` — added by Task 12 QR-5 so an
//!    operator who runs `migrate` while a v0.4 server is still up sees
//!    the same exit-code-3 contract instead of mutating `data/`
//!    underneath the live process.
//!
//! `acquire_exclusive` is the single source of truth for the contract.
//! The contention error is shaped as `std::io::Error` with kind
//! `WouldBlock`, so callers can map it to whatever surface their CLI
//! contract requires (`ServerError::Io`, `MigrationError::Backend`,
//! `(3, "…")`) without tangling kinds.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

/// Filename of the lock under `{data_dir}/system/`. Stable across
/// versions because the contract is shared between server and CLI; if
/// it ever moves, every offline tool must move with it in lockstep.
pub const LOCK_FILENAME: &str = "system.lock";

/// RAII guard over an exclusive `fs2` advisory lock on the system graph.
///
/// Released on drop; a `Drop` failure (extremely rare on Unix) is
/// downgraded to a `tracing::warn!` because there is no recovery path
/// from inside `drop` and the kernel will reap the lock when the file
/// descriptor closes anyway.
#[derive(Debug)]
pub struct SystemLockGuard {
    file: File,
    path: PathBuf,
}

impl SystemLockGuard {
    /// Path to the lock file, useful for error messages and tests.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for SystemLockGuard {
    fn drop(&mut self) {
        // UFCS: `File::unlock` was stabilised in Rust 1.89 but our MSRV
        // is 1.85, so stay on the `fs2::FileExt` trait method.
        if let Err(e) = fs2::FileExt::unlock(&self.file) {
            tracing::warn!(
                path = %self.path.display(),
                "failed to release system graph lock: {e}",
            );
        }
    }
}

/// Acquire an exclusive advisory lock over `{system_dir}/system.lock`.
///
/// `system_dir` is expected to exist (the caller usually creates it via
/// `fs::create_dir_all` first); this helper does not own directory
/// creation because the permission policy ("tighten new dirs to
/// `0o700`, refuse pre-existing loose dirs") is call-site-specific.
///
/// # Errors
///
/// * `ErrorKind::WouldBlock` — another process holds the lock. The
///   message names the offending path so the operator can identify it
///   without parsing flock internals.
/// * Any other `ErrorKind` — propagates directly from `OpenOptions::open`
///   (typically `PermissionDenied` or `NotFound` when the parent dir is
///   absent).
pub fn acquire_exclusive(system_dir: &Path) -> io::Result<SystemLockGuard> {
    let path = system_dir.join(LOCK_FILENAME);
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)?;

    // UFCS: MSRV 1.85 cannot call `std::fs::File::try_lock` (stable in
    // 1.89). The `fs2::FileExt::try_lock_exclusive` trait method is the
    // portable equivalent.
    fs2::FileExt::try_lock_exclusive(&file).map_err(|e| {
        io::Error::new(
            io::ErrorKind::WouldBlock,
            format!(
                "another process holds the system graph lock at {}: {e}",
                path.display()
            ),
        )
    })?;

    Ok(SystemLockGuard { file, path })
}
