// SPDX-License-Identifier: MIT

#[allow(unused)]
mod helpers;

use criterion::{BatchSize, BenchmarkId, Criterion, criterion_group, criterion_main};
use tempfile::TempDir;
use tessera_graph::{Graph, GraphConfig, Properties};

fn open_file_graph(wal_enabled: bool) -> (TempDir, Graph) {
    let tmp = TempDir::new().unwrap();
    let config = GraphConfig {
        memory_limit_bytes: 64 * 1024 * 1024,
        create_if_missing: true,
        adj_cache_capacity: 65_536,
        wal_enabled,
        ..GraphConfig::new()
    };
    let g = Graph::open(tmp.path(), &config).unwrap();
    (tmp, g)
}

fn bench_file_add_node(c: &mut Criterion) {
    let mut group = c.benchmark_group("wal_impact/add_node");

    group.bench_function("wal_enabled", |b| {
        b.iter_batched(
            || open_file_graph(true),
            |(_tmp, mut g)| g.add_node("N", Properties::new()).unwrap(),
            BatchSize::SmallInput,
        );
    });

    group.bench_function("wal_disabled", |b| {
        b.iter_batched(
            || open_file_graph(false),
            |(_tmp, mut g)| g.add_node("N", Properties::new()).unwrap(),
            BatchSize::SmallInput,
        );
    });

    group.bench_function("in_memory", |b| {
        b.iter_batched(
            Graph::new,
            |mut g| g.add_node("N", Properties::new()).unwrap(),
            BatchSize::SmallInput,
        );
    });

    group.finish();
}

fn bench_file_bulk_nodes(c: &mut Criterion) {
    let mut group = c.benchmark_group("wal_impact/bulk_nodes");
    group.sample_size(10);

    for n in [100, 1_000] {
        group.bench_with_input(BenchmarkId::new("wal_enabled", n), &n, |b, &n| {
            b.iter_batched(
                || open_file_graph(true),
                |(_tmp, mut g)| {
                    for _ in 0..n {
                        g.add_node("N", Properties::new()).unwrap();
                    }
                },
                BatchSize::SmallInput,
            );
        });

        group.bench_with_input(BenchmarkId::new("wal_disabled", n), &n, |b, &n| {
            b.iter_batched(
                || open_file_graph(false),
                |(_tmp, mut g)| {
                    for _ in 0..n {
                        g.add_node("N", Properties::new()).unwrap();
                    }
                },
                BatchSize::SmallInput,
            );
        });

        group.bench_with_input(BenchmarkId::new("batch_mode", n), &n, |b, &n| {
            b.iter_batched(
                || open_file_graph(true),
                |(_tmp, mut g)| {
                    g.begin_batch();
                    for _ in 0..n {
                        g.add_node("N", Properties::new()).unwrap();
                    }
                    g.end_batch().unwrap();
                },
                BatchSize::SmallInput,
            );
        });

        group.bench_with_input(BenchmarkId::new("in_memory", n), &n, |b, &n| {
            b.iter_batched(
                Graph::new,
                |mut g| {
                    for _ in 0..n {
                        g.add_node("N", Properties::new()).unwrap();
                    }
                },
                BatchSize::SmallInput,
            );
        });
    }

    group.finish();
}

fn bench_file_add_edge(c: &mut Criterion) {
    let mut group = c.benchmark_group("wal_impact/add_edge");

    group.bench_function("wal_enabled", |b| {
        b.iter_batched(
            || {
                let (tmp, mut g) = open_file_graph(true);
                let a = g.add_node("A", Properties::new()).unwrap();
                let b = g.add_node("B", Properties::new()).unwrap();
                (tmp, g, a, b)
            },
            |(_tmp, mut g, a, b)| g.add_edge("R", a, b, Properties::new()).unwrap(),
            BatchSize::SmallInput,
        );
    });

    group.bench_function("wal_disabled", |b| {
        b.iter_batched(
            || {
                let (tmp, mut g) = open_file_graph(false);
                let a = g.add_node("A", Properties::new()).unwrap();
                let b = g.add_node("B", Properties::new()).unwrap();
                (tmp, g, a, b)
            },
            |(_tmp, mut g, a, b)| g.add_edge("R", a, b, Properties::new()).unwrap(),
            BatchSize::SmallInput,
        );
    });

    group.finish();
}

fn bench_file_flush(c: &mut Criterion) {
    let mut group = c.benchmark_group("wal_impact/flush");

    for n in [100, 1_000] {
        group.bench_with_input(BenchmarkId::new("wal_enabled", n), &n, |b, &n| {
            b.iter_batched(
                || {
                    let (tmp, mut g) = open_file_graph(true);
                    for _ in 0..n {
                        g.add_node("N", Properties::new()).unwrap();
                    }
                    (tmp, g)
                },
                |(_tmp, mut g)| g.flush().unwrap(),
                BatchSize::LargeInput,
            );
        });

        group.bench_with_input(BenchmarkId::new("wal_disabled", n), &n, |b, &n| {
            b.iter_batched(
                || {
                    let (tmp, mut g) = open_file_graph(false);
                    for _ in 0..n {
                        g.add_node("N", Properties::new()).unwrap();
                    }
                    (tmp, g)
                },
                |(_tmp, mut g)| g.flush().unwrap(),
                BatchSize::LargeInput,
            );
        });
    }

    group.finish();
}

// ── Issue #58: cost of the size-triggered checkpoint ─────────────────────

fn open_with_threshold(threshold: Option<u64>) -> (TempDir, Graph) {
    let tmp = TempDir::new().unwrap();
    let config = GraphConfig {
        wal_checkpoint_threshold_bytes: threshold,
        ..GraphConfig::new()
    };
    let g = Graph::open(tmp.path(), &config).unwrap();
    (tmp, g)
}

/// What the threshold costs when it never fires: one comparison per WAL
/// append plus one flag read per batch close. Both arms do exactly the same
/// I/O — a 2,000-node batch writes ~302 KB of journal, nowhere near the 64 MB
/// default — so any gap between them is pure overhead of the mechanism.
///
/// # Reading these numbers
///
/// Each arm is interleaved rather than run to completion in turn. Measured
/// the naive way (all of arm A, then all of arm B), whichever arm ran second
/// came out ~3.5x slower *regardless of which one it was*: every iteration
/// creates and destroys a temp directory, and the filesystem degrades as the
/// run proceeds. Swapping the arms swapped the result, which is what proved
/// the effect belonged to the harness and not to the code under test.
///
/// Interleaving spreads that drift evenly across both arms instead of dumping
/// it on the second one. Compare like slot against like slot (`run1` vs
/// `run1`), and read the gap between an arm's own `run1` and `run2` as the
/// magnitude of the drift. The residual is still filesystem-dominated, so
/// treat overlapping confidence intervals as "no difference detectable above
/// the noise floor" — not as a precise measurement of the comparison's cost.
fn bench_threshold_accounting_overhead(c: &mut Criterion) {
    const NODES: usize = 2_000;
    const DEFAULT: Option<u64> = Some(64 * 1024 * 1024);
    const DISABLED: Option<u64> = None;

    let mut group = c.benchmark_group("wal_checkpoint/accounting_overhead");
    group.sample_size(30);

    // A, B, B, A — each arm runs once early and once late, so the harness
    // drift lands on both equally. The labels carry the slot number because
    // Criterion rejects duplicate IDs within a group rather than pooling them:
    // compare `run1` against `run1` and `run2` against `run2`, and read the
    // early/late spread within one arm as the size of the drift itself.
    for (label, threshold) in [
        ("default_64mb_never_crossed/run1", DEFAULT),
        ("disabled/run1", DISABLED),
        ("disabled/run2", DISABLED),
        ("default_64mb_never_crossed/run2", DEFAULT),
    ] {
        group.bench_function(label, |b| {
            b.iter_batched(
                || open_with_threshold(threshold),
                |(_tmp, mut g)| {
                    g.begin_batch();
                    for _ in 0..NODES {
                        g.add_node("N", Properties::new()).unwrap();
                    }
                    g.end_batch().unwrap();
                },
                BatchSize::LargeInput,
            );
        });
    }

    group.finish();
}

/// Whether a threshold-triggered checkpoint costs what was written since the
/// last one, or what the graph holds in total.
///
/// Each arm pre-loads a graph of a different size and checkpoints it, then
/// measures closing one further batch that writes the *same* amount in every
/// arm and crosses the threshold. If the cost is flat, the arms match; if it
/// tracks the graph's total size, the numbers climb with `preloaded`, and the
/// design does not hold at the volumes issue #58 was reported at.
///
/// # Order matters here
///
/// The sizes are deliberately NOT run smallest-to-largest. This harness
/// penalises whichever arm runs later (see
/// `bench_threshold_accounting_overhead`), so an ascending order would put the
/// biggest graph in the most penalised slot and manufacture exactly the upward
/// slope this benchmark exists to detect. Running them large-first means a
/// genuine size dependency now has to show up *against* the harness bias
/// rather than riding along with it.
fn bench_checkpoint_cost_vs_graph_size(c: &mut Criterion) {
    const THRESHOLD: u64 = 64 * 1024;
    const BATCH_NODES: usize = 500;

    let mut group = c.benchmark_group("wal_checkpoint/cost_vs_graph_size");
    group.sample_size(10);

    for preloaded in [50_000, 1_000, 10_000] {
        group.bench_with_input(
            BenchmarkId::new("preloaded_nodes", preloaded),
            &preloaded,
            |b, &preloaded| {
                b.iter_batched(
                    || {
                        let (tmp, mut g) = open_with_threshold(Some(THRESHOLD));
                        g.begin_batch();
                        for _ in 0..preloaded {
                            g.add_node("N", Properties::new()).unwrap();
                        }
                        g.end_batch().unwrap();
                        // Start every measured close from a checkpointed
                        // journal, so the batch below is all that has
                        // accumulated since.
                        g.flush().unwrap();
                        (tmp, g)
                    },
                    |(_tmp, mut g)| {
                        g.begin_batch();
                        for _ in 0..BATCH_NODES {
                            g.add_node("N", Properties::new()).unwrap();
                        }
                        // Crosses the 64 KB threshold, so this close
                        // checkpoints.
                        g.end_batch().unwrap();
                    },
                    BatchSize::LargeInput,
                );
            },
        );
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_file_add_node,
    bench_file_bulk_nodes,
    bench_file_add_edge,
    bench_file_flush,
    bench_threshold_accounting_overhead,
    bench_checkpoint_cost_vs_graph_size,
);
criterion_main!(benches);
