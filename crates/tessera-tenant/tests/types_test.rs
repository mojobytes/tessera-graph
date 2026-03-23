// Copyright 2026 BelowZero Security OU. All rights reserved.

use tessera_tenant::{DatabaseAddress, DatabaseName, TenantError, TenantId};

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
