// SPDX-License-Identifier: MIT

use criterion::{Criterion, criterion_group, criterion_main};
use ermya_graph::storage::codec::property_codec::{decode_properties, encode_properties};
use ermya_graph::{Properties, Property};

fn single_string() -> Properties {
    let mut p = Properties::new();
    p.insert("name".into(), Property::String("hello world".into()));
    p
}

fn single_i64() -> Properties {
    let mut p = Properties::new();
    p.insert("count".into(), Property::I64(42));
    p
}

fn mixed_5() -> Properties {
    let mut p = Properties::new();
    p.insert("s".into(), Property::String("value".into()));
    p.insert("i".into(), Property::I64(999));
    p.insert("f".into(), Property::F64(1.5));
    p.insert("b".into(), Property::Bool(true));
    p.insert("d".into(), Property::Bytes(vec![1, 2, 3]));
    p
}

fn large_bytes() -> Properties {
    let mut p = Properties::new();
    p.insert("payload".into(), Property::Bytes(vec![0xAA; 1000]));
    p
}

fn bench_encode(c: &mut Criterion) {
    let mut group = c.benchmark_group("codec_property/encode");

    group.bench_function("single_string", |b| {
        let props = single_string();
        b.iter(|| encode_properties(&props).unwrap());
    });

    group.bench_function("single_i64", |b| {
        let props = single_i64();
        b.iter(|| encode_properties(&props).unwrap());
    });

    group.bench_function("mixed_5", |b| {
        let props = mixed_5();
        b.iter(|| encode_properties(&props).unwrap());
    });

    group.bench_function("large_bytes", |b| {
        let props = large_bytes();
        b.iter(|| encode_properties(&props).unwrap());
    });

    group.finish();
}

fn bench_decode(c: &mut Criterion) {
    let mut group = c.benchmark_group("codec_property/decode");

    group.bench_function("single_string", |b| {
        let props = single_string();
        let encoded = encode_properties(&props).unwrap();
        b.iter(|| decode_properties(&encoded, 1, 0).unwrap());
    });

    group.bench_function("mixed_5", |b| {
        let props = mixed_5();
        let encoded = encode_properties(&props).unwrap();
        b.iter(|| decode_properties(&encoded, 5, 0).unwrap());
    });

    group.finish();
}

fn bench_roundtrip(c: &mut Criterion) {
    c.bench_function("codec_property/roundtrip_mixed", |b| {
        let props = mixed_5();
        b.iter(|| {
            let encoded = encode_properties(&props).unwrap();
            decode_properties(&encoded, 5, 0).unwrap()
        });
    });
}

criterion_group!(benches, bench_encode, bench_decode, bench_roundtrip);
criterion_main!(benches);
