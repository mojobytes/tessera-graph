// SPDX-License-Identifier: BSL-1.1

//! `AuthProvider` — the authentication extension point, plus the shared value
//! types identity works with.
//!
//! **Community.** Verifying a credential is the whole surface here.
//!
//! There used to be a second, much wider trait in this file covering user
//! management, grants and the multi-database catalogue at once. The split
//! replaced it with three narrower ones (`UserStore`, `AuthorizationPolicy`,
//! `DatabaseCatalog`), and it survived a while as a bridge that the new ones
//! delegated to. That bridge is gone: the last implementor and the last caller
//! disappeared when the identity store was partitioned, and a trait nobody
//! implements is not an extension point, it is scaffolding.
//!
//! The grant and catalogue value types moved out with it, to the interfaces
//! that use them: they described the shape of paid administrative statements
//! from a file that travels whole to the public repository.

use async_trait::async_trait;

/// Successful authentication outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthOutcome {
    /// Opaque stable identifier (`UUIDv7` when backed by the system graph).
    pub user_id: String,
    /// Assigned roles. Always empty in Fase 1a; populated by RBAC in the
    /// enterprise edition.
    pub roles: Vec<String>,
    /// `true` when the authenticated principal is flagged as admin in
    /// the auth store. Admin bypass of per-database grants (§5.2) uses
    /// this bit. Populated by each `AuthProvider` impl; `false` for
    /// providers with no concept of admin (e.g. `NoAuthProvider`).
    pub is_admin: bool,
}

/// Authentication failure reasons.
///
/// The handler MUST map `InvalidCredentials` / `UnknownUser` /
/// `UserDisabled` to the same generic Bolt error ("authentication
/// failed"). Only the audit log sees the specific variant.
#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("invalid credentials")]
    InvalidCredentials,
    #[error("user not found")]
    UnknownUser,
    #[error("user disabled")]
    UserDisabled,
    #[error("auth backend error: {0}")]
    Backend(String),
}

/// Validate credentials. Object-safe + async.
///
/// Implementations MUST take care that the timing of their response does
/// not reveal the reason for failure to a network observer. See the spec
/// Section 5.3 for the dummy-verify mitigation used by
/// `SystemGraphAuthProvider`.
#[async_trait]
pub trait AuthProvider: Send + Sync + 'static {
    async fn authenticate(
        &self,
        principal: &str,
        credentials: &str,
    ) -> Result<AuthOutcome, AuthError>;
}

/// Access level for a (user, database) pair as seen by the handler.
///
/// Used by `AuthStore::effective_access` and the per-statement
/// `grant-WRITE` check in the Bolt handler (Task 10).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessLevel {
    None,
    Read,
    ReadWrite,
}

impl AccessLevel {
    #[must_use]
    pub const fn allows_write(self) -> bool {
        matches!(self, Self::ReadWrite)
    }

    #[must_use]
    pub const fn allows_read(self) -> bool {
        matches!(self, Self::Read | Self::ReadWrite)
    }
}

/// Non-sensitive summary of a stored user. Never exposes the password hash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserSummary {
    pub username: String,
    pub enabled: bool,
    pub is_admin: bool,
    pub created_at: String,
}

#[derive(Debug, thiserror::Error)]
pub enum AuthStoreError {
    #[error("user already exists: {0}")]
    UserExists(String),
    #[error("user not found: {0}")]
    UserNotFound(String),
    #[error("password too short (min {min} bytes)")]
    PasswordTooShort { min: usize },
    #[error("password too long (max {max} bytes)")]
    PasswordTooLong { max: usize },
    #[error("password cannot be empty")]
    PasswordEmpty,
    #[error("invalid username: {reason}")]
    InvalidUsername { reason: String },
    #[error("cannot remove last admin")]
    LastAdmin,
    #[error("database already exists: {0}")]
    DatabaseExists(String),
    #[error("database not found: {0}")]
    DatabaseNotFound(String),
    #[error("invalid database name: {reason}")]
    InvalidDatabaseName { reason: String },
    #[error("invalid quota: {reason}")]
    InvalidQuota { reason: String },
    #[error("invalid grant: {reason}")]
    InvalidGrant { reason: String },
    #[error("backend error: {0}")]
    Backend(String),
}
