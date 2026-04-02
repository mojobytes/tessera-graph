// Copyright 2026 BelowZero Security OU. All rights reserved.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use base64ct::{Base64UrlUnpadded, Encoding};
use rand::RngCore;

use crate::error::{AuthError, Result};
use crate::rbac::RoleId;
use crate::user::UserId;

/// Opaque session token. Does not implement `Debug` to prevent leaking in logs.
/// Uses constant-time comparison to prevent timing oracle attacks.
// Hash is derived from String bytes, which is consistent with our manual PartialEq
// (constant_time_eq over the same bytes). The Hash/PartialEq contract holds:
// if a == b then hash(a) == hash(b).
#[derive(Clone, Eq, Hash)]
#[allow(clippy::derived_hash_with_manual_eq)]
pub struct SessionToken(String);

impl PartialEq for SessionToken {
    fn eq(&self, other: &Self) -> bool {
        constant_time_eq::constant_time_eq(self.0.as_bytes(), other.0.as_bytes())
    }
}

impl SessionToken {
    /// View the token string (e.g. for sending to the client).
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Reconstruct from a raw string (e.g. received from the client).
    #[must_use]
    pub const fn from_raw(raw: String) -> Self {
        Self(raw)
    }
}

/// Internal session state.
struct Session {
    user_id: UserId,
    /// Monotonic deadline — immune to NTP clock adjustments (HIGH #9).
    expires_at: Instant,
    /// Roles assigned to this session, primarily for external auth users whose
    /// roles are mapped from provider groups rather than stored in `UserStore`.
    roles: Vec<RoleId>,
}

/// Thread-safe session manager with configurable TTL.
pub struct SessionManager {
    sessions: Arc<RwLock<HashMap<SessionToken, Session>>>,
    ttl_seconds: u64,
}

impl SessionManager {
    /// Create a new session manager with the given session TTL in seconds.
    #[must_use]
    pub fn new(ttl_seconds: u64) -> Self {
        Self {
            sessions: Arc::new(RwLock::new(HashMap::new())),
            ttl_seconds,
        }
    }

    /// Create a new session for the given user and return an opaque token.
    ///
    /// The token is 32 cryptographically random bytes encoded as URL-safe base64.
    ///
    /// # Errors
    ///
    /// Returns `AuthError::LockPoisoned` if the internal lock is poisoned.
    pub fn create_session(&self, user_id: UserId) -> Result<SessionToken> {
        self.create_session_with_roles(user_id, Vec::new())
    }

    /// Create a new session with explicit role assignments.
    ///
    /// Used for external auth users whose roles are derived from provider
    /// group mappings rather than from the persistent `UserStore`.
    ///
    /// # Errors
    ///
    /// Returns `AuthError::LockPoisoned` if the internal lock is poisoned.
    pub fn create_session_with_roles(
        &self,
        user_id: UserId,
        roles: Vec<RoleId>,
    ) -> Result<SessionToken> {
        let mut raw = [0u8; 32];
        rand::rng().fill_bytes(&mut raw);

        let encoded = Base64UrlUnpadded::encode_string(&raw);
        let token = SessionToken(encoded);

        let session = Session {
            user_id,
            expires_at: Instant::now() + Duration::from_secs(self.ttl_seconds),
            roles,
        };

        let mut sessions = self
            .sessions
            .write()
            .map_err(|_| AuthError::LockPoisoned("session manager"))?;
        sessions.insert(token.clone(), session);
        drop(sessions);

        Ok(token)
    }

    /// Retrieve the roles associated with a session.
    ///
    /// Returns an empty `Vec` for sessions created without explicit roles
    /// (i.e., local auth users whose roles live in `UserStore`).
    ///
    /// # Errors
    ///
    /// Returns `AuthError::TokenInvalid` if the token is unknown.
    #[allow(clippy::significant_drop_tightening)]
    pub fn session_roles(&self, token: &SessionToken) -> Result<Vec<RoleId>> {
        let sessions = self
            .sessions
            .read()
            .map_err(|_| AuthError::LockPoisoned("session manager"))?;
        let session = sessions.get(token).ok_or(AuthError::TokenInvalid)?;
        Ok(session.roles.clone())
    }

    /// Validate a session token and return the associated user ID.
    ///
    /// Uses a read-lock on the happy path so concurrent validations of live
    /// tokens do not contend. A write-lock is only acquired when the token is
    /// found to be expired and must be removed.
    ///
    /// # Errors
    ///
    /// Returns `AuthError::TokenInvalid` if the token is unknown, or
    /// `AuthError::TokenExpired` if the session has expired.
    #[allow(clippy::significant_drop_tightening)]
    pub fn validate(&self, token: &SessionToken) -> Result<UserId> {
        // --- Read-lock: extract (user_id, expires_at), then release ---
        let (user_id, expires_at) = {
            let sessions = self
                .sessions
                .read()
                .map_err(|_| AuthError::LockPoisoned("session manager"))?;
            let session = sessions.get(token).ok_or(AuthError::TokenInvalid)?;
            (session.user_id, session.expires_at)
        };

        // --- Check expiry without holding any lock ---
        if Instant::now() > expires_at {
            // Write-lock only for the removal of the expired token.
            let mut sessions = self
                .sessions
                .write()
                .map_err(|_| AuthError::LockPoisoned("session manager"))?;
            sessions.remove(token);
            return Err(AuthError::TokenExpired);
        }

        Ok(user_id)
    }

    /// Revoke a specific session.
    ///
    /// # Errors
    ///
    /// Returns `AuthError::LockPoisoned` if the internal lock is poisoned.
    pub fn revoke(&self, token: &SessionToken) -> Result<()> {
        let mut sessions = self
            .sessions
            .write()
            .map_err(|_| AuthError::LockPoisoned("session manager"))?;
        sessions.remove(token);
        drop(sessions);
        Ok(())
    }

    /// Remove all expired sessions and return the count of removed entries.
    ///
    /// Call this periodically from a background task to prevent unbounded
    /// `HashMap` growth under high connection churn (CRITICAL #4).
    ///
    /// Returns `0` if the lock is poisoned (conservative: do not crash the
    /// server on a cleanup task failure).
    #[must_use]
    pub fn purge_expired(&self) -> usize {
        let now = Instant::now();
        let Ok(mut sessions) = self.sessions.write() else {
            return 0;
        };
        let before = sessions.len();
        sessions.retain(|_, s| s.expires_at > now);
        before - sessions.len()
    }

    /// Return the current number of live sessions (for metrics/monitoring).
    ///
    /// Returns `0` if the lock is poisoned.
    #[must_use]
    pub fn session_count(&self) -> usize {
        self.sessions.read().map(|s| s.len()).unwrap_or(0)
    }

    /// Revoke all sessions for a given user (e.g. on password change or deletion).
    ///
    /// # Errors
    ///
    /// Returns `AuthError::LockPoisoned` if the internal lock is poisoned.
    pub fn revoke_all_for_user(&self, user_id: UserId) -> Result<()> {
        let mut sessions = self
            .sessions
            .write()
            .map_err(|_| AuthError::LockPoisoned("session manager"))?;
        sessions.retain(|_, s| s.user_id != user_id);
        drop(sessions);
        Ok(())
    }
}
