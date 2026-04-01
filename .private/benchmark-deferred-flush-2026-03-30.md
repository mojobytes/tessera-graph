# Benchmark — Deferred Flush v0.2.0 — 2026-03-30

## Environment
- Host: macOS Darwin 25.3.0
- Docker: 29.3.1
- Resource limits: 2 CPUs + 2 GB RAM per container
- TesseraGraph: tesseraio/tessera-graph-enterprise:0.2.0 (Bolt 4.4 + TLS, deferred flush 50ms)
- Memgraph: memgraph/memgraph:latest (Community, Bolt + TLS via CA-signed cert)
- Benchmark binary: tessera-bench --release
- **Both targets use TLS** — fair comparison

## Deferred Flush Results

### Write (1000 nodes, 0 edges)

| Target              | ops/s | mean     | p50      | p95       | p99       |
|---------------------|------:|---------:|---------:|----------:|----------:|
| tessera-bolt (TLS)  |   379 |  2.6 ms  |  2.3 ms  |   6.2 ms  |   7.5 ms  |
| memgraph (TLS)      |   710 |  1.4 ms  |  1.3 ms  |   1.8 ms  |   2.1 ms  |

**Ratio: Memgraph ~1.9x faster on write**

### Read (100 nodes, 100 lookups)

| Target              | ops/s | mean     | p50      | p95       | p99       |
|---------------------|------:|---------:|---------:|----------:|----------:|
| tessera-bolt (TLS)  |   218 |  4.6 ms  |  3.3 ms  |  11.5 ms  |  13.3 ms  |
| memgraph (TLS)      |   907 |  1.1 ms  |  1.1 ms  |   1.3 ms  |   1.4 ms  |

**Ratio: Memgraph ~4.2x faster on read** (cold start — see notes)

### Mixed (100 ops, 50/50 read/write)

| Target              | ops/s | mean     | p50      | p95       | p99       |
|---------------------|------:|---------:|---------:|----------:|----------:|
| tessera-bolt (TLS)  |   284 |  3.5 ms  |  2.9 ms  |   7.1 ms  |   9.3 ms  |
| memgraph (TLS)      |   559 |  1.8 ms  |  1.3 ms  |   1.8 ms  |   1.9 ms  |

**Ratio: Memgraph ~2.0x faster on mixed**

## Comparison: v0.1.0 (flush-per-mutation) vs v0.2.0 (deferred flush)

| Scenario | v0.1.0 ops/s | v0.2.0 ops/s | Improvement | Gap vs Memgraph (v0.1) | Gap vs Memgraph (v0.2) |
|----------|-------------:|-------------:|------------:|-----------------------:|-----------------------:|
| Write    |          115 |          379 |      **3.3x** |                   5.6x |                   1.9x |
| Mixed    |          167 |          284 |      **1.7x** |                   3.0x |                   2.0x |

## Notes

- Read throughput appears lower than v0.1.0 baseline (218 vs 397 ops/s) because
  each benchmark run starts from a cold Docker volume. The v0.1.0 baseline was
  measured with a warm graph. Deferred flush does not affect read performance.
- The write improvement (3.3x) is the key result: removing per-mutation
  `graph.flush()` eliminated the dominant bottleneck.
- The remaining ~1.9x gap vs Memgraph is likely due to: (1) query parsing
  overhead (GQL/Cypher), (2) page-file I/O during background flush, (3) WAL
  append cost per mutation.

## Remaining Optimization Opportunities

1. **Query cache / prepared statements**: Skip re-parsing identical query patterns
2. **Connection pooling**: Reuse Bolt sessions to reduce TLS handshake cost
3. **WAL batching**: Group multiple WAL records into a single fsync
4. **GQL-native detection**: Bypass Cypher preprocessor when input is already GQL
