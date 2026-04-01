//! Criterion microbenchmarks: write scenarios.

use criterion::{BatchSize, BenchmarkId, Criterion, criterion_group, criterion_main};
use tessera_graph_benchmark::scenario::{Scenario, WriteScenario};
use tessera_graph_benchmark::tessera_target::TesseraTarget;

fn bench_bulk_write(c: &mut Criterion) {
    let mut group = c.benchmark_group("harness/write");
    group.sample_size(10);

    for n in [100usize, 1_000, 10_000] {
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, &n| {
            b.iter_batched(
                TesseraTarget::new,
                |mut t| {
                    WriteScenario {
                        node_count: n,
                        edge_count: n.saturating_sub(1),
                    }
                    .run(&mut t)
                    .unwrap()
                },
                BatchSize::SmallInput,
            );
        });
    }

    group.finish();
}

criterion_group!(benches, bench_bulk_write);
criterion_main!(benches);
