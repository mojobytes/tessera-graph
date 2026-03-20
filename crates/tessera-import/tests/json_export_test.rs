// Copyright 2026 BelowZero Security OU. All rights reserved.

use tessera_graph::{Graph, Property};
use tessera_import::error::ExportError;
use tessera_import::json::export_json;

fn empty_graph() -> Graph {
    Graph::new()
}

fn graph_with_alice() -> Graph {
    let mut g = Graph::new();
    let props: tessera_graph::Properties = [
        ("name".to_owned(), Property::String("Alice".to_owned())),
        ("age".to_owned(), Property::I64(30)),
    ]
    .into_iter()
    .collect();
    g.add_node("Person", props).unwrap();
    g
}

#[test]
fn export_json_empty_graph_valid_structure() {
    let g = empty_graph();
    let json_str = export_json(&g).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();
    assert!(parsed["nodes"].is_array());
    assert!(parsed["edges"].is_array());
    assert_eq!(parsed["nodes"].as_array().unwrap().len(), 0);
}

#[test]
fn export_json_single_node_label() {
    let g = graph_with_alice();
    let json_str = export_json(&g).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();
    let nodes = parsed["nodes"].as_array().unwrap();
    assert_eq!(nodes.len(), 1);
    assert_eq!(nodes[0]["label"].as_str().unwrap(), "Person");
}

#[test]
fn export_json_node_properties() {
    let g = graph_with_alice();
    let json_str = export_json(&g).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();
    let props = &parsed["nodes"][0]["properties"];
    assert_eq!(props["name"].as_str().unwrap(), "Alice");
    assert_eq!(props["age"].as_i64().unwrap(), 30);
}

#[test]
fn export_json_edge_present() {
    let mut g = graph_with_alice();
    let props_bob: tessera_graph::Properties =
        std::iter::once(("name".to_owned(), Property::String("Bob".to_owned()))).collect();
    g.add_node("Person", props_bob).unwrap();
    let ids = {
        let mut v = g.node_ids();
        v.sort_unstable_by_key(|id| id.as_u64());
        v
    };
    g.add_edge("KNOWS", ids[0], ids[1], tessera_graph::Properties::new())
        .unwrap();
    let json_str = export_json(&g).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();
    let edges = parsed["edges"].as_array().unwrap();
    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0]["label"].as_str().unwrap(), "KNOWS");
}

#[test]
fn export_json_edge_source_target_ids() {
    let mut g = graph_with_alice();
    let props_bob: tessera_graph::Properties =
        std::iter::once(("name".to_owned(), Property::String("Bob".to_owned()))).collect();
    g.add_node("Person", props_bob).unwrap();
    let ids = {
        let mut v = g.node_ids();
        v.sort_unstable_by_key(|id| id.as_u64());
        v
    };
    g.add_edge("KNOWS", ids[0], ids[1], tessera_graph::Properties::new())
        .unwrap();
    let json_str = export_json(&g).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();
    let edge = &parsed["edges"][0];
    assert_eq!(edge["source_id"].as_u64().unwrap(), ids[0].as_u64());
    assert_eq!(edge["target_id"].as_u64().unwrap(), ids[1].as_u64());
}

#[test]
fn export_json_bool_property_serialized_correctly() {
    let mut g = empty_graph();
    let props: tessera_graph::Properties =
        std::iter::once(("active".to_owned(), Property::Bool(true))).collect();
    g.add_node("User", props).unwrap();
    let json_str = export_json(&g).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();
    assert!(
        parsed["nodes"][0]["properties"]["active"]
            .as_bool()
            .unwrap()
    );
}

#[test]
fn export_json_bytes_property_returns_error() {
    let mut g = Graph::new();
    let props: tessera_graph::Properties =
        std::iter::once(("blob".to_owned(), Property::Bytes(vec![0xFF, 0x00]))).collect();
    g.add_node("Blob", props).unwrap();
    let result = export_json(&g);
    assert!(
        matches!(result, Err(ExportError::UnsupportedType { .. })),
        "expected UnsupportedType error, got: {result:?}"
    );
}

#[test]
fn export_json_output_is_pretty_printed() {
    let g = graph_with_alice();
    let json_str = export_json(&g).unwrap();
    // Pretty-printed JSON contains newlines and indentation.
    assert!(
        json_str.contains('\n'),
        "Expected pretty-printed JSON, got: {json_str}"
    );
    assert!(
        json_str.contains("  "),
        "Expected indented JSON, got: {json_str}"
    );
}
