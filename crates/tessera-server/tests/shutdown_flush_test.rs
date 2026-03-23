// Copyright 2026 BelowZero Security OU. All rights reserved.

use std::sync::Arc;

use tessera_graph::GraphConfig;
use tessera_server::shutdown::flush_all_on_shutdown;
use tessera_tenant::{DatabaseAddress, DatabaseName, TenantId, TenantRegistry};

#[allow(clippy::significant_drop_tightening)]
#[test]
fn flush_all_on_shutdown_persists_data() {
    let dir = tempfile::tempdir().unwrap();
    let config = GraphConfig::new();

    let registry = TenantRegistry::new(dir.path(), config);

    let addr = DatabaseAddress {
        tenant: TenantId::new("test").unwrap(),
        database: DatabaseName::new("main").unwrap(),
    };

    // Load a graph and write a node.
    let graph_arc = registry.get_or_load(&addr).unwrap();
    {
        let mut g = graph_arc.write().unwrap();
        g.add_node("ShutdownTest", tessera_graph::props! { "marker" => "yes" })
            .unwrap();
        drop(g);
    }

    // Flush all via the registry.
    flush_all_on_shutdown(&registry);
    drop(registry);

    // Reopen and verify.
    let registry2 = Arc::new(TenantRegistry::new(dir.path(), GraphConfig::new()));
    let node_count = {
        let g2 = registry2.get_or_load(&addr).unwrap();
        let g = g2.read().unwrap();
        let count = g.node_count();
        drop(g);
        count
    };
    assert_eq!(
        node_count,
        1,
        "expected 1 node after reopen, flush_all_on_shutdown did not persist data"
    );
}

#[test]
fn flush_all_on_shutdown_is_noop_when_no_graphs_loaded() {
    let dir = tempfile::tempdir().unwrap();
    let registry = TenantRegistry::new(dir.path(), GraphConfig::new());
    // No graphs loaded — must not panic.
    flush_all_on_shutdown(&registry);
}
