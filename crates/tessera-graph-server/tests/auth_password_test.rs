// SPDX-License-Identifier: LicenseRef-TesseraGraph-Proprietary

//! argon2id roundtrip and policy tests.

use tessera_graph_server::auth::{
    MAX_PASSWORD_LEN, MIN_PASSWORD_LEN, PasswordError, SecretString, hash_password,
    verify_password,
};

#[test]
fn hash_password_roundtrip_verifies() {
    let plain = SecretString::new("correct horse battery staple".to_owned());
    let phc = hash_password(&plain).expect("hash");
    assert!(phc.starts_with("$argon2id$"));
    let verify = SecretString::new("correct horse battery staple".to_owned());
    assert!(verify_password(&verify, &phc).expect("verify"));
}

#[test]
fn hash_password_differs_between_calls() {
    let plain = SecretString::new("hunter2hunter2".to_owned());
    let h1 = hash_password(&plain).unwrap();
    let plain2 = SecretString::new("hunter2hunter2".to_owned());
    let h2 = hash_password(&plain2).unwrap();
    assert_ne!(h1, h2, "different salts must produce different PHC strings");
}

#[test]
fn verify_password_rejects_wrong() {
    let plain = SecretString::new("hunter2hunter2".to_owned());
    let phc = hash_password(&plain).unwrap();
    let wrong = SecretString::new("hunter3hunter3".to_owned());
    assert!(!verify_password(&wrong, &phc).unwrap());
}

#[test]
fn hash_password_rejects_too_long() {
    let long = "a".repeat(MAX_PASSWORD_LEN + 1);
    let plain = SecretString::new(long);
    let err = hash_password(&plain).unwrap_err();
    assert!(matches!(err, PasswordError::TooLong));
}

#[test]
fn hash_password_accepts_max_length() {
    let max = "a".repeat(MAX_PASSWORD_LEN);
    let plain = SecretString::new(max);
    assert!(hash_password(&plain).is_ok());
}

#[test]
fn verify_password_rejects_malformed_phc() {
    let plain = SecretString::new("hunter2hunter2".to_owned());
    let err = verify_password(&plain, "not-a-phc-string").unwrap_err();
    assert!(matches!(err, PasswordError::Hash(_)));
}

#[test]
fn password_length_constants_match_spec() {
    assert_eq!(MIN_PASSWORD_LEN, 8);
    assert_eq!(MAX_PASSWORD_LEN, 1024);
}
