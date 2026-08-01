// SPDX-License-Identifier: BSL-1.1

//! Concurrent reader/writer orchestration for the lock-contention benchmark.
//!
//! Spawns `readers` read-only threads (each timing a MATCH binding compile
//! under a read lock) and `writers` mutation threads (each timing a
//! `MATCH … CREATE` through the chosen [`Variant`]), all released together by a
//! [`Barrier`] so contention starts simultaneously. Collects per-operation
//! latencies. Operation counts are fixed and deterministic; wall-clock latency
//! values are not (contention has inherent variance) — the harness controls
//! this by fixing counts and reporting percentiles, never single runs.

use std::collections::HashMap;
use std::sync::{Arc, Barrier, Mutex, RwLock};
use std::thread;
use std::time::{Duration, Instant};

use tessera_graph::gql::{self, GqlValue};
use tessera_graph::Graph;

use crate::bench_support::dataset::build_dataset;
use crate::bench_support::matrix::{Scenario, Variant};
use crate::bench_support::timed_mutation::time_match_mutation;
use crate::bench_support::variant_shim::execute_match_mutation_single_lock;
use tessera_graph_config::QueryLanguage;

/// Configuration for a single contention run (one matrix point).
#[derive(Debug, Clone, Copy)]
pub struct RunConfig {
    pub scenario: Scenario,
    pub readers: u32,
    pub writers: u32,
    pub ops_per_reader: u32,
    pub ops_per_writer: u32,
    pub dataset_size: u32,
    pub variant: Variant,
}

/// Collected per-operation latencies from a contention run.
#[derive(Debug, Default)]
pub struct ContentionResult {
    pub reader_latencies: Vec<Duration>,
    pub writer_latencies: Vec<Duration>,
}

/// Returns the read-only Cypher used by reader threads (a MATCH over the bench
/// label). Reader threads compile its bindings under a read lock without
/// writing, modelling concurrent readers contending with writers.
fn reader_match_clause() -> gql::MatchClause {
    let stmt = tessera_graph_cypher::parse_with_mode(
        "MATCH (n:BenchNode) RETURN n",
        QueryLanguage::CypherCompat,
    )
    .expect("reader query parses");
    match stmt {
        gql::GqlStatement::Query(q) => q.match_clause,
        other => panic!("expected query, got {other:?}"),
    }
}

/// Returns the writer mutation for a scenario. `match-create` and `match-set`
/// have distinct shapes; `merge` and `unwind` currently reuse the `match-create`
/// shape as a placeholder until those scenarios get their own queries — the
/// scenario axis is wired here so they plug in without touching the runner.
///
/// # Panics
/// Panics if the built-in scenario query fails to parse or is not a mutation —
/// a programming error in this function, not a runtime input error.
pub(crate) fn mutation_for_scenario(scenario: Scenario) -> gql::MutationStatement {
    // `match_same_arms`: merge/unwind deliberately reuse the match-create query
    // for now; kept as separate arms so each gets its own query independently.
    #[allow(clippy::match_same_arms)]
    let cypher = match scenario {
        Scenario::MatchCreate => "MATCH (n:BenchNode) CREATE (x:Tagged)",
        Scenario::MatchSet => "MATCH (n:BenchNode) SET n.touched = true",
        Scenario::Merge => "MATCH (n:BenchNode) CREATE (x:Tagged)",
        Scenario::Unwind => "MATCH (n:BenchNode) CREATE (x:Tagged)",
    };
    let stmt = tessera_graph_cypher::parse_with_mode(cypher, QueryLanguage::CypherCompat)
        .expect("scenario mutation parses");
    match stmt {
        gql::GqlStatement::Mutation(m) => m,
        other => panic!("expected mutation, got {other:?}"),
    }
}

/// Runs one contention scenario and returns the collected latencies.
///
/// The `Barrier` is sized to `readers + writers` (min 1 so an all-zero config
/// does not deadlock). Reader threads time a MATCH binding compile under a read
/// lock; writer threads time the mutation via the configured variant.
///
/// # Panics
/// Panics if the deterministic dataset fails to build or a result mutex is
/// poisoned — both indicate a bug in the harness, not a benchmark input.
#[must_use]
pub fn run_contention(cfg: RunConfig) -> ContentionResult {
    let mut g = Graph::new();
    build_dataset(&mut g, cfg.dataset_size).expect("dataset builds");
    let shared = Arc::new(RwLock::new(g));

    let thread_count = (cfg.readers + cfg.writers).max(1) as usize;
    let barrier = Arc::new(Barrier::new(thread_count));

    let reader_out: Arc<Mutex<Vec<Duration>>> = Arc::new(Mutex::new(Vec::new()));
    let writer_out: Arc<Mutex<Vec<Duration>>> = Arc::new(Mutex::new(Vec::new()));

    let match_clause = Arc::new(reader_match_clause());
    let mutation = Arc::new(mutation_for_scenario(cfg.scenario));

    thread::scope(|scope| {
        for _ in 0..cfg.readers {
            let shared = Arc::clone(&shared);
            let barrier = Arc::clone(&barrier);
            let out = Arc::clone(&reader_out);
            let mc = Arc::clone(&match_clause);
            scope.spawn(move || {
                barrier.wait();
                let mut local = Vec::with_capacity(cfg.ops_per_reader as usize);
                for _ in 0..cfg.ops_per_reader {
                    let start = Instant::now();
                    let guard = shared.read().expect("read lock");
                    let _ = gql::compile_match_bindings(&*guard, &mc, None);
                    drop(guard);
                    local.push(start.elapsed());
                }
                out.lock().expect("reader out").extend(local);
            });
        }
        for _ in 0..cfg.writers {
            let shared = Arc::clone(&shared);
            let barrier = Arc::clone(&barrier);
            let out = Arc::clone(&writer_out);
            let mutation = Arc::clone(&mutation);
            let variant = cfg.variant;
            scope.spawn(move || {
                barrier.wait();
                let mut local = Vec::with_capacity(cfg.ops_per_writer as usize);
                for _ in 0..cfg.ops_per_writer {
                    let elapsed = match variant {
                        // Two-lock path: time it via the shared timing wrapper
                        // (`time_match_mutation`), the same one the single-thread
                        // measurement uses, so both share one timing definition.
                        Variant::TwoLockCurrent => time_match_mutation(
                            &shared,
                            &mutation,
                            &HashMap::new(),
                        )
                        .map(|(d, _, _)| d)
                        .unwrap_or_default(),
                        // Single-lock variant has no two-phase timing wrapper; the
                        // whole mutation is one locked section, timed inline.
                        Variant::SingleLockA => {
                            let params: HashMap<String, GqlValue> = HashMap::new();
                            let start = Instant::now();
                            let _ = execute_match_mutation_single_lock(&shared, &mutation, &params);
                            start.elapsed()
                        }
                    };
                    local.push(elapsed);
                }
                out.lock().expect("writer out").extend(local);
            });
        }
    });

    let reader_latencies = Arc::try_unwrap(reader_out)
        .expect("no reader refs left")
        .into_inner()
        .expect("reader mutex");
    let writer_latencies = Arc::try_unwrap(writer_out)
        .expect("no writer refs left")
        .into_inner()
        .expect("writer mutex");

    ContentionResult { reader_latencies, writer_latencies }
}

/// Runs `run_contention` `runs` times and pools every raw latency sample into a
/// single [`ContentionResult`].
///
/// Percentiles must be computed over the pooled raw samples, never averaged
/// across per-run percentiles (a percentile of percentiles is not a
/// percentile). Pooling raw samples across `runs` independent executions is
/// what turns one noisy single run into a robust baseline, absorbing the
/// wall-clock variance that contention introduces. `runs == 0` yields an empty
/// result.
#[must_use]
pub fn run_contention_repeated(cfg: RunConfig, runs: u32) -> ContentionResult {
    let mut pooled = ContentionResult::default();
    for _ in 0..runs {
        let one = run_contention(cfg);
        pooled.reader_latencies.extend(one.reader_latencies);
        pooled.writer_latencies.extend(one.writer_latencies);
    }
    pooled
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(readers: u32, writers: u32) -> RunConfig {
        RunConfig {
            scenario: Scenario::MatchCreate,
            readers,
            writers,
            ops_per_reader: 10,
            ops_per_writer: 5,
            dataset_size: 50,
            variant: Variant::TwoLockCurrent,
        }
    }

    #[test]
    fn run_contention_returns_exactly_readers_times_ops_samples() {
        let r = run_contention(cfg(3, 1));
        assert_eq!(r.reader_latencies.len(), 30);
    }

    #[test]
    fn run_contention_with_zero_writers_still_produces_reader_samples() {
        let r = run_contention(cfg(3, 0));
        assert_eq!(r.reader_latencies.len(), 30);
        assert_eq!(r.writer_latencies.len(), 0);
    }

    #[test]
    fn run_contention_with_zero_readers_returns_empty_reader_latencies_no_panic() {
        let r = run_contention(cfg(0, 2));
        assert_eq!(r.reader_latencies.len(), 0);
        assert_eq!(r.writer_latencies.len(), 10);
    }

    #[test]
    fn run_contention_is_deterministic_in_operation_count_across_repeated_runs() {
        // The COUNTS are deterministic; latency VALUES are not (wall-clock under
        // contention varies). We assert only lengths, never durations.
        let a = run_contention(cfg(2, 2));
        let b = run_contention(cfg(2, 2));
        assert_eq!(a.reader_latencies.len(), b.reader_latencies.len());
        assert_eq!(a.writer_latencies.len(), b.writer_latencies.len());
    }

    #[test]
    fn run_contention_single_lock_a_variant_produces_samples() {
        let mut c = cfg(1, 1);
        c.variant = Variant::SingleLockA;
        let r = run_contention(c);
        assert_eq!(r.reader_latencies.len(), 10);
        assert_eq!(r.writer_latencies.len(), 5);
    }

    #[test]
    fn run_contention_repeated_accumulates_samples_from_every_run() {
        // 3 readers × 10 ops = 30 reader samples per run; 4 runs => 120.
        // Percentiles are computed over the pooled raw samples, so the caller
        // gets one robust result from N runs instead of one noisy single run.
        let r = run_contention_repeated(cfg(3, 1), 4);
        assert_eq!(r.reader_latencies.len(), 120);
        assert_eq!(r.writer_latencies.len(), 4 * 5);
    }

    #[test]
    fn run_contention_repeated_with_one_run_equals_a_single_run_count() {
        let single = run_contention(cfg(2, 2));
        let repeated = run_contention_repeated(cfg(2, 2), 1);
        assert_eq!(repeated.reader_latencies.len(), single.reader_latencies.len());
        assert_eq!(repeated.writer_latencies.len(), single.writer_latencies.len());
    }

    #[test]
    fn run_contention_repeated_zero_runs_returns_empty() {
        let r = run_contention_repeated(cfg(3, 1), 0);
        assert_eq!(r.reader_latencies.len(), 0);
        assert_eq!(r.writer_latencies.len(), 0);
    }
}
