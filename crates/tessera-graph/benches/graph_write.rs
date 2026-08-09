// SPDX-License-Identifier: MIT

#[allow(unused)]
mod helpers;

use criterion::{BatchSize, BenchmarkId, Criterion, criterion_group, criterion_main};
use tessera_graph::{Graph, Properties};

use helpers::{large_props, small_props};

fn bench_add_node(c: &mut Criterion) {
    let mut group = c.benchmark_group("graph_write/add_node");

    group.bench_function("minimal", |b| {
        b.iter_batched(
            Graph::new,
            |mut g| g.add_node("N", Properties::new()).unwrap(),
            BatchSize::SmallInput,
        );
    });

    group.bench_function("small_props", |b| {
        b.iter_batched(
            Graph::new,
            |mut g| g.add_node("Person", small_props()).unwrap(),
            BatchSize::SmallInput,
        );
    });

    group.bench_function("large_props", |b| {
        b.iter_batched(
            Graph::new,
            |mut g| g.add_node("Entity", large_props()).unwrap(),
            BatchSize::SmallInput,
        );
    });

    group.bench_function("overflow_label", |b| {
        let label = "L".repeat(100);
        b.iter_batched(
            Graph::new,
            |mut g| g.add_node(&label, Properties::new()).unwrap(),
            BatchSize::SmallInput,
        );
    });

    group.finish();
}

fn bench_bulk_add_nodes(c: &mut Criterion) {
    let mut group = c.benchmark_group("graph_write/bulk_nodes");
    group.sample_size(10);

    for n in [1_000, 10_000, 100_000, 1_000_000] {
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, &n| {
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

fn bench_add_edge(c: &mut Criterion) {
    let mut group = c.benchmark_group("graph_write/add_edge");

    group.bench_function("minimal", |b| {
        b.iter_batched(
            || {
                let mut g = Graph::new();
                let a = g.add_node("A", Properties::new()).unwrap();
                let b = g.add_node("B", Properties::new()).unwrap();
                (g, a, b)
            },
            |(mut g, a, b)| g.add_edge("R", a, b, Properties::new()).unwrap(),
            BatchSize::SmallInput,
        );
    });

    group.bench_function("with_props", |b| {
        b.iter_batched(
            || {
                let mut g = Graph::new();
                let a = g.add_node("A", Properties::new()).unwrap();
                let b = g.add_node("B", Properties::new()).unwrap();
                (g, a, b)
            },
            |(mut g, a, b)| g.add_edge("KNOWS", a, b, small_props()).unwrap(),
            BatchSize::SmallInput,
        );
    });

    group.finish();
}

fn bench_bulk_add_edges(c: &mut Criterion) {
    let mut group = c.benchmark_group("graph_write/bulk_edges");
    group.sample_size(10);

    // Chain topology: N+1 nodes, N edges (node_i -> node_{i+1}).
    // Each node has at most 1 outgoing + 1 incoming edge, so adjacency
    // lists stay small — this measures edge write throughput, not
    // adjacency list growth.
    for n in [1_000, 10_000, 100_000, 1_000_000] {
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, &n| {
            b.iter_batched(
                || {
                    let mut g = Graph::new();
                    let mut nodes = Vec::with_capacity(n + 1);
                    for _ in 0..=n {
                        nodes.push(g.add_node("N", Properties::new()).unwrap());
                    }
                    (g, nodes)
                },
                |(mut g, nodes)| {
                    for pair in nodes.windows(2) {
                        g.add_edge("NEXT", pair[0], pair[1], Properties::new())
                            .unwrap();
                    }
                },
                BatchSize::LargeInput,
            );
        });
    }

    group.finish();
}

/// Star topology: one source node with N outgoing edges.
/// This is the typical MATCH...CREATE pattern where one node fans out to many.
/// Adjacency list for the source grows with each edge — measures adjacency
/// page read/write overhead under accumulation.
fn bench_bulk_add_edges_star(c: &mut Criterion) {
    let mut group = c.benchmark_group("graph_write/bulk_edges_star");
    group.sample_size(10);

    for n in [1_000, 10_000, 100_000] {
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, &n| {
            b.iter_batched(
                || {
                    let mut g = Graph::new();
                    let center = g.add_node("Person", small_props()).unwrap();
                    let mut targets = Vec::with_capacity(n);
                    for i in 0..n {
                        let mut p = Properties::new();
                        // allow: test fixture
                        #[allow(clippy::cast_possible_wrap)]
                        p.insert("id".into(), tessera_graph::Property::I64(i as i64));
                        targets.push(g.add_node("Person", p).unwrap());
                    }
                    (g, center, targets)
                },
                |(mut g, center, targets)| {
                    for &t in &targets {
                        g.add_edge("KNOWS", center, t, Properties::new()).unwrap();
                    }
                },
                BatchSize::LargeInput,
            );
        });
    }

    group.finish();
}

/// Same star topology but with batch mode enabled (deferred WAL sync).
/// This is the path used by the server with implicit batching.
fn bench_bulk_add_edges_star_batched(c: &mut Criterion) {
    let mut group = c.benchmark_group("graph_write/bulk_edges_star_batched");
    group.sample_size(10);

    for n in [1_000, 10_000, 100_000] {
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, &n| {
            b.iter_batched(
                || {
                    let mut g = Graph::new();
                    let center = g.add_node("Person", small_props()).unwrap();
                    let mut targets = Vec::with_capacity(n);
                    for i in 0..n {
                        let mut p = Properties::new();
                        // allow: test fixture
                        #[allow(clippy::cast_possible_wrap)]
                        p.insert("id".into(), tessera_graph::Property::I64(i as i64));
                        targets.push(g.add_node("Person", p).unwrap());
                    }
                    (g, center, targets)
                },
                |(mut g, center, targets)| {
                    g.begin_batch();
                    for &t in &targets {
                        g.add_edge("KNOWS", center, t, Properties::new()).unwrap();
                    }
                    g.end_batch().unwrap();
                },
                BatchSize::LargeInput,
            );
        });
    }

    group.finish();
}

/// Measures the combined MATCH lookup + edge creation pattern:
/// property index lookup to find source/target, then `add_edge`.
/// Simulates the server-side MATCH...CREATE without protocol overhead.
fn bench_match_create_pattern(c: &mut Criterion) {
    let mut group = c.benchmark_group("graph_write/match_create_pattern");
    group.sample_size(10);

    for n in [1_000, 10_000] {
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, &n| {
            b.iter_batched(
                || {
                    let mut g = Graph::new();
                    // Create source node
                    let mut sp = Properties::new();
                    sp.insert(
                        "name".into(),
                        tessera_graph::Property::String("Alice".into()),
                    );
                    g.add_node("Person", sp).unwrap();
                    // Create target nodes with unique names
                    for i in 0..n {
                        let mut p = Properties::new();
                        p.insert(
                            "name".into(),
                            tessera_graph::Property::String(format!("T{i}")),
                        );
                        g.add_node("Person", p).unwrap();
                    }
                    g
                },
                |mut g| {
                    g.begin_batch();
                    // Simulate MATCH (a:Person {name:'Alice'})
                    let sources = g.nodes_by_label_and_property(
                        "Person",
                        "name",
                        &tessera_graph::Property::String("Alice".into()),
                    );
                    let src = sources[0];
                    // For each target, do property index lookup + add_edge
                    for i in 0..n {
                        let targets = g.nodes_by_label_and_property(
                            "Person",
                            "name",
                            &tessera_graph::Property::String(format!("T{i}")),
                        );
                        let tgt = targets[0];
                        g.add_edge("KNOWS", src, tgt, Properties::new()).unwrap();
                    }
                    g.end_batch().unwrap();
                },
                BatchSize::LargeInput,
            );
        });
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_add_node,
    bench_bulk_add_nodes,
    bench_add_edge,
    bench_bulk_add_edges,
    bench_bulk_add_edges_star,
    bench_bulk_add_edges_star_batched,
    bench_match_create_pattern,
);
criterion_main!(benches);
