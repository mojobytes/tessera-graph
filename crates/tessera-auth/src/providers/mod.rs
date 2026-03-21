// Copyright 2026 BelowZero Security OU. All rights reserved.

//! External authentication provider abstraction.
//!
//! Supports LDAP and OIDC providers as alternatives to local Argon2id auth.

pub mod group_mapping;
pub mod ldap;
pub mod oidc;

use std::future::Future;
use std::pin::Pin;

use crate::error::Result;

/// Information about a successfully authenticated external user.
#[derive(Debug, Clone)]
pub struct ExternalUserInfo {
    /// Username as known by the external provider.
    pub username: String,
    /// Groups/roles assigned by the external provider (LDAP groups or OIDC claims).
    pub groups: Vec<String>,
    /// Email address, if available.
    pub email: Option<String>,
    /// Display name, if available.
    pub display_name: Option<String>,
}

/// Abstraction over external identity providers (LDAP, OIDC).
///
/// Implementors must be `Send + Sync` to be stored in `Arc<dyn ExternalAuthProvider>`.
///
/// The `credential` field carries a password for LDAP and a JWT for OIDC.
/// It must NEVER be logged.
pub trait ExternalAuthProvider: Send + Sync {
    /// Authenticate a user and return their identity info on success.
    ///
    /// # Errors
    ///
    /// Returns `AuthError::InvalidCredentials` on any authentication failure.
    /// Returns `AuthError::ProviderUnavailable` if the provider cannot be reached.
    fn authenticate(
        &self,
        username: &str,
        credential: &str,
    ) -> Pin<Box<dyn Future<Output = Result<ExternalUserInfo>> + Send + '_>>;

    /// Human-readable provider name for logging and configuration display.
    fn provider_name(&self) -> &'static str;
}
