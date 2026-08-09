// SPDX-License-Identifier: BSL-1.1

//! Lock-contention benchmark entry point (Scenario 1, in-process).
//!
//! Thin shell: reads the declarative matrix (`matrix.toml`, the single source
//! of truth), runs each runnable point through the `bench_support` harness,
//! aggregates latency percentiles, and prints the versioned JSON reports. The
//! real work (dataset build, threaded reader/writer orchestration, warmup,
//! variant dispatch) lives in `bench_support`, unit-tested there.
//!
//! Non-runnable points (Scenario 2, Bolt-in-Docker) are skipped with an
//! explicit message — never silently dropped.

use criterion::{Criterion, criterion_group, criterion_main};

use tessera_graph_server::bench_support::contention_runner::{
    RunConfig, run_contention, run_contention_repeated,
};
use tessera_graph_server::bench_support::latency::LatencyTracker;
use tessera_graph_server::bench_support::matrix::{parse_matrix, runnable_points_with_skip_report};
use tessera_graph_server::bench_support::report::BenchReport;

/// The matrix is embedded at compile time so the bench binary is self-contained.
const MATRIX_TOML: &str = include_str!("matrix.toml");

/// Fixed operation counts per point. Deterministic in count (not in wall-clock
/// latency, which contention makes variable — hence percentiles, not means).
const OPS_PER_READER: u32 = 200;
const OPS_PER_WRITER: u32 = 50;

/// The report for each point pools raw latency samples across this many
/// independent runs, so the percentiles are a robust baseline rather than one
/// noisy single execution. Override with the `LOCK_BENCH_RUNS` env var.
const DEFAULT_RUNS_PER_POINT: u32 = 5;

fn runs_per_point() -> u32 {
    std::env::var("LOCK_BENCH_RUNS")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_RUNS_PER_POINT)
}

fn aggregate(
    latencies: &[std::time::Duration],
) -> tessera_graph_server::bench_support::latency::LatencyStats {
    let mut tracker = LatencyTracker::new();
    for &d in latencies {
        tracker.record(d);
    }
    tracker.stats()
}

fn bench_lock_contention(c: &mut Criterion) {
    let points = parse_matrix(MATRIX_TOML).expect("matrix.toml parses");
    let (runnable, skipped) = runnable_points_with_skip_report(&points);

    for msg in &skipped {
        println!("[lock_contention] {msg}");
    }

    let mut reports: Vec<BenchReport> = Vec::with_capacity(runnable.len());
    let runs = runs_per_point();
    println!("[lock_contention] pooling {runs} runs per point for the report");

    for point in runnable {
        let cfg = RunConfig {
            scenario: point.scenario,
            readers: point.readers,
            writers: point.writers,
            ops_per_reader: OPS_PER_READER,
            ops_per_writer: OPS_PER_WRITER,
            dataset_size: point.dataset_size,
            variant: point.variant,
        };

        // Warmup, discarded — primes caches/allocations before measuring.
        let _ = run_contention(cfg);

        let name = point.name();
        c.bench_function(&name, |b| {
            b.iter(|| {
                let _ = run_contention(cfg);
            });
        });

        // The reported percentiles pool raw samples over `runs` independent
        // executions — a robust baseline, not one noisy single run.
        let result = run_contention_repeated(cfg, runs);
        let report = BenchReport::new(
            *point,
            aggregate(&result.reader_latencies),
            aggregate(&result.writer_latencies),
            result.reader_latencies.len() as u64,
            result.writer_latencies.len() as u64,
        );
        reports.push(report);
    }

    match serde_json::to_string_pretty(&reports) {
        Ok(json) => println!("[lock_contention] reports:\n{json}"),
        Err(e) => eprintln!("[lock_contention] failed to serialize reports: {e}"),
    }
}

criterion_group!(benches, bench_lock_contention);
criterion_main!(benches);
