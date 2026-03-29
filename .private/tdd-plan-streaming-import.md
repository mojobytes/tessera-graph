# TDD Plan: Streaming Import for JSON, GQL, and CSV formats

## Context

`handle_import` in `crates/tessera-cli/src/main.rs` (line 168) loads the entire file
into a `String`, then calls one of three batch functions (`json_to_gql_statements`,
`split_gql_statements`, `csv_nodes_to_gql`) which each return a `Vec<String>` of all
statements before a single Bolt query is sent. For a 195 MB JSON file with 200k nodes
and 200k edges the peak heap allocation reaches ~800 MB and several minutes pass before
the first statement executes.

The fix is to add three streaming functions that accept a reader, call a callback for
each statement as it is generated, and leave the existing batch functions completely
untouched for dry-run and existing tests.

**Stack detected**: Rust, tokio async runtime, serde_json, csv crate (already in Cargo.toml)
**Conventions**: `#[cfg(test)] mod tests` inline, `.unwrap()` / `.expect()` in tests
annotated `// OK: test`, `std::io::Cursor` as test reader, `CliError::ImportExport`
for all domain errors, `must_use`, doc comments on every public item
**Affects hot path**: YES — the execution loop in `handle_import` is the core import
pipeline. Every statement passes through it.

## Decisions Resolved (no architect consultation needed)

All architectural choices are specified in the problem statement. No blockers.

## Dependency Audit

`Cargo.toml` already has:
- `csv = "1"` — lazy record iteration available
- `serde_json = { workspace = true }` — `Deserializer::from_reader` + streaming API available
- `tokio = { ..., features = ["rt-multi-thread", "macros", "net", "io-util", "time"] }` —
  `spawn_blocking` and `mpsc::channel` available once `sync` feature is added

**One dependency change required**: `tokio` needs the `sync` feature flag for
`tokio::sync::mpsc`. It is not currently listed in `Cargo.toml` features.

---

## Plan de Ejecución

### Phase 1 — Dependency and Error Plumbing (15 min)

1. [ ] Add `sync` to tokio features in `Cargo.toml`
   - File: `crates/tessera-cli/Cargo.toml`
   - Action: Modify `tokio` dependency line, append `"sync"` to the features array
   - Output: `tokio = { workspace = true, features = ["rt-multi-thread", "macros", "net", "io-util", "time", "sync"] }`
   - Verify: `cargo check -p tessera-cli` passes

---

### Phase 2 — GQL Streaming (simplest format, establishes the pattern) (25 min)

2. [ ] Implement `stream_gql_import` in `import.rs`
   - File: `crates/tessera-cli/src/import.rs`
   - Action: Add new public function after `split_gql_statements` (around line 82)
   - Signature: `pub fn stream_gql_import<R: BufRead>(reader: R, mut on_stmt: impl FnMut(String) -> Result<(), CliError>) -> Result<usize, CliError>`
   - Logic:
     - Allocate `current: String` and `count: usize`
     - Call `reader.lines()` (or `read_line` in a loop) — one allocation per line
     - Apply the exact same blank/comment/semicolon logic as `split_gql_statements`
     - When a complete statement is ready call `on_stmt(stmt)?`; increment `count`
     - After loop, flush any trailing non-semicolon statement via `on_stmt`
     - Return `Ok(count)`
   - Note: reuse `is_comment_line` (already private, accessible in same module)
   - Output: function compiles, no warnings

3. [ ] Write inline unit tests for `stream_gql_import`
   - File: `crates/tessera-cli/src/import.rs`, `#[cfg(test)] mod tests`
   - Tests to add:
     - `stream_gql_single_statement` — `Cursor::new("CREATE (:A);")` yields 1 stmt, count=1
     - `stream_gql_multiple_statements` — 3 semicolon-terminated lines yield 3 stmts
     - `stream_gql_skips_comments_and_blanks` — comments and blanks yield 0 stmts
     - `stream_gql_multiline_statement` — multi-line statement is joined correctly
     - `stream_gql_no_trailing_semicolon` — trailing statement without `;` is flushed
     - `stream_gql_callback_error_propagates` — callback returning `Err` causes early return
     - `stream_gql_parity_with_batch` — for a corpus of 10 statements, streaming output
       equals `split_gql_statements` output (parity test)
   - Output: `cargo test -p tessera-cli stream_gql` all pass

---

### Phase 3 — CSV Streaming (25 min)

4. [ ] Implement `stream_csv_import` in `import.rs`
   - File: `crates/tessera-cli/src/import.rs`
   - Action: Add new public function after `csv_nodes_to_gql`
   - Signature: `pub fn stream_csv_import<R: Read>(reader: R, mut on_stmt: impl FnMut(String) -> Result<(), CliError>) -> Result<usize, CliError>`
   - Logic:
     - Build `csv::ReaderBuilder::new().has_headers(true).from_reader(reader)`
     - Capture headers (same as `csv_nodes_to_gql`)
     - Iterate `reader.records()` lazily — each record is deserialized one at a time
     - For each record apply identical label/props logic as `csv_nodes_to_gql`
     - Call `on_stmt(stmt)?`; increment `count`
     - Return `Err` if no rows processed (same semantics as batch version)
     - Return `Ok(count)`
   - Note: the `csv` crate's `Reader` already iterates lazily; no buffering needed

5. [ ] Write inline unit tests for `stream_csv_import`
   - File: `crates/tessera-cli/src/import.rs`, `#[cfg(test)] mod tests`
   - Tests to add:
     - `stream_csv_basic` — 2-row CSV yields 2 stmts with correct label/props
     - `stream_csv_numeric_values` — numbers not quoted, strings quoted
     - `stream_csv_empty_props_omitted` — empty columns omitted from props
     - `stream_csv_empty_label_error` — empty label column returns `Err`
     - `stream_csv_no_data_rows_error` — header-only CSV returns `Err`
     - `stream_csv_callback_error_propagates` — early termination on callback `Err`
     - `stream_csv_parity_with_batch` — output equals `csv_nodes_to_gql` for same input
   - Output: `cargo test -p tessera-cli stream_csv` all pass

---

### Phase 4 — JSON Streaming (most complex, serde Visitor pattern) (45 min)

This is the highest-risk task. Implement it in two sub-steps.

6. [ ] Implement helper `node_value_to_gql_stmt` and `edge_value_to_gql_stmt`
   - File: `crates/tessera-cli/src/import.rs`
   - Action: Extract the per-element statement generation from `json_to_gql_statements`
     into two private functions that take `&serde_json::Value` and return `Result<String, CliError>`
   - Signatures:
     - `fn node_value_to_gql_stmt(node: &serde_json::Value) -> Result<String, CliError>`
     - `fn edge_value_to_gql_stmt(edge: &serde_json::Value) -> Result<String, CliError>`
   - Refactor `json_to_gql_statements` to delegate to these helpers (behavior identical, tests stay green)
   - Output: all existing JSON tests still pass; no new warnings

7. [ ] Implement `stream_json_import` using `serde_json::Deserializer::from_reader`
   - File: `crates/tessera-cli/src/import.rs`
   - Action: Add new public function after `json_to_gql_statements`
   - Signature: `pub fn stream_json_import<R: Read>(reader: R, mut on_stmt: impl FnMut(String) -> Result<(), CliError>) -> Result<usize, CliError>`
   - Implementation strategy (no custom Visitor needed — use `serde_json::Value` lazily):

     ```
     Use serde_json::Deserializer::from_reader(reader) to get a streaming
     deserializer. Deserialize the root object partially:

     1. Manually drive the deserializer with MapAccess to read key by key.
     2. When key == "nodes": deserialize the value as a streaming array using
        serde_json::Deserializer's SeqAccess-based approach. The trick:
        deserialize the array value as serde_json::Value one element at a time
        by deserializing the enclosing array as an iterator using
        `StreamDeserializer` or by reading array elements via a custom
        `serde::de::Visitor` with `visit_seq`.
     3. For each element call `node_value_to_gql_stmt(&elem)?` then `on_stmt(stmt)?`.
     4. Drop the element before reading the next.
     5. When key == "edges": same pattern for edges.
     6. Return Ok(total_count).
     ```

   - Concrete serde approach — use `serde_json::Deserializer` with a hand-written
     top-level `Visitor` that implements `visit_map`. Inside `visit_map`:
     - Loop `map.next_key::<String>()?`
     - If key is "nodes" or "edges", call
       `map.next_value_seed(ArrayStreamSeed { on_stmt, converter, count })`
       where `ArrayStreamSeed` is a private struct implementing `DeserializeSeed`
       that visits each array element as `serde_json::Value`, calls the
       converter function, then calls `on_stmt`.
     - For unknown keys, consume with `map.next_value::<serde_json::Value>()?`.
     - After map: return `Err` if neither nodes nor edges produced any statements.

   - The `ArrayStreamSeed` struct:
     ```rust
     struct ArrayStreamSeed<'a, F> {
         on_stmt: &'a mut F,
         converter: fn(&serde_json::Value) -> Result<String, CliError>,
         count: &'a mut usize,
     }
     impl<'de, F> DeserializeSeed<'de> for ArrayStreamSeed<'_, F>
     where F: FnMut(String) -> Result<(), CliError>
     {
         type Value = ();
         fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<(), D::Error> {
             d.deserialize_seq(self)
         }
     }
     impl<'de, F> Visitor<'de> for ArrayStreamSeed<'_, F>
     where F: FnMut(String) -> Result<(), CliError>
     {
         type Value = ();
         fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<(), A::Error> {
             while let Some(elem) = seq.next_element::<serde_json::Value>()? {
                 let stmt = (self.converter)(&elem)
                     .map_err(serde::de::Error::custom)?;
                 (self.on_stmt)(stmt)
                     .map_err(serde::de::Error::custom)?;
                 *self.count += 1;
             }
             Ok(())
         }
     }
     ```
   - Output: function compiles with no warnings

8. [ ] Write inline unit tests for `stream_json_import`
   - File: `crates/tessera-cli/src/import.rs`, `#[cfg(test)] mod tests`
   - Tests to add:
     - `stream_json_nodes_only` — JSON with 2 nodes, 0 edges yields 2 stmts
     - `stream_json_edges_only` — JSON with 0 nodes and edges array has error (empty check)
     - `stream_json_nodes_and_edges` — 2 nodes + 1 edge yields 3 stmts, order: nodes first then edge
     - `stream_json_single_quote_escaped` — property `O'Brien` → `O''Brien` in output
     - `stream_json_boolean_null` — bool/null props render correctly
     - `stream_json_invalid_json_error` — malformed JSON returns `Err`
     - `stream_json_empty_arrays_error` — `{"nodes":[],"edges":[]}` returns `Err`
     - `stream_json_callback_error_propagates` — callback `Err` causes early return
     - `stream_json_parity_with_batch` — for a 5-node/2-edge corpus, streaming output
       equals `json_to_gql_statements` output exactly (parity test)
   - Output: `cargo test -p tessera-cli stream_json` all pass

---

### Phase 5 — Rewrite `handle_import` in `main.rs` (30 min)

9. [ ] Replace the bulk-load execution path in `handle_import`
   - File: `crates/tessera-cli/src/main.rs`
   - Action: Restructure `handle_import` — keep the `dry_run` branch using existing
     batch functions, replace the live import branch with streaming + channel bridge
   - New live import structure:

     ```
     // 1. Open reader — either stdin or BufReader<File>
     let reader: Box<dyn Read + Send> = if args.file == "-" {
         Box::new(std::io::stdin().lock())
     } else {
         Box::new(BufReader::new(File::open(&args.file)?))
     };

     // 2. Create bounded channel (backpressure: 64 statements)
     let (tx, mut rx) = tokio::sync::mpsc::channel::<Result<String, CliError>>(64);

     // 3. Spawn blocking thread to drive streaming parser
     let fmt = fmt.to_owned(); // owned copy for move into blocking task
     let producer = tokio::task::spawn_blocking(move || {
         let send_stmt = |stmt: String| {
             tx.blocking_send(Ok(stmt))
                 .map_err(|_| CliError::ImportExport("channel closed".into()))
         };
         let result = match fmt.as_str() {
             "json"      => import::stream_json_import(reader, send_stmt),
             "gql"       => import::stream_gql_import(BufReader::new(reader), send_stmt),
             "csv-nodes" => import::stream_csv_import(reader, send_stmt),
             other => Err(CliError::ImportExport(format!(
                 "unsupported import format: {other}"
             ))),
         };
         // Signal end — send Err only if the producer itself failed
         if let Err(e) = result {
             let _ = tx.blocking_send(Err(e));
         }
         // tx drops here, closing channel
     });

     // 4. Consume channel in async context — drive Bolt queries
     let mut count = 0usize;
     while let Some(item) = rx.recv().await {
         let stmt = item?; // propagate producer error
         query::execute_query(session, &stmt, "gql").await?;
         count += 1;
         if count % PROGRESS_INTERVAL == 0 {
             eprintln!("Imported {count} statements...");
         }
     }

     // 5. Await producer to surface any panic
     producer.await
         .map_err(|e| CliError::ImportExport(format!("import thread panicked: {e}")))?;

     eprintln!("Imported {count} statements total.");
     ```

   - Remove the large-file warning (no longer loads into memory — the warning is obsolete)
   - Remove the `content` variable and all `read_to_string` calls from the live path
   - Keep `should_report_progress` — it is still used in tests; update the call site to
     the simpler `count % PROGRESS_INTERVAL == 0` since total is not known upfront
   - Dry-run path: unchanged — still reads content and calls batch functions for summary
   - Output: `cargo check -p tessera-cli` with no errors or warnings

10. [ ] Add required `use` imports at top of `main.rs`
    - File: `crates/tessera-cli/src/main.rs`
    - Add: `use std::fs::File;`, `use std::io::{BufReader, Read};`
    - Verify these don't conflict with existing imports in the file
    - Output: no unused import warnings

---

### Phase 6 — Performance Regression Guard (hot path) (20 min)

11. [ ] Add throughput benchmark guard test for streaming JSON import
    - File: `crates/tessera-cli/src/import.rs`, `#[cfg(test)] mod tests`
    - Test name: `stream_json_throughput_regression_guard`
    - Logic:
      - Build a synthetic in-memory JSON string with 10,000 nodes using a loop
        (avoids touching the filesystem, deterministic)
      - Time the streaming import using `std::time::Instant`
      - Assert `count == 10_000`
      - Assert elapsed < 2 seconds (generous upper bound to avoid CI flakes;
        actual perf on dev machine should be < 100 ms for 10k nodes in memory)
    - This guards against regressions that accidentally re-introduce buffering
    - Output: test passes in under 2 seconds on any reasonable machine

12. [ ] Add throughput benchmark guard test for streaming CSV import
    - File: `crates/tessera-cli/src/import.rs`, `#[cfg(test)] mod tests`
    - Test name: `stream_csv_throughput_regression_guard`
    - Logic: same pattern — 10,000 rows generated in memory, assert count, assert elapsed < 2 s
    - Output: test passes

13. [ ] Add throughput benchmark guard test for streaming GQL import
    - File: `crates/tessera-cli/src/import.rs`, `#[cfg(test)] mod tests`
    - Test name: `stream_gql_throughput_regression_guard`
    - Logic: same pattern — 10,000 `CREATE (:Person {name: 'N'});` statements, assert count, assert elapsed < 2 s
    - Output: test passes

---

### Phase 7 — Final Compile and Full Test Run (10 min)

14. [ ] Compile entire workspace and run all tests
    - Command: `cargo test -p tessera-cli`
    - Verify: zero errors, zero warnings (warnings = errors per workspace lints)
    - Verify: all pre-existing tests still pass (batch functions untouched)
    - Verify: all new streaming tests pass

---

## Estimation

| Phase | Task(s) | Time |
|-------|---------|------|
| Dependency | 1 | 15 min |
| GQL streaming | 2–3 | 50 min |
| CSV streaming | 4–5 | 50 min |
| JSON streaming | 6–8 | 90 min |
| handle_import rewrite | 9–10 | 40 min |
| Performance guards | 11–13 | 30 min |
| Final verify | 14 | 10 min |
| **Total** | | **~5 h** |

---

## Criteria de Éxito

- [x] `cargo test -p tessera-cli` passes with zero warnings
- [x] All 7 parity tests confirm streaming output is identical to batch output
- [x] `handle_import` no longer calls `read_to_string` on the live import path
- [x] Peak memory for a 200k node JSON is bounded by channel size (64 × ~300 bytes = ~20 KB),
      not by file size
- [x] Throughput guards: 10k-node JSON streamed in < 2 s, CSV in < 2 s, GQL in < 2 s
- [x] Existing dry-run behavior unchanged: batch functions are still called for `--dry-run`
- [x] `is_large_file` warning removed from live path (retained in code for dry-run if applicable,
      or removed entirely if no longer used)

---

## Risk Notes

**JSON Visitor complexity**: The `serde_json::Deserializer` streaming API with
`DeserializeSeed` is well-established but less commonly used. If the `ArrayStreamSeed`
pattern encounters lifetime issues with the `&mut on_stmt` closure, the fallback is to
use a `RefCell<&mut F>` wrapper to give `Visitor` interior mutability during the
`visit_seq` call. Document the workaround in a code comment if applied.

**Channel error handling**: If `execute_query` returns `Err`, `rx.recv()` loop exits and
`rx` is dropped. The producer's next `blocking_send` will then fail with a closed-channel
error. The producer catches this as `CliError::ImportExport("channel closed")`. The async
side must NOT await `producer` after an early return from the `while let` loop — use
`producer.abort()` before returning the query error, or restructure with `select!`.
Plan step 9 must handle this explicitly to avoid a goroutine leak equivalent.

**`should_report_progress` function**: It takes a `total` parameter. After this change
the live path no longer has a total. Either add an overload or leave the function for
dry-run/tests and use a direct modulo check in the streaming path (step 9 already does this).
The function must not be deleted — its tests exist and it is used in `handle_exec`.
Verify before removing any references.
