// Copyright 2026 BelowZero Security OU. All rights reserved.

use std::collections::HashMap;
use std::sync::Arc;

use tessera_audit::AuditLog;
use tessera_auth::lbac::Clearance;
use tessera_auth::policy::AuthPolicy;
use tessera_auth::providers::ExternalAuthProvider;
use tessera_auth::rate_limit::{LoginAttemptTracker, LoginPolicy};
use tessera_auth::rbac::Permission;
use tessera_auth::session::{SessionManager, SessionToken};
use tessera_auth::user::{UserId, UserStoreHandle};
use tessera_monitor::MetricsRegistry;
use tessera_protocol::tls::TlsConfig;
use tessera_tenant::TenantRegistry;

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
    external_provider: Option<Arc<dyn ExternalAuthProvider>>,
    group_mapping: Arc<HashMap<String, String>>,
    metrics: Arc<MetricsRegistry>,
    tenant_registry: Arc<TenantRegistry>,
    login_tracker: Arc<LoginAttemptTracker>,
    login_policy: LoginPolicy,
}

impl ServerContext {
    /// Create a new server context with local authentication only.
    ///
    /// All parameters are mandatory — there is no way to construct a context
    /// without authentication and TLS.
    #[must_use]
    pub fn new(
        auth_policy: Arc<AuthPolicy>,
        sessions: Arc<SessionManager>,
        audit: Arc<AuditLog>,
        tls: TlsConfig,
        user_store: Arc<UserStoreHandle>,
        metrics: Arc<MetricsRegistry>,
        tenant_registry: Arc<TenantRegistry>,
    ) -> Self {
        Self {
            auth_policy,
            sessions,
            audit,
            tls,
            user_store,
            external_provider: None,
            group_mapping: Arc::new(HashMap::new()),
            metrics,
            tenant_registry,
            login_tracker: Arc::new(LoginAttemptTracker::new()),
            // Default: lock after 5 failures for 300 seconds.
            login_policy: LoginPolicy::new(5, 300),
        }
    }

    /// Override the default login policy (5 failures / 300s lockout).
    #[must_use]
    pub const fn with_login_policy(mut self, policy: LoginPolicy) -> Self {
        self.login_policy = policy;
        self
    }

    /// Set an external authentication provider (LDAP or OIDC).
    ///
    /// When set, the server will use this provider instead of local auth.
    #[must_use]
    pub fn with_external_provider(
        mut self,
        provider: Arc<dyn ExternalAuthProvider>,
        group_mapping: Arc<HashMap<String, String>>,
    ) -> Self {
        self.external_provider = Some(provider);
        self.group_mapping = group_mapping;
        self
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

    /// Access the external authentication provider, if configured.
    #[must_use]
    pub fn external_provider(&self) -> Option<&Arc<dyn ExternalAuthProvider>> {
        self.external_provider.as_ref()
    }

    /// Access the external group-to-role mapping.
    #[must_use]
    pub const fn group_mapping(&self) -> &Arc<HashMap<String, String>> {
        &self.group_mapping
    }

    /// Access the metrics registry.
    #[must_use]
    pub const fn metrics(&self) -> &Arc<MetricsRegistry> {
        &self.metrics
    }

    /// Access the tenant registry.
    #[must_use]
    pub const fn tenant_registry(&self) -> &Arc<TenantRegistry> {
        &self.tenant_registry
    }

    /// Access the login attempt tracker.
    #[must_use]
    pub const fn login_tracker(&self) -> &Arc<LoginAttemptTracker> {
        &self.login_tracker
    }

    /// Access the login policy.
    #[must_use]
    pub const fn login_policy(&self) -> &LoginPolicy {
        &self.login_policy
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

    /// Resolve the LBAC `Clearance` for the session identified by `token`.
    ///
    /// Steps: (1) validate token → `UserId`, (2) look up clearance for that user.
    ///
    /// # Errors
    ///
    /// Returns `AuthError` if the token is invalid, expired, or the user cannot
    /// be found. **Fail-safe**: any error results in denial, never in granting access.
    pub fn resolve_clearance(&self, token: &SessionToken) -> tessera_auth::Result<Clearance> {
        let user_id = self.sessions.validate(token)?;
        self.user_store.get_clearance(user_id)
    }
}
