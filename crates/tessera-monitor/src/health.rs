// Copyright 2026 BelowZero Security OU. All rights reserved.

//! Health check abstraction for the metrics HTTP server.
//!
//! Production code uses [`AtomicHealthFlag`] which is set by the flush task.
//! Tests use [`StaticHealth`] for deterministic assertions.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

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
    flag: AtomicBool,
}

impl AtomicHealthFlag {
    /// Create a new flag in the healthy state.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            flag: AtomicBool::new(true),
        })
    }

    /// Mark the server as healthy.
    pub fn set_healthy(&self) {
        self.flag.store(true, Ordering::Relaxed);
    }

    /// Mark the server as degraded (unhealthy).
    pub fn set_degraded(&self) {
        self.flag.store(false, Ordering::Relaxed);
    }
}

impl HealthProvider for AtomicHealthFlag {
    fn is_healthy(&self) -> bool {
        self.flag.load(Ordering::Relaxed)
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
    fn static_health_returns_fixed_state() {
        assert!(StaticHealth::new(true).is_healthy());
        assert!(!StaticHealth::new(false).is_healthy());
    }
}
