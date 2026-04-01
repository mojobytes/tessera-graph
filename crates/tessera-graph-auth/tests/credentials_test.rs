// Copyright 2026 BelowZero Security OU. All rights reserved.

use tessera_graph_auth::credentials::{Password, PasswordHasher, PasswordPolicy};

#[test]
fn hash_password_produces_valid_argon2id_hash() {
    let hasher = PasswordHasher::new();
    let password = Password::new("SecurePass123!").unwrap();
    let hash = hasher.hash(&password).unwrap();
    assert!(
        hash.as_str().starts_with("$argon2id$"),
        "hash must start with $argon2id$, got: {}",
        hash.as_str()
    );
}

#[test]
fn verify_correct_password_returns_ok() {
    let hasher = PasswordHasher::new();
    let password = Password::new("SecurePass123!").unwrap();
    let hash = hasher.hash(&password).unwrap();
    assert!(hasher.verify(&password, &hash).is_ok());
}

#[test]
fn verify_wrong_password_returns_err() {
    let hasher = PasswordHasher::new();
    let password = Password::new("SecurePass123!").unwrap();
    let hash = hasher.hash(&password).unwrap();
    let wrong = Password::new("WrongPassword1!").unwrap();
    assert!(hasher.verify(&wrong, &hash).is_err());
}

#[test]
fn two_hashes_of_same_password_differ() {
    let hasher = PasswordHasher::new();
    let password = Password::new("SecurePass123!").unwrap();
    let hash1 = hasher.hash(&password).unwrap();
    let hash2 = hasher.hash(&password).unwrap();
    assert_ne!(
        hash1.as_str(),
        hash2.as_str(),
        "different salts must produce different hashes"
    );
}

#[test]
fn empty_password_is_rejected_by_policy() {
    let result = Password::new("");
    assert!(result.is_err());
}

#[test]
fn password_too_short_is_rejected() {
    let result = Password::new("Short1!");
    assert!(result.is_err());
}

#[test]
fn custom_policy_enforces_uppercase() {
    let policy = PasswordPolicy::builder()
        .min_length(8)
        .require_uppercase(true)
        .build();
    let result = Password::with_policy("alllowercase1!", &policy);
    assert!(result.is_err());
}

#[test]
fn custom_policy_enforces_digit() {
    let policy = PasswordPolicy::builder()
        .min_length(8)
        .require_digit(true)
        .build();
    let result = Password::with_policy("NoDigitsHere!", &policy);
    assert!(result.is_err());
}

#[test]
fn custom_policy_enforces_symbol() {
    let policy = PasswordPolicy::builder()
        .min_length(8)
        .require_symbol(true)
        .build();
    let result = Password::with_policy("NoSymbolHere1", &policy);
    assert!(result.is_err());
}

#[test]
fn default_policy_accepts_strong_password() {
    let password = Password::new("StrongP@ss1");
    assert!(password.is_ok());
}

#[test]
fn builder_default_enforces_all_requirements() {
    // PasswordPolicyBuilder::default() must mirror PasswordPolicy::default():
    // min_length=8, require_uppercase=true, require_digit=true, require_symbol=true.
    let policy = PasswordPolicy::builder().build();

    // Missing uppercase
    assert!(Password::with_policy("alllower1!", &policy).is_err());
    // Missing digit
    assert!(Password::with_policy("NoDigitHere!", &policy).is_err());
    // Missing symbol
    assert!(Password::with_policy("NoSymbolHere1", &policy).is_err());
    // Too short
    assert!(Password::with_policy("Ab1!", &policy).is_err());
    // Satisfies all requirements
    assert!(Password::with_policy("StrongP@ss1", &policy).is_ok());
}
