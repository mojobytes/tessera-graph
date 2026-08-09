// SPDX-License-Identifier: MIT

use tessera_graph::GqlQuery;
use tessera_graph::gql;
use tessera_graph::gql::{BinOp, CreatePattern, Expr, GqlStatement, Literal, MutationClause};

#[test]
fn parse_stub_returns_ok() {
    let result = gql::parse("MATCH (a) RETURN a");
    assert!(result.is_ok());
    let _query: GqlQuery = result.unwrap();
}

#[test]
fn parse_full_query_all_clauses_integration() {
    let q = gql::parse(
        "MATCH (a:Person)-[:KNOWS]->(b) WHERE a.age > 25 \
         RETURN a.name, b.name ORDER BY a.name ASC LIMIT 10",
    )
    .unwrap();
    assert_eq!(q.match_clause.patterns.len(), 1);
    assert!(q.where_clause.is_some());
    assert_eq!(q.return_clause.items.len(), 2);
    assert!(q.order_by.is_some());
    assert_eq!(q.limit.unwrap().count, 10);
}

#[test]
fn parse_syntax_error_returns_err() {
    let result = gql::parse("NOT VALID GQL AT ALL !!!");
    assert!(result.is_err());
}

#[test]
fn parse_empty_string_is_err() {
    assert!(gql::parse("").is_err());
}

#[test]
fn parse_multi_label_node_is_unsupported_error() {
    let err = gql::parse("MATCH (a:Foo:Bar) RETURN a").unwrap_err();
    assert!(matches!(err, tessera_graph::Error::GqlUnsupported(_)));
}

#[test]
fn parse_unicode_string_literal_in_where() {
    let q = gql::parse("MATCH (a) WHERE a.name = '\u{65E5}\u{672C}\u{8A9E}' RETURN a").unwrap();
    match &q.where_clause.unwrap().predicate {
        Expr::BinaryOp { right, .. } => {
            assert_eq!(
                right.as_ref(),
                &Expr::Literal(Literal::Str("\u{65E5}\u{672C}\u{8A9E}".into()))
            );
        }
        _ => panic!("expected BinaryOp in WHERE"),
    }
}

#[test]
fn parse_distinct_return() {
    let q = gql::parse("MATCH (a) RETURN DISTINCT a.name").unwrap();
    assert!(q.return_clause.distinct);
}

// ── Aggregation integration tests ───────────────────────────────────────────

#[test]
fn parse_count_star() {
    let q = gql::parse("MATCH (a) RETURN COUNT(*)").unwrap();
    assert_eq!(q.return_clause.items.len(), 1);
    match &q.return_clause.items[0].expr {
        Expr::Aggregate { func, arg } => {
            assert_eq!(*func, gql::AggFunc::Count);
            assert!(arg.is_none());
        }
        other => panic!("expected Aggregate, got {other:?}"),
    }
}

#[test]
fn parse_count_with_argument() {
    let q = gql::parse("MATCH (a) RETURN COUNT(a.name)").unwrap();
    match &q.return_clause.items[0].expr {
        Expr::Aggregate { func, arg } => {
            assert_eq!(*func, gql::AggFunc::Count);
            assert!(arg.is_some());
        }
        other => panic!("expected Aggregate, got {other:?}"),
    }
}

#[test]
fn parse_sum_aggregate() {
    let q = gql::parse("MATCH (a) RETURN SUM(a.salary)").unwrap();
    match &q.return_clause.items[0].expr {
        Expr::Aggregate { func, .. } => assert_eq!(*func, gql::AggFunc::Sum),
        other => panic!("expected Aggregate, got {other:?}"),
    }
}

#[test]
fn parse_avg_aggregate() {
    let q = gql::parse("MATCH (a) RETURN AVG(a.age)").unwrap();
    match &q.return_clause.items[0].expr {
        Expr::Aggregate { func, .. } => assert_eq!(*func, gql::AggFunc::Avg),
        other => panic!("expected Aggregate, got {other:?}"),
    }
}

#[test]
fn parse_min_max_aggregate() {
    let q = gql::parse("MATCH (a) RETURN MIN(a.age), MAX(a.age)").unwrap();
    assert_eq!(q.return_clause.items.len(), 2);
    match &q.return_clause.items[0].expr {
        Expr::Aggregate { func, .. } => assert_eq!(*func, gql::AggFunc::Min),
        other => panic!("expected Aggregate, got {other:?}"),
    }
    match &q.return_clause.items[1].expr {
        Expr::Aggregate { func, .. } => assert_eq!(*func, gql::AggFunc::Max),
        other => panic!("expected Aggregate, got {other:?}"),
    }
}

#[test]
fn parse_collect_aggregate() {
    let q = gql::parse("MATCH (a) RETURN COLLECT(a.name)").unwrap();
    match &q.return_clause.items[0].expr {
        Expr::Aggregate { func, .. } => assert_eq!(*func, gql::AggFunc::Collect),
        other => panic!("expected Aggregate, got {other:?}"),
    }
}

#[test]
fn parse_aggregate_with_alias() {
    let q = gql::parse("MATCH (a) RETURN COUNT(*) AS total").unwrap();
    assert_eq!(q.return_clause.items[0].alias.as_deref(), Some("total"));
}

#[test]
fn parse_deeply_nested_aggregate_is_rejected() {
    // Build a deeply nested expression inside an aggregate to trigger depth limit.
    let inner = "(".repeat(200) + "1" + &")".repeat(200);
    let query = format!("MATCH (a) RETURN COUNT({inner})");
    let err = gql::parse(&query).unwrap_err();
    assert!(
        matches!(err, tessera_graph::Error::GqlSyntaxError { .. }),
        "expected GqlSyntaxError for depth limit, got {err:?}"
    );
}

// ── Delimited identifier (ISO GQL) integration tests ─────────────────────────

#[test]
fn parse_create_with_delimited_identifier_property_key() {
    let stmt = gql::parse_statement("CREATE (:Plant {\"Average Pyranometer\": 'value'})").unwrap();
    match stmt {
        GqlStatement::Mutation(ms) => match ms.mutation {
            MutationClause::Create(c) => {
                let props = match &c.patterns[0] {
                    CreatePattern::Node { props, .. } => props,
                    CreatePattern::Edge { .. } => panic!("expected Node"),
                };
                assert_eq!(props[0].0, "Average Pyranometer");
            }
            _ => panic!("expected Create"),
        },
        GqlStatement::Query(_)
        | GqlStatement::Pipeline(_)
        | GqlStatement::Admin(_)
        | GqlStatement::ConstReturn(_)
        | GqlStatement::Ddl(_)
        | GqlStatement::Call(_) => {
            panic!("expected Mutation")
        }
    }
}

#[test]
fn parse_where_with_delimited_identifier_property_access() {
    let q = gql::parse("MATCH (n) WHERE n.\"Average Pyranometer\" > 100 RETURN n").unwrap();
    match &q.where_clause.unwrap().predicate {
        Expr::BinaryOp { left, op, right } => {
            assert_eq!(*op, BinOp::Gt);
            match left.as_ref() {
                Expr::PropAccess { var, prop } => {
                    assert_eq!(var, "n");
                    assert_eq!(prop, "Average Pyranometer");
                }
                other => panic!("expected PropAccess, got {other:?}"),
            }
            assert_eq!(right.as_ref(), &Expr::Literal(Literal::Int(100)));
        }
        other => panic!("expected BinaryOp in WHERE, got {other:?}"),
    }
}
