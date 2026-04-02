// Copyright 2026 BelowZero Security OU. All rights reserved.

//! Health check abstraction for the metrics HTTP server.
//!
//! Production code uses [`AtomicHealthFlag`] which is set by the flush task.
//! Tests use [`StaticHealth`] for deterministic assertions.

use std::sync::atomic::{AtomicBool, Ordering};

/// Reports whether the server is healthy.
///
/// Implementors must be `Send + Sync` so the health state can be shared
/// across the metrics HTTP server and the background flush task.
pub trait HealthProvider: Send + Sync {
    /// Returns `true` if the server is considered healthy.
    fn is_healthy(&self) -> bool;
}

/// Atomic flag toggled by background tasks (flush, WAL) to signal health.
///
/// Defaults to healthy (`true`). The flush task sets it to `false` after
/// consecutive errors and resets it on success.
pub struct AtomicHealthFlag {
    /// Flush health — set to `false` after consecutive flush errors.
    flag: AtomicBool,
    /// Disk space health — `true` when free space is below threshold.
    disk_degraded: AtomicBool,
}

impl AtomicHealthFlag {
    /// Create a new flag in the healthy state.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            flag: AtomicBool::new(true),
            disk_degraded: AtomicBool::new(false),
        }
    }

    /// Mark flush health as healthy.
    pub fn set_healthy(&self) {
        self.flag.store(true, Ordering::Relaxed);
    }

    /// Mark flush health as degraded (unhealthy).
    pub fn set_degraded(&self) {
        self.flag.store(false, Ordering::Relaxed);
    }

    /// Mark disk space as degraded (below threshold).
    pub fn set_disk_degraded(&self) {
        self.disk_degraded.store(true, Ordering::Relaxed);
    }

    /// Clear disk space degradation (space recovered above threshold).
    pub fn clear_disk_degraded(&self) {
        self.disk_degraded.store(false, Ordering::Relaxed);
    }
}

impl Default for AtomicHealthFlag {
    fn default() -> Self {
        Self::new()
    }
}

impl HealthProvider for AtomicHealthFlag {
    fn is_healthy(&self) -> bool {
        self.flag.load(Ordering::Relaxed) && !self.disk_degraded.load(Ordering::Relaxed)
    }
}

/// Fixed health state for tests.
pub struct StaticHealth(bool);

impl StaticHealth {
    /// Create a static health provider with the given state.
    #[must_use]
    pub const fn new(healthy: bool) -> Self {
        Self(healthy)
    }
}

impl HealthProvider for StaticHealth {
    fn is_healthy(&self) -> bool {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_flag_defaults_to_healthy() {
        let flag = AtomicHealthFlag::new();
        assert!(flag.is_healthy());
    }

    #[test]
    fn atomic_flag_can_be_set_degraded_and_recovered() {
        let flag = AtomicHealthFlag::new();
        flag.set_degraded();
        assert!(!flag.is_healthy());
        flag.set_healthy();
        assert!(flag.is_healthy());
    }

    #[test]
    fn disk_degraded_not_overridden_by_flush_success() {
        let flag = AtomicHealthFlag::new();
        flag.set_disk_degraded();
        flag.set_healthy(); // flush success must NOT clear disk degradation
        assert!(!flag.is_healthy(), "disk degradation must persist after flush success");
    }

    #[test]
    fn disk_degraded_clears_when_space_recovers() {
        let flag = AtomicHealthFlag::new();
        flag.set_disk_degraded();
        assert!(!flag.is_healthy());
        flag.clear_disk_degraded();
        assert!(flag.is_healthy());
    }

    #[test]
    fn flush_errors_degrade_independent_of_disk() {
        let flag = AtomicHealthFlag::new();
        flag.set_degraded(); // flush failure
        assert!(!flag.is_healthy());
        flag.clear_disk_degraded(); // no-op — disk was never degraded
        assert!(!flag.is_healthy(), "flush degradation must persist");
    }

    #[test]
    fn both_degraded_both_must_clear_for_healthy() {
        let flag = AtomicHealthFlag::new();
        flag.set_degraded();
        flag.set_disk_degraded();
        assert!(!flag.is_healthy());
        flag.set_healthy(); // clear flush
        assert!(!flag.is_healthy(), "disk still degraded");
        flag.clear_disk_degraded();
        assert!(flag.is_healthy(), "both cleared — healthy");
    }

    #[test]
    fn static_health_returns_fixed_state() {
        assert!(StaticHealth::new(true).is_healthy());
        assert!(!StaticHealth::new(false).is_healthy());
    }
}
