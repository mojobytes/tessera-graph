// Copyright 2026 BelowZero Security OU. All rights reserved.

//! Server startup configuration derived from environment variables.

use std::path::PathBuf;
use tessera_graph::GraphConfig;

/// Default background flush interval in milliseconds.
const DEFAULT_FLUSH_INTERVAL_MS: u64 = 50;

/// Default query cache capacity (number of parsed ASTs to keep).
const DEFAULT_QUERY_CACHE_CAPACITY: usize = 1024;

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

        let memory_limit_bytes = std::env::var("TESSERA_MEMORY_LIMIT_MB")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(64)
            * 1024
            * 1024;

        let wal_enabled = std::env::var("TESSERA_WAL_ENABLED")
            .map(|v| v.to_lowercase() != "false")
            .unwrap_or(true);

        let default_tenant =
            std::env::var("TESSERA_DEFAULT_TENANT").unwrap_or_else(|_| "default".to_owned());

        let flush_interval_ms = Self::parse_flush_interval(
            std::env::var("TESSERA_FLUSH_INTERVAL_MS").ok().as_deref(),
        );

        let query_cache_capacity = std::env::var("TESSERA_QUERY_CACHE_CAPACITY")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(DEFAULT_QUERY_CACHE_CAPACITY);

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
