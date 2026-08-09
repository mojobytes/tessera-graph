// SPDX-License-Identifier: BSL-1.1

//! `LocalAuthProvider` — username/password authentication against a local
//! user store (**Community edition**).
//!
//! This is the Community identity provider: it verifies a presented
//! credential against locally-stored users. It depends only on a
//! [`UserStore`] trait object — never on grants ([`super::AuthorizationPolicy`])
//! or the database catalogue ([`super::DatabaseCatalog`]), both Enterprise.
//! That decoupling is the whole point: an open Community server can
//! authenticate local logins without pulling in the authorization or
//! multi-tenancy machinery.
//!
//! `LocalAuthProvider` is one implementation of the [`AuthProvider`]
//! extension point. Corporate identity providers (JWT, OIDC, LDAP) are
//! additional implementations of the *same* trait and live in the Enterprise
//! crate — nothing here is rewritten to add them; they are new impls.

use std::sync::Arc;

use async_trait::async_trait;

use super::system_graph::DUMMY_HASH;
use super::{AuthError, AuthOutcome, AuthProvider, SecretString, UserStore, verify_password};

/// Authenticates username/password logins against a [`UserStore`].
///
/// Holds the store as `Arc<dyn UserStore>`, so it works with any local user
/// backend — the system-graph store today, a file-backed one tomorrow —
/// without depending on a concrete type or on the authorization surface.
pub struct LocalAuthProvider {
    store: Arc<dyn UserStore>,
}

impl LocalAuthProvider {
    /// Construct a provider reading users from `store`.
    ///
    /// Generic over the concrete store type so callers can pass an
    /// `Arc<SystemGraphAuthStore>` (or any other `UserStore`) directly; the
    /// unsize coercion to `Arc<dyn UserStore>` happens here rather than at
    /// every call site.
    #[must_use]
    pub fn from_store<S: UserStore>(store: Arc<S>) -> Self {
        Self { store }
    }

    /// Construct from an already-erased `Arc<dyn UserStore>`.
    #[must_use]
    pub fn from_dyn_store(store: Arc<dyn UserStore>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl AuthProvider for LocalAuthProvider {
    async fn authenticate(
        &self,
        principal: &str,
        credentials: &str,
    ) -> Result<AuthOutcome, AuthError> {
        let creds = SecretString::new(credentials.to_owned());

        // `get_user_for_auth` normalises the username internally and returns a
        // cloned auth projection, so the store's lock is not held across the
        // argon2 verify below (which the previous, store-coupled provider did).
        let record = self
            .store
            .get_user_for_auth(principal)
            .await
            .map_err(|e| AuthError::Backend(e.to_string()))?;

        match record {
            None => {
                // Run a dummy verify to keep response timing flat with the
                // invalid-credentials path (both take ~50-100 ms of argon2).
                // The result is discarded.
                let _ = verify_password(&creds, &DUMMY_HASH);
                Err(AuthError::UnknownUser)
            }
            Some(u) if !u.enabled => {
                // Verify against the real hash so a disabled account is
                // indistinguishable from an enabled one with wrong
                // credentials, to a network observer.
                let _ = verify_password(&creds, &u.password_hash);
                Err(AuthError::UserDisabled)
            }
            Some(u) => {
                let ok = verify_password(&creds, &u.password_hash)
                    .map_err(|e| AuthError::Backend(e.to_string()))?;
                if ok {
                    Ok(AuthOutcome {
                        user_id: u.id,
                        roles: Vec::new(),
                        is_admin: u.is_admin,
                    })
                } else {
                    Err(AuthError::InvalidCredentials)
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{AuthStoreError, UserAuthRecord, UserSummary};

    /// A minimal `UserStore` that implements ONLY the user-management surface
    /// — no grants, no catalogue. Its mere existence proves `LocalAuthProvider`
    /// needs nothing beyond `UserStore`: if the provider referenced
    /// `AuthorizationPolicy` or `DatabaseCatalog`, this test file would not
    /// compile, because `FakeUserStore` implements neither.
    struct FakeUserStore {
        record: Option<UserAuthRecord>,
    }

    #[async_trait]
    impl UserStore for FakeUserStore {
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
            Ok(self.record.clone())
        }
    }

    /// Contract: `LocalAuthProvider` authenticates against a bare `UserStore`
    /// that implements neither `AuthorizationPolicy` nor `DatabaseCatalog`.
    /// An unknown user (empty store) yields `UnknownUser`.
    #[tokio::test]
    async fn local_auth_provider_only_needs_user_store() {
        let store = Arc::new(FakeUserStore { record: None });
        let provider = LocalAuthProvider::from_store(store);
        let result = provider.authenticate("alice", "whatever").await;
        assert!(matches!(result, Err(AuthError::UnknownUser)));
    }

    /// A wrong password against a known, enabled user yields
    /// `InvalidCredentials` — confirms the verify path runs through the
    /// projection returned by `get_user_for_auth`, not a store internal.
    #[tokio::test]
    async fn wrong_password_yields_invalid_credentials() {
        let hash = crate::auth::hash_password(&SecretString::new("correct-horse".to_owned()))
            .expect("hash");
        let store = Arc::new(FakeUserStore {
            record: Some(UserAuthRecord {
                password_hash: hash,
                enabled: true,
                is_admin: false,
                id: "user-1".to_owned(),
            }),
        });
        let provider = LocalAuthProvider::from_store(store);
        let result = provider.authenticate("alice", "wrong-password").await;
        assert!(matches!(result, Err(AuthError::InvalidCredentials)));
    }
}
