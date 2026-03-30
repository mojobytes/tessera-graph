// Copyright 2026 BelowZero Security OU. All rights reserved.

//! Tests validating that WAL recovery provides durability without explicit flush().
//!
//! These tests underpin the deferred-flush optimisation: if WAL recovery can
//! reconstruct all mutations, then per-mutation `flush()` in the Bolt handler
//! is unnecessary and can be moved to a background task.

use tessera_graph::{GraphConfig, Properties};
use tessera_tenant::{DatabaseAddress, DatabaseName, TenantId, TenantRegistry};

fn test_addr(tenant: &str, db: &str) -> DatabaseAddress {
    DatabaseAddress {
        tenant: TenantId::new(tenant).unwrap(),
        database: DatabaseName::new(db).unwrap(),
    }
}

/// A single node added without flush must survive WAL recovery.
#[test]
fn wal_recovery_preserves_node_without_flush() {
    let dir = tempfile::tempdir().unwrap();
    let addr = test_addr("t1", "wal1");

    // Write a node, do NOT flush.
    {
        let registry = TenantRegistry::new(dir.path(), GraphConfig::new());
        let arc = registry.get_or_load(&addr).unwrap();
        let mut g = arc.write().unwrap();
        g.add_node("Person", Properties::new()).unwrap();
        // Intentionally no flush — drop triggers no implicit flush.
        drop(g);
        drop(arc);
        drop(registry);
    }

    // Reopen: WAL replay should reconstruct the node.
    {
        let registry = TenantRegistry::new(dir.path(), GraphConfig::new());
        let arc = registry.get_or_load(&addr).unwrap();
        let g = arc.read().unwrap();
        assert_eq!(
            g.node_count(),
            1,
            "WAL recovery must reconstruct the unflushed node"
        );
    }
}

/// Multiple nodes and edges added without flush must survive WAL recovery.
#[test]
fn wal_recovery_preserves_graph_topology_without_flush() {
    let dir = tempfile::tempdir().unwrap();
    let addr = test_addr("t1", "wal2");

    {
        let registry = TenantRegistry::new(dir.path(), GraphConfig::new());
        let arc = registry.get_or_load(&addr).unwrap();
        let mut g = arc.write().unwrap();

        let a = g.add_node("A", Properties::new()).unwrap();
        let b = g.add_node("B", Properties::new()).unwrap();
        let c = g.add_node("C", Properties::new()).unwrap();
        g.add_edge("KNOWS", a, b, Properties::new()).unwrap();
        g.add_edge("KNOWS", b, c, Properties::new()).unwrap();

        // No flush.
        drop(g);
        drop(arc);
        drop(registry);
    }

    {
        let registry = TenantRegistry::new(dir.path(), GraphConfig::new());
        let arc = registry.get_or_load(&addr).unwrap();
        let g = arc.read().unwrap();
        assert_eq!(g.node_count(), 3, "WAL recovery must reconstruct 3 nodes");
        assert_eq!(g.edge_count(), 2, "WAL recovery must reconstruct 2 edges");
    }
}

/// Interleaved flush + unflushed mutations must all survive.
#[test]
fn wal_recovery_after_partial_flush() {
    let dir = tempfile::tempdir().unwrap();
    let addr = test_addr("t1", "wal3");

    {
        let registry = TenantRegistry::new(dir.path(), GraphConfig::new());
        let arc = registry.get_or_load(&addr).unwrap();
        let mut g = arc.write().unwrap();

        // First batch: flushed.
        g.add_node("Flushed", Properties::new()).unwrap();
        g.add_node("Flushed", Properties::new()).unwrap();
        g.flush().unwrap();

        // Second batch: NOT flushed.
        g.add_node("Unflushed", Properties::new()).unwrap();
        g.add_node("Unflushed", Properties::new()).unwrap();
        g.add_node("Unflushed", Properties::new()).unwrap();

        drop(g);
        drop(arc);
        drop(registry);
    }

    {
        let registry = TenantRegistry::new(dir.path(), GraphConfig::new());
        let arc = registry.get_or_load(&addr).unwrap();
        let g = arc.read().unwrap();
        assert_eq!(
            g.node_count(),
            5,
            "WAL recovery must reconstruct both flushed and unflushed nodes"
        );
    }
}

/// Node label must survive WAL recovery without flush.
#[test]
fn wal_recovery_preserves_label_without_flush() {
    let dir = tempfile::tempdir().unwrap();
    let addr = test_addr("t1", "wal4");

    let original_id;
    {
        let registry = TenantRegistry::new(dir.path(), GraphConfig::new());
        let arc = registry.get_or_load(&addr).unwrap();
        let mut g = arc.write().unwrap();
        original_id = g.add_node("Person", Properties::new()).unwrap();
        drop(g);
        drop(arc);
        drop(registry);
    }

    {
        let registry = TenantRegistry::new(dir.path(), GraphConfig::new());
        let arc = registry.get_or_load(&addr).unwrap();
        let g = arc.read().unwrap();
        assert_eq!(g.node_count(), 1);
        let node = g.node(original_id).unwrap();
        assert_eq!(node.label(), "Person");
    }
}
