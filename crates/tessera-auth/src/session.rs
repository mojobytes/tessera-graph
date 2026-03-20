// Copyright 2026 BelowZero Security OU. All rights reserved.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use base64ct::{Base64UrlUnpadded, Encoding};
use rand::RngCore;

use crate::error::{AuthError, Result};
use crate::user::UserId;
use crate::utils::unix_timestamp;

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
    expires_at: u64,
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
        let mut raw = [0u8; 32];
        rand::rng().fill_bytes(&mut raw);

        let encoded = Base64UrlUnpadded::encode_string(&raw);
        let token = SessionToken(encoded);

        let now = unix_timestamp();
        let session = Session {
            user_id,
            expires_at: now + self.ttl_seconds,
        };

        let mut sessions = self
            .sessions
            .write()
            .map_err(|_| AuthError::LockPoisoned("session manager"))?;
        sessions.insert(token.clone(), session);
        drop(sessions);

        Ok(token)
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
        if unix_timestamp() > expires_at {
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
