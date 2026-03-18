// Copyright (c) 2026 BelowZero Security OU. All rights reserved.
// SPDX-License-Identifier: LicenseRef-BelowZero-Enterprise

//! Criterion microbenchmarks: read (point-lookup) scenarios.

use criterion::{BatchSize, BenchmarkId, Criterion, criterion_group, criterion_main};
use tessera_benchmark::dataset::{ChainDataset, Dataset};
use tessera_benchmark::scenario::{ReadScenario, Scenario};
use tessera_benchmark::tessera_target::TesseraTarget;

fn bench_point_lookups(c: &mut Criterion) {
    let mut group = c.benchmark_group("harness/read");
    group.sample_size(10);

    for lookups in [100usize, 1_000, 10_000] {
        group.bench_with_input(BenchmarkId::from_parameter(lookups), &lookups, |b, &lookups| {
            b.iter_batched(
                || {
                    let mut t = TesseraTarget::new();
                    let ds = ChainDataset { length: 1_000 };
                    let result = ds.build(&mut t).unwrap();
                    (t, result.nodes)
                },
                |(mut t, handles)| {
                    ReadScenario {
                        node_handles: handles,
                        lookup_iterations: lookups,
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

criterion_group!(benches, bench_point_lookups);
criterion_main!(benches);
