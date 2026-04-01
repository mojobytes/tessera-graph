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

// ── Audit configuration ─────────────────────────────────────────────────────

/// Configuration for the activity audit log.
#[derive(Debug, Clone)]
pub struct AuditConfig {
    /// Whether audit logging is enabled.
    pub enabled: bool,
    /// Path to the audit log file.
    pub log_path: std::path::PathBuf,
    /// Maximum size in bytes before rotating. 0 = disabled.
    pub rotation_max_size_bytes: u64,
    /// Maximum number of rotated files to keep. 0 = keep all.
    pub max_rotated_files: usize,
    /// Channel buffer capacity for the async audit writer.
    pub channel_capacity: usize,
}

impl AuditConfig {
    /// Load audit configuration from environment variables.
    ///
    /// - `TESSERA_AUDIT_ENABLED` (default `"true"`)
    /// - `TESSERA_AUDIT_PATH` (default `"audit.ndjson"`)
    /// - `TESSERA_AUDIT_ROTATION_MAX_MB` (default `100`)
    /// - `TESSERA_AUDIT_MAX_FILES` (default `0` = keep all)
    #[must_use]
    pub fn from_env() -> Self {
        let enabled = std::env::var("TESSERA_AUDIT_ENABLED")
            .map(|v| v != "false" && v != "0")
            .unwrap_or(true);

        let log_path = std::env::var("TESSERA_AUDIT_PATH")
            .map_or_else(|_| std::path::PathBuf::from("audit.ndjson"), std::path::PathBuf::from);

        let rotation_max_mb: u64 = std::env::var("TESSERA_AUDIT_ROTATION_MAX_MB")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(100);
        let rotation_max_size_bytes = rotation_max_mb * 1024 * 1024;

        let max_rotated_files: usize = std::env::var("TESSERA_AUDIT_MAX_FILES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);

        let channel_capacity: usize = std::env::var("TESSERA_AUDIT_CHANNEL_CAPACITY")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(4096);

        Self {
            enabled,
            log_path,
            rotation_max_size_bytes,
            max_rotated_files,
            channel_capacity,
        }
    }
}

impl Default for AuditConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            log_path: std::path::PathBuf::from("audit.ndjson"),
            rotation_max_size_bytes: 100 * 1024 * 1024,
            max_rotated_files: 0,
            channel_capacity: 4096,
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
            assert_eq!(mode.to_string().parse::<QueryLanguage>().unwrap(), mode); // OK: test
        }
    }

    // ── AuditConfig ─────────────────────────────────────────────────────────

    #[test]
    fn audit_config_default_values() {
        let cfg = AuditConfig::default();
        assert!(cfg.enabled);
        assert_eq!(cfg.log_path, std::path::PathBuf::from("audit.ndjson"));
        assert_eq!(cfg.rotation_max_size_bytes, 100 * 1024 * 1024);
        assert_eq!(cfg.max_rotated_files, 0);
        assert_eq!(cfg.channel_capacity, 4096);
    }

    #[test]
    fn audit_config_custom_values() {
        let cfg = AuditConfig {
            enabled: false,
            log_path: std::path::PathBuf::from("/tmp/custom.ndjson"),
            rotation_max_size_bytes: 0,
            max_rotated_files: 5,
            channel_capacity: 2048,
        };
        assert!(!cfg.enabled);
        assert_eq!(cfg.rotation_max_size_bytes, 0);
        assert_eq!(cfg.max_rotated_files, 5);
    }

    #[test]
    fn audit_config_clone_preserves_fields() {
        let cfg = AuditConfig::default();
        let cloned = cfg.clone();
        assert_eq!(cloned.enabled, cfg.enabled);
        assert_eq!(cloned.log_path, cfg.log_path);
        assert_eq!(cloned.rotation_max_size_bytes, cfg.rotation_max_size_bytes);
    }
}
