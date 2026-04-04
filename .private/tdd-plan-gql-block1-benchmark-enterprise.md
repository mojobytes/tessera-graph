# TDD Plan: GQL Block 1 — Variable-Length Paths + shortestPath (Enterprise)

**Created**: 2026-04-03
**Branch**: `feature/gql-block1-enterprise` (from `develop`)
**Scope**: Enterprise repo only. Zero writes to MIT core.

---

## Contexto

GQL Block 1 adds two advanced graph-traversal features:

1. **Variable-length paths** — `MATCH (a)-[*1..5]->(b) RETURN a, b`
2. **shortestPath()** — `MATCH p = shortestPath((a)-[*]->(b)) RETURN p`

Both are read-only query extensions. The MIT core parser already contains
`EdgeLength::Variable { min, max }` in `ast.rs` and the `Expr::FunctionCall`
variant (both unconditionally present — the AST types are NOT gated). However,
the parser (`parse_edge_bracket_content`, line 831) returns
`Error::GqlUnsupported` the instant it sees `Token::Star` inside an edge
bracket, before the AST is ever constructed. `Expr::FunctionCall` is
`#[cfg(feature = "extended-gql")]` in the parser's expression production, so
`shortestPath(...)` parsing is also gated.

The enterprise workspace already activates `features = ["extended-gql"]` for
`tessera-graph` in the root `Cargo.toml`. This means:

- `EdgeLength::Variable` is usable in enterprise code with no changes.
- `Expr::FunctionCall` is available in the AST and in `eval_expr`.
- BUT the **parser still rejects `*` unconditionally** (the guard at line 831
  is not feature-gated — it is an unconditional `return Err(...)` regardless
  of feature flags).

**Conclusion on what requires a core change:**

The parser's `*` rejection at line 831 of `parser.rs` is NOT behind a
`#[cfg(feature = "extended-gql")]` guard. It fires for all builds, including
the enterprise build. This means `tessera_graph::gql::parse()` will fail with
`GqlUnsupported` before the AST is constructed. There is no way to intercept
after parsing, because parsing never succeeds.

**The MIT core needs exactly one change** to unblock both features:

```
parser.rs line 831-832 — wrap the unconditional rejection in a cfg guard:
```

```rust
// BEFORE (rejects in all builds):
if *self.peek() == Token::Star {
    return Err(Self::unsupported_feature("variable-length paths"));
}

// AFTER (only rejects in builds without extended-gql):
#[cfg(not(feature = "extended-gql"))]
if *self.peek() == Token::Star {
    return Err(Self::unsupported_feature("variable-length paths"));
}
// Under extended-gql: fall through and parse the range into EdgeLength::Variable.
```

Plus the range parsing logic (`*`, `*N`, `*N..M`, `*..M`) that populates
`EdgeLength::Variable { min, max }`.

Similarly, `shortestPath(...)` requires the function-call parser branch to
accept a call named `shortestPath` and produce `Expr::FunctionCall`.
Examining the parser under `extended-gql`, line 1086 handles `FunctionCall` —
but only for `id`, `type`, `labels`. The `shortestPath` call needs a
syntactic wrapper (`p = shortestPath(...)`) that the current parser does not
handle at all (it is a path expression, not a scalar expression).

**Summary of required MIT core changes (prerequisite session):**

| Change | File | Lines affected |
|--------|------|----------------|
| Gate `*` rejection behind `#[cfg(not(feature = "extended-gql"))]` | `parser.rs` | ~831-832 |
| Add range parser for `*`, `*N`, `*N..M`, `*..M` → `EdgeLength::Variable` | `parser.rs` | ~807-836 |
| Add `shortestPath(pattern)` path expression parser under `extended-gql` | `parser.rs` | ~1086 block |
| Expose `EdgeLength` via `pub use` in `gql/mod.rs` | `mod.rs` | line 21 block |

Until the MIT core change is merged, the enterprise parser will not receive a
`GqlQuery` containing `EdgeLength::Variable` or a `shortestPath` call, so
enterprise compilation logic cannot be reached.

---

## Approach Selected: Enterprise Compiler Extension in `tessera-graph-storage`

Once the MIT core ships the parser fix, the architecture is:

```
query string
    → tessera_graph_cypher::parse_with_mode_cached()  [existing]
        → preprocessor: no changes needed for variable-length paths
        → tessera_graph::gql::parse_statement()        [core — fixed parser]
            → GqlStatement::Query(GqlQuery { match_clause: ..., ... })
                with EdgeLength::Variable populated
    → bolt_handler::handle_run()
        → GqlStatement::Query arm
            → NEW: tessera_graph_storage::gql::execute_extended()
                   (replaces direct call to tessera_graph::gql::execute)
                   detects Variable edges → runs BFS/DFS expansion
                   detects shortestPath → runs Dijkstra/BFS
                   plain queries → delegates to tessera_graph::gql::execute
```

The new function `execute_extended` lives in
`crates/tessera-graph-storage/src/gql/mod.rs` alongside the existing
`execute_mut`. This mirrors the existing pattern: the MIT core handles
vanilla queries, enterprise adds enterprise-only read logic.

The single wiring change in `bolt_handler.rs` replaces:
```rust
tessera_graph::gql::execute(&secure, q)
```
with:
```rust
tessera_graph_storage::gql::execute_extended(&secure, q)
```

---

**Stack detectado**: Rust / no async (pure sync query execution)
**Convenciones**: TDD cycles with throughput regression guards; `#[cfg(not(test))]` never used; tests in `#[cfg(test)]` blocks inside the source file
**Afecta hot path**: YES — `execute_extended` sits on the query hot path (every read query goes through it). Must include throughput regression guard.

---

## Decisiones Previas Necesarias

**BLOQUEANTE — Prerequisite MIT core session required before this plan can start.**

The following work must happen in a separate session that targets the MIT core
repo (`tessera-graph`), NOT this repo:

1. Gate the `*` rejection in `parser.rs` behind `#[cfg(not(feature = "extended-gql"))]`.
2. Add the range parser for `EdgeLength::Variable` under `extended-gql`.
3. Add the `shortestPath(pattern)` syntactic form under `extended-gql`.
4. Ensure `EdgeLength` is re-exported from `gql/mod.rs` so enterprise code can import it.
5. The existing MIT core tests must continue to pass without `extended-gql` (no regression).
6. New MIT core tests: `parse("MATCH (a)-[*1..3]->(b) RETURN a") → Ok(query with Variable edge)`.

**This enterprise plan assumes those MIT core changes are merged and available
at the workspace path before Phase 1 begins.**

If they are not merged, Phase 1 will fail at compile time with:
`error[E0277]: the trait ... is not satisfied` or the tests will produce
`Error::GqlUnsupported` instead of a parsed AST.

---

## Plan de Ejecución

### Phase 0: Core prerequisite verification (5 min, no code written)

1. [ ] Verify MIT core has merged the parser fix
   - File: `/Volumes/WD_BLACK/repos/MojoBytes/tessera-ecosystem/tessera-graph/crates/tessera-graph/src/gql/parser.rs`
   - Action: confirm `Token::Star` path is gated `#[cfg(not(feature = "extended-gql"))]`
   - Action: confirm `parse("MATCH (a)-[*1..3]->(b) RETURN a, b")` returns `Ok(_)` in enterprise workspace
   - Output: `cargo test -p tessera-graph --features extended-gql` passes with no new failures

---

### Phase 1: Feature 1 RED — Variable-length path tests (25 min)

2. [ ] Write failing tests for `execute_extended` — variable-length paths
   - File: `crates/tessera-graph-storage/src/gql/mod.rs` (new `#[cfg(test)]` block at bottom)
   - Action: Add test fixture builder (inline, reusable across tests): creates a small
     linear chain graph A→B→C→D with label `NEXT`
   - Tests to add (all must compile but FAIL because `execute_extended` does not exist yet):
     ```
     variable_path_fixed_depth_1_matches_single_hop
     variable_path_range_1_to_2_finds_direct_and_two_hop
     variable_path_range_2_to_2_skips_single_hop
     variable_path_unbounded_traverses_entire_chain
     variable_path_zero_min_matches_start_node_itself
     variable_path_direction_incoming_is_respected
     variable_path_label_filter_restricts_edge_type
     variable_path_no_match_returns_empty
     variable_path_plain_query_delegates_to_core
     ```
   - Output: `cargo test -p tessera-graph-storage variable_path` → compile error (function not found)

---

### Phase 2: Feature 2 RED — shortestPath tests (20 min)

3. [ ] Write failing tests for `execute_extended` — shortestPath
   - File: `crates/tessera-graph-storage/src/gql/mod.rs` (same test block)
   - Tests to add:
     ```
     shortest_path_direct_connection_returns_one_hop
     shortest_path_indirect_returns_minimum_hops
     shortest_path_no_path_returns_empty
     shortest_path_cycle_does_not_loop_forever
     shortest_path_returns_correct_start_and_end_vars
     ```
   - Output: compile error (function not found) — all RED confirmed

---

### Phase 3: Implement `execute_extended` skeleton (20 min)

4. [ ] Create public signature and delegation path in `execute_extended`
   - File: `crates/tessera-graph-storage/src/gql/mod.rs`
   - Action: Add `pub fn execute_extended<G: GraphAccess + ?Sized>(graph: &G, query: &GqlQuery) -> tessera_graph::Result<GqlResult>`
   - Action: Implement the routing logic:
     ```rust
     fn query_needs_extension(query: &GqlQuery) -> (bool, bool) {
         // returns (has_variable_edges, has_shortest_path)
     }
     ```
   - If no extension needed: delegate to `tessera_graph::gql::execute(graph, query)`
   - If needs extension: dispatch to Feature 1 or Feature 2 handlers (stub: return `Err(GqlCompileError("not yet implemented"))`)
   - Output: `variable_path_plain_query_delegates_to_core` passes (GREEN); all other tests still RED

---

### Phase 4: Implement variable-length path expansion (60 min)

5. [ ] Implement BFS path expansion engine
   - File: `crates/tessera-graph-storage/src/gql/variable_path.rs` (new file)
   - Action: Create `pub(super) fn expand_variable_path<G: GraphAccess + ?Sized>(...)` with signature:
     ```rust
     pub(super) fn expand_variable_path<G: GraphAccess + ?Sized>(
         graph: &G,
         start_ids: &[NodeId],
         direction: AstDirection,
         edge_label: Option<&str>,
         min: u32,   // 0 means start node itself is a valid match
         max: u32,   // u32::MAX means unbounded (caller must cap at a safety limit)
     ) -> tessera_graph::Result<Vec<(NodeId, NodeId)>>
     ```
   - BFS: use `VecDeque<(NodeId, u32 depth)>` and `HashSet<NodeId>` for cycle
     prevention. Visit up to depth=max, collect `(start, reached)` pairs where
     `depth >= min`.
   - Unbounded max: cap at 1000 hops (hard safety limit; document in doc comment).
   - Direction: use `graph.outgoing_edges(id)` / `graph.incoming_edges(id)` /
     both, matching `AstDirection`.
   - Edge label filter: skip edges where `edge.label() != edge_label` when label is Some.
   - Output: unit tests in the same file for the pure expansion logic

6. [ ] Wire expansion into `execute_extended` for variable-edge queries
   - File: `crates/tessera-graph-storage/src/gql/mod.rs`
   - Action: When a path pattern contains `EdgeLength::Variable`, replace the
     `compile_path_pattern` delegation with:
     a. Resolve start nodes (using label/prop filters from the start `NodePattern`)
     b. For each edge pattern with `Variable { min, max }`, call `expand_variable_path`
     c. Apply end node label/prop filters
     d. Construct `PatternMatch` rows from `(start_var, start_node), (end_var, end_node)` pairs
     e. Apply WHERE predicate from the query's `where_clause` using `eval_expr` (via re-export or direct import)
     f. Apply RETURN projection, ORDER BY, LIMIT
   - The WHERE / RETURN / ORDER BY / LIMIT logic reuses the core's internal
     helpers via `tessera_graph::gql::execute` on a synthetic single-node
     query, OR by re-implementing the projection steps inline using publicly
     exported types (`GqlRow`, `GqlResult`, `GqlValue`).
   - NOTE: `eval_expr` and `project_row` are private in the core compiler. The
     cleanest approach is to generate a synthetic `GqlQuery` for the post-match
     phases and call `tessera_graph::gql::execute` with pre-filtered rows
     encoded as a single-node pattern. If this is impractical, implement a
     minimal `eval_expr` subset directly in the enterprise crate for WHERE
     evaluation on the expanded rows.
   - Output: all `variable_path_*` tests pass (GREEN)

---

### Phase 5: Implement shortestPath (45 min)

7. [ ] Implement BFS shortest-path engine
   - File: `crates/tessera-graph-storage/src/gql/shortest_path.rs` (new file)
   - Action: Create `pub(super) fn bfs_shortest_path<G: GraphAccess + ?Sized>(...)`:
     ```rust
     pub(super) fn bfs_shortest_path<G: GraphAccess + ?Sized>(
         graph: &G,
         source: NodeId,
         target: NodeId,
         direction: AstDirection,
         edge_label: Option<&str>,
         max_depth: u32,      // from the `*` range if provided, else cap at 1000
     ) -> tessera_graph::Result<Option<Vec<NodeId>>>  // None = no path
     ```
   - BFS with parent tracking: `HashMap<NodeId, NodeId>` to reconstruct path.
   - Cycle safety: `HashSet<NodeId>` visited set.
   - Max depth enforcement: abort and return `None` when depth exceeds `max_depth`.
   - Output: unit tests covering direct path, multi-hop, no-path, cycle cases

8. [ ] Wire shortestPath into `execute_extended`
   - File: `crates/tessera-graph-storage/src/gql/mod.rs`
   - Action: Parse the `shortestPath` function call from the query AST:
     The function appears as `Expr::FunctionCall { name: "shortestpath", args: [path_expr] }`
     where `path_expr` encodes the pattern `(a)-[*]->(b)`.
   - Detect the pattern in the `MATCH` clause or `RETURN` clause.
   - Resolve source and target node IDs from bound variables (or inline label constraints).
   - Call `bfs_shortest_path` for each (source, target) candidate pair.
   - Project results: return columns matching the RETURN clause aliases.
   - Output: all `shortest_path_*` tests pass (GREEN)

---

### Phase 6: Throughput regression guard (20 min)

9. [ ] Add throughput regression guard for `execute_extended` plain-query delegation
   - File: `crates/tessera-graph-storage/src/gql/mod.rs` test block
   - Action: Measure throughput of `execute_extended` on a plain `MATCH (n) RETURN n` query
     (no variable edges, no shortestPath) to verify delegation overhead is negligible:
     ```rust
     #[test]
     fn execute_extended_plain_query_throughput_regression_guard() {
         // Build a graph with 100 nodes
         // Warm-up: 1000 iterations
         // Measure: 10_000 iterations
         // Assert ops/s >= 50_000 (debug) / 500_000 (release)
         // Criterion: no more than 10% overhead vs raw tessera_graph::gql::execute baseline
     }
     ```
   - Threshold rationale: matches the `gql_native_fastpath_throughput_regression_guard`
     threshold in `tessera-graph-cypher` (same call depth).
   - Output: test passes; regression guard is committed with the implementation

---

### Phase 7: Wire into `bolt_handler.rs` (15 min)

10. [ ] Replace `tessera_graph::gql::execute` with `execute_extended` in the Bolt handler
    - File: `crates/tessera-graph-server/src/bolt_handler.rs`
    - Line: ~491 — `GqlStatement::Query(ref q)` arm
    - Change:
      ```rust
      // Before:
      let result = tessera_graph::gql::execute(&secure, q)
      // After:
      let result = tessera_graph_storage::gql::execute_extended(&secure, q)
      ```
    - No other changes to `bolt_handler.rs`.
    - Output: `cargo build -p tessera-graph-server` succeeds with zero warnings

---

### Phase 8: REFACTOR — Extract WHERE/RETURN evaluation helper (20 min)

11. [ ] Eliminate duplication in projection logic
    - File: `crates/tessera-graph-storage/src/gql/mod.rs`
    - Review the WHERE + RETURN + ORDER BY + LIMIT code written in Phase 4/5.
    - Extract a `apply_query_pipeline(matches, query) -> GqlResult` helper that
      both the variable-path and shortest-path code paths share.
    - Output: no behaviour change; all tests still GREEN; no duplicated match
      arms for WHERE / ORDER BY / LIMIT

---

### Phase 9: Compile and lint pass (10 min)

12. [ ] Full workspace compile and test
    - Command: `cargo test --workspace`
    - Command: `cargo clippy --workspace -- -D warnings`
    - Output: zero errors, zero warnings, all tests GREEN

---

### Phase 10: Wiring verification (10 min)

13. [ ] Run verify-wiring skill
    - Confirm `execute_extended` is called from `bolt_handler.rs` (not dead code).
    - Confirm `variable_path.rs` and `shortest_path.rs` are declared as submodules.
    - Confirm no old `tessera_graph::gql::execute` call remains in `bolt_handler.rs`.
    - Output: verify-wiring passes with no unwired components detected

---

## Estimacion Total

| Phase | Tiempo estimado |
|-------|----------------|
| Phase 0: Core verification | 5 min |
| Phase 1: Variable-path RED | 25 min |
| Phase 2: shortestPath RED | 20 min |
| Phase 3: Skeleton | 20 min |
| Phase 4: Variable-path GREEN | 60 min |
| Phase 5: shortestPath GREEN | 45 min |
| Phase 6: Throughput guard | 20 min |
| Phase 7: Wiring | 15 min |
| Phase 8: Refactor | 20 min |
| Phase 9: Lint pass | 10 min |
| Phase 10: Verification | 10 min |
| **Total** | **~4 horas** |

Prerequisite MIT core session: ~1 hour separately (not counted here).

---

## Criterios de Exito

- [ ] All `variable_path_*` tests pass
- [ ] All `shortest_path_*` tests pass
- [ ] `execute_extended_plain_query_throughput_regression_guard` passes at
      >= 50,000 ops/s (debug) / 500,000 ops/s (release)
- [ ] Throughput overhead of `execute_extended` over raw `execute` <= 10%
- [ ] `cargo clippy --workspace -- -D warnings` produces zero diagnostics
- [ ] `cargo test --workspace` produces zero failures
- [ ] `bolt_handler.rs` calls `execute_extended`, not `tessera_graph::gql::execute`, for the read path
- [ ] MIT core tests continue to pass without `extended-gql` (verified in prerequisite session)
- [ ] Unbounded path traversal is capped at 1000 hops (test: `variable_path_cycle_does_not_loop_forever`)

---

## Archivos que se crean o modifican

| Archivo | Accion |
|---------|--------|
| `crates/tessera-graph-storage/src/gql/mod.rs` | Modificar — add `execute_extended`, tests |
| `crates/tessera-graph-storage/src/gql/variable_path.rs` | Crear — BFS expansion engine |
| `crates/tessera-graph-storage/src/gql/shortest_path.rs` | Crear — BFS shortest-path engine |
| `crates/tessera-graph-server/src/bolt_handler.rs` | Modificar — single call site swap |

No new crate is needed. No changes to `Cargo.toml` files (all required types
are already imported via `tessera-graph` with `extended-gql` feature active).

---

## Nota sobre eval_expr y projection

The MIT core exports `GqlResult`, `GqlRow`, `GqlValue` but does NOT export
`eval_expr`, `project_row`, or `apply_order_by`. These are private functions.
Two options for Phase 4/5 projection:

**Option A** (preferred): Implement a minimal local `eval_where` in enterprise
that handles only the `Expr` variants needed for WHERE predicates on path
results (PropAccess, BinaryOp, Literal comparisons). Project RETURN items by
directly constructing `GqlRow` using `GqlValue` from public node/edge
accessors. This is ~60 lines, not a significant duplication risk given the
narrow scope.

**Option B**: Request MIT core to expose `eval_expr` as `pub(crate)` or as a
public helper under `extended-gql`. This is a smaller core change than the
parser fix and could be bundled with the prerequisite session.

The plan assumes Option A for self-containment. If the implementation reveals
significant complexity, escalate to Option B via the MIT core prerequisite list.
