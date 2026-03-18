// Copyright 2026 BelowZero Security OU. All rights reserved.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

/// Tracks failed login attempts per username.
///
/// All state is held in-process memory and is **not** persisted across restarts.
/// A process crash, clean shutdown, or horizontal scaling event will reset all
/// counters, so an attacker who can trigger a restart gains a clean slate.
///
/// # Limitations
///
/// - **In-memory only**: counters are lost on every process restart. Persistent
///   brute-force protection (e.g. backed by Redis or a database) is out of scope
///   for this crate.
/// - **Single-process only**: in a multi-replica deployment each replica maintains
///   its own independent counter. Distribute authentication requests through a
///   sticky load balancer, or replace this tracker with a shared-state backend,
///   if cross-replica lockout is required.
/// - **No persistence on panic**: if the process panics the attempt log is lost,
///   which can be exploited to reset lockouts.
pub struct LoginAttemptTracker {
    attempts: Mutex<HashMap<String, (u32, Instant)>>,
}

impl LoginAttemptTracker {
    /// Create a new tracker with no recorded attempts.
    #[must_use]
    pub fn new() -> Self {
        Self {
            attempts: Mutex::new(HashMap::new()),
        }
    }

    /// Record a failed authentication attempt for the given username.
    ///
    /// # Panics
    ///
    /// Panics if the internal lock is poisoned.
    pub fn record_failure(&self, username: &str) {
        let mut map = self.attempts.lock().expect("tracker lock poisoned");
        let entry = map
            .entry(username.to_owned())
            .or_insert_with(|| (0, Instant::now()));
        entry.0 += 1;
        entry.1 = Instant::now();
        drop(map);
    }

    /// Reset the failure counter after a successful login.
    ///
    /// # Panics
    ///
    /// Panics if the internal lock is poisoned.
    pub fn record_success(&self, username: &str) {
        let mut map = self.attempts.lock().expect("tracker lock poisoned");
        map.remove(username);
    }

    /// Check if the account is currently locked due to too many failed attempts.
    ///
    /// # Panics
    ///
    /// Panics if the internal lock is poisoned.
    #[must_use]
    pub fn is_locked(&self, username: &str, policy: &LoginPolicy) -> bool {
        let map = self.attempts.lock().expect("tracker lock poisoned");
        match map.get(username) {
            Some(&(count, last_attempt)) => {
                if count < policy.max_attempts {
                    return false;
                }
                last_attempt.elapsed().as_secs() < policy.lockout_duration_secs
            }
            None => false,
        }
    }
}

impl Default for LoginAttemptTracker {
    fn default() -> Self {
        Self::new()
    }
}

/// Configuration for brute-force protection.
pub struct LoginPolicy {
    /// Number of failed attempts before lockout.
    pub max_attempts: u32,
    /// Duration of lockout in seconds.
    pub lockout_duration_secs: u64,
}

impl LoginPolicy {
    /// Create a new login policy.
    #[must_use]
    pub const fn new(max_attempts: u32, lockout_duration_secs: u64) -> Self {
        Self {
            max_attempts,
            lockout_duration_secs,
        }
    }
}
