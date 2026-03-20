// Copyright 2026 BelowZero Security OU. All rights reserved.

use std::sync::Arc;

use tessera_audit::AuditLog;
use tessera_auth::AuthPolicy;
use tessera_auth::credentials::{Password, PasswordPolicy};
use tessera_auth::rbac::{Permission, RoleStore, RoleStoreHandle};
use tessera_auth::session::SessionManager;
use tessera_auth::user::UserStoreHandle;
use tessera_protocol::tls::{ClientAuth, TlsConfigBuilder};
use tessera_server::context::ServerContext;

fn test_tls_config() -> tessera_protocol::TlsConfig {
    let dir = tempfile::tempdir().unwrap();
    let key_pair = rcgen::KeyPair::generate().unwrap();
    let params = rcgen::CertificateParams::new(vec!["localhost".to_owned()]).unwrap();
    let cert = params.self_signed(&key_pair).unwrap();
    let cert_path = dir.path().join("cert.pem");
    let key_path = dir.path().join("key.pem");
    std::fs::write(&cert_path, cert.pem()).unwrap();
    std::fs::write(&key_path, key_pair.serialize_pem()).unwrap();

    TlsConfigBuilder::new()
        .cert_file(cert_path)
        .key_file(key_path)
        .client_auth(ClientAuth::None)
        .build()
        .unwrap()
}

fn test_context() -> ServerContext {
    let admin_pw = Password::new("Admin@Init1!").unwrap();
    let user_store =
        Arc::new(UserStoreHandle::new("admin", &admin_pw, &PasswordPolicy::default()).unwrap());
    let sessions = Arc::new(SessionManager::new(3600));
    let policy = Arc::new(AuthPolicy::new(
        Arc::clone(&user_store),
        RoleStoreHandle::with_defaults(),
    ));

    let dir = tempfile::tempdir().unwrap();
    let audit = Arc::new(AuditLog::open(&dir.path().join("audit.ndjson")).unwrap());

    let tls = test_tls_config();

    ServerContext::new(policy, sessions, audit, tls, user_store)
}

#[test]
fn server_context_is_send_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<ServerContext>();
}

#[test]
fn permission_check_propagates_through_context() {
    let ctx = test_context();

    // Create a session for admin
    let admin_pw = Password::new("Admin@Init1!").unwrap();
    let user_store =
        Arc::new(UserStoreHandle::new("admin", &admin_pw, &PasswordPolicy::default()).unwrap());
    user_store
        .assign_role("admin", RoleStore::ADMIN_ROLE_ID)
        .unwrap();

    let sessions = Arc::new(SessionManager::new(3600));
    let policy = Arc::new(AuthPolicy::new(
        user_store.clone(),
        RoleStoreHandle::with_defaults(),
    ));

    let admin_pw2 = Password::new("Admin@Init1!").unwrap();
    let admin_id = user_store.authenticate("admin", &admin_pw2).unwrap();
    let token = sessions.create_session(admin_id).unwrap();

    let dir = tempfile::tempdir().unwrap();
    let audit = Arc::new(AuditLog::open(&dir.path().join("audit.ndjson")).unwrap());
    let tls = test_tls_config();

    let ctx2 = ServerContext::new(policy, sessions, audit, tls, user_store);
    assert!(
        ctx2.check_permission(&token, Permission::NodeCreate)
            .is_ok()
    );

    // Original context's sessions don't have this token
    assert!(
        ctx.check_permission(&token, Permission::NodeCreate)
            .is_err()
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
