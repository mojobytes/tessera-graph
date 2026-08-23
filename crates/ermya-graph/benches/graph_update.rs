// SPDX-License-Identifier: MIT

#[allow(unused)]
mod helpers;

use criterion::{Criterion, criterion_group, criterion_main};
use ermya_graph::{Graph, Properties, Property};

use helpers::small_props;

fn bench_update_node(c: &mut Criterion) {
    let mut group = c.benchmark_group("graph_update/node");

    group.bench_function("same_props", |b| {
        let mut g = Graph::new();
        let id = g.add_node("Person", small_props()).unwrap();
        let node = g.node(id).unwrap();
        b.iter(|| g.update_node(id, &node).unwrap());
    });

    group.bench_function("add_property", |b| {
        let mut g = Graph::new();
        let id = g.add_node("Person", small_props()).unwrap();
        b.iter(|| {
            let mut node = g.node(id).unwrap();
            node.properties_mut()
                .insert("score".into(), Property::F64(99.5));
            g.update_node(id, &node).unwrap();
        });
    });

    group.bench_function("change_label_same_len", |b| {
        let mut g = Graph::new();
        let id = g.add_node("NodeA", Properties::new()).unwrap();
        let node = g.node(id).unwrap();
        b.iter(|| g.update_node(id, &node).unwrap());
    });

    group.finish();
}

fn bench_update_edge(c: &mut Criterion) {
    let mut group = c.benchmark_group("graph_update/edge");

    group.bench_function("add_weight", |b| {
        let mut g = Graph::new();
        let a = g.add_node("A", Properties::new()).unwrap();
        let b_node = g.add_node("B", Properties::new()).unwrap();
        let eid = g.add_edge("KNOWS", a, b_node, Properties::new()).unwrap();
        b.iter(|| {
            let mut edge = g.edge(eid).unwrap();
            edge.properties_mut()
                .insert("weight".into(), Property::F64(1.5));
            g.update_edge(eid, &edge).unwrap();
        });
    });

    group.finish();
}

criterion_group!(benches, bench_update_node, bench_update_edge);
criterion_main!(benches);
