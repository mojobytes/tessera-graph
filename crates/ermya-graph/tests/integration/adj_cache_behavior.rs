// SPDX-License-Identifier: MIT

use ermya_graph::{Graph, GraphConfig, Properties, props};
use tempfile::TempDir;

const fn small_cache_config() -> GraphConfig {
    GraphConfig {
        memory_limit_bytes: 64 * 1024,
        create_if_missing: true,
        adj_cache_capacity: 8, // minimum capacity
        wal_enabled: true,
        ..GraphConfig::new()
    }
}

#[test]
fn adj_cache_miss_reloads_from_disk() {
    let tmp = TempDir::new().unwrap();
    let config = small_cache_config();

    let mut ids = Vec::new();
    {
        let mut g = Graph::open(tmp.path(), &config).unwrap();
        // Create 20 nodes (well over cache capacity of 8)
        for i in 0..20 {
            let n = g.add_node("N", props! { "i" => i64::from(i) }).unwrap();
            ids.push(n);
        }
        // Add edges between consecutive nodes
        for w in ids.windows(2) {
            g.add_edge("NEXT", w[0], w[1], Properties::new()).unwrap();
        }
        g.flush().unwrap();
    }

    // Reopen with small cache — most adjacency entries will be evicted
    {
        let g = Graph::open(tmp.path(), &config).unwrap();
        assert_eq!(g.node_count(), 20);
        assert_eq!(g.edge_count(), 19);

        // Verify ALL nodes' outgoing edges are accessible (even evicted ones)
        for (i, &nid) in ids.iter().enumerate() {
            let out = g.outgoing_edges(nid).unwrap();
            if i < 19 {
                assert_eq!(out.len(), 1, "node {i} should have 1 outgoing edge");
            } else {
                assert_eq!(out.len(), 0, "last node should have 0 outgoing edges");
            }
        }

        // Verify incoming edges too
        for (i, &nid) in ids.iter().enumerate() {
            let inc = g.incoming_edges(nid).unwrap();
            if i == 0 {
                assert_eq!(inc.len(), 0, "first node should have 0 incoming edges");
            } else {
                assert_eq!(inc.len(), 1, "node {i} should have 1 incoming edge");
            }
        }
    }
}

/// Verifies that the `adj_cache` is pre-warmed on `Graph::open`, so that
/// `outgoing_edges` after a reopen does not fall back to the O(N) page scan.
#[test]
fn adj_cache_warmed_on_open() {
    let tmp = TempDir::new().unwrap();
    let config = GraphConfig {
        memory_limit_bytes: 4 * 1024 * 1024,
        create_if_missing: true,
        adj_cache_capacity: 256,
        wal_enabled: true,
        ..GraphConfig::new()
    };

    let hub;
    let mut targets = Vec::new();

    // Create hub node with 3 outgoing edges, then close.
    {
        let mut g = Graph::open(tmp.path(), &config).unwrap();
        hub = g.add_node("Hub", Properties::new()).unwrap();
        for _ in 0..3 {
            let t = g.add_node("T", Properties::new()).unwrap();
            g.add_edge("LINK", hub, t, Properties::new()).unwrap();
            targets.push(t);
        }
        g.flush().unwrap();
    }

    // Reopen and query outgoing edges without any prior cache miss.
    {
        let g = Graph::open(tmp.path(), &config).unwrap();
        let out = g.outgoing_edges(hub).unwrap();
        assert_eq!(out.len(), 3, "hub must have 3 outgoing edges after reopen");
        for t in &targets {
            assert!(
                out.iter().any(|e| e.target() == *t),
                "target {t:?} must be reachable after reopen"
            );
        }
    }
}

#[test]
fn adj_cache_works_with_node_removal() {
    let tmp = TempDir::new().unwrap();
    let config = small_cache_config();

    {
        let mut g = Graph::open(tmp.path(), &config).unwrap();
        let a = g.add_node("A", Properties::new()).unwrap();
        let b = g.add_node("B", Properties::new()).unwrap();
        let c = g.add_node("C", Properties::new()).unwrap();
        g.add_edge("R", a, b, Properties::new()).unwrap();
        g.add_edge("R", b, c, Properties::new()).unwrap();
        g.remove_node(b).unwrap();
        g.flush().unwrap();

        assert_eq!(g.node_count(), 2);
        assert_eq!(g.edge_count(), 0);
        assert!(g.outgoing_edges(a).unwrap().is_empty());
        assert!(g.incoming_edges(c).unwrap().is_empty());
    }
}
