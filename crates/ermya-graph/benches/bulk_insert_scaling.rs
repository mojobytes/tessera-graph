// SPDX-License-Identifier: MIT

//! Buffer-pool LRU scaling benchmarks (issue #51).
//!
//! Regression guard for the buffer-pool LRU cost. Before the fix,
//! `BufferPool::touch_lru_inner` was `O(pool_size)` per cached-page access, so
//! a bulk insert (which re-touches pages as their slots fill) was `O(N^2)`.
//!
//! Two views, both isolating the buffer pool (no WAL, no fsync, no disk on the
//! hot path):
//!
//! - `touch_cache_hit`: the direct microbenchmark. Pre-loads `N` pages, then
//!   times a single `put_page` cache-hit on the oldest page (worst case for the
//!   old front-to-back scan). Post-fix this is flat across `N`; pre-fix it grows
//!   linearly with `N`.
//! - `fill_pool`: an end-to-end-ish view — fills a pool with `N` distinct pages
//!   and re-touches each once, so the aggregate is `O(N^2)` pre-fix and `O(N)`
//!   post-fix. Per-page cost (total / N) should be flat post-fix.

use std::fs::File;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use ermya_graph::storage::backend::DataFile;
use ermya_graph::storage::buffer_pool::BufferPool;
use ermya_graph::storage::page::{PAGE_SIZE, new_page_buf};
use ermya_graph::{Graph, GraphConfig, Properties};
use tempfile::NamedTempFile;

/// Builds a pool with capacity for `cached * 2` pages (so nothing is evicted)
/// backed by a temp file holding `cached` zeroed pages, and pre-fills it with
/// `cached` distinct pages via `put_page`. No WAL, no fsync — `put_page` writes
/// into the in-memory frame only.
fn prefilled_pool(cached: u32) -> (BufferPool, NamedTempFile) {
    use std::io::Write;
    let mut f = NamedTempFile::new_in("/private/tmp").expect("tempfile on internal disk");
    let zeroed = [0u8; PAGE_SIZE];
    for _ in 0..cached {
        f.write_all(&zeroed).unwrap();
    }
    f.flush().unwrap();

    let pool = BufferPool::new(cached as usize * 2 * PAGE_SIZE);
    let handle: File = f.as_file().try_clone().unwrap();
    pool.register_file(DataFile::Nodes, handle);

    for i in 0..cached {
        let mut data = new_page_buf();
        data[0] = u8::try_from(i % 256).unwrap();
        pool.put_page(DataFile::Nodes, i, &data).unwrap();
    }
    (pool, f)
}

/// Direct microbenchmark: cost of one `put_page` cache-hit as a function of how
/// many pages are already cached. This is the operation that was `O(pool_size)`.
fn bench_touch_cache_hit(c: &mut Criterion) {
    let mut group = c.benchmark_group("buffer_pool/touch_cache_hit");
    let mut payload = new_page_buf();
    payload[0] = 0xAB;

    for cached in [64_u32, 512, 4_000, 16_000] {
        group.bench_with_input(
            BenchmarkId::from_parameter(cached),
            &cached,
            |b, &cached| {
                let (pool, _f) = prefilled_pool(cached);
                // Re-put page 0 (least-recently-used): a cache hit that fires
                // touch_lru_inner. Pre-fix this scans the whole LRU list.
                b.iter(|| {
                    pool.put_page(DataFile::Nodes, 0, &payload).unwrap();
                });
            },
        );
    }
    group.finish();
}

/// Aggregate view: fill a pool with `N` distinct pages, then re-touch each once.
/// The re-touch loop is `O(N^2)` pre-fix, `O(N)` post-fix. Criterion reports the
/// whole loop; divide by `N` mentally to compare per-page cost across sizes.
fn bench_fill_and_retouch(c: &mut Criterion) {
    let mut group = c.benchmark_group("buffer_pool/fill_and_retouch");
    group.sample_size(20);

    for n in [1_000_u32, 4_000, 16_000] {
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, &n| {
            b.iter_batched(
                || prefilled_pool(n),
                |(pool, _f)| {
                    let mut data = new_page_buf();
                    for i in 0..n {
                        data[0] = u8::try_from(i % 256).unwrap();
                        pool.put_page(DataFile::Nodes, i, &data).unwrap();
                    }
                },
                criterion::BatchSize::SmallInput,
            );
        });
    }
    group.finish();
}

/// End-to-end view (issue #51 repro): insert `n` nodes through the full engine
/// (`Graph::add_node`), which re-writes each page many times as its slots fill —
/// the exact workload that hit `O(N^2)`. WAL is disabled and the data dir lives
/// on the internal disk (`/private/tmp`), so the measurement reflects CPU cost
/// (dominated by the buffer-pool touch), not `fsync` latency. Compare per-node
/// cost (total / N) across sizes: flat post-fix, super-linear pre-fix.
fn bench_engine_bulk_insert(c: &mut Criterion) {
    let mut group = c.benchmark_group("engine/bulk_insert_no_wal");
    group.sample_size(10);

    for n in [2_000_u64, 8_000, 20_000] {
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, &n| {
            b.iter_batched(
                || {
                    let dir = tempfile::tempdir_in("/private/tmp").expect("tempdir");
                    let config = GraphConfig {
                        memory_limit_bytes: 64 * 1024 * 1024,
                        create_if_missing: true,
                        wal_enabled: false,
                        ..GraphConfig::default()
                    };
                    let graph = Graph::open(dir.path(), &config).expect("open");
                    (graph, dir)
                },
                |(mut graph, _dir)| {
                    for i in 0..n {
                        graph
                            .add_node(format!("Event{}", i % 8), Properties::new())
                            .expect("add_node");
                    }
                },
                criterion::BatchSize::PerIteration,
            );
        });
    }
    group.finish();
}

/// Extreme-scale end-to-end view: the same engine bulk insert as
/// `bench_engine_bulk_insert` but at the sizes where the buffer-pool LRU cost
/// stops being a rounding error and starts to dominate. At small N the LRU touch
/// is a small fraction of per-node cost (page codec, string heap, indexes); its
/// growth is `O(pool_size)` while everything else is flat, so there is a
/// crossover N beyond which the LRU dominates. This benchmark straddles it.
fn bench_engine_bulk_insert_extreme(c: &mut Criterion) {
    let mut group = c.benchmark_group("engine/bulk_insert_extreme");
    group.sample_size(10);

    for n in [20_000_u64, 50_000, 100_000] {
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, &n| {
            b.iter_batched(
                || {
                    let dir = tempfile::tempdir_in("/private/tmp").expect("tempdir");
                    let config = GraphConfig {
                        memory_limit_bytes: 64 * 1024 * 1024,
                        create_if_missing: true,
                        wal_enabled: false,
                        ..GraphConfig::default()
                    };
                    let graph = Graph::open(dir.path(), &config).expect("open");
                    (graph, dir)
                },
                |(mut graph, _dir)| {
                    for i in 0..n {
                        graph
                            .add_node(format!("Event{}", i % 8), Properties::new())
                            .expect("add_node");
                    }
                },
                criterion::BatchSize::PerIteration,
            );
        });
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_touch_cache_hit,
    bench_fill_and_retouch,
    bench_engine_bulk_insert,
    bench_engine_bulk_insert_extreme,
);
criterion_main!(benches);
