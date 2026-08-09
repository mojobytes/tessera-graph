// SPDX-License-Identifier: MIT

use criterion::{Criterion, criterion_group, criterion_main};
use tessera_graph::storage::codec::node_codec::{decode_node_slot, encode_node_slot, label_hash};
use tessera_graph::{Node, NodeId, Properties, Property};

fn make_node(id: u64, label: &str, props: Properties) -> Node {
    Node::new_for_bench(NodeId::from_raw(id), label, props)
}

fn inline_props() -> Properties {
    let mut p = Properties::new();
    p.insert("name".into(), Property::String("Alice".into()));
    p.insert("age".into(), Property::I64(30));
    p
}

fn bench_encode(c: &mut Criterion) {
    let mut group = c.benchmark_group("codec_node/encode");

    group.bench_function("inline", |b| {
        let node = make_node(42, "Person", inline_props());
        b.iter(|| encode_node_slot(&node).unwrap());
    });

    group.bench_function("overflow_label", |b| {
        let label = "L".repeat(100);
        let node = make_node(42, &label, Properties::new());
        b.iter(|| encode_node_slot(&node).unwrap());
    });

    group.bench_function("overflow_props", |b| {
        let mut props = Properties::new();
        props.insert("data".into(), Property::Bytes(vec![0xAB; 50]));
        let node = make_node(42, "N", props);
        b.iter(|| encode_node_slot(&node).unwrap());
    });

    group.finish();
}

fn bench_decode(c: &mut Criterion) {
    let mut group = c.benchmark_group("codec_node/decode");

    group.bench_function("inline", |b| {
        let node = make_node(42, "Person", inline_props());
        let (slot, _) = encode_node_slot(&node).unwrap();
        b.iter(|| {
            decode_node_slot(
                &slot,
                0,
                |_| panic!("no label resolve"),
                |_| panic!("no props resolve"),
            )
            .unwrap()
        });
    });

    group.finish();
}

fn bench_label_hash(c: &mut Criterion) {
    c.bench_function("codec_node/label_hash", |b| {
        b.iter(|| label_hash("Person"));
    });
}

criterion_group!(benches, bench_encode, bench_decode, bench_label_hash);
criterion_main!(benches);
