// SPDX-License-Identifier: BSL-1.1

//! Trait-shape tests for the async `AuthProvider` + `NoAuthProvider`.

use std::sync::Arc;

use tessera_graph_server::auth::{AuthError, AuthOutcome, AuthProvider, NoAuthProvider};

#[tokio::test]
async fn no_auth_accepts_any_credentials() {
    let provider: Arc<dyn AuthProvider> = Arc::new(NoAuthProvider);
    let outcome = provider
        .authenticate("whatever", "whatever")
        .await
        .expect("no-auth must succeed");
    assert_eq!(outcome.user_id, "anonymous");
    assert!(outcome.roles.is_empty());
}

#[tokio::test]
async fn no_auth_accepts_empty_credentials() {
    let provider: Arc<dyn AuthProvider> = Arc::new(NoAuthProvider);
    let outcome = provider.authenticate("", "").await.expect("ok");
    assert_eq!(outcome.user_id, "anonymous");
}

#[test]
fn auth_error_display_is_documented() {
    let _ = format!("{}", AuthError::InvalidCredentials);
    let _ = format!("{}", AuthError::UnknownUser);
    let _ = format!("{}", AuthError::UserDisabled);
    let _ = format!("{}", AuthError::Backend("db down".to_owned()));
}

#[test]
fn auth_outcome_roles_default_empty() {
    let outcome = AuthOutcome {
        user_id: "u".to_owned(),
        roles: vec![],
        is_admin: false,
    };
    assert!(outcome.roles.is_empty());
}

#[test]
fn auth_provider_is_object_safe() {
    fn accept(_: Arc<dyn AuthProvider>) {}
    accept(Arc::new(NoAuthProvider));
}

#[test]
fn auth_outcome_exposes_is_admin_field() {
    let outcome = tessera_graph_server::auth::AuthOutcome {
        user_id: "u".into(),
        roles: vec![],
        is_admin: true,
    };
    assert!(outcome.is_admin);
}

#[test]
fn auth_outcome_default_is_admin_is_false_for_noop() {
    use tessera_graph_server::auth::{AuthProvider, NoAuthProvider};
    let provider = NoAuthProvider;
    let outcome = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(provider.authenticate("anything", "anything"))
        .expect("noop always succeeds");
    assert!(!outcome.is_admin);
}
