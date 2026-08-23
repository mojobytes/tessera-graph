// SPDX-License-Identifier: BSL-1.1

//! `UserStore` — local user management (Community edition).
//!
//! This trait covers **only** the management of local user accounts:
//! creating, disabling, re-passwording, and listing them. It deliberately
//! does **not** include grants (see `AuthorizationPolicy`) or the database
//! catalogue (see `DatabaseCatalog`), both of which are Enterprise concerns
//! and live behind their own traits.
//!
//! The split exists so the open Community server can authenticate local
//! users without pulling in the authorization/multi-tenancy machinery: a
//! `LocalAuthProvider` needs only a `UserStore`, never the full identity
//! surface. Because the Enterprise crate lives in a separate repository, this
//! trait — like every other extension point — is public API, not
//! `pub(crate)`.

use async_trait::async_trait;

use super::{AuthStoreError, SecretString, UserSummary};

/// Read-only projection of a stored user, containing exactly the fields a
/// provider needs to authenticate a login attempt — the password hash to
/// verify against, whether the account is enabled, and the admin bit.
///
/// This lets a provider authenticate against a `UserStore` trait object
/// without the store having to expose its internal record layout or its
/// user table. It never carries the plaintext password.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserAuthRecord {
    /// `PHC`-format password hash to verify the presented credential against.
    pub password_hash: String,
    /// `false` for a disabled account; authentication must still run a real
    /// verify against the hash so a disabled account is indistinguishable
    /// from a wrong password to a network observer.
    pub enabled: bool,
    /// Whether the principal is flagged as admin in the store.
    pub is_admin: bool,
    /// Opaque stable identifier for the user (`UUIDv7` when backed by the
    /// system graph).
    pub id: String,
}

/// Local user account management. **Community edition.**
///
/// Object-safe + async, so it can be held as `Arc<dyn UserStore>`.
#[async_trait]
pub trait UserStore: Send + Sync + 'static {
    async fn create_user(
        &self,
        username: &str,
        password_plain: &SecretString,
        is_admin: bool,
    ) -> Result<(), AuthStoreError>;

    async fn drop_user(&self, username: &str) -> Result<(), AuthStoreError>;

    async fn set_password(
        &self,
        username: &str,
        password_plain: &SecretString,
    ) -> Result<(), AuthStoreError>;

    async fn set_enabled(&self, username: &str, enabled: bool) -> Result<(), AuthStoreError>;

    async fn set_admin(&self, username: &str, is_admin: bool) -> Result<(), AuthStoreError>;

    async fn list_users(&self) -> Result<Vec<UserSummary>, AuthStoreError>;

    /// Fetch the authentication-relevant projection of a user by name.
    ///
    /// This is the single hook a `LocalAuthProvider` uses to verify a login,
    /// replacing direct access to the store's internal user table. Returns
    /// `Ok(None)` when no such user exists; the caller is responsible for
    /// running a dummy verify in that case to keep response timing flat.
    async fn get_user_for_auth(
        &self,
        username: &str,
    ) -> Result<Option<UserAuthRecord>, AuthStoreError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    struct StubUserStore;

    #[async_trait]
    impl UserStore for StubUserStore {
        async fn create_user(
            &self,
            _username: &str,
            _password_plain: &SecretString,
            _is_admin: bool,
        ) -> Result<(), AuthStoreError> {
            unimplemented!()
        }
        async fn drop_user(&self, _username: &str) -> Result<(), AuthStoreError> {
            unimplemented!()
        }
        async fn set_password(
            &self,
            _username: &str,
            _password_plain: &SecretString,
        ) -> Result<(), AuthStoreError> {
            unimplemented!()
        }
        async fn set_enabled(&self, _username: &str, _enabled: bool) -> Result<(), AuthStoreError> {
            unimplemented!()
        }
        async fn set_admin(&self, _username: &str, _is_admin: bool) -> Result<(), AuthStoreError> {
            unimplemented!()
        }
        async fn list_users(&self) -> Result<Vec<UserSummary>, AuthStoreError> {
            unimplemented!()
        }
        async fn get_user_for_auth(
            &self,
            _username: &str,
        ) -> Result<Option<UserAuthRecord>, AuthStoreError> {
            unimplemented!()
        }
    }

    /// Accepts any `UserStore` as a trait object — only compiles while the
    /// trait stays object-safe.
    fn assert_object_safe(_store: &dyn UserStore) {}

    /// Compile-time contract: `UserStore` must stay object-safe so it can be
    /// held as `Arc<dyn UserStore>` by a provider. If it stops being
    /// object-safe (e.g. a generic method is added), this fails to compile.
    #[test]
    fn user_store_is_object_safe() {
        assert_object_safe(&StubUserStore);
    }
}
