// Copyright 2026 BelowZero Security OU. All rights reserved.

//! Background flush task that periodically persists dirty graph pages.
//!
//! WAL guarantees durability for every mutation. This task amortises the
//! cost of page-file flush across many mutations instead of paying it
//! per-operation.

use std::path::PathBuf;
use std::sync::Arc;
use tessera_graph_monitor::AtomicHealthFlag;
use tessera_graph_tenant::TenantRegistry;
use tokio::sync::watch;
use tokio::task::JoinHandle;

/// Maximum consecutive flush errors before the health flag is set to degraded.
const MAX_CONSECUTIVE_ERRORS: u32 = 3;

/// Default minimum free disk space (100 MB) before marking the server degraded.
/// Override with `TESSERA_MIN_FREE_DISK_MB`.
pub const MIN_FREE_DISK_BYTES: u64 = 100 * 1024 * 1024;

/// Return available bytes on the filesystem containing `path`, or `None`
/// if the check fails (non-existent path, permission denied, etc.).
#[must_use]
pub fn check_available_disk_bytes(path: &std::path::Path) -> Option<u64> {
    fs2::available_space(path).ok()
}

/// Updates health state based on flush outcome. Returns the new consecutive error count.
fn update_health_state(
    failed: bool,
    consecutive_errors: u32,
    health: &AtomicHealthFlag,
) -> u32 {
    if failed {
        let new_count = consecutive_errors.saturating_add(1);
        if new_count >= MAX_CONSECUTIVE_ERRORS {
            health.set_degraded();
        }
        new_count
    } else {
        health.set_healthy();
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tessera_graph_monitor::HealthProvider;

    #[test]
    fn health_stays_healthy_on_success() {
        let flag = AtomicHealthFlag::new();
        let count = update_health_state(false, 0, &flag);
        assert_eq!(count, 0);
        assert!(flag.is_healthy());
    }

    #[test]
    fn health_stays_healthy_under_threshold() {
        let flag = AtomicHealthFlag::new();
        let count = update_health_state(true, 0, &flag);
        assert_eq!(count, 1);
        assert!(flag.is_healthy()); // not yet at threshold (3)

        let count = update_health_state(true, count, &flag);
        assert_eq!(count, 2);
        assert!(flag.is_healthy()); // still under threshold
    }

    #[test]
    fn health_degrades_at_threshold() {
        let flag = AtomicHealthFlag::new();
        let mut count = 0;
        for _ in 0..MAX_CONSECUTIVE_ERRORS {
            count = update_health_state(true, count, &flag);
        }
        assert_eq!(count, MAX_CONSECUTIVE_ERRORS);
        assert!(!flag.is_healthy(), "must be degraded after {MAX_CONSECUTIVE_ERRORS} errors");
    }

    #[test]
    fn disk_space_threshold_constant_is_positive() {
        assert!(
            MIN_FREE_DISK_BYTES > 0,
            "threshold must be positive"
        );
    }

    #[test]
    fn available_space_check_does_not_panic_on_nonexistent_path() {
        let result = check_available_disk_bytes(std::path::Path::new("/nonexistent/path/xyz"));
        // Must not panic. Result may be None.
        let _ = result;
    }

    #[test]
    fn available_space_returns_some_for_existing_path() {
        let result = check_available_disk_bytes(std::path::Path::new("/tmp"));
        assert!(result.is_some(), "expected Some for /tmp");
        assert!(
            result.expect("already checked is_some") > 0, // OK: test
            "expected positive free space on /tmp"
        );
    }

    #[test]
    fn health_recovers_after_success() {
        let flag = AtomicHealthFlag::new();
        // Drive to degraded
        let mut count = 0;
        for _ in 0..MAX_CONSECUTIVE_ERRORS {
            count = update_health_state(true, count, &flag);
        }
        assert!(!flag.is_healthy());

        // One success recovers
        let count = update_health_state(false, count, &flag);
        assert_eq!(count, 0);
        assert!(flag.is_healthy(), "must recover after successful flush");
    }
}

/// Spawns a background tokio task that calls
/// [`TenantRegistry::flush_all`] every `interval_ms` milliseconds.
///
/// If `interval_ms` is `0`, returns a no-op task that completes
/// immediately (sync-mode: caller is responsible for per-mutation flush).
///
/// The task exits cleanly when `shutdown_rx` receives `true`.
///
/// The `health` flag is set to degraded after [`MAX_CONSECUTIVE_ERRORS`]
/// flush failures and reset to healthy on the next successful flush.
pub fn spawn_background_flush(
    registry: Arc<TenantRegistry>,
    interval_ms: u64,
    mut shutdown_rx: watch::Receiver<bool>,
    health: Arc<AtomicHealthFlag>,
    base_dir: PathBuf,
) -> JoinHandle<()> {
    if interval_ms == 0 {
        return tokio::spawn(async {});
    }

    let min_free = std::env::var("TESSERA_MIN_FREE_DISK_MB")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map_or(MIN_FREE_DISK_BYTES, |mb| mb.saturating_mul(1024 * 1024));

    let period = std::time::Duration::from_millis(interval_ms);

    tokio::spawn(async move {
        let mut interval = tokio::time::interval(period);
        let mut consecutive_errors: u32 = 0;
        // First tick completes immediately — skip it.
        interval.tick().await;

        loop {
            tokio::select! {
                _ = interval.tick() => {
                    let failed = match registry.flush_all() {
                        Ok(errors) if !errors.is_empty() => {
                            for (addr, err) in &errors {
                                tracing::warn!(
                                    tenant = %addr.tenant,
                                    database = %addr.database,
                                    error = %err,
                                    "background flush failed for graph",
                                );
                            }
                            true
                        }
                        Err(err) => {
                            tracing::error!(error = %err, "background flush_all failed");
                            true
                        }
                        _ => false,
                    };

                    // Disk space check (HIGH #7).
                    if let Some(available) = check_available_disk_bytes(&base_dir) {
                        if available < min_free {
                            tracing::warn!(
                                available_mb = available / (1024 * 1024),
                                threshold_mb = min_free / (1024 * 1024),
                                "low disk space — marking server degraded",
                            );
                            health.set_degraded();
                        }
                    }

                    consecutive_errors = update_health_state(
                        failed,
                        consecutive_errors,
                        &health,
                    );
                }
                _ = shutdown_rx.changed() => {
                    break;
                }
            }
        }
    })
}
