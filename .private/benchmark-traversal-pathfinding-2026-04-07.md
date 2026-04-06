# Benchmark: Traversal & Pathfinding — 2026-04-07

## Setup

- **TesseraGraph**: v0.2.3 (Docker, `tesseraio/tessera-graph-enterprise:0.2.3`)
  - Optimized BFS variable-hop + bidirectional BFS shortestPath
  - Bolt 4.4 + TLS (self-signed), 2 CPUs / 2GB RAM
- **Memgraph**: latest (Docker, `memgraph/memgraph:latest`)
  - Bolt 4.4 + TLS (self-signed), 2 CPUs / 2GB RAM
- **Driver**: Python neo4j v4.4.13 (monkey-patched `check_supported_server_product`)
- **Host**: macOS Darwin 25.3.0, Docker Desktop
- **Dataset**: 1000 nodes (chain) + 999 NEXT edges + 9 SHORTCUT edges (every 100 nodes)
- **Queries inline** (no Bolt parameters — TesseraGraph no los soporta)
- **Iterations**: 100 per scenario

## Results

| Scenario | TesseraGraph | Memgraph | Gap |
|---|---|---|---|
| Traversal (BFS depth=5) | 36.7 qps (27.2ms mean) | 868.5 qps (1.2ms mean) | **23.7x** |
| ShortestPath | 25.9 qps (38.6ms mean) | N/A (syntax incompatible) | — |
| Dataset setup (1000 nodes) | 29.8s | 2.1s | **14.2x** |

### TesseraGraph detail

| Scenario | qps | mean | p50 | p95 | p99 |
|---|---|---|---|---|---|
| Traversal | 36.7 | 27.2ms | 27.7ms | 33.2ms | 55.4ms |
| ShortestPath | 25.9 | 38.6ms | 38.9ms | 43.7ms | 70.6ms |

### Memgraph detail

| Scenario | qps | mean | p50 | p95 | p99 |
|---|---|---|---|---|---|
| Traversal | 868.5 | 1.2ms | 1.0ms | 1.5ms | 7.3ms |
| ShortestPath | SKIPPED | — | — | — | — |

## Analysis

### Traversal gap: 23.7x (worse than the 10x documented in the TDD plan)

The TDD plan assumed a 10x gap based on in-process benchmarks. The end-to-end Bolt
measurement shows **23.7x**. The optimized BFS engine (verified at 220 qps in-process
in debug mode, likely 2000+ in release) is not the bottleneck. The overhead is in
layers between the Bolt wire and the BFS execution:

1. **Query parsing**: each `MATCH (a:Node {idx: 0})-[*1..5]->(b:Node) RETURN count(b)`
   goes through lexer → parser → AST → compiler every time (query cache may not hit
   for variable-hop queries that bypass the MIT core execute path)
2. **Property matching**: `{idx: 0}` in MATCH triggers a linear scan of all Node-labeled
   nodes to find the start node — no index
3. **Bolt protocol overhead**: PackStream serialization/deserialization per message
4. **TLS overhead**: ~17% measured in previous benchmarks
5. **No parameter support**: queries are reparsed on every iteration instead of using
   prepared statements

### ShortestPath: no Memgraph comparison

Memgraph uses Cypher syntax `shortestPath((a)-[*]->(b))` while TesseraGraph uses
GQL function syntax `shortestPath(a, b)`. The neo4j Python driver sent the Cypher
syntax to Memgraph, which failed. A separate Memgraph-specific query would be needed.

### Dataset setup: 14.2x slower

TesseraGraph creates nodes/edges one-by-one via Bolt (no batch CREATE). Memgraph
does the same but is much faster per-operation. TesseraGraph's setup overhead is
dominated by per-query parsing + MATCH lookups for edge creation.

## Observations

1. `count_nodes()` returned 0 for TesseraGraph — `MATCH (n:Node) RETURN count(n)`
   may have an issue with the aggregation path, or the node label is not preserved
   correctly. Needs investigation.
2. The Python neo4j driver requires monkey-patching `check_supported_server_product`
   because it rejects any server agent that doesn't start with `Neo4j/`.
3. TesseraGraph does not support Bolt parameters (`$param`), forcing all values to
   be inlined in query text. This prevents query caching for parametrized queries.

## Priority investigation areas for next session

1. **Property index / start node resolution** — `{idx: 0}` triggers full scan. An
   index on `idx` (or using node ID directly) would eliminate this.
2. **Query cache for optimized path** — the `execute_query` enterprise path may bypass
   the query cache that exists for MIT core queries.
3. **Bolt parameter support** — enabling `$param` would allow query plan caching.
4. **Batch CREATE** — bulk insert support for dataset setup.
5. **Profile a single query end-to-end** — measure time spent in each layer:
   TLS → Bolt decode → parse → compile → execute → Bolt encode → TLS.

## Benchmark script

`.private/bench_traversal.py` — Python 3.12, neo4j driver v4.4.13

## Previous benchmarks for context

| Scenario | v0.1.0 | v0.2.0+opt | Memgraph | This run |
|---|---|---|---|---|
| Write (Bolt) | 115 qps | 706 qps | 710 qps | — |
| Mixed (Bolt) | 167 qps | 830 qps | 559 qps | — |
| Traversal (Bolt) | — | — | 868 qps | 36.7 qps |
| Traversal (in-process) | — | 189k qps | — | — |

The in-process traversal (189k qps) vs Bolt traversal (36.7 qps) shows a **5000x**
overhead from the network/protocol stack. This is the real bottleneck.
