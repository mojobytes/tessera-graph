# TDD Plan: Phase 3 TCP Protocol Quality Fixes

**Created**: 2026-03-20
**Branch**: `feature/phase3-quality-fixes` (from `develop`)
**Scope**: 7 critical findings + 4 recommended improvements from the Phase 3 quality review

---

## Context

The Phase 3 TCP Protocol implementation is functionally correct but has seven security and
correctness defects that must be resolved before merge, plus four high-impact code-quality issues.
The defects range from a silently broken security guarantee (TLS not applied), a race condition in
connection counting, to an auth-error info leak that exposes internal password validation messages
to clients.

**Stack detected**: Rust 2024, Tokio async runtime, `tokio-rustls` for TLS, `thiserror` for errors,
`serde_json` for wire encoding, `rcgen`/`tempfile` in dev-dependencies.

**Conventions observed**:
- Tests live in `crates/<crate>/tests/<name>_test.rs`
- Copyright header: `// Copyright 2026 BelowZero Security OU. All rights reserved.`
- All `clippy::all` lints are `deny`; `pedantic` and `nursery` are `warn`
- Error types use `thiserror`
- Warnings are treated as errors in CI

**Affected hot paths**: `serve()` accept loop (connection throughput), `run()` message dispatch loop
(ping-pong throughput). Both are guarded by `ping_pong_throughput_guard` in
`crates/tessera-server/tests/throughput_test.rs`.

**Does not affect hot path** (no new async I/O on fast path): findings 2 (Semaphore replaces
AtomicUsize — equivalent overhead), 6 (scoping block, no spawn_blocking on read path), 8 (Debug
redaction), 9 (encode validation), 10 (test helpers), 11 (logging macros).

---

## Decisions Resolved Before Planning

All architectural decisions are resolved:
- TLS wrapping uses `tokio_rustls::TlsAcceptor` (dependency already in `Cargo.toml`)
- Connection limit uses `tokio::sync::Semaphore` (idiomatic tokio, no new dependency)
- `ServerMessage::ProtocolError` variant is added (fix 4) rather than a generic string
- Auth errors always return the constant string `"authentication failed"`
- Graceful drain uses `tokio::task::JoinSet` with a configurable drain timeout
- `encode()` returns `Result<Vec<u8>, ProtocolError>` (breaking change within the crate, callers updated)
- `std::sync::RwLock` is kept (not replaced with `tokio::sync::RwLock`) — the fix is a visible scoping block with a safety comment, which is accurate and non-invasive

---

## Execution Plan

### Phase 0: Extract shared test helpers (Finding 10 — prerequisite to all new tests)

Doing this first means every subsequent TDD cycle writes tests once, in the right place.

#### 0.1 — Create `crates/tessera-server/tests/common/mod.rs` (20 min)

- **File**: `crates/tessera-server/tests/common/mod.rs` — Create
- **Action**: Extract the three duplicated helpers (`test_tls_config`, `test_context`,
  `spawn_handler`) that exist identically in `connection_handler_test.rs`,
  `listener_test.rs`, and `throughput_test.rs`.
- **Output**: One canonical module with:
  - `pub fn test_tls_config() -> tessera_protocol::TlsConfig`
  - `pub fn test_context() -> Arc<ServerContext>`
  - `pub fn spawn_handler(ctx, graph) -> (FramedWriter, FramedReader, Sender<bool>)` — the
    duplex-stream variant used by connection handler tests
  - Each function carries the copyright header; `#[allow(dead_code)]` where Rust warns on
    re-export before all consumers are updated.

**Verification**: `cargo check --tests -p tessera-server` passes with no warnings.

#### 0.2 — Update all four test files to use `common` (25 min)

- **Files**: `connection_handler_test.rs`, `listener_test.rs`, `throughput_test.rs`,
  `auth_integration_test.rs` — Modify
- **Action**: Replace the local `test_tls_config` and `test_context` definitions with
  `mod common; use common::{test_context, test_tls_config};` at the top of each file.
  `spawn_handler` is only used in `connection_handler_test.rs` — remove local definition there.
  `auth_integration_test.rs` has a slightly different `test_context` signature (returns value, not
  `Arc`) — adapt it to call `common::test_context()` and clone or wrap as needed.
- **Output**: Zero duplicated helper code across test files.
- **Verification**: `cargo test -p tessera-server` — all pre-existing tests pass, zero new
  warnings.

---

### Phase 1: Protocol error variant (Finding 4 — unblocks Finding 3 tests)

Finding 3 (auth info leak) requires knowing what message to assert on for malformed JSON. Adding
`ProtocolError` variant first makes that assertion precise.

#### 1.1 RED — Test that malformed JSON returns `ProtocolError`, not `AuthError` (15 min)

- **File**: `crates/tessera-server/tests/connection_handler_test.rs` — Modify
- **Test name**: `connection_handler_malformed_frame_returns_protocol_error`
- **Arrange**: `spawn_handler(ctx, graph)` via common helper.
- **Act**: Write raw bytes that are valid frame length but invalid JSON:
  ```rust
  let bad_json = b"{ this is not json }";
  let frame = tessera_protocol::frame::encode(bad_json);
  // write raw bytes directly to the writer's underlying stream
  writer.write_frame(bad_json).await.unwrap();
  ```
- **Assert**:
  ```rust
  assert!(
      matches!(response, ServerMessage::ProtocolError { .. }),
      "expected ProtocolError for malformed JSON, got {response:?}"
  );
  ```
- **Expected result**: Fails to compile (variant does not exist yet) — RED.

#### 1.2 GREEN — Add `ServerMessage::ProtocolError` variant (20 min)

- **File**: `crates/tessera-protocol/src/message.rs` — Modify
- **Action**: Add variant to `ServerMessage`:
  ```rust
  /// A protocol-level error (malformed frame, unknown message type, etc.).
  /// Not an authentication failure.
  ProtocolError { reason: String },
  ```
- **File**: `crates/tessera-server/src/connection.rs` — Modify
- **Action**: In `run()`, replace the `ServerMessage::AuthError` in the JSON parse error arm
  (lines 93–99) with:
  ```rust
  Err(_e) => {
      self.send_message(&ServerMessage::ProtocolError {
          reason: "invalid message format".into(),
      })
      .await?;
      continue;
  }
  ```
  Note: the reason string is fixed — no internal detail leaked here.
- **Verification**: `cargo test -p tessera-server connection_handler_malformed_frame_returns_protocol_error` — GREEN.

#### 1.3 REFACTOR — Confirm no existing test asserts `AuthError` for parse failure (10 min)

Search all test files for assertions on `AuthError` where the trigger is bad JSON. There are none
in the current test suite (the parse-error path was untested). Confirm with:
```bash
cargo test -p tessera-server
cargo test -p tessera-protocol
```
All pass.

---

### Phase 2: Auth error info leak (Finding 3)

#### 2.1 RED — Test that wrong-password response leaks no internal detail (15 min)

- **File**: `crates/tessera-server/tests/connection_handler_test.rs` — Modify
- **Test name**: `connection_handler_auth_error_reason_is_generic`
- **Arrange**: `spawn_handler`, send `ClientMessage::Login` with wrong password.
- **Assert**:
  ```rust
  match response {
      ServerMessage::AuthError { ref reason } => {
          assert_eq!(
              reason, "authentication failed",
              "auth error must not leak internal detail, got: {reason:?}"
          );
      }
      other => panic!("expected AuthError, got {other:?}"),
  }
  ```
- **Expected result**: FAILS — current code leaks `e.to_string()` which would be something like
  "invalid credentials" — RED.

#### 2.2 RED — Test that short-password response leaks no internal detail (10 min)

- **Test name**: `connection_handler_short_password_auth_error_is_generic`
- **Arrange**: Send `ClientMessage::Login` with `password: "x"` (fails `Password::new` validation).
- **Assert**: Same assertion — reason is exactly `"authentication failed"`.
- **Expected result**: FAILS — current code leaks `Password` validation error string — RED.

#### 2.3 GREEN — Replace all `e.to_string()` in `handle_login` with constant (20 min)

- **File**: `crates/tessera-server/src/connection.rs` — Modify
- **Action**: Define a module-level constant at the top of the file:
  ```rust
  /// Generic message returned for all authentication failures.
  /// Must never include internal details.
  const AUTH_FAILURE_MSG: &str = "authentication failed";
  ```
  Replace every `reason: e.to_string()` inside `handle_login` (three sites: `Password::new` error,
  `authenticate` error, `create_session` error) with `reason: AUTH_FAILURE_MSG.into()`.
  Internal errors are logged to the audit log with full detail:
  ```rust
  Err(e) => {
      let _ = self.ctx.audit().record_error(
          None,
          "login",
          None,
          &format!("auth failure for user {username:?}: {e}"),
      );
      self.send_message(&ServerMessage::AuthError {
          reason: AUTH_FAILURE_MSG.into(),
      })
      .await?;
  }
  ```
  For `Password::new` failure and `create_session` failure apply the same pattern.
- **Verification**: Both RED tests pass. Existing `connection_handler_wrong_password_returns_auth_error` still passes (it only asserts `AuthError`, not the reason string).

#### 2.4 REFACTOR — Deduplicate the three error arms into a helper (15 min)

- **File**: `crates/tessera-server/src/connection.rs` — Modify
- **Action**: Extract a private async helper:
  ```rust
  async fn send_auth_failure(&mut self, audit_detail: &str) -> Result<()> {
      let _ = self.ctx.audit().record_error(None, "login", None, audit_detail);
      self.send_message(&ServerMessage::AuthError {
          reason: AUTH_FAILURE_MSG.into(),
      })
      .await
  }
  ```
  Use it in the three arms of `handle_login`.
- **Verification**: `cargo test -p tessera-server` — all pass; `cargo clippy -p tessera-server -- -D warnings` — clean.

---

### Phase 3: Race condition in connection counting (Finding 2)

#### 3.1 RED — Test that exactly `max_connections` connections are accepted under concurrent load (20 min)

- **File**: `crates/tessera-server/tests/listener_test.rs` — Modify
- **Test name**: `connection_limit_is_never_exceeded_under_concurrent_connect`
- **Arrange**: `max_connections = 2`; spawn 10 concurrent tasks all connecting simultaneously and
  sending Ping.
- **Act**: Count how many receive `Pong` vs `capacity` error.
- **Assert**:
  ```rust
  let accepted = pong_count.load(Ordering::SeqCst);
  let rejected = capacity_error_count.load(Ordering::SeqCst);
  assert!(
      accepted <= 2,
      "race condition: {accepted} connections accepted, limit is 2"
  );
  assert_eq!(accepted + rejected, 10, "all connections must receive a response");
  ```
- **Expected result**: Flaky or FAILS — the `load → compare → fetch_add` race means `accepted` can
  exceed 2 — RED.

#### 3.2 GREEN — Replace `AtomicUsize` with `Arc<Semaphore>` in `serve()` (25 min)

- **File**: `crates/tessera-server/src/listener.rs` — Modify
- **Action**:
  1. Remove `use std::sync::atomic::{AtomicUsize, Ordering};`
  2. Add `use tokio::sync::Semaphore;`
  3. Replace `let active = Arc::new(AtomicUsize::new(0));` with:
     ```rust
     let semaphore = Arc::new(Semaphore::new(max_connections));
     ```
  4. Remove the `current >= max_connections` block entirely.
  5. Before spawning, try to acquire a permit:
     ```rust
     let permit = match Arc::clone(&semaphore).try_acquire_owned() {
         Ok(p) => p,
         Err(_) => {
             // At capacity — inform client and close
             let (_, write_half) = tokio::io::split(stream);
             let mut writer = FramedWriter::new(write_half);
             let msg = ServerMessage::CapacityError {
                 reason: "server at capacity".into(),
             };
             match serde_json::to_vec(&msg) {
                 Ok(json) => { let _ = writer.write_frame(&json).await; }
                 Err(_) => {} // serialization failure: just close
             }
             continue;
         }
     };
     ```
  6. Move the permit into the spawned task so it is dropped when the handler finishes:
     ```rust
     tokio::spawn(async move {
         let _permit = permit; // holds the semaphore slot for the lifetime of this task
         let mut handler = ConnectionHandler::new(stream, ctx, graph, idle_timeout, shutdown_rx);
         let _ = handler.run().await;
     });
     ```
  7. Remove `active.fetch_sub(1, ...)` — permit drop is automatic.

  Note: The existing test `server_rejects_connection_when_at_capacity` uses
  `ServerMessage::AuthError { reason }` to match the capacity response. That test must be updated
  in step 3.3 to match the new `CapacityError` variant.

- **File**: `crates/tessera-protocol/src/message.rs` — Modify
- **Action**: Add `CapacityError { reason: String }` variant to `ServerMessage` (semantically more
  accurate than `AuthError` for a connection-count limit).
- **Verification**: `cargo test -p tessera-server connection_limit_is_never_exceeded_under_concurrent_connect` — GREEN.

#### 3.3 REFACTOR — Update assertions in existing capacity test (10 min)

- **File**: `crates/tessera-server/tests/listener_test.rs` — Modify
- **Test**: `server_rejects_connection_when_at_capacity`
- **Action**: Change the match from `ServerMessage::AuthError { ref reason }` to
  `ServerMessage::CapacityError { ref reason }`. Reason string assertion (`"capacity"`) stays the same.
- **Verification**: `cargo test -p tessera-server` — all pass.

---

### Phase 4: unwrap_or_default on capacity serialization (Finding 5)

This is resolved as part of Phase 3 step 3.2: the `match serde_json::to_vec(&msg)` block handles
serialization failure by doing nothing (close the socket), rather than sending an empty frame.
No separate TDD cycle required — the fix is structural. Confirm the `unwrap_or_default` is gone:

```bash
grep -n "unwrap_or_default" crates/tessera-server/src/listener.rs
# must return zero results
```

---

### Phase 5: Graceful drain on shutdown (Finding 7)

#### 5.1 RED — Test that active connections receive `Bye` before server exits (20 min)

- **File**: `crates/tessera-server/tests/listener_test.rs` — Modify
- **Test name**: `graceful_shutdown_drains_active_connections`
- **Arrange**: Start server with `max_connections = 5`, idle timeout of 30 s.
  Connect 3 clients, verify each gets `Pong` to a Ping (confirming they are active handlers).
- **Act**: Send shutdown signal.
- **Assert**:
  ```rust
  for mut reader in client_readers {
      // Each active connection should receive Bye before EOF
      let frame = tokio::time::timeout(
          Duration::from_secs(3),
          reader.read_frame(),
      )
      .await
      .expect("timed out waiting for Bye")
      .unwrap()
      .expect("expected Bye frame");
      let msg: ServerMessage = serde_json::from_slice(&frame).unwrap();
      assert_eq!(msg, ServerMessage::Bye, "expected Bye on shutdown");
  }
  ```
- **Expected result**: FAILS or times out — current implementation orphans tasks — RED.

#### 5.2 GREEN — Introduce `JoinSet` for task tracking and drain on shutdown (30 min)

- **File**: `crates/tessera-server/src/listener.rs` — Modify
- **Action**:
  1. Add `use tokio::task::JoinSet;`
  2. Create `let mut tasks: JoinSet<()> = JoinSet::new();` before the loop.
  3. Prune completed tasks inside the loop to avoid unbounded growth:
     ```rust
     // Reap completed tasks (non-blocking)
     while tasks.try_join_next().is_some() {}
     ```
  4. Replace `tokio::spawn(async move { ... })` with `tasks.spawn(async move { ... })`.
  5. On shutdown signal, replace the immediate `return Ok(())` with a drain:
     ```rust
     _ = shutdown.changed() => {
         if *shutdown.borrow() {
             // Stop accepting; drain in-flight handlers with a timeout.
             let drain_timeout = Duration::from_secs(30);
             let _ = tokio::time::timeout(drain_timeout, async {
                 while tasks.join_next().await.is_some() {}
             })
             .await;
             return Ok(());
         }
         continue;
     }
     ```
  6. Add `use std::time::Duration;` import if not already present (it is, via `idle_timeout`).
- **Verification**: `cargo test -p tessera-server graceful_shutdown_drains_active_connections` — GREEN.
  Existing `graceful_shutdown_stops_accept_loop` also passes.

#### 5.3 REFACTOR — Verify drain timeout is configurable (optional follow-up note)

The drain timeout is hard-coded at 30 s. This is acceptable for now; a future task can expose it
as a `ServerConfig` field. Add a `// TODO(future): expose drain_timeout in ServerConfig` comment.

---

### Phase 6: TLS applied in accept loop (Finding 1 — most impactful security fix)

#### 6.1 RED — Test that connection without TLS handshake is rejected by server (20 min)

- **File**: `crates/tessera-server/tests/listener_test.rs` — Modify
- **Test name**: `serve_requires_tls_plain_tcp_connection_gets_no_response`
- **Arrange**: Start server with `serve_tls()` (see step 6.2).
- **Act**: Connect a plain `TcpStream` (no TLS), write a valid framed Ping.
- **Assert**: Reading from the stream returns an error or EOF — the server drops the connection
  during TLS handshake, never dispatching to `ConnectionHandler`:
  ```rust
  let result = tokio::time::timeout(
      Duration::from_secs(2),
      reader.read_frame(),
  )
  .await;
  // Either timeout (server ignored it) or an I/O error (TLS alert sent)
  // but NOT a valid ServerMessage::Pong
  match result {
      Ok(Ok(Some(frame))) => {
          // If we got a frame, it must NOT be Pong
          let msg: ServerMessage = serde_json::from_slice(&frame).unwrap();
          assert_ne!(msg, ServerMessage::Pong, "plain TCP must not get a Pong");
      }
      _ => {} // timeout or error is the expected path
  }
  ```
- **Expected result**: FAILS — current `serve()` dispatches plain TCP connections to handlers — RED.

#### 6.2 GREEN — Add `serve_tls()` method to `TesseraListener` (35 min)

The current `serve()` is kept for testing `ConnectionHandler` in isolation (via duplex streams).
A new method `serve_tls()` wraps each accepted stream with `TlsAcceptor` before dispatch.

- **File**: `crates/tessera-server/src/listener.rs` — Modify
- **Action**:
  1. Add `use tokio_rustls::TlsAcceptor;`
  2. Add new public method:
     ```rust
     /// Run the accept loop with mandatory TLS wrapping.
     ///
     /// Each accepted `TcpStream` is wrapped with `TlsAcceptor` before being
     /// passed to `ConnectionHandler`. Connections that fail the TLS handshake
     /// are dropped without spawning a handler.
     ///
     /// # Errors
     ///
     /// Returns `ServerError` on unrecoverable listener failure.
     pub async fn serve_tls(
         self,
         ctx: Arc<ServerContext>,
         graph: Arc<RwLock<Graph>>,
         mut shutdown: watch::Receiver<bool>,
         max_connections: usize,
         idle_timeout: Duration,
     ) -> Result<()> {
         let tls_acceptor = TlsAcceptor::from(
             Arc::clone(ctx.tls_config().server_config()),
         );
         let semaphore = Arc::new(tokio::sync::Semaphore::new(max_connections));
         let mut tasks: JoinSet<()> = JoinSet::new();

         loop {
             // Reap completed tasks
             while tasks.try_join_next().is_some() {}

             let stream = tokio::select! {
                 biased;
                 _ = shutdown.changed() => {
                     if *shutdown.borrow() {
                         let drain_timeout = Duration::from_secs(30);
                         let _ = tokio::time::timeout(drain_timeout, async {
                             while tasks.join_next().await.is_some() {}
                         })
                         .await;
                         return Ok(());
                     }
                     continue;
                 }
                 result = self.inner.accept() => {
                     match result {
                         Ok((stream, _addr)) => stream,
                         Err(e) => {
                             tracing::warn!("accept error: {e}");
                             continue;
                         }
                     }
                 }
             };

             let permit = match Arc::clone(&semaphore).try_acquire_owned() {
                 Ok(p) => p,
                 Err(_) => {
                     let (_, write_half) = tokio::io::split(stream);
                     let mut writer = FramedWriter::new(write_half);
                     let msg = ServerMessage::CapacityError {
                         reason: "server at capacity".into(),
                     };
                     match serde_json::to_vec(&msg) {
                         Ok(json) => { let _ = writer.write_frame(&json).await; }
                         Err(_) => {}
                     }
                     continue;
                 }
             };

             let tls_acceptor = tls_acceptor.clone();
             let ctx = Arc::clone(&ctx);
             let graph = Arc::clone(&graph);
             let shutdown_rx = shutdown.clone();

             tasks.spawn(async move {
                 let _permit = permit;
                 match tls_acceptor.accept(stream).await {
                     Ok(tls_stream) => {
                         let mut handler = ConnectionHandler::new(
                             tls_stream, ctx, graph, idle_timeout, shutdown_rx,
                         );
                         let _ = handler.run().await;
                     }
                     Err(e) => {
                         tracing::warn!("TLS handshake failed: {e}");
                         // Connection dropped — no response sent
                     }
                 }
             });
         }
     }
     ```
  3. Note: `tokio_rustls::server::TlsStream<TcpStream>` implements `AsyncRead + AsyncWrite + Unpin`,
     satisfying `ConnectionHandler<S>`.

- **File**: `crates/tessera-server/src/main.rs` — Modify
- **Action**: Replace `listener.serve(...)` with `listener.serve_tls(...)`. Remove the `eprintln!`
  calls (addressed in Phase 9).

- **Verification**: `cargo test -p tessera-server serve_requires_tls_plain_tcp_connection_gets_no_response` — GREEN.

#### 6.3 REFACTOR — Update existing listener tests that used plain `serve()` (20 min)

Tests that rely on plain TCP (`listener_handles_two_concurrent_connections`,
`server_rejects_connection_when_at_capacity`, `graceful_shutdown_stops_accept_loop`,
`idle_connection_is_closed_after_timeout`) use `serve()` without TLS because they connect with
plain `TcpStream`. These tests cover the core logic (connection counting, shutdown, idle timeout)
and remain valid using plain `serve()`.

Add a doc comment to `serve()`: `/// Plain TCP accept loop — for testing only. Production code must use [`serve_tls`].`

Rename `throughput_test.rs`'s `ping_pong_throughput_guard` to use plain `serve()` (it currently
does, since it was always testing plain TCP performance). Add a comment noting that TLS throughput
is a separate benchmark concern.

Add a new TLS-aware integration test in `listener_test.rs`:
- **Test name**: `serve_tls_round_trip_ping_pong`
- Connects a `tokio_rustls::TlsConnector` client using the test certificate, sends Ping, asserts
  Pong. This is the end-to-end smoke test for the full TLS path. (Requires adding
  `tokio-rustls` to dev-dependencies in `Cargo.toml`.)

```toml
# crates/tessera-server/Cargo.toml [dev-dependencies]
tokio-rustls = "0.26"
```

- **Verification**: `cargo test -p tessera-server` — all tests pass.

---

### Phase 7: std::sync::RwLock in async context (Finding 6)

No logic change is required — the lock IS dropped before any `.await`. The fix is a visible
scoping block that makes this invariant obvious and guarded by a safety comment.

#### 7.1 RED — Lint guard test (15 min)

This finding has no behavioral test — the bug is about code fragility. The "test" is a Clippy
check. Add to CI configuration (documented here as a manual step):

```bash
cargo clippy -p tessera-server -- -D clippy::await_holding_lock
```

This will currently report no error (the lock IS dropped before `.await`), confirming the code is
safe. The fix in 7.2 makes the invariant structurally explicit.

#### 7.2 GREEN — Add explicit scoping blocks with safety comments (20 min)

- **File**: `crates/tessera-server/src/connection.rs` — Modify
- **Action**: In `handle_query`, wrap each lock acquisition in an explicit block that makes the
  drop-before-await invariant visible:
  ```rust
  // SAFETY: std::sync::RwLock is held only within this synchronous block.
  // The guard is dropped at the closing `}`, before any `.await` point.
  // If this invariant is violated in the future, `clippy::await_holding_lock` will catch it.
  let response = match stmt {
      GqlStatement::Query(ref q) => {
          let result = {
              let graph = self.graph.read().map_err(|_| {
                  ServerError::Auth(tessera_auth::AuthError::LockPoisoned("graph"))
              })?;
              tessera_graph::gql::execute(&graph, q)
                  .map(|rows| gql_result_to_json(&rows))
          };
          match result {
              Ok((columns, json_rows)) => ServerMessage::QueryResult { columns, rows: json_rows },
              Err(e) => ServerMessage::QueryError { reason: e.to_string() },
          }
      }
      GqlStatement::Mutation(ref m) => {
          let result = {
              let mut graph = self.graph.write().map_err(|_| {
                  ServerError::Auth(tessera_auth::AuthError::LockPoisoned("graph"))
              })?;
              tessera_storage_enterprise::gql::execute_mut(&mut graph, m)
          };
          match result { ... }
      }
  };
  ```
- **Verification**: `cargo clippy -p tessera-server -- -D clippy::await_holding_lock` — clean.
  `cargo test -p tessera-server` — all pass.

---

### Phase 8: Password visible in Debug (Finding 8)

#### 8.1 RED — Test that `Debug` output of `ClientMessage::Login` does not contain the password (15 min)

- **File**: `crates/tessera-protocol/tests/` — create `message_test.rs`
- **Test name**: `client_message_login_debug_redacts_password`
- **Action**:
  ```rust
  let msg = ClientMessage::Login {
      username: "admin".into(),
      password: "super-secret-123".into(),
  };
  let debug = format!("{msg:?}");
  assert!(
      !debug.contains("super-secret-123"),
      "Debug output must not contain the password: {debug}"
  );
  assert!(
      debug.contains("[REDACTED]") || debug.contains("***"),
      "Debug output must show a redaction marker: {debug}"
  );
  ```
- **Expected result**: FAILS — current `#[derive(Debug)]` exposes the field — RED.

#### 8.2 GREEN — Manual `Debug` impl for `ClientMessage` (20 min)

- **File**: `crates/tessera-protocol/src/message.rs` — Modify
- **Action**: Remove `Debug` from the derive list on `ClientMessage`. Add a manual impl:
  ```rust
  impl std::fmt::Debug for ClientMessage {
      fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
          match self {
              Self::Login { username, .. } => f
                  .debug_struct("Login")
                  .field("username", username)
                  .field("password", &"[REDACTED]")
                  .finish(),
              Self::Query { query, language } => f
                  .debug_struct("Query")
                  .field("query", query)
                  .field("language", language)
                  .finish(),
              Self::Logout => write!(f, "Logout"),
              Self::Ping => write!(f, "Ping"),
          }
      }
  }
  ```
- **Verification**: `cargo test -p tessera-protocol client_message_login_debug_redacts_password` — GREEN.

#### 8.3 REFACTOR — Add tests for the non-sensitive variants (10 min)

- **Tests**: `client_message_query_debug_contains_query_text`, `client_message_ping_debug`
- Confirm the non-password variants still show their fields (regression guard on the manual impl).

---

### Phase 9: `encode()` silent truncation (Finding 9)

#### 9.1 RED — Test that `encode()` returns `Err` for payload > `u32::MAX` (15 min)

- **File**: `crates/tessera-protocol/tests/frame_test.rs` (create if absent, or add to existing)
- **Test name**: `encode_returns_error_for_oversized_payload`
- **Action**:
  ```rust
  // We cannot allocate a 4 GiB buffer in a test; test the validation logic
  // by constructing a payload slice reference of the right length via a mock.
  // Use a zero-copy approach: validate the length-check branch only.
  // Since u32::MAX is 4 GiB, test at the boundary using a computed length check.
  //
  // Instead: test that encode() on a MAX_FRAME_SIZE+1 slice returns Err.
  let oversized = vec![0u8; crate::frame::MAX_FRAME_SIZE as usize + 1];
  let result = tessera_protocol::frame::encode(&oversized);
  assert!(result.is_err(), "encode must return Err for payload > MAX_FRAME_SIZE");
  ```
- **Expected result**: Fails to compile — `encode` currently returns `Vec<u8>` not `Result` — RED.

#### 9.2 GREEN — Change `encode()` signature to `Result<Vec<u8>, ProtocolError>` (25 min)

- **File**: `crates/tessera-protocol/src/frame.rs` — Modify
- **Action**:
  1. Change signature:
     ```rust
     /// Encode a payload into a length-prefixed frame.
     ///
     /// # Errors
     ///
     /// Returns `ProtocolError::FrameTooLarge` if `payload.len()` exceeds [`MAX_FRAME_SIZE`].
     pub fn encode(payload: &[u8]) -> Result<Vec<u8>> {
         let len = u32::try_from(payload.len())
             .map_err(|_| ProtocolError::FrameTooLarge { declared: u32::MAX })?;
         if len > MAX_FRAME_SIZE {
             return Err(ProtocolError::FrameTooLarge { declared: len });
         }
         let mut buf = Vec::with_capacity(HEADER_SIZE + payload.len());
         buf.extend_from_slice(&len.to_be_bytes());
         buf.extend_from_slice(payload);
         Ok(buf)
     }
     ```
  2. Remove the `#[allow(clippy::cast_possible_truncation)]` attribute — no longer needed.
  3. Remove `#[must_use]` from the function (it returns `Result` — callers must handle it).

- **File**: `crates/tessera-protocol/src/frame.rs` — Modify `FramedWriter::write_frame`
- **Action**: Propagate the `Result` from `encode`:
  ```rust
  pub async fn write_frame(&mut self, payload: &[u8]) -> Result<()> {
      let frame = encode(payload)?;
      self.writer.write_all(&frame).await?;
      self.writer.flush().await?;
      Ok(())
  }
  ```
  `encode` error (`FrameTooLarge`) is already a `ProtocolError` variant, so `?` works directly.

- **File**: `crates/tessera-server/src/connection.rs` — `send_message` already uses
  `FramedWriter::write_frame` via `?`; no changes needed.

- **Verification**: `cargo test -p tessera-protocol` — GREEN.
  `cargo build -p tessera-server` — compiles cleanly (no call sites to `encode` directly outside the protocol crate).

#### 9.3 REFACTOR — Add boundary tests for `encode` (10 min)

- `encode_accepts_max_frame_size_payload`: payload of exactly `MAX_FRAME_SIZE` bytes succeeds.
- `encode_empty_payload`: zero-length payload produces a 4-byte frame with all-zero length prefix.

---

### Phase 10: Replace `eprintln!` with tracing (Finding 11)

No behavioral test is possible for logging macros in unit tests. The "test" is a Clippy lint and a
code search.

#### 10.1 Audit all `eprintln!` sites (5 min)

```bash
grep -rn "eprintln!" crates/tessera-server/src/
```

Expected sites:
- `listener.rs:90` — accept error
- `main.rs:78` — shutdown message
- `main.rs:87` — listening address
- `main.rs:99` — server error

#### 10.2 Replace with tracing macros (20 min)

- **File**: `crates/tessera-server/src/listener.rs` — Modify
  - `eprintln!("accept error: {e}")` → `tracing::warn!("accept error: {e}")`
  - (`serve_tls` already uses `tracing::warn!` as written in Phase 6 steps)

- **File**: `crates/tessera-server/src/main.rs` — Modify
  - Add `use tracing::{error, info, warn};` (or use fully qualified paths)
  - `eprintln!("\nShutting down...")` → `info!("shutting down")`
  - `eprintln!("TesseraGraph listening on {addr} (TLS)")` → `info!("TesseraGraph listening on {addr} (TLS)")`
  - `eprintln!("Server error: {e}")` → `error!("server error: {e}")`
  - Add a tracing subscriber initialization at the top of `main()`:
    ```rust
    tracing_subscriber::fmt::init();
    ```
    This requires adding `tracing-subscriber` to `[dependencies]` in `Cargo.toml`:
    ```toml
    tracing-subscriber = { version = "0.3", features = ["env-filter"] }
    ```

#### 10.3 Verification (5 min)

```bash
grep -n "eprintln!" crates/tessera-server/src/
# must return zero results
cargo build -p tessera-server
cargo clippy -p tessera-server -- -D warnings
```

---

### Phase 11: Wiring Verification Cycle

This final cycle validates the full integration of all fixes together. No new code is written —
only compilation and test suite execution.

#### 11.1 Full compile check with warnings as errors (10 min)

```bash
RUSTFLAGS="-D warnings" cargo build --workspace
```

Expected: zero errors, zero warnings.

#### 11.2 Full test suite (15 min)

```bash
cargo test --workspace
```

Expected: all tests pass, including:
- `connection_handler_malformed_frame_returns_protocol_error`
- `connection_handler_auth_error_reason_is_generic`
- `connection_handler_short_password_auth_error_is_generic`
- `connection_limit_is_never_exceeded_under_concurrent_connect`
- `graceful_shutdown_drains_active_connections`
- `serve_requires_tls_plain_tcp_connection_gets_no_response`
- `serve_tls_round_trip_ping_pong`
- `client_message_login_debug_redacts_password`
- `encode_returns_error_for_oversized_payload`
- All pre-existing tests unchanged

#### 11.3 Throughput regression guard (10 min)

```bash
cargo test -p tessera-server ping_pong_throughput_guard -- --nocapture
```

Expected: pass at the same thresholds as before (2 000 rps debug, 20 000 rps release).
The Semaphore and JoinSet additions are off the hot path — `try_acquire_owned` is O(1) and
`try_join_next` is O(1).

#### 11.4 Clippy full audit (10 min)

```bash
cargo clippy --workspace -- -D warnings -D clippy::await_holding_lock
```

Expected: zero warnings, zero errors.

#### 11.5 Confirm `eprintln!` is eliminated (5 min)

```bash
grep -rn "eprintln!" crates/tessera-server/src/
# zero results expected
```

#### 11.6 Confirm `unwrap_or_default` is eliminated from listener (5 min)

```bash
grep -n "unwrap_or_default" crates/tessera-server/src/listener.rs
# zero results expected
```

---

## File Change Summary

| File | Change type |
|------|-------------|
| `crates/tessera-server/tests/common/mod.rs` | Create |
| `crates/tessera-server/tests/connection_handler_test.rs` | Modify (use common, add 3 tests) |
| `crates/tessera-server/tests/listener_test.rs` | Modify (use common, add 3 tests, update 1 assertion) |
| `crates/tessera-server/tests/throughput_test.rs` | Modify (use common) |
| `crates/tessera-server/tests/auth_integration_test.rs` | Modify (use common) |
| `crates/tessera-protocol/tests/message_test.rs` | Create |
| `crates/tessera-protocol/tests/frame_test.rs` | Create (or extend) |
| `crates/tessera-protocol/src/message.rs` | Modify (add 2 variants, manual Debug) |
| `crates/tessera-protocol/src/frame.rs` | Modify (encode returns Result) |
| `crates/tessera-server/src/listener.rs` | Modify (Semaphore, JoinSet, serve_tls, tracing) |
| `crates/tessera-server/src/connection.rs` | Modify (AUTH_FAILURE_MSG, ProtocolError, scoping) |
| `crates/tessera-server/src/main.rs` | Modify (serve_tls, tracing) |
| `crates/tessera-server/Cargo.toml` | Modify (tracing-subscriber in deps, tokio-rustls in dev-deps) |

---

## Estimation

| Phase | Implementation | Testing |
|-------|---------------|---------|
| 0 — Extract test helpers | 20 min | 25 min |
| 1 — ProtocolError variant | 20 min | 15 min |
| 2 — Auth info leak | 35 min | 25 min |
| 3 — Semaphore + CapacityError | 30 min | 20 min |
| 5 — Graceful drain | 30 min | 20 min |
| 6 — TLS in accept loop | 55 min | 40 min |
| 7 — RwLock scoping | 20 min | 15 min |
| 8 — Debug redaction | 20 min | 15 min |
| 9 — encode Result | 25 min | 20 min |
| 10 — tracing macros | 20 min | 10 min |
| 11 — Wiring verification | — | 55 min |
| **Total** | **~4.8 h** | **~3.5 h** |

---

## Criteria of Success

- [ ] All 7 critical findings are resolved with no workarounds
- [ ] All 4 recommended improvements (8–11) are resolved
- [ ] `RUSTFLAGS="-D warnings" cargo build --workspace` passes
- [ ] `cargo test --workspace` passes — all tests green
- [ ] `ping_pong_throughput_guard` passes at existing thresholds
- [ ] `cargo clippy --workspace -- -D warnings -D clippy::await_holding_lock` passes
- [ ] Zero `eprintln!` in `crates/tessera-server/src/`
- [ ] Zero `unwrap_or_default` in `crates/tessera-server/src/listener.rs`
- [ ] Zero duplicated `test_context` / `test_tls_config` definitions across test files
- [ ] `serve_tls()` is the method called from `main.rs` — the `(TLS)` log message is no longer a lie
- [ ] `ClientMessage::Login` Debug output does not contain the literal password string
- [ ] All auth failures (any code path in `handle_login`) return exactly `"authentication failed"` to the client
