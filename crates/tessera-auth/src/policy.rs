// Copyright 2026 BelowZero Security OU. All rights reserved.

use std::sync::Arc;

use crate::error::{AuthError, Result};
use crate::rbac::{Permission, RoleStoreHandle};
use crate::session::{SessionManager, SessionToken};
use crate::user::{UserId, UserStoreHandle};

/// Central authorization policy. This is the single point where permit/deny
/// decisions are made. Every query path must go through `AuthPolicy::check`
/// or `AuthPolicy::check_session`.
pub struct AuthPolicy {
    user_store: Arc<UserStoreHandle>,
    role_store: RoleStoreHandle,
}

impl AuthPolicy {
    /// Create a new policy backed by the given user and role stores.
    #[must_use]
    pub const fn new(user_store: Arc<UserStoreHandle>, role_store: RoleStoreHandle) -> Self {
        Self {
            user_store,
            role_store,
        }
    }

    /// Check whether a user has the required permission.
    ///
    /// **Fail-safe**: if the user is unknown or the lock is poisoned, access is denied.
    ///
    /// # Errors
    ///
    /// Returns `AuthError::PermissionDenied` if the user lacks the permission.
    pub fn check(&self, user_id: UserId, required: Permission) -> Result<()> {
        // Look up the user's roles — if user is unknown, deny.
        let role_ids = self.user_store.get_user_roles(user_id).unwrap_or_default();

        // Collect the union of all permissions from all roles — fail-safe on lock poison.
        let permissions = self.role_store.collect_permissions(&role_ids);

        if permissions.contains(&required) {
            Ok(())
        } else {
            Err(AuthError::PermissionDenied { required })
        }
    }

    /// Validate a session token and check the required permission in one step.
    ///
    /// Returns the authenticated `UserId` on success (useful for audit logging).
    ///
    /// # Errors
    ///
    /// Returns token errors or `AuthError::PermissionDenied`.
    pub fn check_session(
        &self,
        token: &SessionToken,
        required: Permission,
        sessions: &SessionManager,
    ) -> Result<UserId> {
        let user_id = sessions.validate(token)?;
        self.check(user_id, required)?;
        Ok(user_id)
    }
}
