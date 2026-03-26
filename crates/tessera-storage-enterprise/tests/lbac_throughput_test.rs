// Copyright 2026 BelowZero Security OU. All rights reserved.

//! Throughput regression guards for LBAC enforcement on hot paths.

use std::collections::BTreeSet;
use tessera_auth::lbac::{Clearance, SecurityLabel, SecurityPolicy};
use tessera_graph::{Graph, GraphAccess, props};
use tessera_storage_enterprise::lbac::SecureGraph;

fn build_graph(node_count: usize) -> (Graph, Vec<tessera_graph::NodeId>) {
    let mut g = Graph::new();
    let label = SecurityLabel::default();
    let mut ids = Vec::with_capacity(node_count);
    for i in 0..node_count {
        #[allow(clippy::cast_possible_wrap)]
        let mut p = props! { "i" => i as i64 };
        SecurityPolicy::inject_label(&mut p, &label);
        let id = g.add_node("N", p).unwrap();
        ids.push(id);
    }
    (g, ids)
}

#[test]
fn node_read_throughput_regression_guard() {
    let iterations = 10_000_u64;
    let (mut g, ids) = build_graph(100);
    let clearance = Clearance::new(0, BTreeSet::new());
    let sg = SecureGraph::new(&mut g, clearance);

    let start = std::time::Instant::now();
    for i in 0..iterations {
        #[allow(clippy::cast_possible_truncation)]
        let id = ids[(i as usize) % ids.len()];
        let _ = sg.node(id).unwrap();
    }
    let elapsed = start.elapsed();

    #[allow(clippy::cast_possible_truncation)]
    let elapsed_us = elapsed.as_micros() as u64;
    let ops_per_sec = iterations * 1_000_000 / u64::max(elapsed_us, 1);

    let min_ops = if cfg!(debug_assertions) {
        50_000
    } else {
        500_000
    };

    assert!(
        ops_per_sec >= min_ops,
        "SecureGraph node() throughput regression: {ops_per_sec} ops/sec (minimum: {min_ops})"
    );
}

#[test]
fn node_ids_throughput_regression_guard() {
    let (mut g, _ids) = build_graph(1_000);
    let clearance = Clearance::new(0, BTreeSet::new());
    let sg = SecureGraph::new(&mut g, clearance);

    let iterations = 100_u64;
    let start = std::time::Instant::now();
    for _ in 0..iterations {
        let _ = sg.node_ids();
    }
    let elapsed = start.elapsed();

    #[allow(clippy::cast_possible_truncation)]
    let elapsed_us = elapsed.as_micros() as u64;
    let ops_per_sec = iterations * 1_000_000 / u64::max(elapsed_us, 1);
    let min_ops = if cfg!(debug_assertions) { 50 } else { 500 };

    assert!(
        ops_per_sec >= min_ops,
        "SecureGraph node_ids() throughput regression: {ops_per_sec} scans/sec (minimum: {min_ops})"
    );
}

#[test]
fn dominance_check_throughput_regression_guard() {
    let comps_label: BTreeSet<String> =
        ["FINANCE", "HR"].iter().map(|s| (*s).to_string()).collect();
    let label = SecurityLabel::new(3, comps_label);
    let comps_clearance: BTreeSet<String> = ["FINANCE", "HR", "LEGAL"]
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    let clearance = Clearance::new(5, comps_clearance);

    let iterations = 1_000_000_u64;
    let start = std::time::Instant::now();
    for _ in 0..iterations {
        let _ = clearance.dominates(&label);
    }
    let elapsed = start.elapsed();

    #[allow(clippy::cast_possible_truncation)]
    let elapsed_us = elapsed.as_micros() as u64;
    let ops_per_sec = iterations * 1_000_000 / u64::max(elapsed_us, 1);
    let min_ops = if cfg!(debug_assertions) {
        1_000_000
    } else {
        10_000_000
    };

    assert!(
        ops_per_sec >= min_ops,
        "dominates() throughput regression: {ops_per_sec} ops/sec (minimum: {min_ops})"
    );
}
