// Copyright 2026 BelowZero Security OU. All rights reserved.

//! Background flush task that periodically persists dirty graph pages.
//!
//! WAL guarantees durability for every mutation. This task amortises the
//! cost of page-file flush across many mutations instead of paying it
//! per-operation.

use std::sync::Arc;
use tessera_monitor::AtomicHealthFlag;
use tessera_tenant::TenantRegistry;
use tokio::sync::watch;
use tokio::task::JoinHandle;

/// Maximum consecutive flush errors before the health flag is set to degraded.
const MAX_CONSECUTIVE_ERRORS: u32 = 3;

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
) -> JoinHandle<()> {
    if interval_ms == 0 {
        return tokio::spawn(async {});
    }

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

                    if failed {
                        consecutive_errors = consecutive_errors.saturating_add(1);
                        if consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                            health.set_degraded();
                        }
                    } else {
                        consecutive_errors = 0;
                        health.set_healthy();
                    }
                }
                _ = shutdown_rx.changed() => {
                    break;
                }
            }
        }
    })
}
