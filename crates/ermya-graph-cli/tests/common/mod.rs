// SPDX-License-Identifier: BSL-1.1

//! Shared seeding helpers for `admin databases` / `admin grants`
//! integration tests. Each test still owns its own `tempfile::TempDir`
//! and drives the CLI binary; the helpers just shortcut the per-test
//! "open the system graph and create N users / databases" boilerplate
//! that would otherwise duplicate across files.
//!
//! These helpers run synchronously by spinning up a dedicated
//! `current_thread` Tokio runtime — the CLI itself is async, but tests
//! treat seeding as a setup step independent of the assertion phase.
//!
//! **Lock contract (QR-Q6).** Unlike the production CLI (which goes
//! through `crate::admin::open_locked_store` and acquires the
//! exclusive `fs2` advisory lock), these test helpers open the system
//! graph **without** the lock. That is safe in tests because each one
//! owns a fresh `tempfile::TempDir`, so there is no other process or
//! thread contending for the same `system/` directory. Reusing the
//! same data dir across helper calls within a single test is fine for
//! the same reason — the helper itself does not race itself. **Do not
//! reuse the test data dir across processes** (spawning the CLI binary
//! while a helper holds an open `Graph` would deadlock against the
//! advisory lock the CLI tries to acquire).

use std::path::Path;
use std::sync::Arc;
use std::sync::RwLock as StdRwLock;

use ermya_graph::{Graph, GraphConfig};
use ermya_graph_server::auth::{
    DatabaseCatalog, DatabaseOptions, SecretString, SystemGraphAuthStore, UserStore,
};

/// Open the system graph at `{data_dir}/system/` and create one user.
/// Idempotent across calls only if `username` differs each time —
/// callers seed distinct users.
//
// `dead_code` is silenced because `tests/common/mod.rs` is compiled
// once per integration-test binary; helpers used only by some binaries
// would otherwise blow up `-D warnings`.
#[allow(dead_code)]
pub fn seed_user(data_dir: &Path, username: &str, password: &str) {
    let system_dir = data_dir.join("system");
    std::fs::create_dir_all(&system_dir).expect("create system/");

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("seed-time tokio runtime");
    rt.block_on(async {
        let graph =
            Graph::open(&system_dir, &GraphConfig::default()).expect("seed-time graph open");
        let store = SystemGraphAuthStore::new(Arc::new(StdRwLock::new(graph)))
            .expect("seed-time auth store");
        store
            .create_user(
                username,
                &SecretString::new(password.to_owned()),
                /* is_admin = */ false,
            )
            .await
            .expect("seed user");
    });
}

/// Open the system graph at `{data_dir}/system/` and create one database
/// with default options.
pub fn seed_database(data_dir: &Path, db_name: &str, created_by: &str) {
    let system_dir = data_dir.join("system");
    std::fs::create_dir_all(&system_dir).expect("create system/");

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("seed-time tokio runtime");
    rt.block_on(async {
        let graph =
            Graph::open(&system_dir, &GraphConfig::default()).expect("seed-time graph open");
        let store = SystemGraphAuthStore::new(Arc::new(StdRwLock::new(graph)))
            .expect("seed-time auth store");
        store
            .create_database(db_name, DatabaseOptions::default(), created_by)
            .await
            .expect("seed database");
    });
}

/// Parse a single audit-log line as JSON. Panics on invalid input so
/// the diagnostic points at the bad line directly. Shared by every
/// integration test that emits Task 14 events.
pub fn parse_log_line(line: &str) -> serde_json::Value {
    serde_json::from_str(line).expect("audit line must be valid JSON")
}

/// Walk a directory recursively yielding file paths. Used by the
/// "no --audit-log → no file" assertions to cover the whole tempdir,
/// since the operator could theoretically point the log anywhere
/// under data-dir.
pub fn walkdir(root: &Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(p) = stack.pop() {
        if let Ok(rd) = std::fs::read_dir(&p) {
            for entry in rd.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else {
                    out.push(path);
                }
            }
        }
    }
    out
}
