// SPDX-License-Identifier: BSL-1.1

//! `AuthorizationPolicy` — grant-based access control (**Enterprise edition**).
//!
//! This trait covers who may read or write which database: issuing and
//! revoking grants, listing them, and resolving the effective access level
//! for a `(user, database)` pair. It is the label-based/role-based access
//! control surface — an Enterprise concern — and is deliberately kept
//! separate from local user management (see [`super::UserStore`], Community)
//! and from the database catalogue (see [`super::DatabaseCatalog`]).
//!
//! The trait is **defined** in the Community server so the open code can hold
//! an `Arc<dyn AuthorizationPolicy>` and wire a policy in, but the Community
//! edition ships no real implementation — a single-database Community server
//! grants unconditional access and never consults a policy. The real,
//! grant-checking implementation lives in the separate (private) Enterprise
//! repository, which is why this is public API rather than `pub(crate)`.

use async_trait::async_trait;

use super::{AccessLevel, AuthStoreError};

// Los tipos de datos de los permisos viven aquí, con la interfaz que los usa.
// Estaban en `traits.rs`, junto a los de autenticación, que sí es Community:
// un fichero público acababa describiendo la forma de `SHOW GRANTS`. La
// interfaz es pública por decisión de producto; sus tipos la acompañan.

/// Target of a grant: either a specific database or the singleton wildcard.
///
/// Used as the input to `AuthStore::grant` / `AuthStore::revoke`.
/// The output-side counterpart returned from `list_grants` is
/// [`GrantTargetName`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GrantTarget {
    Named(String),
    Wildcard,
}

/// Summary of a grant as returned by `list_grants`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    pub username: String,
    pub target: GrantTargetName,
    pub access_level: AccessLevel,
}

/// Serialisable target name for `SHOW GRANTS` output.
///
/// Separate from [`GrantTarget`] so it can evolve independently (e.g. if
/// we later want to tag the wildcard with a display name without
/// touching the input enum).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GrantTargetName {
    Named(String),
    Wildcard,
}

/// Grant-based authorization. **Enterprise edition.**
///
/// Object-safe + async, held as `Arc<dyn AuthorizationPolicy>`.
#[async_trait]
pub trait AuthorizationPolicy: Send + Sync + 'static {
    async fn grant(
        &self,
        username: &str,
        target: GrantTarget,
        level: AccessLevel,
    ) -> Result<(), AuthStoreError>;

    async fn revoke(&self, username: &str, target: GrantTarget) -> Result<(), AuthStoreError>;

    /// List grants, optionally narrowed by user, by target database, or by
    /// both. Wildcard grants are included only when `filter_database` is
    /// `None` — a wildcard is not a specific match for "who has access to
    /// this database?", so the database filter excludes them.
    async fn list_grants(
        &self,
        filter_user: Option<&str>,
        filter_database: Option<&str>,
    ) -> Result<Vec<Grant>, AuthStoreError>;

    /// Hot-path method for the handler: resolves the `is_admin` shortcut,
    /// specific grant, then wildcard grant, in that order, returning the
    /// winning access level (specific beats wildcard, even when weaker).
    async fn effective_access(
        &self,
        username: &str,
        database: &str,
    ) -> Result<AccessLevel, AuthStoreError>;

    /// Idempotently create the `:Wildcard` singleton used as the endpoint for
    /// wildcard grants. Called during startup.
    async fn ensure_bootstrap(&self) -> Result<(), AuthStoreError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    struct StubPolicy;

    #[async_trait]
    impl AuthorizationPolicy for StubPolicy {
        async fn grant(
            &self,
            _username: &str,
            _target: GrantTarget,
            _level: AccessLevel,
        ) -> Result<(), AuthStoreError> {
            unimplemented!()
        }
        async fn revoke(
            &self,
            _username: &str,
            _target: GrantTarget,
        ) -> Result<(), AuthStoreError> {
            unimplemented!()
        }
        async fn list_grants(
            &self,
            _filter_user: Option<&str>,
            _filter_database: Option<&str>,
        ) -> Result<Vec<Grant>, AuthStoreError> {
            unimplemented!()
        }
        async fn effective_access(
            &self,
            _username: &str,
            _database: &str,
        ) -> Result<AccessLevel, AuthStoreError> {
            unimplemented!()
        }
        async fn ensure_bootstrap(&self) -> Result<(), AuthStoreError> {
            unimplemented!()
        }
    }

    fn assert_object_safe(_policy: &dyn AuthorizationPolicy) {}

    /// Compile-time contract: `AuthorizationPolicy` must stay object-safe so
    /// the handler can hold it as `Arc<dyn AuthorizationPolicy>`.
    #[test]
    fn authorization_policy_is_object_safe() {
        assert_object_safe(&StubPolicy);
    }
}
