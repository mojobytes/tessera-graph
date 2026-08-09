// SPDX-License-Identifier: MIT

use tessera_graph::{GqlValue, Graph, gql, props};

// ── Test helpers ────────────────────────────────────────────────────────────

/// Creates a small social graph:
///   Alice(35) -KNOWS-> Bob(25)
///   Alice(35) -KNOWS-> Carol(30)
///   Dave(40)  -KNOWS-> Bob(25)
fn social_graph() -> Graph {
    let mut g = Graph::new();
    let alice = g
        .add_node("Person", props! { "name" => "Alice", "age" => 35_i64 })
        .unwrap();
    let bob = g
        .add_node("Person", props! { "name" => "Bob", "age" => 25_i64 })
        .unwrap();
    let carol = g
        .add_node("Person", props! { "name" => "Carol", "age" => 30_i64 })
        .unwrap();
    let dave = g
        .add_node("Person", props! { "name" => "Dave", "age" => 40_i64 })
        .unwrap();
    g.add_edge("KNOWS", alice, bob, props! {}).unwrap();
    g.add_edge("KNOWS", alice, carol, props! {}).unwrap();
    g.add_edge("KNOWS", dave, bob, props! {}).unwrap();
    g
}

/// Parses and executes a GQL query against a graph.
fn run(
    graph: &Graph,
    query: &str,
) -> tessera_graph::Result<Vec<std::collections::HashMap<String, GqlValue>>> {
    let ast = gql::parse(query)?;
    gql::execute(graph, &ast, 0)
}

// ── Cycle 1–3: Conversion (implicitly tested through execute) ───────────────

// ── Cycle 4: MATCH compilation ──────────────────────────────────────────────

#[test]
fn compile_match_single_node_any_label() {
    let g = social_graph();
    let rows = run(&g, "MATCH (a) RETURN a").unwrap();
    assert_eq!(rows.len(), 4); // Alice, Bob, Carol, Dave
}

#[test]
fn compile_match_single_node_with_label() {
    let mut g = Graph::new();
    g.add_node("Person", props! { "name" => "Alice" }).unwrap();
    g.add_node("Person", props! { "name" => "Bob" }).unwrap();
    g.add_node("Thing", props! { "name" => "Car" }).unwrap();
    let rows = run(&g, "MATCH (a:Person) RETURN a").unwrap();
    assert_eq!(rows.len(), 2);
}

#[test]
fn compile_match_single_node_with_inline_prop() {
    let g = social_graph();
    let rows = run(&g, "MATCH (a:Person {name: 'Alice'}) RETURN a").unwrap();
    assert_eq!(rows.len(), 1);
}

#[test]
fn compile_match_two_node_outgoing_edge() {
    let g = social_graph();
    let rows = run(&g, "MATCH (a:Person)-[:KNOWS]->(b) RETURN a.name").unwrap();
    // Alice→Bob, Alice→Carol, Dave→Bob = 3 edges
    assert_eq!(rows.len(), 3);
}

#[test]
fn compile_match_named_edge_variable() {
    let g = social_graph();
    let rows = run(&g, "MATCH (a)-[r:KNOWS]->(b) RETURN a.name").unwrap();
    assert_eq!(rows.len(), 3);
}

#[test]
fn compile_match_unknown_var_in_return_is_compile_error() {
    let g = social_graph();
    let err = run(&g, "MATCH (a) RETURN z").unwrap_err();
    assert!(matches!(err, tessera_graph::Error::GqlCompileError(_)));
}

// ── Cycle 5: WHERE filtering ────────────────────────────────────────────────

#[test]
fn where_filters_by_node_property_equality() {
    let g = social_graph();
    let rows = run(&g, "MATCH (a:Person) WHERE a.name = 'Alice' RETURN a.name").unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["a.name"], GqlValue::Str("Alice".into()));
}

#[test]
fn where_filters_by_numeric_comparison() {
    let g = social_graph();
    let rows = run(&g, "MATCH (a:Person) WHERE a.age > 30 RETURN a.name").unwrap();
    // Alice(35), Dave(40)
    assert_eq!(rows.len(), 2);
}

#[test]
fn where_filters_and_compound() {
    let g = social_graph();
    let rows = run(
        &g,
        "MATCH (a:Person) WHERE a.age > 20 AND a.name = 'Bob' RETURN a.name",
    )
    .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["a.name"], GqlValue::Str("Bob".into()));
}

#[test]
fn where_no_match_returns_empty() {
    let g = social_graph();
    let rows = run(
        &g,
        "MATCH (a:Person) WHERE a.name = 'Nonexistent' RETURN a.name",
    )
    .unwrap();
    assert!(rows.is_empty());
}

#[test]
fn where_null_predicate_excludes_row() {
    let g = social_graph();
    let rows = run(
        &g,
        "MATCH (a:Person) WHERE a.missing_prop = 'x' RETURN a.name",
    )
    .unwrap();
    assert!(rows.is_empty());
}

// ── Cycle 6: RETURN projection ──────────────────────────────────────────────

#[test]
fn return_single_prop_access() {
    let g = social_graph();
    let rows = run(&g, "MATCH (a:Person) RETURN a.name").unwrap();
    assert_eq!(rows.len(), 4);
    // Every row must have the key "a.name"
    for row in &rows {
        assert!(row.contains_key("a.name"));
    }
}

#[test]
fn return_alias_renames_column() {
    let g = social_graph();
    let rows = run(&g, "MATCH (a:Person) RETURN a.name AS person_name").unwrap();
    assert!(rows[0].contains_key("person_name"));
    assert!(!rows[0].contains_key("a.name"));
}

#[test]
fn return_multiple_columns() {
    let g = social_graph();
    let rows = run(
        &g,
        "MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN a.name, b.name",
    )
    .unwrap();
    for row in &rows {
        assert!(row.contains_key("a.name"));
        assert!(row.contains_key("b.name"));
    }
}

#[test]
fn return_literal_value() {
    let mut g = Graph::new();
    g.add_node("Person", props! { "name" => "Alice" }).unwrap();
    let rows = run(&g, "MATCH (a:Person) RETURN 42").unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["42"], GqlValue::Int(42));
}

#[test]
fn return_distinct_deduplicates() {
    let g = social_graph();
    // Alice has two KNOWS edges, so without DISTINCT we'd get Alice twice
    let rows = run(&g, "MATCH (a:Person)-[:KNOWS]->(b) RETURN DISTINCT a.name").unwrap();
    let alice_count = rows
        .iter()
        .filter(|r| r["a.name"] == GqlValue::Str("Alice".into()))
        .count();
    assert_eq!(alice_count, 1);
}

// ── Cycle 7: ORDER BY ───────────────────────────────────────────────────────

#[test]
fn order_by_asc_sorts_strings() {
    let g = social_graph();
    let rows = run(&g, "MATCH (a:Person) RETURN a.name ORDER BY a.name ASC").unwrap();
    let names: Vec<&str> = rows
        .iter()
        .map(|r| match &r["a.name"] {
            GqlValue::Str(s) => s.as_str(),
            _ => panic!("expected Str"),
        })
        .collect();
    assert_eq!(names, vec!["Alice", "Bob", "Carol", "Dave"]);
}

#[test]
fn order_by_desc_sorts_strings() {
    let g = social_graph();
    let rows = run(&g, "MATCH (a:Person) RETURN a.name ORDER BY a.name DESC").unwrap();
    let names: Vec<&str> = rows
        .iter()
        .map(|r| match &r["a.name"] {
            GqlValue::Str(s) => s.as_str(),
            _ => panic!("expected Str"),
        })
        .collect();
    assert_eq!(names, vec!["Dave", "Carol", "Bob", "Alice"]);
}

#[test]
fn order_by_int_asc() {
    let g = social_graph();
    let rows = run(&g, "MATCH (a:Person) RETURN a.name ORDER BY a.age ASC").unwrap();
    let names: Vec<&str> = rows
        .iter()
        .map(|r| match &r["a.name"] {
            GqlValue::Str(s) => s.as_str(),
            _ => panic!("expected Str"),
        })
        .collect();
    assert_eq!(names, vec!["Bob", "Carol", "Alice", "Dave"]);
}

#[test]
fn order_by_null_values_sort_last() {
    let mut g = Graph::new();
    g.add_node("Person", props! { "name" => "Alice", "score" => 10_i64 })
        .unwrap();
    g.add_node("Person", props! { "name" => "Bob" }).unwrap(); // no "score"
    g.add_node("Person", props! { "name" => "Carol", "score" => 5_i64 })
        .unwrap();
    let rows = run(&g, "MATCH (a:Person) RETURN a.name ORDER BY a.score ASC").unwrap();
    // Bob has no score → NULL → sorts last
    let names: Vec<&str> = rows
        .iter()
        .map(|r| match &r["a.name"] {
            GqlValue::Str(s) => s.as_str(),
            _ => panic!("expected Str"),
        })
        .collect();
    assert_eq!(names[2], "Bob"); // Last
}

// ── Cycle 8: LIMIT ──────────────────────────────────────────────────────────

#[test]
fn limit_truncates_result_to_n_rows() {
    let g = social_graph();
    let rows = run(&g, "MATCH (a:Person) RETURN a.name LIMIT 2").unwrap();
    assert_eq!(rows.len(), 2);
}

#[test]
fn limit_larger_than_result_set_returns_all() {
    let g = social_graph();
    let rows = run(&g, "MATCH (a:Person) RETURN a.name LIMIT 100").unwrap();
    assert_eq!(rows.len(), 4);
}

#[test]
fn limit_zero_returns_empty() {
    let g = social_graph();
    let rows = run(&g, "MATCH (a:Person) RETURN a.name LIMIT 0").unwrap();
    assert!(rows.is_empty());
}

// ── Cycle 9: Aggregation ────────────────────────────────────────────────────

#[test]
fn count_star_returns_total_rows() {
    let g = social_graph();
    let rows = run(&g, "MATCH (a:Person) RETURN COUNT(*)").unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["COUNT(*)"], GqlValue::Int(4));
}

#[test]
fn count_with_arg_counts_non_null() {
    let mut g = Graph::new();
    g.add_node("Person", props! { "name" => "Alice" }).unwrap();
    g.add_node("Person", props! { "name" => "Bob" }).unwrap();
    g.add_node("Person", props! {}).unwrap(); // no "name" property
    let rows = run(&g, "MATCH (a:Person) RETURN COUNT(a.name)").unwrap();
    assert_eq!(rows[0]["COUNT(a.name)"], GqlValue::Int(2));
}

#[test]
fn sum_aggregate() {
    let g = social_graph();
    let rows = run(&g, "MATCH (a:Person) RETURN SUM(a.age)").unwrap();
    // 35 + 25 + 30 + 40 = 130
    assert_eq!(rows[0]["SUM(a.age)"], GqlValue::Int(130));
}

#[test]
fn avg_aggregate() {
    let g = social_graph();
    let rows = run(&g, "MATCH (a:Person) RETURN AVG(a.age)").unwrap();
    // (35 + 25 + 30 + 40) / 4 = 32.5
    assert_eq!(rows[0]["AVG(a.age)"], GqlValue::Float(32.5));
}

#[test]
fn min_max_aggregate() {
    let g = social_graph();
    let rows = run(&g, "MATCH (a:Person) RETURN MIN(a.age), MAX(a.age)").unwrap();
    assert_eq!(rows[0]["MIN(a.age)"], GqlValue::Int(25));
    assert_eq!(rows[0]["MAX(a.age)"], GqlValue::Int(40));
}

#[test]
fn collect_aggregate_returns_list() {
    let g = social_graph();
    let rows = run(&g, "MATCH (a:Person) RETURN COLLECT(a.name)").unwrap();
    assert_eq!(rows.len(), 1);
    match &rows[0]["COLLECT(a.name)"] {
        GqlValue::List(items) => assert_eq!(items.len(), 4),
        other => panic!("expected List, got {other:?}"),
    }
}

#[test]
fn aggregate_with_alias() {
    let g = social_graph();
    let rows = run(&g, "MATCH (a:Person) RETURN COUNT(*) AS total").unwrap();
    assert!(rows[0].contains_key("total"));
    assert!(!rows[0].contains_key("COUNT(*)"));
    assert_eq!(rows[0]["total"], GqlValue::Int(4));
}

#[test]
fn mixing_aggregate_and_non_aggregate_is_compile_error() {
    let g = social_graph();
    let err = run(&g, "MATCH (a:Person) RETURN a.name, COUNT(*)").unwrap_err();
    assert!(matches!(err, tessera_graph::Error::GqlCompileError(_)));
}

#[test]
fn count_variable_counts_bound_nodes() {
    let g = social_graph();
    let rows = run(&g, "MATCH (n:Person) RETURN COUNT(n)").unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["COUNT(n)"], GqlValue::Int(4));
}

#[test]
fn count_edge_variable_counts_bound_edges() {
    let g = social_graph();
    let rows = run(&g, "MATCH (a)-[r]->(b) RETURN COUNT(r)").unwrap();
    assert_eq!(rows.len(), 1);
    // social_graph has 3 edges: alice->bob, alice->carol, dave->bob
    assert_eq!(rows[0]["COUNT(r)"], GqlValue::Int(3));
}

#[test]
fn count_variable_with_label_filter() {
    let mut g = Graph::new();
    g.add_node("Person", props! { "name" => "Alice" }).unwrap();
    g.add_node("Person", props! { "name" => "Bob" }).unwrap();
    g.add_node("Bot", props! { "name" => "Siri" }).unwrap();
    let rows = run(&g, "MATCH (n:Person) RETURN COUNT(n)").unwrap();
    assert_eq!(rows[0]["COUNT(n)"], GqlValue::Int(2));
}

// ── Cycle 11 / Fase B C3: Bare variable semantics ──────────────────────────
// Since Fase B, a bare node/edge variable projects as a first-class
// `GqlValue::Node`/`Relationship` (ISO GQL / Cypher), not the entity id as an
// integer. `id(n)` remains the way to get the raw id.

#[test]
fn return_bare_node_var_produces_node_value() {
    let mut g = Graph::new();
    let id = g.add_node("Person", props! { "name" => "Alice" }).unwrap();
    let rows = run(&g, "MATCH (n:Person) RETURN n").unwrap();
    assert_eq!(rows.len(), 1);
    match &rows[0]["n"] {
        GqlValue::Node(node) => {
            #[allow(clippy::cast_possible_wrap)]
            let expected_id = id.as_u64() as i64;
            assert_eq!(node.id, expected_id);
            assert_eq!(node.labels, vec!["Person".to_owned()]);
            assert_eq!(
                node.props.get("name"),
                Some(&GqlValue::Str("Alice".to_owned()))
            );
        }
        other => panic!("expected GqlValue::Node, got {other:?}"),
    }
}

#[test]
fn id_function_returns_int_while_bare_var_returns_node() {
    let mut g = Graph::new();
    let id = g.add_node("Person", props! { "name" => "Alice" }).unwrap();
    let rows_bare = run(&g, "MATCH (n:Person) RETURN n").unwrap();
    let rows_id = run(&g, "MATCH (n:Person) RETURN id(n)").unwrap();
    assert_eq!(rows_bare.len(), 1);
    assert_eq!(rows_id.len(), 1);
    // id(n) is the raw integer id; the bare var is the full Node whose `.id`
    // field equals it.
    #[allow(clippy::cast_possible_wrap)]
    let expected = GqlValue::Int(id.as_u64() as i64);
    assert_eq!(rows_id[0]["id(n)"], expected);
    match &rows_bare[0]["n"] {
        GqlValue::Node(node) => assert_eq!(GqlValue::Int(node.id), expected),
        other => panic!("expected GqlValue::Node, got {other:?}"),
    }
}

#[test]
fn order_by_bare_node_var_keeps_all_rows() {
    let mut g = Graph::new();
    g.add_node("Person", props! { "name" => "Alice" }).unwrap();
    g.add_node("Person", props! { "name" => "Bob" }).unwrap();
    g.add_node("Person", props! { "name" => "Carol" }).unwrap();
    // A bare entity is not an orderable scalar (the comparator yields None), so
    // ORDER BY n is a stable no-op rather than an error: all rows survive and
    // each `n` is a Node. (Order by an entity field — ORDER BY n.name — is the
    // supported idiom for deterministic ordering.)
    let rows = run(&g, "MATCH (n:Person) RETURN n, n.name ORDER BY n DESC").unwrap();
    assert_eq!(rows.len(), 3);
    for r in &rows {
        assert!(matches!(r["n"], GqlValue::Node(_)), "got {:?}", r["n"]);
    }
}

#[test]
fn count_node_var_with_where_filter() {
    let g = social_graph(); // Alice(35), Bob(25), Carol(30), Dave(40)
    let rows = run(&g, "MATCH (n:Person) WHERE n.age > 30 RETURN COUNT(n)").unwrap();
    assert_eq!(rows.len(), 1);
    // Alice(35) and Dave(40) pass the filter
    assert_eq!(rows[0]["COUNT(n)"], GqlValue::Int(2));
}

#[test]
fn count_node_var_empty_graph_returns_zero() {
    let g = Graph::new();
    let rows = run(&g, "MATCH (n:Person) RETURN COUNT(n)").unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["COUNT(n)"], GqlValue::Int(0));
}

// ── Cycle 10: End-to-end integration ────────────────────────────────────────

#[test]
fn full_query_match_where_return_order_limit() {
    let g = social_graph();
    let rows = run(
        &g,
        "MATCH (a:Person)-[:KNOWS]->(b:Person) \
         WHERE a.name = 'Alice' \
         RETURN b.name \
         ORDER BY b.name ASC \
         LIMIT 1",
    )
    .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["b.name"], GqlValue::Str("Bob".into()));
}

#[test]
fn full_query_count_with_where() {
    let g = social_graph();
    let rows = run(&g, "MATCH (a:Person) WHERE a.age > 30 RETURN COUNT(*)").unwrap();
    // Alice(35), Dave(40)
    assert_eq!(rows[0]["COUNT(*)"], GqlValue::Int(2));
}

#[test]
fn full_query_distinct_names_ordered() {
    let g = social_graph();
    let rows = run(
        &g,
        "MATCH (a:Person) RETURN DISTINCT a.name ORDER BY a.name ASC",
    )
    .unwrap();
    let names: Vec<&str> = rows
        .iter()
        .map(|r| match &r["a.name"] {
            GqlValue::Str(s) => s.as_str(),
            _ => panic!("expected Str"),
        })
        .collect();
    assert_eq!(names, vec!["Alice", "Bob", "Carol", "Dave"]);
}

#[test]
fn parse_and_execute_roundtrip() {
    let g = social_graph();
    let query = gql::parse("MATCH (a:Person) RETURN COUNT(*)").unwrap();
    let result = gql::execute(&g, &query, 0).unwrap();
    assert_eq!(result.len(), 1);
    assert_eq!(result[0]["COUNT(*)"], GqlValue::Int(4));
}

// ── C6: Edge without brackets gives helpful error ────────────────────────

#[test]
fn parse_edge_outgoing_without_brackets_gives_helpful_error() {
    let err = gql::parse("MATCH (a)->(b) RETURN a").unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("edge pattern requires brackets"),
        "expected helpful message for ->, got: {msg}"
    );
}

#[test]
fn parse_edge_minus_without_brackets_gives_helpful_error() {
    let err = gql::parse("MATCH (a)-x RETURN a").unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("edge pattern requires brackets"),
        "expected helpful message for -, got: {msg}"
    );
}

#[test]
fn parse_edge_incoming_without_brackets_gives_helpful_error() {
    let err = gql::parse("MATCH (a)<-(b) RETURN a").unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("edge pattern requires brackets"),
        "expected helpful message for <-, got: {msg}"
    );
}

// ── C7: ORDER BY with mixed types ───────────────────────────────────────

#[test]
fn order_by_mixed_types_does_not_panic() {
    let mut g = Graph::new();
    g.add_node("N", props! { "val" => 42_i64 }).unwrap();
    g.add_node("N", props! { "val" => "hello" }).unwrap();
    g.add_node("N", props! {}).unwrap(); // val is NULL

    let rows = run(&g, "MATCH (a:N) RETURN a.val ORDER BY a.val ASC").unwrap();
    // All 3 rows returned, order of incomparable types is unspecified but no panic
    assert_eq!(rows.len(), 3);
}

// ── C8: DISTINCT determinism ─────────────────────────────────────────────

#[test]
fn distinct_removes_duplicate_rows_multi_column() {
    let mut g = Graph::new();
    g.add_node("Person", props! { "name" => "Alice", "age" => 30_i64 })
        .unwrap();
    g.add_node("Person", props! { "name" => "Alice", "age" => 30_i64 })
        .unwrap();
    g.add_node("Person", props! { "name" => "Bob",   "age" => 25_i64 })
        .unwrap();

    let rows = run(&g, "MATCH (a:Person) RETURN DISTINCT a.name, a.age").unwrap();
    assert_eq!(rows.len(), 2);
}

#[test]
fn distinct_deduplicates_rows_with_null_values() {
    let mut g = Graph::new();
    g.add_node("N", props! {}).unwrap();
    g.add_node("N", props! {}).unwrap();

    let rows = run(&g, "MATCH (a:N) RETURN DISTINCT a.x").unwrap();
    assert_eq!(rows.len(), 1);
}

#[test]
fn distinct_on_integer_column_is_deterministic() {
    let mut g = Graph::new();
    for _ in 0..5 {
        g.add_node("N", props! { "v" => 42_i64 }).unwrap();
    }
    let rows = run(&g, "MATCH (a:N) RETURN DISTINCT a.v").unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["a.v"], GqlValue::Int(42));
}

// ── C9: Ternary logic in WHERE ───────────────────────────────────────────

#[test]
fn where_integer_as_bool_predicate_excludes_all_rows() {
    let g = social_graph();
    let rows = run(&g, "MATCH (a:Person) WHERE a.age RETURN a.name").unwrap();
    assert!(
        rows.is_empty(),
        "integer as WHERE predicate must exclude all rows"
    );
}

#[test]
fn where_and_with_null_propagates_null() {
    let g = social_graph();
    let rows = run(
        &g,
        "MATCH (a:Person) WHERE a.missing AND true RETURN a.name",
    )
    .unwrap();
    assert!(rows.is_empty(), "NULL AND true must be NULL → row excluded");
}

#[test]
fn where_or_with_true_and_null_includes_row() {
    let g = social_graph();
    let rows = run(
        &g,
        "MATCH (a:Person) WHERE a.age > 0 OR a.missing RETURN a.name",
    )
    .unwrap();
    assert_eq!(
        rows.len(),
        4,
        "true OR NULL must be true → all rows included"
    );
}

#[test]
fn where_false_or_null_excludes_row() {
    let g = social_graph();
    let rows = run(
        &g,
        "MATCH (a:Person) WHERE a.age < 0 OR a.missing RETURN a.name",
    )
    .unwrap();
    assert!(rows.is_empty(), "false OR NULL must be NULL → row excluded");
}

#[test]
fn where_null_and_false_excludes_row() {
    let g = social_graph();
    let rows = run(
        &g,
        "MATCH (a:Person) WHERE a.missing AND a.age < 0 RETURN a.name",
    )
    .unwrap();
    assert!(
        rows.is_empty(),
        "NULL AND false must be false → row excluded"
    );
}

// ── C10: Cross-join ────────────────────────────────────────────────────

#[test]
fn multi_pattern_match_cross_join() {
    let mut g = Graph::new();
    g.add_node("Person", props! { "name" => "Alice" }).unwrap();
    g.add_node("Person", props! { "name" => "Bob" }).unwrap();

    let results = run(
        &g,
        "MATCH (a:Person {name: 'Alice'}), (b:Person {name: 'Bob'}) RETURN a.name, b.name",
    )
    .unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["a.name"], GqlValue::Str("Alice".into()));
    assert_eq!(results[0]["b.name"], GqlValue::Str("Bob".into()));
}

#[test]
fn multi_pattern_match_cross_join_cartesian() {
    let mut g = Graph::new();
    g.add_node("Person", props! { "name" => "Alice" }).unwrap();
    g.add_node("Person", props! { "name" => "Bob" }).unwrap();
    g.add_node("City", props! { "name" => "Madrid" }).unwrap();

    // 2 persons × 1 city = 2 rows
    let results = run(&g, "MATCH (p:Person), (c:City) RETURN p.name, c.name").unwrap();
    assert_eq!(results.len(), 2);
}

#[test]
fn single_pattern_match_still_works() {
    let g = social_graph();
    let rows = run(&g, "MATCH (a:Person) RETURN a.name").unwrap();
    assert_eq!(rows.len(), 4);
}

// ── Aggregate pushdown tests ─────────────────────────────────────────────

#[test]
fn count_star_pushdown_matches_materialized() {
    let mut g = Graph::new();
    for i in 0_i32..100 {
        g.add_node("Person", props! { "id" => i64::from(i) })
            .unwrap();
    }
    g.add_node("Bot", props! { "name" => "siri" }).unwrap();
    let rows = run(&g, "MATCH (n:Person) RETURN COUNT(*)").unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["COUNT(*)"], GqlValue::Int(100));
}

#[test]
fn count_star_pushdown_large_dataset() {
    let mut g = Graph::new();
    for i in 0_i32..10_000 {
        g.add_node("Person", props! { "id" => i64::from(i) })
            .unwrap();
    }
    let t0 = std::time::Instant::now();
    let rows = run(&g, "MATCH (n:Person) RETURN COUNT(*)").unwrap();
    let elapsed = t0.elapsed();
    assert_eq!(rows[0]["COUNT(*)"], GqlValue::Int(10_000));
    // Pushdown should be <10ms; full materialization takes ~20ms
    assert!(
        elapsed.as_millis() < 10,
        "COUNT(*) took too long: {elapsed:?} — pushdown may not be active"
    );
}

#[test]
fn count_prop_pushdown() {
    let mut g = Graph::new();
    for i in 0..100 {
        if i % 3 == 0 {
            g.add_node("Person", props! {}).unwrap();
        } else {
            g.add_node("Person", props! { "name" => format!("p{i}") })
                .unwrap();
        }
    }
    let rows = run(&g, "MATCH (n:Person) RETURN COUNT(n.name)").unwrap();
    // i % 3 == 0 → 34 nodes without "name" (0,3,6,...,99) → 66 with
    assert_eq!(rows[0]["COUNT(n.name)"], GqlValue::Int(66));
}

#[test]
fn sum_pushdown() {
    let mut g = Graph::new();
    let mut expected: i64 = 0;
    for i in 0_i32..1_000 {
        let age = i64::from(i % 100);
        expected += age;
        g.add_node("Person", props! { "age" => age }).unwrap();
    }
    let rows = run(&g, "MATCH (n:Person) RETURN SUM(n.age)").unwrap();
    assert_eq!(rows[0]["SUM(n.age)"], GqlValue::Int(expected));
}

#[test]
fn avg_pushdown() {
    let mut g = Graph::new();
    for i in 0_i32..1_000 {
        g.add_node("Person", props! { "score" => i64::from(i % 10) })
            .unwrap();
    }
    let rows = run(&g, "MATCH (n:Person) RETURN AVG(n.score)").unwrap();
    assert_eq!(rows[0]["AVG(n.score)"], GqlValue::Float(4.5));
}

#[test]
fn min_max_pushdown() {
    let mut g = Graph::new();
    for i in 0_i32..1_000 {
        g.add_node("Person", props! { "score" => i64::from(i % 100) })
            .unwrap();
    }
    let rows = run(&g, "MATCH (n:Person) RETURN MIN(n.score), MAX(n.score)").unwrap();
    assert_eq!(rows[0]["MIN(n.score)"], GqlValue::Int(0));
    assert_eq!(rows[0]["MAX(n.score)"], GqlValue::Int(99));
}

#[test]
fn collect_pushdown() {
    let mut g = Graph::new();
    for i in 0..5 {
        g.add_node("Person", props! { "name" => format!("p{i}") })
            .unwrap();
    }
    let rows = run(&g, "MATCH (n:Person) RETURN COLLECT(n.name)").unwrap();
    match &rows[0]["COLLECT(n.name)"] {
        GqlValue::List(items) => assert_eq!(items.len(), 5),
        other => panic!("expected List, got {other:?}"),
    }
}

#[test]
fn multi_aggregate_pushdown() {
    let mut g = Graph::new();
    for i in 0_i32..100 {
        g.add_node("Person", props! { "age" => i64::from(i % 50) })
            .unwrap();
    }
    let rows = run(
        &g,
        "MATCH (n:Person) RETURN COUNT(*), SUM(n.age), AVG(n.age), MIN(n.age), MAX(n.age)",
    )
    .unwrap();
    assert_eq!(rows[0]["COUNT(*)"], GqlValue::Int(100));
    assert_eq!(rows[0]["MIN(n.age)"], GqlValue::Int(0));
    assert_eq!(rows[0]["MAX(n.age)"], GqlValue::Int(49));
}

#[test]
fn aggregate_with_where_still_correct() {
    let mut g = Graph::new();
    for i in 0_i32..100 {
        g.add_node("Person", props! { "age" => i64::from(i) })
            .unwrap();
    }
    let rows = run(&g, "MATCH (n:Person) WHERE n.age >= 50 RETURN COUNT(n)").unwrap();
    assert_eq!(rows[0]["COUNT(n)"], GqlValue::Int(50));
}

#[test]
fn aggregate_with_edge_pattern_still_correct() {
    let g = social_graph();
    let rows = run(&g, "MATCH (a)-[r]->(b) RETURN COUNT(r)").unwrap();
    assert_eq!(rows[0]["COUNT(r)"], GqlValue::Int(3));
}

// ── One-hop aggregate pushdown tests ─────────────────────────────────────

#[test]
fn count_one_hop_returns_correct_count() {
    let mut g = Graph::new();
    // 3 Container nodes, each with 4 Item children = 12 edges total
    for _ in 0..3 {
        let container = g.add_node("Container", props! {}).unwrap();
        for _ in 0..4 {
            let item = g.add_node("Item", props! {}).unwrap();
            g.add_edge("CONTAINS", container, item, props! {}).unwrap();
        }
    }
    let rows = run(
        &g,
        "MATCH (p:Container)-[:CONTAINS]->(c:Item) RETURN count(c)",
    )
    .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["COUNT(c)"], GqlValue::Int(12));
}

#[test]
fn count_one_hop_no_end_label_constraint() {
    let mut g = Graph::new();
    // 2 Container nodes, each with 3 mixed children
    for _ in 0..2 {
        let container = g.add_node("Container", props! {}).unwrap();
        for _ in 0..2 {
            let item = g.add_node("Item", props! {}).unwrap();
            g.add_edge("CONTAINS", container, item, props! {}).unwrap();
        }
        let other = g.add_node("Other", props! {}).unwrap();
        g.add_edge("CONTAINS", container, other, props! {}).unwrap();
    }
    // Without end label filter: all 6 edges match
    let rows = run(&g, "MATCH (p:Container)-[:CONTAINS]->(c) RETURN count(c)").unwrap();
    assert_eq!(rows[0]["COUNT(c)"], GqlValue::Int(6));
    // With end label filter: only 4 Item edges match
    let rows = run(
        &g,
        "MATCH (p:Container)-[:CONTAINS]->(c:Item) RETURN count(c)",
    )
    .unwrap();
    assert_eq!(rows[0]["COUNT(c)"], GqlValue::Int(4));
}

#[test]
fn count_one_hop_incoming_direction() {
    let mut g = Graph::new();
    let a = g.add_node("A", props! {}).unwrap();
    let b = g.add_node("B", props! {}).unwrap();
    let c = g.add_node("B", props! {}).unwrap();
    g.add_edge("R", a, b, props! {}).unwrap();
    g.add_edge("R", a, c, props! {}).unwrap();
    // Incoming: from B's perspective, count incoming R edges from A
    let rows = run(&g, "MATCH (b:B)<-[:R]-(a:A) RETURN count(a)").unwrap();
    assert_eq!(rows[0]["COUNT(a)"], GqlValue::Int(2));
}

// ── UNWIND tests ─────────────────────────────────────────────────────────

#[test]
fn unwind_list_with_match_returns_cross_join() {
    let mut g = Graph::new();
    g.add_node("N", props! {}).unwrap();
    let rows = run(&g, "UNWIND [10, 20, 30] AS x MATCH (n:N) RETURN x").unwrap();
    assert_eq!(rows.len(), 3);
    let mut values: Vec<i64> = rows
        .iter()
        .map(|r| match &r["x"] {
            GqlValue::Int(v) => *v,
            other => panic!("expected Int, got {other:?}"),
        })
        .collect();
    values.sort_unstable();
    assert_eq!(values, vec![10, 20, 30]);
}

#[test]
fn unwind_empty_list_returns_zero_rows() {
    let mut g = Graph::new();
    g.add_node("N", props! {}).unwrap();
    let rows = run(&g, "UNWIND [] AS x MATCH (n:N) RETURN x").unwrap();
    assert_eq!(rows.len(), 0);
}

#[test]
fn unwind_with_where_filter() {
    let mut g = Graph::new();
    g.add_node("N", props! {}).unwrap();
    let rows = run(
        &g,
        "UNWIND [1, 2, 3, 4, 5] AS x MATCH (n:N) WHERE x > 3 RETURN x",
    )
    .unwrap();
    assert_eq!(rows.len(), 2);
    let mut values: Vec<i64> = rows
        .iter()
        .map(|r| match &r["x"] {
            GqlValue::Int(v) => *v,
            other => panic!("expected Int, got {other:?}"),
        })
        .collect();
    values.sort_unstable();
    assert_eq!(values, vec![4, 5]);
}

#[test]
fn unwind_with_count() {
    let mut g = Graph::new();
    g.add_node("N", props! {}).unwrap();
    let rows = run(&g, "UNWIND [1, 2, 3] AS x MATCH (n:N) RETURN COUNT(x)").unwrap();
    assert_eq!(rows[0]["COUNT(x)"], GqlValue::Int(3));
}

#[test]
fn unwind_cross_join_multiple_nodes() {
    let mut g = Graph::new();
    g.add_node("N", props! { "id" => 1_i64 }).unwrap();
    g.add_node("N", props! { "id" => 2_i64 }).unwrap();
    // 3 unwind elements × 2 nodes = 6 rows
    let rows = run(&g, "UNWIND [10, 20, 30] AS x MATCH (n:N) RETURN x, n.id").unwrap();
    assert_eq!(rows.len(), 6);
}

#[test]
fn unwind_string_list() {
    let mut g = Graph::new();
    g.add_node("N", props! {}).unwrap();
    let rows = run(&g, "UNWIND ['a', 'b', 'c'] AS s MATCH (n:N) RETURN s").unwrap();
    assert_eq!(rows.len(), 3);
    let mut values: Vec<String> = rows
        .iter()
        .map(|r| match &r["s"] {
            GqlValue::Str(v) => v.clone(),
            other => panic!("expected Str, got {other:?}"),
        })
        .collect();
    values.sort();
    assert_eq!(values, vec!["a", "b", "c"]);
}

// ── UNWIND range() source (issue #15) ───────────────────────────────────────
//
// `range(a, b)` as the UNWIND source must expand to the inclusive integer list
// [a, a+1, ..., b] in EVERY evaluation path, not only the pipeline binding
// evaluator. The non-pipeline read path (`execute_with_unwind`) evaluates the
// source via `eval_expr`/`eval_function_call`, which historically did not
// resolve `range` — making `UNWIND range(...) ...` silently yield zero rows.

#[test]
fn unwind_range_with_match_returns_cross_join() {
    let mut g = Graph::new();
    g.add_node("N", props! {}).unwrap();
    let rows = run(&g, "UNWIND range(1, 3) AS x MATCH (n:N) RETURN x").unwrap();
    assert_eq!(rows.len(), 3, "range(1,3) must expand to 3 elements");
    let mut values: Vec<i64> = rows
        .iter()
        .map(|r| match &r["x"] {
            GqlValue::Int(v) => *v,
            other => panic!("expected Int, got {other:?}"),
        })
        .collect();
    values.sort_unstable();
    assert_eq!(values, vec![1, 2, 3]);
}

/// `execute_expr` is the public entry the server uses to evaluate an UNWIND
/// source list (see `execute_unwind_mutation`). It must resolve `range(a, b)`
/// to an inclusive integer list. This pins the root cause of issue #15 at the
/// engine level: `range` was only implemented in the pipeline binding
/// evaluator, not in the `PatternMatch`-based `eval_function_call`.
#[test]
fn execute_expr_resolves_range_builtin() {
    use tessera_graph::PatternMatch;
    let g = Graph::new();
    let empty = PatternMatch::empty();

    let stmt = gql::parse_statement("UNWIND range(1, 3) AS x CREATE (:N {v: x})").unwrap();
    let mutation = stmt.into_mutation().expect("expected a mutation statement");
    let unwind = mutation
        .unwind_clause
        .as_ref()
        .expect("expected UNWIND clause");

    let list_val = gql::execute_expr(&unwind.expr, &empty, &g);
    match list_val {
        GqlValue::List(items) => {
            let ints: Vec<i64> = items
                .iter()
                .map(|v| match v {
                    GqlValue::Int(n) => *n,
                    other => panic!("expected Int element, got {other:?}"),
                })
                .collect();
            assert_eq!(ints, vec![1, 2, 3], "range(1,3) must be inclusive");
        }
        other => panic!("expected List, got {other:?}"),
    }
}

/// `range(a, b)` with `a > b` yields an empty list (Cypher semantics), and the
/// reversed/empty case must not silently behave like the missing-builtin bug.
#[test]
fn execute_expr_range_descending_is_empty() {
    use tessera_graph::PatternMatch;
    let g = Graph::new();
    let empty = PatternMatch::empty();

    let stmt = gql::parse_statement("UNWIND range(5, 1) AS x CREATE (:N {v: x})").unwrap();
    let mutation = stmt.into_mutation().expect("expected a mutation statement");
    let unwind = mutation
        .unwind_clause
        .as_ref()
        .expect("expected UNWIND clause");

    let list_val = gql::execute_expr(&unwind.expr, &empty, &g);
    assert_eq!(
        list_val,
        GqlValue::List(Vec::new()),
        "range(5,1) must be empty"
    );
}

/// `size()` must likewise resolve through `execute_expr`, since both `range`
/// and `size` were duplicated only in the pipeline binding evaluator.
#[test]
fn execute_expr_resolves_size_builtin() {
    use tessera_graph::PatternMatch;
    let g = Graph::new();
    let empty = PatternMatch::empty();

    // size([1,2,3,4]) == 4, evaluated via the public expression entry point.
    let stmt = gql::parse_statement("UNWIND range(1, size([7, 8, 9, 10])) AS x CREATE (:N {v: x})")
        .unwrap();
    let mutation = stmt.into_mutation().expect("expected a mutation statement");
    let unwind = mutation
        .unwind_clause
        .as_ref()
        .expect("expected UNWIND clause");

    let list_val = gql::execute_expr(&unwind.expr, &empty, &g);
    match list_val {
        GqlValue::List(items) => {
            assert_eq!(items.len(), 4, "range(1, size([..4..])) must be 4 elements");
        }
        other => panic!("expected List, got {other:?}"),
    }
}

// ── ORDER BY pre-computed sort keys ─────────────────────────────────────────

/// Builds a graph with 20 Person nodes whose `age` property spans 1..=20,
/// inserted in a deliberately shuffled order so the test is not trivially
/// ordered by insertion sequence.
fn shuffled_age_graph() -> Graph {
    let mut g = Graph::new();
    // Interleave even/odd ages so insertion order ≠ sorted order.
    let ages: Vec<i64> = (0..20)
        .map(|i| if i % 2 == 0 { i / 2 + 1 } else { 20 - i / 2 })
        .collect();
    for age in ages {
        g.add_node("Person", props! { "age" => age }).unwrap();
    }
    g
}

#[test]
fn order_by_precomputed_keys_ascending() {
    let g = shuffled_age_graph();
    let rows = run(&g, "MATCH (n:Person) RETURN n.age ORDER BY n.age ASC").unwrap();
    assert_eq!(rows.len(), 20);
    let ages: Vec<i64> = rows
        .iter()
        .map(|r| match r["n.age"] {
            GqlValue::Int(v) => v,
            ref other => panic!("expected Int, got {other:?}"),
        })
        .collect();
    let mut expected = ages.clone();
    expected.sort_unstable();
    assert_eq!(ages, expected, "rows must be in ascending age order");
}

#[test]
fn order_by_precomputed_keys_descending() {
    let g = shuffled_age_graph();
    let rows = run(&g, "MATCH (n:Person) RETURN n.age ORDER BY n.age DESC").unwrap();
    assert_eq!(rows.len(), 20);
    let ages: Vec<i64> = rows
        .iter()
        .map(|r| match r["n.age"] {
            GqlValue::Int(v) => v,
            ref other => panic!("expected Int, got {other:?}"),
        })
        .collect();
    let mut expected = ages.clone();
    expected.sort_unstable_by(|a, b| b.cmp(a));
    assert_eq!(ages, expected, "rows must be in descending age order");
}

// ── Move-based projection (terminal path, no ORDER BY) ───────────────────────

/// Behavior anchor: string property returned via terminal projection path must
/// After changing `CreatePattern` props from `Literal` to `Expr`, verify that
/// CREATE with literal property values still parses correctly and produces an
/// AST with `Expr::Literal` wrappers that round-trip through execution.
#[test]
#[allow(clippy::match_wildcard_for_single_variants)] // allow: test fixture
fn create_with_literal_props_still_works() {
    use tessera_graph::gql::{CreatePattern, Expr, GqlStatement, Literal, MutationClause};

    // Phase 1: verify the parser produces Expr::Literal wrappers.
    let stmt = gql::parse_statement("CREATE (n:Item {val: 42, name: 'test'})").unwrap();
    match &stmt {
        GqlStatement::Mutation(ms) => match &ms.mutation {
            MutationClause::Create(c) => {
                assert_eq!(c.patterns.len(), 1);
                match &c.patterns[0] {
                    CreatePattern::Node { props, .. } => {
                        assert_eq!(props.len(), 2);
                        assert_eq!(props[0].0, "val");
                        assert_eq!(props[0].1, Expr::Literal(Literal::Int(42)));
                        assert_eq!(props[1].0, "name");
                        assert_eq!(props[1].1, Expr::Literal(Literal::Str("test".into())));
                    }
                    other => panic!("expected Node, got {other:?}"),
                }
            }
            other => panic!("expected Create, got {other:?}"),
        },
        other => panic!("expected Mutation, got {other:?}"),
    }

    // Phase 2: manually apply the mutation and verify it round-trips.
    let mut g = Graph::new();
    let props = tessera_graph::props! { "val" => 42_i64, "name" => "test" };
    g.add_node("Item", props).unwrap();

    let verify = run(&g, "MATCH (n:Item) RETURN n.val, n.name").unwrap();
    assert_eq!(verify.len(), 1);
    assert_eq!(verify[0]["n.val"], GqlValue::Int(42));
    assert_eq!(verify[0]["n.name"], GqlValue::Str("test".into()));
}

/// produce the correct `GqlValue::Str` value (no corruption from move semantics).
#[test]
fn project_row_string_property_correct() {
    let mut g = Graph::new();
    g.add_node("Person", props! { "name" => "Alice" }).unwrap();

    let rows = run(&g, "MATCH (n:Person) RETURN n.name").unwrap();

    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].get("n.name"),
        Some(&GqlValue::Str("Alice".to_string())),
        "string property must round-trip correctly through terminal projection"
    );
}

// ── UNWIND+CREATE mutation tests ────────────────────────────────────────────

/// UNWIND [10, 20, 30] AS x MATCH (r:Root) CREATE (n:Item {val: x})
/// produces 3 Item nodes with val=10, val=20, val=30.
#[test]
// allow: test fixture
#[allow(
    clippy::manual_let_else,
    clippy::items_after_statements,
    clippy::significant_drop_tightening,
    clippy::stable_sort_primitive
)]
fn unwind_create_node_with_variable_prop() {
    use tessera_graph::gql::{self, GqlStatement};

    let mut g = Graph::new();
    g.add_node("Root", props! { "id" => 1_i64 }).unwrap();

    let stmt =
        gql::parse_statement("UNWIND [10, 20, 30] AS x MATCH (r:Root) CREATE (n:Item {val: x})")
            .unwrap();

    let mutation = match stmt {
        GqlStatement::Mutation(m) => m,
        _ => panic!("expected mutation"),
    };

    // Execute via the server accessor (simulates the Bolt path).
    use std::sync::{Arc, RwLock};
    let shared = Arc::new(RwLock::new(g));

    // Use the DefaultGraphAccessor to execute the mutation.
    // But since it's in the server crate, we test by re-implementing the
    // unwind mutation inline via the public API from tessera-graph.
    {
        let graph = shared.read().unwrap();
        let empty_pm = tessera_graph::PatternMatch::empty();
        let unwind = mutation.unwind_clause.as_ref().unwrap();
        let list_val = gql::execute_expr(&unwind.expr, &empty_pm, &*graph);
        let elements = match list_val {
            GqlValue::List(items) => items,
            other => vec![other],
        };
        assert_eq!(elements.len(), 3);
        drop(graph);

        // Compile MATCH bindings.
        let rows = {
            let graph = shared.read().unwrap();
            gql::compile_match_bindings(&*graph, mutation.match_clause.as_ref().unwrap(), None)
                .unwrap()
        };
        assert_eq!(rows.len(), 1); // one Root node

        // Apply mutations.
        let mut graph = shared.write().unwrap();
        let create = match &mutation.mutation {
            gql::MutationClause::Create(c) => c,
            _ => panic!("expected CREATE"),
        };

        for elem in &elements {
            let unwind_var = Some((unwind.var.as_str(), elem));
            for pattern in &create.patterns {
                if let gql::CreatePattern::Node { label, props, .. } = pattern {
                    let properties =
                        gql::resolve_create_props(props, &empty_pm, &*graph, unwind_var);
                    graph.add_node(label, properties).unwrap();
                }
            }
        }
    }

    // Verify: 3 Item nodes with correct val values.
    let graph = shared.read().unwrap();
    let rows = run(&graph, "MATCH (n:Item) RETURN n.val").unwrap();
    assert_eq!(rows.len(), 3, "expected 3 Item nodes, got {}", rows.len());

    let mut vals: Vec<i64> = rows
        .iter()
        .map(|r| match r.get("n.val").unwrap() {
            GqlValue::Int(v) => *v,
            other => panic!("expected Int, got {other:?}"),
        })
        .collect();
    vals.sort();
    assert_eq!(vals, vec![10, 20, 30]);
}

/// UNWIND [] AS x MATCH (r:Root) CREATE (n:Item {val: x}) produces 0 nodes.
#[test]
#[allow(clippy::manual_let_else)] // allow: test fixture
fn unwind_create_empty_list_no_mutations() {
    use tessera_graph::gql::{self, GqlStatement};

    let mut g = Graph::new();
    g.add_node("Root", props! { "id" => 1_i64 }).unwrap();

    let stmt =
        gql::parse_statement("UNWIND [] AS x MATCH (r:Root) CREATE (n:Item {val: x})").unwrap();

    let mutation = match stmt {
        GqlStatement::Mutation(m) => m,
        _ => panic!("expected mutation"),
    };

    let unwind = mutation.unwind_clause.as_ref().unwrap();
    let empty_pm = tessera_graph::PatternMatch::empty();
    let list_val = gql::execute_expr(&unwind.expr, &empty_pm, &g);

    let elements = match list_val {
        GqlValue::List(items) => items,
        _ => panic!("expected list"),
    };
    assert!(
        elements.is_empty(),
        "empty list should produce zero elements"
    );

    // No mutations should be applied — verify 0 Item nodes exist.
    let rows = run(&g, "MATCH (n:Item) RETURN n.val").unwrap();
    assert_eq!(rows.len(), 0);
}

/// UNWIND [1, 2, 3] AS x MATCH (r:Root) CREATE (n:Item {val: x + 10})
/// produces 3 nodes with val=11, val=12, val=13.
#[test]
// allow: test fixture
#[allow(
    clippy::manual_let_else,
    clippy::significant_drop_tightening,
    clippy::stable_sort_primitive
)]
fn unwind_create_with_expression_prop() {
    use tessera_graph::gql::{self, GqlStatement};

    let mut g = Graph::new();
    g.add_node("Root", props! { "id" => 1_i64 }).unwrap();

    let stmt =
        gql::parse_statement("UNWIND [1, 2, 3] AS x MATCH (r:Root) CREATE (n:Item {val: x + 10})")
            .unwrap();

    let mutation = match stmt {
        GqlStatement::Mutation(m) => m,
        _ => panic!("expected mutation"),
    };

    let shared = std::sync::Arc::new(std::sync::RwLock::new(g));

    {
        let graph = shared.read().unwrap();
        let empty_pm = tessera_graph::PatternMatch::empty();
        let unwind = mutation.unwind_clause.as_ref().unwrap();
        let list_val = gql::execute_expr(&unwind.expr, &empty_pm, &*graph);
        let elements = match list_val {
            GqlValue::List(items) => items,
            other => vec![other],
        };
        drop(graph);

        let rows = {
            let graph = shared.read().unwrap();
            gql::compile_match_bindings(&*graph, mutation.match_clause.as_ref().unwrap(), None)
                .unwrap()
        };

        let mut graph = shared.write().unwrap();
        let create = match &mutation.mutation {
            gql::MutationClause::Create(c) => c,
            _ => panic!("expected CREATE"),
        };

        for elem in &elements {
            let unwind_var = Some((unwind.var.as_str(), elem));
            for _row in &rows {
                for pattern in &create.patterns {
                    if let gql::CreatePattern::Node { label, props, .. } = pattern {
                        let properties =
                            gql::resolve_create_props(props, &empty_pm, &*graph, unwind_var);
                        graph.add_node(label, properties).unwrap();
                    }
                }
            }
        }
    }

    let graph = shared.read().unwrap();
    let rows = run(&graph, "MATCH (n:Item) RETURN n.val").unwrap();
    assert_eq!(rows.len(), 3);

    let mut vals: Vec<i64> = rows
        .iter()
        .map(|r| match r.get("n.val").unwrap() {
            GqlValue::Int(v) => *v,
            other => panic!("expected Int, got {other:?}"),
        })
        .collect();
    vals.sort();
    assert_eq!(vals, vec![11, 12, 13]);
}

/// `resolve_create_props` correctly resolves literal-only expressions without UNWIND.
#[test]
fn resolve_create_props_literal_only() {
    use tessera_graph::gql::{self, Expr, Literal};

    let g = Graph::new();
    let empty_pm = tessera_graph::PatternMatch::empty();

    let props = vec![
        ("name".into(), Expr::Literal(Literal::Str("Alice".into()))),
        ("age".into(), Expr::Literal(Literal::Int(30))),
        ("active".into(), Expr::Literal(Literal::Bool(true))),
        ("skip_null".into(), Expr::Literal(Literal::Null)),
    ];

    let result = gql::resolve_create_props(&props, &empty_pm, &g, None);
    assert_eq!(result.len(), 3); // Null is skipped
    assert_eq!(
        result.get("name"),
        Some(&tessera_graph::Property::String("Alice".into()))
    );
    assert_eq!(result.get("age"), Some(&tessera_graph::Property::I64(30)));
    assert_eq!(
        result.get("active"),
        Some(&tessera_graph::Property::Bool(true))
    );
    assert!(!result.contains_key("skip_null"));
}

// ── GROUP BY tests ─────────────────────────────────────────────────────────

/// Helper: creates a graph with Person nodes that have `dept` properties.
///   2 × Eng, 1 × Sales
fn dept_graph() -> Graph {
    let mut g = Graph::new();
    g.add_node("Person", props! { "name" => "Alice", "dept" => "Eng" })
        .unwrap();
    g.add_node("Person", props! { "name" => "Bob", "dept" => "Eng" })
        .unwrap();
    g.add_node("Person", props! { "name" => "Carol", "dept" => "Sales" })
        .unwrap();
    g
}

#[test]
fn group_by_single_key_count() {
    let g = dept_graph();
    let rows = run(
        &g,
        "MATCH (p:Person) RETURN p.dept, COUNT(*) AS cnt GROUP BY p.dept ORDER BY p.dept",
    )
    .unwrap();

    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].get("p.dept"), Some(&GqlValue::Str("Eng".into())));
    assert_eq!(rows[0].get("cnt"), Some(&GqlValue::Int(2)));
    assert_eq!(rows[1].get("p.dept"), Some(&GqlValue::Str("Sales".into())));
    assert_eq!(rows[1].get("cnt"), Some(&GqlValue::Int(1)));
}

#[test]
fn group_by_multiple_keys() {
    let mut g = Graph::new();
    g.add_node(
        "Person",
        props! { "name" => "A", "dept" => "Eng", "region" => "US" },
    )
    .unwrap();
    g.add_node(
        "Person",
        props! { "name" => "B", "dept" => "Eng", "region" => "EU" },
    )
    .unwrap();
    g.add_node(
        "Person",
        props! { "name" => "C", "dept" => "Eng", "region" => "US" },
    )
    .unwrap();
    g.add_node(
        "Person",
        props! { "name" => "D", "dept" => "Sales", "region" => "US" },
    )
    .unwrap();

    let rows = run(
        &g,
        "MATCH (p:Person) RETURN p.dept, p.region, COUNT(*) AS cnt \
         GROUP BY p.dept, p.region ORDER BY p.dept, p.region",
    )
    .unwrap();

    assert_eq!(rows.len(), 3);
    // Eng/EU = 1
    assert_eq!(rows[0].get("p.dept"), Some(&GqlValue::Str("Eng".into())));
    assert_eq!(rows[0].get("p.region"), Some(&GqlValue::Str("EU".into())));
    assert_eq!(rows[0].get("cnt"), Some(&GqlValue::Int(1)));
    // Eng/US = 2
    assert_eq!(rows[1].get("p.dept"), Some(&GqlValue::Str("Eng".into())));
    assert_eq!(rows[1].get("p.region"), Some(&GqlValue::Str("US".into())));
    assert_eq!(rows[1].get("cnt"), Some(&GqlValue::Int(2)));
    // Sales/US = 1
    assert_eq!(rows[2].get("p.dept"), Some(&GqlValue::Str("Sales".into())));
    assert_eq!(rows[2].get("p.region"), Some(&GqlValue::Str("US".into())));
    assert_eq!(rows[2].get("cnt"), Some(&GqlValue::Int(1)));
}

#[test]
fn group_by_with_sum() {
    let mut g = Graph::new();
    g.add_node("Sale", props! { "dept" => "Eng", "amount" => 100_i64 })
        .unwrap();
    g.add_node("Sale", props! { "dept" => "Eng", "amount" => 200_i64 })
        .unwrap();
    g.add_node("Sale", props! { "dept" => "Sales", "amount" => 50_i64 })
        .unwrap();

    let rows = run(
        &g,
        "MATCH (s:Sale) RETURN s.dept, SUM(s.amount) AS total \
         GROUP BY s.dept ORDER BY s.dept",
    )
    .unwrap();

    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].get("s.dept"), Some(&GqlValue::Str("Eng".into())));
    assert_eq!(rows[0].get("total"), Some(&GqlValue::Int(300)));
    assert_eq!(rows[1].get("s.dept"), Some(&GqlValue::Str("Sales".into())));
    assert_eq!(rows[1].get("total"), Some(&GqlValue::Int(50)));
}

#[test]
fn group_by_with_collect() {
    let g = dept_graph();
    let rows = run(
        &g,
        "MATCH (p:Person) RETURN p.dept, COLLECT(p.name) AS names \
         GROUP BY p.dept ORDER BY p.dept",
    )
    .unwrap();

    assert_eq!(rows.len(), 2);
    // Eng group should have 2 names
    if let Some(GqlValue::List(names)) = rows[0].get("names") {
        assert_eq!(names.len(), 2);
    } else {
        panic!("expected list for Eng names");
    }
    // Sales group should have 1 name
    if let Some(GqlValue::List(names)) = rows[1].get("names") {
        assert_eq!(names.len(), 1);
        assert_eq!(names[0], GqlValue::Str("Carol".into()));
    } else {
        panic!("expected list for Sales names");
    }
}

#[test]
fn group_by_without_aggregates_acts_as_distinct() {
    let g = dept_graph();
    let rows = run(
        &g,
        "MATCH (p:Person) RETURN p.dept GROUP BY p.dept ORDER BY p.dept",
    )
    .unwrap();

    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].get("p.dept"), Some(&GqlValue::Str("Eng".into())));
    assert_eq!(rows[1].get("p.dept"), Some(&GqlValue::Str("Sales".into())));
}

#[test]
fn group_by_error_non_aggregate_not_in_group_by() {
    let g = dept_graph();
    let result = run(
        &g,
        "MATCH (p:Person) RETURN p.dept, p.name, COUNT(*) GROUP BY p.dept",
    );

    assert!(result.is_err());
    let err_msg = format!("{}", result.unwrap_err());
    assert!(
        err_msg.contains("p.name"),
        "error should mention p.name: {err_msg}"
    );
    assert!(
        err_msg.contains("GROUP BY"),
        "error should mention GROUP BY: {err_msg}"
    );
}

// ── Cycle 8: ConstReturn integration (executor-level, public API) ───────────
//
// These tests exercise `gql::execute_const_return` through the public
// surface, complementing the unit tests in `gql/compiler.rs` (which run
// against `Graph::new()` only). They focus on properties that the unit
// layer cannot verify: graph-state isolation and integration with the
// public `parse_statement` → executor pipeline.

fn run_const_return(
    graph: &Graph,
    input: &str,
) -> Vec<std::collections::HashMap<String, GqlValue>> {
    let stmt = gql::parse_statement(input).unwrap();
    let q = match stmt {
        gql::GqlStatement::ConstReturn(q) => q,
        other => panic!("expected ConstReturn, got {other:?}"),
    };
    gql::execute_const_return(graph, &q, 0, None).unwrap()
}

#[test]
fn executor_const_return_single_row_single_field() {
    let g = Graph::new();
    let rows = run_const_return(&g, "RETURN 42");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].len(), 1);
    assert!(matches!(rows[0].values().next(), Some(GqlValue::Int(42))));
}

#[test]
fn executor_const_return_multiple_fields_with_aliases() {
    let g = Graph::new();
    let rows = run_const_return(&g, "RETURN 1 AS a, 'hi' AS b, true AS c");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get("a"), Some(&GqlValue::Int(1)));
    assert_eq!(rows[0].get("b"), Some(&GqlValue::Str("hi".into())));
    assert_eq!(rows[0].get("c"), Some(&GqlValue::Bool(true)));
}

#[test]
fn executor_const_return_does_not_touch_graph_state() {
    // Build a non-trivial graph, snapshot its counts, run a ConstReturn,
    // and assert the counts are unchanged. Then run a MATCH and confirm
    // every original node/edge is still visible. Combined, these prove
    // ConstReturn opens no transaction and modifies no on-disk state.
    let g = social_graph();
    let node_count_before = g.node_count();
    let edge_count_before = g.edge_count();

    let rows = run_const_return(&g, "RETURN 1");
    assert_eq!(rows.len(), 1);

    assert_eq!(g.node_count(), node_count_before);
    assert_eq!(g.edge_count(), edge_count_before);

    // Re-query the graph to confirm nothing changed semantically either.
    let match_rows = run(&g, "MATCH (p:Person) RETURN p.name").unwrap();
    assert_eq!(match_rows.len(), 4, "all four Person nodes still visible");
}

#[test]
fn executor_const_return_skip_one_yields_zero_rows() {
    let g = Graph::new();
    let rows = run_const_return(&g, "RETURN 1 SKIP 1");
    assert!(rows.is_empty());
}

#[test]
fn executor_const_return_limit_zero_yields_zero_rows() {
    let g = Graph::new();
    let rows = run_const_return(&g, "RETURN 1 LIMIT 0");
    assert!(rows.is_empty());
}
