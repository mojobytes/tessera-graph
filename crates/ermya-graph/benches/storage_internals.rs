// SPDX-License-Identifier: MIT

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use ermya_graph::storage::backend::{DataFile, StorageBackend};
use ermya_graph::storage::codec::adjacency_codec::{
    AdjDirection, AdjacencyRecord, read_adjacency, write_adjacency,
};
use ermya_graph::storage::codec::overflow_codec::{read_overflow, write_overflow};
use ermya_graph::storage::codec::string_codec::StringHeap;
use ermya_graph::storage::memory::MemoryBackend;
use ermya_graph::storage::page::new_page_buf;

fn bench_string_heap(c: &mut Criterion) {
    let mut group = c.benchmark_group("storage/string_heap");

    group.bench_function("append_unique", |b| {
        let mut backend = MemoryBackend::new();
        let mut heap = StringHeap::new();
        let mut counter = 0u64;
        b.iter(|| {
            let s = format!("string_{counter}");
            counter += 1;
            heap.append(&mut backend, &s).unwrap()
        });
    });

    group.bench_function("append_dedup", |b| {
        let mut backend = MemoryBackend::new();
        let mut heap = StringHeap::new();
        heap.append(&mut backend, "cached_string").unwrap();
        b.iter(|| heap.append(&mut backend, "cached_string").unwrap());
    });

    group.bench_function("resolve", |b| {
        let mut backend = MemoryBackend::new();
        let mut heap = StringHeap::new();
        let r = heap.append(&mut backend, "hello world").unwrap();
        b.iter(|| heap.resolve(&backend, r).unwrap());
    });

    group.finish();
}

fn bench_overflow(c: &mut Criterion) {
    let mut group = c.benchmark_group("storage/overflow");

    group.bench_function("write_100B", |b| {
        let mut backend = MemoryBackend::new();
        let data = vec![0xAB; 100];
        b.iter(|| write_overflow(&mut backend, &data).unwrap());
    });

    group.bench_function("write_10KB", |b| {
        let mut backend = MemoryBackend::new();
        let data = vec![0xAB; 10_000];
        b.iter(|| write_overflow(&mut backend, &data).unwrap());
    });

    group.bench_function("read_100B", |b| {
        let mut backend = MemoryBackend::new();
        let data = vec![0xAB; 100];
        let page_id = write_overflow(&mut backend, &data).unwrap();
        b.iter(|| read_overflow(&backend, page_id).unwrap());
    });

    group.bench_function("read_10KB", |b| {
        let mut backend = MemoryBackend::new();
        let data = vec![0xAB; 10_000];
        let page_id = write_overflow(&mut backend, &data).unwrap();
        b.iter(|| read_overflow(&backend, page_id).unwrap());
    });

    group.finish();
}

fn bench_adjacency(c: &mut Criterion) {
    let mut group = c.benchmark_group("storage/adjacency");

    for count in [10_u64, 100, 500] {
        group.bench_with_input(BenchmarkId::new("write", count), &count, |b, &count| {
            let mut backend = MemoryBackend::new();
            let record = AdjacencyRecord {
                node_id: 1,
                direction: AdjDirection::Outgoing,
                edge_ids: (1..=count).collect(),
            };
            b.iter(|| write_adjacency(&mut backend, &record).unwrap());
        });
    }

    for count in [10_u64, 100, 500] {
        group.bench_with_input(BenchmarkId::new("read", count), &count, |b, &count| {
            let mut backend = MemoryBackend::new();
            let record = AdjacencyRecord {
                node_id: 1,
                direction: AdjDirection::Outgoing,
                edge_ids: (1..=count).collect(),
            };
            let page_id = write_adjacency(&mut backend, &record).unwrap();
            b.iter(|| read_adjacency(&backend, page_id).unwrap());
        });
    }

    group.finish();
}

fn bench_memory_backend(c: &mut Criterion) {
    let mut group = c.benchmark_group("storage/memory_backend");

    group.bench_function("allocate_page", |b| {
        let mut backend = MemoryBackend::new();
        b.iter(|| backend.allocate_page(DataFile::Nodes).unwrap());
    });

    group.bench_function("write_read", |b| {
        let mut backend = MemoryBackend::new();
        let page_id = backend.allocate_page(DataFile::Nodes).unwrap();
        let buf = new_page_buf();
        b.iter(|| {
            backend.write_page(DataFile::Nodes, page_id, &buf).unwrap();
            backend.read_page(DataFile::Nodes, page_id).unwrap()
        });
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_string_heap,
    bench_overflow,
    bench_adjacency,
    bench_memory_backend,
);
criterion_main!(benches);
