# TDD Plan: Quality Review Fixes — GraphAccess Enterprise Integration

**Date:** 2026-03-26
**Branch:** `feature/graph-access-trait`
**Crate under change:** `crates/tessera-import`
**Test file:** `crates/tessera-import/tests/import_via_secure_graph_test.rs`

---

## Context

Six quality issues were identified in the enterprise integration review of
`tessera-import`. They affect correctness of error variants, semantic
completeness of `Cargo.toml`, test coverage for LBAC corner cases, and the
ability of export functions to operate through a `SecureGraphRef` (i.e.
returning a filtered view instead of raw data).

**Stack:** Rust 2024 edition, workspace with `clippy::all = deny`.
**Hot path:** Export functions are I/O utilities — not hot paths. No throughput
regression testing required.
**Convenciones observadas:** All integration tests live in
`crates/tessera-import/tests/`. Production sources live in
`crates/tessera-import/src/`. TDD cycles follow the pattern already established
in the existing test file.

---

## Decisions Previas Necesarias

None. All six items are well-scoped. No blocking architectural questions.

---

## Plan de Ejecución

### Cycle 1 — Cargo.toml: add `tessera-storage-enterprise` to `[dev-dependencies]`

**Why first:** Every subsequent test cycle either compiles today (because the
crate is in `[dependencies]`) or will break without this fix once Item 3 moves
the export functions away from `&Graph`. Making the semantic dependency
explicit first eliminates confusion.

- **RED:** No new test needed — the fix is purely semantic. The existing test
  suite must still compile and pass after the change to confirm the
  `[dev-dependencies]` entry is sufficient by itself.
- **GREEN:**
  - File: `crates/tessera-import/Cargo.toml`
  - Action: Modify — add `tessera-storage-enterprise = { workspace = true }` to
    the `[dev-dependencies]` section (after the existing `tessera-auth` line).
  - Result: `[dev-dependencies]` contains both `tessera-auth` and
    `tessera-storage-enterprise`.
- **REFACTOR:** None required.
- **Verify:** `cargo test -p tessera-import` passes with no new errors.

---

### Cycle 2 — Item 4: CSV importers use wrong error variant for graph write failures

**Location:** `crates/tessera-import/src/csv/mod.rs` lines 161–166
(`import_nodes_csv`) and 273–278 (`import_edges_csv`).

Both currently map graph write failures to `ImportError::CsvParse { row, reason }`.
`import_json` correctly maps them to `ImportError::GraphWrite(e.to_string())`.
The `GraphWrite` variant exists at `crates/tessera-import/src/error.rs:28`.

- **RED:**
  - File: `crates/tessera-import/tests/import_via_secure_graph_test.rs`
  - Add test `csv_node_graph_write_failure_returns_graph_write_error`:
    Use a mock or a `SecureGraph` with a read-only scenario that forces a write
    failure. However, since `SecureGraph` passes writes through and `Graph`
    never rejects `add_node`, the cleanest RED test for this item is a
    compilation-level change: write the test that documents the intent without
    being able to trigger the variant today, then fix the source so the variant
    is correct. Alternatively, write a test that verifies the error variant
    *display string* does NOT contain "CSV parse error" when a write fails.

    Practical approach: write an integration test that exercises `import_nodes_csv`
    and `import_edges_csv` with a type that implements `GraphAccess` and returns
    `Err` from `add_node` / `add_edge`. Since the project forbids `unsafe_code`
    and does not have a mock framework, create a minimal private failing adapter
    inside the test module using a zero-field struct that implements
    `GraphAccess` with all reads returning stub data and all writes returning
    `Err(tessera_graph::Error::NodeNotFound(tessera_graph::NodeId::from_u64(0)))`.

    - Test: `csv_node_write_error_variant_is_graph_write` — calls
      `import_nodes_csv` on the failing adapter and asserts
      `matches!(err, ImportError::GraphWrite(_))`, not `ImportError::CsvParse`.
    - Test: `csv_edge_write_error_variant_is_graph_write` — same but for
      `import_edges_csv`, asserts the same variant.
    - Assert: both tests **fail** (RED) because today the code returns
      `ImportError::CsvParse`.

- **GREEN:**
  - File: `crates/tessera-import/src/csv/mod.rs`
  - Line 161–166: replace `.map_err(|e| ImportError::CsvParse { row: row_num, reason: format!("graph write failed: {e}") })?`
    with `.map_err(|e| ImportError::GraphWrite(e.to_string()))?`
  - Line 273–278: replace `.map_err(|e| ImportError::CsvParse { row: row_num, reason: format!("graph write: {e}") })?`
    with `.map_err(|e| ImportError::GraphWrite(e.to_string()))?`
  - Also update the `import_nodes_csv` docstring at line 97–98 to say
    `Returns [`ImportError::GraphWrite`] if inserting a node into the graph
    fails.` (removing the stale `CsvParse` claim).
  - Also update the `import_edges_csv` docstring at line 189 similarly.

- **REFACTOR:** None — the change is a one-for-one variant swap.

- **Verify:** `cargo test -p tessera-import` shows the two new tests GREEN.
  No other tests must fail.

---

### Cycle 3 — Item 5: LBAC cycle 2 does not verify security label level

**Location:** `crates/tessera-import/tests/import_via_secure_graph_test.rs:40`
Test: `import_gql_lbac_enforced_node_invisible_below_clearance`

The test currently asserts invisibility but never checks that the node was
stamped at exactly level 5.

- **RED:**
  - File: `crates/tessera-import/tests/import_via_secure_graph_test.rs`
  - Extend the existing test body (after the `assert_eq!(g.node_count(), 1)` at
    line 55) to add:
    ```rust
    let id = g.nodes_by_label("Secret")[0];
    let raw = g.node(id).unwrap();
    let label = SecurityPolicy::extract_label(raw.properties());
    assert_eq!(label.level, 5, "node imported at clearance 5 must be stamped at level 5");
    ```
  - This test is already GREEN before the extension — no production code change
    needed. The "RED" here is that the assertion does not yet exist (gap).

- **GREEN:** Add the assertion. The test passes immediately because `SecureGraph`
  already stamps nodes at the writer's clearance level (confirmed in
  `crates/tessera-storage-enterprise/src/lbac.rs`).

- **REFACTOR:** None.

---

### Cycle 4 — Item 6: No test for `import_gql` edges via SecureGraph

**Location:** `crates/tessera-import/tests/import_via_secure_graph_test.rs`

- **RED:**
  - Add test `import_gql_edges_via_secure_graph`:
    1. Create `Graph::new()` and `SecureGraph::new(&mut g, clearance(3))`.
    2. Execute two CREATE node statements via `import_gql`.
    3. Execute one CREATE edge statement using GQL MATCH-style
       (use the format supported by the GQL importer — verify in
       `crates/tessera-import/src/gql_import/mod.rs`; edges are created via
       `execute_mut` which calls the GQL compiler).
       Note: as of current implementation, the GQL importer only supports
       CREATE node statements (`GqlStatement::Mutation`). Edge creation through
       GQL depends on whether the GQL compiler's CREATE supports edges.
       **Before writing this test, verify** by reading
       `crates/tessera-storage-enterprise/src/gql/mod.rs` to understand what
       `execute_mut` supports.

    If `execute_mut` supports edge creation via GQL CREATE:
    - Assert `g.edge_count() == 1`.
    - Assert the edge is invisible via `SecureGraphRef::new(&g, clearance(0))`.
    - Assert the edge is visible via `SecureGraphRef::new(&g, clearance(3))`.

    If `execute_mut` does NOT yet support edge creation via GQL, the test
    should instead document this as a known limitation:
    - Write a test `import_gql_edge_creation_not_yet_supported` that imports
      a GQL string containing a relationship CREATE and asserts the edge count
      remains 0 (documenting current behavior as a regression guard).

  - This test is RED because it does not exist yet.

- **GREEN:** Add the test. No production code change needed — this is pure
  test coverage addition.

- **REFACTOR:** None.

- **Prerequisite action:** Read `crates/tessera-storage-enterprise/src/gql/mod.rs`
  before writing the test to confirm which GQL CREATE forms are supported.

---

### Cycle 5 — Item 1: Document and test LBAC behavior in `import_edges_csv` with mixed clearances

**Locations:**
- `crates/tessera-import/src/csv/mod.rs:224` (`build_lookup_index` call in `import_edges_csv`)
- `crates/tessera-import/src/json/mod.rs:105` (`build_lookup_index` call in `import_json`)

**Root cause:** `build_lookup_index(graph)` calls `graph.node_ids()` which on a
`SecureGraph<level=2>` returns only nodes visible at level 2 (confirmed in
`crates/tessera-storage-enterprise/src/lbac.rs:68`, `secure_node_ids` filters
by clearance). Nodes imported at clearance 5 are invisible to the level-2
builder. The resulting `NodeNotFoundForEdge` error is indistinguishable from
"node does not exist" — the consumer cannot tell it is an LBAC denial.

This is a **documented behavior** item, not a code fix. The fix is:
1. Write a test that pins this behavior as a regression guard.
2. Update docstrings to warn callers explicitly.

- **RED:**
  - File: `crates/tessera-import/tests/import_via_secure_graph_test.rs`
  - Add test `import_edges_csv_returns_node_not_found_for_insufficiently_cleared_endpoint`:
    1. Create `Graph::new()`.
    2. Import two nodes with `SecureGraph::new(&mut g, clearance(5))` —
       stamped at level 5.
    3. Attempt to import an edge using `SecureGraph::new(&mut g, clearance(2))`.
    4. Assert the result is
       `Err(ImportError::NodeNotFoundForEdge { label: "Person", prop: "name", value: "Alice" })`.
    5. Assert `g.edge_count() == 0`.
    - This pins the behavior: at clearance 2, the lookup index cannot see
      level-5 nodes, so the edge import returns `NodeNotFoundForEdge`.
  - Same test pattern for `import_json`:
    Add test `import_json_returns_node_not_found_for_insufficiently_cleared_endpoint`:
    1. Same setup: nodes imported at level 5.
    2. Attempt JSON edge import using a level-2 `SecureGraph`.
    3. Assert `Err(ImportError::NodeNotFoundForEdge { ... })`.
  - Both tests are RED because they do not yet exist.

- **GREEN:** Add both tests. No production code change — this cycle documents
  existing behavior, not fixing it.

- **REFACTOR:** Update docstrings in production code to make the behavior
  explicit:
  - `crates/tessera-import/src/csv/mod.rs`: update the `# Errors` section of
    `import_edges_csv` (lines 183–189) to add:
    ```
    /// When `graph` is a [`SecureGraph`], `build_lookup_index` only indexes
    /// nodes that are visible at the writer's clearance level. Nodes imported
    /// at a higher clearance level will be invisible to the index, causing
    /// [`ImportError::NodeNotFoundForEdge`] — indistinguishable from a truly
    /// absent node. Callers must import nodes and edges at the same clearance
    /// level to avoid this.
    ```
  - `crates/tessera-import/src/json/mod.rs`: same note appended to the
    `import_json` docstring's `# Errors` section (before line 51).

---

### Cycle 6 — Item 3: Export functions generic over `GraphAccess`

**Locations:**
- `crates/tessera-import/src/csv/mod.rs:298` — `export_nodes_csv(graph: &Graph)`
- `crates/tessera-import/src/csv/mod.rs:375` — `export_edges_csv(graph: &Graph)`
- `crates/tessera-import/src/json/mod.rs:214` — `export_json(graph: &Graph)`
- `crates/tessera-import/src/gql_export/mod.rs:34` — `export_gql(graph: &Graph)`

All four functions use only methods from `GraphAccess`:
- `graph.node_ids()` — in trait
- `graph.node(id)` — in trait
- `graph.outgoing_edges(*id)` — in trait

They do NOT use any `Graph`-specific inherent methods. The change is a
signature substitution: `graph: &Graph` → `graph: &G` where `G: GraphAccess`.

The `use tessera_graph::Graph` import in `json/mod.rs:7` and
`gql_export/mod.rs:10` will need to be changed to `use tessera_graph::GraphAccess`.
The csv module already imports both (`use tessera_graph::{Graph, GraphAccess, Property}` at line 20) — `Graph` will be unused after the change.

- **RED:**
  - File: `crates/tessera-import/tests/import_via_secure_graph_test.rs`
  - Add four tests — one per export function — verifying that they compile and
    produce filtered output when called with a `SecureGraphRef`:

    Test `export_nodes_csv_via_secure_graph_ref_filters_by_clearance`:
    1. `Graph::new()`, import two nodes: one at clearance 3, one at clearance 5.
    2. Create `SecureGraphRef::new(&g, clearance(3))`.
    3. Call `export_nodes_csv(&sg_ref)`.
    4. Assert the CSV output contains exactly 1 data row (the level-3 node).
    5. Assert the CSV does NOT contain the label of the level-5 node.

    Test `export_edges_csv_via_secure_graph_ref_filters_by_clearance`:
    1. Import two nodes + one edge at clearance 3.
    2. `SecureGraphRef::new(&g, clearance(0))`.
    3. Call `export_edges_csv(&sg_ref)`.
    4. Assert output contains only the header row (no data rows).

    Test `export_json_via_secure_graph_ref_filters_by_clearance`:
    1. Import one node at clearance 5.
    2. `SecureGraphRef::new(&g, clearance(1))`.
    3. Call `export_json(&sg_ref)`.
    4. Assert the `"nodes"` array in the JSON output is empty.

    Test `export_gql_via_secure_graph_ref_filters_by_clearance`:
    1. Import one node at clearance 5 and one at clearance 1.
    2. `SecureGraphRef::new(&g, clearance(1))`.
    3. Call `export_gql(&sg_ref)`.
    4. Assert the output contains exactly one `CREATE` statement.

  - All four tests are RED because the signatures still take `&Graph`.

- **GREEN:**
  - File: `crates/tessera-import/src/csv/mod.rs`
    - Change `pub fn export_nodes_csv(graph: &Graph)` (line 298) to
      `pub fn export_nodes_csv<G: GraphAccess>(graph: &G)`.
    - Change `pub fn export_edges_csv(graph: &Graph)` (line 375) to
      `pub fn export_edges_csv<G: GraphAccess>(graph: &G)`.
    - Remove `Graph` from the `use tessera_graph::{Graph, GraphAccess, Property}` import (line 20)
      — it becomes `use tessera_graph::{GraphAccess, Property}`.
  - File: `crates/tessera-import/src/json/mod.rs`
    - Change `pub fn export_json(graph: &Graph)` (line 214) to
      `pub fn export_json<G: GraphAccess>(graph: &G)`.
    - Change `use tessera_graph::{Graph, GraphAccess}` (line 7) to
      `use tessera_graph::GraphAccess`.
  - File: `crates/tessera-import/src/gql_export/mod.rs`
    - Change `pub fn export_gql(graph: &Graph)` (line 34) to
      `pub fn export_gql<G: GraphAccess>(graph: &G)`.
    - Change `use tessera_graph::Graph` (line 10) to `use tessera_graph::GraphAccess`.
    - Update the body: `graph.node_ids()` is already in `GraphAccess`; no body
      changes needed. The `graph.node(id)` call is also in the trait.

- **REFACTOR:** Update docstrings on all four export functions to reflect the
  new generic signature, e.g.:
  `Export all nodes visible through `graph` to a CSV string. When `graph` is a
  [`SecureGraphRef`], only nodes accessible at the configured clearance level
  are included.`

- **Verify:** `cargo clippy -p tessera-import --all-targets -- -D warnings`
  passes. No stale `&Graph` signature remains in export functions.

---

### Cycle 7 (Final) — Static Verification

This cycle is a checklist of `cargo` and `grep`-level checks. No code is
written here — each check either passes or surfaces a regression.

1. [ ] `cargo test -p tessera-import` — all tests pass.
2. [ ] `cargo clippy -p tessera-import --all-targets -- -D warnings` — zero warnings.
3. [ ] Confirm no stale `&Graph` in export signatures:
   - Pattern to grep: `fn export_nodes_csv(graph: &Graph)` — must not appear.
   - Pattern to grep: `fn export_edges_csv(graph: &Graph)` — must not appear.
   - Pattern to grep: `fn export_json(graph: &Graph)` — must not appear.
   - Pattern to grep: `fn export_gql(graph: &Graph)` — must not appear.
4. [ ] Confirm `GraphWrite` variant is used for write errors in CSV importers:
   - Pattern to grep in `csv/mod.rs`: `ImportError::CsvParse` — must appear
     only for true parse errors (empty CSV, malformed header, malformed row,
     field count mismatch). Must NOT appear near `add_node` or `add_edge` call sites.
5. [ ] Confirm `[dev-dependencies]` in `Cargo.toml` contains
   `tessera-storage-enterprise = { workspace = true }`.
6. [ ] Confirm the two LBAC mixed-clearance tests exist in
   `import_via_secure_graph_test.rs`:
   - `import_edges_csv_returns_node_not_found_for_insufficiently_cleared_endpoint`
   - `import_json_returns_node_not_found_for_insufficiently_cleared_endpoint`
7. [ ] Confirm cycle-2 label-level assertion exists at the end of
   `import_gql_lbac_enforced_node_invisible_below_clearance`.
8. [ ] Confirm cycle-4 edge GQL test exists.
9. [ ] No previously-passing test changed from GREEN to RED.

---

## Estimacion Total

| Phase | Estimated time |
|---|---|
| Cycle 1 — Cargo.toml fix | 5 min |
| Cycle 2 — Error variant fix + docstrings | 20 min |
| Cycle 3 — Label-level assertion | 10 min |
| Cycle 4 — GQL edge test (includes reading gql/mod.rs) | 20 min |
| Cycle 5 — LBAC mixed-clearance tests + docstrings | 25 min |
| Cycle 6 — Export generics (4 functions, 4 tests) | 40 min |
| Cycle 7 — Static verification | 10 min |
| **Total** | **~2 hours** |

---

## Criterios de Exito

- [ ] All six items addressed with at least one test or documentation change each.
- [ ] `cargo test -p tessera-import` passes — green on all cycles.
- [ ] `cargo clippy -p tessera-import --all-targets -- -D warnings` clean.
- [ ] All four export functions have `<G: GraphAccess>` in their signatures.
- [ ] `ImportError::GraphWrite` is used for graph write failures in CSV importers.
- [ ] LBAC behavior under mixed clearances is documented in two docstrings and
  pinned by two regression tests.
- [ ] `[dev-dependencies]` in `tessera-import/Cargo.toml` explicitly lists
  `tessera-storage-enterprise`.
- [ ] No previously-passing test is broken.
