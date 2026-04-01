// Copyright 2026 BelowZero Security OU. All rights reserved.

use tessera_graph::{Graph, Property};
use tessera_graph_import::error::ImportError;
use tessera_graph_import::gql_import::{GqlImportSummary, import_gql};

fn empty_graph() -> Graph {
    Graph::new()
}

#[test]
fn import_gql_create_single_node() {
    let mut g = empty_graph();
    let gql = "CREATE (:Person {name: 'Alice'})";
    let summary = import_gql(&mut g, gql).unwrap();
    assert_eq!(summary.statements_executed, 1);
    assert_eq!(summary.nodes_created, 1);
    assert_eq!(g.node_count(), 1);
}

#[test]
fn import_gql_create_multiple_nodes() {
    let mut g = empty_graph();
    let gql = "CREATE (:Person {name: 'Alice'})\nCREATE (:Person {name: 'Bob'})\n";
    let summary = import_gql(&mut g, gql).unwrap();
    assert_eq!(summary.statements_executed, 2);
    assert_eq!(summary.nodes_created, 2);
    assert_eq!(g.node_count(), 2);
}

#[test]
fn import_gql_skips_blank_lines() {
    let mut g = empty_graph();
    let gql = "CREATE (:Thing {x: 1})\n\n\nCREATE (:Thing {x: 2})\n";
    let summary = import_gql(&mut g, gql).unwrap();
    assert_eq!(summary.nodes_created, 2);
}

#[test]
fn import_gql_skips_slash_slash_comments() {
    let mut g = empty_graph();
    let gql = "// this is a comment\nCREATE (:Node {v: 1})\n";
    let summary = import_gql(&mut g, gql).unwrap();
    assert_eq!(summary.statements_executed, 1);
    assert_eq!(summary.nodes_created, 1);
}

#[test]
fn import_gql_skips_dash_dash_comments() {
    let mut g = empty_graph();
    let gql = "-- this is a comment\nCREATE (:Node {v: 2})\n";
    let summary = import_gql(&mut g, gql).unwrap();
    assert_eq!(summary.statements_executed, 1);
    assert_eq!(summary.nodes_created, 1);
}

#[test]
fn import_gql_skips_read_only_queries() {
    let mut g = empty_graph();
    // MATCH ... RETURN is a read-only query; should be silently ignored.
    let gql = "MATCH (n:Person) RETURN n.name\n";
    let summary = import_gql(&mut g, gql).unwrap();
    assert_eq!(summary.statements_executed, 0);
    assert_eq!(g.node_count(), 0);
}

#[test]
fn import_gql_error_on_bad_statement() {
    let mut g = empty_graph();
    let gql = "THIS IS NOT GQL\n";
    let result = import_gql(&mut g, gql);
    assert!(matches!(
        result,
        Err(ImportError::GqlStatement { line: 1, .. })
    ));
}

#[test]
fn import_gql_accumulates_edge_count() {
    let mut g = empty_graph();
    // Create both nodes and the edge in a single CREATE statement.
    let gql = "CREATE (a:Person {name: 'A'})-[:KNOWS]->(b:Person {name: 'B'})\n";
    let summary = import_gql(&mut g, gql).unwrap();
    assert_eq!(summary.statements_executed, 1);
    assert_eq!(summary.nodes_created, 2);
    assert_eq!(summary.edges_created, 1);
    assert_eq!(g.edge_count(), 1);
}

#[test]
fn gql_import_summary_types_are_usize() {
    let s = GqlImportSummary::default();
    // If this compiles, the types are usize. If u64, the assignment below fails.
    let _: usize = s.nodes_created;
    let _: usize = s.edges_created;
}

#[test]
fn import_gql_node_has_correct_property() {
    let mut g = empty_graph();
    import_gql(&mut g, "CREATE (:City {name: 'Paris', pop: 2000000})").unwrap();
    let id = g.nodes_by_label("City")[0];
    let node = g.node(id).unwrap();
    assert_eq!(
        node.properties().get("name"),
        Some(&Property::String("Paris".to_owned()))
    );
    assert_eq!(
        node.properties().get("pop"),
        Some(&Property::I64(2_000_000))
    );
}
