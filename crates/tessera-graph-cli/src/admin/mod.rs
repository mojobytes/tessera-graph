// SPDX-License-Identifier: BSL-1.1

//! Offline administrative commands.
//!
//! These commands operate directly on the persistent system graph at
//! `{data-dir}/system/` with an `fs2` exclusive advisory lock, which is
//! also held by the running server. That means CLI admin commands and
//! the server are mutually exclusive — by design: we never want two
//! processes mutating identity data concurrently.
//!
//! ## Shared helpers
//!
//! The three offline modules (`users`, `databases`, `grants`) follow an
//! identical prologue: validate `--data-dir`, create the `system/`
//! subdirectory, acquire the advisory lock, open the graph, build the
//! store. Helpers in this module factor that pattern so each module's
//! `run` stays focused on dispatch.

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use tessera_graph::{Graph, GraphConfig};
use tessera_graph_server::auth::SystemGraphAuthStore;
use tessera_graph_server::system_lock::{self, SystemLockGuard};

pub mod hash;
pub mod users;

/// Top-level failure: carries an exit code plus a human-readable
/// message that the caller prints to stderr. The code follows each
/// module's exit-code contract (commonly: `1` generic, `2` last-admin
/// in `users`, `3` lock contended).
pub type AdminResult = Result<(), (i32, String)>;

/// Reject an absent or non-directory `--data-dir` before any further
/// step opens it.
///
/// Pre-fix, the typo `--data-dir /srv/typo` was either forwarded to
/// `migration::run_pending` (which surfaced the absence through
/// whichever I/O step happened first), or silently materialized as a
/// phantom `system/` subdirectory under a missing path by the
/// `admin databases` / `admin grants` modules — both failure modes
/// produced misleading error text and inconsistent exit codes. Validating
/// up front pins both the wording (the argument name) and the exit
/// code (`1`, generic operator-input).
///
/// Regular files at the same path are also rejected so the operator
/// cannot accidentally point at a stray archive (`/var/backups/data.tar`)
/// and watch the next step fall over inside its rename / open.
///
/// `prefix` lets each module tag the error message with its own
/// subcommand name (`admin databases`, `admin grants`, `migrate`) so
/// the operator sees the actual command, not a generic "data-dir"
/// error.
pub(crate) fn validate_data_dir(prefix: &str, data_dir: &Path) -> Result<(), (i32, String)> {
    let md = std::fs::metadata(data_dir).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            (
                1,
                format!("{prefix}: --data-dir {} does not exist", data_dir.display()),
            )
        } else {
            (
                1,
                format!(
                    "{prefix}: cannot stat --data-dir {}: {e}",
                    data_dir.display()
                ),
            )
        }
    })?;
    if !md.is_dir() {
        return Err((
            1,
            format!(
                "{prefix}: --data-dir {} is not a directory",
                data_dir.display()
            ),
        ));
    }
    Ok(())
}

/// Map a [`system_lock::acquire_exclusive`] failure onto the CLI exit
/// contract: contention (`WouldBlock`) → exit `3` with an "is the
/// server running?" hint; everything else → exit `1`.
pub(crate) fn map_lock_error(e: &std::io::Error) -> (i32, String) {
    if e.kind() == std::io::ErrorKind::WouldBlock {
        (
            3,
            format!("system graph is locked by another process (is the server running?): {e}"),
        )
    } else {
        (1, format!("cannot acquire system graph lock: {e}"))
    }
}

/// Carries the locked, opened store + the `fs2` guard that keeps the
/// system graph exclusive for the duration of the operation.
///
/// **Do not let the `_lock` field name fool you** — it must stay alive
/// for the lifetime of `Holder`, because dropping `Holder` drops the
/// guard, which releases the advisory lock. Rebinding the guard to `_`
/// (e.g. via destructuring) would release it immediately and allow a
/// concurrent server start to race the admin operation.
///
/// Callers must keep the whole `LockedStore` alive (don't destructure
/// out the store and drop the rest) so the guard's destructor only runs
/// after the operation has finished.
pub(crate) struct LockedStore {
    pub store: SystemGraphAuthStore,
    // Kept alive for RAII; dropping releases the advisory lock. The
    // underscore prefix silences `dead_code` but the guard semantics
    // come from the destructor running on drop, not from reads.
    _lock: SystemLockGuard,
}

/// Validate `--data-dir`, ensure `system/` exists, acquire the
/// exclusive advisory lock, open the graph, and build the auth store.
///
/// This is the prologue shared verbatim by every offline admin module.
/// The returned [`LockedStore`] must be kept alive (and dropped only
/// after the operation completes) so the advisory lock stays held.
pub(crate) fn open_locked_store(
    prefix: &str,
    data_dir: &str,
) -> Result<LockedStore, (i32, String)> {
    let data_dir = PathBuf::from(data_dir);
    validate_data_dir(prefix, &data_dir)?;
    let system_dir = data_dir.join("system");
    std::fs::create_dir_all(&system_dir)
        .map_err(|e| (1, format!("cannot create {}: {e}", system_dir.display())))?;

    let lock = system_lock::acquire_exclusive(&system_dir).map_err(|e| map_lock_error(&e))?;

    let graph = Graph::open(&system_dir, &GraphConfig::new()).map_err(|e| {
        (
            1,
            format!("cannot open system graph at {}: {e}", system_dir.display()),
        )
    })?;
    let graph = Arc::new(RwLock::new(graph));
    let store = SystemGraphAuthStore::new(Arc::clone(&graph))
        .map_err(|e| (1, format!("cannot initialise auth store: {e}")))?;

    Ok(LockedStore { store, _lock: lock })
}
