// Copyright 2026 BelowZero Security OU. All rights reserved.

//! Background flush task that periodically persists dirty graph pages.
//!
//! WAL guarantees durability for every mutation. This task amortises the
//! cost of page-file flush across many mutations instead of paying it
//! per-operation.

use std::sync::Arc;
use tessera_tenant::TenantRegistry;
use tokio::sync::watch;
use tokio::task::JoinHandle;

/// Spawns a background tokio task that calls
/// [`TenantRegistry::flush_all`] every `interval_ms` milliseconds.
///
/// If `interval_ms` is `0`, returns a no-op task that completes
/// immediately (sync-mode: caller is responsible for per-mutation flush).
///
/// The task exits cleanly when `shutdown_rx` receives `true`.
pub fn spawn_background_flush(
    registry: Arc<TenantRegistry>,
    interval_ms: u64,
    mut shutdown_rx: watch::Receiver<bool>,
) -> JoinHandle<()> {
    if interval_ms == 0 {
        return tokio::spawn(async {});
    }

    let period = std::time::Duration::from_millis(interval_ms);

    tokio::spawn(async move {
        let mut interval = tokio::time::interval(period);
        // First tick completes immediately — skip it.
        interval.tick().await;

        loop {
            tokio::select! {
                _ = interval.tick() => {
                    match registry.flush_all() {
                        Ok(errors) if !errors.is_empty() => {
                            for (addr, err) in &errors {
                                tracing::warn!(
                                    tenant = %addr.tenant,
                                    database = %addr.database,
                                    error = %err,
                                    "background flush failed for graph",
                                );
                            }
                        }
                        Err(err) => {
                            tracing::error!(error = %err, "background flush_all failed");
                        }
                        _ => {}
                    }
                }
                _ = shutdown_rx.changed() => {
                    break;
                }
            }
        }
    })
}
