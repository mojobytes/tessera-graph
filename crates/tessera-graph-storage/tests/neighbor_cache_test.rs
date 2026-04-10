// Copyright 2026 BelowZero Security OU. All rights reserved.

//! Integration tests for `NeighborCache` — correctness and performance.

use tessera_graph::{Edge, Graph, GraphAccess, NodeId, Properties};
use tessera_graph_storage::cache::NeighborCache;

/// Builds a chain graph: N0→N1→N2→...→N(n-1).
fn chain_graph(n: usize) -> (Graph, Vec<NodeId>) {
    let mut g = Graph::new();
    let mut nodes = Vec::with_capacity(n);
    for _ in 0..n {
        nodes.push(g.add_node("N", Properties::new()).expect("add_node"));
    }
    for i in 0..n - 1 {
        g.add_edge("E", nodes[i], nodes[i + 1], Properties::new())
            .expect("add_edge");
    }
    (g, nodes)
}

// ── Correctness ────────────────────────────────────────────────────────────

#[test]
fn cache_miss_populates_from_graph() {
    let (g, nodes) = chain_graph(3);
    let cache = NeighborCache::new(g);

    let out = cache.outgoing_neighbor_ids(nodes[0]).expect("outgoing");
    assert_eq!(out, vec![nodes[1]]);

    let inc = cache.incoming_neighbor_ids(nodes[1]).expect("incoming");
    assert_eq!(inc, vec![nodes[0]]);
}

#[test]
fn cached_results_match_uncached() {
    let (g, nodes) = chain_graph(5);
    let cache = NeighborCache::new(g);

    for &node in &nodes {
        let cached_out = cache.outgoing_neighbor_ids(node).expect("cached out");
        let uncached_out: Vec<NodeId> = cache
            .outgoing_edges(node)
            .unwrap_or_default()
            .iter()
            .map(Edge::target)
            .collect();
        assert_eq!(cached_out, uncached_out, "outgoing mismatch for {node:?}");

        let cached_in = cache.incoming_neighbor_ids(node).expect("cached in");
        let uncached_in: Vec<NodeId> = cache
            .incoming_edges(node)
            .unwrap_or_default()
            .iter()
            .map(Edge::source)
            .collect();
        assert_eq!(cached_in, uncached_in, "incoming mismatch for {node:?}");
    }
}

#[test]
fn mutation_invalidation_produces_correct_results() {
    let (g, nodes) = chain_graph(3); // A→B→C
    let mut cache = NeighborCache::new(g);

    // Populate cache
    let _ = cache.outgoing_neighbor_ids(nodes[0]).expect("populate");

    // Add A→C
    cache
        .add_edge("E", nodes[0], nodes[2], Properties::new())
        .expect("add_edge");

    // Cache was invalidated — should now include C
    let out = cache.outgoing_neighbor_ids(nodes[0]).expect("after add");
    assert!(out.contains(&nodes[1]));
    assert!(out.contains(&nodes[2]));
}

// ── Performance: BFS with NeighborCache ────────────────────────────────────

/// Manual BFS using `NeighborCache::outgoing_neighbor_ids` — simulates what
/// the enterprise traversal engine does.
fn bfs_cached(cache: &NeighborCache<Graph>, start: NodeId) -> Vec<NodeId> {
    use std::collections::{HashSet, VecDeque};
    let mut visited = HashSet::new();
    let mut order = Vec::new();
    let mut queue = VecDeque::new();
    visited.insert(start);
    order.push(start);
    queue.push_back(start);
    while let Some(current) = queue.pop_front() {
        if let Ok(neighbors) = cache.outgoing_neighbor_ids(current) {
            for neighbor in neighbors {
                if visited.insert(neighbor) {
                    order.push(neighbor);
                    queue.push_back(neighbor);
                }
            }
        }
    }
    order
}

/// Manual BFS using `GraphAccess::outgoing_edges` — the uncached (slow) path.
fn bfs_uncached(graph: &Graph, start: NodeId) -> Vec<NodeId> {
    use std::collections::{HashSet, VecDeque};
    let mut visited = HashSet::new();
    let mut order = Vec::new();
    let mut queue = VecDeque::new();
    visited.insert(start);
    order.push(start);
    queue.push_back(start);
    while let Some(current) = queue.pop_front() {
        if let Ok(edges) = graph.outgoing_edges(current) {
            for edge in edges {
                let neighbor = edge.target();
                if visited.insert(neighbor) {
                    order.push(neighbor);
                    queue.push_back(neighbor);
                }
            }
        }
    }
    order
}

#[test]
fn bfs_cached_correctness_chain_1000() {
    let (g, nodes) = chain_graph(1000);
    let cache = NeighborCache::new(g);
    let visited = bfs_cached(&cache, nodes[0]);
    assert_eq!(visited.len(), 1000);
    assert_eq!(visited[0], nodes[0]);
    assert_eq!(visited[999], nodes[999]);
}

#[test]
fn bfs_cache_speedup_vs_uncached() {
    use std::time::Instant;

    let (g, nodes) = chain_graph(1000);
    let cache = NeighborCache::new(g);

    let iterations = 50;

    // Warm up the cache with one pass
    let _ = bfs_cached(&cache, nodes[0]);

    // Measure cached
    let t0 = Instant::now();
    for _ in 0..iterations {
        let visited = bfs_cached(&cache, nodes[0]);
        assert_eq!(visited.len(), 1000);
    }
    let cached_time = t0.elapsed();

    // Measure uncached (uses the inner graph directly)
    let t0 = Instant::now();
    for _ in 0..iterations {
        let visited = bfs_uncached(cache.inner(), nodes[0]);
        assert_eq!(visited.len(), 1000);
    }
    let uncached_time = t0.elapsed();

    let speedup = uncached_time.as_secs_f64() / cached_time.as_secs_f64();
    eprintln!(
        "BFS 1000-node chain x{iterations}: cached={cached_time:?}, uncached={uncached_time:?}, speedup={speedup:.1}x"
    );

    // The cache must be at least 2x faster
    assert!(
        speedup >= 2.0,
        "NeighborCache speedup {speedup:.1}x is below the 2x minimum threshold"
    );
}

#[test]
fn bfs_cached_throughput_regression_guard() {
    use std::time::Instant;

    let (g, nodes) = chain_graph(1000);
    let cache = NeighborCache::new(g);

    // Warm cache
    let _ = bfs_cached(&cache, nodes[0]);

    let iterations = 100;
    let t0 = Instant::now();
    for _ in 0..iterations {
        let visited = bfs_cached(&cache, nodes[0]);
        assert_eq!(visited.len(), 1000);
    }
    let elapsed = t0.elapsed();

    let ops_per_sec = f64::from(iterations) / elapsed.as_secs_f64();
    eprintln!(
        "BFS cached throughput: {iterations} x 1000-node BFS in {elapsed:?} = {ops_per_sec:.0} ops/s"
    );

    let floor = if cfg!(debug_assertions) {
        500.0
    } else {
        5_000.0
    };
    assert!(
        ops_per_sec >= floor,
        "BFS cached throughput {ops_per_sec:.0} ops/s < {floor:.0} ops/s floor"
    );
}
