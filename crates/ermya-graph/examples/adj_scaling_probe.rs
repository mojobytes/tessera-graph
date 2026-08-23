// SPDX-License-Identifier: MIT

//! Microbenchmark probe (QR perf): does creating N edges from the SAME source
//! node inside one batch scale linearly or super-linearly on disk?
//!
//! Run: `cargo run --release --example adj_scaling_probe -p ermya-graph`
//! Each N runs in its own process-internal timer; a per-N wall budget guards
//! against a >N² hang taking the whole machine down.

// A measurement probe: `usize → f64` casts on edge counts are intentional and
// never lose precision at these magnitudes (N ≤ 2000).
#![allow(clippy::cast_precision_loss)]

use std::time::{Duration, Instant};

use ermya_graph::{Graph, GraphConfig, Properties};

fn open_file_graph() -> (tempfile::TempDir, Graph) {
    let tmp = tempfile::TempDir::new().unwrap();
    let config = GraphConfig {
        memory_limit_bytes: 64 * 1024 * 1024,
        create_if_missing: true,
        adj_cache_capacity: 65_536,
        wal_enabled: true,
        ..GraphConfig::new()
    };
    let g = Graph::open(tmp.path(), &config).unwrap();
    (tmp, g)
}

/// Times one batch of N edges from a single source node to N distinct targets.
/// Returns the `end_batch` (flush) duration and the whole-batch duration.
fn run_n(n: usize) -> (Duration, Duration) {
    let (_tmp, mut g) = open_file_graph();

    // One source node, N target nodes.
    let src = g.add_node("Subject", Properties::new()).unwrap();
    let mut targets = Vec::with_capacity(n);
    for _ in 0..n {
        targets.push(g.add_node("Resource", Properties::new()).unwrap());
    }

    let whole = Instant::now();
    g.begin_batch();
    for &t in &targets {
        g.add_edge("HasAccess", src, t, Properties::new()).unwrap();
    }
    let flush = Instant::now();
    g.end_batch().unwrap();
    let flush_dur = flush.elapsed();
    let whole_dur = whole.elapsed();
    (flush_dur, whole_dur)
}

/// Same N edges from one source, but WITHOUT a batch — each `add_edge` writes
/// the adjacency page immediately. This isolates the per-edge rewrite cost the
/// batch is supposed to amortise away.
fn run_n_no_batch(n: usize) -> Duration {
    let (_tmp, mut g) = open_file_graph();
    let src = g.add_node("Subject", Properties::new()).unwrap();
    let mut targets = Vec::with_capacity(n);
    for _ in 0..n {
        targets.push(g.add_node("Resource", Properties::new()).unwrap());
    }
    let whole = Instant::now();
    for &t in &targets {
        g.add_edge("HasAccess", src, t, Properties::new()).unwrap();
    }
    whole.elapsed()
}

fn main() {
    // Per-N wall budget: abort the probe if a single N blows past this, so a
    // quadratic hang cannot run unbounded.
    let budget = Duration::from_secs(40);
    println!(
        "{:>6}  {:>12}  {:>12}  {:>10}",
        "N", "end_batch(s)", "whole(s)", "whole/N(ms)"
    );
    let mut prev: Option<(usize, f64)> = None;
    for &n in &[100usize, 250, 500, 1000, 2000] {
        let start = Instant::now();
        let (flush, whole) = run_n(n);
        let whole_s = whole.as_secs_f64();
        let per = whole_s / n as f64 * 1000.0;
        println!(
            "{n:>6}  {:>12.4}  {:>12.4}  {per:>10.4}",
            flush.as_secs_f64(),
            whole_s
        );
        // Scaling factor vs the previous N: linear → ratio ~ (n/prev_n);
        // quadratic → ratio ~ (n/prev_n)².
        if let Some((pn, ps)) = prev {
            let size_ratio = n as f64 / pn as f64;
            let time_ratio = whole_s / ps;
            let exponent = time_ratio.log(size_ratio);
            println!(
                "        ↳ vs N={pn}: time ×{time_ratio:.2} for size ×{size_ratio:.2} → exponent ≈ {exponent:.2} (1=linear, 2=quadratic)"
            );
        }
        prev = Some((n, whole_s));
        if start.elapsed() > budget {
            println!(
                "        ↳ ABORT: N={n} exceeded the {budget:?} per-N budget; super-linear confirmed, stopping."
            );
            break;
        }
    }

    println!("\n--- NO BATCH (per-edge immediate adjacency write) ---");
    println!("{:>6}  {:>12}  {:>10}", "N", "whole(s)", "whole/N(ms)");
    let mut prev_nb: Option<(usize, f64)> = None;
    for &n in &[100usize, 250, 500, 1000] {
        let start = Instant::now();
        let whole = run_n_no_batch(n);
        let whole_s = whole.as_secs_f64();
        println!(
            "{n:>6}  {:>12.4}  {:>10.4}",
            whole_s,
            whole_s / n as f64 * 1000.0
        );
        if let Some((pn, ps)) = prev_nb {
            let exponent = (whole_s / ps).log(n as f64 / pn as f64);
            println!("        ↳ vs N={pn}: exponent ≈ {exponent:.2}");
        }
        prev_nb = Some((n, whole_s));
        if start.elapsed() > budget {
            println!(
                "        ↳ ABORT: N={n} exceeded budget; quadratic confirmed in no-batch path."
            );
            break;
        }
    }
}
