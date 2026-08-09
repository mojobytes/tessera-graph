// SPDX-License-Identifier: MIT

use tempfile::TempDir;
use tessera_graph::{Graph, GraphConfig, Properties, props};

const fn wal_config() -> GraphConfig {
    GraphConfig {
        memory_limit_bytes: 64 * 1024,
        create_if_missing: true,
        adj_cache_capacity: 1024,
        wal_enabled: true,
        ..GraphConfig::new()
    }
}

#[test]
fn wal_file_created_on_open() {
    let tmp = TempDir::new().unwrap();
    let _g = Graph::open(tmp.path(), &wal_config()).unwrap();
    assert!(tmp.path().join("wal.log").exists());
}

#[test]
fn wal_contains_records_after_mutations() {
    let tmp = TempDir::new().unwrap();
    {
        let mut g = Graph::open(tmp.path(), &wal_config()).unwrap();
        let a = g.add_node("A", props! { "x" => 1_i64 }).unwrap();
        let b = g.add_node("B", Properties::new()).unwrap();
        g.add_edge("R", a, b, Properties::new()).unwrap();
        // Do NOT flush — WAL should contain the records.
    }

    // WAL file should be non-empty.
    let wal_data = std::fs::read(tmp.path().join("wal.log")).unwrap();
    assert!(
        !wal_data.is_empty(),
        "WAL should contain records after mutations without flush"
    );
}

#[test]
fn wal_is_truncated_after_flush() {
    let tmp = TempDir::new().unwrap();
    {
        let mut g = Graph::open(tmp.path(), &wal_config()).unwrap();
        g.add_node("A", Properties::new()).unwrap();
        g.flush().unwrap();
    }

    let wal_data = std::fs::read(tmp.path().join("wal.log")).unwrap();
    assert!(wal_data.is_empty(), "WAL should be empty after flush");
}

#[test]
fn wal_not_created_when_disabled_in_config() {
    let tmp = TempDir::new().unwrap();
    {
        let mut g = Graph::open(tmp.path(), &GraphConfig::without_wal()).unwrap();
        g.add_node("A", Properties::new()).unwrap();
        g.flush().unwrap();
    }
    assert!(!tmp.path().join("wal.log").exists());
}

#[test]
fn flush_without_wal_persists_data() {
    let tmp = TempDir::new().unwrap();
    let no_wal_config = GraphConfig::without_wal();

    {
        let mut g = Graph::open(tmp.path(), &no_wal_config).unwrap();
        let n = g.add_node("Person", props! { "name" => "Alice" }).unwrap();
        g.flush().unwrap();
        assert_eq!(g.node(n).unwrap().label(), "Person");
    }

    // Reopen and verify data persists without WAL.
    {
        let g = Graph::open(tmp.path(), &no_wal_config).unwrap();
        assert_eq!(g.node_count(), 1);
    }
}
