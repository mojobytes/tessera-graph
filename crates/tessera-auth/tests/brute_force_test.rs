// Copyright 2026 BelowZero Security OU. All rights reserved.

use tessera_auth::AuthError;
use tessera_auth::credentials::{Password, PasswordPolicy};
use tessera_auth::rate_limit::{LoginAttemptTracker, LoginPolicy};
use tessera_auth::user::UserStoreHandle;

#[test]
fn five_failed_attempts_trigger_lockout() {
    let tracker = LoginAttemptTracker::new();
    let policy = LoginPolicy::new(5, 60);

    for _ in 0..5 {
        tracker.record_failure("victim");
    }

    assert!(tracker.is_locked("victim", &policy));
}

#[test]
fn locked_account_rejects_correct_password() {
    let store = test_store();
    let tracker = LoginAttemptTracker::new();
    let login_policy = LoginPolicy::new(3, 60);

    // 3 bad attempts
    for _ in 0..3 {
        let bad_pw = Password::new("WrongPassw1!").unwrap();
        let _ = store.authenticate_with_rate_limit("admin", &bad_pw, &tracker, &login_policy);
    }

    // Now correct password is also rejected
    let good_pw = Password::new("Admin@Init1!").unwrap();
    let result = store.authenticate_with_rate_limit("admin", &good_pw, &tracker, &login_policy);
    assert!(matches!(result, Err(AuthError::AccountLocked)));
}

#[test]
fn lockout_expires_after_configured_duration() {
    // 1-second lockout duration — sleeping 2 seconds guarantees expiry on slow CI.
    let tracker = LoginAttemptTracker::new();
    let policy = LoginPolicy::new(3, 1);

    for _ in 0..3 {
        tracker.record_failure("user1");
    }

    std::thread::sleep(std::time::Duration::from_secs(2));
    assert!(!tracker.is_locked("user1", &policy));
}

#[test]
fn successful_login_resets_failure_counter() {
    let store = test_store();
    let tracker = LoginAttemptTracker::new();
    let login_policy = LoginPolicy::new(5, 60);

    // 4 bad attempts (just under threshold)
    for _ in 0..4 {
        let bad_pw = Password::new("WrongPassw1!").unwrap();
        let _ = store.authenticate_with_rate_limit("admin", &bad_pw, &tracker, &login_policy);
    }

    // Successful login resets counter
    let good_pw = Password::new("Admin@Init1!").unwrap();
    assert!(
        store
            .authenticate_with_rate_limit("admin", &good_pw, &tracker, &login_policy)
            .is_ok()
    );

    // Can fail 4 more times without lockout
    for _ in 0..4 {
        let bad_pw = Password::new("WrongPassw1!").unwrap();
        let _ = store.authenticate_with_rate_limit("admin", &bad_pw, &tracker, &login_policy);
    }
    assert!(!tracker.is_locked("admin", &login_policy));
}

#[test]
fn different_users_have_independent_counters() {
    let tracker = LoginAttemptTracker::new();
    let policy = LoginPolicy::new(3, 60);

    for _ in 0..3 {
        tracker.record_failure("alice");
    }

    assert!(tracker.is_locked("alice", &policy));
    assert!(!tracker.is_locked("bob", &policy));
}

fn test_store() -> UserStoreHandle {
    let admin_pw = Password::new("Admin@Init1!").unwrap();
    UserStoreHandle::new("admin", &admin_pw, &PasswordPolicy::default()).unwrap()
}
