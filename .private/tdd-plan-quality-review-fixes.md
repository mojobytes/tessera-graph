# TDD Plan — Quality Review Fixes (Post-Resilience Hardening)

## Contexto

Eight findings from the quality review of the resilience hardening commit. The changes span three crates (`tessera-graph-server`, `tessera-graph-audit`, `tessera-graph-tenant`) and address two categories: correctness bugs in health state management and env-var parsing, and quality improvements in logging, lock strategy, async safety, and test coverage.

None of the fixes touch a query/insert hot path that requires throughput regression guards. Finding 6 (`touch_access_order`) is in the cache miss slow-path — not the hot path — so no throughput benchmark is required.

**Stack detectado**: Rust 2024, Tokio async runtime, workspace with 14 crates.
**Convenciones observadas**:
- Unit tests inline: `#[cfg(test)] mod tests` at bottom of the source file.
- Integration tests: `crates/<crate>/tests/*.rs`.
- `.unwrap()` in tests annotated with `// OK: test`.
- `clippy::all = deny`, `clippy::pedantic = warn`, `clippy::nursery = warn` — warnings are errors.
- `unsafe_code = forbid`.
- `tracing::warn!`/`tracing::error!` for structured logging, never `eprintln!`.
- Helper functions extracted for unit-testability (see `update_health_state`, `parse_env_or_warn`).
- `pub(crate)` visibility by default; promote to `pub` only when a cross-crate consumer needs it.

**Afecta hot path**: No. All findings are in startup, background tasks, or slow paths.

---

## Decisiones Previas Necesarias

None. All architectural decisions are clear from the existing code:
- Finding 1 solution: two separate `AtomicBool` flags inside `AtomicHealthFlag` (disk vs flush errors). Chosen over a single bitfield or enum to keep `AtomicHealthFlag` API additive and avoid breaking callers.
- Finding 6 solution: replace `RwLock<VecDeque>` with `Mutex<VecDeque>` (not `AtomicU64` timestamps) because the VecDeque retain+push_back ordering logic must remain atomic as a unit; per-entry timestamps would require a sorted scan on eviction, which is a larger refactor outside scope.

---

## Plan de Ejecución

---

### Phase 1 — Env Var Validation (Findings 2, 3, 4)

**Rationale**: All three findings share the same root cause — env var parsing that silently falls to a default on invalid input. Fixing them in one phase avoids context-switching between the same files.

#### Finding 2 — `TESSERA_MAX_LOADED_TENANTS` no usa `parse_env_or_warn`

**Root cause observed**: `parse_env_or_warn` is `pub(crate)`, so `main.rs` (a binary, different compilation unit than the lib) cannot access it. The current code at `main.rs:89-92` uses `.ok().and_then(|v| v.parse().ok()).unwrap_or(0)` with no warning.

**Fix strategy**: Promote `parse_env_or_warn` from `pub(crate)` to `pub` in `config.rs`. The function has no unsafe surface, no secrets, and is a pure utility. This lets `main.rs` import and call it directly.

**1.1** [ ] Write failing unit test for `parse_env_or_warn` warning on invalid value (15 min)
- File: `crates/tessera-graph-server/src/config.rs` — inline `#[cfg(test)] mod tests`
- Test name: `parse_env_or_warn_invalid_value_uses_default`
- Action: set env var to `"not_a_number"`, call `parse_env_or_warn::<usize>("TEST_VAR_X", 42)`, assert result == 42.
- Note: The warning itself cannot be asserted in a unit test without a tracing subscriber. The test asserts the return value is the default; the warning is verified by code review of the implementation. Add a comment: `// warning emitted to tracing — verified by code review`.
- RED: currently `pub(crate)` so the test compiles but the function is not callable from main. This step verifies the function's return contract.
- Output: one new test in `config.rs::tests`.

**1.2** [ ] Promote `parse_env_or_warn` to `pub` and update `main.rs` (15 min)
- File: `crates/tessera-graph-server/src/config.rs:16` — change `pub(crate)` to `pub`.
- File: `crates/tessera-graph-server/src/main.rs:89-92` — replace the inline parse block with:
  ```rust
  let max_loaded_tenants: usize =
      tessera_graph_server::config::parse_env_or_warn("TESSERA_MAX_LOADED_TENANTS", 0_usize);
  ```
- Output: `main.rs` now warns on invalid `TESSERA_MAX_LOADED_TENANTS` values. Compile passes.

---

#### Finding 3 — `TESSERA_MIN_FREE_DISK_MB` no usa `parse_env_or_warn`

**Root cause observed**: `flush_task.rs:147-151` reads `TESSERA_MIN_FREE_DISK_MB` via `.ok().and_then(|v| v.parse().ok())` with no warning. The env var is read inside the spawned task, making it untestable.

**Fix strategy**: Move the env var parsing into `PersistenceConfig::from_env()` as a new field `min_free_disk_bytes: u64`. Pass the value to `spawn_background_flush` as a parameter. This makes the configuration observable and testable at the `PersistenceConfig` level.

**1.3** [ ] Write failing test: `PersistenceConfig::from_env` populates `min_free_disk_bytes` (20 min)
- File: `crates/tessera-graph-server/src/config.rs` — inline tests
- Test name: `persistence_config_min_free_disk_default`
- Action: with env var unset, construct `PersistenceConfig::from_env()`, assert `min_free_disk_bytes == MIN_FREE_DISK_BYTES`.
- Test name: `persistence_config_min_free_disk_from_env`
- Action: set `TESSERA_MIN_FREE_DISK_MB=200`, construct config, assert `min_free_disk_bytes == 200 * 1024 * 1024`.
- RED: `PersistenceConfig` doesn't have this field yet.
- Output: two failing tests.

**1.4** [ ] Add `min_free_disk_bytes` field to `PersistenceConfig` (20 min)
- File: `crates/tessera-graph-server/src/config.rs`
- Add field `pub min_free_disk_bytes: u64` to `PersistenceConfig`.
- In `from_env()`, parse using `parse_env_or_warn::<u64>("TESSERA_MIN_FREE_DISK_MB", 0)` — if 0, use `MIN_FREE_DISK_BYTES` as default (import the constant via `use crate::flush_task::MIN_FREE_DISK_BYTES` or make it accessible). Actually: define `DEFAULT_MIN_FREE_DISK_MB: u64 = 100` in `config.rs` and compute `parse_env_or_warn("TESSERA_MIN_FREE_DISK_MB", DEFAULT_MIN_FREE_DISK_MB) * 1024 * 1024`. Remove `MIN_FREE_DISK_BYTES` from `flush_task.rs` or keep it as the computed constant for tests (prefer keeping it in `flush_task.rs` for the `disk_space_threshold_constant_is_positive` test — just stop reading the env var inside the task).
- Update `spawn_background_flush` signature: add `min_free_disk_bytes: u64` parameter, remove the env var read at lines 147-150.
- Update the call site in `main.rs` to pass `persistence.min_free_disk_bytes`.
- GREEN: tests pass.
- Output: `PersistenceConfig` owns the value; `flush_task` receives it as a parameter.

---

#### Finding 4 — `TESSERA_AUDIT_SYNC` sin warning

**Root cause observed**: `main.rs:63-65` uses `.map(|v| v != "false" && v != "0")` — any non-empty, non-`"false"`, non-`"0"` string evaluates to `true`, including typos like `"treu"`. No warning is emitted.

**Fix strategy**: Extract a dedicated `parse_bool_env_or_warn(name, default)` function in `config.rs` that matches `"true"/"1"` for true, `"false"/"0"` for false, and warns on anything else. Apply it to `TESSERA_AUDIT_SYNC` in `main.rs`.

**1.5** [ ] Write failing test for `parse_bool_env_or_warn` (20 min)
- File: `crates/tessera-graph-server/src/config.rs` — inline tests
- Test name: `parse_bool_env_true_values`
- Action: for each of `"true"`, `"1"`: set env, call function, assert returns `true`.
- Test name: `parse_bool_env_false_values`
- Action: for each of `"false"`, `"0"`: set env, call function, assert returns `false`.
- Test name: `parse_bool_env_invalid_falls_to_default`
- Action: set env to `"treu"`, call with default `true`, assert returns `true`. Set env to `"treu"`, call with default `false`, assert returns `false`.
- Test name: `parse_bool_env_unset_uses_default`
- Action: with env var unset, assert returns the default value.
- RED: function doesn't exist yet.

**1.6** [ ] Implement `parse_bool_env_or_warn` and wire into `main.rs` (20 min)
- File: `crates/tessera-graph-server/src/config.rs` — add `pub fn parse_bool_env_or_warn(name: &str, default: bool) -> bool`.
- Implementation:
  ```rust
  pub fn parse_bool_env_or_warn(name: &str, default: bool) -> bool {
      match std::env::var(name) {
          Err(_) => default,
          Ok(v) => match v.to_lowercase().as_str() {
              "true" | "1" => true,
              "false" | "0" => false,
              _ => {
                  tracing::warn!("{name} has invalid value '{v}' — using default ({default})");
                  default
              }
          },
      }
  }
  ```
- File: `crates/tessera-graph-server/src/main.rs:63-65` — replace with `parse_bool_env_or_warn("TESSERA_AUDIT_SYNC", true)`.
- GREEN: all Phase 1 tests pass.
- Output: consistent, warned env var parsing for booleans.

---

### Phase 2 — Health Flag Separation (Finding 1)

**Root cause observed in `flush_task.rs:182-199`**: The disk-space check calls `health.set_degraded()` at line 190. Then `update_health_state()` at line 194 is called — if `failed == false` (flush succeeded), it calls `health.set_healthy()` at line 43, overwriting the disk-space degradation in the same tick. The two degradation causes share one `AtomicBool`.

**Fix strategy**: Add a second `AtomicBool` to `AtomicHealthFlag` named `disk_degraded`. The overall `is_healthy()` returns `flush_flag && !disk_degraded`. Add `set_disk_degraded()` and `clear_disk_degraded()` methods. The flush task calls `set_disk_degraded()` when space is low, `clear_disk_degraded()` when space recovers. `update_health_state` continues to manage the flush flag only, unchanged.

**2.1** [ ] Write failing test: disk degradation survives flush success in same tick (20 min)
- File: `crates/tessera-graph-server/src/flush_task.rs` — inline tests
- Test name: `disk_degraded_not_overridden_by_flush_success`
- Logic:
  1. Create `AtomicHealthFlag::new()`.
  2. Call `health.set_disk_degraded()`.
  3. Call `update_health_state(false, 0, &health)` — simulates a successful flush.
  4. Assert `!health.is_healthy()` — disk degradation must persist.
- RED: `set_disk_degraded` doesn't exist on `AtomicHealthFlag`.

**2.2** [ ] Write failing test: disk recovery clears degradation (10 min)
- File: `crates/tessera-graph-server/src/flush_task.rs` — inline tests
- Test name: `disk_degraded_clears_when_space_recovers`
- Logic:
  1. Create flag, call `set_disk_degraded()`, assert `!is_healthy()`.
  2. Call `clear_disk_degraded()`, assert `is_healthy()`.
- RED: methods don't exist.

**2.3** [ ] Write failing test: flush errors degrade even with healthy disk (10 min)
- File: `crates/tessera-graph-server/src/flush_task.rs` — inline tests
- Test name: `flush_errors_degrade_independent_of_disk`
- Logic: drive `update_health_state` to `MAX_CONSECUTIVE_ERRORS` failures, assert `!is_healthy()`. Then `clear_disk_degraded()` (no-op, disk was never degraded), assert still `!is_healthy()`.
- RED: `clear_disk_degraded` doesn't exist.

**2.4** [ ] Extend `AtomicHealthFlag` with disk degradation API (25 min)
- File: `crates/tessera-graph-monitor/src/health.rs`
- Add second field: `disk_degraded: AtomicBool` initialized to `false`.
- Add `pub fn set_disk_degraded(&self)` and `pub fn clear_disk_degraded(&self)`.
- Change `is_healthy()` impl: `self.flag.load(Relaxed) && !self.disk_degraded.load(Relaxed)`.
- `new()` cannot be `const` after this change if `AtomicBool::new(false)` is not const-stable — verify: `AtomicBool::new` IS const in stable Rust, so `const fn new()` stays valid.
- Existing tests in `health.rs` must still pass (they do not touch disk degradation).
- Output: `AtomicHealthFlag` exposes `set_disk_degraded` and `clear_disk_degraded`.

**2.5** [ ] Update `flush_task.rs` disk-space block to use new API (20 min)
- File: `crates/tessera-graph-server/src/flush_task.rs:182-193`
- Change the disk-space block from calling `health.set_degraded()` to:
  ```rust
  if let Some(available) = check_available_disk_bytes(&base_dir) {
      if available < min_free {
          tracing::warn!( ... );
          health.set_disk_degraded();
      } else {
          health.clear_disk_degraded();
      }
  }
  ```
- The `else` branch clears disk degradation when space recovers — this was also missing before.
- GREEN: all Phase 2 tests pass.

**2.6** [ ] Write test: disk recovery auto-clears on next tick with sufficient space (15 min)
- File: `crates/tessera-graph-server/src/flush_task.rs` — inline tests
- Test name: `disk_degraded_auto_clears_on_recovery`
- This is a REFACTOR-phase regression guard.
- Logic: call `set_disk_degraded()`, assert `!is_healthy()`, call `clear_disk_degraded()`, assert `is_healthy()`. (The tick-level integration is verified by the `flush_task` async loop which is not unit-tested here — the unit API is sufficient.)
- Output: spec is locked in.

---

### Phase 3 — Audit Tracing (Finding 5)

**Root cause observed in `tessera-graph-audit/src/lib.rs:282-298`**: `eprintln!` is used for write errors, flush errors, and sync_data errors. Tracing is not a dependency of `tessera-graph-audit`.

**Fix strategy**: Add `tracing = "0.1"` to `tessera-graph-audit/Cargo.toml`. Replace all three `eprintln!` calls in `AuditWriterTask::run` with `tracing::warn!`. Note: use `warn!` not `error!` because audit write failures are critical but not panic-worthy; the task continues processing. This aligns with the comment "batched flush" — losing one batch is bad but the task must keep running.

**3.1** [ ] Write failing test: `AuditWriterTask::run` uses tracing (not eprintln) (20 min)
- This is a structural/compile-time test, not a runtime assertion. The test cannot directly capture `eprintln!` output without spawning a subprocess. Instead, write an integration test that verifies the audit crate compiles with tracing and that `AuditWriterTask::run` doesn't reference `std::io::stderr` directly.
- Practical approach: the RED step is simply that `tracing` is not in the Cargo.toml — any code using `tracing::warn!` in `lib.rs` will fail to compile.
- File: `crates/tessera-graph-audit/Cargo.toml` — RED: add `tracing = "0.1"` (this is the fix that makes the GREEN step work).
- Actually structure this as: (a) write a test that verifies no `eprintln` appears in `lib.rs` at the source level — this is a grep-style test that cannot be done in Rust. Instead: the RED is the clippy/lint failure that `eprintln!` in library code is not flagged but is inconsistent. The meaningful RED here is a documentation test showing the expected behavior.
- Realistic RED: Add a test to `lib.rs` that captures tracing events and asserts the error path emits a warning. This requires `tracing-test` dev dependency.
- File: `crates/tessera-graph-audit/Cargo.toml` — add to `[dev-dependencies]`: `tracing-test = "0.2"`.
- Test name: `audit_writer_emits_tracing_warn_on_write_error` (async integration test in `crates/tessera-graph-audit/tests/audit_writer_tracing_test.rs`)
- Setup: create an `AuditWriterTask` with a writer backed by a `BufWriter<File>` where the underlying file is closed immediately (simulate a write error on the next write). With `#[traced_test]`, verify at least one `WARN` event is emitted.
- Note: this test is complex to set up reliably. Defer to a simpler formulation: create a `AuditWriterTask` via `AuditLog::open_with_sync`, immediately close the underlying fd with a custom writer, then send one entry and await the task. The `#[traced_test]` macro captures logs.
- RED: test compiles but `tracing::warn!` is not called (still `eprintln!` in source) → the tracing subscriber sees no event → assertion fails.

**3.2** [ ] Add `tracing` dependency and replace `eprintln!` calls (20 min)
- File: `crates/tessera-graph-audit/Cargo.toml` — add `tracing = "0.1"` to `[dependencies]`.
- File: `crates/tessera-graph-audit/src/lib.rs:282` — replace:
  ```rust
  eprintln!("audit write error: {e}");
  ```
  with:
  ```rust
  tracing::warn!(error = %e, "audit write error");
  ```
- File: `crates/tessera-graph-audit/src/lib.rs:288` — same replacement for drain loop.
- File: `crates/tessera-graph-audit/src/lib.rs:293-294` — replace flush eprintln:
  ```rust
  tracing::warn!(error = %e, "audit flush error");
  ```
- File: `crates/tessera-graph-audit/src/lib.rs:296-298` — replace sync_data eprintln:
  ```rust
  tracing::warn!(error = %e, "audit sync_data error");
  ```
- GREEN: tests pass, no `eprintln!` remains in `lib.rs`.

**3.3** [ ] REFACTOR: verify no `eprintln!` remains in audit crate (5 min)
- Run `grep -n "eprintln!" crates/tessera-graph-audit/src/lib.rs` — must return empty.
- This is a manual verification step, not a code change.

---

### Phase 4 — LRU Lock Optimization (Finding 6)

**Root cause observed in `registry.rs:139-144`**: `touch_access_order` acquires a write lock (`access_order.write()`) on every cache hit. Since the `access_order` field is a `VecDeque` that is always written (retain + push_back), `RwLock` provides no benefit — readers never get a shared read. A `Mutex` has lower overhead because it skips the reader-count logic.

**Fix strategy**: Replace `RwLock<VecDeque<DatabaseAddress>>` with `Mutex<VecDeque<DatabaseAddress>>`. All `access_order.write()` calls become `.lock()`. No `access_order.read()` calls exist (confirmed by grepping the file), so this is a drop-in replacement with reduced overhead.

**4.1** [ ] Write failing test: `access_order` uses Mutex (structural contract test) (15 min)
- The type change is the meaningful RED here. Write a compile-time test that ensures `TenantRegistry` compiles with the new field type.
- More useful: write a concurrency test that verifies multiple simultaneous cache hits don't deadlock.
- File: `crates/tessera-graph-tenant/src/registry.rs` — inline tests
- Test name: `concurrent_get_or_load_same_addr_no_deadlock`
- Logic: spawn 8 threads, each calling `registry.get_or_load(&addr)` concurrently, assert all return `Ok`. This test passes with both `RwLock` and `Mutex`, but it documents the concurrency contract.
- Test name: `lru_eviction_still_works_after_mutex_change`
- Logic: create registry with `max_loaded=2`, load 3 different addresses, verify `loaded_count() == 2` (one was evicted).
- RED: these tests currently compile and pass — they are REFACTOR-phase regression guards to be written first so they catch any regression introduced by the type change.

**4.2** [ ] Replace `RwLock<VecDeque>` with `Mutex<VecDeque>` in `TenantRegistry` (20 min)
- File: `crates/tessera-graph-tenant/src/registry.rs`
- Import change: `std::sync::{Arc, Mutex, RwLock}` (add `Mutex`).
- Field: `access_order: Mutex<VecDeque<DatabaseAddress>>`.
- Constructor: `access_order: Mutex::new(VecDeque::new())`.
- All `self.access_order.write()` → `self.access_order.lock()`. Note: `Mutex::lock()` returns `LockResult<MutexGuard>`, so the `map_err` patterns need updating from `map_err(|_| ...)` idiom — but in this file, `access_order` accesses use `if let Ok(mut order) = ...` pattern, which works identically for `Mutex::lock()`.
- In `unload()`: `self.access_order.write()` → `self.access_order.lock()`.
- In the slow-path write-lock eviction block: `self.access_order.write()` → `self.access_order.lock()`.
- GREEN: tests pass, `cargo clippy` clean.

**4.3** [ ] REFACTOR: remove `RwLock` import if no longer needed (5 min)
- File: `crates/tessera-graph-tenant/src/registry.rs:5`
- Check if `RwLock` is still used (it is — for `graphs: RwLock<HashMap<...>>`). Keep import.
- Verify `Mutex` is only used for `access_order` field. Clean up any unused imports.

---

### Phase 5 — Async Blocking (Finding 7)

**Root cause observed in `main.rs:196-213`**: Inside a `tokio::spawn` async block, `std::fs::read_to_string("/proc/self/status")` and `std::fs::read_dir("/proc/self/fd")` are called synchronously. These are blocking syscalls that can stall the Tokio executor thread.

**Fix strategy**: Wrap the entire Linux metrics block in `tokio::task::spawn_blocking`. The result is awaited before updating the atomic metrics. The macOS block has no blocking I/O (it's a no-op comment) — leave it unchanged.

**5.1** [ ] Write failing test: metrics background task compiles with spawn_blocking (15 min)
- This fix is in `main.rs` (binary), which cannot be unit-tested directly. The meaningful verification is:
  (a) Compile-time: `cargo build` succeeds.
  (b) The `spawn_blocking` call returns a `JoinHandle` that must be `.await`ed — this is a structural change that clippy enforces.
- Write a documentation test as a comment explaining the contract: `// spawn_blocking ensures the Tokio executor is not stalled by /proc reads`.
- Practical RED: annotate the existing blocking calls with `// BLOCKING — must wrap in spawn_blocking` and open the issue. The actual test is that `cargo clippy` does not emit `clippy::await_holding_lock` or similar. `tokio::spawn` does not flag blocking calls by default, but adding `#[tokio::main(flavor = "current_thread")]` to a test and running the blocking path would stall — not a useful automated test.
- Better approach: use `tokio_test` or simply trust the fix is structural. The RED in this case is "code does not use spawn_blocking" (observed by reading the file). GREEN is "code uses spawn_blocking". Add a comment-based contract test.
- Actual test to write: a unit test in a `#[cfg(test)]` block inside `main.rs` is not possible (binary). Write the test in a dedicated module inside the lib if the metrics logic is extracted — but extracting it is a larger refactor.
- Pragmatic decision: the test for this finding is a compile + code review. Document the change clearly and add a `// spawn_blocking: prevents blocking the Tokio executor` comment on the spawn site.

**5.2** [ ] Wrap Linux /proc reads in `spawn_blocking` (20 min)
- File: `crates/tessera-graph-server/src/main.rs` — the `#[cfg(target_os = "linux")]` block at lines 195-214.
- Replace the two blocking calls with a single `spawn_blocking` block:
  ```rust
  #[cfg(target_os = "linux")]
  {
      let rss_bytes_result = tokio::task::spawn_blocking(|| {
          let rss = std::fs::read_to_string("/proc/self/status")
              .ok()
              .and_then(|s| {
                  s.lines()
                      .find(|l| l.starts_with("VmRSS:"))
                      .and_then(|l| l.split_whitespace().nth(1))
                      .and_then(|v| v.parse::<u64>().ok())
              })
              .map_or(0, |kb| kb * 1024);
          let fds = std::fs::read_dir("/proc/self/fd")
              .map(|e| e.count() as u64)
              .unwrap_or(0);
          (rss, fds)
      })
      .await;
      if let Ok((rss, fds)) = rss_bytes_result {
          metrics_bg.process_rss_bytes.store(rss, std::sync::atomic::Ordering::Relaxed);
          metrics_bg.open_fds.store(fds, std::sync::atomic::Ordering::Relaxed);
      }
  }
  ```
- The outer async block is inside `tokio::spawn` already. Adding `.await` inside a `tokio::spawn` async block is valid.
- GREEN: `cargo build` succeeds, `cargo clippy` clean.

---

### Phase 6 — DISCARD After Partial PULL Test (Finding 8)

**Root cause observed**: `bolt_handler_test.rs` covers PULL n=2 and PULL all, but no test sends DISCARD after a partial PULL. The `handle_discard` implementation sets `pending_result = None` and sends `SUCCESS {}`. The test must verify: (1) DISCARD clears the cursor, (2) SUCCESS has no `has_more` (or `has_more=false`), (3) a subsequent RUN+PULL works cleanly on a fresh cursor.

**Observed helpers available**:
- `setup_5_nodes()` — creates 5 Person nodes, authenticates, returns `(writer, reader, shutdown, dir)`.
- `pull_n(n)` — creates PULL with `n` parameter.
- `bolt_send`, `bolt_recv` — wire-level helpers.
- `dict_bool(&resp, "has_more")` — extracts has_more from SUCCESS metadata.
- `BoltRequest::Discard { extra: vec![] }` — the DISCARD message type.

**6.1** [ ] Write failing test: DISCARD after partial PULL clears cursor (25 min)
- File: `crates/tessera-graph-server/tests/bolt_handler_test.rs`
- Test name: `discard_after_partial_pull_clears_cursor_and_next_run_works`
- Logic:
  1. `let (mut writer, mut reader, _shutdown, _dir) = setup_5_nodes().await;`
  2. RUN `"MATCH (n:Person) RETURN n.name"` → assert SUCCESS.
  3. PULL n=2 → drain 2 RECORDs, receive SUCCESS with `has_more=true`.
  4. Send `BoltRequest::Discard { extra: vec![] }` → receive SUCCESS. Assert `has_more` is either absent or `false` (the current implementation sends `SUCCESS {}` with no metadata, so `dict_bool(&resp, "has_more")` returns `None` — that is acceptable per Bolt 4.4 spec for DISCARD).
  5. Send a second RUN `"MATCH (n:Person) RETURN n.name"` → assert SUCCESS (not IGNORED or FAILURE — this verifies the handler is not stuck in a bad state).
  6. PULL n=-1 → drain all 5 RECORDs, receive SUCCESS with `has_more=false`.
- Note: step 6 proves that after DISCARD, the next PULL returns a full fresh result (no stale cursor leaking).
- RED: the test should currently PASS because `handle_discard` already clears `pending_result`. This is a COVERAGE test, not a bug fix test. The point is locking in the behavior so future refactors cannot break it.
- Mark the test with a comment: `// Regression guard: DISCARD after partial PULL must clean up pending_result.`
- Output: test compiles and passes on first run (GREEN immediately). This is intentional — it's a regression guard, not a bug reproduction.

**6.2** [ ] Write second test: DISCARD with no pending result succeeds (10 min)
- File: `crates/tessera-graph-server/tests/bolt_handler_test.rs`
- Test name: `discard_without_pending_result_returns_success`
- Logic:
  1. Auth, then send DISCARD immediately after HELLO (no RUN preceding it).
  2. Assert SUCCESS response.
- Rationale: Verifies DISCARD is idempotent when there is nothing to discard. This edge case is not covered and some Bolt clients may send DISCARD speculatively.
- RED/GREEN: likely GREEN immediately, serving as a regression guard.

---

## Estimación Total

| Phase | Findings | Estimate |
|-------|----------|----------|
| Phase 1 — Env var validation | 2, 3, 4 | ~2.5 h |
| Phase 2 — Health flag separation | 1 | ~1.5 h |
| Phase 3 — Audit tracing | 5 | ~1 h |
| Phase 4 — LRU lock | 6 | ~1 h |
| Phase 5 — Async blocking | 7 | ~0.5 h |
| Phase 6 — DISCARD tests | 8 | ~0.5 h |
| **Total** | | **~7 h** |

---

## Criterios de Éxito

- [ ] `cargo test --workspace` passes with zero failures.
- [ ] `cargo clippy --workspace -- -D warnings` emits zero diagnostics.
- [ ] `cargo build --workspace` succeeds in release mode.
- [ ] No `eprintln!` remains in `crates/tessera-graph-audit/src/lib.rs`.
- [ ] `parse_env_or_warn` is callable from `main.rs`.
- [ ] `parse_bool_env_or_warn` exists and is tested for all 4 cases (true, 1, false, 0) + invalid + unset.
- [ ] `TESSERA_MIN_FREE_DISK_MB` is parsed by `PersistenceConfig::from_env()` with a warning on invalid input.
- [ ] `AtomicHealthFlag` has `set_disk_degraded` and `clear_disk_degraded`.
- [ ] `flush_task.rs` disk-space block calls `set_disk_degraded()` / `clear_disk_degraded()` instead of `set_degraded()`.
- [ ] `TenantRegistry::access_order` is `Mutex<VecDeque<...>>` instead of `RwLock<VecDeque<...>>`.
- [ ] `/proc` reads in `main.rs` are wrapped in `tokio::task::spawn_blocking`.
- [ ] `bolt_handler_test.rs` includes `discard_after_partial_pull_clears_cursor_and_next_run_works` and `discard_without_pending_result_returns_success`.

---

## Notas de Implementación

### Orden de ejecución recomendado

Execute phases in order. Phase 2 depends on the `AtomicHealthFlag` changes from `tessera-graph-monitor` being compiled before `tessera-graph-server` tests run — this is guaranteed by Cargo's dependency graph. Phase 1 changes to `config.rs` are self-contained and don't affect other phases.

### Clippy anticipations

- In Phase 1.6, `match v.to_lowercase().as_str()` may trigger `clippy::match_on_vec_items` — use `v.to_ascii_lowercase()` instead to return a `String` and match on `.as_str()`, or use `matches!(v.as_str(), "true" | "1")` in two separate `if` branches.
- In Phase 2.4, the `new()` constructor must use `AtomicBool::new(false)` for `disk_degraded`. If the struct is `const fn new()`, verify both `AtomicBool::new(true)` and `AtomicBool::new(false)` are valid in const context — they are since Rust 1.24.
- In Phase 4.2, `Mutex::lock()` returns `LockResult<MutexGuard>`, and the existing `if let Ok(mut order) = ...` pattern applies identically. No changes to error handling patterns needed.
- In Phase 5.2, the `spawn_blocking` closure captures `metrics_bg` which is `Arc<MetricsRegistry>` — but the closure does not need it (metrics are updated after `.await`). The closure only reads `/proc`, so it captures nothing. Verify no `move` closure captures break `Send` bounds.

### Env var test isolation

Tests that set env vars via `std::env::set_var` are not thread-safe by default. All env var tests in `config.rs` must use unique env var names (e.g., `TEST_PARSE_ENV_WARN_1`, `TEST_PARSE_BOOL_ENV_2`) and clean up with `std::env::remove_var` in a `defer`-like pattern, or use `#[serial_test]`. Prefer unique names per test over serial execution to avoid adding a dependency.
