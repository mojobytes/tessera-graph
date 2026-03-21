# TDD Plan — Prometheus Metrics for TesseraGraph Enterprise

## Context

TesseraGraph Enterprise has a complete server stack (TLS, auth, RBAC, GQL/Cypher execution) but no
observability surface. This plan instruments the server with Prometheus-compatible metrics exposed via
an HTTP `/metrics` endpoint on a separate port. The implementation lives in the already-present but
empty `tessera-monitor` crate and integrates into `tessera-server` at three instrumentation points:
the accept loop (`listener.rs`), the per-connection handler (`connection.rs`), and the binary
entrypoint (`main.rs`).

**Stack detected**: Rust 2024, edition 2024, rust-version 1.85, Tokio 1 async runtime, thiserror 2
(workspace), serde_json (workspace), strict Clippy (`all = deny`, `pedantic = warn`, `nursery = warn`),
`unsafe_code = "forbid"`.

**Conventions observed**:
- Error types via `thiserror` in `error.rs`, aliased as `pub type Result<T>`.
- Integration tests in `crates/<crate>/tests/<name>_test.rs` as separate files (not inline modules).
- Shared test helpers in `crates/<crate>/tests/common/mod.rs`.
- Copyright header `// Copyright 2026 BelowZero Security OU. All rights reserved.` on every file.
- Module exports via `lib.rs` with explicit `pub use`.
- `#[must_use]` on all constructors and pure-value methods.
- Throughput thresholds gated on `cfg!(debug_assertions)` for debug vs. release.
- All crates use `workspace = true` for shared package fields.

**Affects hot path**: YES — `record_query_duration` is called on every query execution. Atomic
operations on the hot path must not introduce measurable regression. A throughput regression guard
is mandatory (Phase 4).

---

## Decisions Already Made

None required. The architecture is fully specified in the prompt. `AtomicU64` with `f64::to_bits()` /
`f64::from_bits()` for the histogram sum is the correct pattern given `unsafe_code = "forbid"`.
Manual HTTP/1.1 over raw `tokio::net::TcpListener` avoids any new external dependency.

---

## New Dependencies

**`crates/tessera-monitor/Cargo.toml`**:
- `tokio = { version = "1", features = ["net", "rt-multi-thread", "macros", "io-util"] }` — TCP
  listener for the metrics HTTP server and async test helpers.

**`crates/tessera-server/Cargo.toml`** — no new dependencies; `tessera-monitor` is already in
workspace deps and already listed as a dependency of `tessera-server`.

---

## Plan de Ejecución

---

### Phase 1: MetricsRegistry — Pure Atomic State

Goal: define the registry struct and its increment/observe helpers. No I/O. All tests are
synchronous (no `#[tokio::test]` needed here).

Test file: `crates/tessera-monitor/tests/registry_test.rs`
Source files: `crates/tessera-monitor/src/registry.rs`, `crates/tessera-monitor/src/lib.rs`

---

#### Cycle 1.1 — Registry construction and counter increment

- RED: Create `crates/tessera-monitor/tests/registry_test.rs`.
  Write test `counter_starts_at_zero_and_increments`:
  ```rust
  let r = MetricsRegistry::new(256);
  assert_eq!(r.connections_accepted.load(Ordering::Relaxed), 0);
  r.connections_accepted.fetch_add(1, Ordering::Relaxed);
  assert_eq!(r.connections_accepted.load(Ordering::Relaxed), 1);
  ```
  Write test `connections_max_set_from_constructor`:
  ```rust
  let r = MetricsRegistry::new(128);
  assert_eq!(r.connections_max.load(Ordering::Relaxed), 128);
  ```
  Both tests must fail to compile because `MetricsRegistry` does not exist yet.

- GREEN: Create `crates/tessera-monitor/src/registry.rs`.
  Define `MetricsRegistry` with all fields listed in the design: counters, gauges, histogram
  buckets array, sum, and count — all `AtomicU64`. Implement `pub fn new(max_connections: u64)
  -> Self` that zeroes all atomics and sets `connections_max`. Export from `lib.rs`.

- REFACTOR: Add `#[must_use]` to `new`. Verify `pub` visibility on all fields is correct
  (fields are public so instrumentation points can write directly without going through
  method calls on the hot path).

---

#### Cycle 1.2 — Histogram bucket constants and `record_query_duration`

- RED: Add test `histogram_record_duration_increments_correct_bucket`:
  ```rust
  let r = MetricsRegistry::new(256);
  r.record_query_duration(0.007); // 7 ms — falls in ≤0.01 bucket (index 2)
  assert_eq!(r.query_duration_count.load(Ordering::Relaxed), 1);
  // All buckets with upper bound >= 0.007 must be incremented (cumulative)
  // BUCKETS = [0.001, 0.005, 0.01, 0.025, ...]
  // index 0 (0.001): 0.007 > 0.001 → NOT incremented
  // index 1 (0.005): 0.007 > 0.005 → NOT incremented
  // index 2 (0.010): 0.007 <= 0.01 → incremented, and all higher indices too
  assert_eq!(r.query_duration_buckets[0].load(Ordering::Relaxed), 0);
  assert_eq!(r.query_duration_buckets[1].load(Ordering::Relaxed), 0);
  assert_eq!(r.query_duration_buckets[2].load(Ordering::Relaxed), 1);
  assert_eq!(r.query_duration_buckets[3].load(Ordering::Relaxed), 1);
  ```
  Add test `histogram_sum_accumulates_as_f64_bits`:
  ```rust
  let r = MetricsRegistry::new(256);
  r.record_query_duration(0.050);
  r.record_query_duration(0.100);
  let sum = f64::from_bits(r.query_duration_sum.load(Ordering::Relaxed));
  assert!((sum - 0.150).abs() < 1e-10);
  ```
  Note: the sum field requires a compare-exchange loop because two atomic operations
  (load + store) are not atomic together. The test verifies correctness, not the
  internal CAS loop.

- GREEN: Add `pub const HISTOGRAM_BUCKETS: [f64; 12]` in `registry.rs`:
  `[0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0]`.
  Implement `pub fn record_query_duration(&self, seconds: f64)`:
  - For each bucket where `seconds <= upper_bound`, call `fetch_add(1, Relaxed)` on all
    remaining bucket counts (cumulative Prometheus semantics — once the value fits in a
    bucket, all larger buckets also include it).
  - CAS loop to add `seconds` to `query_duration_sum` via `f64::to_bits` / `f64::from_bits`.
  - `fetch_add(1, Relaxed)` on `query_duration_count`.

- REFACTOR: Extract the CAS-add-f64 pattern into a private `fn cas_add_f64(atom: &AtomicU64,
  delta: f64)` helper to avoid duplication if needed elsewhere.

---

### Phase 2: Prometheus Text Rendering

Goal: produce the exact Prometheus text exposition format from a `MetricsRegistry`. Pure
string-building, no I/O. Tests verify byte-exact output lines.

Test file: `crates/tessera-monitor/tests/render_test.rs`
Source file: `crates/tessera-monitor/src/render.rs`

---

#### Cycle 2.1 — Gauge and counter rendering

- RED: Create `crates/tessera-monitor/tests/render_test.rs`.
  Write test `render_contains_gauge_metadata_and_value`:
  ```rust
  let r = Arc::new(MetricsRegistry::new(256));
  r.connections_active.store(3, Ordering::Relaxed);
  let output = render_prometheus(&r);
  assert!(output.contains("# HELP tessera_connections_active"));
  assert!(output.contains("# TYPE tessera_connections_active gauge"));
  assert!(output.contains("tessera_connections_active 3\n"));
  assert!(output.contains("tessera_connections_max 256\n"));
  ```
  Write test `render_counter_with_label`:
  ```rust
  let r = Arc::new(MetricsRegistry::new(256));
  r.auth_success.store(10, Ordering::Relaxed);
  r.auth_failure.store(2, Ordering::Relaxed);
  let output = render_prometheus(&r);
  assert!(output.contains(r#"tessera_auth_attempts_total{result="success"} 10"#));
  assert!(output.contains(r#"tessera_auth_attempts_total{result="failure"} 2"#));
  ```
  Write test `render_query_counter_with_two_labels`:
  ```rust
  let r = Arc::new(MetricsRegistry::new(256));
  r.queries_gql_read.store(5, Ordering::Relaxed);
  let output = render_prometheus(&r);
  assert!(output.contains(
      r#"tessera_queries_total{language="gql",type="read"} 5"#
  ));
  ```

- GREEN: Create `crates/tessera-monitor/src/render.rs`.
  Implement `pub fn render_prometheus(registry: &MetricsRegistry) -> String` using
  `String::with_capacity` and `write!` / `writeln!` macro via `fmt::Write`.
  Format per Prometheus text exposition (version 0.0.4):
  ```
  # HELP <name> <description>
  # TYPE <name> <type>
  <name>[{label="value",...}] <value>
  ```
  Handle all metrics: gauges, all counters (with labels where applicable), and produce
  the histogram skeleton (buckets, sum, count) but with zero values to pass compilation.
  Export from `lib.rs`.

- REFACTOR: Extract a private `fn write_counter(buf: &mut String, name: &str, help: &str,
  value: u64)` and `fn write_labeled_counter(buf: &mut String, name: &str, labels: &str,
  value: u64)` helpers to reduce repetition in the render function.

---

#### Cycle 2.2 — Histogram rendering

- RED: Add test `render_histogram_buckets_and_sum`:
  ```rust
  let r = Arc::new(MetricsRegistry::new(256));
  r.record_query_duration(0.007); // lands in ≤0.01 bucket
  let output = render_prometheus(&r);
  // Verify exact bucket lines
  assert!(output.contains(
      r#"tessera_query_duration_seconds_bucket{le="0.001"} 0"#
  ));
  assert!(output.contains(
      r#"tessera_query_duration_seconds_bucket{le="0.01"} 1"#
  ));
  assert!(output.contains(
      r#"tessera_query_duration_seconds_bucket{le="+Inf"} 1"#
  ));
  assert!(output.contains("tessera_query_duration_seconds_count 1\n"));
  // Sum should contain the float value (exact match fragile; check prefix)
  assert!(output.contains("tessera_query_duration_seconds_sum "));
  ```

- GREEN: Extend `render_prometheus` to emit all 12 bucket lines (iterating
  `HISTOGRAM_BUCKETS`), the mandatory `{le="+Inf"}` line (equal to `query_duration_count`),
  `_sum`, and `_count`. Format bucket upper bounds with enough decimal places to avoid
  rendering `0.001` as `0.001000000001` — use `format!("{:.3}", upper)` for buckets up to
  `0.1` and `format!("{:.1}", upper)` for those >= `0.25`, or a single
  `format_bucket_le(f64) -> String` helper with a match on magnitude.

- REFACTOR: Ensure all `# TYPE` lines precede their metric lines. Verify output passes
  `promtool check metrics` format (mentally — no tooling dependency required, just review
  the spec). Clippy nursery lint `clippy::format_collect` may fire if using iterator
  chain with format; suppress with `#[allow]` if needed or restructure.

---

### Phase 3: HTTP Metrics Server

Goal: an async TCP listener that speaks HTTP/1.1 and serves `GET /metrics`. Uses only
`tokio::net::TcpListener` — no hyper. Tests use `tokio::net::TcpStream::connect` and raw
byte reads.

Test file: `crates/tessera-monitor/tests/http_server_test.rs`
Source file: `crates/tessera-monitor/src/server.rs`

---

#### Cycle 3.1 — GET /metrics returns 200 with Prometheus body

- RED: Create `crates/tessera-monitor/tests/http_server_test.rs`.
  Write test `get_metrics_returns_200_with_prometheus_content_type`:
  ```rust
  let registry = Arc::new(MetricsRegistry::new(256));
  let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
  let addr = listener.local_addr().unwrap();
  let reg = Arc::clone(&registry);
  tokio::spawn(async move {
      serve_metrics_on(listener, reg).await;
  });
  tokio::time::sleep(Duration::from_millis(20)).await;

  let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
  stream.write_all(b"GET /metrics HTTP/1.1\r\nHost: localhost\r\n\r\n").await.unwrap();

  let mut buf = Vec::new();
  stream.read_to_end(&mut buf).await.unwrap();
  let response = String::from_utf8(buf).unwrap();

  assert!(response.starts_with("HTTP/1.1 200 OK\r\n"));
  assert!(response.contains("Content-Type: text/plain; version=0.0.4\r\n"));
  assert!(response.contains("tessera_connections_max 256"));
  ```

- GREEN: Create `crates/tessera-monitor/src/server.rs`.
  Implement `pub async fn serve_metrics_on(listener: tokio::net::TcpListener,
  registry: Arc<MetricsRegistry>)` — never returns (runs until task is cancelled).
  For each accepted TCP connection, spawn a task that:
  1. Reads bytes until `\r\n\r\n` is found (request headers complete).
  2. Checks if the first line starts with `GET /metrics`.
  3. If yes: calls `render_prometheus(&registry)`, builds an HTTP/1.1 200 response with
     `Content-Type: text/plain; version=0.0.4`, `Content-Length`, and the body.
  4. If no: writes `HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n`.
  5. Flushes and drops the connection.
  Export `serve_metrics_on` from `lib.rs`.
  Add `pub async fn serve_metrics(addr: &str, registry: Arc<MetricsRegistry>) ->
  std::io::Result<()>` that binds the listener and delegates to `serve_metrics_on` — this
  is the public API called from `main.rs`.

- REFACTOR: Extract `async fn handle_connection(stream: TcpStream, registry:
  Arc<MetricsRegistry>)` to keep `serve_metrics_on` readable. Ensure the request-read loop
  has a maximum byte limit (e.g., 8 KiB) to prevent memory exhaustion from malformed
  requests — this is a security-by-default requirement.

---

#### Cycle 3.2 — Non-/metrics path returns 404

- RED: Add test `non_metrics_path_returns_404`:
  ```rust
  // Same setup as above, different request:
  stream.write_all(b"GET /health HTTP/1.1\r\nHost: localhost\r\n\r\n").await.unwrap();
  let response = /* read response */;
  assert!(response.starts_with("HTTP/1.1 404 Not Found\r\n"));
  ```

- GREEN: Covered by the routing logic already written in Cycle 3.1. This cycle just verifies
  the test is RED before the 404 branch was implemented — if GREEN was written correctly in
  3.1, this test will pass immediately. Mark GREEN by running tests; no new code needed.

- REFACTOR: none.

---

#### Cycle 3.3 — serve_metrics binds and responds (public API surface)

- RED: Add test `serve_metrics_public_api_binds_and_serves`:
  ```rust
  let registry = Arc::new(MetricsRegistry::new(64));
  // Bind on port 0 — but serve_metrics takes an addr string, not a pre-bound listener.
  // We cannot know the port in advance; use a helper that returns the bound port.
  // Test serve_metrics_on directly (already tested) — this test verifies that
  // serve_metrics returns Err on an invalid address:
  let result = serve_metrics("not-a-valid-address:99999", Arc::clone(&registry)).await;
  assert!(result.is_err());
  ```

- GREEN: `serve_metrics` already binds via `TcpListener::bind` and propagates `io::Error`.
  An invalid address causes `bind` to fail and return `Err`. No new code needed.

- REFACTOR: none.

---

### Phase 4: Server Instrumentation

Goal: wire `MetricsRegistry` into `tessera-server` at the three instrumentation points.
`MetricsRegistry` is wrapped in `Arc` and passed through `ServerContext` or as a
separate parameter to `serve`/`serve_tls`.

Source files to modify:
- `crates/tessera-server/src/context.rs` — add `Arc<MetricsRegistry>` field
- `crates/tessera-server/src/listener.rs` — accept/reject + active count + TLS failures
- `crates/tessera-server/src/connection.rs` — auth and query metrics
- `crates/tessera-server/src/main.rs` — wire metrics, start HTTP server

Test file: `crates/tessera-server/tests/metrics_integration_test.rs`

---

#### Cycle 4.1 — Add MetricsRegistry to ServerContext

- RED: Create `crates/tessera-server/tests/metrics_integration_test.rs`.
  Write test `context_exposes_metrics_registry`:
  ```rust
  let ctx = test_context_with_metrics(256);
  let m = ctx.metrics();
  assert_eq!(m.connections_max.load(Ordering::Relaxed), 256);
  ```
  This test requires `test_context_with_metrics` which does not exist yet, so it fails
  to compile. Add `test_context_with_metrics` to `tests/common/mod.rs` once GREEN.

- GREEN: Add `metrics: Arc<MetricsRegistry>` field to `ServerContext`. Add
  `pub fn metrics(&self) -> &Arc<MetricsRegistry>` accessor. Update `ServerContext::new`
  to accept `Arc<MetricsRegistry>`. Update `main.rs` to construct and pass the registry.
  Update `test_context()` in `tests/common/mod.rs` to use a default
  `MetricsRegistry::new(256)`. Add `test_context_with_metrics(max: u64)` helper.
  All existing tests must still compile and pass — the change is additive.

- REFACTOR: Consider a `ServerContext::builder()` pattern if the constructor grows too
  large — but only if Clippy warns about too many arguments (`clippy::too_many_arguments`).

---

#### Cycle 4.2 — Instrument listener: accepted, rejected, active connections

- RED: Add test `listener_increments_accepted_counter`:
  ```rust
  let (ctx, registry) = test_context_and_registry(10);
  let graph = Arc::new(RwLock::new(Graph::new()));
  let listener = TesseraListener::bind("127.0.0.1:0").await.unwrap();
  let addr = listener.local_addr().unwrap();
  let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
  tokio::spawn(async move {
      let _ = listener.serve(ctx, graph, shutdown_rx, 10, Duration::from_secs(30)).await;
  });
  tokio::time::sleep(Duration::from_millis(50)).await;

  let _stream = tokio::net::TcpStream::connect(addr).await.unwrap();
  tokio::time::sleep(Duration::from_millis(50)).await;
  assert_eq!(registry.connections_accepted.load(Ordering::Relaxed), 1);
  assert_eq!(registry.connections_active.load(Ordering::Relaxed), 1);
  let _ = shutdown_tx.send(true);
  ```
  Add test `listener_increments_rejected_counter`:
  ```rust
  // max_connections = 1, connect two clients; second must increment rejected
  assert_eq!(registry.connections_rejected.load(Ordering::Relaxed), 1);
  ```

- GREEN: In `listener.rs` `serve` and `serve_tls`, after a successful `try_acquire_owned`:
  - `ctx.metrics().connections_accepted.fetch_add(1, Ordering::Relaxed)`
  - `ctx.metrics().connections_active.fetch_add(1, Ordering::Relaxed)`
  In the `Err(_)` capacity branch:
  - `ctx.metrics().connections_rejected.fetch_add(1, Ordering::Relaxed)`
  In the spawned task, after `handler.run().await` returns (connection closed), decrement
  active: `ctx.metrics().connections_active.fetch_sub(1, Ordering::Relaxed)`.
  The `_permit` drop already releases the semaphore; the metric decrement is separate.

- REFACTOR: The fetch_sub for `connections_active` must happen even if `handler.run()`
  returns `Err`. The `let _ = handler.run().await;` pattern already swallows the error;
  the decrement after it is unconditional, which is correct.

---

#### Cycle 4.3 — Instrument listener: TLS handshake failures

- RED: Add test `tls_handshake_failure_increments_counter`.
  Using `serve_tls` with a TLS acceptor that will reject a plain TCP connection:
  ```rust
  // Connect with plain TCP to a TLS listener — handshake must fail.
  // After the failed connection attempt, check:
  assert_eq!(registry.tls_handshake_failures.load(Ordering::Relaxed), 1);
  ```
  Note: This test requires a real `serve_tls` call. The test setup in
  `tests/common/mod.rs` already has `test_tls_config()`. Use a plain `TcpStream::connect`
  (no TLS) to trigger the handshake failure.

- GREEN: In the `Err(e)` branch of `tls_acceptor.accept(stream).await` in `serve_tls`:
  - `ctx.metrics().tls_handshake_failures.fetch_add(1, Ordering::Relaxed)`

- REFACTOR: none.

---

#### Cycle 4.4 — Instrument connection: auth attempts

- RED: Add test `successful_login_increments_auth_success`:
  ```rust
  // Use the existing spawn_handler helper (adapted to accept a metrics registry).
  // Send Login with valid credentials; assert auth_success == 1 after response.
  let (ctx, registry) = test_context_and_registry(256);
  let (mut writer, mut reader, _shutdown) =
      spawn_handler_with_context(Arc::clone(&ctx), graph);
  // send Login { username: "admin", password: "Admin@Init1!" }
  // await AuthOk response
  assert_eq!(registry.auth_success.load(Ordering::Relaxed), 1);
  assert_eq!(registry.auth_failure.load(Ordering::Relaxed), 0);
  ```
  Add test `failed_login_increments_auth_failure`.

- GREEN: In `connection.rs` `handle_login`, after sending `ServerMessage::AuthOk`:
  - `self.ctx.metrics().auth_success.fetch_add(1, Ordering::Relaxed)`
  After `send_auth_failure`:
  - `self.ctx.metrics().auth_failure.fetch_add(1, Ordering::Relaxed)`
  The `send_auth_failure` helper is called from multiple branches (local auth, external
  auth, session creation failure); add the increment inside `send_auth_failure` itself
  to avoid duplication.

- REFACTOR: `handle_external_login` also calls `send_auth_failure` on failure — covered
  automatically because the increment is in `send_auth_failure`. Verify by inspection.

---

#### Cycle 4.5 — Instrument connection: query counters and duration

- RED: Add test `query_increments_counter_and_duration`:
  ```rust
  // Authenticate, then send a GQL MATCH query.
  // After QueryResult response:
  assert_eq!(registry.queries_gql_read.load(Ordering::Relaxed), 1);
  assert_eq!(registry.query_duration_count.load(Ordering::Relaxed), 1);
  let sum = f64::from_bits(registry.query_duration_sum.load(Ordering::Relaxed));
  assert!(sum > 0.0);
  ```
  Add test `query_error_increments_error_counter`:
  ```rust
  // Send a malformed GQL query that triggers QueryError response.
  assert_eq!(registry.query_errors.load(Ordering::Relaxed), 1);
  ```
  Add test `mutation_increments_correct_counter`:
  ```rust
  // Send a GQL INSERT / CREATE mutation.
  assert_eq!(registry.queries_gql_mutation.load(Ordering::Relaxed), 1);
  assert_eq!(registry.queries_gql_read.load(Ordering::Relaxed), 0);
  ```

- GREEN: In `connection.rs` `handle_query`:
  - At the start: `let _query_start = std::time::Instant::now();`
  - After `tessera_cypher::parse_with_mode` returns `Err`: increment `query_errors`,
    call `record_query_duration` with elapsed, return.
  - After determining `GqlStatement::Query` or `GqlStatement::Mutation`, increment the
    correct counter based on `lang` (gql/cypher) and statement variant.
  - When the graph execution returns `Err`: increment `query_errors`.
  - At the end of `handle_query` (before returning `Ok`): call
    `self.ctx.metrics().record_query_duration(_query_start.elapsed().as_secs_f64())`.

- REFACTOR: Ensure `record_query_duration` is called on ALL exit paths, including parse
  errors and execution errors, so the histogram accurately counts all query attempts.
  Introduce `let start = Instant::now();` at the very top of `handle_query` before any
  early return.

---

### Phase 5: Wiring and Startup

Goal: start the metrics HTTP server in `main.rs` when `TESSERA_METRICS_BIND` is set.
No new TDD cycle needed — verify with a throughput regression test.

---

#### Task 5.1 — Wire metrics server in main.rs

- File: `crates/tessera-server/src/main.rs`
- Action: Modify
- Changes:
  1. Read `TESSERA_METRICS_BIND` env var (no default — if absent, no server starts).
  2. Create `Arc<MetricsRegistry>` with `max_connections` as the argument.
  3. Pass registry to `ServerContext::new`.
  4. If `TESSERA_METRICS_BIND` is set, spawn a detached Tokio task calling
     `tessera_monitor::serve_metrics(&addr, Arc::clone(&registry)).await`.
     Log the address with `tracing::info!`.
  5. No change to the TLS server startup path.
- Output: `main.rs` compiles, metrics server starts when env var is present.

---

### Phase 6: Throughput Regression Guard

Because `record_query_duration` is called on every query, verify it does not degrade
the existing ping-pong throughput. Additionally add a direct atomic throughput test.

Test file: `crates/tessera-monitor/tests/throughput_test.rs`

---

#### Cycle 6.1 — Atomic counter throughput guard

- RED: Create `crates/tessera-monitor/tests/throughput_test.rs`.
  Write test `counter_increment_throughput_guard`:
  ```rust
  let r = Arc::new(MetricsRegistry::new(256));
  let n: u64 = 1_000_000;
  let start = std::time::Instant::now();
  for _ in 0..n {
      r.queries_gql_read.fetch_add(1, Ordering::Relaxed);
  }
  let elapsed = start.elapsed();
  #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation,
          clippy::cast_sign_loss)]
  let ops_per_sec = (n as f64 / elapsed.as_secs_f64()) as u64;
  let min_ops: u64 = if cfg!(debug_assertions) { 5_000_000 } else { 50_000_000 };
  assert!(
      ops_per_sec >= min_ops,
      "counter throughput regression: {ops_per_sec} ops/s < {min_ops}"
  );
  ```
  Write test `histogram_record_throughput_guard`:
  ```rust
  let r = Arc::new(MetricsRegistry::new(256));
  let n: u64 = 100_000;
  let start = std::time::Instant::now();
  for i in 0..n {
      #[allow(clippy::cast_precision_loss)]
      r.record_query_duration(0.001 * (i % 100) as f64);
  }
  let elapsed = start.elapsed();
  #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation,
          clippy::cast_sign_loss)]
  let ops_per_sec = (n as f64 / elapsed.as_secs_f64()) as u64;
  let min_ops: u64 = if cfg!(debug_assertions) { 500_000 } else { 5_000_000 };
  assert!(
      ops_per_sec >= min_ops,
      "histogram throughput regression: {ops_per_sec} ops/s < {min_ops}"
  );
  ```

- GREEN: Both tests pass immediately if the implementation uses `Relaxed` atomics and a
  tight CAS loop for the sum. No code changes expected. If either fails, the atomics
  are not being used correctly (e.g., wrong ordering causing unnecessary coherence traffic).

- REFACTOR: none.

---

#### Cycle 6.2 — End-to-end server ping-pong regression guard (existing test must pass)

- RED/GREEN: The existing test `ping_pong_throughput_guard` in
  `crates/tessera-server/tests/throughput_test.rs` must continue to pass after all
  instrumentation is in place. Run it as a regression check.
  - Baseline (pre-instrumentation): already established at 2,000 rps (debug) / 20,000 rps
    (release) in the existing test.
  - If the test fails after Phase 4 changes, the instrumentation has introduced lock
    contention or async overhead — diagnose before proceeding.
  - Threshold: no regression greater than 10% is acceptable. The current thresholds
    already have headroom; if the test still passes with the same numbers, no change needed.

---

## File Summary

### New Files
- `crates/tessera-monitor/src/registry.rs` — `MetricsRegistry`, `HISTOGRAM_BUCKETS`, `record_query_duration`
- `crates/tessera-monitor/src/render.rs` — `render_prometheus`
- `crates/tessera-monitor/src/server.rs` — `serve_metrics`, `serve_metrics_on`
- `crates/tessera-monitor/tests/registry_test.rs`
- `crates/tessera-monitor/tests/render_test.rs`
- `crates/tessera-monitor/tests/http_server_test.rs`
- `crates/tessera-monitor/tests/throughput_test.rs`
- `crates/tessera-server/tests/metrics_integration_test.rs`

### Modified Files
- `crates/tessera-monitor/src/lib.rs` — export all new modules and public items
- `crates/tessera-monitor/Cargo.toml` — add `tokio` dependency
- `crates/tessera-server/src/context.rs` — add `Arc<MetricsRegistry>` field + accessor
- `crates/tessera-server/src/listener.rs` — accepted/rejected/active/tls_failures counters
- `crates/tessera-server/src/connection.rs` — auth + query + duration instrumentation
- `crates/tessera-server/src/main.rs` — registry construction + optional metrics server
- `crates/tessera-server/tests/common/mod.rs` — `test_context_with_metrics`, `test_context_and_registry`, `spawn_handler_with_context`

---

## Estimation

| Work item                            | Time     |
|--------------------------------------|----------|
| Phase 1 — MetricsRegistry            | 45 min   |
| Phase 2 — Prometheus render          | 45 min   |
| Phase 3 — HTTP server                | 45 min   |
| Phase 4 — Server instrumentation     | 60 min   |
| Phase 5 — Wiring (main.rs)           | 20 min   |
| Phase 6 — Throughput guards          | 20 min   |
| **Total**                            | **~4 h** |

---

## Criteria for Acceptance

- [ ] All new tests pass: `cargo test -p tessera-monitor && cargo test -p tessera-server`
- [ ] Zero Clippy errors or warnings: `cargo clippy --all-targets -- -D warnings`
- [ ] All existing `tessera-server` tests still pass (regression check)
- [ ] `ping_pong_throughput_guard` passes with the same thresholds as before instrumentation
- [ ] `counter_increment_throughput_guard`: >= 5M ops/s (debug) / 50M ops/s (release)
- [ ] `histogram_record_throughput_guard`: >= 500K ops/s (debug) / 5M ops/s (release)
- [ ] `GET /metrics` returns valid Prometheus text with `Content-Type: text/plain; version=0.0.4`
- [ ] `TESSERA_METRICS_BIND` absent → no metrics server starts (no port bound)
- [ ] No `unsafe` code anywhere in `tessera-monitor`
- [ ] Copyright header present in every new `.rs` file
