// SPDX-License-Identifier: MIT

use tempfile::TempDir;
use tessera_graph::{Graph, GraphConfig, Properties};

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
fn rebuild_indexes_repopulates_after_flush() {
    let tmp = TempDir::new().unwrap();
    let config = wal_config();

    let (nid_a, nid_b, eid);
    {
        let mut g = Graph::open(tmp.path(), &config).unwrap();
        nid_a = g.add_node("Person", Properties::new()).unwrap();
        nid_b = g.add_node("City", Properties::new()).unwrap();
        eid = g
            .add_edge("LIVES_IN", nid_a, nid_b, Properties::new())
            .unwrap();
        g.flush().unwrap();

        // Rebuild indexes — should produce the same state.
        g.rebuild_indexes().unwrap();

        assert_eq!(g.node_count(), 2);
        assert_eq!(g.edge_count(), 1);
        assert!(g.node_exists(nid_a));
        assert!(g.node_exists(nid_b));
        assert!(g.edge(eid).is_ok());

        // Label indexes should also be intact.
        let persons = g.nodes_by_label("Person");
        assert_eq!(persons.len(), 1);
        assert_eq!(persons[0], nid_a);
    }
}

#[test]
fn rebuild_indexes_on_memory_graph_is_noop() {
    let mut g = Graph::new();
    g.rebuild_indexes().unwrap();
    assert_eq!(g.node_count(), 0);
    assert_eq!(g.edge_count(), 0);
}

#[test]
fn rebuild_indexes_on_memory_graph_with_data() {
    let mut g = Graph::new();
    let a = g.add_node("A", Properties::new()).unwrap();
    let b = g.add_node("B", Properties::new()).unwrap();
    g.add_edge("E", a, b, Properties::new()).unwrap();

    g.rebuild_indexes().unwrap();

    assert_eq!(g.node_count(), 2);
    assert_eq!(g.edge_count(), 1);
    assert!(g.node_exists(a));
    assert!(g.node_exists(b));
}

#[test]
fn rebuild_indexes_after_wal_recovery() {
    let tmp = TempDir::new().unwrap();
    let config = wal_config();

    // Session 1: write data, crash without flushing — WAL contains the writes.
    let (nid_a, nid_b, eid);
    {
        let mut g = Graph::open(tmp.path(), &config).unwrap();
        nid_a = g.add_node("Person", Properties::new()).unwrap();
        nid_b = g.add_node("City", Properties::new()).unwrap();
        eid = g
            .add_edge("LIVES_IN", nid_a, nid_b, Properties::new())
            .unwrap();
        drop(g); // crash — no flush
    }

    // Session 2: reopen triggers WAL recovery, then explicitly rebuild indexes.
    {
        let mut g = Graph::open(tmp.path(), &config).unwrap();
        assert!(g.node_exists(nid_a), "WAL recovery must restore nid_a");
        assert!(g.node_exists(nid_b), "WAL recovery must restore nid_b");

        // Hot-repair: caller explicitly invokes rebuild_indexes.
        g.rebuild_indexes().unwrap();

        assert_eq!(g.node_count(), 2);
        assert_eq!(g.edge_count(), 1);
        assert!(g.node_exists(nid_a));
        assert!(g.node_exists(nid_b));
        assert!(g.edge(eid).is_ok());

        let persons = g.nodes_by_label("Person");
        assert_eq!(persons.len(), 1);
        assert_eq!(persons[0], nid_a);
    }
}
