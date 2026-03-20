# TDD Plan — Phase 3: TCP Protocol for TesseraGraph Enterprise

## Context

TesseraGraph Enterprise currently has a complete security stack (auth, RBAC, TLS, audit) and a GQL/Cypher
execution engine, but `main.rs` is an empty stub and there is no network transport. Phase 3 wires all of
this together into a real TCP server that external clients can connect to.

**Stack detected**: Rust 2024, Tokio (to be added), serde/serde_json (workspace), thiserror 2 (workspace),
rustls 0.23 (tessera-protocol), tokio-rustls (to be added).

**Convenciones observadas**:
- Error types with `thiserror` in `error.rs`, re-exported as `pub use error::{XyzError, Result}`.
- Tests in `crates/<crate>/tests/<name>_test.rs` as separate integration test files.
- Copyright comment `// Copyright 2026 BelowZero Security OU. All rights reserved.` in plain `//` (not `//!`).
- Module files in `src/` re-exported via `lib.rs`.
- `#[must_use]` on all constructors and value-returning methods.
- Throughput thresholds gated on `cfg!(debug_assertions)` for debug vs release.
- All crates use `workspace = true` for shared package fields.

**Afecta hot path**: YES — the TCP accept loop and per-connection query dispatch are hot paths.
Throughput regression guards are mandatory.

---

## Decisions Previas Necesarias

None. The architecture is fully specified. TLS and auth are already in place. Tokio is the only async
runtime that fits the existing `Arc<RwLock<...>>` concurrency model and rustls ecosystem.

---

## New Dependencies to Add

**tessera-protocol/Cargo.toml**:
- `tokio = { version = "1", features = ["io-util"] }` — `AsyncRead`/`AsyncWrite` for codec
- `serde = { workspace = true }` — message serialization
- `serde_json = { workspace = true }` — wire encoding

**tessera-server/Cargo.toml**:
- `tokio = { version = "1", features = ["net", "rt-multi-thread", "macros", "sync", "time"] }`
- `tokio-rustls = "0.26"` — async TLS over Tokio streams
- `tessera-graph = { workspace = true }` — Graph type for query execution

---

## Plan de Ejecución

---

### Layer 1: Wire Protocol — Frame Codec

A frame is a length-prefixed binary message:
`[u32 big-endian length][payload bytes]`
Maximum frame size: 16 MiB (enforced by the decoder to prevent memory exhaustion attacks).

---

## Task: Frame Codec

### Cycle 1: Encode a frame
- RED: Write test `frame_encode_produces_length_prefix` in
  `crates/tessera-protocol/tests/frame_test.rs`
  - Assert: encoding `b"hello"` produces exactly 9 bytes: `[0,0,0,5]` + `b"hello"`
- GREEN: Create `crates/tessera-protocol/src/frame.rs`.
  Implement `fn encode(payload: &[u8]) -> Vec<u8>` that writes a 4-byte big-endian length
  followed by the payload.
- REFACTOR: none at this stage.

### Cycle 2: Decode a complete frame
- RED: Add test `frame_decode_complete_frame`
  - Assert: decoding the 9-byte buffer `[0,0,0,5,h,e,l,l,o]` returns `Ok(Some(b"hello"))` and
    advances the cursor by 9.
- GREEN: Implement `fn decode(buf: &mut bytes::BytesMut) -> Result<Option<Vec<u8>>>` using
  `bytes::Buf`. Add `bytes = "1"` to `tessera-protocol` dev-dependencies for tests;
  add to main dependencies because the codec is used in async production paths.
  Return `Ok(None)` when the buffer has fewer than 4 bytes or fewer bytes than the declared length.
- REFACTOR: none.

### Cycle 3: Reject oversized frames
- RED: Add test `frame_decode_rejects_oversized_frame`
  - Assert: a frame declaring length `16 * 1024 * 1024 + 1` (exceeds 16 MiB cap) returns
    `Err(ProtocolError::FrameTooLarge { declared: ... })`.
- GREEN: Add `ProtocolError::FrameTooLarge { declared: u32 }` variant to `error.rs`.
  In `decode`, check `length > MAX_FRAME_SIZE` before waiting for the payload.
  Define `pub const MAX_FRAME_SIZE: u32 = 16 * 1024 * 1024`.
- REFACTOR: none.

### Cycle 4: Async framed reader over `AsyncRead`
- RED: Add test `framed_reader_reads_single_frame` using `tokio::io::duplex`
  - Assert: writing one complete encoded frame to the write half, then calling
    `FramedReader::read_frame(&mut reader).await` returns `Ok(Some(payload))`.
- GREEN: Create `pub struct FramedReader<R>(R)` with
  `pub async fn read_frame(&mut self) -> Result<Option<Vec<u8>>>` that reads the 4-byte length
  header with `AsyncReadExt::read_exact`, then reads the payload. Returns `Ok(None)` on clean
  EOF (0 bytes on header read).
- REFACTOR: none.

### Cycle 5: Async framed writer over `AsyncWrite`
- RED: Add test `framed_writer_writes_encoded_frame`
  - Assert: `FramedWriter::write_frame(payload).await` followed by reading the raw bytes from
    the other end of a `tokio::io::duplex` pipe yields the correctly prefixed bytes.
- GREEN: Create `pub struct FramedWriter<W>(W)` with
  `pub async fn write_frame(&mut self, payload: &[u8]) -> Result<()>` that calls `encode` then
  `AsyncWriteExt::write_all`.
- REFACTOR: none.

---

### Layer 2: Message Types

Wire encoding: JSON over the framed layer (easily debuggable, already a workspace dependency).
All messages are `serde`-serializable enums.

---

## Task: Request and Response Message Types

### Cycle 6: ClientMessage enum serialises and deserialises
- RED: Write test `client_message_login_roundtrip` in
  `crates/tessera-protocol/tests/message_test.rs`
  - Assert: `serde_json::to_string(&ClientMessage::Login { username: "admin".into(), password: "pw".into() })`
    roundtrips back to the same value via `serde_json::from_str`.
- GREEN: Create `crates/tessera-protocol/src/message.rs`.
  Define:
  ```rust
  #[derive(Debug, Serialize, Deserialize)]
  #[serde(tag = "type", rename_all = "snake_case")]
  pub enum ClientMessage {
      Login { username: String, password: String },
      Query { query: String, language: String },
      Logout,
      Ping,
  }
  ```
- REFACTOR: none.

### Cycle 7: ServerMessage enum serialises and deserialises
- RED: Add test `server_message_auth_ok_roundtrip`
  - Assert: `ServerMessage::AuthOk { token: "tok".into() }` roundtrips via JSON.
- GREEN: In `message.rs`, define:
  ```rust
  #[derive(Debug, Serialize, Deserialize)]
  #[serde(tag = "type", rename_all = "snake_case")]
  pub enum ServerMessage {
      AuthOk { token: String },
      AuthError { reason: String },
      QueryResult { columns: Vec<String>, rows: Vec<Vec<serde_json::Value>> },
      QueryError { reason: String },
      Pong,
      Bye,
  }
  ```
- REFACTOR: none.

### Cycle 8: Unknown message type returns a deserialisation error (not a panic)
- RED: Add test `unknown_message_type_returns_err`
  - Assert: `serde_json::from_str::<ClientMessage>(r#"{"type":"unknown"}"#)` returns `Err(...)`.
- GREEN: This is already guaranteed by `#[serde(tag = "type")]` on an exhaustive enum with no
  `#[serde(other)]`. No production code change needed; the test confirms the contract.
- REFACTOR: none.

---

### Layer 3: Connection Handler — Single Client Session Lifecycle

Each accepted TCP stream is driven by `ConnectionHandler`, which owns the `FramedReader`,
`FramedWriter`, and session state (optional `SessionToken`).

---

## Task: ConnectionHandler

### Cycle 9: Unauthenticated query is rejected
- RED: Write test `connection_handler_rejects_query_without_login` in
  `crates/tessera-server/tests/connection_handler_test.rs`
  - Setup: build a `ConnectionHandler` with a test `ServerContext` (reuse helper from
    `auth_integration_test.rs`) and a `tokio::io::duplex` pipe.
  - Action: send a `ClientMessage::Query { ... }` frame before login.
  - Assert: the response frame deserialises to `ServerMessage::AuthError { reason: ... }`.
- GREEN: Create `crates/tessera-server/src/connection.rs`.
  Implement `pub struct ConnectionHandler<S>` (generic over stream) with field
  `session_token: Option<SessionToken>`.
  Implement `pub async fn run(&mut self) -> Result<()>` that drives a `loop`:
  - Read one frame from `FramedReader`.
  - Deserialise to `ClientMessage`.
  - If `session_token.is_none()` and message is not `Login`, write `ServerMessage::AuthError`.
  - On `ClientMessage::Login`, call `authenticate` helper (stub for now — returns `AuthError`).
  - On EOF, break.
  Add `ServerError::Protocol(#[from] ProtocolError)` and `ServerError::Json(serde_json::Error)`
  to a new `crates/tessera-server/src/error.rs`.
- REFACTOR: Extract `send_message` helper to avoid duplicating the serialise+write_frame calls.

### Cycle 10: Successful login returns AuthOk with a session token
- RED: Add test `connection_handler_login_returns_auth_ok`
  - Setup: duplex pipe + valid admin credentials.
  - Assert: response is `ServerMessage::AuthOk { token }` where `!token.is_empty()`.
- GREEN: Implement `authenticate` in `connection.rs`:
  Call `ctx.sessions.create_session(user_id)` after verifying credentials through
  `ctx.auth_policy`. Store the returned `SessionToken` in `self.session_token`.
  Add `sessions: Arc<SessionManager>` and `auth_policy: Arc<AuthPolicy>` to the fields
  accessible from `ConnectionHandler` (pass them in via constructor from `ServerContext`).
  The `ServerContext` needs to expose `Arc<AuthPolicy>`, `Arc<SessionManager>`, and the
  `UserStoreHandle` — add getters `auth_policy()`, `sessions()`, `user_store()` to `ServerContext`.
- REFACTOR: none.

### Cycle 11: Wrong password returns AuthError (not a panic, not a crash)
- RED: Add test `connection_handler_wrong_password_returns_auth_error`
  - Assert: sending `ClientMessage::Login { username: "admin", password: "wrong" }` yields
    `ServerMessage::AuthError { reason }` where `!reason.is_empty()`.
- GREEN: In `authenticate`, map `AuthError::*` to `ServerMessage::AuthError { reason:
  e.to_string() }` and write the response — do NOT propagate the error up to `run`. The
  connection stays alive to allow the client to retry.
- REFACTOR: none.

### Cycle 12: Ping is answered with Pong (no authentication required for liveness checks)
- RED: Add test `connection_handler_ping_returns_pong`
  - Assert: `ClientMessage::Ping` before login returns `ServerMessage::Pong`.
- GREEN: Add a `Ping` arm before the authentication guard in `run`. This is intentional —
  liveness checks must not require authentication.
- REFACTOR: none.

### Cycle 13: Logout invalidates the session and returns Bye
- RED: Add test `connection_handler_logout_invalidates_session`
  - Setup: login first, receive `AuthOk`.
  - Action: send `ClientMessage::Logout`.
  - Assert: response is `ServerMessage::Bye`.
  - Assert: sending another `ClientMessage::Query` after logout returns `ServerMessage::AuthError`.
- GREEN: In `run`, on `Logout`:
  - Call `ctx.sessions.invalidate(token)` (add `pub fn invalidate(&self, token: &SessionToken)`
    to `SessionManager` if not present).
  - Set `self.session_token = None`.
  - Write `ServerMessage::Bye`.
- REFACTOR: none.

### Cycle 14: Query executes through the GQL engine and returns QueryResult
- RED: Add test `connection_handler_query_returns_result`
  - Setup: login, then send `ClientMessage::Query { query: "MATCH (n) RETURN n", language: "gql" }`.
  - Assert: response is `ServerMessage::QueryResult { columns, rows }`.
- GREEN: Add `graph: Arc<RwLock<Graph>>` to `ConnectionHandler`. On `Query`:
  - Check permission via `ctx.check_permission(&token, Permission::NodeRead)`.
  - Acquire read lock on `graph`.
  - Call `tessera_graph::gql::execute(graph, &query)` (read-only path).
  - Serialize the `GqlQueryResult` rows to `Vec<Vec<serde_json::Value>>`.
  - Write `ServerMessage::QueryResult`.
  Add `tessera-graph` as a dependency in `tessera-server/Cargo.toml` (already workspace dep).
- REFACTOR: Extract `execute_query` as a standalone async fn to keep `run` readable.

---

### Layer 4: TCP Listener — Accept Loop

---

## Task: TcpListener and Accept Loop

### Cycle 15: Listener binds to an address
- RED: Write test `listener_binds_to_address` in
  `crates/tessera-server/tests/listener_test.rs`
  - Assert: `TesseraListener::bind("127.0.0.1:0").await` returns `Ok(listener)` and
    `listener.local_addr()` is a valid `SocketAddr`.
- GREEN: Create `crates/tessera-server/src/listener.rs`.
  Implement `pub struct TesseraListener` wrapping `tokio::net::TcpListener`.
  Add `pub async fn bind(addr: &str) -> Result<Self>`.
  Add `pub fn local_addr(&self) -> Result<SocketAddr>`.
  Add `ServerError::Io(#[from] std::io::Error)` to `error.rs`.
- REFACTOR: none.

### Cycle 16: Listener accepts a connection and hands off to ConnectionHandler
- RED: Add test `listener_accepts_and_dispatches_connection`
  - Setup: bind listener, connect a `TcpStream` client.
  - Assert: `listener.accept().await` returns `Ok((stream, addr))` where `addr` is a valid peer.
- GREEN: Implement `pub async fn accept(&self) -> Result<(tokio::net::TcpStream, SocketAddr)>`
  delegating to `TcpListener::accept`.
- REFACTOR: none.

### Cycle 17: Concurrent connections are handled on separate tasks
- RED: Add test `listener_handles_two_concurrent_connections`
  - Setup: spawn the accept loop in a background task; connect two `TcpStream` clients; send
    `ClientMessage::Ping` from each.
  - Assert: both receive `ServerMessage::Pong` without blocking each other.
- GREEN: Add `pub async fn serve(self, ctx: Arc<ServerContext>, graph: Arc<RwLock<Graph>>,
  shutdown: tokio::sync::watch::Receiver<bool>) -> Result<()>` to `TesseraListener`.
  Inside: loop `tokio::select!` between `self.accept()` and `shutdown.changed()`.
  For each accepted stream, `tokio::spawn(ConnectionHandler::new(stream, ctx.clone(),
  graph.clone()).run())`.
- REFACTOR: none.

---

### Layer 5: Connection Limits, Timeouts, and TLS Wrapping

---

## Task: Connection Limits

### Cycle 18: Server enforces maximum concurrent connection limit
- RED: Write test `server_rejects_connection_when_at_capacity` in
  `crates/tessera-server/tests/limits_test.rs`
  - Setup: configure `max_connections = 2`, bind, connect 3 clients.
  - Assert: the 3rd client's TCP stream receives `ServerMessage::AuthError` with reason
    "server at capacity" immediately and the stream is closed.
- GREEN: Add `active_connections: Arc<AtomicUsize>` and `max_connections: usize` to the
  accept loop in `serve`. On accept, atomically increment; if count exceeds max, write
  the capacity error frame and close the stream. On connection handler exit, atomically
  decrement.
  Use `std::sync::atomic::AtomicUsize` + `Ordering::SeqCst`.
- REFACTOR: none.

### Cycle 19: Idle connections are closed after timeout
- RED: Add test `idle_connection_is_closed_after_timeout`
  - Setup: connect a client; do not send any message.
  - Assert: after `idle_timeout_ms` elapses, the connection is dropped (read returns `Ok(None)`
    or `Err`).
- GREEN: Wrap the `FramedReader::read_frame` call in `tokio::time::timeout(idle_timeout, ...)`.
  On `Err(Elapsed)`, write `ServerMessage::Bye` and break the loop.
  Pass `idle_timeout: Duration` to `ConnectionHandler::new`.
- REFACTOR: none.

### Cycle 20: TLS handshake wraps the TCP stream before handing to ConnectionHandler
- RED: Write test `tls_connection_completes_handshake` in
  `crates/tessera-server/tests/tls_integration_test.rs`
  - Setup: generate self-signed cert (reuse helper from existing tests); create a
    `tokio_rustls::TlsAcceptor`; in a background task, accept and complete the TLS handshake
    while a `tokio_rustls::TlsConnector` client connects.
  - Assert: both the server-side `TlsStream<TcpStream>` and client-side stream are established
    (no error).
- GREEN: In `serve`, after accepting the raw `TcpStream`, wrap with
  `TlsAcceptor::from(ctx.tls_config().server_config().clone()).accept(stream).await`.
  Add `pub fn tls_config(&self) -> &TlsConfig` getter to `ServerContext`.
  Add `tokio-rustls = "0.26"` to `tessera-server/Cargo.toml`.
  `ConnectionHandler` becomes generic: `ConnectionHandler<S: AsyncRead + AsyncWrite + Unpin>`.
- REFACTOR: none.

---

### Layer 6: Graceful Shutdown

---

## Task: Graceful Shutdown

### Cycle 21: Shutdown signal stops the accept loop
- RED: Write test `graceful_shutdown_stops_accept_loop` in
  `crates/tessera-server/tests/shutdown_test.rs`
  - Setup: start `serve` with a `watch::channel`; let it accept one ping/pong exchange;
    send shutdown signal via the watch sender.
  - Assert: `serve` completes (i.e. `join.await` does not block forever — use
    `tokio::time::timeout` around it).
- GREEN: In `serve`, when `shutdown.changed().await` triggers and `*shutdown.borrow() == true`,
  break the accept loop. Active connection tasks are already detached — they will run to
  completion or idle-timeout.
- REFACTOR: none.

### Cycle 22: Server sends Bye to active connections on shutdown
- RED: Add test `server_sends_bye_on_shutdown_to_active_connections`
  - Setup: connect a client and keep it idle (logged in); send shutdown signal.
  - Assert: client receives `ServerMessage::Bye` before the stream closes.
- GREEN: Pass a `shutdown: watch::Receiver<bool>` clone to each `ConnectionHandler`.
  In `ConnectionHandler::run`, add a second arm to `tokio::select!` watching
  `shutdown.changed()`. When triggered, write `ServerMessage::Bye` and break.
- REFACTOR: none.

---

### Layer 7: Throughput Regression Guard (Hot Path)

The accept loop and per-connection query dispatch are hot paths. The following tests must pass
in both debug and release to guard against regressions.

---

## Task: Throughput Regression Guard

### Cycle 23: Measure frame encode/decode throughput
- RED: Write test `frame_codec_throughput_guard` in
  `crates/tessera-protocol/tests/throughput_test.rs`
  - Measure: encode + decode 100,000 frames of 256 bytes each, time it.
  - Assert:
    ```rust
    let min_ops = if cfg!(debug_assertions) { 200_000 } else { 1_000_000 };
    assert!(ops_per_sec >= min_ops, "frame codec regression: {} ops/s < {}", ops_per_sec, min_ops);
    ```
- GREEN: No production code change — the guard documents the baseline.
- REFACTOR: none.

### Cycle 24: Measure end-to-end Ping/Pong throughput over plain TCP
- RED: Write test `ping_pong_throughput_guard` in
  `crates/tessera-server/tests/throughput_test.rs`
  - Setup: start `serve` on a loopback port (no TLS — use a plain stream variant for
    benchmarking), connect one client, login.
  - Measure: send 10,000 `Ping` messages, count `Pong` responses, time elapsed.
  - Assert:
    ```rust
    let min_rps = if cfg!(debug_assertions) { 2_000 } else { 20_000 };
    assert!(rps >= min_rps, "ping-pong regression: {} rps < {}", rps, min_rps);
    ```
- GREEN: No production code change — the guard documents the baseline.
- REFACTOR: none.

---

### Final: Wiring Verification

Every new public symbol must have at least one call site in production code (not just in tests).
Verify with `grep -r` after implementation.

---

## Wiring Checklist

- [ ] `frame::encode` is called inside `FramedWriter::write_frame` (production path)
- [ ] `frame::decode` is called inside `FramedReader::read_frame` (production path)
- [ ] `FramedReader` and `FramedWriter` are instantiated in `ConnectionHandler::new`
- [ ] `ClientMessage` is deserialised in `ConnectionHandler::run`
- [ ] `ServerMessage` variants are serialised and written in `ConnectionHandler::run`
- [ ] `TesseraListener::bind` is called in `main.rs`
- [ ] `TesseraListener::serve` is called in `main.rs`
- [ ] The `watch::channel` shutdown sender is held in `main.rs` and sent on `Ctrl-C` via
      `tokio::signal::ctrl_c().await`
- [ ] `ConnectionHandler` generic over `S: AsyncRead + AsyncWrite + Unpin` is instantiated with
      both plain `TcpStream` (tests) and `TlsStream<TcpStream>` (production path in `serve`)
- [ ] `max_connections` config field is read from env/config in `main.rs` and passed to `serve`
- [ ] `idle_timeout` config field is read from env/config in `main.rs` and passed to
      `ConnectionHandler::new`
- [ ] `ServerContext::tls_config()` getter is called in `serve` to construct `TlsAcceptor`
- [ ] `ServerContext::auth_policy()`, `sessions()`, `user_store()` getters are called in
      `ConnectionHandler::authenticate`
- [ ] `SessionManager::invalidate` is called in `ConnectionHandler::run` on `Logout`
- [ ] `active_connections` `AtomicUsize` is decremented on every `ConnectionHandler::run` exit
      (use a RAII guard or ensure the decrement is in the `tokio::spawn` closure after `.await`)
- [ ] `ProtocolError::FrameTooLarge` is returned in `FramedReader::read_frame` in production code
- [ ] New `ServerError` variants (`Protocol`, `Json`, `Io`) are actually returned in
      `ConnectionHandler::run` or `TesseraListener::bind` / `serve`
- [ ] `lib.rs` in `tessera-server` re-exports `connection::ConnectionHandler`,
      `listener::TesseraListener`, and `error::{ServerError, Result}`
- [ ] `lib.rs` in `tessera-protocol` re-exports `frame::{FramedReader, FramedWriter, MAX_FRAME_SIZE}`,
      `message::{ClientMessage, ServerMessage}`
- [ ] No stale empty `fn main() {}` remains — `main.rs` must have a real `#[tokio::main]` entry

---

## File Map

```
crates/tessera-protocol/
  src/
    frame.rs          (NEW — encode, decode, FramedReader, FramedWriter, MAX_FRAME_SIZE)
    message.rs        (NEW — ClientMessage, ServerMessage)
    error.rs          (MODIFY — add FrameTooLarge variant)
    lib.rs            (MODIFY — re-export frame and message modules)
  tests/
    frame_test.rs     (NEW — cycles 1–5)
    message_test.rs   (NEW — cycles 6–8)
    throughput_test.rs (NEW — cycle 23)
  Cargo.toml          (MODIFY — add tokio io-util, serde, serde_json, bytes)

crates/tessera-server/
  src/
    connection.rs     (NEW — ConnectionHandler)
    listener.rs       (NEW — TesseraListener)
    error.rs          (NEW — ServerError)
    context.rs        (MODIFY — add getters: tls_config, auth_policy, sessions, user_store)
    lib.rs            (MODIFY — re-export connection, listener, error)
    main.rs           (MODIFY — replace stub with #[tokio::main] entry point)
  tests/
    connection_handler_test.rs (NEW — cycles 9–14)
    listener_test.rs           (NEW — cycles 15–17)
    limits_test.rs             (NEW — cycles 18–19)
    tls_integration_test.rs    (NEW — cycle 20)
    shutdown_test.rs           (NEW — cycles 21–22)
    throughput_test.rs         (NEW — cycle 24)
  Cargo.toml          (MODIFY — add tokio full, tokio-rustls, tessera-graph)
```

---

## Estimacion Total

- Implementacion: 8–10 horas
- Testing funcional (cycles 1–22): incluido — se escribe primero
- Testing de rendimiento (cycles 23–24): 1 hora adicional
- Wiring verification (final checklist): 30 minutos

## Criterios de Exito

- [ ] `cargo test --workspace` pasa con 0 errores y 0 warnings (warnings = errors por lints del workspace)
- [ ] `cargo clippy --workspace --tests -- -D warnings` pasa limpio
- [ ] Frame encode/decode throughput >= 200,000 ops/s (debug) / 1,000,000 ops/s (release)
- [ ] Ping/Pong throughput >= 2,000 rps (debug) / 20,000 rps (release)
- [ ] Wiring checklist 100% completo
- [ ] `main.rs` arranca el servidor con TLS y auth obligatorios — no hay modo inseguro
