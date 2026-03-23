// Copyright 2026 BelowZero Security OU. All rights reserved.

mod common;

use std::sync::Arc;

use tessera_audit::AuditLog;
use tessera_auth::credentials::{Password, PasswordPolicy};
use tessera_auth::policy::AuthPolicy;
use tessera_auth::rbac::{Permission, RoleStore, RoleStoreHandle};
use tessera_auth::session::SessionManager;
use tessera_auth::user::UserStoreHandle;
use tessera_server::context::ServerContext;

use common::{test_context, test_registry, test_tls_config};

#[test]
fn server_context_is_send_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<ServerContext>();
}

#[test]
fn permission_check_propagates_through_context() {
    let _ctx = test_context();

    // Create a separate context with its own stores to test permission check.
    let admin_pw = Password::new("Admin@Init1!").unwrap();
    let user_store =
        Arc::new(UserStoreHandle::new("admin", &admin_pw, &PasswordPolicy::default()).unwrap());
    user_store
        .assign_role("admin", RoleStore::ADMIN_ROLE_ID)
        .unwrap();

    let sessions = Arc::new(SessionManager::new(3600));
    let policy = Arc::new(AuthPolicy::new(
        Arc::clone(&user_store),
        RoleStoreHandle::with_defaults(),
    ));

    let admin_pw2 = Password::new("Admin@Init1!").unwrap();
    let admin_id = user_store.authenticate("admin", &admin_pw2).unwrap();
    let token = sessions.create_session(admin_id).unwrap();

    let dir = tempfile::tempdir().unwrap();
    let audit = Arc::new(AuditLog::open(&dir.path().join("audit.ndjson")).unwrap());
    let tls = test_tls_config();
    let registry = test_registry();

    let metrics = Arc::new(tessera_monitor::MetricsRegistry::new(256));
    let ctx2 = ServerContext::new(policy, sessions, audit, tls, user_store, metrics, registry);
    assert!(
        ctx2.check_permission(&token, Permission::NodeCreate)
            .is_ok()
    );
}

#[test]
fn unauthenticated_request_is_denied() {
    let ctx = test_context();
    let fake_token = tessera_auth::SessionToken::from_raw("fake-token".to_owned());
    assert!(
        ctx.check_permission(&fake_token, Permission::NodeRead)
            .is_err()
    );
}
