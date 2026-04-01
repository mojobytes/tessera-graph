// Copyright (c) 2026 BelowZero Security OU. All rights reserved.
// SPDX-License-Identifier: LicenseRef-BelowZero-Enterprise

//! Criterion microbenchmarks: mixed (interleaved read/write) scenarios.

use criterion::{BatchSize, BenchmarkId, Criterion, criterion_group, criterion_main};
use tessera_graph_benchmark::scenario::{MixedScenario, Scenario};
use tessera_graph_benchmark::tessera_target::TesseraTarget;

fn bench_mixed_workload(c: &mut Criterion) {
    let mut group = c.benchmark_group("harness/mixed");
    group.sample_size(10);

    for write_ratio in [0.2_f64, 0.5, 0.8] {
        let label = format!("{:.0}pct_write", write_ratio * 100.0);
        group.bench_with_input(BenchmarkId::new("ratio", &label), &write_ratio, |b, &wr| {
            b.iter_batched(
                TesseraTarget::new,
                |mut t| {
                    MixedScenario {
                        write_ratio: wr,
                        total_ops: 1_000,
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

criterion_group!(benches, bench_mixed_workload);
criterion_main!(benches);
