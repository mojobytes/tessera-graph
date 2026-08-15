// SPDX-License-Identifier: MIT

use std::collections::HashSet;

use ermya_graph::{EdgeId, Graph, GraphConfig, NodeId, props};

// ── In-memory label index tests ──────────────────────────────────────

#[test]
fn nodes_by_label_returns_correct_ids() {
    let mut g = Graph::new();
    let p1 = g.add_node("Person", props! { "name" => "Alice" }).unwrap();
    let p2 = g.add_node("Person", props! { "name" => "Bob" }).unwrap();
    let _d = g.add_node("Device", props! {}).unwrap();

    let persons: HashSet<NodeId> = g.nodes_by_label("Person").into_iter().collect();
    assert_eq!(persons, HashSet::from([p1, p2]));
}

#[test]
fn edges_by_label_returns_correct_ids() {
    let mut g = Graph::new();
    let a = g.add_node("A", props! {}).unwrap();
    let b = g.add_node("B", props! {}).unwrap();
    let e1 = g.add_edge("KNOWS", a, b, props! {}).unwrap();
    let e2 = g.add_edge("KNOWS", b, a, props! {}).unwrap();
    let _f = g.add_edge("FOLLOWS", a, b, props! {}).unwrap();

    let knows: HashSet<EdgeId> = g.edges_by_label("KNOWS").into_iter().collect();
    assert_eq!(knows, HashSet::from([e1, e2]));
}

#[test]
fn nodes_by_label_empty_after_remove_all() {
    let mut g = Graph::new();
    let p1 = g.add_node("Person", props! {}).unwrap();
    let p2 = g.add_node("Person", props! {}).unwrap();
    g.remove_node(p1).unwrap();
    g.remove_node(p2).unwrap();
    assert!(g.nodes_by_label("Person").is_empty());
}

#[test]
fn unknown_label_returns_empty_vec() {
    let g = Graph::new();
    assert!(g.nodes_by_label("Nonexistent").is_empty());
    assert!(g.edges_by_label("Nonexistent").is_empty());
}

#[test]
fn nodes_by_label_multiple_labels() {
    let mut g = Graph::new();
    g.add_node("Person", props! {}).unwrap();
    g.add_node("Person", props! {}).unwrap();
    g.add_node("Device", props! {}).unwrap();
    g.add_node("Location", props! {}).unwrap();

    assert_eq!(g.nodes_by_label("Person").len(), 2);
    assert_eq!(g.nodes_by_label("Device").len(), 1);
    assert_eq!(g.nodes_by_label("Location").len(), 1);
    assert!(g.nodes_by_label("Other").is_empty());
}

#[test]
fn remove_node_cascades_edge_label_index() {
    let mut g = Graph::new();
    let a = g.add_node("A", props! {}).unwrap();
    let b = g.add_node("B", props! {}).unwrap();
    g.add_edge("KNOWS", a, b, props! {}).unwrap();
    g.add_edge("FOLLOWS", a, b, props! {}).unwrap();

    // Removing node a should cascade-remove both edges from label indexes
    g.remove_node(a).unwrap();
    assert!(g.edges_by_label("KNOWS").is_empty());
    assert!(g.edges_by_label("FOLLOWS").is_empty());
}

#[test]
fn remove_edge_updates_label_index() {
    let mut g = Graph::new();
    let a = g.add_node("A", props! {}).unwrap();
    let b = g.add_node("B", props! {}).unwrap();
    let e = g.add_edge("KNOWS", a, b, props! {}).unwrap();

    assert_eq!(g.edges_by_label("KNOWS").len(), 1);
    g.remove_edge(e).unwrap();
    assert!(g.edges_by_label("KNOWS").is_empty());
}

// ── Persistence tests ────────────────────────────────────────────────

#[test]
fn label_index_persists_across_reopen() {
    let tmp = tempfile::TempDir::new().unwrap();
    let config = GraphConfig::new();

    {
        let mut g = Graph::open(tmp.path(), &config).unwrap();
        g.add_node("Person", props! { "name" => "Alice" }).unwrap();
        g.add_node("Person", props! { "name" => "Bob" }).unwrap();
        g.add_node("Device", props! {}).unwrap();
        g.flush().unwrap();
    }

    {
        let g = Graph::open(tmp.path(), &config).unwrap();
        assert_eq!(g.nodes_by_label("Person").len(), 2);
        assert_eq!(g.nodes_by_label("Device").len(), 1);
    }
}

#[test]
fn label_index_persists_edges_across_reopen() {
    let tmp = tempfile::TempDir::new().unwrap();
    let config = GraphConfig::new();

    {
        let mut g = Graph::open(tmp.path(), &config).unwrap();
        let a = g.add_node("A", props! {}).unwrap();
        let b = g.add_node("B", props! {}).unwrap();
        g.add_edge("KNOWS", a, b, props! {}).unwrap();
        g.add_edge("FOLLOWS", a, b, props! {}).unwrap();
        g.flush().unwrap();
    }

    {
        let g = Graph::open(tmp.path(), &config).unwrap();
        assert_eq!(g.edges_by_label("KNOWS").len(), 1);
        assert_eq!(g.edges_by_label("FOLLOWS").len(), 1);
    }
}

#[test]
fn label_index_rebuilds_when_index_file_missing() {
    let tmp = tempfile::TempDir::new().unwrap();
    let config = GraphConfig::new();

    {
        let mut g = Graph::open(tmp.path(), &config).unwrap();
        g.add_node("Person", props! { "name" => "Alice" }).unwrap();
        g.add_node("Person", props! { "name" => "Bob" }).unwrap();
        g.add_node("Device", props! {}).unwrap();
        let a = g.nodes_by_label("Person")[0];
        let b = g.nodes_by_label("Device")[0];
        g.add_edge("USES", a, b, props! {}).unwrap();
        g.flush().unwrap();
    }

    // Delete index.bin — force rebuild from page scan
    std::fs::remove_file(tmp.path().join("index.bin")).unwrap();

    {
        let g = Graph::open(tmp.path(), &config).unwrap();
        assert_eq!(g.nodes_by_label("Person").len(), 2);
        assert_eq!(g.nodes_by_label("Device").len(), 1);
        assert_eq!(g.edges_by_label("USES").len(), 1);
    }
}

#[test]
fn label_index_rebuilds_when_index_file_corrupt() {
    let tmp = tempfile::TempDir::new().unwrap();
    let config = GraphConfig::new();

    {
        let mut g = Graph::open(tmp.path(), &config).unwrap();
        g.add_node("Person", props! {}).unwrap();
        g.flush().unwrap();
    }

    // Corrupt index.bin
    std::fs::write(tmp.path().join("index.bin"), b"GARBAGE").unwrap();

    {
        let g = Graph::open(tmp.path(), &config).unwrap();
        assert_eq!(g.nodes_by_label("Person").len(), 1);
    }
}

#[test]
fn incremental_label_index_across_sessions() {
    let tmp = tempfile::TempDir::new().unwrap();
    let config = GraphConfig::new();

    // Session 1: add Person nodes
    {
        let mut g = Graph::open(tmp.path(), &config).unwrap();
        g.add_node("Person", props! {}).unwrap();
        g.add_node("Person", props! {}).unwrap();
        g.flush().unwrap();
    }

    // Session 2: add Device nodes
    {
        let mut g = Graph::open(tmp.path(), &config).unwrap();
        assert_eq!(g.nodes_by_label("Person").len(), 2);
        g.add_node("Device", props! {}).unwrap();
        g.flush().unwrap();
    }

    // Session 3: verify both labels
    {
        let g = Graph::open(tmp.path(), &config).unwrap();
        assert_eq!(g.nodes_by_label("Person").len(), 2);
        assert_eq!(g.nodes_by_label("Device").len(), 1);
    }
}

// ── Label index consistency on update ────────────────────────────────

#[test]
fn update_node_label_updates_index() {
    let mut g = Graph::new();
    let id = g.add_node("Person", props! {}).unwrap();

    let mut node = g.node(id).unwrap();
    node.set_label("Device");
    g.update_node(id, &node).unwrap();

    assert!(
        g.nodes_by_label("Person").is_empty(),
        "old label 'Person' must be removed from index"
    );
    let devices = g.nodes_by_label("Device");
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0], id);
}

#[test]
fn update_edge_label_updates_index() {
    let mut g = Graph::new();
    let a = g.add_node("A", props! {}).unwrap();
    let b = g.add_node("B", props! {}).unwrap();
    let eid = g.add_edge("KNOWS", a, b, props! {}).unwrap();

    let mut edge = g.edge(eid).unwrap();
    edge.set_label("FOLLOWS");
    g.update_edge(eid, &edge).unwrap();

    assert!(
        g.edges_by_label("KNOWS").is_empty(),
        "old label 'KNOWS' must be removed from index"
    );
    let follows = g.edges_by_label("FOLLOWS");
    assert_eq!(follows.len(), 1);
    assert_eq!(follows[0], eid);
}

// ── Throughput regression guard ──────────────────────────────────────

#[test]
fn label_index_insert_throughput_regression_guard() {
    let mut g = Graph::new();
    let labels = ["Person", "Device", "Location", "Event", "Resource"];
    let first_start = std::time::Instant::now();
    for i in 0..5_000_usize {
        g.add_node(labels[i % labels.len()], props! {}).unwrap();
    }
    let first_half = first_start.elapsed();
    let second_start = std::time::Instant::now();
    for i in 5_000..10_000_usize {
        g.add_node(labels[i % labels.len()], props! {}).unwrap();
    }
    let second_half = second_start.elapsed();
    let ratio = second_half.as_secs_f64() / first_half.as_secs_f64().max(f64::EPSILON);
    // Equal-sized halves should cost roughly the same. Super-linear index
    // maintenance makes the second half grow disproportionately.
    assert!(
        ratio < 3.0,
        "label index insert scaling regression: ratio {ratio:.2} \
         (first 5k {first_half:?}, second 5k {second_half:?})"
    );
}
