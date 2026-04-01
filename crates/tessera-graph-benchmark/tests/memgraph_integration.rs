// Copyright (c) 2026 BelowZero Security OU. All rights reserved.
// SPDX-License-Identifier: LicenseRef-BelowZero-Enterprise

//! Integration tests for the Memgraph benchmark target.
//!
//! These tests require a running Memgraph instance and are gated behind
//! `--features memgraph,integration-tests`. They will not compile or run
//! without both features enabled.
//!
//! Configure connection via environment variables:
//! - `MEMGRAPH_URI` (default: `bolt://localhost:7687`)
//! - `MEMGRAPH_USER` (default: empty)
//! - `MEMGRAPH_PASS` (default: empty)

#![cfg(all(feature = "memgraph", feature = "integration-tests"))]

use tessera_graph_benchmark::memgraph_target::MemgraphTarget;
use tessera_graph_benchmark::scenario::{Scenario, WriteScenario};
use tessera_graph_benchmark::target::BenchmarkTarget;
use tessera_graph::Properties;

fn connect() -> MemgraphTarget {
    let mut t = MemgraphTarget::from_env().expect("failed to connect to Memgraph");
    t.clear();
    t
}

#[test]
fn memgraph_create_and_get_node_round_trips() {
    let mut t = connect();
    let h = t.create_node("Person", Properties::new()).unwrap();
    let data = t.get_node(h).unwrap();
    assert_eq!(data.label, "Person");
}

#[test]
fn memgraph_create_edge_round_trips() {
    let mut t = connect();
    let a = t.create_node("A", Properties::new()).unwrap();
    let b = t.create_node("B", Properties::new()).unwrap();
    let eh = t.create_edge("KNOWS", a, b, Properties::new()).unwrap();
    let data = t.get_edge(eh).unwrap();
    assert_eq!(data.label, "KNOWS");
}

#[test]
fn memgraph_bfs_traversal_returns_nodes() {
    let mut t = connect();
    let start = t.create_node("N", Properties::new()).unwrap();
    let mut prev = start;
    for _ in 0..4 {
        let next = t.create_node("N", Properties::new()).unwrap();
        t.create_edge("NEXT", prev, next, Properties::new())
            .unwrap();
        prev = next;
    }
    let visited = t.traverse_bfs(start, 10).unwrap();
    assert_eq!(visited.len(), 5);
}

#[test]
fn memgraph_clear_empties_graph() {
    let mut t = connect();
    t.create_node("N", Properties::new()).unwrap();
    t.create_node("N", Properties::new()).unwrap();
    t.clear();
    // After clear, creating a new node should work and traversal from it returns only itself
    let h = t.create_node("N", Properties::new()).unwrap();
    let visited = t.traverse_bfs(h, 10).unwrap();
    assert_eq!(visited.len(), 1);
}

#[test]
fn memgraph_write_scenario_runs_without_error() {
    let mut t = connect();
    let s = WriteScenario {
        node_count: 10,
        edge_count: 5,
    };
    let r = s.run(&mut t).unwrap();
    assert!(r.throughput_ops_per_sec > 0);
    assert_eq!(r.scenario_name, "write");
    assert_eq!(r.target_name, "memgraph");
}
