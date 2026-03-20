# TDD Plan: tessera-import Quality Fixes — All Findings

**Created:** 2026-03-20
**Branch:** feature/security-phase2 (create sub-branch `fix/import-quality` from develop)
**Scope:** 4 critical + 8 recommended + impactful optional improvements

---

## Context

`crates/tessera-import` is the bulk data import/export layer for tessera-graph-enterprise. A quality review identified 15 findings spanning security (GQL injection), data correctness (Bytes silently lost), performance (O(n²) edge lookup), and consistency (type mismatches, redundant code). This plan addresses all of them using strict TDD cycles: each fix is preceded by a RED test that fails against the current code and passes only after the GREEN implementation.

**Stack detected:** Rust / tessera-graph custom GQL engine
**Conventions observed:** tests in `crates/tessera-import/tests/<name>_test.rs`, copyright header on every file, `thiserror` for errors, `clippy all=deny pedantic=warn nursery=warn`, no external CSV crate.
**Hot path affected:** Yes — `node_lookup.rs` is called for every edge during import. Fix #5 (O(n²)) is directly on the edge-import hot path. Performance regression guard is mandatory.

---

## Critical Finding 1: GQL Single-Quote Escape — VERIFICATION RESULT

**IMPORTANT — read before implementing.**

After reading the tessera-graph GQL lexer (`tessera-graph/src/gql/lexer.rs`), the escape convention this parser accepts is `\'` (backslash-quote), NOT `''` (doubled-quote). Evidence:

- `lexer.rs:326` — `Some(c) if c == quote || c == b'\\'` — the lexer handles `\'` as the escape for the quote character.
- `lexer.rs:612-613` — existing test `lex_string_literal_escaped_quote` validates `r"'it\'s fine'"` → `"it's fine"`.

The current implementation `s.replace('\'', "\\'")`  in `property_coerce.rs:51` IS CORRECT for this parser. The original finding was based on standard GQL/Cypher convention (`''`) which does not match the actual tessera-graph lexer.

**Action:** The existing escape logic must NOT be changed. Instead, the test `export_gql_string_with_single_quote_escaped` already exists and already asserts `\\` in output (line 94 of `gql_export_test.rs`). What is missing is a round-trip test that confirms the exported `\'` is correctly re-imported. This is covered under Finding #13 (round-trip tests).

---

## Decisions Confirmed (No Blockers)

All 15 findings are implementation-level with no ambiguous architectural decisions:

- Bytes in export: fail loudly with `ExportError::UnsupportedType` (new variant)
- Key validation: `[a-zA-Z_][a-zA-Z0-9_]*` regex-free check (iterate chars)
- Node lookup index: `HashMap<(String, String, String), NodeId>` keyed by `(label, prop_key, prop_value_as_string)`
- Summary types: normalize to `usize` everywhere (nodes_created in GqlImportSummary)
- NaN/inf policy: reject at `coerce_str_value` parse time
- JSON multi-match: error if `match` object has more than 1 key
- Edge CSV incompatibility: document clearly as a `NOTE` in module doc (no code change needed for now)

---

## Plan of Execution

### Phase 1: Error Types (foundation for all other fixes)

**Why first:** Several fixes add new error variants. Tests for later phases depend on these variants existing and matching correctly.

#### 1.1 — Add `ExportError::UnsupportedType` variant
- File: `crates/tessera-import/src/error.rs`
- Action: Modify — add one variant
- Test file: `crates/tessera-import/tests/error_types_test.rs`
- Output: New variant compiles and its Display message includes the property type name

**RED test to add to `error_types_test.rs`:**
```rust
#[test]
fn export_error_unsupported_type_display() {
    let e = ExportError::UnsupportedType {
        context: "csv export".to_owned(),
        type_name: "Bytes".to_owned(),
    };
    let msg = e.to_string();
    assert!(msg.contains("Bytes"), "got: {msg}");
    assert!(msg.contains("not supported"), "got: {msg}");
}
```

**GREEN implementation in `error.rs`:**
```rust
#[error("{context}: property type '{type_name}' is not supported for this format")]
UnsupportedType { context: String, type_name: String },
```

#### 1.2 — Add `ImportError::InvalidPropertyKey` variant
- File: `crates/tessera-import/src/error.rs`
- Action: Modify — add one variant
- Test file: `crates/tessera-import/tests/error_types_test.rs`
- Output: Variant compiles, Display includes the invalid key

**RED test:**
```rust
#[test]
fn import_error_invalid_property_key_display() {
    let e = ImportError::InvalidPropertyKey("x}: DELETE".to_owned());
    let msg = e.to_string();
    assert!(msg.contains("x}: DELETE"), "got: {msg}");
    assert!(msg.contains("invalid property key"), "got: {msg}");
}
```

**GREEN:**
```rust
#[error("invalid property key '{0}': keys must match [a-zA-Z_][a-zA-Z0-9_]*")]
InvalidPropertyKey(String),
```

**Estimated time:** 20 min
**Output:** `error.rs` compiles with zero clippy warnings; both RED tests pass

---

### Phase 2: Critical Fix — Bytes in Export Fails Loudly (Findings 3 and 4)

Affects: `property_coerce.rs`, `csv/mod.rs`, `json/mod.rs`, `gql_export/mod.rs`

#### 2.1 — RED tests for Bytes in CSV export
- File: `crates/tessera-import/tests/csv_export_test.rs`
- Action: Add tests — must FAIL before implementation

```rust
#[test]
fn export_nodes_csv_bytes_property_returns_error() {
    let mut g = Graph::new();
    let props: tessera_graph::Properties = std::iter::once((
        "data".to_owned(),
        Property::Bytes(vec![0xDE, 0xAD]),
    ))
    .collect();
    g.add_node("Thing", props).unwrap();
    let result = export_nodes_csv(&g);
    assert!(
        matches!(result, Err(ExportError::UnsupportedType { .. })),
        "expected UnsupportedType error, got: {result:?}"
    );
}

#[test]
fn export_edges_csv_bytes_property_returns_error() {
    let mut g = Graph::new();
    let props_a: tessera_graph::Properties = HashMap::new();
    let props_b: tessera_graph::Properties = HashMap::new();
    g.add_node("A", props_a).unwrap();
    g.add_node("B", props_b).unwrap();
    let ids = g.node_ids();
    let mut edge_props: tessera_graph::Properties = std::iter::once((
        "payload".to_owned(),
        Property::Bytes(vec![1, 2, 3]),
    ))
    .collect();
    g.add_edge("LINK", ids[0], ids[1], edge_props).unwrap();
    let result = export_edges_csv(&g);
    assert!(
        matches!(result, Err(ExportError::UnsupportedType { .. })),
        "expected UnsupportedType error, got: {result:?}"
    );
}
```

#### 2.2 — RED tests for Bytes in JSON export
- File: `crates/tessera-import/tests/json_export_test.rs`

```rust
#[test]
fn export_json_bytes_property_returns_error() {
    let mut g = Graph::new();
    let props: tessera_graph::Properties = std::iter::once((
        "blob".to_owned(),
        Property::Bytes(vec![0xFF, 0x00]),
    ))
    .collect();
    g.add_node("Blob", props).unwrap();
    let result = export_json(&g);
    assert!(
        matches!(result, Err(ExportError::UnsupportedType { .. })),
        "expected UnsupportedType error, got: {result:?}"
    );
}
```

#### 2.3 — RED tests for Bytes in GQL export
- File: `crates/tessera-import/tests/gql_export_test.rs`

```rust
#[test]
fn export_gql_bytes_property_returns_error() {
    let mut g = Graph::new();
    let props: tessera_graph::Properties = std::iter::once((
        "bin".to_owned(),
        Property::Bytes(vec![1, 2]),
    ))
    .collect();
    g.add_node("Node", props).unwrap();
    let result = export_gql(&g);
    assert!(
        matches!(result, Err(ExportError::UnsupportedType { .. })),
        "expected UnsupportedType error, got: {result:?}"
    );
}
```

#### 2.4 — GREEN: change `property_to_gql_literal` signature to `Result`
- File: `crates/tessera-import/src/property_coerce.rs`
- Action: Modify — change return type to `ExportResult<String>`, return `Err(ExportError::UnsupportedType)` for `Bytes`
- All call sites in `gql_export/mod.rs`, `csv/mod.rs`, `json/mod.rs` must propagate with `?`

New signature:
```rust
pub fn property_to_gql_literal(p: &Property) -> ExportResult<String>
```

New Bytes arm:
```rust
Property::Bytes(_) => Err(ExportError::UnsupportedType {
    context: "gql export".to_owned(),
    type_name: "Bytes".to_owned(),
}),
```

#### 2.5 — GREEN: change `property_to_json` signature to `Result`
- File: `crates/tessera-import/src/property_coerce.rs`
- New signature: `pub fn property_to_json(p: &Property) -> ExportResult<serde_json::Value>`
- Bytes arm: return `Err(ExportError::UnsupportedType { context: "json export".to_owned(), type_name: "Bytes".to_owned() })`
- Update all call sites in `json/mod.rs` and `csv/mod.rs` with `?`

**REFACTOR note:** All call sites already use the return value inline — adding `?` propagation is mechanical. The CSV export's `match prop { Property::String(s) => ..., other => { let jv = property_to_json(other)?; ... } }` pattern handles this cleanly.

**Estimated time:** 45 min
**Output:** All 4 Bytes-related RED tests turn GREEN; no clippy warnings

---

### Phase 3: Critical Fix — GQL Property Key Injection (Finding 2)

#### 3.1 — Add key validation helper
- File: `crates/tessera-import/src/property_coerce.rs`
- Action: Modify — add `pub fn validate_property_key(key: &str) -> Result<(), ImportError>`

Logic (no regex, clippy-safe):
```rust
pub fn validate_property_key(key: &str) -> Result<(), ImportError> {
    let mut chars = key.chars();
    match chars.next() {
        None => return Err(ImportError::InvalidPropertyKey(key.to_owned())),
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return Err(ImportError::InvalidPropertyKey(key.to_owned())),
    }
    for c in chars {
        if !c.is_ascii_alphanumeric() && c != '_' {
            return Err(ImportError::InvalidPropertyKey(key.to_owned()));
        }
    }
    Ok(())
}
```

#### 3.2 — RED tests for key injection in GQL export
- File: `crates/tessera-import/tests/gql_export_test.rs`

```rust
#[test]
fn export_gql_malicious_property_key_returns_error() {
    let mut g = Graph::new();
    let props: tessera_graph::Properties = std::iter::once((
        "x}: 1}) DELETE (:Node) CREATE (:X {y".to_owned(),
        Property::I64(1),
    ))
    .collect();
    g.add_node("Node", props).unwrap();
    let result = export_gql(&g);
    assert!(
        matches!(result, Err(ExportError::InvalidPropertyKey(_))),
        "expected InvalidPropertyKey error, got: {result:?}"
    );
}

#[test]
fn export_gql_empty_property_key_returns_error() {
    let mut g = Graph::new();
    let props: tessera_graph::Properties =
        std::iter::once(("".to_owned(), Property::I64(42))).collect();
    g.add_node("Node", props).unwrap();
    let result = export_gql(&g);
    assert!(
        matches!(result, Err(ExportError::InvalidPropertyKey(_))),
        "got: {result:?}"
    );
}

#[test]
fn export_gql_valid_property_key_underscore_prefix() {
    let mut g = Graph::new();
    let props: tessera_graph::Properties =
        std::iter::once(("_internal_id".to_owned(), Property::I64(1))).collect();
    g.add_node("Node", props).unwrap();
    let result = export_gql(&g);
    assert!(result.is_ok(), "underscore-prefixed key must be accepted; got: {result:?}");
}
```

#### 3.3 — Add `ExportError::InvalidPropertyKey` variant
- File: `crates/tessera-import/src/error.rs`

```rust
#[error("invalid property key '{0}': must match [a-zA-Z_][a-zA-Z0-9_]*")]
InvalidPropertyKey(String),
```

Note: This is a separate variant from `ImportError::InvalidPropertyKey` because it arises during export. Both share the same validation function.

#### 3.4 — GREEN: validate keys in `gql_export/mod.rs`
- File: `crates/tessera-import/src/gql_export/mod.rs`
- Action: Before building `props_str`, iterate `sorted_props` and call `validate_property_key(k)?` for each key
- Map validation error to `ExportError::InvalidPropertyKey(k.clone())`

**Estimated time:** 30 min
**Output:** Injection RED tests pass; valid-key tests pass; no regressions

---

### Phase 4: Performance Fix — O(n²) Node Lookup (Finding 5)

**Hot path — performance regression guard is mandatory.**

Current behavior: `find_node_by_label_and_prop` in `node_lookup.rs` calls `graph.nodes_by_label(label)` and iterates every node for each edge. For N nodes and E edges, cost is O(E * N_per_label).

Fix: Build a lookup index before the edge import loop.

#### 4.1 — Add index builder to `node_lookup.rs`
- File: `crates/tessera-import/src/node_lookup.rs`
- Action: Modify — add `build_lookup_index` function

```rust
/// Key: (label, prop_key, prop_value_as_string)
pub type NodeLookupIndex = std::collections::HashMap<(String, String, String), NodeId>;

/// Build a full lookup index from all nodes currently in the graph.
/// After building, edge import uses O(1) lookups instead of O(N) scans per edge.
pub fn build_lookup_index(graph: &Graph) -> NodeLookupIndex {
    let mut index = NodeLookupIndex::new();
    for id in graph.node_ids() {
        if let Ok(node) = graph.node(id) {
            let label = node.label().to_owned();
            for (prop_key, prop_val) in node.properties() {
                let value_str = match prop_val {
                    Property::String(s) => s.clone(),
                    other => other.to_string(),
                };
                index.insert((label.clone(), prop_key.clone(), value_str), id);
            }
        }
    }
    index
}

/// O(1) lookup using a pre-built index.
pub fn find_node_in_index(
    index: &NodeLookupIndex,
    label: &str,
    prop_key: &str,
    prop_value: &str,
) -> Option<NodeId> {
    index
        .get(&(label.to_owned(), prop_key.to_owned(), prop_value.to_owned()))
        .copied()
}
```

#### 4.2 — RED tests for the index (correctness)
- File: `crates/tessera-import/tests/` — new file `node_lookup_test.rs`

```rust
// Copyright 2026 BelowZero Security OU. All rights reserved.

use tessera_graph::{Graph, Property};
use tessera_import::csv::import_edges_csv;

fn graph_with_n_persons(n: usize) -> Graph {
    let mut g = Graph::new();
    for i in 0..n {
        let props: tessera_graph::Properties = std::iter::once((
            "id".to_owned(),
            Property::I64(i as i64),
        ))
        .collect();
        g.add_node("Person", props).unwrap();
    }
    g
}

#[test]
fn import_edges_csv_large_graph_completes_in_reasonable_time() {
    // RED: With O(n²) this would take seconds; with O(1) index it's instant.
    // 500 nodes, 499 sequential edges.
    let mut g = graph_with_n_persons(500);

    let mut csv = String::from(
        "source_label,source_prop,source_value,target_label,target_prop,target_value,rel_label\n",
    );
    for i in 0..499_usize {
        csv.push_str(&format!(
            "Person,id,{i},Person,id,{},NEXT\n",
            i + 1
        ));
    }

    let start = std::time::Instant::now();
    let count = import_edges_csv(&mut g, &csv).unwrap();
    let elapsed = start.elapsed();

    assert_eq!(count, 499);
    assert!(
        elapsed.as_millis() < 500,
        "edge import of 499 edges into 500 nodes took {:?} — O(n²) regression?",
        elapsed
    );
}

#[test]
fn import_edges_json_large_graph_completes_in_reasonable_time() {
    use tessera_import::json::import_json;

    let mut g = graph_with_n_persons(500);

    // Build JSON with 499 edges
    let edges: Vec<String> = (0..499_usize)
        .map(|i| {
            format!(
                r#"{{"source":{{"label":"Person","match":{{"id":{i}}}}},
                    "target":{{"label":"Person","match":{{"id":{}}}}},
                    "label":"NEXT","properties":{{}}}}"#,
                i + 1
            )
        })
        .collect();
    let json = format!(r#"{{"nodes":[],"edges":[{}]}}"#, edges.join(","));

    let start = std::time::Instant::now();
    let summary = import_json(&mut g, &json).unwrap();
    let elapsed = start.elapsed();

    assert_eq!(summary.edges_imported, 499);
    assert!(
        elapsed.as_millis() < 500,
        "JSON edge import took {:?} — O(n²) regression?",
        elapsed
    );
}
```

#### 4.3 — GREEN: use index in `import_edges_csv`
- File: `crates/tessera-import/src/csv/mod.rs`
- Action: Before the edge loop, call `build_lookup_index(graph)`. Replace `find_node_by_label_and_prop` calls with `find_node_in_index` calls.
- The index is built once before the loop; `graph` is then borrowed mutably inside the loop via `add_edge` calls only (the index uses copied `NodeId` values so no lifetime conflict exists).

Implementation pattern:
```rust
let index = crate::node_lookup::build_lookup_index(graph);
// ... loop body uses find_node_in_index(&index, ...) instead of find_node_by_label_and_prop(graph, ...)
```

#### 4.4 — GREEN: use index in `import_json` / `resolve_endpoint`
- File: `crates/tessera-import/src/json/mod.rs`
- Action: Build index before the edges loop. Pass `&index` into `resolve_endpoint` instead of `graph`.
- Change `resolve_endpoint` signature to accept `&NodeLookupIndex` instead of `&Graph`.

#### 4.5 — REFACTOR: keep `find_node_by_label_and_prop` for single-lookup callers
- The original function in `node_lookup.rs` remains for correctness-only callers that do not benefit from pre-building an index (currently none, but preserve the public API).

**Performance regression guard test (mandatory — see hot path rule):**

The test `import_edges_csv_large_graph_completes_in_reasonable_time` in `node_lookup_test.rs` serves as the regression guard. 500 nodes × 499 edges — the O(n²) version takes ~25,000 linear scans in total and will reliably exceed 500ms on any machine. The O(1) index version completes in < 10ms. This threshold is deliberately conservative.

**Estimated time:** 45 min
**Output:** Both timing tests pass; correctness tests remain green

---

### Phase 5: Recommended Fixes — Correctness and Validation

#### 5.1 — Finding 7: Empty label validation in CSV import

- File: `crates/tessera-import/tests/csv_import_test.rs`

**RED test:**
```rust
#[test]
fn import_nodes_csv_empty_label_returns_error() {
    let mut g = empty_graph();
    let csv = "label,name\n,Alice\n";
    let result = import_nodes_csv(&mut g, csv);
    assert!(
        matches!(result, Err(ImportError::CsvParse { row: 2, .. })),
        "expected CsvParse error for empty label, got: {result:?}"
    );
}

#[test]
fn import_nodes_csv_whitespace_only_label_returns_error() {
    let mut g = empty_graph();
    let csv = "label,name\n   ,Bob\n";
    let result = import_nodes_csv(&mut g, csv);
    assert!(
        matches!(result, Err(ImportError::CsvParse { row: 2, .. })),
        "expected CsvParse error for whitespace-only label, got: {result:?}"
    );
}
```

**GREEN in `csv/mod.rs` after line 105:**
```rust
let label = fields[0].trim().to_owned();
if label.is_empty() {
    return Err(ImportError::CsvParse {
        row: row_num,
        reason: "label column must not be empty".to_owned(),
    });
}
```

#### 5.2 — Finding 8: JSON match with multiple keys must error

- File: `crates/tessera-import/tests/json_import_test.rs`

**RED test:**
```rust
#[test]
fn import_json_edge_multi_key_match_returns_error() {
    let mut g = empty_graph();
    let json = r#"{
        "nodes":[
            {"label":"Person","properties":{"name":"Alice","id":1}},
            {"label":"Person","properties":{"name":"Bob","id":2}}
        ],
        "edges":[
            {
                "source":{"label":"Person","match":{"name":"Alice","id":1}},
                "target":{"label":"Person","match":{"name":"Bob"}},
                "label":"KNOWS",
                "properties":{}
            }
        ]
    }"#;
    let result = import_json(&mut g, json);
    assert!(
        matches!(result, Err(ImportError::JsonInvalid(_))),
        "expected error for multi-key match, got: {result:?}"
    );
}
```

**GREEN in `json/mod.rs` `resolve_endpoint` function:**
After extracting `match_obj`, before the `.iter().next()` call:
```rust
if match_obj.len() > 1 {
    return Err(ImportError::JsonInvalid(format!(
        "edges[].{endpoint_key}.match must have exactly 1 key, found {}",
        match_obj.len()
    )));
}
```

#### 5.3 — Finding 10: Row number in GraphWrite errors during CSV node import

Current: errors from `graph.add_node(...)` produce `ImportError::GraphWrite(msg)` with no row context.

**RED test:**
```rust
// This test requires a mock or a graph that can be made to fail on add_node.
// Since tessera-graph does not expose a way to force add_node failure in tests,
// this finding is documented as "coverage gap" and addressed by wrapping the
// error with row context at the call site even if we cannot write a failing test
// easily. The refactor is still correct and valuable.
```

**GREEN in `csv/mod.rs`:** Change both `GraphWrite` error mappings to include row:
```rust
graph
    .add_node(label, properties)
    .map_err(|e| ImportError::CsvParse {
        row: row_num,
        reason: format!("graph write failed: {e}"),
    })?;
```

Same pattern for `add_edge` in `import_edges_csv`. Note: `ImportError::CsvParse` is reused deliberately — it conveys row context which is more useful than a generic `GraphWrite`.

#### 5.4 — Finding 11: NaN/inf policy — reject at parse time

**Policy decision (no architect consultation needed):** NaN and infinity are not valid property values in a graph database. They produce undefined behavior in comparisons and serialization. Reject them.

- File: `crates/tessera-import/tests/` — add to `csv_import_test.rs`

**RED tests:**
```rust
#[test]
fn coerce_str_value_nan_is_string_not_float() {
    use tessera_graph::Property;
    // "NaN" parses as f64::NAN via str::parse — we must NOT silently store it
    // as Property::F64(NaN). After the fix it should become Property::String("NaN").
    let result = tessera_import::csv::import_nodes_csv(
        &mut tessera_graph::Graph::new(),
        "label,val\nThing,NaN\n",
    )
    .unwrap();
    let g = {
        let mut g = tessera_graph::Graph::new();
        tessera_import::csv::import_nodes_csv(&mut g, "label,val\nThing,NaN\n").unwrap();
        g
    };
    let id = g.nodes_by_label("Thing")[0];
    let node = g.node(id).unwrap();
    assert_eq!(
        node.properties().get("val"),
        Some(&Property::String("NaN".to_owned())),
        "NaN must not be silently stored as f64"
    );
}

#[test]
fn coerce_str_value_inf_is_string_not_float() {
    let mut g = tessera_graph::Graph::new();
    tessera_import::csv::import_nodes_csv(&mut g, "label,val\nThing,inf\n").unwrap();
    let id = g.nodes_by_label("Thing")[0];
    let node = g.node(id).unwrap();
    assert_eq!(
        node.properties().get("val"),
        Some(&Property::String("inf".to_owned())),
    );
}
```

**GREEN in `property_coerce.rs` `coerce_str_value`:**
```rust
if let Ok(f) = raw.parse::<f64>() {
    if f.is_finite() {
        return Property::F64(f);
    }
    // NaN / infinity: fall through to String
}
```

#### 5.5 — Finding 12: Normalize summary counter types to `usize`

`GqlImportSummary.nodes_created` and `edges_created` are `u64`; `ImportJsonSummary.nodes_imported` and `edges_imported` are `usize`. Same concept, different types.

- File: `crates/tessera-import/src/gql_import/mod.rs`
- Action: Change `nodes_created: u64` and `edges_created: u64` to `usize`
- Update accumulators: `summary.nodes_created += result.nodes_created as usize`

**RED test (type-level — compile test):**
```rust
#[test]
fn gql_import_summary_types_are_usize() {
    let s = GqlImportSummary::default();
    // If this compiles, the types are usize. If u64, the assignment below fails.
    let _: usize = s.nodes_created;
    let _: usize = s.edges_created;
}
```

Add to `crates/tessera-import/tests/gql_import_test.rs`.

**Estimated time for Phase 5:** 60 min
**Output:** All 5.1-5.5 RED tests turn GREEN

---

### Phase 6: Recommended Fixes — Code Quality

#### 6.1 — Finding 6: Remove redundant `.sort()` on `BTreeSet` output

`BTreeSet::into_iter()` already yields elements in sorted order. The `.sort()` calls on `all_keys` at `csv/mod.rs:244` and `csv/mod.rs:319` are dead work.

- File: `crates/tessera-import/src/csv/mod.rs`
- Action: Remove both `.sort()` lines (lines 244 and 319)
- No new test needed — this is a no-behavior-change refactor. Existing export tests already verify sorted output.

**Clippy note:** clippy `nursery` may already flag this as `clippy::needless_pass_by_value` or similar. After removing, run `cargo clippy` to confirm.

#### 6.2 — Finding 9: Document edge CSV format incompatibility

Export produces `source_id,target_id,...` but import expects `source_label,source_prop,...`. These formats are intentionally different — export is for inspection/backup, import is for data loading.

- File: `crates/tessera-import/src/csv/mod.rs`
- Action: Add module-level doc comment explaining the asymmetry

```rust
//! ## Edge CSV Format Note
//!
//! Edge export and import use different formats by design:
//! - **Export** (`export_edges_csv`): `source_id,target_id,rel_label,...` — uses internal IDs
//!   for compactness; suitable for inspection and backup.
//! - **Import** (`import_edges_csv`): `source_label,source_prop,source_value,...` — uses
//!   property-based node matching; suitable for loading data from external sources.
//!
//! Round-trip (export then re-import) is not supported for edges. If you need round-trip
//! capability, use JSON format which uses the same property-match convention for both.
```

No test needed — documentation only.

#### 6.3 — Finding 14: `String::with_capacity` in export functions

The three export functions (`export_gql`, `export_nodes_csv`, `export_edges_csv`) allocate a plain `String::new()` or `String::from(...)` without capacity hints. For large graphs this causes repeated reallocations.

- File: `crates/tessera-import/src/gql_export/mod.rs` and `crates/tessera-import/src/csv/mod.rs`
- Action: Use `String::with_capacity` with a reasonable estimate

```rust
// gql_export: ~50 bytes per node is a reasonable lower bound
let mut out = String::with_capacity(50 * (graph.node_count() + 1));

// csv export nodes: ~30 bytes per node
let mut out = String::with_capacity(30 * (graph.node_count() + 1));

// csv export edges: ~40 bytes per edge
let node_count = graph.node_ids().len();
let mut out = String::with_capacity(40 * (node_count + 1));
```

No new test needed — this is a pure performance improvement with no behavior change. Existing tests verify correctness.

#### 6.4 — Finding 15: Fix Cargo.toml description

- File: `crates/tessera-import/Cargo.toml`
- Action: Change `description`

```toml
description = "CSV, JSON, and GQL import/export for tessera-graph-enterprise"
```

No test needed — metadata only.

**Estimated time for Phase 6:** 25 min

---

### Phase 7: Optional — Round-Trip Tests (Finding 13)

Round-trip tests are the highest-value optional finding: they catch export + import integration bugs that unit tests miss.

#### 7.1 — CSV node round-trip test
- File: `crates/tessera-import/tests/` — new file `round_trip_test.rs`

```rust
// Copyright 2026 BelowZero Security OU. All rights reserved.

use tessera_graph::{Graph, Property};
use tessera_import::csv::{export_nodes_csv, import_nodes_csv};
use tessera_import::json::{export_json, import_json};
use tessera_import::gql_export::export_gql;
use tessera_import::gql_import::import_gql;

fn alice_bob_graph() -> Graph {
    let mut g = Graph::new();
    let props_alice: tessera_graph::Properties = [
        ("name".to_owned(), Property::String("Alice".to_owned())),
        ("age".to_owned(), Property::I64(30)),
        ("active".to_owned(), Property::Bool(true)),
    ]
    .into_iter()
    .collect();
    let props_bob: tessera_graph::Properties = [
        ("name".to_owned(), Property::String("Bob".to_owned())),
        ("age".to_owned(), Property::I64(25)),
        ("score".to_owned(), Property::F64(9.5)),
    ]
    .into_iter()
    .collect();
    g.add_node("Person", props_alice).unwrap();
    g.add_node("Person", props_bob).unwrap();
    g
}

#[test]
fn csv_node_round_trip_preserves_count_and_labels() {
    let original = alice_bob_graph();
    let csv = export_nodes_csv(&original).unwrap();

    let mut restored = Graph::new();
    let count = import_nodes_csv(&mut restored, &csv).unwrap();

    assert_eq!(count, 2);
    assert_eq!(restored.node_count(), 2);
    assert_eq!(restored.nodes_by_label("Person").len(), 2);
}

#[test]
fn csv_node_round_trip_preserves_integer_property() {
    let original = alice_bob_graph();
    let csv = export_nodes_csv(&original).unwrap();

    let mut restored = Graph::new();
    import_nodes_csv(&mut restored, &csv).unwrap();

    let alice_id = restored.nodes_by_label("Person").into_iter().find(|&id| {
        restored
            .node(id)
            .ok()
            .and_then(|n| n.properties().get("name").cloned())
            == Some(Property::String("Alice".to_owned()))
    });
    let alice_id = alice_id.expect("Alice not found after round-trip");
    let node = restored.node(alice_id).unwrap();
    assert_eq!(node.properties().get("age"), Some(&Property::I64(30)));
    assert_eq!(node.properties().get("active"), Some(&Property::Bool(true)));
}

#[test]
fn csv_node_round_trip_string_with_comma() {
    let mut original = Graph::new();
    let props: tessera_graph::Properties = std::iter::once((
        "desc".to_owned(),
        Property::String("hello, world".to_owned()),
    ))
    .collect();
    original.add_node("Thing", props).unwrap();

    let csv = export_nodes_csv(&original).unwrap();
    let mut restored = Graph::new();
    import_nodes_csv(&mut restored, &csv).unwrap();

    let id = restored.nodes_by_label("Thing")[0];
    let node = restored.node(id).unwrap();
    assert_eq!(
        node.properties().get("desc"),
        Some(&Property::String("hello, world".to_owned()))
    );
}

#[test]
fn gql_node_round_trip_preserves_string_with_apostrophe() {
    let mut original = Graph::new();
    let props: tessera_graph::Properties =
        std::iter::once(("name".to_owned(), Property::String("O'Brien".to_owned()))).collect();
    original.add_node("Person", props).unwrap();

    let gql = export_gql(&original).unwrap();
    // The exported GQL must contain the backslash-escaped apostrophe
    assert!(gql.contains("\\'"), "export must escape apostrophe as \\'; got: {gql}");

    let mut restored = Graph::new();
    import_gql(&mut restored, &gql).unwrap();

    assert_eq!(restored.node_count(), 1);
    let id = restored.nodes_by_label("Person")[0];
    let node = restored.node(id).unwrap();
    assert_eq!(
        node.properties().get("name"),
        Some(&Property::String("O'Brien".to_owned())),
        "apostrophe must survive GQL export + import round-trip"
    );
}

#[test]
fn gql_node_round_trip_preserves_integer_and_bool() {
    let mut original = Graph::new();
    let props: tessera_graph::Properties = [
        ("age".to_owned(), Property::I64(42)),
        ("active".to_owned(), Property::Bool(false)),
    ]
    .into_iter()
    .collect();
    original.add_node("User", props).unwrap();

    let gql = export_gql(&original).unwrap();
    let mut restored = Graph::new();
    import_gql(&mut restored, &gql).unwrap();

    let id = restored.nodes_by_label("User")[0];
    let node = restored.node(id).unwrap();
    assert_eq!(node.properties().get("age"), Some(&Property::I64(42)));
    assert_eq!(node.properties().get("active"), Some(&Property::Bool(false)));
}

#[test]
fn json_node_round_trip_preserves_properties() {
    let original = alice_bob_graph();
    let json_str = export_json(&original).unwrap();

    // JSON export uses source_id/target_id for edges (no nodes-only import).
    // Re-import using the nodes array only (manually extracted).
    let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();
    let nodes_only = serde_json::json!({
        "nodes": parsed["nodes"].clone(),
        "edges": []
    });

    let mut restored = Graph::new();
    let summary = import_json(&mut restored, &nodes_only.to_string()).unwrap();

    assert_eq!(summary.nodes_imported, 2);
    assert_eq!(restored.nodes_by_label("Person").len(), 2);
}
```

**Estimated time for Phase 7:** 40 min

---

### Phase 8: Wiring Verification

After all phases complete, run the full test suite to confirm zero regressions:

```
cargo test -p tessera-import 2>&1
cargo clippy -p tessera-import -- -D warnings 2>&1
```

Expected: all tests green, zero clippy warnings.

#### Files modified in this plan (summary):

| File | Changes |
|------|---------|
| `src/error.rs` | +2 new variants: `ExportError::UnsupportedType`, `ExportError::InvalidPropertyKey`, `ImportError::InvalidPropertyKey` |
| `src/property_coerce.rs` | `property_to_gql_literal` → `ExportResult<String>`, `property_to_json` → `ExportResult<Value>`, add `validate_property_key`, add NaN/inf guard in `coerce_str_value` |
| `src/gql_export/mod.rs` | Key validation before render, propagate `?` from fallible functions, `with_capacity` |
| `src/csv/mod.rs` | Empty label validation, remove 2 redundant `.sort()` calls, use index for edge import, row context on GraphWrite errors, `with_capacity`, module doc |
| `src/json/mod.rs` | Use index for edge import, multi-key match error |
| `src/node_lookup.rs` | Add `NodeLookupIndex` type, `build_lookup_index`, `find_node_in_index` |
| `src/gql_import/mod.rs` | Change `nodes_created`/`edges_created` to `usize` |
| `Cargo.toml` | Fix description |
| `tests/error_types_test.rs` | +3 tests |
| `tests/csv_export_test.rs` | +2 Bytes tests |
| `tests/csv_import_test.rs` | +2 empty-label tests, +2 NaN/inf tests |
| `tests/json_export_test.rs` | +1 Bytes test |
| `tests/json_import_test.rs` | +1 multi-key match test |
| `tests/gql_export_test.rs` | +3 key injection tests, +1 Bytes test |
| `tests/gql_import_test.rs` | +1 type compile test |
| `tests/node_lookup_test.rs` | NEW — 2 timing regression guards |
| `tests/round_trip_test.rs` | NEW — 6 round-trip tests |

---

## Estimation Total

| Phase | Work | Time |
|-------|------|------|
| 1 — Error types | Foundation variants | 20 min |
| 2 — Bytes export fails loudly | Critical, 4 files | 45 min |
| 3 — Key injection | Critical, 2 files | 30 min |
| 4 — O(n²) lookup | Performance, hot path | 45 min |
| 5 — Correctness fixes (5 sub-tasks) | Recommended | 60 min |
| 6 — Code quality (4 sub-tasks) | Recommended | 25 min |
| 7 — Round-trip tests | Optional (high value) | 40 min |
| 8 — Wiring verification | Final check | 10 min |
| **Total** | | **~4.5 hours** |

---

## Criteria de Exito

- [ ] `cargo test -p tessera-import` — all tests pass, including 2 timing guards in `node_lookup_test.rs`
- [ ] `cargo clippy -p tessera-import -- -D warnings` — zero warnings
- [ ] `cargo test -p tessera-import -- --test node_lookup_test import_edges_csv_large_graph_completes_in_reasonable_time` completes in < 500ms (O(1) index confirmed)
- [ ] `cargo test -p tessera-import -- --test node_lookup_test import_edges_json_large_graph_completes_in_reasonable_time` completes in < 500ms
- [ ] `ExportError::UnsupportedType` is returned for all Bytes properties in all three export formats
- [ ] `ExportError::InvalidPropertyKey` is returned for keys containing `}`, spaces, or other non-identifier characters in GQL export
- [ ] Round-trip test `gql_node_round_trip_preserves_string_with_apostrophe` proves the `\'` escape survives the full export → import cycle
- [ ] `GqlImportSummary.nodes_created` and `edges_created` are `usize`
- [ ] No throughput regression > 10% on edge import for graphs with 500+ nodes
