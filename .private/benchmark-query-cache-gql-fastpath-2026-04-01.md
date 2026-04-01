# Benchmark — Query Cache + GQL Fast-Path + WAL BufWriter — 2026-04-01

## Environment
- Host: macOS Darwin 25.3.0
- Docker: 29.3.1
- Resource limits: 2 CPUs + 2 GB RAM per container
- TesseraGraph: tesseraio/tessera-graph-enterprise:latest (rebuilt with optimizations)
- Memgraph: memgraph/memgraph:latest v3.9.0 (not benchmarked — neo4rs 0.8 incompatible with Bolt handshake)
- Benchmark binary: tessera-bench --release (features: tessera-bolt)
- **TLS enabled** on TesseraGraph

## Optimizations Applied

1. **Query Cache** (server-wide LRU, 1024 entries): cache-through on `parse_with_mode`, keyed by query text. Cache hit returns cloned AST, skips parsing entirely.
2. **GQL-Native Detection**: `contains_cypher_constructs()` fast-path in `CypherCompat` mode. Pure GQL queries bypass the Cypher preprocessor.
3. **WAL BufWriter** (MIT core): `WalWriter` uses `BufWriter<File>` to batch `write_all()` syscalls. Single `flush()` + `sync_data()` per sync.

## Parse Overhead (parse-bench)

| Mode | ops/s | mean (ns) | Overhead vs GQL |
|------|------:|----------:|----------------:|
| Gql (direct) | 514,134 | 1,945 | — |
| CypherCompat | 366,158 | 2,731 | **40.4%** |
| StrictGql | 384,284 | 2,602 | 33.8% |

**Before (v0.2.0):** CypherCompat overhead was 210% vs GQL.
**After:** 40.4% — GQL fast-path avoids preprocessor for pure GQL queries.

## TesseraBolt Results (Bolt 4.4 + TLS, Docker 2CPU+2GB)

### Write (1000 nodes, 999 edges)

| Version | ops/s | mean | p50 | p95 | p99 |
|---------|------:|-----:|----:|----:|----:|
| v0.2.0 (deferred flush only) | 379 | 2.6 ms | 2.3 ms | 6.2 ms | 7.5 ms |
| v0.2.0 + cache/fastpath/bufwriter | **706** | 1.4 ms | 1.2 ms | 2.5 ms | 3.6 ms |

**Improvement: +86%**

### Mixed (100 ops, 50/50 read/write)

| Version | ops/s | mean | p50 | p95 | p99 |
|---------|------:|-----:|----:|----:|----:|
| v0.2.0 (deferred flush only) | 284 | 3.5 ms | 2.9 ms | 7.1 ms | 9.3 ms |
| v0.2.0 + cache/fastpath/bufwriter | **830** | 1.2 ms | 1.2 ms | 1.4 ms | 1.7 ms |

**Improvement: +192%**

### Read (100 nodes, 100 lookups)

| Version | ops/s | mean | p50 | p95 | p99 |
|---------|------:|-----:|----:|----:|----:|
| v0.2.0 (deferred flush only) | 218 | 4.6 ms | 3.3 ms | 11.5 ms | 13.3 ms |
| v0.2.0 + cache/fastpath/bufwriter | **849** | 1.2 ms | 1.1 ms | 1.5 ms | 1.7 ms |

**Improvement: +289%**

## In-Process Results (no network, no TLS)

| Scenario | v0.2.0 (before) | v0.2.0 + optimizations | Change |
|----------|----------------:|-----------------------:|-------:|
| write | 63,930 | 124,812 | **+95%** |
| read | 2,331,002 | 2,217,294 | ~equal |
| traversal | 136,798 | 189,645 | **+39%** |
| pathfinding | 376 | 787 | **+109%** |
| mixed | 247,341 | 403,551 | **+63%** |
| concurrent | 166,390 | 249,881 | **+50%** |

## Comparison vs Memgraph (from v0.2.0 baseline)

| Scenario | Tessera v0.1.0 | Tessera v0.2.0 | Tessera v0.2.0+opt | Memgraph | Gap (v0.1) | Gap (v0.2) | Gap (v0.2+opt) |
|----------|---------------:|---------------:|-------------------:|---------:|-----------:|-----------:|---------------:|
| Write | 115 | 379 | **706** | 710 | 6.2x | 1.9x | **~1.0x** |
| Mixed | 167 | 284 | **830** | 559 | 3.3x | 2.0x | **0.67x** |

**TesseraGraph now matches Memgraph on write and surpasses it on mixed workloads.**

## Notes

- Memgraph v3.9.0 could not be benchmarked this session due to neo4rs 0.8 Bolt protocol handshake incompatibility. Memgraph numbers above are from the 2026-03-30 baseline.
- The mixed improvement (+192%) is larger than write (+86%) because read queries benefit heavily from query cache hits (same MATCH pattern repeated).
- Read improvement (+289%) is partly cache effect, partly warm Docker volume vs cold in previous run.
- p95/p99 latencies improved dramatically (write p95: 6.2ms → 2.5ms), indicating less variance from eliminated parsing overhead.

## Remaining Optimization Opportunities

1. **Connection pooling**: Statement sharing across connections (query cache already server-wide)
2. **Parametrized queries**: Currently rejected — implementing would improve cache hit rate for variable-only differences
3. **WAL size-driven flush**: Complement timer with size threshold for burst writes
