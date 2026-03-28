//! Integration tests for Phase 1.5.3: GQL Mutations (enterprise).
//!
//! Each test runs the full pipeline:
//! `gql::parse_statement(query_string) → execute_mut(&mut graph, &stmt) → assert graph state`

use tessera_graph::{GqlMutationResult, GqlStatement, Graph, gql, props};
use tessera_storage_enterprise::gql::execute_mut;

// ── Helper ───────────────────────────────────────────────────────────────────

fn run_mutation(graph: &mut Graph, query: &str) -> tessera_graph::Result<GqlMutationResult> {
    let stmt = gql::parse_statement(query)?;
    let ms = stmt.as_mutation().expect("expected a mutation statement");
    execute_mut(graph, &ms)
}

// ── CREATE node ──────────────────────────────────────────────────────────────

#[test]
fn create_node_persists_label_and_properties() {
    let mut g = Graph::new();
    run_mutation(&mut g, "CREATE (n:Person {name: 'Alice', age: 30})").unwrap();
    let ids = g.nodes_by_label("Person");
    assert_eq!(ids.len(), 1);
    let node = g.node(ids[0]).unwrap();
    assert_eq!(node.label(), "Person");
    assert_eq!(
        node.properties().get("name").unwrap().as_str(),
        Some("Alice")
    );
    assert_eq!(node.properties().get("age").unwrap().as_i64(), Some(30));
}

#[test]
fn create_multiple_nodes_separate_statements() {
    let mut g = Graph::new();
    run_mutation(&mut g, "CREATE (:Person {name: 'Alice'})").unwrap();
    run_mutation(&mut g, "CREATE (:Person {name: 'Bob'})").unwrap();
    assert_eq!(g.node_count(), 2);
}

// ── CREATE edge ──────────────────────────────────────────────────────────────

#[test]
fn create_inline_edge_produces_two_nodes_and_one_edge() {
    let mut g = Graph::new();
    let r = run_mutation(
        &mut g,
        "CREATE (a:Person {name: 'Alice'})-[:KNOWS]->(b:Person {name: 'Bob'})",
    )
    .unwrap();
    assert_eq!(r.nodes_created, 2);
    assert_eq!(r.edges_created, 1);
    assert_eq!(g.node_count(), 2);
    assert_eq!(g.edge_count(), 1);
    let edge_ids = g.edges_by_label("KNOWS");
    assert_eq!(edge_ids.len(), 1);
    let edge = g.edge(edge_ids[0]).unwrap();
    let source = g.node(edge.source()).unwrap();
    let target = g.node(edge.target()).unwrap();
    assert_eq!(
        source.properties().get("name").unwrap().as_str(),
        Some("Alice")
    );
    assert_eq!(
        target.properties().get("name").unwrap().as_str(),
        Some("Bob")
    );
}

#[test]
fn create_edge_with_properties() {
    let mut g = Graph::new();
    run_mutation(
        &mut g,
        "CREATE (a:Person {name: 'Alice'})-[:KNOWS {since: 2020}]->(b:Person {name: 'Bob'})",
    )
    .unwrap();
    let edge_ids = g.edges_by_label("KNOWS");
    let edge = g.edge(edge_ids[0]).unwrap();
    assert_eq!(edge.properties().get("since").unwrap().as_i64(), Some(2020));
}

// ── DELETE ───────────────────────────────────────────────────────────────────

#[test]
fn delete_isolated_node() {
    let mut g = Graph::new();
    g.add_node("Temp", props! {}).unwrap();
    run_mutation(&mut g, "MATCH (n:Temp) DELETE n").unwrap();
    assert_eq!(g.node_count(), 0);
}

#[test]
fn delete_node_with_edges_requires_detach() {
    let mut g = Graph::new();
    let a = g.add_node("Person", props! { "name" => "Alice" }).unwrap();
    let b = g.add_node("Person", props! { "name" => "Bob" }).unwrap();
    g.add_edge("KNOWS", a, b, props! {}).unwrap();
    let err = run_mutation(&mut g, "MATCH (n:Person {name: 'Alice'}) DELETE n").unwrap_err();
    assert!(err.to_string().contains("DETACH"));
    assert_eq!(g.node_count(), 2); // unchanged
}

#[test]
fn detach_delete_removes_node_and_incident_edges() {
    let mut g = Graph::new();
    let a = g.add_node("Person", props! { "name" => "Alice" }).unwrap();
    let b = g.add_node("Person", props! { "name" => "Bob" }).unwrap();
    g.add_edge("KNOWS", a, b, props! {}).unwrap();
    g.add_edge("LIKES", b, a, props! {}).unwrap();
    run_mutation(&mut g, "MATCH (n:Person {name: 'Alice'}) DETACH DELETE n").unwrap();
    assert_eq!(g.node_count(), 1);
    assert_eq!(g.edge_count(), 0);
}

#[test]
fn detach_delete_all_nodes_of_label() {
    let mut g = Graph::new();
    g.add_node("Temp", props! {}).unwrap();
    g.add_node("Temp", props! {}).unwrap();
    g.add_node("Keep", props! {}).unwrap();
    run_mutation(&mut g, "MATCH (n:Temp) DETACH DELETE n").unwrap();
    assert_eq!(g.node_count(), 1);
    assert_eq!(g.nodes_by_label("Keep").len(), 1);
}

// ── SET ───────────────────────────────────────────────────────────────────────

#[test]
fn set_updates_property_on_matched_node() {
    let mut g = Graph::new();
    let id = g
        .add_node("Person", props! { "name" => "Alice", "age" => 25_i64 })
        .unwrap();
    run_mutation(&mut g, "MATCH (n:Person {name: 'Alice'}) SET n.age = 26").unwrap();
    let node = g.node(id).unwrap();
    assert_eq!(node.properties().get("age").unwrap().as_i64(), Some(26));
    assert_eq!(
        node.properties().get("name").unwrap().as_str(),
        Some("Alice")
    );
}

#[test]
fn set_adds_new_property_to_node() {
    let mut g = Graph::new();
    let id = g.add_node("Person", props! { "name" => "Bob" }).unwrap();
    run_mutation(&mut g, "MATCH (n:Person {name: 'Bob'}) SET n.active = true").unwrap();
    let node = g.node(id).unwrap();
    assert_eq!(
        node.properties().get("active").unwrap().as_bool(),
        Some(true)
    );
}

#[test]
fn set_applies_to_all_matched_nodes() {
    let mut g = Graph::new();
    g.add_node("Person", props! { "name" => "A" }).unwrap();
    g.add_node("Person", props! { "name" => "B" }).unwrap();
    run_mutation(&mut g, "MATCH (n:Person) SET n.active = false").unwrap();
    for id in g.nodes_by_label("Person") {
        let node = g.node(id).unwrap();
        assert_eq!(
            node.properties().get("active").unwrap().as_bool(),
            Some(false)
        );
    }
}

// ── MERGE ─────────────────────────────────────────────────────────────────────

#[test]
fn merge_creates_when_not_found() {
    let mut g = Graph::new();
    let r = run_mutation(&mut g, "MERGE (n:Config {key: 'theme', value: 'dark'})").unwrap();
    assert_eq!(r.nodes_created, 1);
    assert_eq!(g.node_count(), 1);
}

#[test]
fn merge_finds_existing_node() {
    let mut g = Graph::new();
    g.add_node("Config", props! { "key" => "theme", "value" => "dark" })
        .unwrap();
    let r = run_mutation(&mut g, "MERGE (n:Config {key: 'theme', value: 'dark'})").unwrap();
    assert_eq!(r.nodes_created, 0);
    assert_eq!(g.node_count(), 1);
}

#[test]
fn merge_is_idempotent_repeated_calls() {
    let mut g = Graph::new();
    for _ in 0..3 {
        run_mutation(&mut g, "MERGE (n:Singleton {key: 'x'})").unwrap();
    }
    assert_eq!(g.node_count(), 1);
}

#[test]
fn merge_different_props_creates_different_nodes() {
    let mut g = Graph::new();
    run_mutation(&mut g, "MERGE (n:Person {name: 'Alice'})").unwrap();
    run_mutation(&mut g, "MERGE (n:Person {name: 'Bob'})").unwrap();
    assert_eq!(g.node_count(), 2);
}

// ── Backward compatibility: read queries still work via parse_statement ────────

#[test]
fn read_query_via_parse_statement_still_works() {
    let mut g = Graph::new();
    g.add_node("Person", props! { "name" => "Alice" }).unwrap();
    let stmt = gql::parse_statement("MATCH (a:Person) RETURN a.name").unwrap();
    assert!(matches!(stmt, GqlStatement::Query(_)));
    // Original parse() + execute() path still works unchanged.
    let q = gql::parse("MATCH (a:Person) RETURN a.name").unwrap();
    let rows = gql::execute(&g, &q).unwrap();
    assert_eq!(rows.len(), 1);
}

// ── Phase 1.1: set_clause combined with mutation guard ────────────────────────

#[test]
fn set_clause_combined_with_mutation_returns_error() {
    use tessera_graph::gql::{
        CreateClause, Expr, Literal, MutationClause, MutationStatement, SetAssignment, SetClause,
    };
    let stmt = MutationStatement {
        match_clause: None,
        set_clause: Some(SetClause {
            assignments: vec![SetAssignment {
                var: "n".into(),
                prop: "active".into(),
                value: Expr::Literal(Literal::Bool(true)),
            }],
        }),
        mutation: MutationClause::Create(CreateClause { patterns: vec![] }),
    };
    let mut g = Graph::new();
    let err = execute_mut(&mut g, &stmt).unwrap_err();
    assert!(
        err.to_string().contains("set_clause"),
        "error must mention set_clause: {err}"
    );
}

// ── Phase 2.1: DELETE error includes relationship counts ──────────────────────

#[test]
fn delete_with_edges_error_includes_relationship_counts() {
    let mut g = Graph::new();
    let a = g.add_node("Person", props! { "name" => "Alice" }).unwrap();
    let b = g.add_node("Person", props! { "name" => "Bob" }).unwrap();
    g.add_edge("KNOWS", a, b, props! {}).unwrap();
    let err = run_mutation(&mut g, "MATCH (n:Person {name: 'Alice'}) DELETE n").unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("DETACH"), "must mention DETACH: {msg}");
    assert!(msg.contains("outgoing"), "must mention edge counts: {msg}");
}

// ── Phase 2.2: SET multiple properties counts each assignment ─────────────────

#[test]
fn set_multiple_properties_counts_each_assignment() {
    let mut g = Graph::new();
    g.add_node("Person", props! { "name" => "Alice", "age" => 25_i64 })
        .unwrap();
    let r = run_mutation(
        &mut g,
        "MATCH (n:Person {name: 'Alice'}) SET n.age = 30, n.city = 'Berlin'",
    )
    .unwrap();
    assert_eq!(r.properties_set, 2, "two property assignments");
    let ids = g.nodes_by_label("Person");
    let node = g.node(ids[0]).unwrap();
    assert_eq!(node.properties().get("age").unwrap().as_i64(), Some(30));
    assert_eq!(
        node.properties().get("city").unwrap().as_str(),
        Some("Berlin")
    );
}

// ── Phase 3.1: Unbound variable error paths ───────────────────────────────────

#[test]
fn delete_unbound_variable_returns_error() {
    use tessera_graph::gql::{DeleteClause, MutationClause, MutationStatement};
    let stmt = MutationStatement {
        match_clause: None,
        set_clause: None,
        mutation: MutationClause::Delete(DeleteClause {
            vars: vec!["z".into()],
            detach: false,
        }),
    };
    let mut g = Graph::new();
    let err = execute_mut(&mut g, &stmt).unwrap_err();
    assert!(
        err.to_string().contains("unbound variable"),
        "must report unbound variable: {err}"
    );
}

#[test]
fn set_unbound_variable_returns_error() {
    use tessera_graph::gql::{
        Expr, Literal, MutationClause, MutationStatement, SetAssignment, SetClause,
    };
    let stmt = MutationStatement {
        match_clause: None,
        set_clause: None,
        mutation: MutationClause::Set(SetClause {
            assignments: vec![SetAssignment {
                var: "x".into(),
                prop: "foo".into(),
                value: Expr::Literal(Literal::Int(1)),
            }],
        }),
    };
    let mut g = Graph::new();
    let err = execute_mut(&mut g, &stmt).unwrap_err();
    assert!(
        err.to_string().contains("unbound variable"),
        "must report unbound variable: {err}"
    );
}

// ── MATCH...CREATE edge between existing nodes ────────────────────────────────

#[test]
fn match_create_edge_between_existing_nodes() {
    let mut g = Graph::new();
    run_mutation(&mut g, "CREATE (:Person {name: 'Alice'})").unwrap();
    run_mutation(&mut g, "CREATE (:Person {name: 'Bob'})").unwrap();
    assert_eq!(g.node_count(), 2);
    assert_eq!(g.edge_count(), 0);

    let result = run_mutation(
        &mut g,
        "MATCH (a:Person {name: 'Alice'}), (b:Person {name: 'Bob'}) \
         CREATE (a)-[:KNOWS]->(b)",
    )
    .unwrap();

    assert_eq!(result.nodes_created, 0, "no new nodes should be created");
    assert_eq!(result.edges_created, 1);
    assert_eq!(g.edge_count(), 1);
}

#[test]
fn match_create_edge_with_properties() {
    let mut g = Graph::new();
    run_mutation(&mut g, "CREATE (:Person {name: 'Alice'})").unwrap();
    run_mutation(&mut g, "CREATE (:Person {name: 'Bob'})").unwrap();

    let result = run_mutation(
        &mut g,
        "MATCH (a:Person {name: 'Alice'}), (b:Person {name: 'Bob'}) \
         CREATE (a)-[:KNOWS {since: 2024}]->(b)",
    )
    .unwrap();

    assert_eq!(result.edges_created, 1);

    let alice_id = g.nodes_by_label("Person")[0];
    let edges = g.outgoing_edges(alice_id).unwrap();
    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0].label(), "KNOWS");
    assert_eq!(edges[0].properties().get("since").unwrap().as_i64(), Some(2024));
}

#[test]
fn match_create_edge_unbound_var_is_error() {
    let mut g = Graph::new();
    run_mutation(&mut g, "CREATE (:Person {name: 'Alice'})").unwrap();

    let err = run_mutation(
        &mut g,
        "MATCH (a:Person {name: 'Alice'}) CREATE (a)-[:KNOWS]->(b)",
    );
    assert!(err.is_err(), "unbound variable 'b' should cause error");
}

// ── Enterprise-only features still rejected ───────────────────────────────────

#[test]
fn parse_statement_rejects_multi_label_node() {
    let err = gql::parse_statement("MATCH (a:Foo:Bar) RETURN a").unwrap_err();
    assert!(matches!(err, tessera_graph::Error::GqlUnsupported(_)));
}

#[test]
fn parse_statement_rejects_variable_length_path() {
    let err = gql::parse_statement("MATCH (a)-[*]->(b) RETURN a").unwrap_err();
    assert!(matches!(err, tessera_graph::Error::GqlUnsupported(_)));
}
