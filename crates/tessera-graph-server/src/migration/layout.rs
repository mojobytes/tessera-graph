// SPDX-License-Identifier: BSL-1.1

//! El guardián de la versión del formato en disco.
//!
//! # Por qué no es de pago
//!
//! El módulo de migración estaba clasificado entero como de la edición de pago,
//! y no lo era. Dentro convivían dos cosas:
//!
//! - **Este guardián**, que comprueba que el formato en disco coincide con el
//!   que el binario entiende y se niega a tocar nada si no. Lo necesita
//!   cualquier servidor que escriba en disco, tenga una base o cien: sin él, un
//!   binario nuevo abriría datos con un formato viejo y los corrompería en
//!   silencio.
//! - **El plan de migración de una base a varias**, que sí es de pago por
//!   definición: sin catálogo no hay a qué migrar.
//!
//! Lo destapó compilar el árbol público por primera vez: el montaje común
//! llamaba al guardián en tres sitios y el módulo entero no viajaba, así que no
//! compilaba. La clasificación estaba mal, no el código.
//!
//! El cliente público confirma el diagnóstico desde el otro lado: su orden de
//! actualización de formato también usa esto.

use std::fs;
use std::io::Write;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// The disk layout this binary expects. Bumped whenever a non-backward
/// compatible on-disk format change ships. Multi-database (v0.5.0) is
/// the first bump from the implicit `1` baseline.
pub const CURRENT_DISK_LAYOUT: u32 = 2;

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SchemaVersion {
    pub disk_layout: u32,
    pub last_migrated_at_ms: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum MigrationError {
    #[error(
        "disk layout out of date; run 'tessera-graph-cli migrate' first \
         (found layout {found}, server expects {expected})"
    )]
    OutOfDate { found: u32, expected: u32 },
    #[error(
        "disk layout newer than server; upgrade server or restore older \
         data dir (found layout {found}, server expects {expected})"
    )]
    TooNew { found: u32, expected: u32 },
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("parse: {0}")]
    Parse(#[from] serde_json::Error),
    /// Migration step failed against the storage / system-graph
    /// backend. Wraps the upstream message verbatim rather than
    /// re-exporting the concrete error type — the migrator owns one
    /// failure surface and downstream `ServerError::Migration`
    /// formatting stays uniform.
    #[error("migration backend failure: {0}")]
    Backend(String),
}

/// Read `.tessera-version` from `data_dir` and reject any layout
/// other than [`CURRENT_DISK_LAYOUT`]. Missing marker is treated as
/// out-of-date — first-time installs are expected to call
/// [`write_if_missing`] right after this check.
///
/// # Errors
///
/// Returns [`MigrationError::OutOfDate`] when the file is absent or
/// names a layout below `CURRENT_DISK_LAYOUT`,
/// [`MigrationError::TooNew`] when the layout is above, and the
/// underlying I/O / parse errors in transport-level failures.
pub fn read_or_reject(data_dir: &Path) -> Result<SchemaVersion, MigrationError> {
    let path = data_dir.join(VERSION_FILE);
    // Read directly and discriminate on `ErrorKind::NotFound` rather
    // than the classic `exists() && fs::read(path)` TOCTOU pair: a
    // local actor with write access to `data_dir` could intercalate
    // between the check and the read.
    let bytes = match fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(MigrationError::OutOfDate {
                found: 0,
                expected: CURRENT_DISK_LAYOUT,
            });
        }
        Err(e) => return Err(MigrationError::Io(e)),
    };
    let v: SchemaVersion = serde_json::from_slice(&bytes)?;
    if v.disk_layout < CURRENT_DISK_LAYOUT {
        return Err(MigrationError::OutOfDate {
            found: v.disk_layout,
            expected: CURRENT_DISK_LAYOUT,
        });
    }
    if v.disk_layout > CURRENT_DISK_LAYOUT {
        return Err(MigrationError::TooNew {
            found: v.disk_layout,
            expected: CURRENT_DISK_LAYOUT,
        });
    }
    Ok(v)
}

/// Write a fresh `.tessera-version` with the current layout iff the
/// marker is absent. Used by the startup path to auto-bootstrap a
/// genuinely empty `data_dir` without overwriting the migration
/// history written by the CLI migrator (Task 12).
///
/// Returning `Ok(false)` signals "marker already present, left
/// untouched"; `Ok(true)` signals "marker just written".
///
/// # Errors
///
/// Propagates [`std::io::Error`] from the file write when the marker
/// is absent.
pub fn write_if_missing(data_dir: &Path, layout: u32) -> std::io::Result<bool> {
    // Use the open-with-`create_new` flag (`O_EXCL` on Unix) instead
    // of an `exists()` probe: the kernel guarantees atomicity, closing
    // the TOCTOU window between "is the marker there?" and "write it".
    // A pre-existing marker surfaces as `AlreadyExists`, which we
    // translate to the documented `Ok(false)` signal.
    match write_marker_with_perms(data_dir, layout, /* exclusive = */ true) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(e) => Err(e),
    }
}

/// Write a fresh `.tessera-version` with the current layout. Used by
/// the CLI migrator (Task 12) and by the startup helper
/// [`write_if_missing`].
///
/// # Errors
///
/// Propagates [`std::io::Error`] from the file write.
pub fn write(data_dir: &Path, layout: u32) -> std::io::Result<()> {
    write_marker_with_perms(data_dir, layout, /* exclusive = */ false)
}

pub(super) const VERSION_FILE: &str = ".tessera-version";

/// Internal write helper shared by [`write`] and [`write_if_missing`].
///
/// On Unix the marker is created (or truncated) with mode `0o600` so
/// it is not world-readable — the contents (disk layout, migration
/// timestamp) are useful for fingerprinting by a local user. Coherent
/// with the `0o700` enforced on the `system/` directory.
///
/// # Threat model (decision C, QR S7)
///
/// The marker leaks *local fingerprinting data* (disk layout integer,
/// last-migration epoch ms) to anyone with read access to `data_dir`.
/// We accept that exposure rather than attempt to encrypt or hide the
/// marker because:
///
/// - The disk layout integer is implicit in the `databases/` /
///   `system/` directory shape; an attacker with read access to
///   `data_dir` can already infer the layout from `ls`.
/// - The migration timestamp is operationally useful (audit trails,
///   "did this server actually upgrade?") and is durable provenance,
///   not a secret.
/// - Hiding the marker would conflict with the on-disk format being
///   inspectable by `tessera-graph-cli migrate --dry-run`, which is
///   the operator-facing recovery path.
///
/// The hardening goal is "no local user other than the owner sees
/// this": `0o600` on the file plus `0o700` on the parent dir
/// (enforced by the server's `enforce_system_dir_perms`) closes that
/// window. Cross-host exposure is the deployment's responsibility
/// (LUKS / dm-crypt / S3 SSE — outside this crate's surface).
///
/// `exclusive = true` opens with `O_EXCL` and surfaces
/// [`std::io::ErrorKind::AlreadyExists`] when the marker is already
/// there — used by [`write_if_missing`] to close the
/// `exists()`-then-`write` TOCTOU window.
pub(super) fn write_marker_with_perms(
    data_dir: &Path,
    layout: u32,
    exclusive: bool,
) -> std::io::Result<()> {
    let now_ms = u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis()),
    )
    .unwrap_or(0);
    let v = SchemaVersion {
        disk_layout: layout,
        last_migrated_at_ms: now_ms,
    };
    let s = serde_json::to_string_pretty(&v).map_err(std::io::Error::other)?;

    let path = data_dir.join(VERSION_FILE);
    let mut opts = fs::OpenOptions::new();
    opts.write(true);
    if exclusive {
        opts.create_new(true);
    } else {
        opts.create(true).truncate(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(&path)?;
    f.write_all(s.as_bytes())?;
    f.sync_all()?;
    Ok(())
}
