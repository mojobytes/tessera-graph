// Copyright 2026 BelowZero Security OU. All rights reserved.

//! Graceful shutdown helpers.

use std::sync::{Arc, RwLock};
use tessera_graph::Graph;

/// Flush the graph to disk as part of graceful shutdown.
///
/// Acquires the write lock, calls `Graph::flush`, and logs the outcome.
/// On lock poisoning, logs an error and does not panic — the process is
/// exiting anyway and panicking would suppress any pending destructors.
pub fn flush_on_shutdown(graph: &Arc<RwLock<Graph>>) {
    match graph.write() {
        Ok(mut g) => {
            if let Err(e) = g.flush() {
                tracing::error!("shutdown flush failed: {e}");
            } else {
                tracing::info!("graph flushed to disk on shutdown");
            }
        }
        Err(e) => {
            tracing::error!("graph lock poisoned at shutdown, flush skipped: {e}");
        }
    }
}
