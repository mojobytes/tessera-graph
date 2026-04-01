// Copyright 2026 BelowZero Security OU. All rights reserved.

use tessera_graph_tenant::{DatabaseAddress, DatabaseName, TenantError, TenantId};

#[test]
fn tenant_id_new_valid() {
    let id = TenantId::new("acme-corp").unwrap();
    assert_eq!(id.as_str(), "acme-corp");
    assert_eq!(id.to_string(), "acme-corp");
}

#[test]
fn tenant_id_rejects_empty() {
    let err = TenantId::new("").unwrap_err();
    assert!(matches!(err, TenantError::InvalidName(_)));
}

#[test]
fn tenant_id_rejects_slash() {
    let err = TenantId::new("acme/corp").unwrap_err();
    assert!(matches!(err, TenantError::InvalidName(_)));
}

#[test]
fn database_name_default_is_literal_default() {
    let name = DatabaseName::default_name();
    assert_eq!(name.as_str(), DatabaseName::DEFAULT);
    assert_eq!(name.as_str(), "default");
}

#[test]
fn database_name_new_valid() {
    let name = DatabaseName::new("production").unwrap();
    assert_eq!(name.as_str(), "production");
}

#[test]
fn database_name_rejects_empty() {
    let err = DatabaseName::new("").unwrap_err();
    assert!(matches!(err, TenantError::InvalidName(_)));
}

#[test]
fn database_name_rejects_slash() {
    let err = DatabaseName::new("prod/db").unwrap_err();
    assert!(matches!(err, TenantError::InvalidName(_)));
}

#[test]
fn database_address_display_format() {
    let addr = DatabaseAddress {
        tenant: TenantId::new("acme").unwrap(),
        database: DatabaseName::new("production").unwrap(),
    };
    assert_eq!(addr.to_string(), "acme/production");
}

// --- C1: path-traversal rejection tests ---

#[test]
fn tenant_id_rejects_dot_dot() {
    assert!(matches!(
        TenantId::new("..").unwrap_err(),
        TenantError::InvalidName(_)
    ));
}

#[test]
fn tenant_id_rejects_single_dot() {
    assert!(matches!(
        TenantId::new(".").unwrap_err(),
        TenantError::InvalidName(_)
    ));
}

#[test]
fn tenant_id_rejects_null_byte() {
    assert!(matches!(
        TenantId::new("a\0b").unwrap_err(),
        TenantError::InvalidName(_)
    ));
}

#[test]
fn tenant_id_rejects_backslash() {
    assert!(matches!(
        TenantId::new(r"a\b").unwrap_err(),
        TenantError::InvalidName(_)
    ));
}

#[test]
fn tenant_id_rejects_dot_prefix() {
    assert!(matches!(
        TenantId::new(".hidden").unwrap_err(),
        TenantError::InvalidName(_)
    ));
}

#[test]
fn tenant_id_rejects_space() {
    assert!(matches!(
        TenantId::new("a b").unwrap_err(),
        TenantError::InvalidName(_)
    ));
}

#[test]
fn tenant_id_rejects_unicode() {
    assert!(matches!(
        TenantId::new("café").unwrap_err(),
        TenantError::InvalidName(_)
    ));
}

#[test]
fn tenant_id_allows_alphanumeric_hyphen_underscore() {
    assert!(TenantId::new("acme-corp_2").is_ok());
    assert!(TenantId::new("ACME123").is_ok());
    assert!(TenantId::new("a").is_ok());
}

#[test]
fn database_name_rejects_dot_dot() {
    assert!(matches!(
        DatabaseName::new("..").unwrap_err(),
        TenantError::InvalidName(_)
    ));
}

#[test]
fn database_name_rejects_null_byte() {
    assert!(matches!(
        DatabaseName::new("prod\0uction").unwrap_err(),
        TenantError::InvalidName(_)
    ));
}

#[test]
fn database_name_rejects_backslash() {
    assert!(matches!(
        DatabaseName::new(r"prod\db").unwrap_err(),
        TenantError::InvalidName(_)
    ));
}

#[test]
fn database_name_rejects_space() {
    assert!(matches!(
        DatabaseName::new("prod db").unwrap_err(),
        TenantError::InvalidName(_)
    ));
}

#[test]
fn database_name_allows_alphanumeric_hyphen_underscore() {
    assert!(DatabaseName::new("production-v2").is_ok());
    assert!(DatabaseName::new("test_db").is_ok());
}
