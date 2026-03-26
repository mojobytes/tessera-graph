# Design Document: GraphAccess Trait for tessera-graph (MIT)

**Created**: 2026-03-21
**Status**: Approved by user, pending implementation in tessera-graph repo
**Context**: This document lives in the enterprise repo but describes work to be done in the MIT tessera-graph repo.

---

## Problem

`tessera-graph` was designed to be flexible so solutions built on top could mount their own logic. However, `PatternBuilder`, `NeighborQuery`, `TraversalBuilder`, `SubgraphQuery`, and the GQL compiler all take `&Graph` directly — a concrete type, not a trait. This makes it impossible to:

- Filter nodes by visibility (LBAC in tessera-graph-enterprise)
- Cache nodes in memory (read-through cache)
- Audit individual node accesses
- Federate data from multiple sources
- Apply on-read transformations (encryption, redaction)

## Solution

Introduce a `GraphAccess` trait that abstracts data access. `Graph` implements it. All query builders and the GQL compiler become generic over `GraphAccess` instead of taking `&Graph`.

## Trait Definition

```rust
pub trait GraphAccess {
    // --- Node reads ---
    fn node_ids(&self) -> Vec<NodeId>;
    fn nodes_by_label(&self, label: &str) -> Vec<NodeId>;
    fn node(&self, id: NodeId) -> Result<Node>;
    fn node_exists(&self, id: NodeId) -> bool;
    fn node_count(&self) -> usize;

    // --- Edge reads ---
    fn edges_by_label(&self, label: &str) -> Vec<EdgeId>;
    fn edge(&self, id: EdgeId) -> Result<Edge>;
    fn edge_count(&self) -> usize;
    fn outgoing_edges(&self, node: NodeId) -> Result<Vec<Edge>>;
    fn incoming_edges(&self, node: NodeId) -> Result<Vec<Edge>>;

    // --- Node mutations ---
    fn add_node(&mut self, label: &str, properties: Properties) -> Result<NodeId>;
    fn update_node(&mut self, id: NodeId, node: &Node) -> Result<()>;
    fn remove_node(&mut self, id: NodeId) -> Result<Node>;

    // --- Edge mutations ---
    fn add_edge(&mut self, label: &str, source: NodeId, target: NodeId, properties: Properties) -> Result<EdgeId>;
    fn update_edge(&mut self, id: EdgeId, edge: &Edge) -> Result<()>;
    fn remove_edge(&mut self, id: EdgeId) -> Result<Edge>;
}
```

## What's NOT in the trait

Lifecycle/storage operations stay as direct methods on `Graph`:
- `new()`, `open(path, config)` — constructors
- `flush()` — persistence
- `begin_batch()`, `end_batch()` — batching
- `SharedGraph` wrapper — thread-safe access

These are implementation-specific. A `SecureGraph` wrapper doesn't open files — it wraps a `Graph` that's already open.

## Changes Required

### 1. New file: `src/access.rs`
- Define the `GraphAccess` trait
- `impl GraphAccess for Graph` — delegates to existing methods

### 2. `add_node` signature change
Current: `fn add_node(&mut self, label: impl Into<String>, properties: Properties) -> Result<NodeId>`
Trait: `fn add_node(&mut self, label: &str, properties: Properties) -> Result<NodeId>`

`impl Into<String>` is not object-safe and makes the trait non-dyn-compatible. Change to `&str` — the concrete `Graph::add_node` can keep the `impl Into<String>` as sugar that calls the trait method internally.

Same applies to `add_edge` label parameter.

### 3. Make builders generic
Each builder currently holds `graph: &'g Graph`. Change to `graph: &'g G` where `G: GraphAccess`:

| Builder | File | Current | New |
|---------|------|---------|-----|
| `PatternBuilder<'g>` | `src/query/pattern.rs` | `graph: &'g Graph` | `graph: &'g G` where `G: GraphAccess` |
| `NeighborQuery<'g>` | `src/query/neighbor.rs` | `graph: &'g Graph` | `graph: &'g G` where `G: GraphAccess` |
| `TraversalBuilder<'g>` | `src/query/traversal.rs` | `graph: &'g Graph` | `graph: &'g G` where `G: GraphAccess` |
| `SubgraphQuery<'g>` | `src/query/subgraph.rs` | `graph: &'g Graph` | `graph: &'g G` where `G: GraphAccess` |

### 4. Make GQL compiler generic
Functions in `src/gql/compiler.rs` that take `&Graph`:

| Function | Change |
|----------|--------|
| `execute(graph: &Graph, query: &GqlQuery)` | `execute(graph: &G, query: &GqlQuery) where G: GraphAccess` |
| `compile_match(graph: &Graph, mc: &MatchClause)` | `compile_match(graph: &G, ...)` |
| `compile_path_pattern(graph: &Graph, pp: &PathPattern)` | `compile_path_pattern(graph: &G, ...)` |
| `compile_match_for_mutation(graph: &Graph, ...)` | `compile_match_for_mutation(graph: &G, ...)` |

### 5. Convenience methods on `Graph`
`Graph` keeps its sugar methods (`neighbors()`, `pattern()`, `traverse()`, `subgraph()`) that return builders parameterized with `Graph` as the concrete `G`. Existing user code doesn't break.

### 6. Update tessera-storage-enterprise (separate PR)
After the MIT trait is merged:
- `execute_mut` in `tessera-storage-enterprise` changes from `graph: &mut Graph` to `graph: &mut G where G: GraphAccess`
- This enables `SecureGraph` to intercept mutations for LBAC clearance checks

### 7. Re-exports
`src/lib.rs` adds: `pub use access::GraphAccess;`

## What Must NOT Change

- All existing tests pass without modification
- `Graph::new()`, `Graph::open()` API unchanged
- `let mut g = Graph::new(); g.add_node("Label", props!{})` still works
- Public API is additive only — no removals

## Impact on tessera-graph-enterprise

Once the trait is merged in the MIT repo:
1. `SecureGraph` in enterprise implements `GraphAccess`, wrapping `&Graph` with clearance filtering
2. `PatternBuilder<'_, SecureGraph>` works out of the box
3. `gql::execute(&secure_graph, &query)` filters nodes at access time, not post-query
4. No data ever enters memory if the user doesn't have clearance

## Testing Strategy

- All existing tests continue to pass (Graph implements GraphAccess)
- New tests verify that a mock `GraphAccess` implementation works with all builders
- New test: `PatternBuilder<'_, MockGraph>` finds patterns correctly
- New test: `NeighborQuery<'_, MockGraph>` filters correctly
- Performance regression guard: PatternBuilder throughput unchanged

## Estimation

~4-6 hours. The refactor is mechanical (replace `&Graph` with `&G: GraphAccess`) but touches many files and requires careful attention to lifetime propagation.
