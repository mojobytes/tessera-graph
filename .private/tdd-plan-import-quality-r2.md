# TDD Plan: tessera-import Re-Review Fixes (Round 2)

## Context

Eight targeted fixes across `tessera-import`: three critical correctness/performance
issues and five impactful recommended improvements surfaced in a second review pass.
The work spans `node_lookup.rs`, `property_coerce.rs`, `csv/mod.rs`,
`json/mod.rs`, `gql_import/mod.rs`, and the round-trip test file.

**Stack detected**: Rust / Cargo workspace
**Conventions**: `tests/<name>_test.rs`, copyright header on every file,
`#![deny(clippy::all)]` + `warn(pedantic, nursery)`, `thiserror` for errors
**Affects hot path**: Yes — `find_node_in_index` is called once per edge during
import; every allocation saved is multiplied by edge count.

---

## Decisions Previas Necesarias

None. All findings have clear, agreed-upon fixes.

---

## Plan de Ejecucion

### Fase 1: Extract `is_valid_property_key` (enables Findings 2 and 4)

This is the shared bool predicate that unblocks everything that follows.

1. [ ] Add `is_valid_property_key` to `property_coerce.rs` (15 min)
   - Archivo: `crates/tessera-import/src/property_coerce.rs`
   - Accion: Add pure bool function above `validate_property_key`.
   - Signature: `pub fn is_valid_property_key(key: &str) -> bool`
   - Logic: mirrors the char-by-char check in `validate_property_key` but
     returns `bool` instead of `Result`.
   - Refactor `validate_property_key` to delegate to `is_valid_property_key`
     so there is zero duplication.
   - Output: single source of truth for key validation; no dead code.

2. [ ] Write test for `is_valid_property_key` (10 min)
   - Archivo: `crates/tessera-import/tests/property_coerce_test.rs` (new file)
   - Test cases (all must pass before moving on):
     - `"name"` → true
     - `"_private"` → true
     - `"prop_1"` → true
     - `""` → false (empty)
     - `"1bad"` → false (starts with digit)
     - `"has-dash"` → false (hyphen)
     - `"has space"` → false (space)
     - `"has\0null"` → false (null byte — also guards the lookup key separator)

---

### Fase 2: Wire Key Validation Into Importers (Finding 2)

`ImportError::InvalidPropertyKey` currently has zero emit sites. Fix that.

3. [ ] Validate property keys during CSV node import (20 min)
   - Archivo: `crates/tessera-import/src/csv/mod.rs`
   - Accion: In `import_nodes_csv`, after building `prop_keys` from headers
     (line 104), iterate and call `is_valid_property_key` on each key. On
     failure, return `ImportError::InvalidPropertyKey(key.to_owned())`.
   - Same check in `import_edges_csv` for `edge_prop_keys` (line 183).
   - Output: bad header keys are rejected at import time, not silently stored.

4. [ ] Validate property keys during JSON node import (20 min)
   - Archivo: `crates/tessera-import/src/json/mod.rs`
   - Accion: In the `for (k, v) in props_obj` loop inside `import_json` (both
     the node properties loop ~line 81 and the edge properties loop ~line 121),
     call `is_valid_property_key(k)` before inserting. Return
     `ImportError::InvalidPropertyKey(k.clone())` on failure.
   - Output: malformed JSON property keys are rejected.

5. [ ] Write tests for key validation rejection (15 min)
   - Archivo: `crates/tessera-import/tests/csv_import_test.rs`
   - New test `csv_import_rejects_invalid_property_key_in_header`: CSV with
     header `label,1bad_key` must return `Err(ImportError::InvalidPropertyKey)`.
   - Archivo: `crates/tessera-import/tests/json_import_test.rs`
   - New test `json_import_rejects_invalid_property_key`: JSON node with
     `"properties": {"1bad": 1}` must return
     `Err(ImportError::InvalidPropertyKey("1bad"))`.

---

### Fase 3: Fix CSV Unclosed Quote Detection (Finding 3 — CRITICAL)

6. [ ] Change `parse_csv_line` signature to return `Result` (20 min)
   - Archivo: `crates/tessera-import/src/csv/mod.rs`
   - Accion: Change return type from `Vec<String>` to
     `Result<Vec<String>, &'static str>`.
   - After the `while` loop, add:
     ```rust
     if in_quotes {
         return Err("unclosed quoted field");
     }
     ```
   - Push the last `current` field only on success path.
   - Update all three call sites (`import_nodes_csv` line 96, line 113, and
     `import_edges_csv` lines 171, 195) to propagate with `?`, mapping the
     `&'static str` into `ImportError::CsvParse { row, reason }`.
   - Header parse errors use `row: 0`. Data row errors use the current
     `row_num`.
   - Output: malformed CSV is caught; no silent data corruption.

7. [ ] Write test for unclosed quote (10 min)
   - Archivo: `crates/tessera-import/tests/csv_import_test.rs`
   - New test `csv_import_rejects_unclosed_quote`: input
     `"label,name\nPerson,\"Alice"` (no closing quote) must return
     `Err(ImportError::CsvParse { row: 2, .. })` where reason contains
     "unclosed".

---

### Fase 4: Eliminate Triple Allocation in `find_node_in_index` (Finding 1 — CRITICAL)

8. [ ] Change `NodeLookupIndex` key from tuple to single String (25 min)
   - Archivo: `crates/tessera-import/src/node_lookup.rs`
   - Accion: Change the type alias from
     `HashMap<(String, String, String), NodeId>` to
     `HashMap<String, NodeId>`.
   - Add a private inline helper:
     ```rust
     #[inline]
     fn lookup_key(label: &str, prop_key: &str, prop_value: &str) -> String {
         format!("{label}\0{prop_key}\0{prop_value}")
     }
     ```
     The null byte `\0` cannot appear in validated label/key strings (covered
     by `is_valid_property_key`; labels are plain identifiers in practice).
   - Update `build_lookup_index`: replace `(label.clone(), prop_key.clone(), value_str)` with `lookup_key(&label, prop_key, &value_str)`.
   - Update `find_node_in_index`: replace the three `to_owned()` calls with
     a single `index.get(&lookup_key(label, prop_key, prop_value)).copied()`.
     No heap allocation needed for the lookup itself — use
     `HashMap::get` with a `String` key built once.
   - Note: This reduces lookup from 3 allocations to 1 (the format string).
     The further zero-allocation path (using a custom `Hash` impl) is a future
     optimization; document it with a `// TODO(perf)` comment.
   - Output: 2 of 3 allocations removed per lookup; type alias updated.

9. [ ] Verify existing node_lookup_test still passes (5 min)
   - Archivo: `crates/tessera-import/tests/node_lookup_test.rs`
   - No new tests needed — the performance regression guard already covers
     O(1) correctness.

---

### Fase 5: Fix Silent Node-Read Errors in `build_lookup_index` (Finding 5/6)

10. [ ] Change `build_lookup_index` to return `Result` (20 min)
    - Archivo: `crates/tessera-import/src/node_lookup.rs`
    - Accion: Change signature to
      `pub fn build_lookup_index(graph: &Graph) -> Result<NodeLookupIndex, ImportError>`.
    - Change `if let Ok(node) = graph.node(id)` to:
      ```rust
      let node = graph.node(id).map_err(|e| ImportError::GraphWrite(e.to_string()))?;
      ```
      Use `ImportError::GraphWrite` since `GraphRead` is not a variant;
      alternatively use `ImportError::CsvParse` with a sentinel row of 0 — but
      the cleaner option is to add `ImportError::GraphRead(String)` as a new
      variant for symmetry with `ExportError::GraphRead`.
    - Add `ImportError::GraphRead(String)` variant to `error.rs`:
      ```rust
      #[error("graph read error: {0}")]
      GraphRead(String),
      ```
    - Update `build_lookup_index` to use `ImportError::GraphRead`.
    - Update all three call sites: `csv/mod.rs` line 187, `json/mod.rs` line
      102, to propagate with `?`.
    - Output: node read errors surface rather than silently producing an
      incomplete index.

11. [ ] Write test for build_lookup_index error propagation (15 min)
    - Archivo: `crates/tessera-import/tests/node_lookup_test.rs`
    - New test `build_lookup_index_propagates_through_edge_import`: This is
      hard to trigger with a well-functioning graph; instead write a
      documentation test comment explaining the contract, and add a unit test
      that verifies `build_lookup_index` on an empty graph returns `Ok(empty)`.
    - The key regression protection is that callers now propagate errors
      (compile-time enforcement via `?`).

---

### Fase 6: Eliminate Double `node_ids()` Call in `export_nodes_csv` (Finding 7)

12. [ ] Deduplicate `node_ids()` call (10 min)
    - Archivo: `crates/tessera-import/src/csv/mod.rs`
    - Accion: Line 275 calls `graph.node_ids().len()` for capacity, then line
      287 calls `graph.node_ids()` again to iterate.
    - Fix: collect once: `let node_ids: Vec<_> = graph.node_ids();` before the
      key-collection loop. Use `node_ids.iter()` in the first loop, then
      sort/iterate over `node_ids` in the second loop. Capacity hint becomes
      `node_ids.len()`.
    - Output: single call; avoids potential inconsistency if the graph were
      modified concurrently (defensive hygiene).

---

### Fase 7: Fix Misleading `unwrap_or(usize::MAX)` (Finding 8)

13. [ ] Replace misleading fallback in `gql_import/mod.rs` (10 min)
    - Archivo: `crates/tessera-import/src/gql_import/mod.rs`
    - Lines 62–64: `usize::try_from(result.nodes_created).unwrap_or(usize::MAX)`
      silently saturates; `usize::MAX` added to a `usize` counter would
      immediately overflow in debug mode.
    - The actual type of `result.nodes_created` / `result.edges_created` must
      be checked. If it is `u64` or `i64`:
      - Use `usize::try_from(x).unwrap_or_else(|_| usize::MAX)` — same
        behaviour but self-documenting — is still wrong for the overflow reason.
      - Correct fix: use `saturating_add` on the counter:
        ```rust
        summary.nodes_created = summary.nodes_created
            .saturating_add(usize::try_from(result.nodes_created).unwrap_or(usize::MAX));
        ```
      - Or, if the domain guarantees the count fits in `usize` on any real
        workload, use `as usize` with a `#[allow(clippy::cast_possible_truncation)]`
        and a comment: `// Safe: node count never exceeds usize on supported platforms`.
      - Preferred approach: `saturating_add` — honest semantics, no panic.
    - Output: counter accumulation does not overflow in debug builds.

---

### Fase 8: Strengthen JSON Round-Trip Test (Finding 9)

14. [ ] Verify property values in `json_node_round_trip_preserves_properties` (15 min)
    - Archivo: `crates/tessera-import/tests/round_trip_test.rs`
    - Existing test (lines 138–155) only asserts count and label presence.
    - Extend (or add a sibling test named
      `json_node_round_trip_preserves_property_values`) that:
      1. Imports the round-tripped graph.
      2. Finds Alice by name.
      3. Asserts `age == Property::I64(30)` and `active == Property::Bool(true)`.
      4. Finds Bob by name.
      5. Asserts `age == Property::I64(25)` and `score == Property::F64(9.5)`.
    - Pattern mirrors the existing `csv_node_round_trip_preserves_integer_property`
      test so there is symmetry between CSV and JSON round-trip coverage.
    - Output: JSON import cannot silently drop or corrupt typed property values.

---

### Fase 9: Performance Regression Guard

The `find_node_in_index` function is in the edge-import hot path. The existing
test in `node_lookup_test.rs` already enforces O(1) behaviour via a timing
assert (500 nodes, 499 edges, < 500 ms). That guard remains valid post-refactor.

15. [ ] Confirm timing guard still holds after key-type change (5 min)
    - Run `cargo test -p tessera-import import_edges_csv_large_graph` and
      `import_edges_json_large_graph` after completing Fase 4.
    - No new benchmark file needed — the regression guard is already wired.
    - If either test exceeds 500 ms, investigate before merging.

---

## Estimacion Total

| Fase | Trabajo | Tiempo |
|------|---------|--------|
| 1 — Extract bool helper | Impl + test | 25 min |
| 2 — Wire key validation | Impl + tests | 55 min |
| 3 — Unclosed quote | Impl + test | 30 min |
| 4 — Reduce allocations | Impl | 30 min |
| 5 — Propagate read errors | Impl + test | 35 min |
| 6 — Deduplicate node_ids | Impl | 10 min |
| 7 — Fix unwrap_or | Impl | 10 min |
| 8 — JSON test | Test | 15 min |
| 9 — Regression guard verify | Verify | 5 min |
| **Total** | | **~3.5 h** |

---

## Orden de Ejecucion

Fases deben ejecutarse en orden: Fase 1 → 2 → 3 → 4 → 5 → 6 → 7 → 8 → 9.

Fase 1 is a prerequisite for Fase 2 (imports the bool helper).
Fase 4 and Fase 5 both modify `node_lookup.rs` — do Fase 4 first, then Fase 5
on the already-refactored file to avoid merge conflicts within the file.
Fases 6, 7, 8 are independent and can be batched after 5 if time allows.

---

## Criterios de Exito

- [ ] `cargo clippy -p tessera-import -- -D warnings` passes with zero warnings
- [ ] `cargo test -p tessera-import` passes (all existing + all new tests)
- [ ] `ImportError::InvalidPropertyKey` has at least 2 emit sites (CSV + JSON)
- [ ] `parse_csv_line` returns `Result`; unclosed quote returns `Err`
- [ ] `NodeLookupIndex` key is `String`, not a 3-tuple
- [ ] `build_lookup_index` returns `Result<NodeLookupIndex, ImportError>`
- [ ] `export_nodes_csv` calls `graph.node_ids()` exactly once
- [ ] `summary.nodes_created` accumulation uses `saturating_add`
- [ ] JSON round-trip test asserts all typed property values for both nodes
- [ ] Timing regression guard (`< 500 ms` for 499 edges) still passes
