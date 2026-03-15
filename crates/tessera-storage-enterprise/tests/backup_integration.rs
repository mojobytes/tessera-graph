use std::fs;
use std::sync::Arc;

use tessera_graph::{props, Graph, GraphConfig, SharedGraph};
use tessera_storage_enterprise::backup::{BackupEngine, BackupManifest};
use tessera_storage_enterprise::txn::{IsolationLevel, TransactionManager};

// ── Helper ────────────────────────────────────────────────────────────────────

fn setup(root: &tempfile::TempDir) -> (BackupEngine, SharedGraph, Arc<TransactionManager>) {
    let graph_dir = root.path().join("graph");
    let wal_path = root.path().join("enterprise.wal");
    let graph = Graph::open(&graph_dir, &GraphConfig::new()).unwrap();
    let shared = SharedGraph::new(graph);
    let txn_mgr = Arc::new(TransactionManager::open(&wal_path).unwrap());
    let engine = BackupEngine::new(shared.clone(), Arc::clone(&txn_mgr), &graph_dir);
    (engine, shared, txn_mgr)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

/// Full transaction flow: committed nodes survive backup/restore, post-backup
/// nodes do not.
#[test]
fn full_txn_flow_backup_and_restore() {
    let root = tempfile::TempDir::new().unwrap();
    let (engine, shared, txn_mgr) = setup(&root);

    // T1: add 2 nodes and commit.
    let mut t1 = txn_mgr.begin(IsolationLevel::ReadCommitted).unwrap();
    {
        let mut g = shared.write();
        g.add_node("Person", props! { "name" => "Alice" }).unwrap();
        g.add_node("Person", props! { "name" => "Bob" }).unwrap();
        drop(g);
    }
    txn_mgr.commit(&mut t1).unwrap();

    // Create snapshot.
    let backup_dir = root.path().join("backup_001");
    engine.create_snapshot(&backup_dir).unwrap();

    // Add a third node AFTER the snapshot.
    shared
        .write()
        .add_node("Person", props! { "name" => "Charlie" })
        .unwrap();

    // Restore snapshot to a fresh directory.
    let restore_dir = root.path().join("restored_001");
    BackupEngine::restore(&backup_dir, &restore_dir).unwrap();

    let restored = Graph::open(&restore_dir, &GraphConfig::new()).unwrap();
    assert_eq!(
        restored.node_count(),
        2,
        "post-backup node must not appear in restore"
    );
}

/// Restore preserves exact property values on individual nodes.
#[test]
fn restore_preserves_individual_property_values() {
    let root = tempfile::TempDir::new().unwrap();
    let (engine, shared, _txn_mgr) = setup(&root);

    shared
        .write()
        .add_node("User", props! { "name" => "Alice", "age" => 30i64 })
        .unwrap();

    let backup_dir = root.path().join("backup_props");
    engine.create_snapshot(&backup_dir).unwrap();

    let restore_dir = root.path().join("restored_props");
    BackupEngine::restore(&backup_dir, &restore_dir).unwrap();

    let restored = Graph::open(&restore_dir, &GraphConfig::new()).unwrap();
    let ids = restored.nodes_by_label("User");
    assert_eq!(ids.len(), 1, "exactly one User node expected after restore");

    let node = restored.node(ids[0]).unwrap();
    assert_eq!(node.label(), "User");

    let props = node.properties();
    assert_eq!(
        props.get("name").and_then(|p| p.as_str()),
        Some("Alice"),
        "name property must be preserved"
    );
    assert_eq!(
        props.get("age").and_then(tessera_graph::Property::as_i64),
        Some(30),
        "age property must be preserved"
    );
}

/// Restore preserves edges and their properties.
#[test]
fn restore_preserves_edges_and_edge_properties() {
    let root = tempfile::TempDir::new().unwrap();
    let (engine, shared, _txn_mgr) = setup(&root);

    {
        let mut g = shared.write();
        let paris = g.add_node("City", props! { "name" => "Paris" }).unwrap();
        let lyon = g.add_node("City", props! { "name" => "Lyon" }).unwrap();
        g.add_edge("ROUTE", paris, lyon, props! { "km" => 470i64 })
            .unwrap();
        drop(g);
    }

    let backup_dir = root.path().join("backup_edges");
    engine.create_snapshot(&backup_dir).unwrap();

    let restore_dir = root.path().join("restored_edges");
    BackupEngine::restore(&backup_dir, &restore_dir).unwrap();

    let restored = Graph::open(&restore_dir, &GraphConfig::new()).unwrap();
    assert_eq!(restored.node_count(), 2, "both city nodes must survive restore");
    assert_eq!(restored.edge_count(), 1, "the ROUTE edge must survive restore");

    let edge_ids = restored.edges_by_label("ROUTE");
    assert_eq!(edge_ids.len(), 1);

    let edge = restored.edge(edge_ids[0]).unwrap();
    assert_eq!(edge.label(), "ROUTE");
    assert_eq!(
        edge.properties()
            .get("km")
            .and_then(tessera_graph::Property::as_i64),
        Some(470),
        "km property must be preserved on the edge"
    );
}

/// Two sequential snapshots are independent: each restores to its own
/// point-in-time, and the manifest LSNs are monotonically increasing.
#[test]
fn sequential_snapshots_are_independent() {
    let root = tempfile::TempDir::new().unwrap();
    let (engine, shared, _txn_mgr) = setup(&root);

    // Add N1, take snapshot 1.
    shared
        .write()
        .add_node("Node", props! { "id" => "N1" })
        .unwrap();

    let backup_1 = root.path().join("backup_seq_1");
    engine.create_snapshot(&backup_1).unwrap();

    // Add N2, take snapshot 2.
    shared
        .write()
        .add_node("Node", props! { "id" => "N2" })
        .unwrap();

    let backup_2 = root.path().join("backup_seq_2");
    engine.create_snapshot(&backup_2).unwrap();

    // Restore both snapshots independently.
    let restore_1 = root.path().join("restored_seq_1");
    BackupEngine::restore(&backup_1, &restore_1).unwrap();

    let restore_2 = root.path().join("restored_seq_2");
    BackupEngine::restore(&backup_2, &restore_2).unwrap();

    let g1 = Graph::open(&restore_1, &GraphConfig::new()).unwrap();
    let g2 = Graph::open(&restore_2, &GraphConfig::new()).unwrap();

    assert_eq!(g1.node_count(), 1, "snapshot 1 must contain only N1");
    assert_eq!(g2.node_count(), 2, "snapshot 2 must contain N1 and N2");

    // Parse manifests and verify LSN monotonicity.
    let manifest_1_txt = fs::read_to_string(backup_1.join("manifest.txt")).unwrap();
    let manifest_2_txt = fs::read_to_string(backup_2.join("manifest.txt")).unwrap();
    let manifest_1 = BackupManifest::parse(&manifest_1_txt).unwrap();
    let manifest_2 = BackupManifest::parse(&manifest_2_txt).unwrap();

    assert!(
        manifest_2.snapshot_lsn >= manifest_1.snapshot_lsn,
        "LSN of snapshot 2 ({}) must be >= LSN of snapshot 1 ({})",
        manifest_2.snapshot_lsn,
        manifest_1.snapshot_lsn
    );
}

/// Backup can be created while a transaction is active (not yet committed).
/// The uncommitted data is not part of the snapshot, but the operation succeeds.
#[test]
fn concurrent_backup_with_active_transaction() {
    let root = tempfile::TempDir::new().unwrap();
    let (engine, shared, txn_mgr) = setup(&root);

    // Add a committed node.
    shared
        .write()
        .add_node("Thing", props! { "key" => "value" })
        .unwrap();

    // Begin a transaction but do NOT commit it.
    let _t1 = txn_mgr.begin(IsolationLevel::ReadCommitted).unwrap();

    // `create_snapshot` must succeed even with an active transaction.
    let backup_dir = root.path().join("backup_active_txn");
    engine.create_snapshot(&backup_dir).unwrap();

    // `verify_backup` must pass.
    BackupEngine::verify_backup(&backup_dir).unwrap();

    // Restore and open — must see exactly 1 committed node.
    let restore_dir = root.path().join("restored_active_txn");
    BackupEngine::restore(&backup_dir, &restore_dir).unwrap();

    let restored = Graph::open(&restore_dir, &GraphConfig::new()).unwrap();
    assert_eq!(restored.node_count(), 1);
}

/// `verify_backup` succeeds on a RESTORED directory (files are bit-perfect copies,
/// so checksums must match the manifest written at backup time).
#[test]
fn verify_passes_on_restored_copy() {
    let root = tempfile::TempDir::new().unwrap();
    let (engine, shared, _txn_mgr) = setup(&root);

    shared
        .write()
        .add_node("Item", props! { "name" => "test" })
        .unwrap();

    let backup_dir = root.path().join("backup_verify_restore");
    engine.create_snapshot(&backup_dir).unwrap();

    let restore_dir = root.path().join("restored_verify");
    BackupEngine::restore(&backup_dir, &restore_dir).unwrap();

    // `verify_backup` on the RESTORED directory must pass — files were
    // copied bit-perfect, including the manifest with embedded checksums.
    BackupEngine::verify_backup(&restore_dir).unwrap();
}
