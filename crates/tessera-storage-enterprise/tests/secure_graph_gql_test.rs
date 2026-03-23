// Copyright 2026 BelowZero Security OU. All rights reserved.

//! Integration tests: `execute_mut` through `SecureGraph`.

use std::collections::BTreeSet;
use tessera_auth::lbac::{Clearance, SecurityLabel, SecurityPolicy};
use tessera_graph::{gql, props, Graph};
use tessera_storage_enterprise::gql::execute_mut;
use tessera_storage_enterprise::lbac::SecureGraph;

fn comps(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|s| (*s).to_string()).collect()
}

fn clearance(level: u16, compartments: &[&str]) -> Clearance {
    Clearance::new(level, comps(compartments))
}

fn run_through_secure(
    g: &mut Graph,
    c: Clearance,
    query: &str,
) -> tessera_graph::Result<tessera_graph::GqlMutationResult> {
    let mut sg = SecureGraph::new(g, c);
    let stmt = gql::parse_statement(query).unwrap();
    let ms = stmt.as_mutation().expect("mutation expected");
    execute_mut(&mut sg, &ms)
}

#[test]
fn create_through_secure_graph_inherits_caller_clearance() {
    let mut g = Graph::new();
    run_through_secure(
        &mut g,
        clearance(0, &[]),
        "CREATE (n:Person {name: 'Alice'})",
    )
    .unwrap();
    let ids = g.nodes_by_label("Person");
    assert_eq!(ids.len(), 1);
    let raw = g.node(ids[0]).unwrap();
    let label = SecurityPolicy::extract_label(raw.properties());
    assert_eq!(label.level, 0);
    assert!(label.compartments.is_empty());
}

#[test]
fn delete_through_secure_graph_denied_when_level_insufficient() {
    let mut g = Graph::new();
    // Create a classified node directly (bypassing SecureGraph)
    let label = SecurityLabel::new(5, BTreeSet::new());
    let mut p = props! { "name" => "Bob" };
    SecurityPolicy::inject_label(&mut p, &label);
    g.add_node("Person", p).unwrap();
    // Try to delete with insufficient clearance — MATCH returns no matches for 'n',
    // so DELETE receives an unbound variable and returns an error.
    let result = run_through_secure(
        &mut g,
        clearance(3, &[]),
        "MATCH (n:Person {name: 'Bob'}) DETACH DELETE n",
    );
    assert!(result.is_err(), "DELETE with unbound variable should fail");
    assert_eq!(g.node_count(), 1, "node must still exist — it was invisible to MATCH");
}

#[test]
fn match_only_returns_nodes_visible_to_clearance() {
    let mut g = Graph::new();
    // Public node
    let mut p1 = props! { "name" => "Public" };
    SecurityPolicy::inject_label(&mut p1, &SecurityLabel::default());
    g.add_node("Person", p1).unwrap();
    // Classified node
    let label_secret = SecurityLabel::new(3, comps(&["SECRET"]));
    let mut p2 = props! { "name" => "Secret" };
    SecurityPolicy::inject_label(&mut p2, &label_secret);
    g.add_node("Person", p2).unwrap();
    // Query with low clearance — only public visible
    let sg = SecureGraph::new(&mut g, clearance(0, &[]));
    let q = gql::parse("MATCH (n:Person) RETURN n.name").unwrap();
    let rows = gql::execute(&sg, &q).unwrap();
    assert_eq!(rows.len(), 1);
}

#[test]
fn existing_execute_mut_on_plain_graph_still_works() {
    let mut g = Graph::new();
    let stmt = gql::parse_statement("CREATE (n:Person {name: 'Alice'})").unwrap();
    let ms = stmt.as_mutation().unwrap();
    execute_mut(&mut g, &ms).unwrap();
    assert_eq!(g.node_count(), 1);
}
