// Copyright 2026 BelowZero Security OU. All rights reserved.

use std::fmt;

/// Unique tenant identifier (e.g., "acme-corp").
/// Must be non-empty and cannot contain '/'.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TenantId(String);

impl TenantId {
    /// Creates a new `TenantId` from the given name.
    ///
    /// # Errors
    ///
    /// Returns [`crate::TenantError::InvalidName`] if the name is empty or contains `/`.
    pub fn new(name: impl Into<String>) -> Result<Self, crate::TenantError> {
        let name = name.into();
        if name.is_empty() {
            return Err(crate::TenantError::InvalidName("tenant name cannot be empty".into()));
        }
        if name.contains('/') {
            return Err(crate::TenantError::InvalidName(
                "tenant name cannot contain '/'".into(),
            ));
        }
        Ok(Self(name))
    }

    /// Returns the underlying string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for TenantId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Database name within a tenant (e.g., "production").
/// Must be non-empty and cannot contain '/'.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DatabaseName(String);

impl DatabaseName {
    /// The literal string used as the default database name.
    pub const DEFAULT: &'static str = "default";

    /// Creates a new `DatabaseName` from the given name.
    ///
    /// # Errors
    ///
    /// Returns [`crate::TenantError::InvalidName`] if the name is empty or contains `/`.
    pub fn new(name: impl Into<String>) -> Result<Self, crate::TenantError> {
        let name = name.into();
        if name.is_empty() {
            return Err(crate::TenantError::InvalidName(
                "database name cannot be empty".into(),
            ));
        }
        if name.contains('/') {
            return Err(crate::TenantError::InvalidName(
                "database name cannot contain '/'".into(),
            ));
        }
        Ok(Self(name))
    }

    /// Returns a `DatabaseName` with the value `"default"`.
    #[must_use]
    pub fn default_name() -> Self {
        Self(Self::DEFAULT.to_owned())
    }

    /// Returns the underlying string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for DatabaseName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Full database address: tenant + database.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DatabaseAddress {
    /// The tenant this database belongs to.
    pub tenant: TenantId,
    /// The database within the tenant.
    pub database: DatabaseName,
}

impl fmt::Display for DatabaseAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.tenant, self.database)
    }
}
