// Copyright (c) 2026 BelowZero Security OU. All rights reserved.
// SPDX-License-Identifier: LicenseRef-BelowZero-Enterprise

//! Integration tests for the `TesseraGraph` Bolt benchmark target.
//!
//! These tests require a running `TesseraGraph` server and are gated behind
//! `--features tessera-bolt,integration-tests`.
//!
//! Configure connection via environment variables:
//! - `TESSERA_BOLT_HOST` (default: `localhost`)
//! - `TESSERA_BOLT_PORT` (default: `7687`)
//! - `TESSERA_BOLT_USER` (default: `admin`)
//! - `TESSERA_BOLT_PASS` (default: `Admin.123`)

#![cfg(all(feature = "tessera-bolt", feature = "integration-tests"))]

use tessera_graph::Properties;
use tessera_graph_benchmark::target::BenchmarkTarget;
use tessera_graph_benchmark::tessera_bolt_target::TesseraBoltTarget;

fn connect() -> TesseraBoltTarget {
    let mut t = TesseraBoltTarget::from_env().expect("failed to connect to TesseraGraph");
    t.clear();
    t
}

// --- debug: verify get_node works (implies resolve_node_ids works) ---

#[test]
fn tessera_bolt_create_and_get_node_round_trips() {
    let mut t = connect();
    let h = t.create_node("Person", Properties::new()).unwrap(); // OK: test
    let data = t.get_node(h).unwrap(); // OK: test
    assert_eq!(data.label, "Person");
}

// --- traverse_bfs ---

#[test]
fn tessera_bolt_bfs_isolated_node_returns_start() {
    let mut t = connect();
    let start = t.create_node("N", Properties::new()).unwrap(); // OK: test
    let visited = t.traverse_bfs(start, 10).unwrap(); // OK: test
    assert_eq!(visited.len(), 1, "isolated node BFS must return [start]");
    assert_eq!(visited[0], start);
}

#[test]
fn tessera_bolt_bfs_returns_all_nodes_in_chain() {
    let mut t = connect();
    let start = t.create_node("N", Properties::new()).unwrap(); // OK: test
    let mut prev = start;
    for _ in 0..4 {
        let next = t.create_node("N", Properties::new()).unwrap(); // OK: test
        t.create_edge("NEXT", prev, next, Properties::new())
            .unwrap(); // OK: test
        prev = next;
    }
    let visited = t.traverse_bfs(start, 10).unwrap(); // OK: test
    assert_eq!(visited.len(), 5, "BFS on 5-node chain must return 5");
}

// --- traverse_dfs ---

#[test]
fn tessera_bolt_dfs_returns_all_nodes_in_chain() {
    let mut t = connect();
    let start = t.create_node("N", Properties::new()).unwrap(); // OK: test
    let mut prev = start;
    for _ in 0..4 {
        let next = t.create_node("N", Properties::new()).unwrap(); // OK: test
        t.create_edge("NEXT", prev, next, Properties::new())
            .unwrap(); // OK: test
        prev = next;
    }
    let visited = t.traverse_dfs(start, 10).unwrap(); // OK: test
    assert_eq!(visited.len(), 5, "DFS on 5-node chain must return 5");
}

// --- shortest_path ---

#[test]
fn tessera_bolt_shortest_path_finds_path_in_chain() {
    let mut t = connect();
    let a = t.create_node("N", Properties::new()).unwrap(); // OK: test
    let b = t.create_node("N", Properties::new()).unwrap(); // OK: test
    let c = t.create_node("N", Properties::new()).unwrap(); // OK: test
    t.create_edge("E", a, b, Properties::new()).unwrap(); // OK: test
    t.create_edge("E", b, c, Properties::new()).unwrap(); // OK: test
    let path = t.shortest_path(a, c).unwrap(); // OK: test
    assert!(path.is_some(), "path A→B→C must be found");
    assert_eq!(path.unwrap().len(), 3, "path must have 3 nodes");
}

#[test]
fn tessera_bolt_shortest_path_unreachable_returns_none() {
    let mut t = connect();
    let a = t.create_node("N", Properties::new()).unwrap(); // OK: test
    let b = t.create_node("N", Properties::new()).unwrap(); // OK: test
    let path = t.shortest_path(a, b).unwrap(); // OK: test
    assert!(path.is_none(), "disconnected nodes must return None");
}
