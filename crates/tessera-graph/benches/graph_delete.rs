// SPDX-License-Identifier: LicenseRef-TesseraGraph-Proprietary

#[allow(unused)]
mod helpers;

use criterion::{BenchmarkId, Criterion, BatchSize, criterion_group, criterion_main};
use tessera_graph::{Graph, Properties};

fn bench_remove_edge(c: &mut Criterion) {
    let mut group = c.benchmark_group("graph_delete/edge");

    group.bench_function("simple", |b| {
        b.iter_batched(
            || {
                let mut g = Graph::new();
                let a = g.add_node("A", Properties::new()).unwrap();
                let b_node = g.add_node("B", Properties::new()).unwrap();
                let eid = g.add_edge("R", a, b_node, Properties::new()).unwrap();
                (g, eid)
            },
            |(mut g, eid)| g.remove_edge(eid).unwrap(),
            BatchSize::SmallInput,
        );
    });

    group.finish();
}

fn bench_remove_node(c: &mut Criterion) {
    let mut group = c.benchmark_group("graph_delete/node");

    group.bench_function("no_edges", |b| {
        b.iter_batched(
            || {
                let mut g = Graph::new();
                let id = g.add_node("N", Properties::new()).unwrap();
                (g, id)
            },
            |(mut g, id)| g.remove_node(id).unwrap(),
            BatchSize::SmallInput,
        );
    });

    for degree in [5, 10, 50, 100] {
        group.bench_with_input(
            BenchmarkId::new("cascade", degree),
            &degree,
            |b, &degree| {
                b.iter_batched(
                    || {
                        let (g, center) = helpers::star_graph(degree);
                        (g, center)
                    },
                    |(mut g, center)| g.remove_node(center).unwrap(),
                    BatchSize::SmallInput,
                );
            },
        );
    }

    group.finish();
}

criterion_group!(benches, bench_remove_edge, bench_remove_node);
criterion_main!(benches);
