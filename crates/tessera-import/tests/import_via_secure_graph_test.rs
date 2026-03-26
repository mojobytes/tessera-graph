// Copyright 2026 BelowZero Security OU. All rights reserved.

//! Tests verifying that all import functions accept `SecureGraph` (not just `Graph`)
//! and that LBAC enforcement is applied to imported data.

use std::collections::BTreeSet;

use tessera_auth::lbac::{Clearance, SecurityPolicy};
use tessera_graph::{Graph, GraphAccess as _};
use tessera_import::csv::{export_edges_csv, export_nodes_csv, import_edges_csv, import_nodes_csv};
use tessera_import::error::ImportError;
use tessera_import::gql_export::export_gql;
use tessera_import::gql_import::import_gql;
use tessera_import::json::{export_json, import_json};
use tessera_storage_enterprise::lbac::{SecureGraph, SecureGraphRef};

const fn clearance(level: u16) -> Clearance {
    Clearance::new(level, BTreeSet::new())
}

// ── import_gql accepts SecureGraph ──────────────────────────────────────────

#[test]
fn import_gql_accepts_secure_graph() {
    let mut g = Graph::new();
    let mut sg = SecureGraph::new(&mut g, clearance(5));
    let summary = import_gql(&mut sg, "CREATE (:Person {name: 'Alice'})").unwrap();
    assert_eq!(summary.statements_executed, 1);
    assert_eq!(summary.nodes_created, 1);
    // The node exists in the underlying graph.
    assert_eq!(g.node_count(), 1);
    // Security label was injected by SecureGraph (Bell-LaPadula).
    let id = g.nodes_by_label("Person")[0];
    let raw = g.node(id).unwrap();
    let label = SecurityPolicy::extract_label(raw.properties());
    assert_eq!(label.level, 5);
}

// ── import_gql LBAC enforcement ─────────────────────────────────────────────

#[test]
fn import_gql_lbac_enforced_node_invisible_below_clearance() {
    let mut g = Graph::new();
    // Import with level-5 clearance — nodes get stamped at level 5.
    {
        let mut sg = SecureGraph::new(&mut g, clearance(5));
        import_gql(&mut sg, "CREATE (:Secret {name: 'classified'})").unwrap();
    }
    // A level-1 clearance user cannot see the imported node via SecureGraph.
    let sg_low = SecureGraphRef::new(&g, clearance(1));
    assert_eq!(
        sg_low.node_count(),
        0,
        "node stamped at level 5 must be invisible at level 1"
    );
    // But the raw graph still has it.
    assert_eq!(g.node_count(), 1);
    // Verify the label level is exactly 5 (Item 5 quality fix).
    let id = g.nodes_by_label("Secret")[0];
    let raw = g.node(id).unwrap();
    let label = SecurityPolicy::extract_label(raw.properties());
    assert_eq!(
        label.level, 5,
        "node imported at clearance 5 must be stamped at level 5"
    );
}

// ── import_gql edges via SecureGraph (Item 6) ───────────────────────────────

#[test]
fn import_gql_edges_via_secure_graph() {
    let mut g = Graph::new();
    {
        let mut sg = SecureGraph::new(&mut g, clearance(3));
        // Edge CREATE requires all nodes and the edge in a single statement.
        import_gql(
            &mut sg,
            "CREATE (a:Person {name: 'Alice'})-[:KNOWS]->(b:Person {name: 'Bob'})",
        )
        .unwrap();
    }
    // Raw graph has 2 nodes and 1 edge.
    assert_eq!(g.node_count(), 2);
    assert_eq!(g.edge_count(), 1);
    // Edge is invisible to a lower clearance.
    let sg_low = SecureGraphRef::new(&g, clearance(0));
    assert_eq!(
        sg_low.edge_count(),
        0,
        "edge at level 3 invisible at level 0"
    );
    // Edge is visible at the same clearance.
    let sg_ok = SecureGraphRef::new(&g, clearance(3));
    assert_eq!(sg_ok.edge_count(), 1);
}

// ── import_nodes_csv accepts SecureGraph ─────────────────────────────────────

#[test]
fn import_nodes_csv_accepts_secure_graph() {
    let mut g = Graph::new();
    let mut sg = SecureGraph::new(&mut g, clearance(3));
    let csv = "label,name\nPerson,Alice\nPerson,Bob\n";
    let count = import_nodes_csv(&mut sg, csv).unwrap();
    assert_eq!(count, 2);
    assert_eq!(g.node_count(), 2);
    // Both nodes carry the clearance label.
    for id in g.nodes_by_label("Person") {
        let raw = g.node(id).unwrap();
        let lbl = SecurityPolicy::extract_label(raw.properties());
        assert_eq!(lbl.level, 3);
    }
}

// ── import_nodes_csv write failure returns GraphWrite, not CsvParse ──────────

#[test]
fn csv_node_write_error_variant_is_graph_write() {
    let g = Graph::new();
    // SecureGraphRef is read-only — all mutations return Err.
    let mut sg_ro = SecureGraphRef::new(&g, clearance(1));
    let err = import_nodes_csv(&mut sg_ro, "label,name\nPerson,Alice\n").unwrap_err();
    assert!(
        matches!(err, ImportError::GraphWrite(_)),
        "expected GraphWrite, got: {err}"
    );
}

// ── import_edges_csv LBAC enforcement ───────────────────────────────────────

#[test]
fn import_edges_csv_lbac_enforced() {
    let mut g = Graph::new();
    // First import nodes so we have IDs to reference.
    {
        let mut sg = SecureGraph::new(&mut g, clearance(2));
        import_nodes_csv(&mut sg, "label,name\nPerson,Alice\nPerson,Bob\n").unwrap();
    }
    assert_eq!(g.node_count(), 2);
    // Import edges via SecureGraph.
    let edge_csv = "source_label,source_prop,source_value,target_label,target_prop,target_value,rel_label\n\
                    Person,name,Alice,Person,name,Bob,KNOWS\n";
    {
        let mut sg = SecureGraph::new(&mut g, clearance(2));
        import_edges_csv(&mut sg, edge_csv).unwrap();
    }
    // A level-0 user cannot see the edge (stamped at level 2).
    let sg_low = SecureGraphRef::new(&g, clearance(0));
    assert_eq!(
        sg_low.edge_count(),
        0,
        "edge stamped at level 2 must be invisible at level 0"
    );
    // But via raw graph or high-clearance view, the edge exists.
    let sg_high = SecureGraphRef::new(&g, clearance(2));
    assert_eq!(sg_high.edge_count(), 1);
}

// ── import_edges_csv mixed clearance returns NodeNotFoundForEdge (Item 1) ───

#[test]
fn import_edges_csv_returns_node_not_found_for_insufficiently_cleared_endpoint() {
    let mut g = Graph::new();
    // Import nodes at clearance 5.
    {
        let mut sg = SecureGraph::new(&mut g, clearance(5));
        import_nodes_csv(&mut sg, "label,name\nPerson,Alice\nPerson,Bob\n").unwrap();
    }
    assert_eq!(g.node_count(), 2);
    // Attempt to import edges at clearance 2 — nodes are invisible.
    let edge_csv = "source_label,source_prop,source_value,target_label,target_prop,target_value,rel_label\n\
                    Person,name,Alice,Person,name,Bob,KNOWS\n";
    let mut sg_low = SecureGraph::new(&mut g, clearance(2));
    let err = import_edges_csv(&mut sg_low, edge_csv).unwrap_err();
    assert!(
        matches!(err, ImportError::NodeNotFoundForEdge { .. }),
        "expected NodeNotFoundForEdge when nodes are above caller's clearance, got: {err}"
    );
    assert_eq!(g.edge_count(), 0, "no edge should have been created");
}

// ── import_json accepts SecureGraph ─────────────────────────────────────────

#[test]
fn import_json_accepts_secure_graph() {
    let mut g = Graph::new();
    let mut sg = SecureGraph::new(&mut g, clearance(4));
    let json = r#"{"nodes":[{"label":"Device","properties":{"id":"d1"}}],"edges":[]}"#;
    let summary = import_json(&mut sg, json).unwrap();
    assert_eq!(summary.nodes_imported, 1);
    assert_eq!(g.node_count(), 1);
    let id = g.nodes_by_label("Device")[0];
    let raw = g.node(id).unwrap();
    let lbl = SecurityPolicy::extract_label(raw.properties());
    assert_eq!(lbl.level, 4);
}

// ── import_json mixed clearance returns NodeNotFoundForEdge (Item 1) ────────

#[test]
fn import_json_returns_node_not_found_for_insufficiently_cleared_endpoint() {
    let mut g = Graph::new();
    // Import nodes at clearance 5.
    {
        let mut sg = SecureGraph::new(&mut g, clearance(5));
        import_nodes_csv(&mut sg, "label,name\nPerson,Alice\nPerson,Bob\n").unwrap();
    }
    // Attempt JSON edge import at clearance 2.
    let json = r#"{
        "nodes": [],
        "edges": [{
            "source": {"label": "Person", "match": {"name": "Alice"}},
            "target": {"label": "Person", "match": {"name": "Bob"}},
            "label": "KNOWS",
            "properties": {}
        }]
    }"#;
    let mut sg_low = SecureGraph::new(&mut g, clearance(2));
    let err = import_json(&mut sg_low, json).unwrap_err();
    assert!(
        matches!(err, ImportError::NodeNotFoundForEdge { .. }),
        "expected NodeNotFoundForEdge when nodes are above caller's clearance, got: {err}"
    );
}

// ── export_nodes_csv via SecureGraphRef filters by clearance ─────────────────

#[test]
fn export_nodes_csv_via_secure_graph_ref_filters_by_clearance() {
    let mut g = Graph::new();
    // Node at clearance 3.
    {
        let mut sg = SecureGraph::new(&mut g, clearance(3));
        import_nodes_csv(&mut sg, "label,name\nPerson,Alice\n").unwrap();
    }
    // Node at clearance 5.
    {
        let mut sg = SecureGraph::new(&mut g, clearance(5));
        import_nodes_csv(&mut sg, "label,name\nPerson,Bob\n").unwrap();
    }
    assert_eq!(g.node_count(), 2);
    // Export at clearance 3 — only Alice visible.
    let sg = SecureGraphRef::new(&g, clearance(3));
    let csv = export_nodes_csv(&sg).unwrap();
    assert_eq!(
        csv.lines().skip(1).filter(|l| !l.is_empty()).count(),
        1,
        "only 1 node visible at clearance 3"
    );
    assert!(csv.contains("Alice"), "Alice should be in the export");
    assert!(
        !csv.contains("Bob"),
        "Bob (clearance 5) should be filtered out"
    );
}

// ── export_edges_csv via SecureGraphRef filters by clearance ─────────────────

#[test]
fn export_edges_csv_via_secure_graph_ref_filters_by_clearance() {
    let mut g = Graph::new();
    {
        let mut sg = SecureGraph::new(&mut g, clearance(3));
        import_nodes_csv(&mut sg, "label,name\nPerson,Alice\nPerson,Bob\n").unwrap();
        let edge_csv = "source_label,source_prop,source_value,target_label,target_prop,target_value,rel_label\n\
                        Person,name,Alice,Person,name,Bob,KNOWS\n";
        import_edges_csv(&mut sg, edge_csv).unwrap();
    }
    // Export at clearance 0 — nothing visible.
    let sg_low = SecureGraphRef::new(&g, clearance(0));
    let csv = export_edges_csv(&sg_low).unwrap();
    assert_eq!(
        csv.lines().skip(1).filter(|l| !l.is_empty()).count(),
        0,
        "no edges visible at clearance 0"
    );
}

// ── export_json via SecureGraphRef filters by clearance ──────────────────────

#[test]
fn export_json_via_secure_graph_ref_filters_by_clearance() {
    let mut g = Graph::new();
    {
        let mut sg = SecureGraph::new(&mut g, clearance(5));
        import_nodes_csv(&mut sg, "label,name\nPerson,Alice\n").unwrap();
    }
    // Export at clearance 1 — no nodes visible.
    let sg_low = SecureGraphRef::new(&g, clearance(1));
    let json_str = export_json(&sg_low).unwrap();
    let root: serde_json::Value = serde_json::from_str(&json_str).unwrap();
    let nodes = root["nodes"].as_array().unwrap();
    assert!(
        nodes.is_empty(),
        "no nodes should be visible at clearance 1"
    );
}

// ── export_gql via SecureGraphRef filters by clearance ───────────────────────

#[test]
fn export_gql_via_secure_graph_ref_filters_by_clearance() {
    let mut g = Graph::new();
    // Node at clearance 1.
    {
        let mut sg = SecureGraph::new(&mut g, clearance(1));
        import_nodes_csv(&mut sg, "label,name\nPerson,Alice\n").unwrap();
    }
    // Node at clearance 5.
    {
        let mut sg = SecureGraph::new(&mut g, clearance(5));
        import_nodes_csv(&mut sg, "label,name\nPerson,Bob\n").unwrap();
    }
    // Export at clearance 1 — only Alice.
    let sg = SecureGraphRef::new(&g, clearance(1));
    let gql = export_gql(&sg).unwrap();
    assert_eq!(
        gql.lines().filter(|l| l.starts_with("CREATE")).count(),
        1,
        "only 1 CREATE statement at clearance 1"
    );
}
