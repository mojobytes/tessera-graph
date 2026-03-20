// Copyright 2026 BelowZero Security OU. All rights reserved.

use std::sync::{Arc, RwLock};

use tessera_auth::credentials::{Password, PasswordPolicy};
use tessera_auth::policy::AuthPolicy;
use tessera_auth::rbac::{Permission, RoleStore, RoleStoreHandle};
use tessera_auth::user::{UserId, UserStoreHandle};

#[test]
fn admin_user_can_perform_any_operation() {
    let (policy, admin_id) = setup_with_admin();
    for &perm in Permission::all() {
        assert!(
            policy.check(admin_id, perm).is_ok(),
            "admin should have permission: {perm}"
        );
    }
}

#[test]
fn readonly_user_cannot_create_node() {
    let (policy, readonly_id) = setup_with_readonly_user();
    let result = policy.check(readonly_id, Permission::NodeCreate);
    assert!(result.is_err());
}

#[test]
fn readonly_user_can_read_node() {
    let (policy, readonly_id) = setup_with_readonly_user();
    assert!(policy.check(readonly_id, Permission::NodeRead).is_ok());
}

#[test]
fn user_with_no_roles_is_denied_everything() {
    let (policy, norole_id) = setup_with_no_role_user();
    for &perm in Permission::all() {
        assert!(
            policy.check(norole_id, perm).is_err(),
            "user with no roles should be denied: {perm}"
        );
    }
}

#[test]
fn user_with_multiple_roles_union_of_permissions() {
    let store = test_user_store();
    let mut role_store = RoleStore::with_defaults();

    // Create a custom role with just GraphBackup
    let backup_perms = std::iter::once(Permission::GraphBackup).collect();
    let backup_role_id = role_store
        .create_custom_role("backup", backup_perms)
        .unwrap();

    // Create user with readonly + backup roles
    let pw = Password::new("TestPass1!!!").unwrap();
    let user_id = store
        .create_user(
            "multi",
            &pw,
            vec![RoleStore::READONLY_ROLE_ID, backup_role_id],
            &PasswordPolicy::default(),
        )
        .unwrap();

    let policy = AuthPolicy::new(
        Arc::new(store),
        RoleStoreHandle::from_arc(Arc::new(RwLock::new(role_store))),
    );

    // Has readonly perms
    assert!(policy.check(user_id, Permission::NodeRead).is_ok());
    // Has backup perm from custom role
    assert!(policy.check(user_id, Permission::GraphBackup).is_ok());
    // Doesn't have write perms
    assert!(policy.check(user_id, Permission::NodeCreate).is_err());
}

#[test]
fn unknown_user_id_is_denied() {
    let (policy, _) = setup_with_admin();
    let fake_id = UserId::new(9999);
    assert!(policy.check(fake_id, Permission::NodeRead).is_err());
}

#[test]
fn permission_denied_error_contains_required_permission() {
    let (policy, readonly_id) = setup_with_readonly_user();
    let err = policy
        .check(readonly_id, Permission::NodeCreate)
        .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("node:create"),
        "error should mention required permission, got: {msg}"
    );
}

// --- Helpers ---

fn test_user_store() -> UserStoreHandle {
    let admin_pw = Password::new("Admin@Init1!").unwrap();
    UserStoreHandle::new("admin", &admin_pw, &PasswordPolicy::default()).unwrap()
}

fn setup_with_admin() -> (AuthPolicy, UserId) {
    let store = test_user_store();
    store
        .assign_role("admin", RoleStore::ADMIN_ROLE_ID)
        .unwrap();
    let admin_pw = Password::new("Admin@Init1!").unwrap();
    let admin_id = store.authenticate("admin", &admin_pw).unwrap();
    let policy = AuthPolicy::new(Arc::new(store), RoleStoreHandle::with_defaults());
    (policy, admin_id)
}

fn setup_with_readonly_user() -> (AuthPolicy, UserId) {
    let store = test_user_store();
    let pw = Password::new("ReadOnly1!!!").unwrap();
    let user_id = store
        .create_user(
            "reader",
            &pw,
            vec![RoleStore::READONLY_ROLE_ID],
            &PasswordPolicy::default(),
        )
        .unwrap();
    let policy = AuthPolicy::new(Arc::new(store), RoleStoreHandle::with_defaults());
    (policy, user_id)
}

fn setup_with_no_role_user() -> (AuthPolicy, UserId) {
    let store = test_user_store();
    let pw = Password::new("NoRoles1!!!!").unwrap();
    let user_id = store
        .create_user("norole", &pw, vec![], &PasswordPolicy::default())
        .unwrap();
    let policy = AuthPolicy::new(Arc::new(store), RoleStoreHandle::with_defaults());
    (policy, user_id)
}
