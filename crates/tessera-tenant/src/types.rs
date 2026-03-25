// Copyright 2026 BelowZero Security OU. All rights reserved.

use std::fmt;

/// Unique tenant identifier (e.g., "acme-corp").
///
/// Must be non-empty and contain only ASCII alphanumeric characters, hyphens (`-`),
/// or underscores (`_`). This whitelist prevents path-traversal attacks when the
/// name is used as a filesystem directory component.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TenantId(String);

impl TenantId {
    /// Creates a new `TenantId` from the given name.
    ///
    /// # Errors
    ///
    /// Returns [`crate::TenantError::InvalidName`] if the name is empty or contains
    /// characters outside `[a-zA-Z0-9_-]`.
    pub fn new(name: impl Into<String>) -> Result<Self, crate::TenantError> {
        let name = name.into();
        if !is_valid_name(&name) {
            return Err(crate::TenantError::InvalidName(format!(
                "tenant name must be non-empty and contain only [a-zA-Z0-9_-], got: {name:?}"
            )));
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
///
/// Must be non-empty and contain only ASCII alphanumeric characters, hyphens (`-`),
/// or underscores (`_`). This whitelist prevents path-traversal attacks when the
/// name is used as a filesystem directory component.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DatabaseName(String);

impl DatabaseName {
    /// The literal string used as the default database name.
    pub const DEFAULT: &'static str = "default";

    /// Creates a new `DatabaseName` from the given name.
    ///
    /// # Errors
    ///
    /// Returns [`crate::TenantError::InvalidName`] if the name is empty or contains
    /// characters outside `[a-zA-Z0-9_-]`.
    pub fn new(name: impl Into<String>) -> Result<Self, crate::TenantError> {
        let name = name.into();
        if !is_valid_name(&name) {
            return Err(crate::TenantError::InvalidName(format!(
                "database name must be non-empty and contain only [a-zA-Z0-9_-], got: {name:?}"
            )));
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

/// Returns `true` iff `name` is non-empty and every character is ASCII
/// alphanumeric, `-`, or `_`. This whitelist guarantees the name is safe to
/// use as a single filesystem directory component — no traversal, no null
/// bytes, no shell-special characters.
fn is_valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}
