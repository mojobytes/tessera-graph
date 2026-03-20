// Copyright 2026 BelowZero Security OU. All rights reserved.

use std::sync::Arc;

use tessera_audit::AuditLog;
use tessera_auth::policy::AuthPolicy;
use tessera_auth::rbac::Permission;
use tessera_auth::session::{SessionManager, SessionToken};
use tessera_auth::user::{UserId, UserStoreHandle};
use tessera_protocol::tls::TlsConfig;

/// Server context that holds all security components.
///
/// This type cannot be constructed without an `AuthPolicy` and `TlsConfig`,
/// ensuring the server is secure by default at the type-system level.
pub struct ServerContext {
    auth_policy: Arc<AuthPolicy>,
    sessions: Arc<SessionManager>,
    audit: Arc<AuditLog>,
    tls: TlsConfig,
    user_store: Arc<UserStoreHandle>,
}

impl ServerContext {
    /// Create a new server context. All parameters are mandatory — there is
    /// no way to construct a context without authentication and TLS.
    #[must_use]
    pub const fn new(
        auth_policy: Arc<AuthPolicy>,
        sessions: Arc<SessionManager>,
        audit: Arc<AuditLog>,
        tls: TlsConfig,
        user_store: Arc<UserStoreHandle>,
    ) -> Self {
        Self {
            auth_policy,
            sessions,
            audit,
            tls,
            user_store,
        }
    }

    /// Access the TLS configuration.
    #[must_use]
    pub const fn tls_config(&self) -> &TlsConfig {
        &self.tls
    }

    /// Access the authentication policy.
    #[must_use]
    pub const fn auth_policy(&self) -> &Arc<AuthPolicy> {
        &self.auth_policy
    }

    /// Access the session manager.
    #[must_use]
    pub const fn sessions(&self) -> &Arc<SessionManager> {
        &self.sessions
    }

    /// Access the audit log.
    #[must_use]
    pub const fn audit(&self) -> &Arc<AuditLog> {
        &self.audit
    }

    /// Access the user store.
    #[must_use]
    pub const fn user_store(&self) -> &Arc<UserStoreHandle> {
        &self.user_store
    }

    /// Validate a session token and check the required permission.
    ///
    /// On success, records a success audit entry and returns the authenticated
    /// user ID. On failure, records a denied audit entry.
    ///
    /// # Errors
    ///
    /// Returns `AuthError` on authentication or authorization failure.
    pub fn check_permission(
        &self,
        token: &SessionToken,
        required: Permission,
    ) -> tessera_auth::Result<UserId> {
        match self
            .auth_policy
            .check_session(token, required, &self.sessions)
        {
            Ok(user_id) => {
                let _ = self
                    .audit
                    .record_success(Some(user_id.raw()), &required.to_string(), None);
                Ok(user_id)
            }
            Err(e) => {
                let is_authz_error = matches!(
                    e,
                    tessera_auth::AuthError::PermissionDenied { .. }
                        | tessera_auth::AuthError::TokenInvalid
                        | tessera_auth::AuthError::TokenExpired
                );
                if is_authz_error {
                    let _ =
                        self.audit
                            .record_denied(None, &required.to_string(), None, &e.to_string());
                } else {
                    let _ =
                        self.audit
                            .record_error(None, &required.to_string(), None, &e.to_string());
                }
                Err(e)
            }
        }
    }
}
