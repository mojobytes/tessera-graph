// Copyright 2026 BelowZero Security OU. All rights reserved.

use std::sync::Arc;
use std::thread;

use tempfile::tempdir;
use tessera_graph::{GraphConfig, props};
use tessera_graph_tenant::{DatabaseAddress, DatabaseName, TenantError, TenantId, TenantRegistry};

fn test_addr() -> DatabaseAddress {
    DatabaseAddress {
        tenant: TenantId::new("concurrent-tenant").unwrap(),
        database: DatabaseName::new("shared-db").unwrap(),
    }
}

/// Eight threads call `get_or_load` simultaneously on the same address.
/// All must succeed and observe each other's writes (same `Arc`).
#[test]
fn concurrent_get_or_load_same_address_single_instance() {
    let tmp = tempdir().unwrap();
    let registry = Arc::new(TenantRegistry::new(tmp.path(), GraphConfig::without_wal()));
    let addr = test_addr();

    // Pre-create so all threads are loading (not racing on create_dir_all).
    registry.create_database(&addr).unwrap();

    // Spawn all threads before joining any — intentional two-phase pattern.
    #[allow(clippy::needless_collect)]
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let reg = Arc::clone(&registry);
            let a = addr.clone();
            thread::spawn(move || reg.get_or_load(&a).unwrap())
        })
        .collect();

    let arcs: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();

    // Write a node via the first arc.
    arcs[0].write().unwrap().add_node("X", props! {}).unwrap();

    // All other arcs must see the same node (they share the same Arc).
    for arc in &arcs[1..] {
        assert_eq!(
            arc.read().unwrap().node_count(),
            1,
            "arc does not share the same Graph instance"
        );
    }
}

/// Eight threads call `create_database` simultaneously.
/// Exactly one must succeed; the rest must get `DatabaseAlreadyExists`.
#[test]
fn concurrent_create_database_only_one_succeeds() {
    let tmp = tempdir().unwrap();
    let registry = Arc::new(TenantRegistry::new(tmp.path(), GraphConfig::without_wal()));
    let addr = test_addr();

    // Spawn all threads before joining any — intentional two-phase pattern.
    #[allow(clippy::needless_collect)]
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let reg = Arc::clone(&registry);
            let a = addr.clone();
            thread::spawn(move || reg.create_database(&a))
        })
        .collect();

    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();

    let successes = results.iter().filter(|r| r.is_ok()).count();
    let already_exists = results
        .iter()
        .filter(|r| matches!(r, Err(TenantError::DatabaseAlreadyExists { .. })))
        .count();

    assert_eq!(
        successes, 1,
        "expected exactly one success, got {successes}"
    );
    assert_eq!(
        already_exists, 7,
        "expected 7 DatabaseAlreadyExists, got {already_exists}"
    );
}
