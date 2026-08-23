// SPDX-License-Identifier: MIT

use criterion::{Criterion, criterion_group, criterion_main};
use ermya_graph::storage::codec::edge_codec::{decode_edge_slot, encode_edge_slot};
use ermya_graph::{Edge, EdgeId, NodeId, Properties, Property};

fn make_edge(id: u64, label: &str, source: u64, target: u64, props: Properties) -> Edge {
    Edge::new_for_bench(
        EdgeId::from_raw(id),
        label,
        NodeId::from_raw(source),
        NodeId::from_raw(target),
        props,
    )
}

fn inline_props() -> Properties {
    let mut p = Properties::new();
    p.insert("w".into(), Property::I64(10));
    p
}

fn bench_encode(c: &mut Criterion) {
    let mut group = c.benchmark_group("codec_edge/encode");

    group.bench_function("inline", |b| {
        let edge = make_edge(1, "KNOWS", 10, 20, inline_props());
        b.iter(|| encode_edge_slot(&edge).unwrap());
    });

    group.bench_function("overflow_label", |b| {
        let label = "E".repeat(100);
        let edge = make_edge(1, &label, 10, 20, Properties::new());
        b.iter(|| encode_edge_slot(&edge).unwrap());
    });

    group.bench_function("overflow_props", |b| {
        let mut props = Properties::new();
        props.insert("data".into(), Property::Bytes(vec![0xCD; 50]));
        let edge = make_edge(1, "R", 10, 20, props);
        b.iter(|| encode_edge_slot(&edge).unwrap());
    });

    group.finish();
}

fn bench_decode(c: &mut Criterion) {
    let mut group = c.benchmark_group("codec_edge/decode");

    group.bench_function("inline", |b| {
        let edge = make_edge(1, "KNOWS", 10, 20, inline_props());
        let (slot, _) = encode_edge_slot(&edge).unwrap();
        b.iter(|| {
            decode_edge_slot(
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

criterion_group!(benches, bench_encode, bench_decode);
criterion_main!(benches);
