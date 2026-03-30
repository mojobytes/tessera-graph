# Benchmark Baseline — 2026-03-30

## Environment
- Host: macOS Darwin 25.3.0
- Docker: 29.3.1
- Resource limits: 2 CPUs + 2 GB RAM per container
- TesseraGraph: tesseraio/tessera-graph-enterprise:latest (Bolt 4.4 + TLS)
- Memgraph: memgraph/memgraph:latest (Community, Bolt + TLS via CA-signed cert)
- TesseraGraph ports: 7687 (Bolt+TLS), 9090 (Prometheus)
- Memgraph ports: 7688→7687 (Bolt+TLS)
- Benchmark binary: tessera-bench --release
- **Both targets use TLS** — fair comparison

## Bolt+TLS Comparison: TesseraGraph vs Memgraph

### Write (1000 nodes, 0 edges)

| Target              | ops/s | mean     | p50      | p95       | p99       |
|---------------------|------:|---------:|---------:|----------:|----------:|
| tessera-bolt (TLS)  |   115 |  8.6 ms  |  8.3 ms  |  11.3 ms  |  17.0 ms  |
| memgraph (TLS)      |   644 |  1.6 ms  |  1.3 ms  |   2.2 ms  |   3.9 ms  |

**Ratio: Memgraph ~5.6x faster on write**

### Read (100 nodes, 100 lookups)

| Target              | ops/s | mean     | p50      | p95       | p99       |
|---------------------|------:|---------:|---------:|----------:|----------:|
| tessera-bolt (TLS)  |   397 |  2.5 ms  |  2.4 ms  |   3.1 ms  |   3.8 ms  |
| memgraph (TLS)      |   718 |  1.4 ms  |  1.2 ms  |   2.1 ms  |   3.6 ms  |

**Ratio: Memgraph ~1.8x faster on read**

### Mixed (100 ops, 50/50 read/write)

| Target              | ops/s | mean     | p50      | p95       | p99       |
|---------------------|------:|---------:|---------:|----------:|----------:|
| tessera-bolt (TLS)  |   167 |  6.0 ms  |  6.3 ms  |   9.8 ms  |  12.1 ms  |
| memgraph (TLS)      |   499 |  2.0 ms  |  1.3 ms  |   2.2 ms  |   3.1 ms  |

**Ratio: Memgraph ~3x faster on mixed**

## TLS Impact on Memgraph (plain TCP vs TLS)

| Scenario | Plain TCP ops/s | TLS ops/s | TLS overhead |
|----------|----------------:|----------:|-------------:|
| Write    |             777 |       644 |         ~17% |
| Read     |             676 |       718 |           0% |
| Mixed    |             493 |       499 |           0% |

TLS adds ~17% overhead on write, negligible on reads.

## GQL vs CypherCompat Parsing Overhead

10,000 iterations x 6 queries (INSERT/CREATE, MATCH, MATCH+WHERE, path pattern, MATCH+RETURN)

| Mode         | ops/s   | mean/query | Overhead vs Gql |
|--------------|--------:|-----------:|----------------:|
| Gql (direct) | 532,561 |    1,878 ns |             — |
| CypherCompat | 171,495 |    5,831 ns |        +210.5% |
| StrictGql    | 394,720 |    2,533 ns |         +34.9% |

- **CypherCompat is 3.1x slower** than Gql-direct parsing (preprocessor `cypher_to_gql` + re-parse)
- **StrictGql adds ~35%** overhead (validation scan before parse)
- In absolute terms: 5.8 µs vs 1.9 µs per query — still sub-10µs, so parsing
  is NOT the bottleneck compared to network + persistence (~8 ms per op via Bolt)

## Root Causes of Gap (TesseraGraph vs Memgraph)

1. **Flush-per-mutation** (dominant): TesseraGraph persists every write to disk
   via `graph.flush()`. Memgraph is in-memory only. This alone explains the
   ~5x write gap.
2. **Query execution pipeline**: TesseraGraph: parse GQL/Cypher → compile →
   execute → flush → respond. Memgraph: parse Cypher → execute (in-memory) → respond.
3. **TLS overhead**: Roughly equal on both (both use self-signed certs over localhost).

## TesseraGraph In-Process Reference (no network, no parsing)

| Scenario     | ops/s     | mean     | p50      | p95       | p99        |
|--------------|----------:|---------:|---------:|----------:|-----------:|
| write        |    63,930 | 15.6 µs  | 10.2 µs  |  38.8 µs  |   84.1 µs  |
| read         | 2,331,002 |  429 ns  |  368 ns  |   657 ns  |    750 ns  |
| traversal    |   136,798 |  7.3 µs  |  5.4 µs  |  24.2 µs  |   24.9 µs  |
| pathfinding  |       376 |  2.7 ms  |  2.5 ms  |   4.2 ms  |    5.5 ms  |
| mixed        |   247,341 |  4.0 µs  |  4.6 µs  |   9.2 µs  |   19.6 µs  |
| concurrent   |   166,390 |  9.7 µs  |  4.2 µs  |  29.1 µs  |  136.6 µs  |

## Optimization Opportunities

1. **Batch flush / WAL-only mode**: Defer `graph.flush()` to commit boundaries
   instead of per-mutation. This would close most of the write gap.
2. **Connection pooling**: Reuse Bolt sessions instead of per-query overhead.
3. **Skip preprocessor for GQL-native queries**: Auto-detect query language and
   bypass `cypher_to_gql` when input is already valid GQL.
4. **Prepared statements / query cache**: Avoid re-parsing identical query patterns.
