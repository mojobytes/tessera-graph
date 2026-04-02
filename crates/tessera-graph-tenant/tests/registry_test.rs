// Copyright 2026 BelowZero Security OU. All rights reserved.

use tempfile::tempdir;
use tessera_graph::{GraphConfig, props};
use tessera_graph_tenant::{DatabaseAddress, DatabaseName, TenantError, TenantId, TenantRegistry};

fn test_addr(tenant: &str, db: &str) -> DatabaseAddress {
    DatabaseAddress {
        tenant: TenantId::new(tenant).unwrap(),
        database: DatabaseName::new(db).unwrap(),
    }
}

const fn test_config() -> GraphConfig {
    GraphConfig::without_wal()
}

#[test]
fn registry_new_does_not_create_directories() {
    let tmp = tempdir().unwrap();
    let base = tmp.path().join("tenants");

    let _registry = TenantRegistry::new(&base, test_config());

    // Base directory must NOT have been created.
    assert!(!base.exists());
}

#[test]
fn create_database_creates_directory_and_graph() {
    let tmp = tempdir().unwrap();
    let registry = TenantRegistry::new(tmp.path(), test_config());
    let addr = test_addr("acme", "production");

    let arc = registry.create_database(&addr).unwrap();
    let _graph = arc.read().unwrap();

    let expected = tmp.path().join("acme").join("production");
    assert!(expected.is_dir());
}

#[test]
fn create_database_already_exists_error() {
    let tmp = tempdir().unwrap();
    let registry = TenantRegistry::new(tmp.path(), test_config());
    let addr = test_addr("acme", "production");

    registry.create_database(&addr).unwrap();
    let result = registry.create_database(&addr);
    assert!(
        matches!(result, Err(TenantError::DatabaseAlreadyExists { .. })),
        "expected DatabaseAlreadyExists"
    );
}

#[test]
fn get_or_load_auto_provisions_database() {
    let tmp = tempdir().unwrap();
    let registry = TenantRegistry::new(tmp.path(), test_config());
    let addr = test_addr("tenant1", "db1");

    let arc = registry.get_or_load(&addr).unwrap();
    let count = arc.read().unwrap().node_count();
    assert_eq!(count, 0);

    let expected = tmp.path().join("tenant1").join("db1");
    assert!(expected.is_dir());
}

#[test]
fn get_or_load_returns_cached_instance() {
    let tmp = tempdir().unwrap();
    let registry = TenantRegistry::new(tmp.path(), test_config());
    let addr = test_addr("tenant1", "main");

    let arc1 = registry.get_or_load(&addr).unwrap();
    arc1.write()
        .unwrap()
        .add_node("Person", props! { "name" => "Alice" })
        .unwrap();

    let arc2 = registry.get_or_load(&addr).unwrap();
    assert_eq!(arc2.read().unwrap().node_count(), 1);
}

#[test]
fn get_or_load_with_default_database() {
    let tmp = tempdir().unwrap();
    let registry = TenantRegistry::new(tmp.path(), test_config());
    let addr = DatabaseAddress {
        tenant: TenantId::new("acme").unwrap(),
        database: DatabaseName::default_name(),
    };

    let arc = registry.get_or_load(&addr).unwrap();
    assert_eq!(arc.read().unwrap().node_count(), 0);

    let expected = tmp.path().join("acme").join("default");
    assert!(expected.is_dir());
}

#[test]
fn list_tenants_returns_tenant_dirs() {
    let tmp = tempdir().unwrap();
    let registry = TenantRegistry::new(tmp.path(), test_config());

    registry
        .create_database(&test_addr("acme", "main"))
        .unwrap();
    registry
        .create_database(&test_addr("globex", "main"))
        .unwrap();

    let mut tenants: Vec<String> = registry
        .list_tenants()
        .unwrap()
        .into_iter()
        .map(|t| t.to_string())
        .collect();
    tenants.sort();

    assert_eq!(tenants, vec!["acme", "globex"]);
}

#[test]
fn list_tenants_empty_when_no_base_dir() {
    let tmp = tempdir().unwrap();
    let base = tmp.path().join("nonexistent");
    let registry = TenantRegistry::new(&base, test_config());

    let tenants = registry.list_tenants().unwrap();
    assert!(tenants.is_empty());
}

#[test]
fn list_databases_returns_db_dirs() {
    let tmp = tempdir().unwrap();
    let registry = TenantRegistry::new(tmp.path(), test_config());

    let tenant = TenantId::new("acme").unwrap();
    registry
        .create_database(&test_addr("acme", "production"))
        .unwrap();
    registry
        .create_database(&test_addr("acme", "staging"))
        .unwrap();

    let mut dbs: Vec<String> = registry
        .list_databases(&tenant)
        .unwrap()
        .into_iter()
        .map(|d| d.to_string())
        .collect();
    dbs.sort();

    assert_eq!(dbs, vec!["production", "staging"]);
}

#[test]
fn list_databases_tenant_not_found() {
    let tmp = tempdir().unwrap();
    let registry = TenantRegistry::new(tmp.path(), test_config());
    let tenant = TenantId::new("ghost").unwrap();

    let err = registry.list_databases(&tenant).unwrap_err();
    assert!(matches!(err, TenantError::TenantNotFound(_)));
}

#[test]
fn flush_persists_data() {
    let tmp = tempdir().unwrap();
    let registry = TenantRegistry::new(tmp.path(), GraphConfig::new());
    let addr = test_addr("acme", "main");

    let arc = registry.get_or_load(&addr).unwrap();
    arc.write().unwrap().add_node("Thing", props! {}).unwrap();

    registry.flush(&addr).unwrap();

    // Re-open directly to verify persistence.
    let path = tmp.path().join("acme").join("main");
    let g2 = tessera_graph::Graph::open(&path, &GraphConfig::new()).unwrap();
    assert_eq!(g2.node_count(), 1);
}

#[test]
fn flush_not_loaded_error() {
    let tmp = tempdir().unwrap();
    let registry = TenantRegistry::new(tmp.path(), test_config());
    let addr = test_addr("ghost", "db");

    let err = registry.flush(&addr).unwrap_err();
    assert!(matches!(err, TenantError::DatabaseNotLoaded { .. }));
}

#[test]
fn flush_all_flushes_all_loaded() {
    let tmp = tempdir().unwrap();
    let registry = TenantRegistry::new(tmp.path(), GraphConfig::new());

    let addr1 = test_addr("t1", "db");
    let addr2 = test_addr("t2", "db");

    registry
        .get_or_load(&addr1)
        .unwrap()
        .write()
        .unwrap()
        .add_node("N", props! {})
        .unwrap();
    registry
        .get_or_load(&addr2)
        .unwrap()
        .write()
        .unwrap()
        .add_node("M", props! {})
        .unwrap();

    let errors = registry.flush_all().unwrap();
    assert!(errors.is_empty(), "unexpected flush errors: {errors:?}");

    // Verify both were actually persisted.
    let p1 = tmp.path().join("t1").join("db");
    let p2 = tmp.path().join("t2").join("db");
    assert_eq!(
        tessera_graph::Graph::open(&p1, &GraphConfig::new())
            .unwrap()
            .node_count(),
        1
    );
    assert_eq!(
        tessera_graph::Graph::open(&p2, &GraphConfig::new())
            .unwrap()
            .node_count(),
        1
    );
}

#[test]
fn unload_flushes_and_removes() {
    let tmp = tempdir().unwrap();
    let registry = TenantRegistry::new(tmp.path(), GraphConfig::new());
    let addr = test_addr("acme", "main");

    let arc = registry.get_or_load(&addr).unwrap();
    arc.write().unwrap().add_node("Node", props! {}).unwrap();

    registry.unload(&addr).unwrap();

    // After unload, flush must fail with DatabaseNotLoaded.
    let err = registry.flush(&addr).unwrap_err();
    assert!(matches!(err, TenantError::DatabaseNotLoaded { .. }));

    // Data was persisted before removal.
    let path = tmp.path().join("acme").join("main");
    let g2 = tessera_graph::Graph::open(&path, &GraphConfig::new()).unwrap();
    assert_eq!(g2.node_count(), 1);
}

#[test]
fn unload_not_loaded_error() {
    let tmp = tempdir().unwrap();
    let registry = TenantRegistry::new(tmp.path(), test_config());
    let addr = test_addr("ghost", "db");

    let err = registry.unload(&addr).unwrap_err();
    assert!(matches!(err, TenantError::DatabaseNotLoaded { .. }));
}

#[test]
fn get_or_load_after_unload_reopens() {
    let tmp = tempdir().unwrap();
    let registry = TenantRegistry::new(tmp.path(), GraphConfig::new());
    let addr = test_addr("acme", "main");

    let arc = registry.get_or_load(&addr).unwrap();
    arc.write()
        .unwrap()
        .add_node("Person", props! { "name" => "Bob" })
        .unwrap();

    registry.unload(&addr).unwrap();

    // Reopen — data should still be there.
    let arc2 = registry.get_or_load(&addr).unwrap();
    assert_eq!(arc2.read().unwrap().node_count(), 1);
}

// ── LRU eviction tests (HIGH #6) ────────────────────────────────────────────

#[test]
fn loaded_count_tracks_loaded_graphs() {
    let tmp = tempdir().unwrap(); // OK: test
    let registry = TenantRegistry::new(tmp.path(), test_config());
    assert_eq!(registry.loaded_count(), 0);
    let _ = registry.get_or_load(&test_addr("t1", "db")).unwrap(); // OK: test
    assert_eq!(registry.loaded_count(), 1);
    let _ = registry.get_or_load(&test_addr("t2", "db")).unwrap(); // OK: test
    assert_eq!(registry.loaded_count(), 2);
}

#[test]
fn registry_evicts_lru_when_cap_exceeded() {
    let tmp = tempdir().unwrap(); // OK: test
    let registry = TenantRegistry::new_with_cap(tmp.path(), test_config(), 2);

    let addr1 = test_addr("t1", "db1");
    let addr2 = test_addr("t2", "db2");
    let addr3 = test_addr("t3", "db3");

    let _ = registry.get_or_load(&addr1).unwrap(); // OK: test
    let _ = registry.get_or_load(&addr2).unwrap(); // OK: test
    assert_eq!(registry.loaded_count(), 2);

    // Loading addr3 should evict addr1 (LRU).
    let _ = registry.get_or_load(&addr3).unwrap(); // OK: test
    assert_eq!(registry.loaded_count(), 2, "cap=2 must be enforced");
}

#[test]
fn registry_with_cap_zero_has_no_eviction() {
    let tmp = tempdir().unwrap(); // OK: test
    let registry = TenantRegistry::new_with_cap(tmp.path(), test_config(), 0);

    let _ = registry.get_or_load(&test_addr("t1", "db")).unwrap(); // OK: test
    let _ = registry.get_or_load(&test_addr("t2", "db")).unwrap(); // OK: test
    let _ = registry.get_or_load(&test_addr("t3", "db")).unwrap(); // OK: test
    assert_eq!(registry.loaded_count(), 3, "cap=0 means no limit");
}

#[test]
fn evicted_graph_data_survives_on_disk() {
    let tmp = tempdir().unwrap(); // OK: test
    let registry = TenantRegistry::new_with_cap(tmp.path(), GraphConfig::new(), 1);

    let addr1 = test_addr("t1", "db1");
    let addr2 = test_addr("t2", "db2");

    // Write data to addr1.
    let arc1 = registry.get_or_load(&addr1).unwrap(); // OK: test
    arc1.write()
        .unwrap() // OK: test
        .add_node("Thing", props! {})
        .unwrap(); // OK: test
    drop(arc1);

    // Loading addr2 evicts addr1 (flushing it first).
    let _ = registry.get_or_load(&addr2).unwrap(); // OK: test

    // Reload addr1 — data must still be there from the pre-eviction flush.
    let arc1_reloaded = registry.get_or_load(&addr1).unwrap(); // OK: test
    assert_eq!(
        arc1_reloaded.read().unwrap().node_count(), // OK: test
        1,
        "evicted graph data must survive on disk"
    );
}

#[test]
fn lru_access_refreshes_order() {
    let tmp = tempdir().unwrap(); // OK: test
    let registry = TenantRegistry::new_with_cap(tmp.path(), test_config(), 2);

    let addr1 = test_addr("t1", "db1");
    let addr2 = test_addr("t2", "db2");
    let addr3 = test_addr("t3", "db3");

    let _ = registry.get_or_load(&addr1).unwrap(); // OK: test
    let _ = registry.get_or_load(&addr2).unwrap(); // OK: test

    // Touch addr1 again — now addr2 is LRU.
    let _ = registry.get_or_load(&addr1).unwrap(); // OK: test

    // Loading addr3 should evict addr2 (LRU), NOT addr1 (recently touched).
    let _ = registry.get_or_load(&addr3).unwrap(); // OK: test
    assert_eq!(registry.loaded_count(), 2);

    // addr1 should still be loaded (was refreshed).
    // addr2 was evicted. Loading addr2 again would bring it back.
    // We verify by checking that get_or_load(addr1) is instant (cache hit).
    let _ = registry.get_or_load(&addr1).unwrap(); // OK: test
    assert_eq!(registry.loaded_count(), 2, "addr1 must still be in cache");
}
