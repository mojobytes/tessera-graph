// SPDX-License-Identifier: BSL-1.1

//! Cypher compatibility layer for `ErmyaGraph` Enterprise.
//!
//! Provides `parse_with_mode` which accepts Cypher-flavoured syntax in
//! `CypherCompat` mode and rejects it in `StrictGql` mode.

use ermya_graph::GqlStatement;
use ermya_graph_config::QueryLanguage;

pub mod admin;
pub mod cache;
pub mod call;
pub mod ddl;
pub(crate) mod parse_util;
pub mod preprocessor;

pub use admin::try_parse_admin;
pub use ddl::try_parse_ddl;

/// Parses a query string respecting the configured query language mode.
///
/// - `Gql`: passes input directly to the GQL parser (no transformation).
/// - `CypherCompat`: pre-processes Cypher-specific syntax before parsing.
/// - `StrictGql`: scans for Cypher-only constructs and rejects them with
///   diagnostic errors before parsing.
///
/// Admin statements (`CREATE USER`, `DROP USER`, `ALTER USER`,
/// `SHOW USERS`) are recognised first, independently of `mode`, and
/// routed to the server-side admin dispatcher. Only if the input is not
/// an admin statement does the mode-specific GQL pipeline run.
///
/// # Errors
///
/// Returns a parse error from the core GQL parser, from the admin
/// parser, or a `GqlSyntaxError` from the preprocessor when Cypher
/// constructs are detected in `StrictGql` mode, or when the input
/// contains malformed Cypher syntax (unclosed comments, etc.) in
/// `CypherCompat` mode.
pub fn parse_with_mode(input: &str, mode: QueryLanguage) -> ermya_graph::Result<GqlStatement> {
    // CALL is checked first — it has a unique leading `CALL` keyword shared with
    // no other statement class, and carries its own UNWIND/RETURN pipeline.
    if let Some(stmt) = call::try_parse_call(input)? {
        return Ok(GqlStatement::Call(Box::new(stmt)));
    }
    // DDL is checked next: `CREATE INDEX`/`DROP INDEX` share a leading
    // `CREATE`/`DROP` keyword with node mutations and must be intercepted
    // before the GQL token parser (or admin parser) sees them.
    if let Some(stmt) = ddl::try_parse_ddl(input)? {
        return Ok(GqlStatement::Ddl(stmt));
    }
    if let Some(stmt) = admin::try_parse_admin(input)? {
        return Ok(GqlStatement::Admin(stmt));
    }
    match mode {
        QueryLanguage::Gql => ermya_graph::gql::parse_statement(input),
        QueryLanguage::CypherCompat => {
            if preprocessor::contains_cypher_constructs(input) {
                let preprocessed = preprocessor::cypher_to_gql(input)?;
                ermya_graph::gql::parse_statement(&preprocessed)
            } else {
                ermya_graph::gql::parse_statement(input)
            }
        }
        QueryLanguage::StrictGql => {
            preprocessor::reject_cypher_constructs(input)?;
            ermya_graph::gql::parse_statement(input)
        }
    }
}

/// Cache-through wrapper around [`parse_with_mode`].
///
/// On a cache hit, returns a clone of the stored AST without parsing.
/// On a cache miss, parses the query and stores the result.
/// Parse errors are never cached.
///
/// `params_signature` participates in the cache key so two RUNs of the
/// same query with different `$param` bindings do not alias. Callers
/// should compute the signature via [`cache::hash_params`]; an empty
/// param map produces `0` (constant time, no hashing).
///
/// # Errors
///
/// Returns the same errors as [`parse_with_mode`].
pub fn parse_with_mode_cached(
    input: &str,
    mode: QueryLanguage,
    params_signature: u64,
    cache: &cache::QueryCache,
) -> ermya_graph::Result<GqlStatement> {
    let key = cache::CacheKey {
        query: input.to_owned(),
        params_signature,
    };
    if let Some(stmt) = cache.get(&key) {
        return Ok(stmt);
    }
    let stmt = parse_with_mode(input, mode)?;
    cache.insert(key, stmt.clone());
    Ok(stmt)
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Cycle 1.2: parse_with_mode_cached ---

    #[test]
    fn cache_miss_populates_on_first_call() {
        let qc = cache::QueryCache::new(16);
        let q = "MATCH (n) RETURN n";
        let key = cache::CacheKey {
            query: q.to_owned(),
            params_signature: 0,
        };
        assert!(qc.get(&key).is_none());
        parse_with_mode_cached(q, QueryLanguage::CypherCompat, 0, &qc).unwrap(); // OK: test
        assert!(qc.get(&key).is_some());
    }

    #[test]
    fn cache_hit_returns_equal_statement() {
        let cache = cache::QueryCache::new(16);
        let q = "MATCH (n) RETURN n";
        let first = parse_with_mode_cached(q, QueryLanguage::CypherCompat, 0, &cache).unwrap(); // OK: test
        let second = parse_with_mode_cached(q, QueryLanguage::CypherCompat, 0, &cache).unwrap(); // OK: test
        assert_eq!(first, second);
    }

    #[test]
    fn parse_error_is_not_cached() {
        let qc = cache::QueryCache::new(16);
        let _ = parse_with_mode_cached("NOT VALID ~~~", QueryLanguage::CypherCompat, 0, &qc);
        assert!(
            qc.get(&cache::CacheKey {
                query: "NOT VALID ~~~".to_owned(),
                params_signature: 0,
            })
            .is_none()
        );
    }

    // --- Issue #12: STARTS WITH / ENDS WITH end-to-end via parse_with_mode ---
    //
    // The bug surfaced for Bolt clients calling parse_with_mode(_,
    // CypherCompat). These tests confirm the chain parse_with_mode ->
    // parse_statement -> parse_comparison -> peek_is_with_keyword is wired,
    // and that StrictGql still rejects the Cypher-only operator with a clear
    // diagnostic.

    #[test]
    fn cypher_compat_starts_with_parses_to_correct_ast() {
        use ermya_graph::gql::{BinOp, Expr};
        let stmt = parse_with_mode(
            "MATCH (a:Person) WHERE a.name STARTS WITH 'Al' RETURN a",
            QueryLanguage::CypherCompat,
        )
        .unwrap(); // OK: test
        match stmt {
            GqlStatement::Query(q) => match q.where_clause.expect("expected WHERE").predicate {
                Expr::BinaryOp { op, .. } => assert_eq!(op, BinOp::StartsWith),
                other => panic!("expected StartsWith, got {other:?}"),
            },
            other => panic!("expected Query, got {other:?}"),
        }
    }

    #[test]
    fn cypher_compat_ends_with_parses_to_correct_ast() {
        use ermya_graph::gql::{BinOp, Expr};
        let stmt = parse_with_mode(
            "MATCH (a:Person) WHERE a.email ENDS WITH '@corp.com' RETURN a",
            QueryLanguage::CypherCompat,
        )
        .unwrap(); // OK: test
        match stmt {
            GqlStatement::Query(q) => match q.where_clause.expect("expected WHERE").predicate {
                Expr::BinaryOp { op, .. } => assert_eq!(op, BinOp::EndsWith),
                other => panic!("expected EndsWith, got {other:?}"),
            },
            other => panic!("expected Query, got {other:?}"),
        }
    }

    #[test]
    fn strict_gql_rejects_starts_with() {
        // STARTS WITH is Cypher-only; StrictGql must reject it with a clear
        // error that points the user at cypher-compat mode.
        let err = parse_with_mode(
            "MATCH (a) WHERE a.name STARTS WITH 'x' RETURN a",
            QueryLanguage::StrictGql,
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("STARTS WITH") || msg.to_lowercase().contains("cypher"),
            "error should mention STARTS WITH or cypher mode, got: {msg}"
        );
    }

    // --- Cycle 1.3: throughput regression guard ---

    #[test]
    fn parse_cached_throughput_regression_guard() {
        let cache = cache::QueryCache::new(256);
        let query = "MATCH (n) RETURN n";
        parse_with_mode_cached(query, QueryLanguage::CypherCompat, 0, &cache).unwrap();

        let iterations = 50_000u32;
        let start = std::time::Instant::now();
        for _ in 0..iterations {
            parse_with_mode_cached(query, QueryLanguage::CypherCompat, 0, &cache).unwrap();
        }
        let ops = f64::from(iterations) / start.elapsed().as_secs_f64();
        let min = if cfg!(debug_assertions) {
            200_000.0
        } else {
            2_000_000.0
        };
        assert!(
            ops >= min,
            "cache throughput regression: {ops:.0} ops/s < {min:.0} minimum"
        );
    }

    // --- Cycle 2.2: GQL-native fast-path in CypherCompat ---

    #[test]
    fn cypher_compat_pure_gql_equals_gql_mode() {
        let q = "MATCH (n) RETURN n";
        let via_compat = parse_with_mode(q, QueryLanguage::CypherCompat).unwrap();
        let via_gql = parse_with_mode(q, QueryLanguage::Gql).unwrap();
        assert_eq!(via_compat, via_gql);
    }

    #[test]
    fn cypher_compat_still_preprocesses_optional_match() {
        let result = parse_with_mode("OPTIONAL MATCH (n) RETURN n", QueryLanguage::CypherCompat);
        assert!(
            result.is_ok(),
            "OPTIONAL MATCH must be transformed, not rejected"
        );
    }

    #[test]
    fn cypher_compat_still_preprocesses_backtick_idents() {
        let result = parse_with_mode("MATCH (`n`) RETURN n", QueryLanguage::CypherCompat);
        assert!(result.is_ok());
    }

    // --- 3e/3f: scalar functions & list predicates via CypherCompat ---
    //
    // The .NET pilot calls parse_with_mode(_, CypherCompat). These confirm the
    // new evaluator/grammar surface is reachable through that exact path (the
    // same wiring class as the #12 STARTS WITH gap), not just from the engine's
    // direct parse entry point.

    #[test]
    fn cypher_compat_scalar_functions_parse() {
        // toLower/toUpper/coalesce reach the parser unrejected by the
        // preprocessor and produce FunctionCall nodes.
        let stmt = parse_with_mode(
            "MATCH (n:Person) RETURN coalesce(n.nick, toLower(n.name)), toUpper(n.name)",
            QueryLanguage::CypherCompat,
        )
        .unwrap();
        assert!(matches!(stmt, GqlStatement::Query(_)));
    }

    #[test]
    fn cypher_compat_list_predicate_parses_to_list_predicate_ast() {
        use ermya_graph::gql::{Expr, ListPredKind};
        let stmt = parse_with_mode(
            "MATCH (n:Resource) WHERE ALL(x IN [1, 2, 3] WHERE x > 0) RETURN n",
            QueryLanguage::CypherCompat,
        )
        .unwrap();
        match stmt {
            GqlStatement::Query(q) => match q.where_clause.expect("expected WHERE").predicate {
                Expr::ListPredicate { kind, .. } => assert_eq!(kind, ListPredKind::All),
                other => panic!("expected ListPredicate, got {other:?}"),
            },
            other => panic!("expected Query, got {other:?}"),
        }
    }

    #[test]
    fn cypher_compat_all_four_list_predicates_parse() {
        for kw in ["ALL", "ANY", "NONE", "SINGLE"] {
            let q = format!("MATCH (n) WHERE {kw}(x IN [1, 2] WHERE x > 0) RETURN n");
            assert!(
                parse_with_mode(&q, QueryLanguage::CypherCompat).is_ok(),
                "{kw} predicate must parse via CypherCompat"
            );
        }
    }

    // --- Issue #61: append-only reaches the wire ---
    //
    // The mode worked entirely in the engine but could only be declared from
    // Rust, so no networked client could switch it on — and the rejection path
    // wired up to the protocol error code could never be provoked end to end.
    // These pin the statement class the router actually produces, in both
    // query languages, which is what makes the feature reachable at all.

    #[test]
    fn alter_label_append_only_routes_to_ddl_in_both_modes() {
        for mode in [QueryLanguage::Gql, QueryLanguage::CypherCompat] {
            let stmt = parse_with_mode("ALTER LABEL :Event SET APPEND ONLY", mode).unwrap();
            assert!(
                matches!(
                    &stmt,
                    GqlStatement::Ddl(ermya_graph::gql::DdlStatement::SetLabelAppendOnly {
                        label,
                        on: true,
                    }) if label == "Event"
                ),
                "expected DDL SetLabelAppendOnly in {mode:?}, got {stmt:?}"
            );
        }
    }

    #[test]
    fn remove_append_only_routes_to_ddl() {
        let stmt = parse_with_mode(
            "ALTER LABEL :Event REMOVE APPEND ONLY",
            QueryLanguage::CypherCompat,
        )
        .unwrap();
        assert!(matches!(
            &stmt,
            GqlStatement::Ddl(ermya_graph::gql::DdlStatement::SetLabelAppendOnly {
                on: false,
                ..
            })
        ));
    }

    #[test]
    fn show_append_only_info_routes_to_ddl() {
        let stmt = parse_with_mode("SHOW APPEND ONLY INFO", QueryLanguage::CypherCompat).unwrap();
        assert!(matches!(
            &stmt,
            GqlStatement::Ddl(ermya_graph::gql::DdlStatement::ShowAppendOnlyInfo)
        ));
    }

    #[test]
    fn alter_label_does_not_shadow_an_ordinary_match() {
        // `ALTER` must only introduce DDL when followed by LABEL, or a label
        // that merely starts with those letters would stop parsing as a query.
        let stmt =
            parse_with_mode("MATCH (n:Alteration) RETURN n", QueryLanguage::CypherCompat).unwrap();
        assert!(
            !matches!(&stmt, GqlStatement::Ddl(_)),
            "an ordinary MATCH must not be captured by the DDL router"
        );
    }

    // --- Cycle 2.3: GQL fast-path throughput regression guard ---

    #[test]
    fn gql_native_fastpath_throughput_regression_guard() {
        let q = "MATCH (n) RETURN n";
        let iters = 20_000u32;
        let start = std::time::Instant::now();
        for _ in 0..iters {
            parse_with_mode(q, QueryLanguage::CypherCompat).unwrap();
        }
        let ops = f64::from(iters) / start.elapsed().as_secs_f64();
        let min = if cfg!(debug_assertions) {
            50_000.0
        } else {
            500_000.0
        };
        assert!(
            ops >= min,
            "GQL fast-path regression: {ops:.0} ops/s < {min:.0}"
        );
    }
}
