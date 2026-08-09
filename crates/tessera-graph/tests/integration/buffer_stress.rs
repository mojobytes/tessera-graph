// SPDX-License-Identifier: MIT

use tempfile::TempDir;
use tessera_graph::{Graph, GraphConfig, props};

/// Minimum buffer pool: 8 pages = 32 KB.
/// Forces frequent eviction even with small workloads.
const fn stress_config() -> GraphConfig {
    GraphConfig {
        memory_limit_bytes: 8 * 4096,
        create_if_missing: true,
        adj_cache_capacity: 1024,
        wal_enabled: true,
        ..GraphConfig::new()
    }
}

#[test]
fn many_nodes_small_pool() {
    let tmp = TempDir::new().unwrap();

    // 200 nodes = ~7 node pages (31 per page), plus adjacency, strings, overflow.
    // With only 8 pool slots, eviction will happen constantly.
    let mut ids = Vec::new();
    {
        let mut g = Graph::open(tmp.path(), &stress_config()).unwrap();
        for i in 0_i64..200 {
            ids.push(g.add_node("TestNode", props! { "idx" => i }).unwrap());
        }
        g.flush().unwrap();
    }

    {
        let g = Graph::open(tmp.path(), &stress_config()).unwrap();
        assert_eq!(g.node_count(), 200);

        // Verify all nodes are readable (requires page loads under eviction pressure)
        for &nid in &ids {
            let node = g.node(nid).unwrap();
            assert_eq!(node.label(), "TestNode");
        }
    }
}

#[test]
fn many_edges_small_pool() {
    let tmp = TempDir::new().unwrap();

    {
        let mut g = Graph::open(tmp.path(), &stress_config()).unwrap();

        // Chain: n0 -> n1 -> n2 -> ... -> n99
        let mut prev = g.add_node("N", props! {}).unwrap();
        for _ in 1..100 {
            let cur = g.add_node("N", props! {}).unwrap();
            g.add_edge("NEXT", prev, cur, props! {}).unwrap();
            prev = cur;
        }
        g.flush().unwrap();
    }

    {
        let g = Graph::open(tmp.path(), &stress_config()).unwrap();
        assert_eq!(g.node_count(), 100);
        assert_eq!(g.edge_count(), 99);
    }
}

#[test]
fn interleaved_read_write_small_pool() {
    let tmp = TempDir::new().unwrap();
    let mut g = Graph::open(tmp.path(), &stress_config()).unwrap();

    let mut ids = Vec::new();
    for i in 0..50_usize {
        let nid = g
            .add_node("N", props! { "i" => i64::try_from(i).unwrap() })
            .unwrap();
        ids.push(nid);

        // Read a previous node after every 10 writes (causes page thrashing)
        if i > 0 && i % 10 == 0 {
            let node = g.node(ids[i / 2]).unwrap();
            assert_eq!(node.label(), "N");
        }
    }

    g.flush().unwrap();

    // Verify all nodes
    drop(g);
    let g = Graph::open(tmp.path(), &stress_config()).unwrap();
    assert_eq!(g.node_count(), 50);
}

#[test]
fn overflow_data_small_pool() {
    let tmp = TempDir::new().unwrap();
    let long_label = "L".repeat(100); // Forces string heap overflow
    let big_prop = "P".repeat(200); // Forces property overflow

    let mut ids = Vec::new();
    {
        let mut g = Graph::open(tmp.path(), &stress_config()).unwrap();
        for _ in 0..20 {
            let nid = g
                .add_node(&long_label, props! { "data" => big_prop.as_str() })
                .unwrap();
            ids.push(nid);
        }
        g.flush().unwrap();
    }

    {
        let g = Graph::open(tmp.path(), &stress_config()).unwrap();
        assert_eq!(g.node_count(), 20);
        for &nid in &ids {
            let node = g.node(nid).unwrap();
            assert_eq!(node.label(), long_label);
        }
    }
}

#[test]
fn star_graph_small_pool() {
    let tmp = TempDir::new().unwrap();

    let center;
    {
        let mut g = Graph::open(tmp.path(), &stress_config()).unwrap();
        center = g.add_node("Hub", props! {}).unwrap();
        for i in 0_i64..50 {
            let leaf = g.add_node("Leaf", props! { "i" => i }).unwrap();
            g.add_edge("LINK", center, leaf, props! {}).unwrap();
        }
        g.flush().unwrap();
    }

    {
        let g = Graph::open(tmp.path(), &stress_config()).unwrap();
        assert_eq!(g.node_count(), 51);
        assert_eq!(g.edge_count(), 50);

        let out = g.outgoing_edges(center).unwrap();
        assert_eq!(out.len(), 50);
    }
}
