// Copyright (c) 2026 BelowZero Security OU. All rights reserved.
// SPDX-License-Identifier: LicenseRef-BelowZero-Enterprise

//! Criterion microbenchmarks: pathfinding (shortest path) scenarios.

use criterion::{BatchSize, BenchmarkId, Criterion, criterion_group, criterion_main};
use tessera_benchmark::dataset::{ChainDataset, Dataset};
use tessera_benchmark::scenario::{PathfindingScenario, Scenario};
use tessera_benchmark::tessera_target::TesseraTarget;

fn bench_shortest_path(c: &mut Criterion) {
    let mut group = c.benchmark_group("harness/pathfinding");
    group.sample_size(10);

    for size in [100usize, 1_000, 5_000] {
        group.bench_with_input(BenchmarkId::from_parameter(size), &size, |b, &size| {
            b.iter_batched(
                || {
                    let mut t = TesseraTarget::new();
                    let ds = ChainDataset { length: size };
                    let result = ds.build(&mut t).unwrap();
                    let from = result.nodes[0];
                    let to = *result.nodes.last().unwrap();
                    (t, from, to)
                },
                |(mut t, from, to)| {
                    PathfindingScenario {
                        from,
                        to,
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

criterion_group!(benches, bench_shortest_path);
criterion_main!(benches);
