// SPDX-License-Identifier: MIT

//! Integration tests for the `shortestPath()` GQL function.

use tessera_graph::{Graph, props};

fn execute_query(graph: &Graph, query_str: &str) -> Vec<tessera_graph::gql::GqlRow> {
    let query = tessera_graph::gql::parse(query_str).unwrap();
    tessera_graph::gql::execute(graph, &query, 0).unwrap()
}

#[test]
fn shortest_path_two_connected_nodes_returns_list() {
    let mut g = Graph::new();
    let a = g.add_node("P", props! { "name" => "Alice" }).unwrap();
    let b = g.add_node("P", props! { "name" => "Bob" }).unwrap();
    g.add_edge("KNOWS", a, b, props! {}).unwrap();

    let rows = execute_query(
        &g,
        "MATCH (a:P {name: 'Alice'}), (b:P {name: 'Bob'}) RETURN shortestPath(a, b)",
    );
    assert_eq!(rows.len(), 1);
    let val = rows[0].get("shortestpath(a, b)").expect("column exists");
    match val {
        tessera_graph::gql::GqlValue::List(ids) => {
            assert_eq!(ids.len(), 2, "path should contain 2 nodes");
        }
        other => panic!("expected List, got {other:?}"),
    }
}

#[test]
fn shortest_path_chain_returns_intermediate_nodes() {
    let mut g = Graph::new();
    let a = g.add_node("P", props! { "name" => "A" }).unwrap();
    let b = g.add_node("P", props! { "name" => "B" }).unwrap();
    let c = g.add_node("P", props! { "name" => "C" }).unwrap();
    g.add_edge("R", a, b, props! {}).unwrap();
    g.add_edge("R", b, c, props! {}).unwrap();

    let rows = execute_query(
        &g,
        "MATCH (a:P {name: 'A'}), (c:P {name: 'C'}) RETURN shortestPath(a, c)",
    );
    assert_eq!(rows.len(), 1);
    let val = rows[0].get("shortestpath(a, c)").expect("column exists");
    match val {
        tessera_graph::gql::GqlValue::List(ids) => {
            assert_eq!(ids.len(), 3, "path A→B→C should have 3 nodes");
        }
        other => panic!("expected List, got {other:?}"),
    }
}

#[test]
fn shortest_path_unreachable_returns_null() {
    let mut g = Graph::new();
    let _a = g.add_node("P", props! { "name" => "A" }).unwrap();
    let _b = g.add_node("P", props! { "name" => "B" }).unwrap();
    // No edge between them

    let rows = execute_query(
        &g,
        "MATCH (a:P {name: 'A'}), (b:P {name: 'B'}) RETURN shortestPath(a, b)",
    );
    assert_eq!(rows.len(), 1);
    let val = rows[0].get("shortestpath(a, b)").expect("column exists");
    assert_eq!(*val, tessera_graph::gql::GqlValue::Null);
}

#[test]
fn shortest_path_same_node_returns_single_element_list() {
    let mut g = Graph::new();
    let _a = g.add_node("P", props! { "name" => "A" }).unwrap();

    let rows = execute_query(&g, "MATCH (a:P {name: 'A'}) RETURN shortestPath(a, a)");
    assert_eq!(rows.len(), 1);
    let val = rows[0].get("shortestpath(a, a)").expect("column exists");
    match val {
        tessera_graph::gql::GqlValue::List(ids) => {
            assert_eq!(ids.len(), 1, "same-node path should have 1 element");
        }
        other => panic!("expected List, got {other:?}"),
    }
}

#[test]
fn shortest_path_with_cycle_finds_direct_route() {
    // A→B→C→A (cycle), also A→C direct.
    // shortestPath(A, C) should return [A, C] (1 hop, not A→B→C).
    let mut g = Graph::new();
    let a = g.add_node("P", props! { "name" => "A" }).unwrap();
    let b = g.add_node("P", props! { "name" => "B" }).unwrap();
    let c = g.add_node("P", props! { "name" => "C" }).unwrap();
    g.add_edge("R", a, b, props! {}).unwrap();
    g.add_edge("R", b, c, props! {}).unwrap();
    g.add_edge("R", c, a, props! {}).unwrap();
    g.add_edge("R", a, c, props! {}).unwrap();

    let rows = execute_query(
        &g,
        "MATCH (a:P {name: 'A'}), (c:P {name: 'C'}) RETURN shortestPath(a, c)",
    );
    assert_eq!(rows.len(), 1);
    let val = rows[0].get("shortestpath(a, c)").expect("column present");
    match val {
        tessera_graph::gql::GqlValue::List(ids) => {
            assert_eq!(
                ids.len(),
                2,
                "direct A→C is shortest (2 nodes), got {ids:?}"
            );
        }
        other => panic!("expected List, got {other:?}"),
    }
}

#[test]
fn shortest_path_unbound_variable_returns_compile_error() {
    let g = Graph::new();
    let query = tessera_graph::gql::parse("MATCH (a:P) RETURN shortestPath(a, z)").unwrap();
    let result = tessera_graph::gql::execute(&g, &query, 0);
    assert!(result.is_err(), "unbound 'z' should produce compile error");
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("'z' is not bound"),
        "error should name unbound variable, got: {err_msg}"
    );
}

// ── Cypher-style shortestPath tests ─────────────────────────────────────────

#[test]
#[allow(clippy::many_single_char_names)] // allow: test fixture
fn shortest_path_cypher_style_with_hop_limit() {
    // Chain: A -> B -> C -> D
    let mut g = Graph::new();
    let a = g.add_node("P", props! { "name" => "A" }).unwrap();
    let b = g.add_node("P", props! { "name" => "B" }).unwrap();
    let c = g.add_node("P", props! { "name" => "C" }).unwrap();
    let d = g.add_node("P", props! { "name" => "D" }).unwrap();
    g.add_edge("R", a, b, props! {}).unwrap();
    g.add_edge("R", b, c, props! {}).unwrap();
    g.add_edge("R", c, d, props! {}).unwrap();

    // Max 2 hops: D is 3 hops away, should return NULL
    let rows = execute_query(
        &g,
        "MATCH (a:P {name: 'A'}), (d:P {name: 'D'}) RETURN shortestPath((a)-[*..2]->(d))",
    );
    assert_eq!(rows.len(), 1);
    let val = rows[0].get("shortestPath(...)").expect("column exists");
    assert_eq!(
        *val,
        tessera_graph::gql::GqlValue::Null,
        "3-hop path exceeds *..2 limit"
    );

    // Max 3 hops: D is exactly 3 hops away, should return [A,B,C,D]
    let rows = execute_query(
        &g,
        "MATCH (a:P {name: 'A'}), (d:P {name: 'D'}) RETURN shortestPath((a)-[*..3]->(d))",
    );
    assert_eq!(rows.len(), 1);
    let val = rows[0].get("shortestPath(...)").expect("column exists");
    match val {
        tessera_graph::gql::GqlValue::List(ids) => {
            assert_eq!(
                ids.len(),
                4,
                "path A->B->C->D should have 4 nodes, got {ids:?}"
            );
        }
        other => panic!("expected List, got {other:?}"),
    }
}

#[test]
fn shortest_path_cypher_style_with_label_filter() {
    // A -KNOWS-> B -KNOWS-> C, A -BLOCKS-> C
    let mut g = Graph::new();
    let a = g.add_node("P", props! { "name" => "A" }).unwrap();
    let b = g.add_node("P", props! { "name" => "B" }).unwrap();
    let c = g.add_node("P", props! { "name" => "C" }).unwrap();
    g.add_edge("KNOWS", a, b, props! {}).unwrap();
    g.add_edge("KNOWS", b, c, props! {}).unwrap();
    g.add_edge("BLOCKS", a, c, props! {}).unwrap();

    // Filter to KNOWS only: should take A->B->C (3 nodes), not direct BLOCKS edge
    let rows = execute_query(
        &g,
        "MATCH (a:P {name: 'A'}), (c:P {name: 'C'}) RETURN shortestPath((a)-[:KNOWS*]->(c))",
    );
    assert_eq!(rows.len(), 1);
    let val = rows[0].get("shortestPath(...)").expect("column exists");
    match val {
        tessera_graph::gql::GqlValue::List(ids) => {
            assert_eq!(
                ids.len(),
                3,
                "KNOWS-only path A->B->C should have 3 nodes, got {ids:?}"
            );
        }
        other => panic!("expected List, got {other:?}"),
    }
}

#[test]
fn shortest_path_cypher_style_no_path_returns_null() {
    // A and B exist but no edges
    let mut g = Graph::new();
    let _a = g.add_node("P", props! { "name" => "A" }).unwrap();
    let _b = g.add_node("P", props! { "name" => "B" }).unwrap();

    let rows = execute_query(
        &g,
        "MATCH (a:P {name: 'A'}), (b:P {name: 'B'}) RETURN shortestPath((a)-[*]->(b))",
    );
    assert_eq!(rows.len(), 1);
    let val = rows[0].get("shortestPath(...)").expect("column exists");
    assert_eq!(*val, tessera_graph::gql::GqlValue::Null);
}

#[test]
fn shortest_path_legacy_still_works() {
    // Verify the legacy shortestPath(a, b) syntax still works after parser change
    let mut g = Graph::new();
    let a = g.add_node("P", props! { "name" => "A" }).unwrap();
    let b = g.add_node("P", props! { "name" => "B" }).unwrap();
    g.add_edge("R", a, b, props! {}).unwrap();

    let rows = execute_query(
        &g,
        "MATCH (a:P {name: 'A'}), (b:P {name: 'B'}) RETURN shortestPath(a, b)",
    );
    assert_eq!(rows.len(), 1);
    let val = rows[0].get("shortestpath(a, b)").expect("column exists");
    match val {
        tessera_graph::gql::GqlValue::List(ids) => {
            assert_eq!(ids.len(), 2, "path should contain 2 nodes");
        }
        other => panic!("expected List, got {other:?}"),
    }
}

#[test]
fn shortest_path_cypher_style_undirected() {
    // A -> B -> C, query with undirected pattern from C to A
    let mut g = Graph::new();
    let a = g.add_node("P", props! { "name" => "A" }).unwrap();
    let b = g.add_node("P", props! { "name" => "B" }).unwrap();
    let c = g.add_node("P", props! { "name" => "C" }).unwrap();
    g.add_edge("R", a, b, props! {}).unwrap();
    g.add_edge("R", b, c, props! {}).unwrap();

    // Undirected: C can reach A via C<-B<-A
    let rows = execute_query(
        &g,
        "MATCH (c:P {name: 'C'}), (a:P {name: 'A'}) RETURN shortestPath((c)-[*]-(a))",
    );
    assert_eq!(rows.len(), 1);
    let val = rows[0].get("shortestPath(...)").expect("column exists");
    match val {
        tessera_graph::gql::GqlValue::List(ids) => {
            assert_eq!(
                ids.len(),
                3,
                "undirected path C-B-A should have 3 nodes, got {ids:?}"
            );
        }
        other => panic!("expected List, got {other:?}"),
    }
}

#[test]
fn shortest_path_cypher_style_same_node() {
    let mut g = Graph::new();
    let _a = g.add_node("P", props! { "name" => "A" }).unwrap();

    let rows = execute_query(
        &g,
        "MATCH (a:P {name: 'A'}) RETURN shortestPath((a)-[*]->(a))",
    );
    assert_eq!(rows.len(), 1);
    let val = rows[0].get("shortestPath(...)").expect("column exists");
    match val {
        tessera_graph::gql::GqlValue::List(ids) => {
            assert_eq!(ids.len(), 1, "same-node path should have 1 element");
        }
        other => panic!("expected List, got {other:?}"),
    }
}
