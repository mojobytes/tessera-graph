// SPDX-License-Identifier: MIT

//! Integration tests for variable-length path GQL queries.

use ermya_graph::{Graph, props};

use crate::helpers::graph_builders::build_chain;

fn execute_query(graph: &Graph, query_str: &str) -> Vec<ermya_graph::gql::GqlRow> {
    let query = ermya_graph::gql::parse(query_str).unwrap();
    ermya_graph::gql::execute(graph, &query, 0).unwrap()
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[test]
fn var_len_single_hop_matches_direct_edge() {
    let mut g = Graph::new();
    let a = g.add_node("P", props! {}).unwrap();
    let b = g.add_node("P", props! {}).unwrap();
    g.add_edge("KNOWS", a, b, props! {}).unwrap();

    let rows = execute_query(&g, "MATCH (a)-[*1..1]->(b) RETURN id(a), id(b)");
    assert_eq!(rows.len(), 1);
}

#[test]
fn var_len_two_hops_matches_chain() {
    let mut g = Graph::new();
    let a = g.add_node("P", props! {}).unwrap();
    let b = g.add_node("P", props! {}).unwrap();
    let c = g.add_node("P", props! {}).unwrap();
    g.add_edge("KNOWS", a, b, props! {}).unwrap();
    g.add_edge("KNOWS", b, c, props! {}).unwrap();

    let rows = execute_query(&g, "MATCH (a)-[*1..2]->(b) RETURN DISTINCT id(b)");
    // a→b (1 hop), a→c (2 hops), b→c (1 hop) — distinct end IDs are b and c
    assert_eq!(
        rows.len(),
        2,
        "expected exactly 2 distinct destinations, got {rows:?}"
    );
}

#[test]
#[allow(clippy::many_single_char_names)] // allow: test fixture
fn var_len_upper_bound_respected() {
    let mut g = Graph::new();
    let a = g.add_node("Start", props! {}).unwrap();
    let b = g.add_node("N", props! {}).unwrap();
    let c = g.add_node("N", props! {}).unwrap();
    let d = g.add_node("N", props! {}).unwrap();
    g.add_edge("R", a, b, props! {}).unwrap();
    g.add_edge("R", b, c, props! {}).unwrap();
    g.add_edge("R", c, d, props! {}).unwrap();

    let rows = execute_query(&g, "MATCH (a:Start)-[*1..2]->(b) RETURN DISTINCT id(b)");
    // a→b (1 hop), a→c (2 hops). D is 3 hops — excluded.
    assert_eq!(rows.len(), 2, "should reach B and C only, got {rows:?}");
}

#[test]
fn var_len_min_bound_respected() {
    let mut g = Graph::new();
    let a = g.add_node("Start", props! {}).unwrap();
    let b = g.add_node("N", props! {}).unwrap();
    let c = g.add_node("N", props! {}).unwrap();
    g.add_edge("R", a, b, props! {}).unwrap();
    g.add_edge("R", b, c, props! {}).unwrap();

    let rows = execute_query(&g, "MATCH (a:Start)-[*2..2]->(b) RETURN DISTINCT id(b)");
    // Only C (2 hops from a). B is excluded (1 hop).
    assert_eq!(rows.len(), 1);
}

#[test]
fn var_len_unbounded_finds_all() {
    let mut g = Graph::new();
    build_chain(&mut g, "N", "R", 5);

    let rows = execute_query(&g, "MATCH (a:N)-[*]->(b) RETURN DISTINCT id(b)");
    // 5-node chain with [*] (min=0): each node reaches itself (depth 0) plus
    // all subsequent nodes. Distinct reachable IDs: all 5 nodes.
    assert_eq!(
        rows.len(),
        5,
        "expected all 5 nodes (min=0 includes self), got {rows:?}"
    );
}

#[test]
#[allow(clippy::many_single_char_names)] // allow: test fixture
fn var_len_with_label_filter() {
    let mut g = Graph::new();
    let a = g.add_node("P", props! {}).unwrap();
    let b = g.add_node("P", props! {}).unwrap();
    let c = g.add_node("P", props! {}).unwrap();
    let d = g.add_node("P", props! {}).unwrap();
    g.add_edge("KNOWS", a, b, props! {}).unwrap();
    g.add_edge("KNOWS", b, c, props! {}).unwrap();
    g.add_edge("LIKES", a, d, props! {}).unwrap();

    let rows = execute_query(&g, "MATCH (a:P)-[:KNOWS*1..2]->(b) RETURN DISTINCT id(b)");
    let ids: std::collections::HashSet<_> = rows
        .iter()
        .filter_map(|r| {
            if let Some(ermya_graph::gql::GqlValue::Int(id)) = r.get("id(b)") {
                Some(*id)
            } else {
                None
            }
        })
        .collect();
    #[allow(clippy::cast_possible_wrap)]
    {
        assert!(ids.contains(&(b.as_u64() as i64)));
        assert!(ids.contains(&(c.as_u64() as i64)));
        assert!(!ids.contains(&(d.as_u64() as i64)));
    }
}

#[test]
fn var_len_no_match_returns_empty() {
    let mut g = Graph::new();
    let a = g.add_node("P", props! {}).unwrap();
    let b = g.add_node("P", props! {}).unwrap();
    g.add_edge("R", a, b, props! {}).unwrap();

    let rows = execute_query(&g, "MATCH (a:P)-[*3..5]->(b) RETURN id(b)");
    assert!(rows.is_empty());
}

#[test]
fn var_len_cycle_does_not_loop_forever() {
    let mut g = Graph::new();
    let a = g.add_node("P", props! {}).unwrap();
    let b = g.add_node("P", props! {}).unwrap();
    g.add_edge("R", a, b, props! {}).unwrap();
    g.add_edge("R", b, a, props! {}).unwrap();

    let rows = execute_query(&g, "MATCH (a:P)-[*1..10]->(b) RETURN DISTINCT id(b)");
    // A→B (from A), A (from B) = 2 distinct IDs.
    assert_eq!(
        rows.len(),
        2,
        "cycle should reach exactly 2 distinct nodes, got {rows:?}"
    );
}

#[test]
fn var_len_start_node_filtered() {
    let mut g = Graph::new();
    let alice = g.add_node("P", props! { "name" => "Alice" }).unwrap();
    let bob = g.add_node("P", props! { "name" => "Bob" }).unwrap();
    let carol = g.add_node("P", props! {}).unwrap();
    let dave = g.add_node("P", props! {}).unwrap();

    g.add_edge("KNOWS", alice, bob, props! {}).unwrap();
    g.add_edge("KNOWS", carol, dave, props! {}).unwrap();

    let rows = execute_query(&g, "MATCH (a:P {name: 'Alice'})-[*1..2]->(b) RETURN id(b)");
    assert_eq!(rows.len(), 1);
    #[allow(clippy::cast_possible_wrap)]
    if let Some(ermya_graph::gql::GqlValue::Int(id)) = rows[0].get("id(b)") {
        assert_eq!(*id, bob.as_u64() as i64);
    } else {
        panic!("expected Int id");
    }
}

#[test]
#[allow(clippy::many_single_char_names)] // allow: test fixture
fn var_len_bfs_no_duplicate_results_diamond_graph() {
    // Diamond: A→B, A→C, B→D, C→D
    // D reachable from A via two paths — must appear exactly once with DISTINCT.
    let mut g = Graph::new();
    let a = g.add_node("S", props! {}).unwrap();
    let b = g.add_node("N", props! {}).unwrap();
    let c = g.add_node("N", props! {}).unwrap();
    let d = g.add_node("N", props! {}).unwrap();
    g.add_edge("R", a, b, props! {}).unwrap();
    g.add_edge("R", a, c, props! {}).unwrap();
    g.add_edge("R", b, d, props! {}).unwrap();
    g.add_edge("R", c, d, props! {}).unwrap();

    let rows = execute_query(&g, "MATCH (a:S)-[*1..2]->(b) RETURN DISTINCT id(b)");
    // From A: B(1), C(1), D(2) = 3 distinct destinations.
    assert_eq!(rows.len(), 3, "diamond: expected B, C, D — got {rows:?}");
}

#[test]
fn var_len_min_zero_emits_start_node() {
    // [*0..2] means: include start node (0 hops) plus reachable within 2 hops.
    let mut g = Graph::new();
    let a = g.add_node("S", props! {}).unwrap();
    let b = g.add_node("N", props! {}).unwrap();
    g.add_edge("R", a, b, props! {}).unwrap();

    let rows = execute_query(&g, "MATCH (a:S)-[*0..2]->(b) RETURN id(a), id(b)");
    // Row 1: a→a (0 hops), Row 2: a→b (1 hop) = 2 rows from A as start.
    assert_eq!(
        rows.len(),
        2,
        "min=0 should include start node self-binding, got {rows:?}"
    );
}

#[test]
#[allow(clippy::many_single_char_names)] // allow: test fixture
fn mixed_fixed_then_variable_hop() {
    // A -[:STEP]-> B -[:HOP]-> C -[:HOP]-> D -[:HOP]-> E
    let mut g = Graph::new();
    let a = g.add_node("S", props! {}).unwrap();
    let b = g.add_node("M", props! {}).unwrap();
    let c = g.add_node("M", props! {}).unwrap();
    let d = g.add_node("M", props! {}).unwrap();
    let e = g.add_node("M", props! {}).unwrap();
    g.add_edge("STEP", a, b, props! {}).unwrap();
    g.add_edge("HOP", b, c, props! {}).unwrap();
    g.add_edge("HOP", c, d, props! {}).unwrap();
    g.add_edge("HOP", d, e, props! {}).unwrap();

    let rows = execute_query(
        &g,
        "MATCH (a:S)-[:STEP]->(b)-[:HOP*1..2]->(c) RETURN DISTINCT id(c)",
    );
    // From A: fixed -> B; then from B: HOP*1..2 -> C(1), D(2).
    assert_eq!(rows.len(), 2, "expected C and D only, got {rows:?}");
}
