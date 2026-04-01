//! Cypher compatibility layer for `TesseraGraph` Enterprise.
//!
//! Provides `parse_with_mode` which accepts Cypher-flavoured syntax in
//! `CypherCompat` mode and rejects it in `StrictGql` mode.

use tessera_config::QueryLanguage;
use tessera_graph::GqlStatement;

pub mod cache;
pub mod preprocessor;

/// Parses a query string respecting the configured query language mode.
///
/// - `Gql`: passes input directly to the GQL parser (no transformation).
/// - `CypherCompat`: pre-processes Cypher-specific syntax before parsing.
/// - `StrictGql`: scans for Cypher-only constructs and rejects them with
///   diagnostic errors before parsing.
///
/// # Errors
///
/// Returns a parse error from the core GQL parser, or a `GqlSyntaxError` from
/// the preprocessor when Cypher constructs are detected in `StrictGql` mode,
/// or when the input contains malformed Cypher syntax (unclosed comments, etc.)
/// in `CypherCompat` mode.
pub fn parse_with_mode(input: &str, mode: QueryLanguage) -> tessera_graph::Result<GqlStatement> {
    match mode {
        QueryLanguage::Gql => tessera_graph::gql::parse_statement(input),
        QueryLanguage::CypherCompat => {
            if preprocessor::contains_cypher_constructs(input) {
                let preprocessed = preprocessor::cypher_to_gql(input)?;
                tessera_graph::gql::parse_statement(&preprocessed)
            } else {
                tessera_graph::gql::parse_statement(input)
            }
        }
        QueryLanguage::StrictGql => {
            preprocessor::reject_cypher_constructs(input)?;
            tessera_graph::gql::parse_statement(input)
        }
    }
}

/// Cache-through wrapper around [`parse_with_mode`].
///
/// On a cache hit, returns a clone of the stored AST without parsing.
/// On a cache miss, parses the query and stores the result.
/// Parse errors are never cached.
///
/// # Errors
///
/// Returns the same errors as [`parse_with_mode`].
pub fn parse_with_mode_cached(
    input: &str,
    mode: QueryLanguage,
    cache: &cache::QueryCache,
) -> tessera_graph::Result<GqlStatement> {
    if let Some(stmt) = cache.get(input) {
        return Ok(stmt);
    }
    let stmt = parse_with_mode(input, mode)?;
    cache.insert(input.to_owned(), stmt.clone());
    Ok(stmt)
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Cycle 1.2: parse_with_mode_cached ---

    #[test]
    fn cache_miss_populates_on_first_call() {
        let cache = cache::QueryCache::new(16);
        let q = "MATCH (n) RETURN n";
        assert!(cache.get(q).is_none());
        parse_with_mode_cached(q, QueryLanguage::CypherCompat, &cache).unwrap(); // OK: test
        assert!(cache.get(q).is_some());
    }

    #[test]
    fn cache_hit_returns_equal_statement() {
        let cache = cache::QueryCache::new(16);
        let q = "MATCH (n) RETURN n";
        let first = parse_with_mode_cached(q, QueryLanguage::CypherCompat, &cache).unwrap(); // OK: test
        let second = parse_with_mode_cached(q, QueryLanguage::CypherCompat, &cache).unwrap(); // OK: test
        assert_eq!(first, second);
    }

    #[test]
    fn parse_error_is_not_cached() {
        let cache = cache::QueryCache::new(16);
        let _ = parse_with_mode_cached("NOT VALID ~~~", QueryLanguage::CypherCompat, &cache);
        assert!(cache.get("NOT VALID ~~~").is_none());
    }

    // --- Cycle 1.3: throughput regression guard ---

    #[test]
    fn parse_cached_throughput_regression_guard() {
        let cache = cache::QueryCache::new(256);
        let query = "MATCH (n) RETURN n";
        parse_with_mode_cached(query, QueryLanguage::CypherCompat, &cache).unwrap();

        let iterations = 50_000u32;
        let start = std::time::Instant::now();
        for _ in 0..iterations {
            parse_with_mode_cached(query, QueryLanguage::CypherCompat, &cache).unwrap();
        }
        let ops = f64::from(iterations) / start.elapsed().as_secs_f64();
        let min = if cfg!(debug_assertions) { 200_000.0 } else { 2_000_000.0 };
        assert!(ops >= min, "cache throughput regression: {ops:.0} ops/s < {min:.0} minimum");
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
        assert!(result.is_ok(), "OPTIONAL MATCH must be transformed, not rejected");
    }

    #[test]
    fn cypher_compat_still_preprocesses_backtick_idents() {
        let result = parse_with_mode("MATCH (`n`) RETURN n", QueryLanguage::CypherCompat);
        assert!(result.is_ok());
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
        let min = if cfg!(debug_assertions) { 50_000.0 } else { 500_000.0 };
        assert!(ops >= min, "GQL fast-path regression: {ops:.0} ops/s < {min:.0}");
    }
}
