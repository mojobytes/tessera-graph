// SPDX-License-Identifier: LicenseRef-TesseraGraph-Proprietary

//! Integration tests for the GQL `WITH` pipeline clause.
//!
//! Each test exercises a distinct semantic of WITH end-to-end through
//! `gql::parse_statement` (+ `execute_pipeline`). No mocks — real
//! in-memory `Graph`.

/// Phase 1 RED: `Token::With` does not exist yet. Once added, the lexer
/// must tokenize `WITH` (any case) as `Token::With`, NOT as an identifier.
#[test]
fn with_is_tokenized_as_keyword_not_identifier() {
    use tessera_graph::gql::token::Token;

    // Lexer produces a dedicated Token::With variant.
    let tokens = tessera_graph::gql::lexer::Lexer::new("WITH").tokenize().unwrap();
    assert!(
        matches!(tokens.first().map(|t| &t.token), Some(Token::With)),
        "expected first token to be Token::With, got {:?}",
        tokens.first().map(|t| &t.token)
    );

    // Case-insensitive.
    let tokens_lc = tessera_graph::gql::lexer::Lexer::new("with").tokenize().unwrap();
    assert!(matches!(
        tokens_lc.first().map(|t| &t.token),
        Some(Token::With)
    ));

    // Display form is uppercase "WITH".
    assert_eq!(Token::With.to_string(), "WITH");
}

/// Phase 2 RED: the pipeline AST types are constructible.
#[test]
fn pipeline_ast_is_constructible() {
    use tessera_graph::gql::{
        Expr, Literal, MatchClause, PipelineQuery, PipelineStage, PipelineTerminal,
        ReturnClause, ReturnItem, SkipClause, WithClause,
    };

    let stage = PipelineStage::With(WithClause {
        distinct: false,
        items: vec![ReturnItem {
            expr: Expr::Var("a".into()),
            alias: None,
        }],
        where_clause: None,
        order_by: None,
        skip: Some(SkipClause { count: 0 }),
        limit: None,
    });

    let pq = PipelineQuery {
        stages: vec![
            PipelineStage::Match {
                clause: MatchClause { patterns: vec![], path_var: None },
                where_clause: None,
            },
            stage,
        ],
        terminal: PipelineTerminal::Return {
            clause: ReturnClause {
                distinct: false,
                items: vec![ReturnItem {
                    expr: Expr::Literal(Literal::Int(1)),
                    alias: None,
                }],
            },
            order_by: None,
            skip: None,
            limit: None,
        },
    };

    assert_eq!(pq.stages.len(), 2);
    assert!(matches!(pq.terminal, PipelineTerminal::Return { .. }));
}

/// Phase 2 RED: `Expr::Subscript` and `Expr::ListLit` exist.
#[test]
fn expr_subscript_and_list_lit_constructible() {
    use tessera_graph::gql::{Expr, Literal};

    let lst = Expr::ListLit(vec![
        Expr::Literal(Literal::Int(10)),
        Expr::Literal(Literal::Int(20)),
    ]);
    let sub = Expr::Subscript {
        list: Box::new(lst),
        index: Box::new(Expr::Literal(Literal::Int(1))),
    };
    assert!(matches!(sub, Expr::Subscript { .. }));
}

// ── Phase 3 RED: parser routes WITH inputs to `PipelineQuery` ───────────────

/// `MATCH (a) WITH a RETURN a` parses into a pipeline with two stages (MATCH,
/// WITH) and a RETURN terminal.
#[test]
fn parse_simple_match_with_return_produces_pipeline() {
    use tessera_graph::gql::{GqlStatement, PipelineStage, PipelineTerminal};

    let stmt = tessera_graph::gql::parse_statement("MATCH (a) WITH a RETURN a").unwrap();
    let GqlStatement::Pipeline(pq) = stmt else {
        panic!("expected Pipeline, got {stmt:?}");
    };
    assert_eq!(pq.stages.len(), 2);
    assert!(matches!(pq.stages[0], PipelineStage::Match { .. }));
    assert!(matches!(pq.stages[1], PipelineStage::With(_)));
    assert!(matches!(pq.terminal, PipelineTerminal::Return { .. }));
}

/// `MATCH (a) WITH a WHERE a.age > 30 ORDER BY a.id SKIP 1 LIMIT 10 RETURN a`
/// exercises every optional sub-clause of WITH.
#[test]
fn parse_with_full_options() {
    use tessera_graph::gql::{GqlStatement, PipelineStage};

    let stmt = tessera_graph::gql::parse_statement(
        "MATCH (a) WITH a WHERE a.age > 30 ORDER BY a.id SKIP 1 LIMIT 10 RETURN a",
    )
    .unwrap();
    let GqlStatement::Pipeline(pq) = stmt else {
        panic!("expected Pipeline");
    };
    let PipelineStage::With(w) = &pq.stages[1] else {
        panic!("expected second stage to be WITH");
    };
    assert!(w.where_clause.is_some(), "WITH WHERE missing");
    assert!(w.order_by.is_some(), "WITH ORDER BY missing");
    assert_eq!(w.skip.map(|s| s.count), Some(1));
    assert_eq!(w.limit.map(|l| l.count), Some(10));
}

/// Two chained WITH stages produce three pipeline stages before the terminal.
#[test]
fn parse_two_chained_with_stages() {
    use tessera_graph::gql::{GqlStatement, PipelineStage};

    let stmt = tessera_graph::gql::parse_statement(
        "MATCH (a) WITH a ORDER BY a.id WITH a RETURN a",
    )
    .unwrap();
    let GqlStatement::Pipeline(pq) = stmt else {
        panic!("expected Pipeline");
    };
    assert_eq!(pq.stages.len(), 3);
    assert!(matches!(pq.stages[0], PipelineStage::Match { .. }));
    assert!(matches!(pq.stages[1], PipelineStage::With(_)));
    assert!(matches!(pq.stages[2], PipelineStage::With(_)));
}

/// UNWIND after WITH produces an `Unwind` stage inside the pipeline.
#[test]
fn parse_with_then_unwind_pipeline() {
    use tessera_graph::gql::{GqlStatement, PipelineStage};

    let stmt = tessera_graph::gql::parse_statement(
        "MATCH (a) WITH collect(a) AS nodes \
         UNWIND nodes AS x \
         WITH x AS a \
         RETURN a",
    )
    .unwrap();
    let GqlStatement::Pipeline(pq) = stmt else {
        panic!("expected Pipeline");
    };
    assert_eq!(pq.stages.len(), 4);
    assert!(matches!(pq.stages[0], PipelineStage::Match { .. }));
    assert!(matches!(pq.stages[1], PipelineStage::With(_)));
    assert!(matches!(pq.stages[2], PipelineStage::Unwind(_)));
    assert!(matches!(pq.stages[3], PipelineStage::With(_)));
}

/// `WITH ... SET` produces a `PipelineTerminal::Set` terminal.
#[test]
fn parse_with_set_mutation_terminal() {
    use tessera_graph::gql::{GqlStatement, PipelineTerminal};

    let stmt = tessera_graph::gql::parse_statement(
        "MATCH (a:Person) WITH a WITH a AS b SET b.x = 1",
    )
    .unwrap();
    let GqlStatement::Pipeline(pq) = stmt else {
        panic!("expected Pipeline");
    };
    assert!(matches!(pq.terminal, PipelineTerminal::Set(_)));
}

/// Legacy flat queries (no WITH) MUST still parse as `GqlStatement::Query`,
/// not as `Pipeline`. This guards against accidentally routing everything
/// through the pipeline AST.
#[test]
fn parse_query_without_with_stays_flat() {
    use tessera_graph::gql::GqlStatement;

    let stmt = tessera_graph::gql::parse_statement("MATCH (a:Person) RETURN a").unwrap();
    assert!(
        matches!(stmt, GqlStatement::Query(_)),
        "legacy flat path must remain; got {stmt:?}"
    );
}

// ── Phase 4 RED: parse list subscript and range/size builtins ───────────────

/// `RETURN lst[1]` parses as an `Expr::Subscript`.
#[test]
fn parse_list_subscript_in_return() {
    use tessera_graph::gql::{Expr, GqlStatement};

    let stmt = tessera_graph::gql::parse_statement(
        "MATCH (a) WITH [10, 20, 30] AS lst RETURN lst[1]",
    )
    .unwrap();
    let GqlStatement::Pipeline(pq) = stmt else {
        panic!("expected Pipeline");
    };
    let tessera_graph::gql::PipelineTerminal::Return { clause, .. } = &pq.terminal else {
        panic!("expected Return terminal");
    };
    assert!(
        matches!(&clause.items[0].expr, Expr::Subscript { .. }),
        "expected Subscript, got {:?}",
        clause.items[0].expr
    );
}

/// An all-literal list `[1, 2, 3]` parses as `Literal::List` (existing
/// behaviour preserved). A list containing non-literal elements parses as
/// `Expr::ListLit` (new behaviour, added by Phase 4 so that references like
/// `nodes[i]` work when `nodes` is a bound variable).
#[test]
fn parse_list_literal_homogeneous_vs_heterogeneous() {
    use tessera_graph::gql::{Expr, GqlStatement, Literal};

    // All-literal: keeps `Literal::List` — backwards-compatible.
    let stmt = tessera_graph::gql::parse_statement(
        "MATCH (a) WITH [1, 2, 3] AS xs RETURN xs",
    )
    .unwrap();
    let GqlStatement::Pipeline(pq) = stmt else {
        panic!();
    };
    let tessera_graph::gql::PipelineStage::With(w) = &pq.stages[1] else {
        panic!();
    };
    assert!(matches!(&w.items[0].expr, Expr::Literal(Literal::List(_))));

    // Contains a variable reference: becomes `Expr::ListLit`.
    let stmt = tessera_graph::gql::parse_statement(
        "MATCH (a) WITH [1, a, 3] AS xs RETURN xs",
    )
    .unwrap();
    let GqlStatement::Pipeline(pq) = stmt else {
        panic!();
    };
    let tessera_graph::gql::PipelineStage::With(w) = &pq.stages[1] else {
        panic!();
    };
    assert!(matches!(&w.items[0].expr, Expr::ListLit(_)));
}

/// The full target query parses without error (no execution yet).
/// This is the parser-level milestone for unblocking
/// `ensure_asset_indices_assigned`.
#[test]
fn parse_ensure_asset_indices_assigned_query() {
    let stmt = tessera_graph::gql::parse_statement(
        "MATCH (a:AssetNode) \
         WITH a ORDER BY a.id \
         WITH collect(a) AS nodes \
         UNWIND range(0, size(nodes) - 1) AS i \
         WITH nodes[i] AS a, i AS idx \
         SET a.asset_idx = idx",
    );
    assert!(stmt.is_ok(), "parse should succeed; got: {stmt:?}");
}

// ── Phase 5 RED: execute a minimal passthrough pipeline ─────────────────────

/// T01-equivalent: `MATCH (a:Person) WITH a RETURN a` must return the same
/// number of rows as `MATCH (a:Person) RETURN a`. The WITH stage projects `a`
/// unchanged; this exercises the core pipeline executor with one read-only
/// WITH stage.
#[test]
fn execute_simple_passthrough_pipeline() {
    use tessera_graph::{props, Graph};

    let mut g = Graph::new();
    g.add_node("Person", props! { "name" => "Alice", "age" => 30_i64 }).unwrap();
    g.add_node("Person", props! { "name" => "Bob", "age" => 40_i64 }).unwrap();
    g.add_node("Thing", props! { "name" => "Car" }).unwrap();

    let stmt = tessera_graph::gql::parse_statement(
        "MATCH (a:Person) WITH a RETURN a",
    )
    .unwrap();
    let tessera_graph::gql::GqlStatement::Pipeline(ref pq) = stmt else {
        panic!("expected Pipeline");
    };
    let rows = tessera_graph::gql::execute_pipeline(&g, pq, 0).unwrap();
    assert_eq!(rows.len(), 2, "expected 2 Person nodes, got {rows:?}");
    assert!(rows.iter().all(|r| r.contains_key("a")));
}

/// T02-equivalent: `MATCH (a:Person) WITH a AS person RETURN person` renames
/// the column. Tests that WITH aliases propagate into the next stage's scope.
#[test]
fn execute_with_alias_rename() {
    use tessera_graph::{props, Graph};

    let mut g = Graph::new();
    g.add_node("Person", props! { "name" => "Alice" }).unwrap();

    let stmt = tessera_graph::gql::parse_statement(
        "MATCH (a:Person) WITH a AS person RETURN person",
    )
    .unwrap();
    let tessera_graph::gql::GqlStatement::Pipeline(ref pq) = stmt else {
        panic!();
    };
    let rows = tessera_graph::gql::execute_pipeline(&g, pq, 0).unwrap();
    assert_eq!(rows.len(), 1);
    assert!(
        rows[0].contains_key("person"),
        "expected alias 'person' in {rows:?}"
    );
    assert!(!rows[0].contains_key("a"), "original name must not leak");
}

// ── Phase 6 RED: WITH WHERE / ORDER BY / LIMIT / SKIP / DISTINCT / agg ──────

fn ages_graph() -> tessera_graph::Graph {
    use tessera_graph::{props, Graph};
    let mut g = Graph::new();
    g.add_node("Person", props! { "name" => "Alice", "age" => 30_i64, "dept" => "A" }).unwrap();
    g.add_node("Person", props! { "name" => "Bob",   "age" => 40_i64, "dept" => "A" }).unwrap();
    g.add_node("Person", props! { "name" => "Carol", "age" => 25_i64, "dept" => "B" }).unwrap();
    g.add_node("Person", props! { "name" => "Dave",  "age" => 50_i64, "dept" => "B" }).unwrap();
    g
}

/// T03: `WITH a.name AS name RETURN name` — scalar projection.
#[test]
fn execute_with_property_projection() {
    use tessera_graph::GqlValue;

    let g = ages_graph();
    let stmt = tessera_graph::gql::parse_statement(
        "MATCH (a:Person) WITH a.name AS name RETURN name",
    )
    .unwrap();
    let tessera_graph::gql::GqlStatement::Pipeline(ref pq) = stmt else {
        panic!();
    };
    let rows = tessera_graph::gql::execute_pipeline(&g, pq, 0).unwrap();
    assert_eq!(rows.len(), 4);
    let names: std::collections::HashSet<String> = rows
        .iter()
        .filter_map(|r| match r.get("name") {
            Some(GqlValue::Str(s)) => Some(s.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        names,
        ["Alice", "Bob", "Carol", "Dave"].into_iter().map(String::from).collect(),
    );
}

/// T04: `WITH a WHERE a.age > 30 RETURN a` filters the bindings.
#[test]
fn execute_with_where_filter() {
    let g = ages_graph();
    let stmt = tessera_graph::gql::parse_statement(
        "MATCH (a:Person) WITH a WHERE a.age > 30 RETURN a",
    )
    .unwrap();
    let tessera_graph::gql::GqlStatement::Pipeline(ref pq) = stmt else {
        panic!();
    };
    let rows = tessera_graph::gql::execute_pipeline(&g, pq, 0).unwrap();
    // Bob(40), Dave(50) survive.
    assert_eq!(rows.len(), 2);
}

/// T05: `WITH a ORDER BY a.age RETURN a.name AS name` sorts ascending.
#[test]
fn execute_with_order_by_ascending() {
    use tessera_graph::GqlValue;

    let g = ages_graph();
    let stmt = tessera_graph::gql::parse_statement(
        "MATCH (a:Person) WITH a ORDER BY a.age RETURN a.name AS name",
    )
    .unwrap();
    let tessera_graph::gql::GqlStatement::Pipeline(ref pq) = stmt else {
        panic!();
    };
    let rows = tessera_graph::gql::execute_pipeline(&g, pq, 0).unwrap();
    let names: Vec<String> = rows
        .iter()
        .filter_map(|r| match r.get("name") {
            Some(GqlValue::Str(s)) => Some(s.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(names, vec!["Carol", "Alice", "Bob", "Dave"]);
}

/// T06: `WITH a ORDER BY a.age DESC`.
#[test]
fn execute_with_order_by_descending() {
    use tessera_graph::GqlValue;

    let g = ages_graph();
    let stmt = tessera_graph::gql::parse_statement(
        "MATCH (a:Person) WITH a ORDER BY a.age DESC RETURN a.name AS name",
    )
    .unwrap();
    let tessera_graph::gql::GqlStatement::Pipeline(ref pq) = stmt else {
        panic!();
    };
    let rows = tessera_graph::gql::execute_pipeline(&g, pq, 0).unwrap();
    let names: Vec<String> = rows
        .iter()
        .filter_map(|r| match r.get("name") {
            Some(GqlValue::Str(s)) => Some(s.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(names, vec!["Dave", "Bob", "Alice", "Carol"]);
}

/// T07: `WITH a LIMIT 2 RETURN a` truncates to 2 rows.
#[test]
fn execute_with_limit() {
    let g = ages_graph();
    let stmt = tessera_graph::gql::parse_statement(
        "MATCH (a:Person) WITH a LIMIT 2 RETURN a",
    )
    .unwrap();
    let tessera_graph::gql::GqlStatement::Pipeline(ref pq) = stmt else {
        panic!();
    };
    let rows = tessera_graph::gql::execute_pipeline(&g, pq, 0).unwrap();
    assert_eq!(rows.len(), 2);
}

/// SKIP+LIMIT: skip first 1, keep next 2.
#[test]
fn execute_with_skip_and_limit() {
    use tessera_graph::GqlValue;
    let g = ages_graph();
    let stmt = tessera_graph::gql::parse_statement(
        "MATCH (a:Person) WITH a ORDER BY a.age SKIP 1 LIMIT 2 RETURN a.name AS name",
    )
    .unwrap();
    let tessera_graph::gql::GqlStatement::Pipeline(ref pq) = stmt else {
        panic!();
    };
    let rows = tessera_graph::gql::execute_pipeline(&g, pq, 0).unwrap();
    let names: Vec<String> = rows
        .iter()
        .filter_map(|r| match r.get("name") {
            Some(GqlValue::Str(s)) => Some(s.clone()),
            _ => None,
        })
        .collect();
    // ages sorted: Carol(25), Alice(30), Bob(40), Dave(50)
    // SKIP 1 → [Alice, Bob, Dave]; LIMIT 2 → [Alice, Bob]
    assert_eq!(names, vec!["Alice", "Bob"]);
}

/// T08: `WITH collect(a) AS all RETURN all` — single-row aggregate; the
/// value must be a `GqlValue::List` with exactly one entry per node bound
/// by the MATCH. Each entry is the node's id (`GqlValue::Int`).
#[test]
fn execute_with_collect_list_contents() {
    use tessera_graph::GqlValue;

    let g = ages_graph();
    let stmt = tessera_graph::gql::parse_statement(
        "MATCH (a:Person) WITH collect(a) AS all RETURN all",
    )
    .unwrap();
    let tessera_graph::gql::GqlStatement::Pipeline(ref pq) = stmt else {
        panic!();
    };
    let rows = tessera_graph::gql::execute_pipeline(&g, pq, 0).unwrap();
    assert_eq!(rows.len(), 1, "collect without grouping emits one row");
    match rows[0].get("all") {
        Some(GqlValue::List(items)) => {
            assert_eq!(items.len(), 4, "expected 4 Person nodes, got {items:?}");
            for item in items {
                assert!(
                    matches!(item, GqlValue::Node(_)),
                    "since Fase B each collect(a) entry is a first-class Node, got {item:?}",
                );
            }
        }
        other => panic!("expected List value for 'all', got {other:?}"),
    }
}

/// T09: `WITH count(a) AS n RETURN n` — single-row aggregate.
#[test]
fn execute_with_count_aggregate() {
    use tessera_graph::GqlValue;

    let g = ages_graph();
    let stmt = tessera_graph::gql::parse_statement(
        "MATCH (a:Person) WITH count(a) AS n RETURN n",
    )
    .unwrap();
    let tessera_graph::gql::GqlStatement::Pipeline(ref pq) = stmt else {
        panic!();
    };
    let rows = tessera_graph::gql::execute_pipeline(&g, pq, 0).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get("n"), Some(&GqlValue::Int(4)));
}

/// T11: `WITH a.dept AS dept, count(a) AS n RETURN dept, n` — grouping.
#[test]
fn execute_with_groupby_via_mixed_projection() {
    use tessera_graph::GqlValue;

    let g = ages_graph();
    let stmt = tessera_graph::gql::parse_statement(
        "MATCH (a:Person) WITH a.dept AS dept, count(a) AS n RETURN dept, n",
    )
    .unwrap();
    let tessera_graph::gql::GqlStatement::Pipeline(ref pq) = stmt else {
        panic!();
    };
    let rows = tessera_graph::gql::execute_pipeline(&g, pq, 0).unwrap();
    assert_eq!(rows.len(), 2, "expected one row per dept: {rows:?}");
    let mut counts_by_dept: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
    for r in &rows {
        let dept = match r.get("dept") {
            Some(GqlValue::Str(s)) => s.clone(),
            other => panic!("bad dept: {other:?}"),
        };
        let n = match r.get("n") {
            Some(GqlValue::Int(v)) => *v,
            other => panic!("bad n: {other:?}"),
        };
        counts_by_dept.insert(dept, n);
    }
    assert_eq!(counts_by_dept.get("A"), Some(&2));
    assert_eq!(counts_by_dept.get("B"), Some(&2));
}

/// T20: `WITH DISTINCT a.dept AS dept RETURN dept` — 2 rows.
#[test]
fn execute_with_distinct() {
    use tessera_graph::GqlValue;

    let g = ages_graph();
    let stmt = tessera_graph::gql::parse_statement(
        "MATCH (a:Person) WITH DISTINCT a.dept AS dept RETURN dept",
    )
    .unwrap();
    let tessera_graph::gql::GqlStatement::Pipeline(ref pq) = stmt else {
        panic!();
    };
    let rows = tessera_graph::gql::execute_pipeline(&g, pq, 0).unwrap();
    assert_eq!(rows.len(), 2);
    let depts: std::collections::HashSet<String> = rows
        .iter()
        .filter_map(|r| match r.get("dept") {
            Some(GqlValue::Str(s)) => Some(s.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        depts,
        ["A", "B"].into_iter().map(String::from).collect()
    );
}

/// T10: two chained WITH stages preserve ordering.
#[test]
fn execute_two_chained_with_preserves_ordering() {
    use tessera_graph::GqlValue;

    let g = ages_graph();
    let stmt = tessera_graph::gql::parse_statement(
        "MATCH (a:Person) WITH a ORDER BY a.age WITH a RETURN a.name AS name",
    )
    .unwrap();
    let tessera_graph::gql::GqlStatement::Pipeline(ref pq) = stmt else {
        panic!();
    };
    let rows = tessera_graph::gql::execute_pipeline(&g, pq, 0).unwrap();
    let names: Vec<String> = rows
        .iter()
        .filter_map(|r| match r.get("name") {
            Some(GqlValue::Str(s)) => Some(s.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(names, vec!["Carol", "Alice", "Bob", "Dave"]);
}

/// T21: scope isolation — `a` is out of scope after `WITH a.name AS name`.
/// The pipeline scope validator rejects the reference to `a` in RETURN,
/// surfacing the typo/rename mistake as a compile-time error rather than
/// silently evaluating it to Null.
#[test]
fn execute_scope_isolation_after_with() {
    let g = ages_graph();
    let stmt = tessera_graph::gql::parse_statement(
        "MATCH (a:Person) WITH a.name AS name RETURN a",
    )
    .unwrap();
    let tessera_graph::gql::GqlStatement::Pipeline(ref pq) = stmt else {
        panic!();
    };
    let err = tessera_graph::gql::execute_pipeline(&g, pq, 0)
        .expect_err("resolving out-of-scope `a` must fail validation");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("'a'") && msg.contains("not bound"),
        "expected scope error mentioning 'a' not bound, got: {msg}"
    );
}

// ── Phase 7 RED: Subscript, ListLit, range(), size() evaluation ─────────────

/// T12: `range(0, 2)` returns `[0, 1, 2]`.
#[test]
fn execute_range_positive() {
    use tessera_graph::{props, Graph, GqlValue};

    let mut g = Graph::new();
    g.add_node("Person", props! { "name" => "Alice" }).unwrap();

    let stmt = tessera_graph::gql::parse_statement(
        "MATCH (a:Person) WITH range(0, 2) AS r RETURN r",
    )
    .unwrap();
    let tessera_graph::gql::GqlStatement::Pipeline(ref pq) = stmt else {
        panic!();
    };
    let rows = tessera_graph::gql::execute_pipeline(&g, pq, 0).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].get("r"),
        Some(&GqlValue::List(vec![
            GqlValue::Int(0),
            GqlValue::Int(1),
            GqlValue::Int(2),
        ])),
    );
}

/// Q11: `range()` must cap excessive ranges rather than exhaust memory /
/// loop forever. `range(0, i64::MAX)` would produce 2^63 elements without
/// a guard; the implementation must return `Null` (or an empty list)
/// once the capped capacity is exceeded.
#[test]
fn execute_range_caps_excessive_length() {
    use tessera_graph::{props, Graph, GqlValue};

    let mut g = Graph::new();
    g.add_node("Person", props! { "name" => "Alice" }).unwrap();

    // i64::MAX as a literal isn't parseable directly; use a large but
    // representable bound (exceeds the 1M cap by two orders of magnitude).
    let stmt = tessera_graph::gql::parse_statement(
        "MATCH (a:Person) WITH range(0, 100000000) AS r RETURN r",
    )
    .unwrap();
    let tessera_graph::gql::GqlStatement::Pipeline(ref pq) = stmt else {
        panic!();
    };
    let rows = tessera_graph::gql::execute_pipeline(&g, pq, 0).unwrap();
    assert_eq!(
        rows[0].get("r"),
        Some(&GqlValue::Null),
        "expected Null for range exceeding cap",
    );
}

/// Q11: boundary — `range(0, 999_999)` is exactly at the 1M cap and should
/// still succeed (`1_000_000` elements). `range(0, 1_000_000)` would be
/// `1_000_001` elements → exceeds cap.
#[test]
fn execute_range_at_cap_boundary() {
    use tessera_graph::{props, Graph, GqlValue};

    let mut g = Graph::new();
    g.add_node("Person", props! { "name" => "Alice" }).unwrap();

    // 1_000_000 elements — at the cap.
    let stmt = tessera_graph::gql::parse_statement(
        "MATCH (a:Person) WITH range(0, 999999) AS r RETURN r",
    )
    .unwrap();
    let tessera_graph::gql::GqlStatement::Pipeline(ref pq) = stmt else {
        panic!();
    };
    let rows = tessera_graph::gql::execute_pipeline(&g, pq, 0).unwrap();
    match rows[0].get("r") {
        Some(GqlValue::List(items)) => assert_eq!(items.len(), 1_000_000),
        other => panic!("expected List of 1M elements, got {other:?}"),
    }
}

/// T13: `range(5, 2)` with start > end returns `[]`.
#[test]
fn execute_range_empty_when_start_gt_end() {
    use tessera_graph::{props, Graph, GqlValue};

    let mut g = Graph::new();
    g.add_node("Person", props! { "name" => "Alice" }).unwrap();

    let stmt = tessera_graph::gql::parse_statement(
        "MATCH (a:Person) WITH range(5, 2) AS r RETURN r",
    )
    .unwrap();
    let tessera_graph::gql::GqlStatement::Pipeline(ref pq) = stmt else {
        panic!();
    };
    let rows = tessera_graph::gql::execute_pipeline(&g, pq, 0).unwrap();
    assert_eq!(rows[0].get("r"), Some(&GqlValue::List(vec![])));
}

/// T14 (inline form, matches plan spec exactly): `WITH collect(a) AS lst,
/// size(collect(a)) AS n RETURN n` — inline `size(collect(...))` in the
/// same WITH projection produces the node count.
#[test]
fn execute_inline_size_collect_within_same_with() {
    use tessera_graph::GqlValue;

    let g = ages_graph();
    let stmt = tessera_graph::gql::parse_statement(
        "MATCH (a:Person) \
         WITH collect(a) AS lst, size(collect(a)) AS n \
         RETURN n",
    )
    .unwrap();
    let tessera_graph::gql::GqlStatement::Pipeline(ref pq) = stmt else {
        panic!();
    };
    let rows = tessera_graph::gql::execute_pipeline(&g, pq, 0).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get("n"), Some(&GqlValue::Int(4)));
}

/// T14 (variable-reference form, additional coverage): two-stage pipeline
/// where `size(lst)` reads a WITH-introduced variable in a later stage.
/// Complements the inline form above; both must return the same count.
#[test]
fn execute_size_on_collect() {
    use tessera_graph::GqlValue;

    let g = ages_graph();
    let stmt = tessera_graph::gql::parse_statement(
        "MATCH (a:Person) WITH collect(a) AS lst, count(a) AS n \
         WITH n, size(lst) AS sz RETURN n, sz",
    )
    .unwrap();
    let tessera_graph::gql::GqlStatement::Pipeline(ref pq) = stmt else {
        panic!();
    };
    let rows = tessera_graph::gql::execute_pipeline(&g, pq, 0).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get("n"), Some(&GqlValue::Int(4)));
    assert_eq!(rows[0].get("sz"), Some(&GqlValue::Int(4)));
}

/// T15: `[10, 20, 30][1]` → 20. Literal list via `Literal::List`.
#[test]
fn execute_subscript_on_literal_list() {
    use tessera_graph::{props, Graph, GqlValue};

    let mut g = Graph::new();
    g.add_node("Person", props! { "name" => "Alice" }).unwrap();

    let stmt = tessera_graph::gql::parse_statement(
        "MATCH (a:Person) WITH [10, 20, 30] AS lst RETURN lst[1]",
    )
    .unwrap();
    let tessera_graph::gql::GqlStatement::Pipeline(ref pq) = stmt else {
        panic!();
    };
    let rows = tessera_graph::gql::execute_pipeline(&g, pq, 0).unwrap();
    assert_eq!(rows.len(), 1);
    // Column surface name is "lst[1]".
    assert_eq!(rows[0].get("lst[1]"), Some(&GqlValue::Int(20)));
}

/// T16: `lst[5]` where `lst = [10, 20]` → Null (out-of-bounds).
#[test]
fn execute_subscript_out_of_bounds_returns_null() {
    use tessera_graph::{props, Graph, GqlValue};

    let mut g = Graph::new();
    g.add_node("Person", props! { "name" => "Alice" }).unwrap();

    let stmt = tessera_graph::gql::parse_statement(
        "MATCH (a:Person) WITH [10, 20] AS lst RETURN lst[5]",
    )
    .unwrap();
    let tessera_graph::gql::GqlStatement::Pipeline(ref pq) = stmt else {
        panic!();
    };
    let rows = tessera_graph::gql::execute_pipeline(&g, pq, 0).unwrap();
    assert_eq!(rows[0].get("lst[5]"), Some(&GqlValue::Null));
}

/// Q7: `Subscript` and `ListLit` must evaluate recursively in legacy
/// `MATCH ... RETURN` (no pipeline), not silently collapse to `Null`.
#[test]
fn execute_subscript_in_legacy_match_return() {
    use tessera_graph::{gql, props, Graph, GqlValue};

    let mut g = Graph::new();
    g.add_node("Person", props! { "name" => "Alice" }).unwrap();

    // No WITH — flat `MATCH ... RETURN`. Parser produces `GqlQuery`, not
    // `PipelineQuery`, and goes through `gql::execute`.
    let query = gql::parse("MATCH (a:Person) RETURN [10, 20, 30][1] AS x").unwrap();
    let rows = gql::execute(&g, &query, 0).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get("x"), Some(&GqlValue::Int(20)));
}

// ── Phase 8 RED: UNWIND as a pipeline stage ────────────────────────────────

/// T17-equivalent (read-only kernel of the target query):
/// `MATCH (a) WITH collect(a) AS nodes UNWIND range(0, size(nodes)-1) AS i
/// WITH nodes[i] AS a, i AS idx RETURN a, idx` — produces one row per node
/// with `idx` taking 0..N-1 and `a` the node at that position.
#[test]
fn execute_unwind_after_with_produces_one_row_per_element() {
    use tessera_graph::GqlValue;

    let g = ages_graph();
    let stmt = tessera_graph::gql::parse_statement(
        "MATCH (a:Person) \
         WITH collect(a) AS nodes \
         UNWIND range(0, size(nodes) - 1) AS i \
         WITH nodes[i] AS a, i AS idx \
         RETURN a, idx",
    )
    .unwrap();
    let tessera_graph::gql::GqlStatement::Pipeline(ref pq) = stmt else {
        panic!();
    };
    let rows = tessera_graph::gql::execute_pipeline(&g, pq, 0).unwrap();
    assert_eq!(rows.len(), 4, "expected one row per Person");

    // idx values are exactly 0..3, each appearing once.
    let mut idxs: Vec<i64> = rows
        .iter()
        .filter_map(|r| match r.get("idx") {
            Some(GqlValue::Int(v)) => Some(*v),
            _ => None,
        })
        .collect();
    idxs.sort_unstable();
    assert_eq!(idxs, vec![0, 1, 2, 3]);

    // Every row has a first-class node `a` (since Fase B `nodes[i]` over a
    // `collect(a)` list yields a `GqlValue::Node`, not the raw id).
    for r in &rows {
        assert!(matches!(r.get("a"), Some(GqlValue::Node(_))), "got {:?}", r.get("a"));
    }
}

/// Simple UNWIND of a literal list after WITH: expands each element.
#[test]
fn execute_unwind_literal_list_after_with() {
    use tessera_graph::{props, Graph, GqlValue};

    let mut g = Graph::new();
    g.add_node("Person", props! { "name" => "Alice" }).unwrap();

    let stmt = tessera_graph::gql::parse_statement(
        "MATCH (a:Person) WITH a UNWIND [10, 20, 30] AS n RETURN n",
    )
    .unwrap();
    let tessera_graph::gql::GqlStatement::Pipeline(ref pq) = stmt else {
        panic!();
    };
    let rows = tessera_graph::gql::execute_pipeline(&g, pq, 0).unwrap();
    // One Person × three list elements = three rows.
    assert_eq!(rows.len(), 3);
    let mut ns: Vec<i64> = rows
        .iter()
        .filter_map(|r| match r.get("n") {
            Some(GqlValue::Int(v)) => Some(*v),
            _ => None,
        })
        .collect();
    ns.sort_unstable();
    assert_eq!(ns, vec![10, 20, 30]);
}

/// UNWIND of an empty list produces zero rows (binding is dropped).
#[test]
fn execute_unwind_empty_list_drops_binding() {
    use tessera_graph::{props, Graph};

    let mut g = Graph::new();
    g.add_node("Person", props! { "name" => "Alice" }).unwrap();

    let stmt = tessera_graph::gql::parse_statement(
        "MATCH (a:Person) WITH a UNWIND [] AS n RETURN n",
    )
    .unwrap();
    let tessera_graph::gql::GqlStatement::Pipeline(ref pq) = stmt else {
        panic!();
    };
    let rows = tessera_graph::gql::execute_pipeline(&g, pq, 0).unwrap();
    assert_eq!(rows.len(), 0);
}

/// UNWIND of a non-list (null) value produces zero rows — Cypher semantics.
#[test]
fn execute_unwind_null_drops_binding() {
    use tessera_graph::{props, Graph};

    let mut g = Graph::new();
    g.add_node("Person", props! { "name" => "Alice" }).unwrap();

    let stmt = tessera_graph::gql::parse_statement(
        "MATCH (a:Person) WITH a UNWIND null AS n RETURN n",
    )
    .unwrap();
    let tessera_graph::gql::GqlStatement::Pipeline(ref pq) = stmt else {
        panic!();
    };
    let rows = tessera_graph::gql::execute_pipeline(&g, pq, 0).unwrap();
    assert_eq!(rows.len(), 0);
}

/// UNWIND after WITH preserves the incoming binding variables — the
/// downstream RETURN can still access the outer `a`.
#[test]
fn execute_unwind_preserves_outer_binding() {
    use tessera_graph::{props, Graph, GqlValue};

    let mut g = Graph::new();
    g.add_node("Person", props! { "name" => "Alice", "age" => 30_i64 }).unwrap();

    let stmt = tessera_graph::gql::parse_statement(
        "MATCH (a:Person) WITH a UNWIND [1, 2] AS n RETURN a.name AS name, n",
    )
    .unwrap();
    let tessera_graph::gql::GqlStatement::Pipeline(ref pq) = stmt else {
        panic!();
    };
    let rows = tessera_graph::gql::execute_pipeline(&g, pq, 0).unwrap();
    assert_eq!(rows.len(), 2);
    for r in &rows {
        assert_eq!(r.get("name"), Some(&GqlValue::Str("Alice".into())));
    }
}

// ── Phase 9 RED: SET mutation terminal on pipeline bindings ────────────────

/// T18 end-to-end: the full `ensure_asset_indices_assigned::AssetNode` query.
/// Each `AssetNode` must be updated with `asset_idx = <position in id-sorted
/// order>`. After execution, a follow-up read verifies the indices.
#[test]
fn execute_with_set_mutation_assigns_asset_idx() {
    use tessera_graph::{gql, props, Graph, GqlValue};

    let mut g = Graph::new();
    // Insert nodes in reverse id order so the ORDER BY a.id is meaningful.
    g.add_node("AssetNode", props! { "id" => 5_i64 }).unwrap();
    g.add_node("AssetNode", props! { "id" => 1_i64 }).unwrap();
    g.add_node("AssetNode", props! { "id" => 4_i64 }).unwrap();
    g.add_node("AssetNode", props! { "id" => 2_i64 }).unwrap();
    g.add_node("AssetNode", props! { "id" => 3_i64 }).unwrap();

    let stmt = gql::parse_statement(
        "MATCH (a:AssetNode) \
         WITH a ORDER BY a.id \
         WITH collect(a) AS nodes \
         UNWIND range(0, size(nodes) - 1) AS i \
         WITH nodes[i] AS a, i AS idx \
         SET a.asset_idx = idx",
    )
    .unwrap();

    let result = gql::execute_pipeline_mutation(&mut g, &stmt, None).unwrap();
    assert_eq!(
        result.properties_set, 5,
        "expected 5 property assignments, got {result:?}",
    );

    // Verify: MATCH (a:AssetNode) RETURN a.id AS id, a.asset_idx AS idx
    let readback = gql::parse("MATCH (a:AssetNode) RETURN a.id AS id, a.asset_idx AS idx")
        .unwrap();
    let rows = gql::execute(&g, &readback, 0).unwrap();
    assert_eq!(rows.len(), 5);

    // Each id must have `asset_idx = id - 1` (since ids are 1..5 and
    // sorted order starts at 0).
    for r in &rows {
        let id = match r.get("id") {
            Some(GqlValue::Int(v)) => *v,
            other => panic!("bad id: {other:?}"),
        };
        let idx = match r.get("idx") {
            Some(GqlValue::Int(v)) => *v,
            other => panic!("bad idx for id={id}: {other:?}"),
        };
        assert_eq!(idx, id - 1, "node id={id} should have asset_idx={}", id - 1);
    }
}

/// Simpler Phase 9 RED: `MATCH (a) WITH a WITH a AS b SET b.x = 42` —
/// verifies that SET works with a bare pipeline (no UNWIND, no collect)
/// and that the alias-renamed binding `b` resolves to the original node.
#[test]
fn execute_with_set_via_simple_pipeline() {
    use tessera_graph::{gql, props, Graph, GqlValue};

    let mut g = Graph::new();
    g.add_node("Person", props! { "name" => "Alice" }).unwrap();

    let stmt = gql::parse_statement(
        "MATCH (a:Person) WITH a WITH a AS b SET b.x = 42",
    )
    .unwrap();
    let result = gql::execute_pipeline_mutation(&mut g, &stmt, None).unwrap();
    assert_eq!(result.properties_set, 1);

    let readback = gql::parse("MATCH (a:Person) RETURN a.x AS x").unwrap();
    let rows = gql::execute(&g, &readback, 0).unwrap();
    assert_eq!(rows[0].get("x"), Some(&GqlValue::Int(42)));
}

/// Phase 9 RED: SET against a non-bound alias is a mutation error, not a
/// silent no-op. `SET unknown.x = 1` with `unknown` not in scope must
/// surface an error.
#[test]
fn execute_with_set_unknown_variable_is_error() {
    use tessera_graph::{gql, props, Graph};

    let mut g = Graph::new();
    g.add_node("Person", props! { "name" => "Alice" }).unwrap();

    let stmt = gql::parse_statement(
        "MATCH (a:Person) WITH a.name AS n SET a.x = 1",
    )
    .unwrap();
    let result = gql::execute_pipeline_mutation(&mut g, &stmt, None);
    assert!(
        result.is_err(),
        "expected error: `a` is out of scope after WITH a.name AS n",
    );
}

/// Cycle 17: a `MATCH … WITH … SET` pipeline run inside a transaction writes
/// pending property updates (the MATCH phase sees committed nodes; the SET phase
/// writes into the txn's delta chain) — invisible to auto-commit until COMMIT.
#[test]
fn execute_pipeline_set_in_txn_writes_pending_not_autocommit() {
    use tessera_graph::{gql, props, Graph, GqlValue};

    let mut g = Graph::new();
    g.enable_mvcc();
    let node_id = g.add_node("Person", props! { "name" => "Alice" }).unwrap();

    let txn = g.begin_txn().unwrap();
    let stmt = gql::parse_statement("MATCH (a:Person) WITH a AS b SET b.x = 42").unwrap();
    let result = gql::execute_pipeline_mutation(&mut g, &stmt, Some(txn)).unwrap();
    assert_eq!(result.properties_set, 1);

    // Inside the txn the update is visible; to auto-commit it is not.
    assert_eq!(
        g.node_in_txn(txn, node_id).unwrap().properties().get("x"),
        Some(&tessera_graph::Property::I64(42))
    );
    assert_eq!(g.node(node_id).unwrap().properties().get("x"), None);

    g.commit_txn(txn).unwrap();
    let readback = gql::parse("MATCH (a:Person) RETURN a.x AS x").unwrap();
    let rows = gql::execute(&g, &readback, 0).unwrap();
    assert_eq!(rows[0].get("x"), Some(&GqlValue::Int(42)));
}

/// `ListLit` (with a bound variable) projects through and can be subscripted.
#[test]
fn execute_list_lit_with_variable() {
    use tessera_graph::GqlValue;

    let g = ages_graph();
    let stmt = tessera_graph::gql::parse_statement(
        "MATCH (a:Person) WITH [a.age, 999] AS pair RETURN pair[1]",
    )
    .unwrap();
    let tessera_graph::gql::GqlStatement::Pipeline(ref pq) = stmt else {
        panic!();
    };
    let rows = tessera_graph::gql::execute_pipeline(&g, pq, 0).unwrap();
    // 4 Person nodes → 4 rows, each with pair[1] = 999.
    assert_eq!(rows.len(), 4);
    for r in &rows {
        assert_eq!(r.get("pair[1]"), Some(&GqlValue::Int(999)));
    }
}
