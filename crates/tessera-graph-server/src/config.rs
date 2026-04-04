// Copyright 2026 BelowZero Security OU. All rights reserved.

//! Server startup configuration derived from environment variables.

use std::path::PathBuf;
use tessera_graph::GraphConfig;

/// Default background flush interval in milliseconds.
const DEFAULT_FLUSH_INTERVAL_MS: u64 = 50;

/// Default query cache capacity (number of parsed ASTs to keep).
const DEFAULT_QUERY_CACHE_CAPACITY: usize = 1024;

/// Default minimum free disk space in megabytes.
const DEFAULT_MIN_FREE_DISK_MB: u64 = 100;

/// Parse an environment variable, logging a warning if the value is present but
/// cannot be parsed. Returns `default` if the variable is unset or invalid.
#[must_use]
pub fn parse_env_or_warn<T: std::str::FromStr>(name: &str, default: T) -> T {
    match std::env::var(name) {
        Err(_) => default, // not set
        Ok(v) => v.parse().unwrap_or_else(|_| {
            tracing::warn!("{name} has invalid value '{v}' — using default");
            default
        }),
    }
}

/// Parse a boolean environment variable. Accepts `"true"/"1"` for true,
/// `"false"/"0"` for false. Warns and returns `default` on any other value.
#[must_use]
pub fn parse_bool_env_or_warn(name: &str, default: bool) -> bool {
    std::env::var(name).map_or(default, |v| {
        match v.to_ascii_lowercase().as_str() {
            "true" | "1" => true,
            "false" | "0" => false,
            _ => {
                tracing::warn!("{name} has invalid value '{v}' — using default ({default})");
                default
            }
        }
    })
}

/// Parsed persistence configuration derived from environment variables.
#[derive(Debug)]
pub struct PersistenceConfig {
    /// Absolute path to the data directory.
    /// `None` means in-memory mode (`Graph::new()`).
    pub data_dir: Option<PathBuf>,
    /// Graph engine configuration (buffer pool, WAL, etc.).
    pub graph_config: GraphConfig,
    /// Default tenant name used when no `db` field is provided in HELLO.
    pub default_tenant: String,
    /// Background flush interval in milliseconds.
    /// `0` means synchronous flush after each mutation (legacy behaviour).
    /// `TESSERA_FLUSH_INTERVAL_MS` (default `50`).
    pub flush_interval_ms: u64,
    /// Maximum number of parsed query ASTs to cache server-wide.
    /// `TESSERA_QUERY_CACHE_CAPACITY` (default `1024`).
    pub query_cache_capacity: usize,
    /// Minimum free disk space in bytes before marking the server degraded.
    /// `TESSERA_MIN_FREE_DISK_MB` (default `100`).
    pub min_free_disk_bytes: u64,
}

impl PersistenceConfig {
    /// Read persistence settings from environment variables.
    ///
    /// - `TESSERA_DATA_DIR`: if set, enables file-backed storage at that path.
    /// - `TESSERA_MEMORY_LIMIT_MB`: buffer pool size in megabytes (default 64).
    /// - `TESSERA_WAL_ENABLED`: `"false"` disables WAL (default enabled).
    /// - `TESSERA_DEFAULT_TENANT`: default tenant name (default `"default"`).
    /// - `TESSERA_FLUSH_INTERVAL_MS`: background flush interval in ms (default 50, 0 = sync).
    /// - `TESSERA_QUERY_CACHE_CAPACITY`: parsed query cache size (default 1024).
    #[must_use]
    pub fn from_env() -> Self {
        let data_dir = std::env::var("TESSERA_DATA_DIR").ok().map(PathBuf::from);

        let memory_limit_bytes =
            parse_env_or_warn::<usize>("TESSERA_MEMORY_LIMIT_MB", 64) * 1024 * 1024;

        let wal_enabled = parse_bool_env_or_warn("TESSERA_WAL_ENABLED", true);

        let default_tenant =
            std::env::var("TESSERA_DEFAULT_TENANT").unwrap_or_else(|_| "default".to_owned());

        let flush_interval_ms =
            parse_env_or_warn("TESSERA_FLUSH_INTERVAL_MS", DEFAULT_FLUSH_INTERVAL_MS);

        let query_cache_capacity =
            parse_env_or_warn("TESSERA_QUERY_CACHE_CAPACITY", DEFAULT_QUERY_CACHE_CAPACITY);

        let min_free_disk_bytes =
            parse_env_or_warn("TESSERA_MIN_FREE_DISK_MB", DEFAULT_MIN_FREE_DISK_MB)
                .saturating_mul(1024 * 1024);

        Self {
            data_dir,
            graph_config: GraphConfig {
                memory_limit_bytes,
                create_if_missing: true,
                adj_cache_capacity: GraphConfig::new().adj_cache_capacity,
                wal_enabled,
            },
            default_tenant,
            flush_interval_ms,
            query_cache_capacity,
            min_free_disk_bytes,
        }
    }

    /// Parse flush interval from an optional string value.
    /// Returns `DEFAULT_FLUSH_INTERVAL_MS` (50) when `None` or unparseable.
    #[must_use]
    pub fn parse_flush_interval(raw: Option<&str>) -> u64 {
        raw.and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(DEFAULT_FLUSH_INTERVAL_MS)
    }
}

