//! Criterion microbenchmarks: traversal scenarios.

use criterion::{BatchSize, BenchmarkId, Criterion, criterion_group, criterion_main};
use tessera_graph_benchmark::dataset::{ChainDataset, Dataset};
use tessera_graph_benchmark::scenario::{Scenario, TraversalScenario};
use tessera_graph_benchmark::tessera_target::TesseraTarget;

fn bench_bfs_traversal(c: &mut Criterion) {
    let mut group = c.benchmark_group("harness/traversal/bfs");
    group.sample_size(20);

    for size in [100usize, 1_000, 5_000] {
        group.bench_with_input(BenchmarkId::from_parameter(size), &size, |b, &size| {
            b.iter_batched(
                || {
                    let mut t = TesseraTarget::new();
                    let ds = ChainDataset { length: size };
                    let result = ds.build(&mut t).unwrap();
                    (t, result.nodes[0])
                },
                |(mut t, start)| {
                    TraversalScenario {
                        start,
                        max_depth: size as u32,
                        iterations: 1,
                    }
                    .run(&mut t)
                    .unwrap()
                },
                BatchSize::LargeInput,
            );
        });
    }

    group.finish();
}

criterion_group!(benches, bench_bfs_traversal);
criterion_main!(benches);
