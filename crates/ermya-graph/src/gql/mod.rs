// SPDX-License-Identifier: MIT

//! GQL (ISO/IEC 39075:2024) query language — read-only and mutation subsets.
//!
//! This module implements Layer 3 of the query engine: a GQL parser that
//! compiles queries and mutations into Layer 2 (`PatternBuilder`) operations
//! and direct `Graph` API calls.
//!
//! All GQL features are available without any feature flag. The supported
//! capabilities include:
//!
//! | Feature | Example |
//! |---|---|
//! | Multi-label nodes | `(a:Person:Employee)` |
//! | Variable-length paths | `-[:REL*1..5]->` |
//! | List literals | `RETURN [1, 2, 3]` |
//! | `RETURN` as a root statement (no preceding `MATCH`/`UNWIND`/`CREATE`) | `RETURN 1`, `RETURN $x` |
//! | `$name` / `$1` parameter placeholders in any expression position | `RETURN $x`, `MATCH (n) WHERE n.id = $1 RETURN n` |
//! | `UNWIND ... AS` (standalone and as a pipeline stage) | `UNWIND range(1,5) AS i` |
//! | `GROUP BY` with mixed aggregate/non-aggregate `RETURN` | `MATCH (p) RETURN p.city, COUNT(*)` |
//! | `shortestPath()` with constrained BFS | `shortestPath((a)-[:KNOWS*..3]->(b))` |
//! | `WITH` pipeline stages (projection, grouping, chaining) | `MATCH (a) WITH a ORDER BY a.age LIMIT 5 RETURN a` |
//! | `COLLECT` aggregate | `RETURN collect(p.name)` |
//! | Mutation terminal `SET` inside a pipeline | `MATCH (a) WITH a SET a.idx = n` |
//! | Nested aggregates | `size(collect(x))` |
//!
//! # Limitations
//!
//! - Transactions (explicit `BEGIN` / `COMMIT`) and isolation levels are
//!   not implemented. Writes use the `Graph` batch/WAL primitives.
//! - `MERGE` is parsed but only a subset of semantics is executed.
//!
//! [`Error::GqlUnsupported`]: crate::Error::GqlUnsupported
//! [`Error::GqlSyntaxError`]: crate::Error::GqlSyntaxError

pub(crate) mod ast;
pub(crate) mod compiler;
#[doc(hidden)]
pub mod lexer;
pub mod mutation_exec;
pub mod param_substitution;
pub(crate) mod parser;
pub(crate) mod path_materialization;
#[doc(hidden)]
pub mod token;
pub mod txn_view;

pub use mutation_exec::{
    apply_match_mutation_body, apply_merge_write, apply_unwind_create_body,
    apply_unwind_delete_body, eval_unwind_and_match, execute_bare_merge, execute_bare_mutation,
    execute_unwind_mutation, merge_lookup, MergeLookup, ResultRow,
};

pub use txn_view::{TxnReadView, TxnView};

pub use ast::{
    AccessLevelAst, AdminStatement, AggFunc, BinOp, CallStatement, ConstReturnQuery, CreateClause,
    CreatePattern, DatabaseOptions, DdlStatement, DeleteClause, EdgeLength, Expr, GqlQuery,
    GqlStatement, GrantTargetAst, GroupByClause, ListPredKind, Literal, MatchClause, MergeClause,
    MutationClause, MutationStatement, OrderByClause, ParamRef, PipelineQuery, PipelineStage,
    PipelineTerminal, ReturnClause, ReturnItem, SecretPlainPassword, SetAssignment, SetClause,
    SkipClause, UnwindClause, WhereClause, WithClause,
};
pub use compiler::{
    execute, execute_call_result, execute_const_return, execute_expr, expr_surface_name,
    GqlMutationResult, GqlNode, GqlPath, GqlRelationship, GqlResult, GqlRow, GqlValue,
    RESULT_CAP_MSG_PREFIX, TIMEOUT_MSG_PREFIX,
};

pub use compiler::{
    apply_map_to_node_merge, apply_map_to_node_overwrite, compile_match_bindings,
    compile_match_for_mutation, compile_match_rows, compile_match_rows_for_mutation,
    execute_pipeline, execute_pipeline_mutation, execute_pipeline_with_deadline,
    execute_with_deadline, gql_value_from_property, gql_value_to_property, resolve_create_props,
    MatchRow,
};

// Deprecated literal-only SET helper. Kept for API compatibility with
// 0.2.x consumers; new code should rely on `apply_pipeline_set`.
#[allow(deprecated)]
pub use compiler::eval_set_value;

#[doc(hidden)]
pub use compiler::literal_to_property;

/// Parses a GQL read-only query string into a typed AST.
///
/// Only parses `MATCH ... RETURN ...` queries. For mutations (CREATE, SET,
/// DELETE, MERGE), use [`parse_statement`].
///
/// # Errors
/// Returns [`crate::Error::GqlSyntaxError`] on malformed input.
pub fn parse(input: &str) -> crate::Result<GqlQuery> {
    let tokens = lexer::Lexer::new(input).tokenize()?;
    parser::Parser::new(tokens).parse()
}

/// Parses a GQL statement — either a read-only query, a mutation, or a
/// standalone projection.
///
/// Supports all statement types:
/// - `MATCH ... RETURN ...` — read-only query, returns `GqlStatement::Query`
/// - `RETURN <expr-list>` — constant-row projection with no preceding
///   `MATCH`/`UNWIND`/`CREATE`; returns `GqlStatement::ConstReturn`.
///   Used by Bolt clients for keep-alive style RUNs (`RETURN 1`).
/// - `CREATE ...` — create mutation
/// - `MATCH ... DELETE ...` / `MATCH ... DETACH DELETE ...` — delete mutation
/// - `MATCH ... SET ...` — set mutation
/// - `MERGE ...` — merge mutation
/// - `UNWIND ...` — standalone unwind pipeline
///
/// `$name` and `$1` parameter placeholders may appear in any expression
/// position; they are resolved post-parse by
/// [`param_substitution::apply`].
///
/// # Errors
/// Returns [`crate::Error::GqlSyntaxError`] on malformed input.
pub fn parse_statement(input: &str) -> crate::Result<GqlStatement> {
    let tokens = lexer::Lexer::new(input).tokenize()?;
    parser::Parser::new(tokens).parse_statement()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_parse_statement_accessible() {
        let stmt = parse_statement("CREATE (n:Person {name: 'Test'})").unwrap();
        assert!(matches!(stmt, GqlStatement::Mutation(_)));
    }

    #[test]
    fn public_parse_statement_query_path() {
        let stmt = parse_statement("MATCH (a:Person) RETURN a.name").unwrap();
        assert!(matches!(stmt, GqlStatement::Query(_)));
    }

    #[test]
    fn gql_mutation_result_is_accessible() {
        let r = GqlMutationResult::default();
        assert_eq!(r.nodes_created, 0);
    }

    #[test]
    fn gql_mutation_result_default_has_new_fields_zero_and_no_updates() {
        let r = GqlMutationResult::default();
        assert_eq!(r.labels_added, 0);
        assert_eq!(r.elements_changed, 0);
        assert!(!r.contains_updates());
    }

    #[test]
    fn gql_mutation_result_contains_updates_true_when_any_counter_nonzero() {
        let r = GqlMutationResult {
            nodes_created: 1,
            ..Default::default()
        };
        assert!(r.contains_updates());
    }
}
