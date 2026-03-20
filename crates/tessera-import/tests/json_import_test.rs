// Copyright 2026 BelowZero Security OU. All rights reserved.

use tessera_graph::{Graph, Property};
use tessera_import::error::ImportError;
use tessera_import::json::import_json;

fn empty_graph() -> Graph {
    Graph::new()
}

// ── Node import ──────────────────────────────────────────────────────────────

#[test]
fn import_json_single_node() {
    let mut g = empty_graph();
    let json = r#"{"nodes":[{"label":"Person","properties":{"name":"Alice"}}],"edges":[]}"#;
    let summary = import_json(&mut g, json).unwrap();
    assert_eq!(summary.nodes_imported, 1);
    assert_eq!(summary.edges_imported, 0);
    assert_eq!(g.node_count(), 1);
}

#[test]
fn import_json_node_integer_property() {
    let mut g = empty_graph();
    let json = r#"{"nodes":[{"label":"Person","properties":{"age":30}}],"edges":[]}"#;
    import_json(&mut g, json).unwrap();
    let id = g.nodes_by_label("Person")[0];
    let node = g.node(id).unwrap();
    assert_eq!(node.properties().get("age"), Some(&Property::I64(30)));
}

#[test]
fn import_json_node_bool_property() {
    let mut g = empty_graph();
    let json = r#"{"nodes":[{"label":"User","properties":{"active":true}}],"edges":[]}"#;
    import_json(&mut g, json).unwrap();
    let id = g.nodes_by_label("User")[0];
    let node = g.node(id).unwrap();
    assert_eq!(node.properties().get("active"), Some(&Property::Bool(true)));
}

#[test]
fn import_json_multiple_nodes() {
    let mut g = empty_graph();
    let json = r#"{
        "nodes":[
            {"label":"Person","properties":{"name":"Alice"}},
            {"label":"Person","properties":{"name":"Bob"}}
        ],
        "edges":[]
    }"#;
    let summary = import_json(&mut g, json).unwrap();
    assert_eq!(summary.nodes_imported, 2);
    assert_eq!(g.node_count(), 2);
}

#[test]
fn import_json_edge_between_nodes() {
    let mut g = empty_graph();
    let json = r#"{
        "nodes":[
            {"label":"Person","properties":{"name":"Alice"}},
            {"label":"Person","properties":{"name":"Bob"}}
        ],
        "edges":[
            {
                "source":{"label":"Person","match":{"name":"Alice"}},
                "target":{"label":"Person","match":{"name":"Bob"}},
                "label":"KNOWS",
                "properties":{}
            }
        ]
    }"#;
    let summary = import_json(&mut g, json).unwrap();
    assert_eq!(summary.nodes_imported, 2);
    assert_eq!(summary.edges_imported, 1);
    assert_eq!(g.edge_count(), 1);
}

#[test]
fn import_json_edge_with_properties() {
    let mut g = empty_graph();
    let json = r#"{
        "nodes":[
            {"label":"Person","properties":{"name":"Alice"}},
            {"label":"Person","properties":{"name":"Bob"}}
        ],
        "edges":[
            {
                "source":{"label":"Person","match":{"name":"Alice"}},
                "target":{"label":"Person","match":{"name":"Bob"}},
                "label":"KNOWS",
                "properties":{"since":2020}
            }
        ]
    }"#;
    import_json(&mut g, json).unwrap();
    let ids = g.edges_by_label("KNOWS");
    let edge = g.edge(ids[0]).unwrap();
    assert_eq!(edge.properties().get("since"), Some(&Property::I64(2020)));
}

#[test]
fn import_json_error_invalid_json() {
    let mut g = empty_graph();
    let result = import_json(&mut g, "not-json");
    assert!(matches!(result, Err(ImportError::JsonInvalid(_))));
}

#[test]
fn import_json_error_missing_nodes_field() {
    let mut g = empty_graph();
    let json = r#"{"edges":[]}"#;
    let result = import_json(&mut g, json);
    assert!(matches!(result, Err(ImportError::JsonMissingField(_))));
}

#[test]
fn import_json_error_node_not_found_for_edge() {
    let mut g = empty_graph();
    let json = r#"{
        "nodes":[{"label":"Person","properties":{"name":"Alice"}}],
        "edges":[
            {
                "source":{"label":"Person","match":{"name":"Alice"}},
                "target":{"label":"Person","match":{"name":"NoOne"}},
                "label":"KNOWS",
                "properties":{}
            }
        ]
    }"#;
    let result = import_json(&mut g, json);
    assert!(matches!(
        result,
        Err(ImportError::NodeNotFoundForEdge { .. })
    ));
}
