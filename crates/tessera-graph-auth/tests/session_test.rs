// Copyright 2026 BelowZero Security OU. All rights reserved.

use std::sync::Arc;

use tessera_graph_auth::session::{SessionManager, SessionToken};
use tessera_graph_auth::user::UserId;

#[test]
fn create_session_returns_opaque_token() {
    let mgr = SessionManager::new(3600);
    let token = mgr.create_session(UserId::new(1)).unwrap();
    assert!(!token.as_str().is_empty());
}

#[test]
fn validate_valid_token_returns_user_id() {
    let mgr = SessionManager::new(3600);
    let token = mgr.create_session(UserId::new(42)).unwrap();
    let id = mgr.validate(&token).unwrap();
    assert_eq!(id, UserId::new(42));
}

#[test]
fn validate_expired_token_returns_token_expired() {
    // TTL of 1 second — token expires 1 second after creation.
    // Sleeping 2 seconds guarantees `now > expires_at` even on slow CI.
    let mgr = SessionManager::new(1);
    let token = mgr.create_session(UserId::new(1)).unwrap();
    std::thread::sleep(std::time::Duration::from_secs(2));
    let result = mgr.validate(&token);
    assert!(result.is_err());
}

#[test]
fn validate_unknown_token_returns_token_invalid() {
    let mgr = SessionManager::new(3600);
    let fake = SessionToken::from_raw("totally-fake-token".to_owned());
    let result = mgr.validate(&fake);
    assert!(result.is_err());
}

#[test]
fn revoke_session_then_validate_returns_token_invalid() {
    let mgr = SessionManager::new(3600);
    let token = mgr.create_session(UserId::new(1)).unwrap();
    mgr.revoke(&token).unwrap();
    assert!(mgr.validate(&token).is_err());
}

#[test]
fn two_sessions_for_same_user_are_independent() {
    let mgr = SessionManager::new(3600);
    let t1 = mgr.create_session(UserId::new(1)).unwrap();
    let t2 = mgr.create_session(UserId::new(1)).unwrap();
    assert_ne!(t1.as_str(), t2.as_str());

    // Revoking one doesn't affect the other
    mgr.revoke(&t1).unwrap();
    assert!(mgr.validate(&t1).is_err());
    assert!(mgr.validate(&t2).is_ok());
}

#[test]
fn session_manager_is_send_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<SessionManager>();
}

#[test]
fn token_is_url_safe_base64() {
    let mgr = SessionManager::new(3600);
    let token = mgr.create_session(UserId::new(1)).unwrap();
    let s = token.as_str();
    // URL-safe base64 uses only alphanumeric, '-', '_', and optionally '='
    assert!(
        s.chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == '_' || c == '='),
        "token contains non-URL-safe characters: {s}"
    );
}

#[test]
fn revoke_all_for_user_invalidates_all_sessions() {
    let mgr = SessionManager::new(3600);
    let t1 = mgr.create_session(UserId::new(5)).unwrap();
    let t2 = mgr.create_session(UserId::new(5)).unwrap();
    let t3 = mgr.create_session(UserId::new(6)).unwrap();

    mgr.revoke_all_for_user(UserId::new(5)).unwrap();

    assert!(mgr.validate(&t1).is_err());
    assert!(mgr.validate(&t2).is_err());
    assert!(mgr.validate(&t3).is_ok()); // different user unaffected
}

#[test]
fn revoke_returns_ok_on_success() {
    let mgr = SessionManager::new(3600);
    let token = mgr.create_session(UserId::new(1)).unwrap();
    assert!(mgr.revoke(&token).is_ok());
}

#[test]
fn revoke_unknown_token_returns_ok() {
    let mgr = SessionManager::new(3600);
    let fake = SessionToken::from_raw("nonexistent".to_owned());
    assert!(mgr.revoke(&fake).is_ok());
}

#[test]
fn revoke_all_for_user_returns_ok() {
    let mgr = SessionManager::new(3600);
    let _ = mgr.create_session(UserId::new(9)).unwrap();
    assert!(mgr.revoke_all_for_user(UserId::new(9)).is_ok());
}

#[test]
fn session_token_equality_is_constant_time() {
    let t1 = SessionToken::from_raw("abc".to_owned());
    let t2 = SessionToken::from_raw("abc".to_owned());
    let t3 = SessionToken::from_raw("xyz".to_owned());
    assert!(t1 == t2, "equal tokens must compare as equal");
    assert!(t1 != t3, "different tokens must compare as not equal");
}

#[test]
fn concurrent_validates_of_valid_token_all_succeed() {
    let mgr = Arc::new(SessionManager::new(3600));
    let token = Arc::new(mgr.create_session(UserId::new(7)).unwrap());

    let handles: Vec<_> = (0..50)
        .map(|_| {
            let mgr = Arc::clone(&mgr);
            let token = Arc::clone(&token);
            std::thread::spawn(move || mgr.validate(&token))
        })
        .collect();

    for handle in handles {
        assert!(
            handle.join().unwrap().is_ok(),
            "all concurrent validates of a live token must succeed"
        );
    }
}
