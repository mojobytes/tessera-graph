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
#[derive(Default)]
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
    /// If the internal lock is poisoned (another thread panicked while holding
    /// it), the failure is silently dropped. This is fail-safe: the worst case
    /// is that one failed attempt goes unrecorded.
    pub fn record_failure(&self, username: &str) {
        let Ok(mut map) = self.attempts.lock() else {
            return;
        };
        let entry = map
            .entry(username.to_owned())
            .or_insert_with(|| (0, Instant::now()));
        entry.0 += 1;
        entry.1 = Instant::now();
    }

    /// Reset the failure counter after a successful login.
    ///
    /// If the internal lock is poisoned, the reset is silently skipped.
    /// This is fail-safe: the worst case is that the counter stays elevated,
    /// which may cause an earlier lockout — not a bypass.
    pub fn record_success(&self, username: &str) {
        let Ok(mut map) = self.attempts.lock() else {
            return;
        };
        map.remove(username);
    }

    /// Check if the account is currently locked due to too many failed attempts.
    ///
    /// If the internal lock is poisoned, returns `true` (locked) — fail-safe
    /// default that denies access rather than allowing it.
    #[must_use]
    pub fn is_locked(&self, username: &str, policy: &LoginPolicy) -> bool {
        let Ok(map) = self.attempts.lock() else {
            // Lock poisoned — fail-safe: treat as locked.
            return true;
        };
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
