// Copyright 2026 BelowZero Security OU. All rights reserved.

//! External authentication dispatch layer.
//!
//! Bridges external identity providers (LDAP, OIDC) with the internal session
//! and RBAC system. Responsible for synthesizing transient `UserId`s and mapping
//! `ProviderUnavailable` errors to `InvalidCredentials` before they reach the wire.

use std::collections::HashMap;
use std::sync::Arc;

use tessera_auth::error::AuthError;
use tessera_auth::providers::group_mapping::map_groups;
use tessera_auth::providers::ExternalAuthProvider;
use tessera_auth::session::{SessionManager, SessionToken};
use tessera_auth::user::UserId;

/// Authenticate via an external provider and create an internal session.
///
/// On success, returns a transient `UserId` (FNV-1a hash of username) and a
/// `SessionToken`. On failure, all errors are mapped to `InvalidCredentials`
/// before reaching the wire — `ProviderUnavailable` is logged but never exposed.
///
/// # Errors
///
/// Returns `AuthError::InvalidCredentials` on any failure.
pub async fn authenticate_external<S: std::hash::BuildHasher + Send + Sync>(
    username: &str,
    credential: &str,
    provider: &Arc<dyn ExternalAuthProvider>,
    group_mapping: &HashMap<String, String, S>,
    sessions: &Arc<SessionManager>,
) -> Result<(UserId, SessionToken), AuthError> {
    let info = match provider.authenticate(username, credential).await {
        Ok(info) => info,
        Err(AuthError::ProviderUnavailable(detail)) => {
            tracing::warn!("external auth provider unavailable: {detail}");
            return Err(AuthError::InvalidCredentials);
        }
        Err(_) => return Err(AuthError::InvalidCredentials),
    };

    // Synthesize a transient UserId from the username
    let user_id = UserId::new(hash_username(&info.username));

    // Map external groups to internal roles (for RBAC)
    let roles = map_groups(&info.groups, group_mapping);

    // Create a session with the mapped roles attached — these are not stored
    // in `UserStore` (the user is transient) but live in the session itself.
    let token = sessions
        .create_session_with_roles(user_id, roles)
        .map_err(|_| AuthError::InvalidCredentials)?;

    Ok((user_id, token))
}

/// Stable 64-bit FNV-1a hash of a username string.
///
/// Used to synthesize a transient `UserId` for external users without
/// modifying the persistent `UserStore`.
///
/// # Security
///
/// FNV-1a is **not** collision-resistant. Birthday collisions become probable
/// at ~2^32 (~4 billion) distinct users. A collision would cause two external
/// users to share the same `UserId` and therefore the same session visibility.
/// For deployments expecting >10^6 concurrent external users, replace this
/// with a cryptographic hash (e.g., BLAKE3 truncated to 64 bits).
fn hash_username(username: &str) -> u64 {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0100_0000_01b3;

    let mut hash = FNV_OFFSET;
    for byte in username.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;
    use tessera_auth::providers::ExternalUserInfo;

    struct AlwaysOkProvider;

    impl ExternalAuthProvider for AlwaysOkProvider {
        fn authenticate(
            &self,
            username: &str,
            _cred: &str,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = tessera_auth::Result<ExternalUserInfo>> + Send + '_>,
        > {
            let username = username.to_owned();
            Box::pin(async move {
                Ok(ExternalUserInfo {
                    username,
                    groups: vec!["admin".to_owned()],
                    email: None,
                    display_name: None,
                })
            })
        }
        fn provider_name(&self) -> &'static str {
            "mock-ok"
        }
    }

    struct AlwaysFailProvider;

    impl ExternalAuthProvider for AlwaysFailProvider {
        fn authenticate(
            &self,
            _username: &str,
            _cred: &str,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = tessera_auth::Result<ExternalUserInfo>> + Send + '_>,
        > {
            Box::pin(async { Err(AuthError::InvalidCredentials) })
        }
        fn provider_name(&self) -> &'static str {
            "mock-fail"
        }
    }

    struct UnavailableProvider;

    impl ExternalAuthProvider for UnavailableProvider {
        fn authenticate(
            &self,
            _username: &str,
            _cred: &str,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = tessera_auth::Result<ExternalUserInfo>> + Send + '_>,
        > {
            Box::pin(async {
                Err(AuthError::ProviderUnavailable("timeout".to_owned()))
            })
        }
        fn provider_name(&self) -> &'static str {
            "mock-unavailable"
        }
    }

    #[tokio::test]
    async fn external_auth_success_creates_session() {
        let provider: Arc<dyn ExternalAuthProvider> = Arc::new(AlwaysOkProvider);
        let sessions = Arc::new(SessionManager::new(3600));
        let mapping = HashMap::new();

        let (user_id, token) =
            authenticate_external("alice", "any-cred", &provider, &mapping, &sessions)
                .await
                .expect("auth ok"); // OK: test

        let validated = sessions.validate(&token).expect("valid session"); // OK: test
        assert!(validated == user_id);
    }

    #[tokio::test]
    async fn external_auth_stores_mapped_roles_in_session() {
        let provider: Arc<dyn ExternalAuthProvider> = Arc::new(AlwaysOkProvider);
        let sessions = Arc::new(SessionManager::new(3600));

        // Map "admin" group to "admin" role
        let mut mapping = HashMap::new();
        mapping.insert("admin".to_owned(), "admin".to_owned());

        let (_user_id, token) =
            authenticate_external("alice", "any-cred", &provider, &mapping, &sessions)
                .await
                .expect("auth ok"); // OK: test

        let roles = sessions.session_roles(&token).expect("roles"); // OK: test
        assert!(!roles.is_empty(), "external user roles must be stored in session");
    }

    #[tokio::test]
    async fn external_auth_without_mapping_has_no_roles() {
        let provider: Arc<dyn ExternalAuthProvider> = Arc::new(AlwaysOkProvider);
        let sessions = Arc::new(SessionManager::new(3600));
        let mapping = HashMap::new(); // no group→role mapping

        let (_user_id, token) =
            authenticate_external("alice", "any-cred", &provider, &mapping, &sessions)
                .await
                .expect("auth ok"); // OK: test

        let roles = sessions.session_roles(&token).expect("roles"); // OK: test
        assert!(roles.is_empty(), "no mapping means no roles");
    }

    #[tokio::test]
    async fn external_auth_failure_returns_invalid_credentials() {
        let provider: Arc<dyn ExternalAuthProvider> = Arc::new(AlwaysFailProvider);
        let sessions = Arc::new(SessionManager::new(3600));
        let mapping = HashMap::new();

        let err = authenticate_external("alice", "wrong", &provider, &mapping, &sessions)
            .await
            .err()
            .expect("should fail"); // OK: test
        assert!(matches!(err, AuthError::InvalidCredentials));
    }

    #[tokio::test]
    async fn provider_unavailable_mapped_to_invalid_credentials() {
        let provider: Arc<dyn ExternalAuthProvider> = Arc::new(UnavailableProvider);
        let sessions = Arc::new(SessionManager::new(3600));
        let mapping = HashMap::new();

        let err = authenticate_external("alice", "any", &provider, &mapping, &sessions)
            .await
            .err()
            .expect("should fail"); // OK: test
        assert!(matches!(err, AuthError::InvalidCredentials));
    }

    #[test]
    fn hash_username_is_deterministic() {
        let h1 = hash_username("alice");
        let h2 = hash_username("alice");
        assert_eq!(h1, h2);
    }

    #[test]
    fn hash_username_different_for_different_users() {
        let h1 = hash_username("alice");
        let h2 = hash_username("bob");
        assert_ne!(h1, h2);
    }
}
