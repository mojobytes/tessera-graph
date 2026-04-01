// Copyright 2026 BelowZero Security OU. All rights reserved.

//! Tests for `flush_interval_ms` configuration parsing.

use tessera_graph_server::config::PersistenceConfig;

#[test]
fn flush_interval_default_is_50ms() {
    assert_eq!(PersistenceConfig::parse_flush_interval(None), 50);
}

#[test]
fn flush_interval_zero_means_sync() {
    assert_eq!(PersistenceConfig::parse_flush_interval(Some("0")), 0);
}

#[test]
fn flush_interval_custom_value() {
    assert_eq!(PersistenceConfig::parse_flush_interval(Some("200")), 200);
}

#[test]
fn flush_interval_invalid_falls_back_to_default() {
    assert_eq!(PersistenceConfig::parse_flush_interval(Some("abc")), 50);
}
