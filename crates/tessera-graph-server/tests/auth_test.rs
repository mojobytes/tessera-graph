// SPDX-License-Identifier: BSL-1.1

//! Unit tests for auth primitives. Renewed for Fase 1a — the previous
//! single-password `PasswordAuthProvider` is gone; multi-user via the
//! `SystemGraphAuthProvider` is exercised in `auth_system_graph_test.rs`
//! (added in Task 5).

use tessera_graph_server::auth::SecretString;

#[test]
fn secret_string_holds_bytes() {
    let s = SecretString::new("hunter2".to_owned());
    assert_eq!(s.as_bytes(), b"hunter2");
    assert_eq!(s.len(), 7);
    assert!(!s.is_empty());
}

#[test]
fn secret_string_empty_is_allowed_at_type_level() {
    // Policy (min length) is enforced at the AuthStore, not at the type.
    let s = SecretString::new(String::new());
    assert_eq!(s.as_bytes(), b"");
    assert!(s.is_empty());
}

#[test]
fn secret_string_debug_is_redacted() {
    let s = SecretString::new("hunter2".to_owned());
    let rendered = format!("{s:?}");
    assert!(!rendered.contains("hunter2"));
    assert!(rendered.contains("redacted"));
    assert!(rendered.contains('7'));
}
