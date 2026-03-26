# TDD Plan: GraphAccess Enterprise Integration

**Created**: 2026-03-26
**Branch**: `feature/graph-access-trait`
**Status**: PENDING IMPLEMENTATION

---

## Context

The `GraphAccess` trait is fully implemented in the MIT repo `tessera-graph`
(commit `216f728`, branch `feature/graph-access-trait`). The enterprise repo
already imports it via the path dependency `tessera-graph = { path =
"../tessera-graph", features = ["enterprise-helpers"] }`.

**Stack detected**: Rust, workspace with 13 crates, no async in storage layer,
Tokio in `tessera-server`.

**Conventions observed**:
- Integration tests live in `crates/<crate>/tests/`, one file per concern.
- Test helpers (`clearance()`, `label()`, `make_graph_with_node()`) are defined
  as free functions at the top of each test file — no shared helper modules in
  `tessera-storage-enterprise`.
- Production code never uses `#[cfg(test)]` modules inside `src/`; all tests
  are external crates.
- Copyright header `// Copyright 2026 BelowZero Security OU. All rights reserved.`
  on every new file.
- Workspace `clippy::all = deny`, `clippy::pedantic = warn` — all warnings are
  treated as errors.

**Affects hot path**: `import_gql`, `import_nodes_csv`, `import_edges_csv`,
`import_json` are bulk-write paths but not the query hot path. Performance
regression guards are not required for these changes.

---

## What Is Already Done (Do Not Reimplement)

The following items are COMPLETE in the enterprise repo on the current branch.
Reading the code confirms this — do not touch them.

| Item | File | Status |
|------|------|--------|
| `SecureGraph<'g, G: GraphAccess>` struct | `crates/tessera-storage-enterprise/src/lbac.rs` | DONE |
| `SecureGraphRef<'g, G: GraphAccess>` struct | `crates/tessera-storage-enterprise/src/lbac.rs` | DONE |
| `impl GraphAccess for SecureGraph<'_, G>` | `crates/tessera-storage-enterprise/src/lbac.rs` | DONE |
| `impl GraphAccess for SecureGraphRef<'_, G>` | `crates/tessera-storage-enterprise/src/lbac.rs` | DONE |
| `execute_mut<G: GraphAccess>(graph: &mut G, ...)` | `crates/tessera-storage-enterprise/src/gql/mod.rs` | DONE |
| Server wiring: `SecureGraph::new(&mut *graph, clearance)` | `crates/tessera-server/src/bolt_handler.rs:448` | DONE |
| Server wiring: `SecureGraphRef::new(&*graph, clearance)` | `crates/tessera-server/src/bolt_handler.rs:436` | DONE |
| All LBAC filter helpers generic over `G: GraphAccess` | `crates/tessera-storage-enterprise/src/lbac.rs` (`filter` module) | DONE |

---

## The Actual Gap

The `tessera-import` crate has three public functions that still take `&mut Graph`
directly instead of `&mut G where G: GraphAccess`:

| Function | File | Current signature |
|----------|------|-------------------|
| `import_gql` | `crates/tessera-import/src/gql_import/mod.rs:32` | `pub fn import_gql(graph: &mut Graph, gql_text: &str)` |
| `import_nodes_csv` | `crates/tessera-import/src/csv/mod.rs:99` | `pub fn import_nodes_csv(graph: &mut Graph, csv: &str)` |
| `import_edges_csv` | `crates/tessera-import/src/csv/mod.rs:190` | `pub fn import_edges_csv(graph: &mut Graph, csv: &str)` |
| `import_json` | `crates/tessera-import/src/json/mod.rs:52` | `pub fn import_json(graph: &mut Graph, json_text: &str)` |

**Security implication**: these import paths bypass LBAC entirely. An
administrator importing data via the CLI or a direct API call goes through
`&mut Graph`, not through `SecureGraph`, meaning no Bell-LaPadula enforcement
is applied and no security labels are stamped on imported nodes and edges.

**Call sites to migrate after the function signatures change**:
- `crates/tessera-import/tests/gql_import_test.rs` — calls `import_gql(&mut g, ...)` where `g: Graph`
- `crates/tessera-import/tests/csv_import_test.rs` — calls `import_nodes_csv` / `import_edges_csv` where `g: Graph`
- `crates/tessera-import/tests/json_import_test.rs` — calls `import_json` where `g: Graph`
- `crates/tessera-import/tests/round_trip_test.rs` — likely calls importers with `Graph`

These test call sites pass `&mut g` where `g: Graph`. Because `Graph: GraphAccess`
(implemented in `tessera-graph/src/access.rs`), changing the signature to
`&mut G: GraphAccess` is backward-compatible: existing tests continue to compile
without modification. This is the key property that makes the migration safe.

---

## Decisions Already Made

All architectural decisions are in `.private/design-graph-access-trait.md`.
The design document §6 focuses only on `execute_mut` (already done), but the
same principle extends to the import functions: any function that writes to a
graph must accept `&mut G: GraphAccess` so that `SecureGraph` can intercept
the writes.

No additional architectural decisions are needed. No blockers.

---

## Plan de Ejecución

### Cycle 1: `import_gql` Generic Over `GraphAccess`

**RED**
Write test `import_gql_accepts_secure_graph` in new file
`crates/tessera-import/tests/import_via_secure_graph_test.rs`.

```rust
// Verify that import_gql works with SecureGraph, not just Graph.
// This is the key property: the import path goes through LBAC enforcement.
use tessera_auth::lbac::{Clearance, SecurityPolicy};
use tessera_graph::{Graph, GraphAccess};
use tessera_import::gql_import::import_gql;
use tessera_storage_enterprise::lbac::SecureGraph;
use std::collections::BTreeSet;

#[test]
fn import_gql_accepts_secure_graph() {
    let mut g = Graph::new();
    let clearance = Clearance::new(5, BTreeSet::new());
    let mut sg = SecureGraph::new(&mut g, clearance);
    let summary = import_gql(&mut sg, "CREATE (:Person {name: 'Alice'})").unwrap();
    assert_eq!(summary.statements_executed, 1);
    assert_eq!(summary.nodes_created, 1);
    // The node exists in the underlying graph
    assert_eq!(g.node_count(), 1);
    // Security label was injected by SecureGraph (Bell-LaPadula)
    let id = g.nodes_by_label("Person")[0];
    let raw = g.node(id).unwrap();
    let label = SecurityPolicy::extract_label(raw.properties());
    assert_eq!(label.level, 5);
}
```

Assert: `import_gql` compiles with `&mut SecureGraph`, and security labels are
applied to imported nodes because SecureGraph intercepts the `add_node` call.

This test fails to compile with the current `pub fn import_gql(graph: &mut Graph, ...)`
signature because `&mut SecureGraph` is not `&mut Graph`.

**GREEN**
- File: `crates/tessera-import/src/gql_import/mod.rs`
  - Change signature from:
    `pub fn import_gql(graph: &mut Graph, gql_text: &str) -> ImportResult<GqlImportSummary>`
  - To:
    `pub fn import_gql<G: GraphAccess>(graph: &mut G, gql_text: &str) -> ImportResult<GqlImportSummary>`
  - Remove `use tessera_graph::Graph;` from imports (Graph is no longer referenced
    in the function signature — only `GraphAccess` is needed)
  - Add `use tessera_graph::GraphAccess;` import
  - Body: no changes needed — `tessera_storage_enterprise::gql::execute_mut(graph, &m)`
    already accepts `&mut G: GraphAccess`, so it compiles unchanged.

**REFACTOR**: none. The change is purely in the signature.

---

### Cycle 2: `import_gql` LBAC Enforcement Verification

**RED**
In the same file `crates/tessera-import/tests/import_via_secure_graph_test.rs`:

```rust
#[test]
fn import_gql_lbac_enforced_node_invisible_below_clearance() {
    let mut g = Graph::new();
    // Import with level-5 clearance — nodes get stamped at level 5
    let clearance_high = Clearance::new(5, BTreeSet::new());
    {
        let mut sg = SecureGraph::new(&mut g, clearance_high);
        import_gql(&mut sg, "CREATE (:Secret {name: 'classified'})").unwrap();
    }
    // A level-1 clearance user cannot see the imported node via SecureGraph
    let clearance_low = Clearance::new(1, BTreeSet::new());
    let sg_low = SecureGraph::new(&mut g, clearance_low);
    assert_eq!(sg_low.node_count(), 0, "node stamped at level 5 must be invisible at level 1");
    // But the raw graph still has it
    assert_eq!(g.node_count(), 1);
}
```

Assert: the security label stamped during import is enforced on subsequent reads
through a lower-clearance `SecureGraph`. This is the core LBAC property that
was previously broken because `import_gql` bypassed `SecureGraph`.

**GREEN**: Satisfied by Cycle 1's change. `SecureGraph::add_node` injects the
caller's clearance level as the security label (Bell-LaPadula no-write-down,
`lbac.rs:347`). No additional code needed.

**REFACTOR**: none.

---

### Cycle 3: `build_lookup_index` and `find_node_by_label_and_prop` Generic Over `GraphAccess`

`import_edges_csv` and `import_json` both call `build_lookup_index(graph)`, which
is defined in `crates/tessera-import/src/node_lookup.rs:68` as
`pub fn build_lookup_index(graph: &Graph)`. If the public importer functions become
generic but `build_lookup_index` stays concrete, the code will not compile.
This cycle must come before Cycles 4 and 5.

**RED**
In `crates/tessera-import/tests/import_via_secure_graph_test.rs` (or in a new
test added to `crates/tessera-import/tests/node_lookup_test.rs`):

```rust
#[test]
fn build_lookup_index_works_with_secure_graph_ref() {
    use tessera_import::node_lookup::build_lookup_index;
    let mut g = Graph::new();
    let clearance = Clearance::new(3, BTreeSet::new());
    // Insert nodes at clearance level 3
    {
        let mut sg = SecureGraph::new(&mut g, clearance.clone());
        GraphAccess::add_node(&mut sg, "Person", [("name".to_owned(), tessera_graph::Property::String("Alice".to_owned()))].into_iter().collect()).unwrap();
    }
    // build_lookup_index via SecureGraph (only visible nodes are indexed)
    let sg = tessera_storage_enterprise::lbac::SecureGraphRef::new(&g, clearance.clone());
    let index = build_lookup_index(&sg).unwrap();
    assert!(!index.is_empty());
}
```

Assert: `build_lookup_index` compiles with `&SecureGraphRef`. This test fails
to compile with the current `pub fn build_lookup_index(graph: &Graph)` signature.

**GREEN**
- File: `crates/tessera-import/src/node_lookup.rs`
  - Change `build_lookup_index` signature from:
    `pub fn build_lookup_index(graph: &Graph) -> ImportResult<NodeLookupIndex>`
  - To:
    `pub fn build_lookup_index<G: GraphAccess>(graph: &G) -> ImportResult<NodeLookupIndex>`
  - Change `find_node_by_label_and_prop` signature from:
    `pub fn find_node_by_label_and_prop(graph: &Graph, label: &str, ...)`
  - To:
    `pub fn find_node_by_label_and_prop<G: GraphAccess>(graph: &G, label: &str, ...)`
  - Replace `use tessera_graph::Graph;` with `use tessera_graph::GraphAccess;`
    (add `NodeId` and `Property` to the existing `tessera_graph` use if needed)
  - Body: calls `graph.node_ids()`, `graph.node(id)`, `graph.nodes_by_label(...)`,
    `graph.node(id)` — all are `GraphAccess` methods, no body changes needed.

**REFACTOR**: none.

---

### Cycle 4: `import_nodes_csv` and `import_edges_csv` Generic Over `GraphAccess`

**RED**
In `crates/tessera-import/tests/import_via_secure_graph_test.rs`:

```rust
#[test]
fn import_nodes_csv_accepts_secure_graph() {
    let mut g = Graph::new();
    let clearance = Clearance::new(3, BTreeSet::new());
    let mut sg = SecureGraph::new(&mut g, clearance);
    let csv = "label,name\nPerson,Alice\nPerson,Bob\n";
    let count = tessera_import::csv::import_nodes_csv(&mut sg, csv).unwrap();
    assert_eq!(count, 2);
    assert_eq!(g.node_count(), 2);
    // Both nodes carry the clearance label
    for id in g.nodes_by_label("Person") {
        let raw = g.node(id).unwrap();
        let lbl = SecurityPolicy::extract_label(raw.properties());
        assert_eq!(lbl.level, 3);
    }
}
```

Assert: `import_nodes_csv` compiles with `&mut SecureGraph`, nodes receive LBAC labels.

**GREEN**
- File: `crates/tessera-import/src/csv/mod.rs`
  - Change signature of `import_nodes_csv` from:
    `pub fn import_nodes_csv(graph: &mut Graph, csv: &str) -> ImportResult<usize>`
  - To:
    `pub fn import_nodes_csv<G: GraphAccess>(graph: &mut G, csv: &str) -> ImportResult<usize>`
  - Change signature of `import_edges_csv` from:
    `pub fn import_edges_csv(graph: &mut Graph, csv: &str) -> ImportResult<usize>`
  - To:
    `pub fn import_edges_csv<G: GraphAccess>(graph: &mut G, csv: &str) -> ImportResult<usize>`
  - Replace `use tessera_graph::Graph;` with `use tessera_graph::GraphAccess;`
    (or add `GraphAccess` to the existing use statement; keep `Graph` only if
    it is referenced in the function bodies for type annotations)
  - Body: calls to `graph.add_node(...)` and `graph.add_edge(...)` already use
    the `GraphAccess` methods — no body changes needed.

**REFACTOR**: none.

---

### Cycle 5: CSV LBAC Enforcement Verification

**RED**
In `crates/tessera-import/tests/import_via_secure_graph_test.rs`:

```rust
#[test]
fn import_edges_csv_lbac_enforced() {
    let mut g = Graph::new();
    let clearance = Clearance::new(2, BTreeSet::new());
    // First import the nodes so we have IDs to reference
    {
        let mut sg = SecureGraph::new(&mut g, clearance.clone());
        tessera_import::csv::import_nodes_csv(&mut sg, "label,name\nPerson,Alice\nPerson,Bob\n")
            .unwrap();
    }
    assert_eq!(g.node_count(), 2);
    // import_edges_csv CSV format: source_label,source_prop,source_value,
    // target_label,target_prop,target_value,rel_label (7 required columns)
    let edge_csv = "source_label,source_prop,source_value,target_label,target_prop,target_value,rel_label\n\
                    Person,name,Alice,Person,name,Bob,KNOWS\n";
    {
        let mut sg = SecureGraph::new(&mut g, clearance.clone());
        tessera_import::csv::import_edges_csv(&mut sg, edge_csv).unwrap();
    }
    // A level-0 user cannot see the edge (stamped at level 2)
    let sg_low = SecureGraph::new(&mut g, Clearance::new(0, BTreeSet::new()));
    assert_eq!(sg_low.edge_count(), 0);
}
```

Assert: edges imported via `import_edges_csv` receive LBAC labels and are
invisible to lower-clearance readers.

**GREEN**: Satisfied by Cycle 4's signature change. `SecureGraph::add_edge`
injects the caller's clearance level. No additional code needed.

**REFACTOR**: none.

---

### Cycle 6: `import_json` Generic Over `GraphAccess`

**RED**
In `crates/tessera-import/tests/import_via_secure_graph_test.rs`:

```rust
#[test]
fn import_json_accepts_secure_graph() {
    let mut g = Graph::new();
    let clearance = Clearance::new(4, BTreeSet::new());
    let mut sg = SecureGraph::new(&mut g, clearance);
    let json = r#"{"nodes":[{"label":"Device","properties":{"id":"d1"}}],"edges":[]}"#;
    let summary = tessera_import::json::import_json(&mut sg, json).unwrap();
    assert_eq!(summary.nodes_imported, 1);
    assert_eq!(g.node_count(), 1);
    let id = g.nodes_by_label("Device")[0];
    let raw = g.node(id).unwrap();
    let lbl = SecurityPolicy::extract_label(raw.properties());
    assert_eq!(lbl.level, 4);
}
```

Assert: `import_json` compiles with `&mut SecureGraph`, nodes receive LBAC labels.

`ImportJsonSummary` field names (confirmed from `src/json/mod.rs`):
- `nodes_imported: usize` (line 17)
- `edges_imported: usize` (line 19)

**GREEN**
- File: `crates/tessera-import/src/json/mod.rs`
  - Change signature of `import_json` from:
    `pub fn import_json(graph: &mut Graph, json_text: &str) -> ImportResult<ImportJsonSummary>`
  - To:
    `pub fn import_json<G: GraphAccess>(graph: &mut G, json_text: &str) -> ImportResult<ImportJsonSummary>`
  - Replace `use tessera_graph::Graph;` with `use tessera_graph::GraphAccess;`
    (verify: line 7 of `src/json/mod.rs` has `use tessera_graph::Graph;`)
  - Body: calls `graph.add_node(...)`, `graph.add_edge(...)`, and
    `build_lookup_index(graph)` — all compile unchanged because
    `build_lookup_index` was made generic in Cycle 3. No body changes needed.

**REFACTOR**: none.

---

### Cycle 7: Existing Import Tests Still Pass (Regression Guard)

This cycle has no new code. It verifies backward compatibility.

**Rationale**: All four import functions previously took `&mut Graph`. After
the signature change to `&mut G: GraphAccess`, the existing tests pass `&mut g`
where `g: Graph`. Because `Graph: GraphAccess` (implemented in
`tessera-graph/src/access.rs`), Rust infers `G = Graph` and the code compiles
unchanged. No existing test file needs to be modified.

**Verification command**:
```
cargo test -p tessera-import
```

Expected: all tests in `crates/tessera-import/tests/` pass without modification.

**If any test fails**: the only valid fix is to adjust the signature change in
the GREEN step of the relevant cycle — do NOT modify the existing test files to
paper over a regression.

---

## Final Cycle: Wiring and Integration Verification

This is a static analysis step. No new code.

### Wiring Checklist

Run these grep commands and confirm each result:

**1. Every new `pub fn` with `GraphAccess` bound has at least one call site in production code**

```
grep -rn "import_gql\|import_nodes_csv\|import_edges_csv\|import_json" \
  crates/ --include="*.rs" | grep -v "^.*tests/\|^.*//\|fn import_"
```
Expected: at minimum, the `tessera-cli` or a future server import endpoint
calls these functions. If no production call site exists yet (the CLI might
call them directly on `&mut Graph` before this plan), confirm that the function
is still exported and usable — it does not need a production call site if it
is a library function exposed for consumers. Document the result.

**2. `SecureGraph` and `SecureGraphRef` implement `GraphAccess`**

```
grep -n "impl GraphAccess for Secure" \
  crates/tessera-storage-enterprise/src/lbac.rs
```
Expected: two lines — `impl<G: GraphAccess> GraphAccess for SecureGraph<'_, G>`
and `impl<G: GraphAccess> GraphAccess for SecureGraphRef<'_, G>`.

**3. Server uses the trait-based path (not bypassing SecureGraph)**

```
grep -n "SecureGraph\|SecureGraphRef\|execute_mut" \
  crates/tessera-server/src/bolt_handler.rs
```
Expected:
- Line 436: `SecureGraphRef::new(&*graph, clearance)` — read path
- Line 448: `SecureGraph::new(&mut *graph, clearance)` — write path
- Line 449: `execute_mut(&mut secure, m)` — mutation through SecureGraph

**4. No stale `&Graph` or `&mut Graph` references remain in import function signatures**

```
grep -n "fn import_gql\|fn import_nodes_csv\|fn import_edges_csv\|fn import_json\|fn build_lookup_index\|fn find_node_by_label_and_prop" \
  crates/tessera-import/src/gql_import/mod.rs \
  crates/tessera-import/src/csv/mod.rs \
  crates/tessera-import/src/json/mod.rs \
  crates/tessera-import/src/node_lookup.rs
```
Expected: all six signatures show `G: GraphAccess` type parameter, no `Graph`
in the parameter position.

**5. No `use tessera_graph::Graph` remaining in import source files (only needed if Graph is used in the body)**

Verify each of the three files. It is acceptable to keep `use tessera_graph::Graph`
if `Graph` appears in a function body for any reason (e.g., a default
constructor in a test). If `Graph` only appeared in the old function signature,
remove it.

**6. `tessera-import` Cargo.toml has `tessera-storage-enterprise` dependency (needed if the new test file imports it)**

Check `crates/tessera-import/Cargo.toml`. The new test file
`import_via_secure_graph_test.rs` imports `tessera_storage_enterprise::lbac::SecureGraph`
and `tessera_auth::lbac::Clearance`. If these are not in `[dev-dependencies]`,
add them.

```
grep -n "tessera-storage-enterprise\|tessera-auth" \
  crates/tessera-import/Cargo.toml
```

If absent, add to `[dev-dependencies]`:
```toml
tessera-storage-enterprise = { workspace = true }
tessera-auth               = { workspace = true }
```

**7. All tests pass**

```
cargo test -p tessera-storage-enterprise
cargo test -p tessera-import
cargo test -p tessera-server
cargo clippy --workspace --all-targets
```
All must return 0 failures and 0 warnings.

### Wiring Checklist (must pass before merge)

- [ ] `build_lookup_index` signature uses `G: GraphAccess` — not `&Graph`
- [ ] `find_node_by_label_and_prop` signature uses `G: GraphAccess` — not `&Graph`
- [ ] `import_gql` signature uses `G: GraphAccess` — not `&mut Graph`
- [ ] `import_nodes_csv` signature uses `G: GraphAccess` — not `&mut Graph`
- [ ] `import_edges_csv` signature uses `G: GraphAccess` — not `&mut Graph`
- [ ] `import_json` signature uses `G: GraphAccess` — not `&mut Graph`
- [ ] `SecureGraph` implements `GraphAccess` (already done — confirm not broken)
- [ ] `SecureGraphRef` implements `GraphAccess` (already done — confirm not broken)
- [ ] Server `bolt_handler.rs` still uses `SecureGraph`/`SecureGraphRef` (already done)
- [ ] `execute_mut` still uses `G: GraphAccess` (already done — confirm not broken)
- [ ] New test file `import_via_secure_graph_test.rs` exists with at least 6 tests
- [ ] All `cargo test -p tessera-import` pass — existing tests unchanged
- [ ] All `cargo test -p tessera-storage-enterprise` pass — existing tests unchanged
- [ ] `cargo clippy --workspace --all-targets` — 0 warnings

---

## Estimation

| Phase | Time |
|-------|------|
| Cycle 1 — `import_gql` signature + RED test | 20 min |
| Cycle 2 — LBAC enforcement test for `import_gql` | 10 min |
| Cycle 3 — `build_lookup_index` / `find_node_by_label_and_prop` generic + RED test | 20 min |
| Cycle 4 — CSV importers signature change + RED test | 20 min |
| Cycle 5 — CSV LBAC enforcement test | 15 min |
| Cycle 6 — `import_json` signature change + RED test | 20 min |
| Cycle 7 — regression verification (`cargo test -p tessera-import`) | 10 min |
| Final cycle — static analysis + checklist | 15 min |
| **Total** | **~130 min** |

---

## Success Criteria

- [ ] `cargo test -p tessera-import` — all existing tests pass, at least 6 new tests pass
- [ ] `cargo test -p tessera-storage-enterprise` — all existing tests pass unchanged
- [ ] `cargo clippy --workspace --all-targets` — 0 warnings (treated as errors)
- [ ] `build_lookup_index` and `find_node_by_label_and_prop` accept `&G: GraphAccess`
- [ ] The four import functions accept `&mut G: GraphAccess`
- [ ] `import_gql_accepts_secure_graph` passes — import path goes through LBAC
- [ ] `import_gql_lbac_enforced_node_invisible_below_clearance` passes — security label applied on import
- [ ] `build_lookup_index_works_with_secure_graph_ref` passes — index built from filtered view
- [ ] `import_nodes_csv_accepts_secure_graph` passes — CSV nodes receive LBAC labels
- [ ] `import_edges_csv_lbac_enforced` passes — CSV edges receive LBAC labels
- [ ] `import_json_accepts_secure_graph` passes — JSON nodes receive LBAC labels
- [ ] Existing `gql_import_test.rs`, `csv_import_test.rs`, `json_import_test.rs`,
      `node_lookup_test.rs` tests pass WITHOUT modification — backward compatible

---

## Files Touched

| File | Change |
|------|--------|
| `crates/tessera-import/src/node_lookup.rs` | Signature change for `build_lookup_index` and `find_node_by_label_and_prop` |
| `crates/tessera-import/src/gql_import/mod.rs` | Signature change only |
| `crates/tessera-import/src/csv/mod.rs` | Signature change only (2 functions: `import_nodes_csv`, `import_edges_csv`) |
| `crates/tessera-import/src/json/mod.rs` | Signature change only |
| `crates/tessera-import/Cargo.toml` | Add `tessera-storage-enterprise` and `tessera-auth` to `[dev-dependencies]` if absent |
| `crates/tessera-import/tests/import_via_secure_graph_test.rs` | New file — at least 6 integration tests |

**Files NOT touched** (already correct):
- `crates/tessera-storage-enterprise/src/lbac.rs` — complete
- `crates/tessera-storage-enterprise/src/gql/mod.rs` — complete
- `crates/tessera-server/src/bolt_handler.rs` — complete
- Any existing test file in `tessera-import/tests/` — backward compatible, no changes needed
