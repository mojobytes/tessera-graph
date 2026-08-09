// SPDX-License-Identifier: MIT

#[allow(unused)]
mod helpers;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use tessera_graph::{Graph, Properties};

use helpers::{reverse_star_graph, small_props, star_graph};

fn bench_node_lookup(c: &mut Criterion) {
    let mut group = c.benchmark_group("graph_read/node_lookup");

    // Verify O(1) lookup across different graph sizes
    for size in [1_000, 10_000, 100_000, 1_000_000] {
        let mut g = Graph::new();
        let mut ids = Vec::with_capacity(size);
        for _ in 0..size {
            ids.push(g.add_node("Person", small_props()).unwrap());
        }

        let middle = ids[size / 2];

        group.bench_with_input(BenchmarkId::new("mid", size), &size, |b, _| {
            b.iter(|| g.node(middle).unwrap());
        });
    }

    group.finish();
}

fn bench_edge_lookup(c: &mut Criterion) {
    let mut group = c.benchmark_group("graph_read/edge_lookup");

    // Chain topology: N edges between N+1 nodes
    for size in [1_000, 10_000, 100_000] {
        let mut g = Graph::new();
        let mut nodes = Vec::with_capacity(size + 1);
        for _ in 0..=size {
            nodes.push(g.add_node("N", Properties::new()).unwrap());
        }
        let mut edge_ids = Vec::with_capacity(size);
        for pair in nodes.windows(2) {
            edge_ids.push(g.add_edge("NEXT", pair[0], pair[1], small_props()).unwrap());
        }

        let mid_eid = edge_ids[size / 2];

        group.bench_with_input(BenchmarkId::new("mid", size), &size, |b, _| {
            b.iter(|| g.edge(mid_eid).unwrap());
        });
    }

    group.finish();
}

fn bench_outgoing_edges(c: &mut Criterion) {
    let mut group = c.benchmark_group("graph_read/outgoing_edges");

    for degree in [5, 10, 100, 500, 1_000] {
        group.bench_with_input(
            BenchmarkId::from_parameter(degree),
            &degree,
            |b, &degree| {
                let (g, center) = star_graph(degree);
                b.iter(|| g.outgoing_edges(center).unwrap());
            },
        );
    }

    group.finish();
}

fn bench_incoming_edges(c: &mut Criterion) {
    let mut group = c.benchmark_group("graph_read/incoming_edges");

    for degree in [5, 10, 100, 500, 1_000] {
        group.bench_with_input(
            BenchmarkId::from_parameter(degree),
            &degree,
            |b, &degree| {
                let (g, center) = reverse_star_graph(degree);
                b.iter(|| g.incoming_edges(center).unwrap());
            },
        );
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_node_lookup,
    bench_edge_lookup,
    bench_outgoing_edges,
    bench_incoming_edges,
);
criterion_main!(benches);
