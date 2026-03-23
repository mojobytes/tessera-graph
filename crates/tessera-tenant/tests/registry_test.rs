// Copyright 2026 BelowZero Security OU. All rights reserved.

use tempfile::tempdir;
use tessera_graph::{GraphConfig, props};
use tessera_tenant::{DatabaseAddress, DatabaseName, TenantError, TenantId, TenantRegistry};

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

    registry.create_database(&test_addr("acme", "main")).unwrap();
    registry.create_database(&test_addr("globex", "main")).unwrap();

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
    arc.write()
        .unwrap()
        .add_node("Thing", props! {})
        .unwrap();

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

    let errors = registry.flush_all();
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
    arc.write()
        .unwrap()
        .add_node("Node", props! {})
        .unwrap();

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
