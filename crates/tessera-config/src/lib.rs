//! Configuration management for tessera-graph-enterprise.
// Copyright 2026 BelowZero Security OU. All rights reserved.

use std::fmt;
use std::str::FromStr;

/// Query language mode for the enterprise server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum QueryLanguage {
    /// ISO GQL (default) — standard GQL syntax only.
    #[default]
    Gql,
    /// Cypher compatibility — accepts Cypher-specific syntax alongside GQL.
    CypherCompat,
    /// Strict GQL — rejects any Cypher-only constructs with diagnostic errors.
    StrictGql,
}

impl fmt::Display for QueryLanguage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Gql => write!(f, "gql"),
            Self::CypherCompat => write!(f, "cypher-compat"),
            Self::StrictGql => write!(f, "strict-gql"),
        }
    }
}

/// Error returned when parsing an invalid query language string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseQueryLanguageError(String);

impl fmt::Display for ParseQueryLanguageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "unknown query language '{}'; expected 'gql', 'cypher-compat', or 'strict-gql'",
            self.0
        )
    }
}

impl std::error::Error for ParseQueryLanguageError {}

impl FromStr for QueryLanguage {
    type Err = ParseQueryLanguageError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "gql" => Ok(Self::Gql),
            "cypher-compat" => Ok(Self::CypherCompat),
            "strict-gql" => Ok(Self::StrictGql),
            _ => Err(ParseQueryLanguageError(s.to_owned())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_gql() {
        assert_eq!(QueryLanguage::default(), QueryLanguage::Gql);
    }

    #[test]
    fn from_str_gql() {
        assert_eq!("gql".parse::<QueryLanguage>().unwrap(), QueryLanguage::Gql);
    }

    #[test]
    fn from_str_cypher_compat() {
        assert_eq!(
            "cypher-compat".parse::<QueryLanguage>().unwrap(),
            QueryLanguage::CypherCompat
        );
    }

    #[test]
    fn from_str_strict_gql() {
        assert_eq!(
            "strict-gql".parse::<QueryLanguage>().unwrap(),
            QueryLanguage::StrictGql
        );
    }

    #[test]
    fn from_str_invalid() {
        assert!("invalid".parse::<QueryLanguage>().is_err());
    }

    #[test]
    fn display_roundtrip() {
        for mode in [
            QueryLanguage::Gql,
            QueryLanguage::CypherCompat,
            QueryLanguage::StrictGql,
        ] {
            assert_eq!(mode.to_string().parse::<QueryLanguage>().unwrap(), mode);
        }
    }
}
