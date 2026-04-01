// Copyright 2026 BelowZero Security OU. All rights reserved.

use std::sync::Arc;

use tessera_graph_auth::credentials::{Password, PasswordPolicy};
use tessera_graph_auth::policy::AuthPolicy;
use tessera_graph_auth::rbac::{Permission, RoleStore, RoleStoreHandle};
use tessera_graph_auth::session::SessionManager;
use tessera_graph_auth::user::{UserId, UserStoreHandle};

fn test_user_store() -> UserStoreHandle {
    let admin_pw = Password::new("Admin@Init1!").unwrap();
    UserStoreHandle::new("admin", &admin_pw, &PasswordPolicy::default()).unwrap()
}

fn setup_policy(store: &UserStoreHandle) -> AuthPolicy {
    AuthPolicy::new(Arc::new(store.clone()), RoleStoreHandle::with_defaults())
}

#[test]
fn privilege_escalation_readonly_cannot_call_admin_operation() {
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
    let policy = setup_policy(&store);

    assert!(policy.check(user_id, Permission::AdminUsers).is_err());
    assert!(policy.check(user_id, Permission::AdminRoles).is_err());
    assert!(policy.check(user_id, Permission::GraphFlush).is_err());
}

#[test]
fn privilege_escalation_readwrite_cannot_manage_users() {
    let store = test_user_store();
    let pw = Password::new("ReadWrite1!!").unwrap();
    let user_id = store
        .create_user(
            "writer",
            &pw,
            vec![RoleStore::READWRITE_ROLE_ID],
            &PasswordPolicy::default(),
        )
        .unwrap();
    let policy = setup_policy(&store);

    assert!(policy.check(user_id, Permission::AdminUsers).is_err());
    assert!(policy.check(user_id, Permission::AdminRoles).is_err());
    assert!(policy.check(user_id, Permission::AdminAudit).is_err());
}

#[test]
fn token_reuse_after_logout_is_rejected() {
    let store = test_user_store();
    store
        .assign_role("admin", RoleStore::ADMIN_ROLE_ID)
        .unwrap();
    let policy = setup_policy(&store);
    let sessions = SessionManager::new(3600);

    let admin_pw = Password::new("Admin@Init1!").unwrap();
    let admin_id = store.authenticate("admin", &admin_pw).unwrap();
    let token = sessions.create_session(admin_id).unwrap();

    // Valid before revocation
    assert!(
        policy
            .check_session(&token, Permission::NodeRead, &sessions)
            .is_ok()
    );

    // Revoke (logout)
    sessions.revoke(&token).unwrap();

    // Rejected after revocation
    assert!(
        policy
            .check_session(&token, Permission::NodeRead, &sessions)
            .is_err()
    );
}

#[test]
fn concurrent_session_creation_all_tokens_unique() {
    let mgr = Arc::new(SessionManager::new(3600));
    let mut handles = vec![];

    for i in 0..100 {
        let mgr = Arc::clone(&mgr);
        handles.push(std::thread::spawn(move || {
            mgr.create_session(UserId::new(i)).unwrap()
        }));
    }

    let tokens: Vec<String> = handles
        .into_iter()
        .map(|h| h.join().unwrap().as_str().to_owned())
        .collect();

    let unique: std::collections::HashSet<_> = tokens.iter().collect();
    assert_eq!(unique.len(), 100, "all 100 tokens must be unique");
}

#[test]
fn password_hash_never_appears_in_list_users_output() {
    let store = test_user_store();
    let pw = Password::new("SecretPass1!").unwrap();
    store
        .create_user("alice", &pw, vec![], &PasswordPolicy::default())
        .unwrap();

    let usernames = store.list_usernames().unwrap();
    for name in &usernames {
        assert!(
            !name.contains("argon2"),
            "username list leaked a hash: {name}"
        );
    }
}

#[test]
fn expired_token_even_one_second_over_is_rejected() {
    // TTL of 1 second — token expires 1 second after creation.
    // Sleeping 2 seconds guarantees `now > expires_at` even on slow CI.
    let mgr = SessionManager::new(1);
    let token = mgr.create_session(UserId::new(1)).unwrap();
    std::thread::sleep(std::time::Duration::from_secs(2));

    assert!(mgr.validate(&token).is_err());
}
