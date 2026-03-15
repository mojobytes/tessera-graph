//! Cypher compatibility layer for `TesseraGraph` Enterprise.
//!
//! Provides `parse_with_mode` which accepts Cypher-flavoured syntax in
//! `CypherCompat` mode and rejects it in `StrictGql` mode.

use tessera_config::QueryLanguage;
use tessera_graph::GqlStatement;

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
pub fn parse_with_mode(
    input: &str,
    mode: QueryLanguage,
) -> tessera_graph::Result<GqlStatement> {
    match mode {
        QueryLanguage::Gql => tessera_graph::gql::parse_statement(input),
        QueryLanguage::CypherCompat => {
            let preprocessed = preprocessor::cypher_to_gql(input)?;
            tessera_graph::gql::parse_statement(&preprocessed)
        }
        QueryLanguage::StrictGql => {
            preprocessor::reject_cypher_constructs(input)?;
            tessera_graph::gql::parse_statement(input)
        }
    }
}
