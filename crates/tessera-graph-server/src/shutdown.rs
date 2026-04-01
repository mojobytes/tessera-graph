// Copyright 2026 BelowZero Security OU. All rights reserved.

//! Graceful shutdown helpers.

use tessera_graph_tenant::TenantRegistry;

/// Flush all loaded databases to disk as part of graceful shutdown.
///
/// Iterates every graph in the registry and calls `flush`. All databases are
/// attempted even if some fail — partial failures are logged but never panic.
pub fn flush_all_on_shutdown(registry: &TenantRegistry) {
    match registry.flush_all() {
        Ok(errors) if errors.is_empty() => {
            tracing::info!("all databases flushed to disk on shutdown");
        }
        Ok(errors) => {
            for (addr, err) in &errors {
                tracing::error!("shutdown flush failed for {addr}: {err}");
            }
        }
        Err(e) => {
            tracing::error!("cannot enumerate graphs during shutdown: {e}");
        }
    }
}
