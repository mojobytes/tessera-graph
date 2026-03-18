// Copyright 2026 BelowZero Security OU. All rights reserved.

use tessera_auth::rbac::{Permission, RoleStore, RoleStoreHandle};

#[test]
fn admin_role_has_all_permissions() {
    let store = RoleStore::with_defaults();
    let admin = store.get(RoleStore::ADMIN_ROLE_ID).unwrap();
    for perm in Permission::all() {
        assert!(
            admin.permissions().contains(perm),
            "admin role missing permission: {perm}"
        );
    }
}

#[test]
fn readonly_role_cannot_create_nodes() {
    let store = RoleStore::with_defaults();
    let readonly = store.get(RoleStore::READONLY_ROLE_ID).unwrap();
    assert!(!readonly.permissions().contains(&Permission::NodeCreate));
}

#[test]
fn readwrite_role_can_create_and_delete() {
    let store = RoleStore::with_defaults();
    let rw = store.get(RoleStore::READWRITE_ROLE_ID).unwrap();
    assert!(rw.permissions().contains(&Permission::NodeCreate));
    assert!(rw.permissions().contains(&Permission::NodeDelete));
    assert!(rw.permissions().contains(&Permission::EdgeCreate));
    assert!(rw.permissions().contains(&Permission::EdgeDelete));
}

#[test]
fn monitor_role_has_only_monitor_permission() {
    let store = RoleStore::with_defaults();
    let monitor = store.get(RoleStore::MONITOR_ROLE_ID).unwrap();
    assert!(monitor.permissions().contains(&Permission::Monitor));
    assert!(!monitor.permissions().contains(&Permission::NodeCreate));
    assert!(!monitor.permissions().contains(&Permission::AdminUsers));
}

#[test]
fn custom_role_with_single_permission() {
    let mut store = RoleStore::with_defaults();
    let perms = std::iter::once(Permission::GraphBackup).collect();
    let id = store.create_custom_role("backup-operator", perms).unwrap();
    let role = store.get(id).unwrap();
    assert_eq!(role.name(), "backup-operator");
    assert!(role.permissions().contains(&Permission::GraphBackup));
    assert_eq!(role.permissions().len(), 1);
}

#[test]
fn permission_display_roundtrip() {
    for &perm in Permission::all() {
        let s = perm.to_string();
        let parsed: Permission = s.parse().unwrap();
        assert_eq!(perm, parsed);
    }
}

#[test]
fn role_is_serializable_json() {
    let store = RoleStore::with_defaults();
    let admin = store.get(RoleStore::ADMIN_ROLE_ID).unwrap();
    let json = serde_json::to_string(admin).unwrap();
    let _: serde_json::Value = serde_json::from_str(&json).unwrap();
}

#[test]
fn cannot_delete_predefined_role() {
    let mut store = RoleStore::with_defaults();
    assert!(store.delete_role(RoleStore::ADMIN_ROLE_ID).is_err());
    assert!(store.delete_role(RoleStore::READONLY_ROLE_ID).is_err());
}

#[test]
fn can_delete_custom_role() {
    let mut store = RoleStore::with_defaults();
    let perms = std::iter::once(Permission::Monitor).collect();
    let id = store.create_custom_role("temp", perms).unwrap();
    assert!(store.delete_role(id).is_ok());
    assert!(store.get(id).is_none());
}

#[test]
fn role_store_handle_is_send_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<RoleStoreHandle>();
}

#[test]
fn role_store_handle_gets_predefined_role() {
    let handle = RoleStoreHandle::with_defaults();
    let admin = handle.get(RoleStore::ADMIN_ROLE_ID);
    assert!(admin.is_some());
    assert_eq!(admin.unwrap().name(), "admin");
}

#[test]
fn role_store_handle_collects_permissions() {
    let handle = RoleStoreHandle::with_defaults();
    let perms = handle.collect_permissions(&[RoleStore::READONLY_ROLE_ID]);
    assert!(perms.contains(&Permission::NodeRead));
    assert!(perms.contains(&Permission::EdgeRead));
    assert!(!perms.contains(&Permission::NodeCreate));
}
