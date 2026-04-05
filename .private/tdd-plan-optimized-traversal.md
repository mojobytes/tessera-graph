# TDD Plan: Optimized Traversal & ShortestPath (Enterprise)

**Date**: 2026-04-05
**Branch**: feature/resilience-streaming-quality
**Benchmark gap**: Traversal 10x, Pathfinding 21x slower than Memgraph

## Root Causes (MIT core compiler.rs)

1. `expand_variable_hop()` (L970-1054): `graph.node()` + HashMap clone per visited node in BFS
2. `shortest_path_bfs()` (L233-273): Unidirectional BFS only
3. `PatternMatch`: Full deep clone of HashMap<String, Node> per result row

## Strategy

Enterprise `execute_query()` in `tessera-graph-storage/src/gql/mod.rs`:
- Inspect AST for `EdgeLength::Variable` or `shortestPath` function calls
- If detected → optimized enterprise execution
- Otherwise → delegate to `tessera_graph::gql::execute()`
- Wire at `bolt_handler.rs:491`

---

## Fase 1: Query Classifier

### Cycle 1.1: needs_optimized_execution detects variable-hop
- RED: `crates/tessera-graph-storage/tests/optimized_traversal_test.rs`
  - Test calls `needs_optimized_execution(&query)` on `MATCH (a)-[*1..3]->(b) RETURN b`
  - Assert: returns `true`
- GREEN: `crates/tessera-graph-storage/src/gql/mod.rs`
  - `pub fn needs_optimized_execution(query: &GqlQuery) -> bool`
  - Walk `match_clause.patterns[].hops[]` for `EdgeLength::Variable`
  - Walk `return_clause.items[]` for `Expr::FunctionCall { name: "shortestpath" }`
- REFACTOR: none

### Cycle 1.2: Fixed-hop and shortestPath classification
- RED: Tests for fixed-hop → `false`, shortestPath → `true`, empty → `false`
- GREEN: Already covered by 1.1 implementation
- REFACTOR: none

---

## Fase 2: Optimized Variable-Hop Traversal

### Cycle 2.1: Basic chain traversal matches MIT core
- RED: Chain `A -> B -> C -> D`
  - Query: `MATCH (a:Node {name:'A'})-[*1..3]->(b:Node) RETURN b.name`
  - Assert: `execute_query` results == `tessera_graph::gql::execute` results (order-independent)
- GREEN: `pub fn execute_query<G: GraphAccess>(graph: &G, query: &GqlQuery) -> Result<GqlResult>`
  - `!needs_optimized_execution` → delegate to MIT core
  - Has WHERE → delegate to MIT core (documented limitation)
  - Otherwise → `execute_variable_hop_query`:
    - Build `HashSet<NodeId>` from `nodes_by_label(end_label)` for O(1) label check
    - BFS tracking `NodeId` only (not full `Node`)
    - Only `graph.node(id)` when emitting result that passes label check
    - Project RETURN items into `HashMap<String, GqlValue>` directly
- REFACTOR: none

### Cycle 2.2: Edge cases
- RED: Four tests:
  1. `min=0` includes start node
  2. Cycles produce no duplicates
  3. Depth boundary exact (depth=3 stops at 3)
  4. No-match label → empty result
- GREEN: Handle in BFS logic
- REFACTOR: Extract helper functions if BFS body exceeds 50 lines

### Cycle 2.3: Throughput guard
- RED: Tree 500 nodes, branching factor 4, 200 queries
  - Threshold: `debug >= 500 qps`, `release >= 5000 qps`
- GREEN: Already fast from deferred-fetch optimization
- REFACTOR: none

---

## Fase 3: Bidirectional BFS for shortestPath

### Cycle 3.1: Basic shortest path matches MIT core
- RED: Graph `A -> B -> C -> D -> E` plus shortcut `A -> D`
  - Query: `MATCH (a:Node {name:'A'}) MATCH (b:Node {name:'E'}) RETURN shortestPath(a, b)`
  - Assert: path length == MIT core path length
- GREEN: `execute_query` detects `FunctionCall { name: "shortestpath" }` in RETURN
  - Route to `bidirectional_bfs_shortest_path(graph, from, to) -> Option<Vec<NodeId>>`
  - Forward frontier: `outgoing_edges`, backward frontier: `incoming_edges`
  - Expand smaller frontier each step
  - Reconstruct path from both parent maps when frontiers intersect
- REFACTOR: none

### Cycle 3.2: Edge cases
- RED: Four tests:
  1. Unreachable → NULL
  2. Same source/target → single-element list
  3. Direct edge → length-1 path
  4. Multi-path graph → returns minimum-length path
- GREEN: Handle in bidirectional BFS
- REFACTOR: none

### Cycle 3.3: Throughput guard
- RED: Grid graph 1000 nodes, 100 queries
  - Threshold: `enterprise_rate >= mit_rate * 0.9` (debug), `>= mit_rate * 1.2` (release)
- GREEN: Bidirectional BFS inherently faster for large graphs
- REFACTOR: none

---

## Fase 4: Wire into bolt_handler

### Cycle 4.1: Type compatibility
- RED: Test that `execute_query` accepts `&SecureGraphRef<Graph>`
- GREEN: Already generic over `G: GraphAccess`
- REFACTOR: none

### Cycle 4.2: Replace call site
- File: `crates/tessera-graph-server/src/bolt_handler.rs:491`
- Before: `tessera_graph::gql::execute(&secure, q)`
- After: `tessera_graph_storage::gql::execute_query(&secure, q)`
- Verify: `cargo check --workspace`

### Cycle 4.3: Full suite
- `cargo test --workspace` green, zero warnings

---

## Fase 5: Wiring Verification

- [ ] `execute_query` called from `bolt_handler.rs` (not just tests)
- [ ] `needs_optimized_execution` called from `execute_query`
- [ ] No orphaned internal functions (all `fn` have call sites)
- [ ] MIT core `execute` still reachable via delegation path
- [ ] Feature gate `#[cfg(feature = "extended-gql")]` on all shortestPath code
- [ ] Zero stale references to old `tessera_graph::gql::execute` in bolt_handler

---

## Critical Implementation Notes

- **Feature gate**: `Expr::FunctionCall` only exists under `extended-gql`. All pattern matching on it must be gated.
- **Label check during BFS**: `nodes_by_label()` → `HashSet<NodeId>` once before BFS. O(1) membership check per node instead of full `graph.node()` fetch.
- **RETURN projection**: Build `HashMap<String, GqlValue>` directly. Do NOT reuse `eval_expr` from MIT core (not public). Variable names come from `NodePattern::var`.
- **WHERE fallback**: Queries with WHERE delegate to MIT core. Documented limitation — avoids reimplementing expression evaluator.

## Success Criteria

- [ ] Variable-hop results identical to MIT core (order-independent comparison)
- [ ] shortestPath results identical to MIT core
- [ ] Variable-hop throughput: >= 500 qps debug / >= 5000 qps release
- [ ] shortestPath: enterprise >= MIT core × 1.2 in release
- [ ] `cargo test --workspace` green
- [ ] Zero warnings
- [ ] Benchmark: traversal gap vs Memgraph < 3x (down from 10x)
- [ ] Benchmark: pathfinding gap vs Memgraph < 3x (down from 21x)
