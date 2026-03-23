// Copyright 2026 BelowZero Security OU. All rights reserved.

use std::sync::{Arc, RwLock};
use tessera_graph::{Graph, GraphConfig};
use tessera_server::shutdown::flush_on_shutdown;

#[allow(clippy::significant_drop_tightening)]
#[test]
fn flush_on_shutdown_persists_data() {
    let dir = tempfile::tempdir().unwrap();
    let config = GraphConfig::new();

    let graph = Arc::new(RwLock::new(
        Graph::open(dir.path(), &config).unwrap(),
    ));

    // Write a node directly — simulates data mutated before shutdown.
    {
        let mut g = graph.write().unwrap();
        g.add_node("ShutdownTest", tessera_graph::props! { "marker" => "yes" })
            .unwrap();
    }

    // Shutdown flush.
    flush_on_shutdown(&graph);
    drop(graph);

    // Reopen and verify.
    let reopened = Graph::open(dir.path(), &config).unwrap();
    assert_eq!(
        reopened.node_count(),
        1,
        "expected 1 node after reopen, flush_on_shutdown did not persist data"
    );
}

#[test]
fn flush_on_shutdown_is_noop_on_in_memory_graph() {
    let graph = Arc::new(RwLock::new(Graph::new()));
    flush_on_shutdown(&graph); // must not panic
}
