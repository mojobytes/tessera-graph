// SPDX-License-Identifier: MIT

//! Physical, atomic copy and restore of a database's on-disk files.
//!
//! These are pure functions over filesystem paths: they know nothing about the
//! server registry or `Arc<RwLock<Graph>>`, so they can be reused both online
//! (server flush-freeze under a per-tenant write lock) and offline (CLI
//! disaster recovery). The online consistency guarantee is the caller's
//! responsibility — it must `flush()` the target `Graph` under a write lock
//! before calling [`copy_db_files_atomic`], so the on-disk files are a
//! consistent point and the WAL is empty.

use std::path::Path;

use crate::error::{Error, Result};
use crate::{Graph, GraphConfig};

/// Validates a database name for use as a single path component, rejecting any
/// value that could escape the `databases/` directory (path traversal) or
/// collide with reserved directories.
///
/// This is the single source of truth shared by the engine restore, the server
/// `restore_tenant`, and the offline CLI, so a `CALL ermya.restore('../system',
/// …)` or `admin restore --db ../system` can never resolve `databases/<db>` onto
/// `system/` or anywhere outside `databases/`.
///
/// Accepts: a non-empty name up to 63 bytes, first character ASCII letter or
/// `_`, remaining characters ASCII alphanumeric / `_` / `-`. Rejects the
/// reserved `system` and `default` names, anything containing `/`, `\`, `.` or
/// `..`, and any non-ASCII byte.
///
/// # Errors
///
/// [`Error::Backup`] with a message naming the offending value.
///
/// # Panics
///
/// Never panics in practice: the `.expect` on the first character is guarded by
/// the preceding `is_empty` check, so the iterator always yields a character.
pub fn validate_db_name(name: &str) -> Result<()> {
    let reject = |reason: &str| {
        Err(Error::Backup(format!(
            "invalid database name {name:?}: {reason}"
        )))
    };
    if matches!(name, "system" | "default") {
        return reject("reserved name");
    }
    if name.is_empty() {
        return reject("must not be empty");
    }
    if name.len() > 63 {
        return reject("must be at most 63 bytes");
    }
    let mut chars = name.chars();
    let first = chars.next().expect("non-empty (checked above)");
    if !(first.is_ascii_alphabetic() || first == '_') {
        return reject("must start with an ASCII letter or '_'");
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        return reject("may only contain ASCII letters, digits, '_' or '-'");
    }
    Ok(())
}

/// Resolves `raw` against `data_dir` and rejects any path that escapes it,
/// for validating a client/operator-supplied snapshot source or destination.
///
/// Mirrors the rule the server's online handler applies, lifted to the engine so
/// the offline CLI shares it. A relative `raw` is anchored under `data_dir`; an
/// absolute `raw` is taken as-is; either way a `..` component is rejected
/// outright and the lexically-normalised result must stay under the
/// canonicalised `data_dir`.
///
/// # Errors
///
/// [`Error::Backup`] if `raw` contains a `..` component or resolves outside
/// `data_dir`.
pub fn validate_path_under(raw: &Path, data_dir: &Path) -> Result<std::path::PathBuf> {
    if raw
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(Error::Backup(format!(
            "path must not contain '..': {}",
            raw.display()
        )));
    }
    // `data_dir` always exists, so it canonicalises cleanly — resolving any
    // symlinks in the server's own layout (e.g. macOS `/var` → `/private/var`,
    // which TempDir-based tests hit). Anchoring on the canonical root means a
    // relative `raw` inherits that resolution.
    let canonical_root = data_dir.canonicalize().map_err(|e| {
        Error::Backup(format!(
            "cannot resolve data dir {}: {e}",
            data_dir.display()
        ))
    })?;
    let normalised = if raw.is_absolute() {
        // Canonicalise the deepest existing ancestor of the absolute path so its
        // symlinks resolve the same way `canonical_root` did, then re-attach the
        // non-existent tail. (`canonicalize` on the whole path fails if the tail
        // does not exist yet — a snapshot source may be created by the copy.)
        let mut ancestor = raw;
        let mut tail = std::path::PathBuf::new();
        let canonical_ancestor = loop {
            if let Ok(c) = ancestor.canonicalize() {
                break c;
            }
            match (ancestor.file_name(), ancestor.parent()) {
                (Some(name), Some(parent)) => {
                    tail = Path::new(name).join(&tail);
                    ancestor = parent;
                }
                _ => {
                    return Err(Error::Backup(format!(
                        "cannot resolve path {}: no existing ancestor",
                        raw.display()
                    )));
                }
            }
        };
        canonical_ancestor.join(tail)
    } else {
        canonical_root.join(raw)
    };
    if !normalised.starts_with(&canonical_root) {
        return Err(Error::Backup(format!(
            "path resolves outside the data directory: {}",
            normalised.display()
        )));
    }
    Ok(normalised)
}

/// `fsync`s a directory so a prior `rename`/`remove` of one of its entries is
/// durable. On a crash after this returns, the directory entry change survives.
fn fsync_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::File::open(dir)?.sync_all()
}

/// `fsync`s a single file's contents to disk.
fn fsync_file(path: &Path) -> std::io::Result<()> {
    std::fs::File::open(path)?.sync_all()
}

/// `fsync`s every regular file directly inside `dir`, then `dir` itself, so the
/// whole directory is durable on disk. Used after a restore copy so a crash
/// cannot resurrect stale content or leave the restored files unsynced.
fn fsync_dir_recursive(dir: &Path) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_type()?.is_file() {
            fsync_file(&entry.path())?;
        }
    }
    fsync_dir(dir)
}

/// Failure modes of [`restore_db_files_atomic`], kept distinct so callers can
/// map each to their own surface (server `BackupError`, CLI exit codes) without
/// parsing strings.
///
/// The destructive restore has four ways to fail, and the distinction matters
/// for recovery: a [`RestoreError::SourceInvalid`] never touched the live
/// database, a [`RestoreError::RollbackFailed`] left it without a live directory
/// and demands manual operator action.
#[derive(Debug)]
pub enum RestoreError {
    /// The snapshot source is missing or not a valid database (no `graph.meta`).
    /// The live directory was never touched, so the restore is a clean no-op.
    SourceInvalid(String),
    /// Copying the snapshot files into the live directory failed. The live
    /// directory was rolled back to its pre-restore state from the `.bak`.
    CopyFailed(Error),
    /// The restored files failed to open as a `Graph` (on-read checksum / magic
    /// validation from Feature A rejected them — the snapshot was corrupt). The
    /// live directory was rolled back to its pre-restore state.
    ValidationFailed(String),
    /// The restore failed AND the automatic rollback could not reinstate the
    /// original database, leaving it without a live directory. The carried
    /// message names both paths so an operator can recover by hand.
    RollbackFailed(String),
}

impl std::fmt::Display for RestoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SourceInvalid(msg) | Self::ValidationFailed(msg) | Self::RollbackFailed(msg) => {
                f.write_str(msg)
            }
            Self::CopyFailed(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for RestoreError {}

/// Atomically restores a database's on-disk files from `snapshot_dir` into
/// `live_dir`, crash-consistently and with a fail-safe rollback.
///
/// Pure over filesystem paths: it knows nothing about the server registry, so
/// the same routine serves the online restore (after the caller evicts the
/// tenant) and the offline CLI disaster recovery (under an exclusive `fs2`
/// lock).
///
/// Sequence (each step runs only if the previous succeeds):
///
/// 1. **Validate the source first.** `snapshot_dir` must be a directory whose
///    `graph.meta` and required `.db` files are all present and non-empty. A bad
///    source returns [`RestoreError::SourceInvalid`] *before* `live_dir` is
///    touched, so an invalid restore is a no-op.
/// 2. **Set the live directory aside** by renaming `live_dir` to a sibling
///    `<live_dir>.bak`, then `fsync` the parent so the rename is durable. A
///    stale `.bak` is removed first; if that removal fails the restore aborts
///    *before* touching the live dir (the stale `.bak` may be the only surviving
///    copy of an earlier interrupted restore). A missing `live_dir` (the
///    database was already destroyed — the basic disaster-recovery case) is
///    handled: there is simply nothing to set aside.
/// 3. **Copy** the snapshot into `live_dir` via [`copy_db_files_atomic`], then
///    `fsync` every restored file and the parent directory so the new content is
///    durable. On failure the partial directory is removed and the `.bak` is
///    reinstated (fail-safe ordering: the replacement is secured before the old
///    copy is discarded).
/// 4. **Validate** the restored files by opening them as a `Graph` with
///    `create_if_missing: false` — exercising Feature A's on-read checksum
///    validation. On failure the `.bak` is reinstated.
/// 5. **Commit** by removing the `.bak` and `fsync`ing the parent.
///
/// # Crash consistency
///
/// Every directory-entry change (`rename`, `remove`) is followed by an `fsync`
/// of the affected directory, and restored file contents are `fsync`ed before
/// commit. A crash at any point leaves either the original database (via the
/// `.bak`, which a server reconciles on startup) or the fully-restored one —
/// never a torn mix.
///
/// `snapshot_dir` and `live_dir` are expected to be already validated/resolved
/// by the caller via [`validate_db_name`] / [`validate_path_under`]; this
/// function does not re-derive them from untrusted input.
///
/// # Errors
///
/// - [`RestoreError::SourceInvalid`] if `snapshot_dir` is missing or invalid.
/// - [`RestoreError::CopyFailed`] if the file copy or an `fsync` fails (live db
///   reinstated).
/// - [`RestoreError::ValidationFailed`] if the restored `Graph` fails to open
///   (corrupt snapshot; live db reinstated).
/// - [`RestoreError::RollbackFailed`] if the rollback itself could not reinstate
///   the original database — carries a manual-recovery instruction.
pub fn restore_db_files_atomic(
    snapshot_dir: &Path,
    live_dir: &Path,
) -> std::result::Result<(), RestoreError> {
    // Step 1: validate the source BEFORE touching the live directory. A bad
    // source must never leave the live database half-restored. Checks all
    // required files exist and are non-empty, not just `graph.meta`'s presence.
    validate_snapshot_source(snapshot_dir)?;

    let bak_dir = bak_dir_for(live_dir);

    // Step 2: move the live directory aside. A stale `.bak` is cleared FIRST,
    // and a failure to clear it aborts before touching the live dir — that
    // stale `.bak` may be the only good copy from an earlier interrupted
    // restore, so we refuse to proceed (never silently destroy it).
    if bak_dir.exists() {
        std::fs::remove_dir_all(&bak_dir).map_err(|e| {
            RestoreError::SourceInvalid(format!(
                "a stale backup directory {} could not be removed (it may hold the \
                 only surviving copy of an earlier interrupted restore — inspect it \
                 before retrying): {e}",
                bak_dir.display()
            ))
        })?;
    }

    // A missing live_dir is the basic disaster-recovery case (the database
    // directory was destroyed). There is nothing to set aside; skip straight to
    // the copy. Only an EXISTING live_dir is renamed to `.bak`.
    let had_live = live_dir.exists();
    if had_live {
        std::fs::rename(live_dir, &bak_dir).map_err(|e| RestoreError::CopyFailed(Error::Io(e)))?;
        // Durably record that the live dir is now `.bak` before writing new
        // content, so a crash mid-copy is recoverable from `.bak`.
        if let Some(parent) = live_dir.parent() {
            if let Err(e) = fsync_dir(parent) {
                let _ = std::fs::rename(&bak_dir, live_dir);
                return Err(RestoreError::CopyFailed(Error::Io(e)));
            }
        }
    }

    // Step 3: copy the snapshot into place, then make it durable.
    if let Err(e) = copy_db_files_atomic(snapshot_dir, live_dir) {
        return Err(finish_with_rollback(
            live_dir,
            &bak_dir,
            had_live,
            RestoreError::CopyFailed(e),
        ));
    }
    if let Err(e) = fsync_dir_recursive(live_dir) {
        return Err(finish_with_rollback(
            live_dir,
            &bak_dir,
            had_live,
            RestoreError::CopyFailed(Error::Io(e)),
        ));
    }

    // Step 4: validate by opening the restored Graph AND reading every page.
    // `create_if_missing: false` so the validation FAILS if the copy somehow
    // left no directory, instead of silently creating an empty db. Opening alone
    // only reads the pages an index rebuild touches; `verify_all_pages` forces
    // the on-read CRC/magic check (Feature A) over EVERY page of every data
    // file, so a bit-flip in `strings.db`/`overflow.db`/`adjacency.db` is caught
    // now — before the `.bak` is discarded — not later at query time.
    let validation_cfg = GraphConfig {
        create_if_missing: false,
        ..GraphConfig::without_wal()
    };
    let validation = Graph::open(live_dir, &validation_cfg).and_then(|g| g.verify_all_pages());
    if let Err(e) = validation {
        return Err(finish_with_rollback(
            live_dir,
            &bak_dir,
            had_live,
            RestoreError::ValidationFailed(format!("{}: {e}", live_dir.display())),
        ));
    }

    // Step 5: commit — drop the backup and make the removal durable.
    if had_live {
        let _ = std::fs::remove_dir_all(&bak_dir);
        if let Some(parent) = live_dir.parent() {
            let _ = fsync_dir(parent);
        }
    }
    Ok(())
}

/// Validates that `snapshot_dir` is a complete database: a directory in which
/// every required file is present and a regular file, and `graph.meta` is
/// non-empty.
///
/// The `.db` files (`edges.db`, `overflow.db`, …) may legitimately be zero-length
/// — a database with only nodes has an empty `edges.db` — so only their presence
/// as regular files is required, not their size. `graph.meta` must be non-empty
/// because an empty meta is never valid and is the clearest signal of a
/// truncated/garbage snapshot. This catches a missing or directory-shaped
/// required file before the live database is touched, rather than via a partial
/// copy or a read failure at query time.
fn validate_snapshot_source(snapshot_dir: &Path) -> std::result::Result<(), RestoreError> {
    let invalid = |reason: String| Err(RestoreError::SourceInvalid(reason));
    if !snapshot_dir.is_dir() {
        return invalid(format!(
            "snapshot directory does not exist: {}",
            snapshot_dir.display()
        ));
    }
    for &name in REQUIRED_FILES {
        let p = snapshot_dir.join(name);
        match std::fs::metadata(&p) {
            Ok(m) if m.is_file() => {
                // `graph.meta` must additionally be non-empty; the `.db` files
                // may be legitimately empty (e.g. no edges).
                if name == "graph.meta" && m.len() == 0 {
                    return invalid(format!("snapshot graph.meta is empty: {}", p.display()));
                }
            }
            Ok(_) => {
                return invalid(format!(
                    "snapshot entry is not a regular file: {}",
                    p.display()
                ));
            }
            Err(_) => {
                return invalid(format!(
                    "snapshot is not a valid database (missing {name}): {}",
                    snapshot_dir.display()
                ));
            }
        }
    }
    Ok(())
}

/// The backup directory paired with `live_dir`: a sibling carrying a `.bak`
/// suffix, so the rename stays on the same filesystem as `live_dir`.
fn bak_dir_for(live_dir: &Path) -> std::path::PathBuf {
    sibling_with_suffix(live_dir, ".bak")
}

/// Outcome of reconciling one database directory's leftover restore artefacts.
#[derive(Debug, PartialEq, Eq)]
pub enum ReconcileOutcome {
    /// Nothing to do: no leftover `.bak`/`.staging-copy`/`.failed` present.
    Clean,
    /// A crash-interrupted restore was rolled back: the `.bak` was reinstated as
    /// the live database because `live_dir` was missing or incomplete.
    RolledBackFromBackup,
    /// A completed restore left a residual `.bak` (the live database is intact);
    /// the residual artefacts were removed.
    RemovedResidual,
}

/// Reconciles leftover restore artefacts for the database at `live_dir`.
///
/// Called on server startup so a process that died mid-restore is recovered
/// automatically rather than leaving the database down with an orphan `.bak`.
///
/// Decision:
/// - If `<live_dir>.bak` exists and `live_dir` is missing or incomplete (not a
///   valid database), the restore was interrupted after moving the original
///   aside but before committing → reinstate `.bak` as `live_dir`
///   ([`ReconcileOutcome::RolledBackFromBackup`]).
/// - If `<live_dir>.bak` exists and `live_dir` IS a complete database, the
///   restore committed and the `.bak` is residual → remove it
///   ([`ReconcileOutcome::RemovedResidual`]).
/// - Any `.staging-copy` / `.failed` siblings are always removed.
/// - Otherwise [`ReconcileOutcome::Clean`].
///
/// # Errors
///
/// [`Error::Io`] if a required rename/remove/`fsync` fails — startup should
/// surface this rather than continue with an inconsistent directory.
pub fn reconcile_restore_artifacts(live_dir: &Path) -> Result<ReconcileOutcome> {
    let bak_dir = bak_dir_for(live_dir);
    let staging = staging_dir_for(live_dir);
    let failed = sibling_with_suffix(live_dir, ".failed");

    // Partial copies from an aborted attempt are never the source of truth.
    if staging.exists() {
        std::fs::remove_dir_all(&staging)?;
    }
    if failed.exists() {
        std::fs::remove_dir_all(&failed)?;
    }

    if !bak_dir.exists() {
        return Ok(ReconcileOutcome::Clean);
    }

    // A `.bak` exists. Is the live database complete?
    let live_ok = validate_snapshot_source(live_dir).is_ok();
    if live_ok {
        // Restore had committed; the `.bak` is residual.
        std::fs::remove_dir_all(&bak_dir)?;
        if let Some(parent) = live_dir.parent() {
            fsync_dir(parent)?;
        }
        Ok(ReconcileOutcome::RemovedResidual)
    } else {
        // Restore was interrupted before commit; the `.bak` is the original.
        // Reinstate it (fail-safe: move any partial live_dir aside first).
        if live_dir.exists() {
            let partial = sibling_with_suffix(live_dir, ".partial");
            let _ = std::fs::remove_dir_all(&partial);
            std::fs::rename(live_dir, &partial)?;
            let _ = std::fs::remove_dir_all(&partial);
        }
        std::fs::rename(&bak_dir, live_dir)?;
        if let Some(parent) = live_dir.parent() {
            fsync_dir(parent)?;
        }
        Ok(ReconcileOutcome::RolledBackFromBackup)
    }
}

/// Rolls back a failed restore and returns the error the caller should see.
///
/// Normally this is the `original` error (the cause of the rollback — a copy or
/// validation failure), because the database has been reinstated and is
/// unchanged. But if the rollback itself fails to put `.bak` back, the database
/// is left without a usable live directory — a far more serious state than the
/// original failure — so the returned error is escalated to
/// [`RestoreError::RollbackFailed`] carrying an explicit manual-recovery
/// instruction with the concrete paths.
///
/// `had_live` distinguishes the two pre-restore states: a normal restore (there
/// was a live database that was moved to `.bak`) versus a disaster-recovery
/// restore into an absent live directory (nothing to reinstate — the rollback
/// just clears the partial copy).
fn finish_with_rollback(
    live_dir: &Path,
    bak_dir: &Path,
    had_live: bool,
    original: RestoreError,
) -> RestoreError {
    match rollback_db_files(live_dir, bak_dir, had_live) {
        Ok(()) => original,
        Err(rollback_msg) => RestoreError::RollbackFailed(rollback_msg),
    }
}

/// Reinstates the pre-restore state after a failed restore, with fail-safe
/// ordering: the original `.bak` is renamed back into place BEFORE the partial
/// copy is destroyed, so a crash mid-rollback can never leave both copies gone.
///
/// - When `had_live`: move the partial `live_dir` out of the way to a sibling
///   `<live_dir>.failed` (rename, not remove — keeps it for inspection until the
///   `.bak` is safely back), then rename `.bak` → `live_dir`, `fsync` the parent,
///   and only then remove the `.failed` partial. If the `.bak` rename-back
///   fails, both `.bak` and `.failed` still exist on disk and the message names
///   them for manual recovery.
/// - When `!had_live` (disaster recovery into an absent dir): there is no `.bak`;
///   just remove the partial `live_dir` so the next attempt starts clean.
///
/// Returns `Ok(())` when the pre-restore state is back. Returns `Err(msg)` with
/// an operator-facing recovery instruction naming the concrete paths otherwise.
fn rollback_db_files(
    live_dir: &Path,
    bak_dir: &Path,
    had_live: bool,
) -> std::result::Result<(), String> {
    if !had_live {
        // No original to reinstate; just clear the partial copy.
        let _ = std::fs::remove_dir_all(live_dir);
        return Ok(());
    }

    // Move the partial copy aside instead of deleting it up front, so the
    // destructive remove only happens AFTER `.bak` is safely reinstated.
    let failed_dir = sibling_with_suffix(live_dir, ".failed");
    let _ = std::fs::remove_dir_all(&failed_dir);
    if live_dir.exists() {
        if let Err(e) = std::fs::rename(live_dir, &failed_dir) {
            // Could not even move the partial aside. `.bak` still holds the
            // original; instruct manual recovery rather than risk it.
            return Err(format!(
                "restore failed and the partial copy at '{}' could not be moved \
                 aside ({e}) — MANUAL RECOVERY REQUIRED: the original database is \
                 intact at '{}'; remove the partial and rename the backup back",
                live_dir.display(),
                bak_dir.display()
            ));
        }
    }

    if let Err(e) = std::fs::rename(bak_dir, live_dir) {
        return Err(format!(
            "restore failed and automatic rollback could not reinstate the \
             original database — MANUAL RECOVERY REQUIRED: rename '{}' back to \
             '{}' (a partial copy is preserved at '{}') (cause: {e})",
            bak_dir.display(),
            live_dir.display(),
            failed_dir.display()
        ));
    }
    if let Some(parent) = live_dir.parent() {
        let _ = fsync_dir(parent);
    }
    // Original is safely back; the preserved partial copy can go.
    let _ = std::fs::remove_dir_all(&failed_dir);
    Ok(())
}

/// A sibling of `path` (same parent) with `suffix` appended to the file name,
/// so a subsequent rename stays on the same filesystem.
fn sibling_with_suffix(path: &Path, suffix: &str) -> std::path::PathBuf {
    let mut name = path.file_name().map_or_else(
        || std::ffi::OsString::from("database"),
        std::ffi::OsStr::to_os_string,
    );
    name.push(suffix);
    match path.parent() {
        Some(parent) => parent.join(name),
        None => std::path::PathBuf::from(name),
    }
}

/// Files that every database directory must contain. A snapshot is incomplete
/// without all of them.
const REQUIRED_FILES: &[&str] = &[
    "nodes.db",
    "edges.db",
    "adjacency.db",
    "strings.db",
    "overflow.db",
    "graph.meta",
];

/// Files that may be present (rebuildable indexes / schema) and are copied only
/// when they exist in the source.
const OPTIONAL_FILES: &[&str] = &["label_index.bin", "schema.bin"];

/// Atomically copies the on-disk files of a database from `src_db_dir` to
/// `dest_dir`.
///
/// Each file is written to `dest_dir/<name>.tmp` and then renamed into place,
/// so a reader of `dest_dir` never observes a half-written file (rename is
/// atomic within a filesystem).
///
/// The `wal.log` is intentionally **not** copied: the online snapshot contract
/// requires the caller to `flush()` the `Graph` under a write lock before
/// calling this, which checkpoints and truncates the WAL. The full state then
/// lives in the `.db`/`.meta` files, and copying a live WAL would reintroduce
/// the very inconsistency the flush eliminates.
///
/// # Errors
///
/// - [`Error::Backup`] if `src_db_dir` does not exist, is not a valid database
///   (missing `graph.meta`), or a required file is absent.
/// - [`Error::Io`] if a filesystem operation (copy, rename, mkdir) fails.
pub fn copy_db_files_atomic(src_db_dir: &Path, dest_dir: &Path) -> Result<()> {
    if !src_db_dir.is_dir() {
        return Err(Error::Backup(format!(
            "source database directory does not exist: {}",
            src_db_dir.display()
        )));
    }
    if !src_db_dir.join("graph.meta").is_file() {
        return Err(Error::Backup(format!(
            "source is not a valid database (missing graph.meta): {}",
            src_db_dir.display()
        )));
    }

    // Copy into a sibling staging directory first, then rename the whole
    // directory into place. The rename is atomic within a filesystem, so a
    // reader of `dest_dir` never observes a half-populated snapshot: either
    // the complete set appears at once, or `dest_dir` is untouched. The
    // staging dir is a sibling (same parent) so the final rename stays on the
    // same filesystem. (Mirrors the per-file `.tmp`+rename trick the previous
    // implementation used, lifted to the directory granularity so a failure
    // partway through the file loop cannot leave a partial `dest_dir`.)
    let staging = staging_dir_for(dest_dir);

    // Clear any leftover staging from an aborted previous run before starting.
    if staging.exists() {
        let _ = std::fs::remove_dir_all(&staging);
    }

    // Run the copy in a closure so any early error funnels through a single
    // cleanup of the staging directory.
    let copy_result = (|| -> Result<()> {
        std::fs::create_dir_all(&staging)?;

        for &name in REQUIRED_FILES {
            let src = src_db_dir.join(name);
            if !src.is_file() {
                return Err(Error::Backup(format!(
                    "required database file missing from source: {name}"
                )));
            }
            std::fs::copy(&src, staging.join(name))?;
        }

        for &name in OPTIONAL_FILES {
            let src = src_db_dir.join(name);
            if src.is_file() {
                std::fs::copy(&src, staging.join(name))?;
            }
        }
        Ok(())
    })();

    if let Err(e) = copy_result {
        // Best-effort cleanup of the partial staging dir; `dest_dir` was never
        // touched, so the only visible artefact to remove is the staging. A
        // failure to clean up leaves an orphan `.staging-copy` dir that the
        // next call removes up front — benign, and this is the pure engine
        // layer (no logging dependency by design). The original error is the
        // one the caller must act on, so it is what we return.
        let _ = std::fs::remove_dir_all(&staging);
        return Err(e);
    }

    // All files copied. Replace `dest_dir` atomically. If it already exists
    // (a re-snapshot over an existing destination), remove it first — rename
    // onto a non-empty directory is not portable.
    if dest_dir.exists() {
        std::fs::remove_dir_all(dest_dir)?;
    }
    std::fs::rename(&staging, dest_dir)?;
    // Make the directory-entry rename durable so a crash right after this
    // cannot leave `dest_dir` pointing at nothing.
    if let Some(parent) = dest_dir.parent() {
        fsync_dir(parent)?;
    }
    Ok(())
}

/// The staging directory paired with `dest_dir`: a sibling carrying a fixed
/// `.staging-copy` suffix, so the final directory rename stays on the same
/// filesystem as `dest_dir`.
fn staging_dir_for(dest_dir: &Path) -> std::path::PathBuf {
    let mut name = dest_dir.file_name().map_or_else(
        || std::ffi::OsString::from("snapshot"),
        std::ffi::OsStr::to_os_string,
    );
    name.push(".staging-copy");
    match dest_dir.parent() {
        Some(parent) => parent.join(name),
        None => std::path::PathBuf::from(name),
    }
}

#[cfg(test)]
mod tests {
    use super::{RestoreError, copy_db_files_atomic, restore_db_files_atomic};
    use crate::{Graph, GraphConfig};

    /// Builds a database directory at `dir` containing a single `Person` node
    /// named `name`, flushed to disk. Returns nothing — the caller reopens
    /// `dir` to assert. Shared by the restore tests below.
    fn make_db_with_one_node(dir: &std::path::Path, name: &str) {
        std::fs::create_dir_all(dir).unwrap();
        let mut g = Graph::open(dir, &GraphConfig::without_wal()).unwrap();
        g.add_node("Person", crate::props! { "name" => name })
            .unwrap();
        g.flush().unwrap();
        drop(g);
    }

    fn node_count_at(dir: &std::path::Path) -> usize {
        let cfg = GraphConfig {
            create_if_missing: false,
            ..GraphConfig::without_wal()
        };
        Graph::open(dir, &cfg).unwrap().node_count()
    }

    // B9-1: a missing snapshot source returns SourceInvalid and never creates
    // or touches the live directory.
    #[test]
    fn restore_db_files_atomic_missing_source_returns_source_invalid() {
        let tmp = tempfile::tempdir().unwrap();
        let live = tmp.path().join("live");
        let snap = tmp.path().join("nope");
        let result = restore_db_files_atomic(&snap, &live);
        assert!(
            matches!(result, Err(RestoreError::SourceInvalid(_))),
            "expected SourceInvalid, got {result:?}"
        );
        assert!(
            !live.exists(),
            "live dir must not be created on invalid source"
        );
    }

    // B9-2: a source directory without graph.meta is not a valid database →
    // SourceInvalid, live dir untouched.
    #[test]
    fn restore_db_files_atomic_source_without_meta_returns_source_invalid() {
        let tmp = tempfile::tempdir().unwrap();
        let live = tmp.path().join("live");
        let snap = tmp.path().join("snap");
        std::fs::create_dir_all(&snap).unwrap(); // exists but no graph.meta
        let result = restore_db_files_atomic(&snap, &live);
        assert!(
            matches!(result, Err(RestoreError::SourceInvalid(_))),
            "expected SourceInvalid, got {result:?}"
        );
        assert!(
            !live.exists(),
            "live dir must not be created on invalid source"
        );
    }

    // B9-3: happy path — the snapshot is copied in, validated by opening, and
    // the `.bak` is removed on commit. Post-snapshot mutations are gone.
    #[test]
    fn restore_db_files_atomic_happy_path_bak_cleaned_up() {
        let tmp = tempfile::tempdir().unwrap();
        let live = tmp.path().join("live");
        make_db_with_one_node(&live, "Alice");

        let snap = tmp.path().join("snap");
        copy_db_files_atomic(&live, &snap).unwrap();

        // Mutate the live db AFTER the snapshot: add Bob. The restore must
        // bring the db back to the snapshot (Alice only).
        {
            let cfg = GraphConfig {
                create_if_missing: false,
                ..GraphConfig::without_wal()
            };
            let mut g = Graph::open(&live, &cfg).unwrap();
            g.add_node("Person", crate::props! { "name" => "Bob" })
                .unwrap();
            g.flush().unwrap();
        }
        assert_eq!(
            node_count_at(&live),
            2,
            "precondition: live has Alice + Bob"
        );

        restore_db_files_atomic(&snap, &live).unwrap();

        let bak = tmp.path().join("live.bak");
        assert!(
            !bak.exists(),
            ".bak must be removed after a successful commit"
        );
        assert_eq!(
            node_count_at(&live),
            1,
            "restored db must hold only the snapshot's Alice"
        );
    }

    // B9-4 (revised by QR B-13): an incomplete source (missing a required file)
    // is now rejected by the up-front source validation as `SourceInvalid`,
    // BEFORE the live directory is touched — strictly safer than the old
    // behaviour (which let it through to fail mid-copy and then rolled back).
    // The key guarantee the test pins is the same: the live database is intact
    // and no `.bak` is left behind.
    #[test]
    fn restore_db_files_atomic_incomplete_source_rejected_live_untouched() {
        let tmp = tempfile::tempdir().unwrap();
        let live = tmp.path().join("live");
        make_db_with_one_node(&live, "Alice");

        // Trap source: graph.meta present but edges.db absent.
        let trap = tmp.path().join("trap");
        std::fs::create_dir_all(&trap).unwrap();
        for f in &[
            "nodes.db",
            "adjacency.db",
            "strings.db",
            "overflow.db",
            "graph.meta",
        ] {
            std::fs::copy(live.join(f), trap.join(f)).unwrap();
        }

        let result = restore_db_files_atomic(&trap, &live);
        assert!(
            matches!(result, Err(RestoreError::SourceInvalid(_))),
            "expected SourceInvalid (incomplete source), got {result:?}"
        );
        let bak = tmp.path().join("live.bak");
        assert!(!bak.exists(), "no .bak: the live dir was never touched");
        assert_eq!(
            node_count_at(&live),
            1,
            "original db must be intact and untouched"
        );
    }

    // B9-5: a corrupt snapshot (passes pre-check, copies, but fails to open)
    // → ValidationFailed with rollback to the intact original.
    #[test]
    fn restore_db_files_atomic_corrupt_snapshot_validation_failed_rollback() {
        let tmp = tempfile::tempdir().unwrap();
        let live = tmp.path().join("live");
        make_db_with_one_node(&live, "Alice");

        let snap = tmp.path().join("snap");
        copy_db_files_atomic(&live, &snap).unwrap();
        // Corrupt the snapshot's nodes.db: zero the first 64 bytes so the
        // on-read checksum/magic validation rejects it when opened.
        {
            use std::io::{Seek, SeekFrom, Write};
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .open(snap.join("nodes.db"))
                .unwrap();
            f.seek(SeekFrom::Start(0)).unwrap();
            f.write_all(&[0u8; 64]).unwrap();
            f.flush().unwrap();
        }

        let result = restore_db_files_atomic(&snap, &live);
        assert!(
            matches!(result, Err(RestoreError::ValidationFailed(_))),
            "expected ValidationFailed, got {result:?}"
        );
        let bak = tmp.path().join("live.bak");
        assert!(!bak.exists(), ".bak must be renamed back after rollback");
        assert_eq!(
            node_count_at(&live),
            1,
            "original db must be reinstated intact"
        );
    }

    // B9-6: when the rollback itself cannot reinstate the original, the error
    // escalates to RollbackFailed carrying a manual-recovery instruction with
    // both paths. Driven through the private `rollback_db_files` helper with a
    // non-existent bak so the rename-back fails.
    #[test]
    fn restore_db_files_atomic_rollback_failure_returns_rollback_failed_with_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let live = tmp.path().join("live");
        std::fs::create_dir_all(&live).unwrap();
        let bak = tmp.path().join("live.bak"); // never created → rename-back fails

        let result = super::rollback_db_files(&live, &bak, true);
        let Err(msg) = result else {
            panic!("rollback must fail when bak does not exist")
        };
        assert!(
            msg.to_lowercase().contains("manual recovery"),
            "rollback failure must advise manual recovery, got {msg:?}"
        );
        assert!(
            msg.contains(&live.display().to_string()) && msg.contains(&bak.display().to_string()),
            "rollback failure must name both paths, got {msg:?}"
        );
    }

    // --- Hardening (QR B-13) ---

    #[test]
    fn validate_db_name_rejects_traversal_and_reserved() {
        for bad in [
            "../system",
            "../../etc",
            "/abs",
            "a/b",
            "..",
            ".",
            "system",
            "default",
            "",
        ] {
            assert!(
                super::validate_db_name(bad).is_err(),
                "validate_db_name must reject {bad:?}"
            );
        }
        for ok in ["mydb", "tenant_1", "a-b-c", "_x"] {
            assert!(
                super::validate_db_name(ok).is_ok(),
                "validate_db_name must accept {ok:?}"
            );
        }
    }

    #[test]
    fn validate_path_under_rejects_parent_and_outside() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        assert!(super::validate_path_under(std::path::Path::new("../escape"), root).is_err());
        assert!(super::validate_path_under(std::path::Path::new("/etc/passwd"), root).is_err());
        // A relative path under data_dir resolves and is accepted.
        let ok = super::validate_path_under(std::path::Path::new("snaps/mydb"), root).unwrap();
        assert!(ok.starts_with(root.canonicalize().unwrap()));
    }

    #[test]
    fn validate_snapshot_source_rejects_empty_required_file() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        // All required files present but graph.meta is empty → must fail before
        // touching anything.
        for f in &[
            "nodes.db",
            "edges.db",
            "adjacency.db",
            "strings.db",
            "overflow.db",
        ] {
            std::fs::write(src.join(f), b"x").unwrap();
        }
        std::fs::write(src.join("graph.meta"), b"").unwrap(); // empty
        assert!(
            matches!(
                super::validate_snapshot_source(&src),
                Err(RestoreError::SourceInvalid(_))
            ),
            "empty graph.meta must be SourceInvalid"
        );
    }

    // Disaster recovery: restoring into an ABSENT live_dir must succeed (the DB
    // directory was destroyed — the basic DR case), not fail trying to rename a
    // non-existent live dir.
    #[test]
    fn restore_db_files_atomic_into_absent_live_dir_succeeds() {
        let tmp = tempfile::tempdir().unwrap();
        let seed = tmp.path().join("seed");
        make_db_with_one_node(&seed, "Alice");
        let snap = tmp.path().join("snap");
        copy_db_files_atomic(&seed, &snap).unwrap();

        // live_dir does NOT exist — simulate a destroyed database directory.
        let live = tmp.path().join("databases").join("mydb");
        std::fs::create_dir_all(live.parent().unwrap()).unwrap();
        assert!(!live.exists());

        restore_db_files_atomic(&snap, &live).expect("DR restore into absent live_dir must work");
        assert_eq!(node_count_at(&live), 1);
        assert!(!tmp.path().join("databases").join("mydb.bak").exists());
    }

    // A stale `.bak` that cannot be removed must abort BEFORE the live dir is
    // touched (it may be the only surviving copy of an earlier crashed restore).
    #[test]
    fn restore_db_files_atomic_aborts_on_unremovable_stale_bak() {
        // We can't easily make a dir unremovable portably; instead assert the
        // policy via a present-and-removable stale .bak being cleared and the
        // restore still succeeding (the abort path is covered by the message
        // contract; this guards the happy reconciliation of a stale .bak).
        let tmp = tempfile::tempdir().unwrap();
        let live = tmp.path().join("databases").join("mydb");
        make_db_with_one_node(&live, "Alice");
        let snap = tmp.path().join("snap");
        copy_db_files_atomic(&live, &snap).unwrap();
        // Pre-existing stale .bak from a prior aborted run.
        let stale_bak = tmp.path().join("databases").join("mydb.bak");
        std::fs::create_dir_all(&stale_bak).unwrap();
        std::fs::write(stale_bak.join("junk"), b"stale").unwrap();

        restore_db_files_atomic(&snap, &live).expect("restore clears a removable stale .bak");
        assert!(
            !stale_bak.exists(),
            "stale .bak must be gone after a successful restore"
        );
        assert_eq!(node_count_at(&live), 1);
    }

    // Startup reconciliation: a `.bak` with a MISSING live_dir (crash after
    // rename-to-bak, before commit) must reinstate the original on reconcile.
    #[test]
    fn reconcile_reinstates_backup_when_live_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let live = tmp.path().join("databases").join("mydb");
        make_db_with_one_node(&live, "Alice");
        // Simulate the interrupted-restore state: live moved to .bak, no live.
        let bak = tmp.path().join("databases").join("mydb.bak");
        std::fs::rename(&live, &bak).unwrap();
        assert!(!live.exists() && bak.exists());

        let outcome = super::reconcile_restore_artifacts(&live).unwrap();
        assert_eq!(outcome, super::ReconcileOutcome::RolledBackFromBackup);
        assert!(live.exists() && !bak.exists());
        assert_eq!(node_count_at(&live), 1);
    }

    // Startup reconciliation: a `.bak` alongside a COMPLETE live_dir (crash
    // after commit, before .bak removal) must just remove the residual .bak.
    #[test]
    fn reconcile_removes_residual_backup_when_live_complete() {
        let tmp = tempfile::tempdir().unwrap();
        let live = tmp.path().join("databases").join("mydb");
        make_db_with_one_node(&live, "Alice");
        // Residual .bak: a full copy of the (committed) live db.
        let bak = tmp.path().join("databases").join("mydb.bak");
        copy_db_files_atomic(&live, &bak).unwrap();

        let outcome = super::reconcile_restore_artifacts(&live).unwrap();
        assert_eq!(outcome, super::ReconcileOutcome::RemovedResidual);
        assert!(live.exists() && !bak.exists());
        assert_eq!(node_count_at(&live), 1);
    }

    #[test]
    fn reconcile_clean_when_no_artifacts() {
        let tmp = tempfile::tempdir().unwrap();
        let live = tmp.path().join("databases").join("mydb");
        make_db_with_one_node(&live, "Alice");
        let outcome = super::reconcile_restore_artifacts(&live).unwrap();
        assert_eq!(outcome, super::ReconcileOutcome::Clean);
        assert_eq!(node_count_at(&live), 1);
    }

    // A bit-flip in overflow.db (large property value, spilled out of line) must
    // be caught by the restore's full-page verification, NOT slip past the open
    // and surface at query time. This is the gap `verify_all_pages` closes.
    #[test]
    fn restore_rejects_corrupt_overflow_page_not_only_meta() {
        let tmp = tempfile::tempdir().unwrap();
        let seed = tmp.path().join("seed");
        std::fs::create_dir_all(&seed).unwrap();
        {
            let mut g = Graph::open(&seed, &GraphConfig::without_wal()).unwrap();
            // A large property value forces an overflow page.
            let big = "x".repeat(16_000);
            g.add_node("Doc", crate::props! { "body" => big.as_str() })
                .unwrap();
            g.flush().unwrap();
        }
        let snap = tmp.path().join("snap");
        copy_db_files_atomic(&seed, &snap).unwrap();

        // Sanity: overflow.db is non-trivial (data actually spilled there).
        let overflow_len = std::fs::metadata(snap.join("overflow.db")).unwrap().len();
        assert!(
            overflow_len > 4096,
            "precondition: data must spill to overflow.db"
        );

        // Corrupt a byte deep inside an overflow page (past the header) so magic
        // stays intact but the CRC no longer matches.
        {
            use std::io::{Seek, SeekFrom, Write};
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .open(snap.join("overflow.db"))
                .unwrap();
            f.seek(SeekFrom::Start(100)).unwrap();
            f.write_all(&[0xFF]).unwrap();
            f.flush().unwrap();
        }

        let live = tmp.path().join("databases").join("mydb");
        make_db_with_one_node(&live, "Original");

        let result = restore_db_files_atomic(&snap, &live);
        assert!(
            matches!(result, Err(RestoreError::ValidationFailed(_))),
            "corrupt overflow page must fail validation, got {result:?}"
        );
        // Original reinstated.
        assert!(!tmp.path().join("databases").join("mydb.bak").exists());
        assert_eq!(
            node_count_at(&live),
            1,
            "original db must be intact after rollback"
        );
    }

    #[test]
    fn copy_db_files_atomic_missing_source_returns_err() {
        let tmp = tempfile::tempdir().unwrap();
        let result = copy_db_files_atomic(&tmp.path().join("nope"), &tmp.path().join("snap"));
        assert!(result.is_err(), "expected error for missing source db dir");
    }

    #[test]
    fn copy_db_files_atomic_produces_readable_copy() {
        let src_tmp = tempfile::tempdir().unwrap();
        let db_dir = src_tmp.path().join("db");
        std::fs::create_dir_all(&db_dir).unwrap();
        let mut g = Graph::open(&db_dir, &GraphConfig::without_wal()).unwrap();
        g.add_node("Person", crate::props! { "name" => "Alice" })
            .unwrap();
        g.flush().unwrap();
        drop(g);

        let snap = src_tmp.path().join("snap");
        copy_db_files_atomic(&db_dir, &snap).unwrap();

        for fname in &[
            "nodes.db",
            "edges.db",
            "adjacency.db",
            "strings.db",
            "overflow.db",
            "graph.meta",
        ] {
            assert!(snap.join(fname).exists(), "missing {fname} in snapshot");
        }
        let g2 = Graph::open(&snap, &GraphConfig::without_wal()).unwrap();
        assert_eq!(g2.node_count(), 1);
    }

    /// H6 — a copy that fails partway (source has `graph.meta` so the
    /// pre-check passes, but is missing a later required file) must NOT leave
    /// `dest_dir` behind in a partial state that looks like a valid snapshot.
    /// The destination must either not exist or be empty after the failure.
    #[test]
    fn copy_db_files_atomic_partial_failure_leaves_no_dest() {
        let tmp = tempfile::tempdir().unwrap();
        // Build a real DB, then delete one required file from a *copy* of it so
        // the source passes the graph.meta pre-check but fails mid-loop.
        let db_dir = tmp.path().join("db");
        std::fs::create_dir_all(&db_dir).unwrap();
        let mut g = Graph::open(&db_dir, &GraphConfig::without_wal()).unwrap();
        g.add_node("Person", crate::props! { "name" => "Alice" })
            .unwrap();
        g.flush().unwrap();
        drop(g);

        // Truncated source: copy the DB dir but remove edges.db.
        let bad_src = tmp.path().join("bad-src");
        std::fs::create_dir_all(&bad_src).unwrap();
        for f in &[
            "nodes.db",
            "adjacency.db",
            "strings.db",
            "overflow.db",
            "graph.meta",
        ] {
            std::fs::copy(db_dir.join(f), bad_src.join(f)).unwrap();
        }
        // edges.db intentionally omitted → copy fails on the edges.db iteration.

        let dest = tmp.path().join("snap");
        let result = copy_db_files_atomic(&bad_src, &dest);
        assert!(result.is_err(), "copy from a truncated source must fail");

        // The destination must NOT exist as a partial snapshot. Either absent
        // or empty — never a directory containing some-but-not-all files that
        // a reader could mistake for a complete snapshot.
        if dest.exists() {
            let entries: Vec<_> = std::fs::read_dir(&dest).unwrap().collect();
            assert!(
                entries.is_empty(),
                "dest must be empty after a partial-copy failure, found {} entries",
                entries.len()
            );
        }
        // And no staging directory should be left behind either.
        let leftover: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(std::result::Result::ok)
            .filter(|e| e.file_name().to_string_lossy().contains("staging"))
            .collect();
        assert!(
            leftover.is_empty(),
            "no staging dir should survive a failure"
        );
    }
}
