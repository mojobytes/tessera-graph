// Copyright 2026 BelowZero Security OU. All rights reserved.

use std::collections::HashMap;

use tessera_graph::{Graph, Property};
use tessera_graph_import::csv::{export_edges_csv, export_nodes_csv};
use tessera_graph_import::error::ExportError;

fn empty_graph() -> Graph {
    Graph::new()
}

fn graph_with_alice_and_bob() -> Graph {
    let mut g = Graph::new();
    let props_alice: tessera_graph::Properties = [
        ("name".to_owned(), Property::String("Alice".to_owned())),
        ("age".to_owned(), Property::I64(30)),
    ]
    .into_iter()
    .collect();
    let props_bob: tessera_graph::Properties = [
        ("name".to_owned(), Property::String("Bob".to_owned())),
        ("age".to_owned(), Property::I64(25)),
    ]
    .into_iter()
    .collect();
    g.add_node("Person", props_alice).unwrap();
    g.add_node("Person", props_bob).unwrap();
    g
}

// ── Node export ──────────────────────────────────────────────────────────────

#[test]
fn export_nodes_empty_graph_has_header() {
    let g = empty_graph();
    let csv = export_nodes_csv(&g).unwrap();
    assert!(csv.starts_with("label"), "got: {csv}");
}

#[test]
fn export_nodes_contains_label_column() {
    let g = graph_with_alice_and_bob();
    let csv = export_nodes_csv(&g).unwrap();
    assert!(csv.contains("Person"), "got: {csv}");
}

#[test]
fn export_nodes_header_includes_prop_keys_sorted() {
    let g = graph_with_alice_and_bob();
    let csv = export_nodes_csv(&g).unwrap();
    let header = csv.lines().next().unwrap();
    // "age" comes before "name" alphabetically
    let age_pos = header.find("age").unwrap();
    let name_pos = header.find("name").unwrap();
    assert!(age_pos < name_pos, "header: {header}");
}

#[test]
fn export_nodes_values_present() {
    let g = graph_with_alice_and_bob();
    let csv = export_nodes_csv(&g).unwrap();
    assert!(csv.contains("Alice"), "got: {csv}");
    assert!(csv.contains("Bob"), "got: {csv}");
    assert!(csv.contains("30"), "got: {csv}");
    assert!(csv.contains("25"), "got: {csv}");
}

#[test]
fn export_nodes_value_with_comma_is_quoted() {
    let mut g = empty_graph();
    let props: tessera_graph::Properties = std::iter::once((
        "desc".to_owned(),
        Property::String("hello, world".to_owned()),
    ))
    .collect();
    g.add_node("Thing", props).unwrap();
    let csv = export_nodes_csv(&g).unwrap();
    assert!(
        csv.contains('"'),
        "value with comma should be quoted, got: {csv}"
    );
}

#[test]
fn export_nodes_row_count_matches_node_count() {
    let g = graph_with_alice_and_bob();
    let csv = export_nodes_csv(&g).unwrap();
    // header + 2 data rows + possible trailing newline
    assert_eq!(csv.lines().skip(1).count(), 2);
}

// ── Edge export ──────────────────────────────────────────────────────────────

#[test]
fn export_edges_empty_graph_has_header() {
    let g = empty_graph();
    let csv = export_edges_csv(&g).unwrap();
    assert!(csv.starts_with("source_id"), "got: {csv}");
}

#[test]
fn export_edges_contains_rel_label() {
    let mut g = graph_with_alice_and_bob();
    let ids = g.node_ids();
    g.add_edge("KNOWS", ids[0], ids[1], tessera_graph::Properties::new())
        .unwrap();
    let csv = export_edges_csv(&g).unwrap();
    assert!(csv.contains("KNOWS"), "got: {csv}");
}

#[test]
fn export_nodes_csv_bytes_property_returns_error() {
    let mut g = Graph::new();
    let props: tessera_graph::Properties =
        std::iter::once(("data".to_owned(), Property::Bytes(vec![0xDE, 0xAD]))).collect();
    g.add_node("Thing", props).unwrap();
    let result = export_nodes_csv(&g);
    assert!(
        matches!(result, Err(ExportError::UnsupportedType { .. })),
        "expected UnsupportedType error, got: {result:?}"
    );
}

#[test]
fn export_edges_csv_bytes_property_returns_error() {
    let mut g = Graph::new();
    let props_a: tessera_graph::Properties = HashMap::new();
    let props_b: tessera_graph::Properties = HashMap::new();
    g.add_node("A", props_a).unwrap();
    g.add_node("B", props_b).unwrap();
    let ids = g.node_ids();
    let edge_props: tessera_graph::Properties =
        std::iter::once(("payload".to_owned(), Property::Bytes(vec![1, 2, 3]))).collect();
    g.add_edge("LINK", ids[0], ids[1], edge_props).unwrap();
    let result = export_edges_csv(&g);
    assert!(
        matches!(result, Err(ExportError::UnsupportedType { .. })),
        "expected UnsupportedType error, got: {result:?}"
    );
}

#[test]
fn export_edges_contains_source_and_target_ids() {
    let mut g = graph_with_alice_and_bob();
    let mut ids = g.node_ids();
    ids.sort_unstable_by_key(|id| id.as_u64());
    g.add_edge("KNOWS", ids[0], ids[1], tessera_graph::Properties::new())
        .unwrap();
    let csv = export_edges_csv(&g).unwrap();
    let data_line = csv.lines().nth(1).unwrap();
    assert!(
        data_line.contains(&ids[0].as_u64().to_string()),
        "got: {data_line}"
    );
    assert!(
        data_line.contains(&ids[1].as_u64().to_string()),
        "got: {data_line}"
    );
}
