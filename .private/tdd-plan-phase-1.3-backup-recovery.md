# TDD Plan: Phase 1.3 — Backup & Recovery

**Date**: 2026-03-14
**Scope**: tessera-storage-enterprise
**Branch**: feature/backup-recovery from develop

---

## Architecture Overview

### What the codebase tells us

After reading all twelve source files, the key constraints are:

**FileBackend** stores everything in a flat directory:
- `nodes.db`, `edges.db`, `adjacency.db`, `strings.db`, `overflow.db` — paged data files
- `graph.meta` — single-page metadata (node/edge counts, page counts, dirty flag)
- `wal.log` — write-ahead log (truncated after every `flush`)
- `index.bin` — serialized label indexes

`FileBackend` does NOT expose its `dir` field publicly. The backup engine needs the graph's data directory path passed explicitly — it cannot be derived from `Graph` or `FileBackend` alone.

**TransactionManager** owns `Mutex<WalWriter>`. The WAL writer inside `TransactionManager` is the enterprise WAL, which runs alongside the `FileBackend` WAL (they share the same `wal.log` file path). This is the critical insight: locking `TransactionManager::wal` during the snapshot window freezes new enterprise WAL records, creating a consistent LSN boundary.

**WalWriter::truncate()** clears file content but preserves `next_lsn`. The WAL tail copy must happen before any truncation.

**Graph::flush()** calls `storage.flush()` → `storage.wal_checkpoint_and_truncate()`. So after a flush, the WAL is truncated. The snapshot must capture the WAL tail *before* the next flush.

### Design Decisions

**1. Snapshot format — what files to copy**

The backup directory will contain:
```
<backup_dir>/
    nodes.db
    edges.db
    adjacency.db
    strings.db
    overflow.db
    graph.meta
    wal.log          (WAL tail at snapshot LSN — may be empty if freshly flushed)
    index.bin        (label index — may not exist on new stores)
    manifest.json    (backup metadata: timestamp, snapshot_lsn, file list, CRC32 per file)
```

**2. Consistency point — the freeze window**

The procedure:
1. Lock `TransactionManager::wal` (Mutex) — freezes new enterprise WAL records
2. Call `Graph::flush()` via `SharedGraph::write()` — flushes dirty pages, checkpoints, truncates FileBackend WAL
3. Copy all data files (they are now fully consistent on disk)
4. Copy the WAL file (will be empty or have only the checkpoint record, since flush truncates it)
5. Write the manifest
6. Release the WAL lock — operations resume

This means the WAL tail copy is essentially a copy of the freshly-truncated WAL (empty or near-empty), which is correct: restore will open the snapshot files and `FileBackend::open()` will run WAL recovery on it (finding nothing or the checkpoint, and proceeding normally).

**3. WAL coordination — why locking TransactionManager::wal is sufficient**

`TransactionManager::begin()`, `commit()`, and `rollback()` all take `self.wal.lock()`. By holding that lock during the flush + file copy, we prevent any new Begin/Commit/Rollback records from being appended while we're establishing the consistency point. The `FileBackend` WAL (used for data writes) is flushed and truncated via `Graph::flush()` before the file copy.

**4. Backup manifest — JSON via serde_json or hand-rolled**

Since `serde_json` is not in the workspace dependencies yet, the manifest will be written as a simple hand-serialized JSON-like text file using `std::fmt::Write`. This avoids adding a new dependency for a struct with 5 fields. A `BackupManifest` struct will be defined with manual `Display`/parse implementations. This is intentional: scheduled backups (deferred) will deserve a proper serde integration.

**5. Restore flow**

1. Validate manifest exists and all listed files are present (checksum verification)
2. Create or clear the target graph directory
3. Copy all files from backup to target
4. Open `Graph` normally with `Graph::open()` — WAL recovery runs automatically
5. Verify the restored graph's meta (node count, edge count) matches the manifest

**6. Error variants to add**

```rust
BackupFailed { reason: String }     // generic backup-phase failure
RestoreFailed { reason: String }    // generic restore-phase failure
ManifestCorrupt(String)             // manifest missing, parse error, or checksum mismatch
BackupAlreadyExists(PathBuf)        // target backup dir already exists (safety)
```

**7. Thread safety of BackupEngine**

`BackupEngine` receives `Arc<TransactionManager>` and `Arc<RwLock<Graph>>` (i.e. `SharedGraph`). It does NOT implement `Clone`. It IS `Send + Sync` because its fields are all `Send + Sync`. The backup operation takes `&self` because both `Arc`s provide interior mutability.

### Dependency order

```
Cycle 1: Error variants (BackupFailed, RestoreFailed, ManifestCorrupt, BackupAlreadyExists)
Cycle 2: BackupManifest struct (creation, serialization, deserialization, checksum fields)
Cycle 3: file_copy_with_checksum helper (copy a single file, compute CRC32 of bytes read)
Cycle 4: BackupEngine::new() constructor
Cycle 5: BackupEngine::create_snapshot() — flush + lock WAL + copy files + write manifest
Cycle 6: BackupEngine::verify_backup() — read manifest, re-checksum all files
Cycle 7: BackupEngine::restore() — copy files back, open Graph, verify meta counts
Cycle 8: Integration wiring — smoke test with real Graph writes → backup → restore → verify data
```

---

## Cycle 1: [ERR] — New Error Variants

**Files**:
- `crates/tessera-storage-enterprise/src/error.rs` (modify)

**Problem**: The existing `EnterpriseError` has no variants for backup/restore failures. They need to be added before anything else compiles.

### RED — Write Failing Tests

Add to `error.rs` test module:

```rust
#[test]
fn backup_failed_formats_reason() {
    let e = EnterpriseError::BackupFailed { reason: "disk full".to_owned() };
    let msg = format!("{e}");
    assert!(msg.contains("disk full"), "got: {msg}");
}

#[test]
fn restore_failed_formats_reason() {
    let e = EnterpriseError::RestoreFailed { reason: "corrupt file".to_owned() };
    let msg = format!("{e}");
    assert!(msg.contains("corrupt file"), "got: {msg}");
}

#[test]
fn manifest_corrupt_formats_detail() {
    let e = EnterpriseError::ManifestCorrupt("missing field lsn".to_owned());
    let msg = format!("{e}");
    assert!(msg.contains("missing field lsn"), "got: {msg}");
}

#[test]
fn backup_already_exists_formats_path() {
    use std::path::PathBuf;
    let e = EnterpriseError::BackupAlreadyExists(PathBuf::from("/tmp/backup"));
    let msg = format!("{e}");
    assert!(msg.contains("/tmp/backup"), "got: {msg}");
}
```

### GREEN — Minimal Correct Implementation

Add to the `EnterpriseError` enum in `error.rs`:

```rust
/// Backup operation failed.
#[error("backup failed: {reason}")]
BackupFailed { reason: String },

/// Restore operation failed.
#[error("restore failed: {reason}")]
RestoreFailed { reason: String },

/// Backup manifest is missing, unreadable, or has a checksum mismatch.
#[error("manifest corrupt: {0}")]
ManifestCorrupt(String),

/// A backup already exists at the target path and overwrite was not requested.
#[error("backup already exists at: {}", _0.display())]
BackupAlreadyExists(std::path::PathBuf),
```

### REFACTOR

None needed. The `thiserror` derive handles formatting.

**Estimated time**: 15 min

---

## Cycle 2: [MANIFEST] — BackupManifest Struct

**Files**:
- `crates/tessera-storage-enterprise/src/backup/manifest.rs` (create)
- `crates/tessera-storage-enterprise/src/backup/mod.rs` (create)
- `crates/tessera-storage-enterprise/src/lib.rs` (modify — add `pub mod backup;`)

**Problem**: The backup system needs a manifest file describing the backup contents. This cycle defines the data type, its serialization format, and its deserialization parser.

### RED — Write Failing Tests

Create `crates/tessera-storage-enterprise/src/backup/manifest.rs` with:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_roundtrip_serialize_parse() {
        let original = BackupManifest {
            created_at_unix_secs: 1_700_000_000,
            snapshot_lsn: 42,
            files: vec![
                FileEntry { name: "nodes.db".to_owned(), size_bytes: 4096, crc32: 0xDEAD_BEEF },
                FileEntry { name: "graph.meta".to_owned(), size_bytes: 4096, crc32: 0xCAFE_BABE },
            ],
        };

        let serialized = original.serialize();
        let parsed = BackupManifest::parse(&serialized).unwrap();

        assert_eq!(parsed.created_at_unix_secs, original.created_at_unix_secs);
        assert_eq!(parsed.snapshot_lsn, original.snapshot_lsn);
        assert_eq!(parsed.files.len(), 2);
        assert_eq!(parsed.files[0].name, "nodes.db");
        assert_eq!(parsed.files[0].crc32, 0xDEAD_BEEF);
        assert_eq!(parsed.files[1].name, "graph.meta");
    }

    #[test]
    fn manifest_parse_empty_files_list() {
        let m = BackupManifest {
            created_at_unix_secs: 0,
            snapshot_lsn: 1,
            files: vec![],
        };
        let s = m.serialize();
        let parsed = BackupManifest::parse(&s).unwrap();
        assert_eq!(parsed.files.len(), 0);
    }

    #[test]
    fn manifest_parse_returns_error_on_garbage() {
        let result = BackupManifest::parse("not a manifest");
        assert!(result.is_err());
    }

    #[test]
    fn manifest_file_count() {
        let m = BackupManifest {
            created_at_unix_secs: 0,
            snapshot_lsn: 0,
            files: vec![
                FileEntry { name: "a".to_owned(), size_bytes: 1, crc32: 0 },
                FileEntry { name: "b".to_owned(), size_bytes: 2, crc32: 1 },
                FileEntry { name: "c".to_owned(), size_bytes: 3, crc32: 2 },
            ],
        };
        assert_eq!(m.file_count(), 3);
    }
}
```

### GREEN — Minimal Correct Implementation

```rust
// crates/tessera-storage-enterprise/src/backup/manifest.rs

use crate::error::{EnterpriseError, Result};

/// Metadata for a single file in the backup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEntry {
    /// Filename (no path prefix).
    pub name: String,
    /// Size in bytes at the time of backup.
    pub size_bytes: u64,
    /// CRC32 checksum of the file contents.
    pub crc32: u32,
}

/// Manifest written alongside backup files.
///
/// Serialized as a simple line-based text format:
/// ```text
/// tessera_backup_v1
/// created_at=<unix_secs>
/// snapshot_lsn=<lsn>
/// files=<count>
/// <name> <size_bytes> <crc32_hex>
/// ...
/// ```
#[derive(Debug, Clone)]
pub struct BackupManifest {
    /// Unix timestamp (seconds) when the backup was created.
    pub created_at_unix_secs: u64,
    /// The WAL LSN at the consistency point.
    pub snapshot_lsn: u64,
    /// Ordered list of files in the backup.
    pub files: Vec<FileEntry>,
}

impl BackupManifest {
    /// Returns the number of files listed in the manifest.
    #[must_use]
    pub fn file_count(&self) -> usize {
        self.files.len()
    }

    /// Serializes the manifest to a UTF-8 string.
    #[must_use]
    pub fn serialize(&self) -> String {
        use std::fmt::Write as _;
        let mut out = String::new();
        writeln!(out, "tessera_backup_v1").unwrap();
        writeln!(out, "created_at={}", self.created_at_unix_secs).unwrap();
        writeln!(out, "snapshot_lsn={}", self.snapshot_lsn).unwrap();
        writeln!(out, "files={}", self.files.len()).unwrap();
        for f in &self.files {
            writeln!(out, "{} {} {:08x}", f.name, f.size_bytes, f.crc32).unwrap();
        }
        out
    }

    /// Parses a manifest from its serialized text representation.
    ///
    /// # Errors
    ///
    /// Returns [`EnterpriseError::ManifestCorrupt`] if any field is missing
    /// or cannot be parsed.
    pub fn parse(s: &str) -> Result<Self> {
        let mut lines = s.lines();

        let header = lines.next().ok_or_else(|| corrupt("missing header"))?;
        if header != "tessera_backup_v1" {
            return Err(corrupt("unknown format version"));
        }

        let created_at_unix_secs = parse_u64_field(lines.next(), "created_at")?;
        let snapshot_lsn = parse_u64_field(lines.next(), "snapshot_lsn")?;
        let file_count = parse_usize_field(lines.next(), "files")?;

        let mut files = Vec::with_capacity(file_count);
        for i in 0..file_count {
            let line = lines.next().ok_or_else(|| {
                corrupt(format!("expected file entry {i}, found end of manifest"))
            })?;
            let entry = parse_file_entry(line)?;
            files.push(entry);
        }

        Ok(Self { created_at_unix_secs, snapshot_lsn, files })
    }
}

// ── Private helpers ──────────────────────────────────────────────────

fn corrupt(msg: impl Into<String>) -> EnterpriseError {
    EnterpriseError::ManifestCorrupt(msg.into())
}

fn parse_u64_field(line: Option<&str>, key: &str) -> Result<u64> {
    let line = line.ok_or_else(|| corrupt(format!("missing field '{key}'")))?;
    let value = line
        .strip_prefix(&format!("{key}="))
        .ok_or_else(|| corrupt(format!("malformed field '{key}': got '{line}'")))?;
    value
        .parse::<u64>()
        .map_err(|_| corrupt(format!("field '{key}' is not a u64: '{value}'")))
}

fn parse_usize_field(line: Option<&str>, key: &str) -> Result<usize> {
    parse_u64_field(line, key).map(|v| v as usize)
}

fn parse_file_entry(line: &str) -> Result<FileEntry> {
    let parts: Vec<&str> = line.splitn(3, ' ').collect();
    if parts.len() != 3 {
        return Err(corrupt(format!("malformed file entry: '{line}'")));
    }
    let name = parts[0].to_owned();
    let size_bytes = parts[1]
        .parse::<u64>()
        .map_err(|_| corrupt(format!("invalid size in file entry: '{}'", parts[1])))?;
    let crc32 = u32::from_str_radix(parts[2], 16)
        .map_err(|_| corrupt(format!("invalid crc32 in file entry: '{}'", parts[2])))?;
    Ok(FileEntry { name, size_bytes, crc32 })
}
```

Create `crates/tessera-storage-enterprise/src/backup/mod.rs`:

```rust
//! Online backup and point-in-time recovery for tessera-graph-enterprise.

pub mod manifest;

pub use manifest::{BackupManifest, FileEntry};
```

Modify `crates/tessera-storage-enterprise/src/lib.rs`:

```rust
pub mod backup;
pub mod error;
pub mod txn;
```

### REFACTOR

The `parse_usize_field` cast `v as usize` is intentional for file counts that will never exceed `usize::MAX`.

**Estimated time**: 25 min

---

## Cycle 3: [COPY] — File Copy Helper with CRC32

**Files**:
- `crates/tessera-storage-enterprise/src/backup/copy.rs` (create)
- `crates/tessera-storage-enterprise/src/backup/mod.rs` (modify — add `pub(crate) mod copy;`)
- `crates/tessera-storage-enterprise/Cargo.toml` (modify — add `crc32fast`)

**Problem**: Copying files and computing their CRC32 checksum is a primitive needed by both `create_snapshot` and `verify_backup`. It must be isolated and independently tested.

### RED — Write Failing Tests

```rust
// crates/tessera-storage-enterprise/src/backup/copy.rs (test module)

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use std::io::Write as _;

    #[test]
    fn copy_file_produces_identical_bytes() {
        let src_dir = TempDir::new().unwrap();
        let dst_dir = TempDir::new().unwrap();

        let src_path = src_dir.path().join("data.bin");
        std::fs::write(&src_path, b"hello tessera").unwrap();

        let entry = copy_file_with_checksum(&src_path, dst_dir.path(), "data.bin").unwrap();

        let dst_path = dst_dir.path().join("data.bin");
        let dst_bytes = std::fs::read(&dst_path).unwrap();
        assert_eq!(dst_bytes, b"hello tessera");
        assert_eq!(entry.name, "data.bin");
        assert_eq!(entry.size_bytes, 13);
    }

    #[test]
    fn crc32_checksum_is_deterministic() {
        let src_dir = TempDir::new().unwrap();
        let dst_dir1 = TempDir::new().unwrap();
        let dst_dir2 = TempDir::new().unwrap();

        let src_path = src_dir.path().join("f.bin");
        std::fs::write(&src_path, b"deterministic").unwrap();

        let e1 = copy_file_with_checksum(&src_path, dst_dir1.path(), "f.bin").unwrap();
        let e2 = copy_file_with_checksum(&src_path, dst_dir2.path(), "f.bin").unwrap();
        assert_eq!(e1.crc32, e2.crc32);
        assert_ne!(e1.crc32, 0); // CRC of non-empty content is non-zero (probabilistically)
    }

    #[test]
    fn empty_file_copy_produces_zero_size_entry() {
        let src_dir = TempDir::new().unwrap();
        let dst_dir = TempDir::new().unwrap();
        let src_path = src_dir.path().join("empty.db");
        std::fs::write(&src_path, b"").unwrap();

        let entry = copy_file_with_checksum(&src_path, dst_dir.path(), "empty.db").unwrap();
        assert_eq!(entry.size_bytes, 0);
    }

    #[test]
    fn checksum_file_matches_crc32_of_contents() {
        let src_dir = TempDir::new().unwrap();
        let dst_dir = TempDir::new().unwrap();
        let content = b"checksum_test_data";
        let src_path = src_dir.path().join("c.bin");
        std::fs::write(&src_path, content).unwrap();

        let entry = copy_file_with_checksum(&src_path, dst_dir.path(), "c.bin").unwrap();

        let expected_crc = crc32fast::hash(content);
        assert_eq!(entry.crc32, expected_crc);
    }

    #[test]
    fn copy_nonexistent_file_returns_error() {
        let dst_dir = TempDir::new().unwrap();
        let result = copy_file_with_checksum(
            std::path::Path::new("/nonexistent/path/ghost.db"),
            dst_dir.path(),
            "ghost.db",
        );
        assert!(result.is_err());
    }
}
```

### GREEN — Minimal Correct Implementation

Add `crc32fast = "1"` to:
- `[workspace.dependencies]` in the root `Cargo.toml`
- `[dependencies]` in `crates/tessera-storage-enterprise/Cargo.toml`

Note: `tessera-graph` already uses `crc32fast` (it's in the WAL record codec). The workspace just needs to expose it.

```rust
// crates/tessera-storage-enterprise/src/backup/copy.rs

use std::io::Read as _;
use std::path::Path;

use crate::backup::manifest::FileEntry;
use crate::error::Result;

/// Copies a single file from `src_file_path` to `<dst_dir>/<name>`,
/// computing the CRC32 checksum of the bytes read.
///
/// Returns a [`FileEntry`] with the file's name, size, and checksum.
///
/// # Errors
///
/// Returns an I/O error if the source file cannot be read or the
/// destination file cannot be written.
pub(crate) fn copy_file_with_checksum(
    src_file_path: &Path,
    dst_dir: &Path,
    name: &str,
) -> Result<FileEntry> {
    let bytes = std::fs::read(src_file_path)?;
    let crc32 = crc32fast::hash(&bytes);
    let size_bytes = bytes.len() as u64;

    let dst_path = dst_dir.join(name);
    std::fs::write(&dst_path, &bytes)?;

    Ok(FileEntry { name: name.to_owned(), size_bytes, crc32 })
}

/// Computes the CRC32 of a file's contents without copying it.
///
/// Used during verification to compare against the stored checksum.
///
/// # Errors
///
/// Returns an I/O error if the file cannot be read.
pub(crate) fn checksum_file(path: &Path) -> Result<u32> {
    let bytes = std::fs::read(path)?;
    Ok(crc32fast::hash(&bytes))
}
```

### REFACTOR

`std::fs::read` reads the whole file into memory. For large graphs this is unavoidable when computing CRC32 in a single pass. Future improvement: streaming CRC32 with a chunked reader. Not needed for v1.

**Estimated time**: 20 min

---

## Cycle 4: [ENGINE-NEW] — BackupEngine Constructor

**Files**:
- `crates/tessera-storage-enterprise/src/backup/engine.rs` (create)
- `crates/tessera-storage-enterprise/src/backup/mod.rs` (modify — add `pub mod engine; pub use engine::BackupEngine;`)

**Problem**: `BackupEngine` needs to hold references to both `SharedGraph` and `TransactionManager` (both behind `Arc`). The constructor must validate that the graph path exists.

### RED — Write Failing Tests

```rust
// crates/tessera-storage-enterprise/src/backup/engine.rs (test module)

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tempfile::TempDir;
    use tessera_graph::{Graph, GraphConfig, SharedGraph};
    use crate::txn::manager::TransactionManager;

    fn setup_engine(dir: &TempDir) -> BackupEngine {
        let graph_dir = dir.path().join("graph");
        let wal_path = dir.path().join("enterprise.wal");
        let graph = Graph::open(&graph_dir, &GraphConfig::new()).unwrap();
        let shared = SharedGraph::new(graph);
        let txn_mgr = Arc::new(TransactionManager::open(&wal_path).unwrap());
        BackupEngine::new(shared, txn_mgr, graph_dir)
    }

    #[test]
    fn new_returns_engine() {
        let dir = TempDir::new().unwrap();
        let _engine = setup_engine(&dir);
    }

    #[test]
    fn engine_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<BackupEngine>();
    }
}
```

### GREEN — Minimal Correct Implementation

```rust
// crates/tessera-storage-enterprise/src/backup/engine.rs

use std::path::PathBuf;
use std::sync::Arc;

use tessera_graph::SharedGraph;

use crate::txn::manager::TransactionManager;

/// Coordinates online backup and restore operations.
///
/// Holds `Arc` references to the shared graph and the transaction manager
/// so that backup can be initiated from a separate coordination thread
/// without taking ownership of either.
pub struct BackupEngine {
    graph: SharedGraph,
    txn_mgr: Arc<TransactionManager>,
    /// Absolute path to the graph's data directory.
    graph_dir: PathBuf,
}

impl BackupEngine {
    /// Creates a new `BackupEngine`.
    ///
    /// `graph_dir` must be the directory where the graph stores its data files
    /// (`nodes.db`, `edges.db`, etc.). This path is not derivable from
    /// `SharedGraph` directly because `FileBackend::dir` is private.
    #[must_use]
    pub fn new(
        graph: SharedGraph,
        txn_mgr: Arc<TransactionManager>,
        graph_dir: impl Into<PathBuf>,
    ) -> Self {
        Self {
            graph,
            txn_mgr,
            graph_dir: graph_dir.into(),
        }
    }
}
```

### REFACTOR

None.

**Estimated time**: 15 min

---

## Cycle 5: [SNAPSHOT] — BackupEngine::create_snapshot()

**Files**:
- `crates/tessera-storage-enterprise/src/backup/engine.rs` (modify)

**Problem**: This is the core operation. It must flush the graph, copy all data files, and write the manifest — all while holding the WAL lock to prevent new enterprise WAL records from being appended.

### RED — Write Failing Tests

```rust
// In engine.rs test module — add to existing tests

#[test]
fn create_snapshot_produces_backup_directory() {
    let dir = TempDir::new().unwrap();
    let engine = setup_engine(&dir);

    let backup_dir = dir.path().join("backup_001");
    engine.create_snapshot(&backup_dir).unwrap();

    assert!(backup_dir.exists());
    assert!(backup_dir.join("manifest.txt").exists());
    assert!(backup_dir.join("nodes.db").exists());
    assert!(backup_dir.join("edges.db").exists());
    assert!(backup_dir.join("adjacency.db").exists());
    assert!(backup_dir.join("strings.db").exists());
    assert!(backup_dir.join("overflow.db").exists());
    assert!(backup_dir.join("graph.meta").exists());
    // wal.log may or may not exist (it's truncated after flush),
    // but create_snapshot must copy it regardless
    assert!(backup_dir.join("wal.log").exists());
}

#[test]
fn create_snapshot_manifest_lists_all_files() {
    use crate::backup::manifest::BackupManifest;

    let dir = TempDir::new().unwrap();
    let engine = setup_engine(&dir);

    let backup_dir = dir.path().join("backup_002");
    engine.create_snapshot(&backup_dir).unwrap();

    let manifest_txt = std::fs::read_to_string(backup_dir.join("manifest.txt")).unwrap();
    let manifest = BackupManifest::parse(&manifest_txt).unwrap();

    let names: Vec<&str> = manifest.files.iter().map(|f| f.name.as_str()).collect();
    assert!(names.contains(&"nodes.db"));
    assert!(names.contains(&"edges.db"));
    assert!(names.contains(&"adjacency.db"));
    assert!(names.contains(&"strings.db"));
    assert!(names.contains(&"overflow.db"));
    assert!(names.contains(&"graph.meta"));
    assert!(names.contains(&"wal.log"));
}

#[test]
fn create_snapshot_fails_if_backup_dir_already_exists() {
    use crate::error::EnterpriseError;

    let dir = TempDir::new().unwrap();
    let engine = setup_engine(&dir);

    let backup_dir = dir.path().join("backup_exist");
    std::fs::create_dir_all(&backup_dir).unwrap();

    let result = engine.create_snapshot(&backup_dir);
    assert!(matches!(result, Err(EnterpriseError::BackupAlreadyExists(_))));
}

#[test]
fn create_snapshot_with_data_preserves_file_contents() {
    use tessera_graph::props;

    let dir = TempDir::new().unwrap();
    let graph_dir = dir.path().join("graph");
    let wal_path = dir.path().join("enterprise.wal");
    let graph = Graph::open(&graph_dir, &GraphConfig::new()).unwrap();
    let shared = SharedGraph::new(graph);
    let txn_mgr = Arc::new(TransactionManager::open(&wal_path).unwrap());

    // Write some data before backup
    {
        let mut g = shared.write();
        g.add_node("Person", props! { "name" => "Alice" }).unwrap();
        g.add_node("Person", props! { "name" => "Bob" }).unwrap();
    }

    let engine = BackupEngine::new(shared, txn_mgr, graph_dir);
    let backup_dir = dir.path().join("backup_data");
    engine.create_snapshot(&backup_dir).unwrap();

    // The backup's nodes.db must be non-empty (data was written)
    let nodes_size = std::fs::metadata(backup_dir.join("nodes.db")).unwrap().len();
    assert!(nodes_size > 0, "nodes.db must not be empty after writing data");
}
```

### GREEN — Minimal Correct Implementation

Add to `engine.rs`, method on `BackupEngine`:

```rust
use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::backup::copy::copy_file_with_checksum;
use crate::backup::manifest::{BackupManifest, FileEntry};
use crate::error::{EnterpriseError, Result};

impl BackupEngine {
    /// Creates an online backup snapshot at `backup_dir`.
    ///
    /// # Procedure
    ///
    /// 1. Reject if `backup_dir` already exists (safety guard).
    /// 2. Lock `TransactionManager::wal` to freeze enterprise WAL writes.
    /// 3. Flush and checkpoint the graph (dirty pages → disk, WAL truncated).
    /// 4. Copy all data files while the WAL lock is held.
    /// 5. Write the manifest.
    /// 6. Release WAL lock.
    ///
    /// Operations continue normally after step 6. The freeze window is the
    /// duration of the flush + file copy (typically milliseconds for small
    /// graphs, seconds for large ones).
    ///
    /// # Errors
    ///
    /// Returns [`EnterpriseError::BackupAlreadyExists`] if `backup_dir` exists.
    /// Returns [`EnterpriseError::BackupFailed`] if any step fails.
    /// Returns I/O errors from file operations.
    pub fn create_snapshot(&self, backup_dir: &Path) -> Result<()> {
        // 1. Safety: refuse to overwrite an existing backup
        if backup_dir.exists() {
            return Err(EnterpriseError::BackupAlreadyExists(backup_dir.to_path_buf()));
        }
        fs::create_dir_all(backup_dir)?;

        // 2. Lock the enterprise WAL — freezes Begin/Commit/Rollback records
        let wal_guard = self
            .txn_mgr
            .wal
            .lock()
            .map_err(|_| EnterpriseError::BackupFailed {
                reason: "enterprise WAL lock is poisoned".to_owned(),
            })?;

        // 3. Flush the graph: dirty pages → disk, FileBackend WAL checkpointed
        //    and truncated. This establishes the consistency point.
        {
            let mut g = self.graph.write();
            g.flush().map_err(|e| EnterpriseError::BackupFailed {
                reason: format!("graph flush failed: {e}"),
            })?;
        }

        // Capture the WAL's next_lsn before releasing the lock.
        // After flush, the FileBackend WAL is truncated. The enterprise WAL
        // still tracks its LSN monotonically.
        let snapshot_lsn = wal_guard.next_lsn();

        // 4. Copy data files while WAL lock is held (no new records can be
        //    stamped; in-flight graph mutations are also blocked by the
        //    SharedGraph write lock which we already released — but since
        //    FileBackend WAL is truncated and pages are flushed, the files
        //    are stable on disk right now).
        let data_files = [
            "nodes.db",
            "edges.db",
            "adjacency.db",
            "strings.db",
            "overflow.db",
            "graph.meta",
        ];

        let mut file_entries: Vec<FileEntry> = Vec::with_capacity(data_files.len() + 2);

        for name in &data_files {
            let src = self.graph_dir.join(name);
            let entry = copy_file_with_checksum(&src, backup_dir, name)?;
            file_entries.push(entry);
        }

        // Copy WAL tail (will be empty or contain only a checkpoint record
        // since we just flushed, but PITR requires it).
        let wal_src = self.graph_dir.join("wal.log");
        if !wal_src.exists() {
            // Create empty WAL in backup so restore can open normally
            fs::write(backup_dir.join("wal.log"), b"")?;
            file_entries.push(FileEntry {
                name: "wal.log".to_owned(),
                size_bytes: 0,
                crc32: crc32fast::hash(b""),
            });
        } else {
            let entry = copy_file_with_checksum(&wal_src, backup_dir, "wal.log")?;
            file_entries.push(entry);
        }

        // Copy label index (optional — may not exist on new stores)
        let index_src = self.graph_dir.join("index.bin");
        if index_src.exists() {
            let entry = copy_file_with_checksum(&index_src, backup_dir, "index.bin")?;
            file_entries.push(entry);
        }

        // WAL lock can be released now — all files are copied
        drop(wal_guard);

        // 5. Write manifest
        let created_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        let manifest = BackupManifest {
            created_at_unix_secs: created_at,
            snapshot_lsn,
            files: file_entries,
        };

        let manifest_str = manifest.serialize();
        fs::write(backup_dir.join("manifest.txt"), manifest_str.as_bytes())?;

        Ok(())
    }
}
```

Note on `next_lsn()`: `WalWriter::next_lsn()` is currently `#[cfg(test)]`. This must be promoted to always-public, or we must add a production-visible accessor. The cleanest fix is removing the `#[cfg(test)]` attribute from `next_lsn()` in `tessera-graph/src/wal/writer.rs`.

**This is a required change to tessera-graph (MIT crate)**: Remove `#[cfg(test)]` from `WalWriter::next_lsn()`. This is a pure non-breaking addition.

### REFACTOR

The WAL lock window encompasses both the `graph.write()` flush AND the file copy. An alternative is to release the write lock on the graph first, then copy. But since the FileBackend's data files are on disk and the write lock was dropped before the copy, concurrent mutations after the flush would modify in-memory dirty pages only (not disk), so the on-disk state we copy is the post-flush snapshot. This is actually safe. The REFACTOR step can document this in a comment.

**Estimated time**: 40 min

---

## Cycle 6: [VERIFY] — BackupEngine::verify_backup()

**Files**:
- `crates/tessera-storage-enterprise/src/backup/engine.rs` (modify)

**Problem**: After creating a backup (or before restoring), we need to verify that all files listed in the manifest are present and their CRC32 checksums match.

### RED — Write Failing Tests

```rust
// In engine.rs test module — add

#[test]
fn verify_backup_succeeds_on_valid_backup() {
    let dir = TempDir::new().unwrap();
    let engine = setup_engine(&dir);
    let backup_dir = dir.path().join("bk_verify_ok");
    engine.create_snapshot(&backup_dir).unwrap();

    // Must not return an error
    engine.verify_backup(&backup_dir).unwrap();
}

#[test]
fn verify_backup_detects_corrupted_file() {
    use crate::error::EnterpriseError;

    let dir = TempDir::new().unwrap();
    let engine = setup_engine(&dir);
    let backup_dir = dir.path().join("bk_verify_corrupt");
    engine.create_snapshot(&backup_dir).unwrap();

    // Corrupt nodes.db after backup
    let nodes_path = backup_dir.join("nodes.db");
    let mut bytes = std::fs::read(&nodes_path).unwrap();
    if bytes.is_empty() {
        bytes.push(0xFF);
    } else {
        bytes[0] ^= 0xFF;
    }
    std::fs::write(&nodes_path, &bytes).unwrap();

    let result = engine.verify_backup(&backup_dir);
    assert!(
        matches!(result, Err(EnterpriseError::ManifestCorrupt(_))),
        "expected ManifestCorrupt, got: {result:?}"
    );
}

#[test]
fn verify_backup_detects_missing_manifest() {
    use crate::error::EnterpriseError;

    let dir = TempDir::new().unwrap();
    let engine = setup_engine(&dir);
    let backup_dir = dir.path().join("bk_no_manifest");
    std::fs::create_dir_all(&backup_dir).unwrap();
    // No manifest.txt

    let result = engine.verify_backup(&backup_dir);
    assert!(matches!(result, Err(EnterpriseError::ManifestCorrupt(_))));
}

#[test]
fn verify_backup_detects_missing_data_file() {
    use crate::error::EnterpriseError;

    let dir = TempDir::new().unwrap();
    let engine = setup_engine(&dir);
    let backup_dir = dir.path().join("bk_missing_file");
    engine.create_snapshot(&backup_dir).unwrap();

    // Remove a file after backup
    std::fs::remove_file(backup_dir.join("edges.db")).unwrap();

    let result = engine.verify_backup(&backup_dir);
    assert!(matches!(result, Err(EnterpriseError::ManifestCorrupt(_))));
}
```

### GREEN — Minimal Correct Implementation

```rust
impl BackupEngine {
    /// Verifies the integrity of an existing backup at `backup_dir`.
    ///
    /// Reads the manifest, then re-computes the CRC32 of every listed file
    /// and compares it against the stored checksum.
    ///
    /// # Errors
    ///
    /// Returns [`EnterpriseError::ManifestCorrupt`] if the manifest is missing,
    /// unparseable, or any file's checksum does not match.
    pub fn verify_backup(&self, backup_dir: &Path) -> Result<()> {
        use crate::backup::copy::checksum_file;

        let manifest_path = backup_dir.join("manifest.txt");
        let manifest_txt = std::fs::read_to_string(&manifest_path).map_err(|e| {
            EnterpriseError::ManifestCorrupt(format!("cannot read manifest: {e}"))
        })?;
        let manifest = BackupManifest::parse(&manifest_txt)?;

        for entry in &manifest.files {
            let file_path = backup_dir.join(&entry.name);
            if !file_path.exists() {
                return Err(EnterpriseError::ManifestCorrupt(format!(
                    "listed file '{}' is missing from backup",
                    entry.name
                )));
            }
            let actual_crc = checksum_file(&file_path)?;
            if actual_crc != entry.crc32 {
                return Err(EnterpriseError::ManifestCorrupt(format!(
                    "checksum mismatch for '{}': manifest={:08x} actual={:08x}",
                    entry.name, entry.crc32, actual_crc
                )));
            }
        }

        Ok(())
    }
}
```

### REFACTOR

None. The logic is simple enough.

**Estimated time**: 20 min

---

## Cycle 7: [RESTORE] — BackupEngine::restore()

**Files**:
- `crates/tessera-storage-enterprise/src/backup/engine.rs` (modify)

**Problem**: Restore copies the backup files to a target directory and opens the graph via `Graph::open()`, which triggers WAL recovery automatically. The restored graph's metadata is validated against the manifest's expected state.

### RED — Write Failing Tests

```rust
// In engine.rs test module — add

#[test]
fn restore_creates_openable_graph() {
    use tessera_graph::{GraphConfig, Graph};

    let dir = TempDir::new().unwrap();
    let engine = setup_engine(&dir);

    // Snapshot
    let backup_dir = dir.path().join("bk_restore_ok");
    engine.create_snapshot(&backup_dir).unwrap();

    // Restore to a different location
    let restore_dir = dir.path().join("restored");
    engine.restore(&backup_dir, &restore_dir).unwrap();

    // Can open the restored graph
    let graph = Graph::open(&restore_dir, &GraphConfig::new()).unwrap();
    // Node count is 0 for a fresh graph (no data was written before backup)
    assert_eq!(graph.node_count(), 0);
}

#[test]
fn restore_preserves_graph_data() {
    use tessera_graph::{props, GraphConfig, Graph};

    let dir = TempDir::new().unwrap();
    let graph_dir = dir.path().join("graph");
    let wal_path = dir.path().join("ent.wal");
    let graph = Graph::open(&graph_dir, &GraphConfig::new()).unwrap();
    let shared = SharedGraph::new(graph);
    let txn_mgr = Arc::new(TransactionManager::open(&wal_path).unwrap());

    {
        let mut g = shared.write();
        g.add_node("City", props! { "name" => "Tallinn" }).unwrap();
        g.add_node("City", props! { "name" => "Berlin" }).unwrap();
        g.add_node("City", props! { "name" => "Tokyo" }).unwrap();
    }

    let engine = BackupEngine::new(shared, txn_mgr, &graph_dir);
    let backup_dir = dir.path().join("bk_data");
    engine.create_snapshot(&backup_dir).unwrap();

    let restore_dir = dir.path().join("restored_data");
    engine.restore(&backup_dir, &restore_dir).unwrap();

    let restored = Graph::open(&restore_dir, &GraphConfig::new()).unwrap();
    assert_eq!(restored.node_count(), 3, "all 3 nodes must survive restore");
}

#[test]
fn restore_fails_if_backup_dir_missing() {
    use crate::error::EnterpriseError;

    let dir = TempDir::new().unwrap();
    let engine = setup_engine(&dir);

    let result = engine.restore(
        std::path::Path::new("/nonexistent/backup"),
        &dir.path().join("restored"),
    );
    assert!(matches!(result, Err(EnterpriseError::RestoreFailed { .. })));
}

#[test]
fn restore_fails_if_target_dir_already_exists() {
    use crate::error::EnterpriseError;

    let dir = TempDir::new().unwrap();
    let engine = setup_engine(&dir);

    let backup_dir = dir.path().join("bk_target_exist");
    engine.create_snapshot(&backup_dir).unwrap();

    let restore_dir = dir.path().join("already_exists");
    std::fs::create_dir_all(&restore_dir).unwrap();

    let result = engine.restore(&backup_dir, &restore_dir);
    assert!(matches!(result, Err(EnterpriseError::RestoreFailed { .. })));
}
```

### GREEN — Minimal Correct Implementation

```rust
impl BackupEngine {
    /// Restores a backup from `backup_dir` to `restore_dir`.
    ///
    /// # Procedure
    ///
    /// 1. Verify `backup_dir` exists and contains a valid manifest.
    /// 2. Reject if `restore_dir` already exists (safety guard).
    /// 3. Create `restore_dir` and copy all manifest-listed files into it.
    /// 4. Verify checksums of copied files.
    ///
    /// After `restore()` returns, call `Graph::open(restore_dir, &GraphConfig::new())`
    /// to open the restored graph. WAL recovery runs automatically at open time.
    ///
    /// # Errors
    ///
    /// Returns [`EnterpriseError::RestoreFailed`] if `backup_dir` does not exist,
    /// if `restore_dir` already exists, or if the manifest is invalid.
    /// Returns I/O errors from file operations.
    pub fn restore(&self, backup_dir: &Path, restore_dir: &Path) -> Result<()> {
        // 1. Validate backup source
        if !backup_dir.exists() {
            return Err(EnterpriseError::RestoreFailed {
                reason: format!("backup directory does not exist: {}", backup_dir.display()),
            });
        }

        let manifest_path = backup_dir.join("manifest.txt");
        let manifest_txt = std::fs::read_to_string(&manifest_path).map_err(|e| {
            EnterpriseError::RestoreFailed {
                reason: format!("cannot read manifest: {e}"),
            }
        })?;
        let manifest = BackupManifest::parse(&manifest_txt).map_err(|e| {
            EnterpriseError::RestoreFailed { reason: format!("manifest parse error: {e}") }
        })?;

        // 2. Safety: refuse to overwrite existing target
        if restore_dir.exists() {
            return Err(EnterpriseError::RestoreFailed {
                reason: format!("restore target already exists: {}", restore_dir.display()),
            });
        }
        fs::create_dir_all(restore_dir)?;

        // 3. Copy all manifest-listed files
        for entry in &manifest.files {
            let src = backup_dir.join(&entry.name);
            fs::copy(&src, restore_dir.join(&entry.name)).map_err(|e| {
                EnterpriseError::RestoreFailed {
                    reason: format!("failed to copy '{}': {e}", entry.name),
                }
            })?;
        }

        // 4. Verify checksums in restore target
        use crate::backup::copy::checksum_file;
        for entry in &manifest.files {
            let dst = restore_dir.join(&entry.name);
            let actual_crc = checksum_file(&dst)?;
            if actual_crc != entry.crc32 {
                return Err(EnterpriseError::RestoreFailed {
                    reason: format!(
                        "checksum mismatch after copy for '{}': expected {:08x} got {:08x}",
                        entry.name, entry.crc32, actual_crc
                    ),
                });
            }
        }

        Ok(())
    }
}
```

### REFACTOR

None.

**Estimated time**: 30 min

---

## Cycle 8: [WIRE] — Integration Smoke Test + Final Wiring

**Files**:
- `crates/tessera-storage-enterprise/tests/backup_integration.rs` (create)

**Problem**: Unit tests verify each component in isolation. This integration test exercises the full round-trip: write data → snapshot → restore → verify data survived. It also tests the backup in the context of an active `TransactionManager`.

### RED — Write Failing Tests (integration test file)

```rust
// crates/tessera-storage-enterprise/tests/backup_integration.rs

use std::sync::Arc;
use tempfile::TempDir;
use tessera_graph::{Graph, GraphConfig, SharedGraph, props};
use tessera_storage_enterprise::backup::engine::BackupEngine;
use tessera_storage_enterprise::txn::{
    handle::IsolationLevel, manager::TransactionManager,
};

/// Full round-trip: write nodes and edges, create snapshot,
/// restore to a new directory, verify node and edge counts.
#[test]
fn full_backup_restore_roundtrip() {
    let dir = TempDir::new().unwrap();
    let graph_dir = dir.path().join("live");
    let wal_path = dir.path().join("enterprise.wal");

    // ── Setup ──────────────────────────────────────────────────────
    let graph = Graph::open(&graph_dir, &GraphConfig::new()).unwrap();
    let shared = SharedGraph::new(graph);
    let txn_mgr = Arc::new(TransactionManager::open(&wal_path).unwrap());

    // Write data under a transaction
    let mut txn = txn_mgr.begin(IsolationLevel::ReadCommitted).unwrap();
    {
        let mut g = shared.write();
        let alice = g.add_node("Person", props! { "name" => "Alice" }).unwrap();
        let bob   = g.add_node("Person", props! { "name" => "Bob" }).unwrap();
        g.add_edge("KNOWS", alice, bob, props! {}).unwrap();
    }
    txn_mgr.commit(&mut txn).unwrap();

    // ── Backup ─────────────────────────────────────────────────────
    let engine = BackupEngine::new(
        shared.clone(),
        Arc::clone(&txn_mgr),
        graph_dir.clone(),
    );
    let backup_dir = dir.path().join("snapshot_001");
    engine.create_snapshot(&backup_dir).unwrap();

    // ── Verify backup integrity ─────────────────────────────────────
    engine.verify_backup(&backup_dir).unwrap();

    // ── Restore ────────────────────────────────────────────────────
    let restore_dir = dir.path().join("restored");
    engine.restore(&backup_dir, &restore_dir).unwrap();

    // ── Open restored graph and assert data ────────────────────────
    let restored = Graph::open(&restore_dir, &GraphConfig::new()).unwrap();
    assert_eq!(restored.node_count(), 2, "2 nodes must survive restore");
    assert_eq!(restored.edge_count(), 1, "1 edge must survive restore");
}

/// Backup created while a concurrent transaction is mid-flight.
/// The snapshot consistency point must not include the uncommitted transaction's
/// writes (because flush happens before WAL records from the in-flight txn are
/// written to the FileBackend WAL).
#[test]
fn backup_excludes_uncommitted_txn_data() {
    let dir = TempDir::new().unwrap();
    let graph_dir = dir.path().join("live");
    let wal_path = dir.path().join("enterprise.wal");

    let graph = Graph::open(&graph_dir, &GraphConfig::new()).unwrap();
    let shared = SharedGraph::new(graph);
    let txn_mgr = Arc::new(TransactionManager::open(&wal_path).unwrap());

    // Commit T1 with 1 node
    let mut t1 = txn_mgr.begin(IsolationLevel::ReadCommitted).unwrap();
    {
        let mut g = shared.write();
        g.add_node("Committed", props! {}).unwrap();
    }
    txn_mgr.commit(&mut t1).unwrap();

    // Begin T2 but do NOT commit
    let _t2 = txn_mgr.begin(IsolationLevel::ReadCommitted).unwrap();
    // (no write under t2 — BackupEngine acquires WAL lock, then graph lock)

    let engine = BackupEngine::new(shared, Arc::clone(&txn_mgr), &graph_dir);
    let backup_dir = dir.path().join("bk_concurrent");
    engine.create_snapshot(&backup_dir).unwrap();

    let restore_dir = dir.path().join("restored_concurrent");
    engine.restore(&backup_dir, &restore_dir).unwrap();

    let restored = Graph::open(&restore_dir, &GraphConfig::new()).unwrap();
    // Only T1's node should be present
    assert_eq!(restored.node_count(), 1);
}

/// Back-to-back snapshots produce independent, consistent backups.
#[test]
fn consecutive_snapshots_are_independent() {
    let dir = TempDir::new().unwrap();
    let graph_dir = dir.path().join("live");
    let wal_path = dir.path().join("enterprise.wal");

    let graph = Graph::open(&graph_dir, &GraphConfig::new()).unwrap();
    let shared = SharedGraph::new(graph);
    let txn_mgr = Arc::new(TransactionManager::open(&wal_path).unwrap());

    let engine = BackupEngine::new(shared.clone(), Arc::clone(&txn_mgr), &graph_dir);

    // Snapshot 1: empty graph
    let bk1 = dir.path().join("bk1");
    engine.create_snapshot(&bk1).unwrap();

    // Write a node after snapshot 1
    {
        let mut g = shared.write();
        g.add_node("Thing", props! {}).unwrap();
    }

    // Snapshot 2: graph has 1 node
    let bk2 = dir.path().join("bk2");
    engine.create_snapshot(&bk2).unwrap();

    // Restore snapshot 1 → 0 nodes
    let r1 = dir.path().join("restored1");
    engine.restore(&bk1, &r1).unwrap();
    let g1 = Graph::open(&r1, &GraphConfig::new()).unwrap();
    assert_eq!(g1.node_count(), 0);

    // Restore snapshot 2 → 1 node
    let r2 = dir.path().join("restored2");
    engine.restore(&bk2, &r2).unwrap();
    let g2 = Graph::open(&r2, &GraphConfig::new()).unwrap();
    assert_eq!(g2.node_count(), 1);
}
```

### GREEN — Verify all prior cycles pass

No new implementation code. If all cycles 1–7 are correct, this integration test passes.

### REFACTOR

Review the `create_snapshot` WAL lock window comment to ensure it accurately reflects what happens with `SharedGraph::write()` inside the locked section, and whether that creates deadlock risk (it does not: the WAL lock and the graph write lock are independent Mutex/RwLock primitives and are always acquired in the same order by `BackupEngine`).

**Estimated time**: 25 min

---

## Required Change to tessera-graph (MIT crate)

Before cycle 5 can compile, one change is required in the MIT crate:

**File**: `/Volumes/WD_BLACK/repos/MojoBytes/tessera-ecosystem/tessera-graph/src/wal/writer.rs`

**Change**: Remove `#[cfg(test)]` from `WalWriter::next_lsn()`.

```rust
// Before:
#[must_use]
#[cfg(test)]
pub const fn next_lsn(&self) -> u64 {
    self.next_lsn
}

// After:
#[must_use]
pub const fn next_lsn(&self) -> u64 {
    self.next_lsn
}
```

This is a pure additive change: no existing test is broken, no behavior changes.

---

## Cargo.toml Changes Summary

**Root `Cargo.toml` — `[workspace.dependencies]`**: Add `crc32fast = "1"`.

Note: `crc32fast` is already in tessera-graph's `Cargo.toml` as a direct dependency (used by `wal/record.rs`). Adding it to the workspace level simply lets `tessera-storage-enterprise` declare it via `workspace = true`.

**`crates/tessera-storage-enterprise/Cargo.toml` — `[dependencies]`**: Add `crc32fast.workspace = true`.

**`crates/tessera-storage-enterprise/Cargo.toml` — `[dev-dependencies]`**: Already has `tempfile = "3"`. No additions needed since integration tests use the same `tempfile`.

---

## Final Module Structure

```
crates/tessera-storage-enterprise/src/
    lib.rs                  ← add `pub mod backup;`
    error.rs                ← add 4 new variants
    backup/
        mod.rs              ← re-exports BackupEngine, BackupManifest, FileEntry
        manifest.rs         ← BackupManifest, FileEntry, parse/serialize
        copy.rs             ← copy_file_with_checksum, checksum_file (pub(crate))
        engine.rs           ← BackupEngine: new, create_snapshot, verify_backup, restore
    txn/
        mod.rs
        handle.rs
        manager.rs
        snapshot.rs

crates/tessera-storage-enterprise/tests/
    backup_integration.rs   ← full round-trip integration tests
```

---

## Estimation

| Phase | Estimated Time |
|-------|---------------|
| Cycle 1: Error variants | 15 min |
| Cycle 2: BackupManifest | 25 min |
| Cycle 3: copy helper | 20 min |
| Cycle 4: BackupEngine::new | 15 min |
| Cycle 5: create_snapshot | 40 min |
| Cycle 6: verify_backup | 20 min |
| Cycle 7: restore | 30 min |
| Cycle 8: Integration wiring | 25 min |
| tessera-graph change + Cargo edits | 10 min |
| **Total** | **~3.5 hours** |

---

## Criteria de Exito

- [ ] `cargo clippy -p tessera-storage-enterprise` returns zero errors and zero warnings
- [ ] `cargo test -p tessera-storage-enterprise` passes all unit tests
- [ ] Integration test `full_backup_restore_roundtrip` passes
- [ ] Integration test `backup_excludes_uncommitted_txn_data` passes
- [ ] Integration test `consecutive_snapshots_are_independent` passes
- [ ] `backup_dir` is never mutated after `create_snapshot` completes (immutable backup invariant)
- [ ] Restoring a backup produces a directory that `Graph::open()` accepts without error
- [ ] Backup created while the graph is empty produces a valid, restorable snapshot
- [ ] No `unsafe_code` introduced (workspace `forbid` enforced)

---

## Known Limitations (v1)

These are intentional deferments, not oversights:

1. **Streaming CRC32**: `copy_file_with_checksum` reads entire files into RAM. For graphs in the tens-of-GB range, this is a memory spike. Fix: chunked reads with incremental CRC32.
2. **Scheduled backups**: Deferred to when `tessera-config` is implemented (per roadmap).
3. **Incremental backups**: v1 is always a full snapshot. Incremental (WAL-based) is a future milestone.
4. **Backup encryption**: Deferred to enterprise security features.
5. **`index.bin` optional**: If `index.bin` does not exist at backup time, it is not included. `Graph::open()` will rebuild the label index from pages on restore. This is correct behavior.
6. **WAL lock window includes graph flush**: For large graphs with many dirty pages, the freeze window could be seconds. The production fix is a two-phase approach (flush without lock, then lock briefly to copy already-flushed files). Deferred to v1.1.
