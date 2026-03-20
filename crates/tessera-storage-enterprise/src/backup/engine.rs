use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use tessera_graph::SharedGraph;

use crate::backup::copy::{checksum_file, copy_file_with_checksum};
use crate::backup::manifest::{BackupManifest, FileEntry};
use crate::error::{EnterpriseError, Result};
use crate::txn::TransactionManager;

/// File names of the graph's data files that are always copied.
const DATA_FILES: [&str; 6] = [
    "nodes.db",
    "edges.db",
    "adjacency.db",
    "strings.db",
    "overflow.db",
    "graph.meta",
];

/// Manifest filename within the backup directory.
const MANIFEST_NAME: &str = "manifest.txt";

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

    /// Creates an online backup snapshot at `backup_dir`.
    ///
    /// # Procedure
    ///
    /// 1. Reject if `backup_dir` already exists (safety guard).
    /// 2. Lock `TransactionManager::wal` to freeze enterprise WAL writes.
    /// 3. Flush and checkpoint the graph (dirty pages → disk, WAL truncated).
    /// 4. Copy all data files while the WAL lock is held.
    /// 5. Write the manifest.
    /// 6. Release WAL lock — operations resume.
    ///
    /// The freeze window is the duration of flush + file copy (typically
    /// milliseconds for small graphs, seconds for large ones).
    ///
    /// # Errors
    ///
    /// Returns [`EnterpriseError::BackupAlreadyExists`] if `backup_dir` exists.
    /// Returns [`EnterpriseError::BackupFailed`] if any step fails.
    pub fn create_snapshot(&self, backup_dir: &Path) -> Result<()> {
        // 1. Safety: refuse to overwrite an existing backup
        if backup_dir.exists() {
            return Err(EnterpriseError::BackupAlreadyExists(
                backup_dir.to_path_buf(),
            ));
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

        // 3. Flush: dirty pages → disk, FileBackend WAL checkpointed + truncated.
        {
            let mut g = self.graph.write();
            g.flush().map_err(|e| EnterpriseError::BackupFailed {
                reason: format!("graph flush failed: {e}"),
            })?;
        }

        // Capture the snapshot LSN before releasing the lock.
        let snapshot_lsn = wal_guard.next_lsn();

        // 4. Copy data files while WAL lock is held.
        let mut file_entries: Vec<FileEntry> = Vec::with_capacity(DATA_FILES.len() + 2);

        for name in &DATA_FILES {
            let src = self.graph_dir.join(name);
            if src.exists() {
                let entry = copy_file_with_checksum(&src, backup_dir, name)?;
                file_entries.push(entry);
            } else {
                // Create empty placeholder so restore finds all expected files.
                fs::write(backup_dir.join(name), b"")?;
                file_entries.push(FileEntry {
                    name: (*name).to_owned(),
                    size_bytes: 0,
                    crc32: crc32fast::hash(b""),
                });
            }
        }

        // Copy WAL tail (empty or near-empty after flush).
        let wal_src = self.graph_dir.join("wal.log");
        if wal_src.exists() {
            let entry = copy_file_with_checksum(&wal_src, backup_dir, "wal.log")?;
            file_entries.push(entry);
        } else {
            fs::write(backup_dir.join("wal.log"), b"")?;
            file_entries.push(FileEntry {
                name: "wal.log".to_owned(),
                size_bytes: 0,
                crc32: crc32fast::hash(b""),
            });
        }

        // Copy label index (optional — may not exist on new stores).
        let index_src = self.graph_dir.join("index.bin");
        if index_src.exists() {
            let entry = copy_file_with_checksum(&index_src, backup_dir, "index.bin")?;
            file_entries.push(entry);
        }

        // WAL lock released after all files are copied.
        drop(wal_guard);

        // 5. Write manifest.
        let created_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());

        let manifest = BackupManifest {
            created_at_unix_secs: created_at,
            snapshot_lsn,
            files: file_entries,
        };

        fs::write(
            backup_dir.join(MANIFEST_NAME),
            manifest.serialize().as_bytes(),
        )?;

        Ok(())
    }

    /// Verifies the integrity of an existing backup at `backup_dir`.
    ///
    /// Re-computes the CRC32 of every listed file and compares it against
    /// the stored checksum in the manifest.
    ///
    /// # Errors
    ///
    /// Returns [`EnterpriseError::ManifestCorrupt`] if the manifest is missing,
    /// unparseable, or any file's checksum does not match.
    pub fn verify_backup(backup_dir: &Path) -> Result<()> {
        let manifest_path = backup_dir.join(MANIFEST_NAME);
        let manifest_txt = fs::read_to_string(&manifest_path)
            .map_err(|e| EnterpriseError::ManifestCorrupt(format!("cannot read manifest: {e}")))?;
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

    /// Restores a backup from `backup_dir` to `restore_dir`.
    ///
    /// After `restore()` returns, call `Graph::open(restore_dir, config)`
    /// to open the restored graph. WAL recovery runs automatically.
    ///
    /// # Errors
    ///
    /// Returns [`EnterpriseError::RestoreFailed`] if `backup_dir` does not
    /// exist, if `restore_dir` already exists, or if the backup is invalid.
    pub fn restore(backup_dir: &Path, restore_dir: &Path) -> Result<()> {
        // 1. Validate backup source.
        if !backup_dir.exists() {
            return Err(EnterpriseError::RestoreFailed {
                reason: format!("backup directory does not exist: {}", backup_dir.display()),
            });
        }

        // 2. Safety: refuse to overwrite an existing directory.
        if restore_dir.exists() {
            return Err(EnterpriseError::RestoreFailed {
                reason: format!("restore target already exists: {}", restore_dir.display()),
            });
        }

        // 3. Read and validate the manifest.
        Self::verify_backup(backup_dir)?;

        let manifest_txt = fs::read_to_string(backup_dir.join(MANIFEST_NAME)).map_err(|e| {
            EnterpriseError::RestoreFailed {
                reason: format!("cannot read manifest: {e}"),
            }
        })?;
        let manifest =
            BackupManifest::parse(&manifest_txt).map_err(|e| EnterpriseError::RestoreFailed {
                reason: format!("manifest parse failed: {e}"),
            })?;

        // 4. Create restore directory and copy files.
        fs::create_dir_all(restore_dir)?;

        for entry in &manifest.files {
            let src = backup_dir.join(&entry.name);
            let dst = restore_dir.join(&entry.name);
            fs::copy(&src, &dst)?;
        }

        // Copy the manifest itself so the restored directory is self-contained
        // and can be passed to `verify_backup` independently.
        fs::copy(
            backup_dir.join(MANIFEST_NAME),
            restore_dir.join(MANIFEST_NAME),
        )?;

        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::significant_drop_tightening)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use tessera_graph::{Graph, GraphConfig, SharedGraph};

    fn setup_engine(dir: &TempDir) -> (BackupEngine, PathBuf) {
        let graph_dir = dir.path().join("graph");
        let wal_path = dir.path().join("enterprise.wal");
        let graph = Graph::open(&graph_dir, &GraphConfig::new()).unwrap();
        let shared = SharedGraph::new(graph);
        let txn_mgr = Arc::new(TransactionManager::open(&wal_path).unwrap());
        let engine = BackupEngine::new(shared, txn_mgr, &graph_dir);
        (engine, graph_dir)
    }

    #[test]
    fn new_returns_engine() {
        let dir = TempDir::new().unwrap();
        let (_engine, _) = setup_engine(&dir);
    }

    #[test]
    fn engine_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<BackupEngine>();
    }

    // --- create_snapshot tests ---

    #[test]
    fn create_snapshot_produces_backup_directory() {
        let dir = TempDir::new().unwrap();
        let (engine, _) = setup_engine(&dir);

        let backup_dir = dir.path().join("backup_001");
        engine.create_snapshot(&backup_dir).unwrap();

        assert!(backup_dir.exists());
        assert!(backup_dir.join(MANIFEST_NAME).exists());
        assert!(backup_dir.join("nodes.db").exists());
        assert!(backup_dir.join("edges.db").exists());
        assert!(backup_dir.join("adjacency.db").exists());
        assert!(backup_dir.join("strings.db").exists());
        assert!(backup_dir.join("overflow.db").exists());
        assert!(backup_dir.join("graph.meta").exists());
        assert!(backup_dir.join("wal.log").exists());
    }

    #[test]
    fn create_snapshot_manifest_lists_all_files() {
        let dir = TempDir::new().unwrap();
        let (engine, _) = setup_engine(&dir);

        let backup_dir = dir.path().join("backup_002");
        engine.create_snapshot(&backup_dir).unwrap();

        let manifest_txt = fs::read_to_string(backup_dir.join(MANIFEST_NAME)).unwrap();
        let manifest = BackupManifest::parse(&manifest_txt).unwrap();

        let names: Vec<&str> = manifest.files.iter().map(|f| f.name.as_str()).collect();
        assert!(names.contains(&"nodes.db"));
        assert!(names.contains(&"edges.db"));
        assert!(names.contains(&"graph.meta"));
        assert!(names.contains(&"wal.log"));
    }

    #[test]
    fn create_snapshot_fails_if_backup_dir_already_exists() {
        let dir = TempDir::new().unwrap();
        let (engine, _) = setup_engine(&dir);

        let backup_dir = dir.path().join("backup_exist");
        fs::create_dir_all(&backup_dir).unwrap();

        let result = engine.create_snapshot(&backup_dir);
        assert!(matches!(
            result,
            Err(EnterpriseError::BackupAlreadyExists(_))
        ));
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

        {
            let mut g = shared.write();
            g.add_node("Person", props! { "name" => "Alice" }).unwrap();
            g.add_node("Person", props! { "name" => "Bob" }).unwrap();
        }

        let engine = BackupEngine::new(shared, txn_mgr, &graph_dir);
        let backup_dir = dir.path().join("backup_data");
        engine.create_snapshot(&backup_dir).unwrap();

        let nodes_size = fs::metadata(backup_dir.join("nodes.db")).unwrap().len();
        assert!(
            nodes_size > 0,
            "nodes.db must not be empty after writing data"
        );
    }

    // --- verify_backup tests ---

    #[test]
    fn verify_backup_succeeds_on_valid_backup() {
        let dir = TempDir::new().unwrap();
        let (engine, _) = setup_engine(&dir);
        let backup_dir = dir.path().join("bk_verify_ok");
        engine.create_snapshot(&backup_dir).unwrap();

        BackupEngine::verify_backup(&backup_dir).unwrap();
    }

    #[test]
    fn verify_backup_detects_corrupted_file() {
        let dir = TempDir::new().unwrap();
        let (engine, _) = setup_engine(&dir);
        let backup_dir = dir.path().join("bk_verify_corrupt");
        engine.create_snapshot(&backup_dir).unwrap();

        // Corrupt nodes.db
        let nodes_path = backup_dir.join("nodes.db");
        let mut bytes = fs::read(&nodes_path).unwrap();
        if bytes.is_empty() {
            bytes.push(0xFF);
        } else {
            bytes[0] ^= 0xFF;
        }
        fs::write(&nodes_path, &bytes).unwrap();

        let result = BackupEngine::verify_backup(&backup_dir);
        assert!(matches!(result, Err(EnterpriseError::ManifestCorrupt(_))));
    }

    #[test]
    fn verify_backup_detects_missing_manifest() {
        let dir = TempDir::new().unwrap();
        let backup_dir = dir.path().join("bk_no_manifest");
        fs::create_dir_all(&backup_dir).unwrap();

        let result = BackupEngine::verify_backup(&backup_dir);
        assert!(matches!(result, Err(EnterpriseError::ManifestCorrupt(_))));
    }

    #[test]
    fn verify_backup_detects_missing_data_file() {
        let dir = TempDir::new().unwrap();
        let (engine, _) = setup_engine(&dir);
        let backup_dir = dir.path().join("bk_missing_file");
        engine.create_snapshot(&backup_dir).unwrap();

        fs::remove_file(backup_dir.join("edges.db")).unwrap();

        let result = BackupEngine::verify_backup(&backup_dir);
        assert!(matches!(result, Err(EnterpriseError::ManifestCorrupt(_))));
    }

    // --- restore tests ---

    #[test]
    fn restore_creates_openable_graph() {
        let dir = TempDir::new().unwrap();
        let (engine, _) = setup_engine(&dir);

        let backup_dir = dir.path().join("bk_restore_ok");
        engine.create_snapshot(&backup_dir).unwrap();

        let restore_dir = dir.path().join("restored");
        BackupEngine::restore(&backup_dir, &restore_dir).unwrap();

        let graph = Graph::open(&restore_dir, &GraphConfig::new()).unwrap();
        assert_eq!(graph.node_count(), 0);
    }

    #[test]
    fn restore_preserves_graph_data() {
        use tessera_graph::props;

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
        BackupEngine::restore(&backup_dir, &restore_dir).unwrap();

        let restored = Graph::open(&restore_dir, &GraphConfig::new()).unwrap();
        assert_eq!(restored.node_count(), 3, "all 3 nodes must survive restore");
    }

    #[test]
    fn restore_fails_if_backup_dir_missing() {
        let result = BackupEngine::restore(
            Path::new("/nonexistent/backup"),
            Path::new("/tmp/tessera_restore_test"),
        );
        assert!(matches!(result, Err(EnterpriseError::RestoreFailed { .. })));
    }

    #[test]
    fn restore_fails_if_target_dir_already_exists() {
        let dir = TempDir::new().unwrap();
        let (engine, _) = setup_engine(&dir);

        let backup_dir = dir.path().join("bk_target_exist");
        engine.create_snapshot(&backup_dir).unwrap();

        let restore_dir = dir.path().join("already_exists");
        fs::create_dir_all(&restore_dir).unwrap();

        let result = BackupEngine::restore(&backup_dir, &restore_dir);
        assert!(matches!(result, Err(EnterpriseError::RestoreFailed { .. })));
    }
}
