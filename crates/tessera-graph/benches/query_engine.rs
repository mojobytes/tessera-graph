// SPDX-License-Identifier: LicenseRef-TesseraGraph-Proprietary

#[allow(unused)]
mod helpers;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use tessera_graph::{Direction, Edge, Graph, Properties, Property, props};

use helpers::{binary_tree_graph, chain_graph_with_ids, grid_graph, star_graph};

// ---------------------------------------------------------------
// NeighborQuery
// ---------------------------------------------------------------

fn bench_neighbor_query(c: &mut Criterion) {
    let mut group = c.benchmark_group("query/neighbors");

    for degree in [10, 100, 1_000] {
        group.bench_with_input(
            BenchmarkId::new("outgoing", degree),
            &degree,
            |b, &degree| {
                let (g, center) = star_graph(degree);
                b.iter(|| g.neighbors(center).direction(Direction::Outgoing).collect().unwrap());
            },
        );

        group.bench_with_input(
            BenchmarkId::new("with_label_filter", degree),
            &degree,
            |b, &degree| {
                let (g, center) = star_graph(degree);
                b.iter(|| {
                    g.neighbors(center)
                        .direction(Direction::Outgoing)
                        .label("CONNECTS")
                        .collect()
                        .unwrap()
                });
            },
        );

        group.bench_with_input(
            BenchmarkId::new("node_ids", degree),
            &degree,
            |b, &degree| {
                let (g, center) = star_graph(degree);
                b.iter(|| {
                    g.neighbors(center)
                        .direction(Direction::Outgoing)
                        .node_ids()
                        .unwrap()
                });
            },
        );
    }

    group.finish();
}

// ---------------------------------------------------------------
// BFS / DFS Traversal
// ---------------------------------------------------------------

fn bench_traversal_chain(c: &mut Criterion) {
    let mut group = c.benchmark_group("query/traversal/chain");
    group.sample_size(20);

    for size in [100, 1_000, 10_000] {
        let (g, ids) = chain_graph_with_ids(size);
        let start = ids[0];

        group.bench_with_input(BenchmarkId::new("bfs", size), &size, |b, _| {
            b.iter(|| {
                g.traverse(start)
                    .direction(Direction::Outgoing)
                    .bfs()
                    .collect()
                    .unwrap()
            });
        });

        group.bench_with_input(BenchmarkId::new("dfs", size), &size, |b, _| {
            b.iter(|| {
                g.traverse(start)
                    .direction(Direction::Outgoing)
                    .dfs()
                    .collect()
                    .unwrap()
            });
        });
    }

    group.finish();
}

fn bench_traversal_tree(c: &mut Criterion) {
    let mut group = c.benchmark_group("query/traversal/tree");
    group.sample_size(20);

    // depth 7 = 127 nodes, depth 10 = 1023 nodes, depth 14 = 16383 nodes
    for depth in [7, 10, 14] {
        let (g, root) = binary_tree_graph(depth);
        let node_count = (1_usize << depth) - 1;

        group.bench_with_input(
            BenchmarkId::new("bfs", node_count),
            &depth,
            |b, _| {
                b.iter(|| {
                    g.traverse(root)
                        .direction(Direction::Outgoing)
                        .bfs()
                        .collect()
                        .unwrap()
                });
            },
        );

        group.bench_with_input(
            BenchmarkId::new("dfs", node_count),
            &depth,
            |b, _| {
                b.iter(|| {
                    g.traverse(root)
                        .direction(Direction::Outgoing)
                        .dfs()
                        .collect()
                        .unwrap()
                });
            },
        );
    }

    group.finish();
}

fn bench_traversal_max_depth(c: &mut Criterion) {
    let mut group = c.benchmark_group("query/traversal/max_depth");

    let (g, root) = binary_tree_graph(14); // 16383 nodes

    for max_depth in [2, 5, 8, 12] {
        group.bench_with_input(
            BenchmarkId::from_parameter(max_depth),
            &max_depth,
            |b, &md| {
                b.iter(|| {
                    g.traverse(root)
                        .direction(Direction::Outgoing)
                        .max_depth(md)
                        .bfs()
                        .collect()
                        .unwrap()
                });
            },
        );
    }

    group.finish();
}

fn bench_traversal_collect_paths(c: &mut Criterion) {
    let mut group = c.benchmark_group("query/traversal/collect_paths");
    group.sample_size(20);

    for size in [100, 1_000] {
        let (g, ids) = chain_graph_with_ids(size);
        let start = ids[0];

        group.bench_with_input(BenchmarkId::new("bfs", size), &size, |b, _| {
            b.iter(|| {
                g.traverse(start)
                    .direction(Direction::Outgoing)
                    .bfs()
                    .collect_paths()
                    .unwrap()
            });
        });
    }

    group.finish();
}

// ---------------------------------------------------------------
// Shortest Path (BFS unweighted)
// ---------------------------------------------------------------

fn bench_shortest_path(c: &mut Criterion) {
    let mut group = c.benchmark_group("query/shortest_path");
    group.sample_size(20);

    // Chain: worst-case BFS (must traverse full chain)
    for size in [100, 1_000, 10_000] {
        let (g, ids) = chain_graph_with_ids(size);
        let start = ids[0];
        let end = ids[size - 1];

        group.bench_with_input(BenchmarkId::new("chain_full", size), &size, |b, _| {
            b.iter(|| {
                g.shortest_path(start, end)
                    .direction(Direction::Outgoing)
                    .find()
                    .unwrap()
            });
        });
    }

    // Grid: BFS finds shortest path across diagonal
    for side in [10, 30, 50] {
        let (g, matrix) = grid_graph(side, side);
        let start = matrix[0][0];
        let end = matrix[side - 1][side - 1];
        let total_nodes = side * side;

        group.bench_with_input(
            BenchmarkId::new("grid", total_nodes),
            &side,
            |b, _| {
                b.iter(|| {
                    g.shortest_path(start, end)
                        .direction(Direction::Outgoing)
                        .find()
                        .unwrap()
                });
            },
        );
    }

    // Same node — trivial case
    {
        let (g, ids) = chain_graph_with_ids(1_000);
        let node = ids[500];
        group.bench_function("same_node", |b| {
            b.iter(|| g.shortest_path(node, node).find().unwrap());
        });
    }

    group.finish();
}

// ---------------------------------------------------------------
// Dijkstra (weighted shortest path)
// ---------------------------------------------------------------

fn edge_cost(edge: &Edge) -> f64 {
    match edge.properties().get("cost") {
        Some(Property::F64(v)) => *v,
        _ => 1.0,
    }
}

fn bench_dijkstra(c: &mut Criterion) {
    let mut group = c.benchmark_group("query/dijkstra");
    group.sample_size(20);

    // Weighted chain
    for size in [100, 1_000] {
        let mut g = Graph::new();
        let mut ids = Vec::with_capacity(size);
        for _ in 0..size {
            ids.push(g.add_node("N", Properties::new()).unwrap());
        }
        for pair in ids.windows(2) {
            g.add_edge("R", pair[0], pair[1], props! { "cost" => 1.5 })
                .unwrap();
        }

        let start = ids[0];
        let end = ids[size - 1];

        group.bench_with_input(BenchmarkId::new("chain", size), &size, |b, _| {
            b.iter(|| {
                g.weighted_shortest_path(start, end)
                    .direction(Direction::Outgoing)
                    .weight(edge_cost)
                    .find()
                    .unwrap()
            });
        });
    }

    // Dijkstra vs BFS on same graph (unit weights)
    {
        let (g, ids) = chain_graph_with_ids(1_000);
        let start = ids[0];
        let end = ids[999];

        group.bench_function("vs_bfs_unweighted/dijkstra", |b| {
            b.iter(|| {
                g.weighted_shortest_path(start, end)
                    .direction(Direction::Outgoing)
                    .find()
                    .unwrap()
            });
        });

        group.bench_function("vs_bfs_unweighted/bfs", |b| {
            b.iter(|| {
                g.shortest_path(start, end)
                    .direction(Direction::Outgoing)
                    .find()
                    .unwrap()
            });
        });
    }

    group.finish();
}

// ---------------------------------------------------------------
// Subgraph extraction
// ---------------------------------------------------------------

fn bench_subgraph(c: &mut Criterion) {
    let mut group = c.benchmark_group("query/subgraph");
    group.sample_size(20);

    for depth in [7, 10, 14] {
        let (g, root) = binary_tree_graph(depth);
        let node_count = (1_usize << depth) - 1;

        group.bench_with_input(
            BenchmarkId::new("full_tree", node_count),
            &depth,
            |b, _| {
                b.iter(|| {
                    g.subgraph(root)
                        .direction(Direction::Outgoing)
                        .extract()
                        .unwrap()
                });
            },
        );

        group.bench_with_input(
            BenchmarkId::new("depth_limited_3", node_count),
            &depth,
            |b, _| {
                b.iter(|| {
                    g.subgraph(root)
                        .direction(Direction::Outgoing)
                        .max_depth(3)
                        .extract()
                        .unwrap()
                });
            },
        );
    }

    // With label filter
    {
        let (g, root) = binary_tree_graph(10);
        group.bench_function("label_filter/1023_nodes", |b| {
            b.iter(|| {
                g.subgraph(root)
                    .direction(Direction::Outgoing)
                    .label("CHILD")
                    .extract()
                    .unwrap()
            });
        });
    }

    group.finish();
}

// ---------------------------------------------------------------
// Criterion groups
// ---------------------------------------------------------------

criterion_group!(
    benches,
    bench_neighbor_query,
    bench_traversal_chain,
    bench_traversal_tree,
    bench_traversal_max_depth,
    bench_traversal_collect_paths,
    bench_shortest_path,
    bench_dijkstra,
    bench_subgraph,
);
criterion_main!(benches);
