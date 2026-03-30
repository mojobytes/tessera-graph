// Copyright 2026 BelowZero Security OU. All rights reserved.

//! Tests for the background flush task.

use std::sync::Arc;
use tempfile::TempDir;
use tessera_graph::{Graph, GraphConfig, Properties};
use tessera_server::flush_task::spawn_background_flush;
use tessera_tenant::{DatabaseAddress, DatabaseName, TenantId, TenantRegistry};

fn test_config() -> GraphConfig {
    GraphConfig {
        memory_limit_bytes: 4 * 1024 * 1024,
        create_if_missing: true,
        adj_cache_capacity: 128,
        wal_enabled: true,
    }
}

fn test_registry(tmp: &TempDir) -> Arc<TenantRegistry> {
    Arc::new(TenantRegistry::new(tmp.path(), test_config()))
}

fn test_addr(tenant: &str, db: &str) -> DatabaseAddress {
    DatabaseAddress {
        tenant: TenantId::new(tenant).unwrap(),
        database: DatabaseName::new(db).unwrap(),
    }
}

#[tokio::test]
async fn background_flush_task_flushes_dirty_graph() {
    let tmp = TempDir::new().unwrap();
    let registry = test_registry(&tmp);

    // Load a graph and write a node WITHOUT flushing manually.
    let addr = test_addr("t", "db");
    let graph_arc = registry.get_or_load(&addr).unwrap();
    {
        let mut g = graph_arc.write().unwrap();
        g.add_node("FlushMe", Properties::new()).unwrap();
    }

    // Spawn the background flush task with a short interval.
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let handle = spawn_background_flush(Arc::clone(&registry), 10, shutdown_rx);

    // Give the task time to tick at least once.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // Signal shutdown and wait for the task to exit.
    let _ = shutdown_tx.send(true);
    handle.await.unwrap();

    // Verify: reopen the graph from disk — the node must have been persisted.
    drop(registry);
    let graph_path = tmp.path().join("t").join("db");
    let recovered = Graph::open(&graph_path, &test_config()).unwrap();
    let nodes = recovered.node_ids();
    assert!(!nodes.is_empty(), "flush task should have persisted the node to disk");
}

#[tokio::test]
async fn background_flush_task_exits_on_shutdown() {
    let tmp = TempDir::new().unwrap();
    let registry = test_registry(&tmp);

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let handle = spawn_background_flush(Arc::clone(&registry), 10, shutdown_rx);

    // Signal shutdown immediately.
    let _ = shutdown_tx.send(true);

    // Task should resolve quickly.
    let result = tokio::time::timeout(
        std::time::Duration::from_millis(200),
        handle,
    ).await;
    assert!(result.is_ok(), "flush task must exit within 200ms of shutdown signal");
}

#[tokio::test]
async fn background_flush_zero_interval_returns_noop() {
    let tmp = TempDir::new().unwrap();
    let registry = test_registry(&tmp);

    let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let handle = spawn_background_flush(Arc::clone(&registry), 0, shutdown_rx);

    // With interval=0 (sync mode), the task should complete immediately.
    let result = tokio::time::timeout(
        std::time::Duration::from_millis(200),
        handle,
    ).await;
    assert!(result.is_ok(), "noop flush task (interval=0) must exit immediately");
}
