// SPDX-License-Identifier: BSL-1.1

//! Unit tests for [`GraphAccessor`] — [`DefaultGraphAccessor`] over `Arc<RwLock<Graph>>`.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use ermya_graph::gql::GqlStatement;
use ermya_graph::{Graph, props};
use ermya_graph_config::QueryLanguage;
use ermya_graph_server::DefaultGraphAccessor;
use ermya_graph_server::graph_accessor::GraphAccessor;

fn make_accessor() -> (Arc<RwLock<Graph>>, DefaultGraphAccessor) {
    let graph = Arc::new(RwLock::new(Graph::new()));
    let accessor = DefaultGraphAccessor::new(Arc::clone(&graph));
    (graph, accessor)
}

fn parse_query(cypher: &str) -> GqlStatement {
    ermya_graph_cypher::parse_with_mode(cypher, QueryLanguage::CypherCompat).unwrap()
}

// ── Query execution ─────────────────────────────────────────────────────────

#[test]
fn query_on_empty_graph_returns_empty_rows() {
    let (_graph, accessor) = make_accessor();
    let stmt = parse_query("MATCH (n) RETURN n");
    let GqlStatement::Query(ref q) = stmt else {
        panic!("expected Query");
    };

    let rows = accessor.execute_query(q, HashMap::new(), 0, None).unwrap();
    assert!(rows.is_empty());
}

#[test]
fn query_returns_created_nodes() {
    let (graph, accessor) = make_accessor();

    // Insert a node directly via the graph API.
    {
        let mut g = graph.write().unwrap();
        g.add_node("Person", props! { "name" => "Alice" }).unwrap();
    }

    let stmt = parse_query("MATCH (n:Person) RETURN n.name");
    let GqlStatement::Query(ref q) = stmt else {
        panic!("expected Query");
    };

    let rows = accessor.execute_query(q, HashMap::new(), 0, None).unwrap();
    assert_eq!(rows.len(), 1, "expected 1 row");
}

#[test]
fn query_returns_multiple_rows() {
    let (graph, accessor) = make_accessor();

    {
        let mut g = graph.write().unwrap();
        g.add_node("Person", props! { "name" => "Alice" }).unwrap();
        g.add_node("Person", props! { "name" => "Bob" }).unwrap();
        g.add_node("Person", props! { "name" => "Carol" }).unwrap();
    }

    let stmt = parse_query("MATCH (n:Person) RETURN n.name");
    let GqlStatement::Query(ref q) = stmt else {
        panic!("expected Query");
    };

    let rows = accessor.execute_query(q, HashMap::new(), 0, None).unwrap();
    assert_eq!(rows.len(), 3, "expected 3 rows");
}

// ── Mutation execution ──────────────────────────────────────────────────────

#[test]
fn mutation_creates_node() {
    let (graph, accessor) = make_accessor();

    let stmt = parse_query("CREATE (:City {name: 'Madrid'})");
    let GqlStatement::Mutation(ref m) = stmt else {
        panic!("expected Mutation");
    };

    let (rows, stats) = accessor.execute_mutation(m, HashMap::new(), None).unwrap();
    assert!(rows.is_empty(), "bare CREATE returns no rows");
    assert_eq!(stats.nodes_created, 1);
    assert_eq!(stats.edges_created, 0);
    assert_eq!(stats.labels_added, 1, "the City label");

    // Verify node exists in the graph.
    let g = graph.read().unwrap();
    assert_eq!(g.node_count(), 1, "graph should contain 1 node");
}

#[test]
fn mutation_creates_multiple_nodes() {
    let (_graph, accessor) = make_accessor();

    let stmt = parse_query("CREATE (:A {x: 1}), (:B {x: 2})");
    let GqlStatement::Mutation(ref m) = stmt else {
        panic!("expected Mutation");
    };

    let (rows, stats) = accessor.execute_mutation(m, HashMap::new(), None).unwrap();
    assert!(rows.is_empty(), "bare CREATE returns no rows");
    assert_eq!(stats.nodes_created, 2);
    assert_eq!(stats.edges_created, 0);
    assert_eq!(stats.labels_added, 2, "labels A and B");
}

// ── Issue #45: DELETE / DETACH DELETE via the accessor ────────────────────────

#[test]
fn match_delete_removes_node() {
    let (graph, accessor) = make_accessor();
    {
        let mut g = graph.write().unwrap();
        g.add_node("Person", props! { "name" => "Alice" }).unwrap();
    }
    let stmt = parse_query("MATCH (n:Person) DELETE n");
    let GqlStatement::Mutation(ref m) = stmt else {
        panic!("expected Mutation");
    };
    let (_rows, stats) = accessor.execute_mutation(m, HashMap::new(), None).unwrap();
    assert_eq!(stats.nodes_deleted, 1);
    let g = graph.read().unwrap();
    assert_eq!(g.node_count(), 0);
}

#[test]
fn pipeline_delete_removes_node() {
    let (graph, accessor) = make_accessor();
    {
        let mut g = graph.write().unwrap();
        g.add_node("Person", props! { "name" => "Alice" }).unwrap();
    }
    let stmt = parse_query("MATCH (n:Person) WITH n DELETE n");
    let GqlStatement::Pipeline(ref pq) = stmt else {
        panic!("expected Pipeline");
    };
    let (_rows, stats) = accessor
        .execute_pipeline(pq, HashMap::new(), 0, None)
        .unwrap();
    assert_eq!(stats.nodes_deleted, 1);
    let g = graph.read().unwrap();
    assert_eq!(g.node_count(), 0);
}

#[test]
fn pipeline_delete_in_txn_removes_node_on_commit() {
    let (graph, accessor) = make_accessor();
    {
        let mut g = graph.write().unwrap();
        g.enable_mvcc();
        g.add_node("Person", props! { "name" => "Alice" }).unwrap();
    }
    let txn = accessor.begin_txn().unwrap();
    let stmt = parse_query("MATCH (n:Person) WITH n DELETE n");
    let GqlStatement::Pipeline(ref pq) = stmt else {
        panic!("expected Pipeline");
    };
    let (_rows, stats) = accessor
        .execute_pipeline_in_txn(txn, pq, HashMap::new(), 0, None)
        .unwrap();
    assert_eq!(stats.nodes_deleted, 1);
    // Before commit the node is still visible to a fresh auto-commit read.
    let read_stmt = parse_query("MATCH (n:Person) RETURN n");
    let GqlStatement::Query(ref q) = read_stmt else {
        panic!("expected Query");
    };
    let before = accessor.execute_query(q, HashMap::new(), 0, None).unwrap();
    assert_eq!(before.len(), 1, "visible before commit");
    accessor.commit_txn(txn).unwrap();
    // After commit the node is gone from the visible snapshot.
    let after = accessor.execute_query(q, HashMap::new(), 0, None).unwrap();
    assert_eq!(after.len(), 0, "gone after commit");
}

// ── Error handling ──────────────────────────────────────────────────────────

#[test]
fn query_after_lock_is_not_poisoned() {
    let (_graph, accessor) = make_accessor();

    // Two consecutive queries should work without lock issues.
    let stmt = parse_query("MATCH (n) RETURN n");
    let GqlStatement::Query(ref q) = stmt else {
        panic!("expected Query");
    };

    let r1 = accessor.execute_query(q, HashMap::new(), 0, None);
    let r2 = accessor.execute_query(q, HashMap::new(), 0, None);
    assert!(r1.is_ok());
    assert!(r2.is_ok());
}
