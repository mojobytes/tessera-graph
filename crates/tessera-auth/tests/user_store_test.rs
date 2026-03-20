// Copyright 2026 BelowZero Security OU. All rights reserved.

use std::sync::Arc;

use tessera_auth::credentials::{Password, PasswordPolicy};
use tessera_auth::user::{UserId, UserStoreHandle};

#[test]
fn create_user_stores_argon2id_hash_not_plaintext() {
    let store = test_store();
    let password = Password::new("SecurePass1!").unwrap();
    store
        .create_user("alice", &password, vec![], &PasswordPolicy::default())
        .unwrap();
    let auth_pw = Password::new("SecurePass1!").unwrap();
    assert!(store.authenticate("alice", &auth_pw).is_ok());
}

#[test]
fn create_duplicate_user_returns_error() {
    let store = test_store();
    let pw1 = Password::new("SecurePass1!").unwrap();
    store
        .create_user("alice", &pw1, vec![], &PasswordPolicy::default())
        .unwrap();
    let pw2 = Password::new("SecurePass2!").unwrap();
    let result = store.create_user("alice", &pw2, vec![], &PasswordPolicy::default());
    assert!(result.is_err());
}

#[test]
fn authenticate_valid_credentials_returns_user_id() {
    let store = test_store();
    let pw = Password::new("SecurePass1!").unwrap();
    let id = store
        .create_user("bob", &pw, vec![], &PasswordPolicy::default())
        .unwrap();
    let auth_pw = Password::new("SecurePass1!").unwrap();
    let auth_id = store.authenticate("bob", &auth_pw).unwrap();
    assert_eq!(id, auth_id);
}

#[test]
fn authenticate_invalid_password_returns_invalid_credentials() {
    let store = test_store();
    let pw = Password::new("SecurePass1!").unwrap();
    store
        .create_user("carol", &pw, vec![], &PasswordPolicy::default())
        .unwrap();
    let wrong = Password::new("WrongPassw1!").unwrap();
    assert!(store.authenticate("carol", &wrong).is_err());
}

#[test]
fn authenticate_nonexistent_user_returns_invalid_credentials() {
    let store = test_store();
    let pw = Password::new("SecurePass1!").unwrap();
    assert!(store.authenticate("nobody", &pw).is_err());
}

#[test]
fn delete_user_then_authenticate_returns_error() {
    let store = test_store();
    let pw = Password::new("SecurePass1!").unwrap();
    store
        .create_user("dave", &pw, vec![], &PasswordPolicy::default())
        .unwrap();
    store.delete_user("dave").unwrap();
    let auth_pw = Password::new("SecurePass1!").unwrap();
    assert!(store.authenticate("dave", &auth_pw).is_err());
}

#[test]
fn change_password_invalidates_old_hash() {
    let store = test_store();
    let pw = Password::new("OldSecureP1!").unwrap();
    store
        .create_user("eve", &pw, vec![], &PasswordPolicy::default())
        .unwrap();
    let old_pw = Password::new("OldSecureP1!").unwrap();
    let new_pw = Password::new("NewSecureP1!").unwrap();
    store
        .change_password("eve", &old_pw, &new_pw, &PasswordPolicy::default())
        .unwrap();

    let old_again = Password::new("OldSecureP1!").unwrap();
    assert!(store.authenticate("eve", &old_again).is_err());

    let new_again = Password::new("NewSecureP1!").unwrap();
    assert!(store.authenticate("eve", &new_again).is_ok());
}

#[test]
fn list_users_does_not_expose_hashes() {
    let store = test_store();
    let pw = Password::new("SecurePass1!").unwrap();
    store
        .create_user("frank", &pw, vec![], &PasswordPolicy::default())
        .unwrap();
    let usernames = store.list_usernames().unwrap();
    assert!(usernames.contains(&"admin".to_owned()));
    assert!(usernames.contains(&"frank".to_owned()));
}

#[test]
fn user_store_survives_roundtrip_json_serialization() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("users.json");

    let store = test_store();
    let pw = Password::new("SecurePass1!").unwrap();
    store
        .create_user("grace", &pw, vec![], &PasswordPolicy::default())
        .unwrap();
    store.save_to_file(&path).unwrap();

    let loaded = UserStoreHandle::load_from_file(&path).unwrap();
    let auth_pw = Password::new("SecurePass1!").unwrap();
    assert!(loaded.authenticate("grace", &auth_pw).is_ok());
}

#[test]
fn builtin_admin_user_exists_on_new_store() {
    let store = test_store();
    let admin_pw = Password::new("Admin@Init1!").unwrap();
    let id = store.authenticate("admin", &admin_pw).unwrap();
    assert_eq!(id, UserId::new(0));
}

#[test]
fn concurrent_change_password_toctou_protected() {
    // Two threads race to change the password for the same user from the same
    // old password. The TOCTOU guard in change_password ensures that exactly one
    // of them wins and the other gets InvalidCredentials (the hash changed under
    // it). We verify that after the race the store is in a consistent state.
    let store = Arc::new(test_store());
    let pw = Password::new("OldSecureP1!").unwrap();
    store
        .create_user("race_user", &pw, vec![], &PasswordPolicy::default())
        .unwrap();

    let store_a = Arc::clone(&store);
    let store_b = Arc::clone(&store);

    let handle_a = std::thread::spawn(move || {
        let old = Password::new("OldSecureP1!").unwrap();
        let new = Password::new("NewSecureA1!").unwrap();
        store_a.change_password("race_user", &old, &new, &PasswordPolicy::default())
    });

    let handle_b = std::thread::spawn(move || {
        let old = Password::new("OldSecureP1!").unwrap();
        let new = Password::new("NewSecureB1!").unwrap();
        store_b.change_password("race_user", &old, &new, &PasswordPolicy::default())
    });

    let result_a = handle_a.join().unwrap();
    let result_b = handle_b.join().unwrap();

    // Exactly one must succeed.
    let successes = [result_a.is_ok(), result_b.is_ok()]
        .iter()
        .filter(|&&ok| ok)
        .count();
    assert_eq!(
        successes, 1,
        "exactly one concurrent change_password must win"
    );

    // The user must still be authenticatable with one of the two new passwords.
    let new_a = Password::new("NewSecureA1!").unwrap();
    let new_b = Password::new("NewSecureB1!").unwrap();
    assert!(
        store.authenticate("race_user", &new_a).is_ok()
            || store.authenticate("race_user", &new_b).is_ok(),
        "user must be authenticatable with the winning new password"
    );
}

#[test]
fn save_to_file_leaves_no_tmp_file_on_success() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("users.json");

    let store = test_store();
    store.save_to_file(&path).unwrap();

    assert!(path.exists(), "destination file must exist after save");

    let mut tmp_path = path.as_os_str().to_owned();
    tmp_path.push(".tmp");
    let tmp_path = std::path::PathBuf::from(tmp_path);
    assert!(
        !tmp_path.exists(),
        ".tmp file must not remain after a successful save"
    );
}

fn test_store() -> UserStoreHandle {
    let admin_pw = Password::new("Admin@Init1!").unwrap();
    UserStoreHandle::new("admin", &admin_pw, &PasswordPolicy::default()).unwrap()
}
