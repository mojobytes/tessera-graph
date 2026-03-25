# TDD Plan: Bolt Milestone 6 — Migrate tessera-cli to Bolt 4.4

**Created**: 2026-03-25
**Status**: Ready for execution
**Estimated**: ~7h

---

## Context

The server (M1–M5) now speaks Bolt 4.4 exclusively via `BoltConnectionHandler`.
The old JSON-over-TCP protocol is gone. The CLI (`tessera-cli`) is currently broken
because `connection.rs`, `auth.rs`, and `query.rs` still use `ClientMessage` /
`ServerMessage` (the old JSON framing layer) via `FramedReader` / `FramedWriter`.

This plan migrates the CLI to Bolt 4.4 by introducing a `BoltClient` struct that
encapsulates the full client-side protocol: handshake → HELLO → RUN/PULL cycles →
GOODBYE. All existing CLI commands (query, exec, import, export, ping-equivalent,
REPL) are then rewired to use `BoltClient` instead of the old `Session`.

**Stack detected**: Rust 1.85, Tokio async, tokio-rustls 0.26, tessera-protocol
workspace crate already in `tessera-cli`'s dependencies.

**Conventions observed**:
- Unit tests in `#[cfg(test)] mod tests` inside `src/*.rs`
- Integration tests in `crates/tessera-cli/tests/*.rs` (directory does not yet
  exist — must be created)
- Clippy `deny(all)`, `warn(pedantic, nursery)` — warnings are errors
- `unsafe_code = forbid`
- `// OK: test` comments are required on all `.unwrap()` / `.expect()` inside
  `#[cfg(test)]` blocks

**Does this affect a hot path?**: No. The CLI is a single-user command-line tool.
No throughput regression tests are required.

---

## Architectural Decision: Where Does BoltClient Live?

`BoltClient` **must live in `tessera-protocol`**, not in `tessera-cli`.

Rationale:
- The server-side test helpers in `crates/tessera-server/tests/common/mod.rs`
  already manually reproduce the client-side handshake for integration tests.
  A canonical `BoltClient` in `tessera-protocol` would eliminate that duplication
  in the future and make it available to any crate (e.g., future SDK, bench harness,
  `tessera-server` integration tests).
- `tessera-cli` already depends on `tessera-protocol`. No new dependency needed.
- Placing client logic inside `tessera-cli` would make it impossible to reuse from
  `tessera-server`'s tests without a circular dependency.

The `BoltClient` is added to a new `tessera-protocol/src/bolt_client.rs` module
and exported from `tessera-protocol/src/lib.rs`.

---

## Decisions Previas Necesarias

None. The Bolt 4.4 wire format is fully specified by the existing `tessera-protocol`
building blocks. The architectural placement decision is resolved above. Proceed
directly.

---

## Plan de Ejecución

### Phase 1 — BoltClient: handshake + send/recv primitives

#### Cycle 1.1 — RED: test that BoltClient performs the Bolt 4.4 handshake

1. [ ] Create `crates/tessera-protocol/tests/bolt_client_test.rs` (15 min)
   - File: `crates/tessera-protocol/tests/bolt_client_test.rs`
   - Action: Create
   - Test: `bolt_client_handshake_negotiates_version_44`
     - Spawn a minimal mock server that reads the 20-byte handshake and replies
       `[0x00, 0x04, 0x04, 0x00]`.
     - Call `BoltClient::connect(stream)` (signature TBD in the GREEN step).
     - Assert: returns `Ok(client)` without error.
   - Test: `bolt_client_rejects_unsupported_version_response`
     - Mock server replies `[0x00, 0x00, 0x00, 0x00]` (no common version).
     - Assert: `connect` returns `Err(ProtocolError::BoltInvalidHandshake { .. })`.
   - Output: two failing tests

2. [ ] Create `crates/tessera-protocol/src/bolt_client.rs` skeleton (20 min)
   - File: `crates/tessera-protocol/src/bolt_client.rs`
   - Action: Create — declare `pub struct BoltClient<R, W>` with fields:
     - `reader: BoltChunkedReader<R>`
     - `writer: BoltChunkedWriter<W>`
   - Add `pub async fn connect<S>(stream: S) -> crate::Result<Self>` where
     `S: AsyncRead + AsyncWrite + Unpin` (split internally with `tokio::io::split`).
   - The connect method: writes the 20-byte magic + version proposal, reads 4-byte
     response, validates it is `[0x00, 0x04, 0x04, 0x00]`, returns
     `BoltInvalidHandshake` otherwise.
   - Add `pub mod bolt_client;` to `lib.rs` and re-export `BoltClient`.
   - Output: two passing tests, `cargo clippy` clean

#### Cycle 1.2 — RED: test send_request / recv_response primitives

3. [ ] Extend `bolt_client_test.rs` (15 min)
   - Test: `bolt_client_send_recv_roundtrip`
     - After handshake, call `client.send_request(&BoltRequest::Reset)`.
     - Mock server reads the chunked message, decodes it, asserts it is
       `BoltRequest::Reset`, encodes `BoltResponse::Success { metadata: vec![] }`,
       writes it back chunked.
     - Call `client.recv_response()`, assert `BoltResponse::Success { .. }`.
   - Output: one failing test

4. [ ] Implement `send_request` and `recv_response` on `BoltClient` (20 min)
   - File: `crates/tessera-protocol/src/bolt_client.rs`
   - `pub async fn send_request(&mut self, req: &BoltRequest) -> crate::Result<()>`
     — calls `encode_request`, then `self.writer.write_message`.
   - `pub async fn recv_response(&mut self) -> crate::Result<BoltResponse>`
     — calls `self.reader.read_message`, returns `Err(Io(UnexpectedEof))` on
     `None`, then calls `decode_response`.
   - Output: roundtrip test passes, clippy clean

---

### Phase 2 — BoltClient: HELLO authentication

#### Cycle 2.1 — RED: test hello_auth sends principal/credentials

5. [ ] Add test `bolt_client_hello_success` and `bolt_client_hello_failure` (15 min)
   - File: `crates/tessera-protocol/tests/bolt_client_test.rs`
   - `bolt_client_hello_success`:
     - Post-handshake mock server reads one message, decodes it as
       `BoltRequest::Hello`, asserts `extra` contains `principal = "alice"` and
       `credentials = "secret"`, responds with `BoltResponse::Success { metadata: vec![] }`.
     - Call `client.hello("alice", "secret", None)`, assert `Ok(())`.
   - `bolt_client_hello_failure`:
     - Mock server responds with `BoltResponse::Failure { metadata: vec![("message", "authentication failed")] }`.
     - Call `client.hello("alice", "wrong", None)`, assert `Err(...)` (we will use
       a new `ProtocolError::BoltAuthFailure { message: String }` variant).
   - Output: two failing tests

6. [ ] Add `BoltAuthFailure` to `ProtocolError` and implement `hello` (20 min)
   - File: `crates/tessera-protocol/src/error.rs` — add variant:
     `#[error("bolt authentication failure: {message}")] BoltAuthFailure { message: String }`
   - File: `crates/tessera-protocol/src/bolt_client.rs`
   - Signature: `pub async fn hello(&mut self, username: &str, password: &str, db: Option<&str>) -> crate::Result<()>`
   - Builds `BoltRequest::Hello { extra }` with `principal` and `credentials` keys;
     optionally appends `db` key when `Some`.
   - Sends the request, receives one response:
     - `BoltResponse::Success { .. }` → `Ok(())`
     - `BoltResponse::Failure { metadata }` → `Err(BoltAuthFailure { message: extract_message(metadata) })`
     - anything else → `Err(BoltUnexpectedTag { .. })`
   - Private helper `fn extract_bolt_str(meta: &BoltDict, key: &str) -> String`
     returns the string value or `"(none)"`.
   - Output: both hello tests pass, clippy clean

---

### Phase 3 — BoltClient: run_query (RUN + PULL cycle)

#### Cycle 3.1 — RED: test run_query returns columns and rows

7. [ ] Add tests for `run_query` (20 min)
   - File: `crates/tessera-protocol/tests/bolt_client_test.rs`
   - `bolt_client_run_query_returns_records`:
     - Mock server: receives RUN, responds SUCCESS with `fields: ["name"]` in
       metadata; receives PULL, sends one RECORD `["Alice"]`, then SUCCESS.
     - Call `client.run_query("MATCH (n) RETURN n.name")`.
     - Assert: `columns = ["name"]`, `rows.len() == 1`, first cell is string "Alice".
   - `bolt_client_run_query_empty_result`:
     - Mock server: SUCCESS on RUN, immediate SUCCESS on PULL (no records).
     - Assert: `rows.is_empty()`.
   - `bolt_client_run_query_server_failure`:
     - Mock server: FAILURE on RUN with `message = "syntax error"`.
     - Assert: `Err(BoltAuthFailure { .. })` — actually we need a distinct error.
       Add `ProtocolError::BoltQueryFailure { message: String }` to `error.rs`.
   - Output: three failing tests

8. [ ] Add `BoltQueryFailure` to `ProtocolError` and implement `run_query` (25 min)
   - File: `crates/tessera-protocol/src/error.rs` — add:
     `#[error("bolt query failure: {message}")] BoltQueryFailure { message: String }`
   - File: `crates/tessera-protocol/src/bolt_client.rs`
   - Introduce `pub struct QueryResult { pub columns: Vec<String>, pub rows: Vec<Vec<PackStreamValue>> }`
     (in `bolt_client.rs`, exported from `lib.rs`).
   - Implement `pub async fn run_query(&mut self, query: &str) -> crate::Result<QueryResult>`:
     1. Send `BoltRequest::Run { query, params: vec![], extra: vec![] }`.
     2. Receive response:
        - `Success { metadata }` → extract `fields` list from metadata to get columns.
        - `Failure { metadata }` → return `Err(BoltQueryFailure { message })`.
        - other → `Err(BoltUnexpectedTag)`.
     3. Send `BoltRequest::Pull { extra: vec![] }`.
     4. Loop reading responses:
        - `Record { fields }` → push to rows vec.
        - `Success { .. }` → break.
        - `Failure { metadata }` → return `Err(BoltQueryFailure)`.
        - other → `Err(BoltUnexpectedTag)`.
     5. Return `Ok(QueryResult { columns, rows })`.
   - Note: column extraction from RUN SUCCESS metadata: look for key `"fields"` of
     type `PackStreamValue::List` containing `PackStreamValue::String` items. If
     absent, use empty vec (server may not always return columns for mutations).
   - Output: all three run_query tests pass, clippy clean

---

### Phase 4 — BoltClient: GOODBYE and RESET

#### Cycle 4.1 — RED: test goodbye and reset

9. [ ] Add tests (10 min)
   - File: `crates/tessera-protocol/tests/bolt_client_test.rs`
   - `bolt_client_goodbye_sends_correct_message`:
     - After handshake, call `client.goodbye().await`.
     - Mock server reads the message, decodes it as `BoltRequest::Goodbye`.
     - Assert: `Ok(())`.
   - `bolt_client_reset_sends_correct_message`:
     - After handshake, call `client.reset().await`.
     - Mock server reads, decodes as `BoltRequest::Reset`, responds SUCCESS.
     - Assert: `Ok(())`.
   - Output: two failing tests

10. [ ] Implement `goodbye` and `reset` (10 min)
    - File: `crates/tessera-protocol/src/bolt_client.rs`
    - `pub async fn goodbye(&mut self) -> crate::Result<()>`
      — sends `BoltRequest::Goodbye`, no response expected (server closes connection).
    - `pub async fn reset(&mut self) -> crate::Result<()>`
      — sends `BoltRequest::Reset`, awaits `Success`, returns `Err` on anything else.
    - Output: both tests pass, clippy clean

---

### Phase 5 — Migrate connection.rs: replace Session with BoltSession

#### Cycle 5.1 — RED: unit test BoltSession wraps BoltClient

11. [ ] Add unit tests inside `crates/tessera-cli/src/connection.rs` (20 min)
    - Tests stay inside `#[cfg(test)] mod tests` in `connection.rs`.
    - `bolt_session_hello_success`:
      - Build a duplex mock that performs handshake + replies SUCCESS to HELLO.
      - Call `BoltSession::connect(stream, "admin", "pass", None).await`.
      - Assert `Ok(session)`.
    - `bolt_session_hello_auth_failure`:
      - Mock replies FAILURE to HELLO.
      - Assert `Err(CliError::Auth(_))`.
    - Output: two failing tests (BoltSession does not yet exist)

12. [ ] Replace `Session<R,W>` with `BoltSession` in `connection.rs` (20 min)
    - File: `crates/tessera-cli/src/connection.rs`
    - Remove the old `Session<R,W>` struct and all its impl blocks.
    - Remove imports of `FramedReader`, `FramedWriter`, `ClientMessage`, `ServerMessage`.
    - Define:
      ```rust
      pub struct BoltSession {
          client: tessera_protocol::BoltClient<
              tokio::io::ReadHalf<Box<dyn tokio::io::AsyncRead + Unpin + Send>>,
              tokio::io::WriteHalf<Box<dyn tokio::io::AsyncWrite + Unpin + Send>>,
          >,
      }
      ```
      Actually — `BoltClient<R,W>` is generic. Use it directly:
      ```rust
      pub struct BoltSession<R, W> {
          client: tessera_protocol::BoltClient<R, W>,
      }
      ```
      This mirrors how `Session<R,W>` worked before and keeps the same ergonomics
      for the existing callers (main.rs passes `split(tls_stream)` halves).
    - Add `BoltSession::from_connected(client: BoltClient<R, W>) -> Self`.
    - The old `.send()` / `.recv()` / `.set_token()` / `.token()` methods are
      removed — callers will use `BoltSession::client` methods directly, or we
      expose thin delegation methods as needed in Phase 6.
    - Output: two connection tests pass, clippy clean

---

### Phase 6 — Migrate auth.rs: replace Login/AuthOk with HELLO

#### Cycle 6.1 — RED: test bolt_auth::login uses HELLO

13. [ ] Rewrite unit tests in `crates/tessera-cli/src/auth.rs` (15 min)
    - The existing tests use `FramedReader`/`FramedWriter` mock — replace with
      duplex + Bolt handshake mock.
    - `auth_ok_returns_ok`: mock does handshake, replies SUCCESS to HELLO → `Ok(())`.
    - `auth_failure_returns_cli_error`: mock replies FAILURE → `Err(CliError::Auth(_))`.
    - `auth_unexpected_response_returns_connection_error`: mock replies IGNORED →
      `Err(CliError::Connection(_))`.
    - Output: three failing tests

14. [ ] Rewrite `auth::login` to call `session.client.hello(...)` (15 min)
    - File: `crates/tessera-cli/src/auth.rs`
    - Remove all imports of old protocol types.
    - New signature: `pub async fn login<R, W>(session: &mut BoltSession<R, W>, username: &str, password: &str) -> Result<(), CliError>`
    - Body: calls `session.client.hello(username, password, None).await`, maps
      `ProtocolError::BoltAuthFailure { message }` → `CliError::Auth(message)`,
      maps other errors → `CliError::Connection(...)`.
    - Remove `set_token` / `token()` entirely (Bolt has no token concept — the
      session itself is the authentication context).
    - Output: all three auth tests pass, clippy clean

---

### Phase 7 — Migrate query.rs: replace Query/QueryResult with RUN/PULL

#### Cycle 7.1 — RED: test execute_query uses RUN + PULL

15. [ ] Rewrite unit tests in `crates/tessera-cli/src/query.rs` (20 min)
    - Replace mock session with Bolt chunked duplex mock.
    - `query_result_returns_output`: mock handles handshake, sends RUN SUCCESS with
      `fields: ["name"]`, then PULL with RECORD `["Alice"]` + SUCCESS.
      Assert `output.columns = ["name"]`, `rows.len() == 1`.
    - `query_error_returns_cli_error`: mock sends FAILURE on RUN.
      Assert `Err(CliError::Query(_))`.
    - `empty_result_set`: mock sends RUN SUCCESS, PULL SUCCESS (no records).
      Assert `rows.is_empty()`.
    - `unexpected_response_returns_connection_error`: mock sends IGNORED on RUN.
      Assert `Err(CliError::Connection(_))`.
    - Output: four failing tests

16. [ ] Rewrite `query::execute_query` to call `session.client.run_query(...)` (20 min)
    - File: `crates/tessera-cli/src/query.rs`
    - Remove all old protocol imports.
    - `QueryOutput` struct remains but `rows` changes type from
      `Vec<Vec<serde_json::Value>>` to `Vec<Vec<PackStreamValue>>`.

    **NOTE**: The `output.rs` renderer and the `export.rs` module currently consume
    `Vec<Vec<serde_json::Value>>`. They must be updated to accept
    `Vec<Vec<PackStreamValue>>` in Phase 8. Flag this dependency now.

    - New body: calls `session.client.run_query(query).await`, maps
      `BoltQueryFailure` → `CliError::Query(message)`,
      `BoltAuthFailure` → `CliError::Auth(message)`,
      `Io` → `CliError::Connection(...)`.
    - Map `QueryResult.rows` directly to `QueryOutput.rows`.
    - Output: four query tests pass, clippy clean

---

### Phase 8 — Adapt output.rs and export.rs to PackStreamValue rows

#### Cycle 8.1 — RED: update render and export signatures

17. [ ] Add/update unit tests in `crates/tessera-cli/src/output` and `export.rs` (15 min)
    - File: `crates/tessera-cli/src/output/` (check existing test coverage)
    - Add tests that call `render(format, columns, rows, ...)` with
      `Vec<Vec<PackStreamValue>>` rows containing `String`, `Int`, `Float`, `Null`,
      `Bool` values and assert the rendered strings are correct.
    - File: `crates/tessera-cli/src/export.rs`
    - Add/update tests that pass `PackStreamValue` rows.
    - Output: failing tests (function signatures still use `serde_json::Value`)

18. [ ] Update `output.rs` and `export.rs` to accept `&[Vec<PackStreamValue>]` (25 min)
    - File: `crates/tessera-cli/src/output/` — update `render` function signature.
    - Add a `packstream_value_to_display(v: &PackStreamValue) -> String` helper
      that converts each variant to a human-readable string for table/CSV/JSON
      rendering. Cases: `Null` → `"null"`, `Bool` → `"true"`/`"false"`,
      `Int(i)` → `i.to_string()`, `Float(f)` → format with reasonable precision,
      `String(s)` → `s.clone()`, `List` / `Dict` / `Struct` → JSON-like fallback.
    - File: `crates/tessera-cli/src/export.rs` — update `format_export` to use the
      new row type. For JSON export, serialize each `PackStreamValue` via the helper.
    - Remove `serde_json` dependency from render/export paths (keep it only where
      needed for JSON output mode).
    - Output: all output/export tests pass, clippy clean

---

### Phase 9 — Migrate main.rs: handshake + GOODBYE + ping replacement

#### Cycle 9.1 — RED: integration test for handshake in main flow

19. [ ] Create `crates/tessera-cli/tests/` directory and first integration test (20 min)
    - File: `crates/tessera-cli/tests/bolt_integration_test.rs`
    - Test: `cli_bolt_handshake_and_hello_succeed`
      - Spawn a minimal in-process mock server (duplex) that: performs handshake,
        responds SUCCESS to HELLO.
      - Call the CLI's internal `run_with_stream(stream, config, password).await`
        (an extraction of the current `run()` logic that accepts a pre-built stream
        — see task 20).
      - Assert `Ok(())`.
    - Test: `cli_bolt_hello_wrong_password_exits_auth_error`
      - Mock server responds FAILURE to HELLO.
      - Assert `Err(CliError::Auth(_))`.
    - Output: two failing integration tests

20. [ ] Extract `run_with_stream` from `main.rs` and wire Bolt handshake (25 min)
    - File: `crates/tessera-cli/src/main.rs`
    - The current `run()` function builds TLS, connects TCP, then calls
      `Session::from_split`. Replace that block:
      1. After TLS handshake (`tls_stream`): split into `(reader, writer)`.
      2. Call `BoltClient::connect_split(reader, writer).await?` — a new convenience
         constructor on `BoltClient` that takes pre-split halves (avoids needing to
         know the concrete stream type in `connect(S)`). Add this to `bolt_client.rs`.
      3. Wrap in `BoltSession::from_connected(client)`.
      4. Call `auth::login(&mut session, username, password).await?`.
      5. Call `dispatch_command(...)`.
      6. Call `session.client.goodbye().await` (replacing the old `Logout` send,
         which is a best-effort close — ignore the error).
    - Extract the testable portion (after TLS) into:
      `async fn run_bolt(stream_reader, stream_writer, config, password) -> Result<(), CliError>`
      so integration tests can inject a duplex stream without TLS.
    - **Ping command**: The old `handle_ping` sent `ClientMessage::Ping` and waited
      for `Pong`. The new server has no ping message. Replace ping with a
      HELLO → SUCCESS check: attempt the Bolt handshake and HELLO; if both succeed,
      print "OK" and GOODBYE. If handshake or HELLO fails, return an appropriate
      `CliError`. This is semantically correct — a successful authentication proves
      the server is reachable and healthy.
    - Remove all imports of `ClientMessage`, `ServerMessage`, `FramedReader`,
      `FramedWriter`.
    - Output: integration tests pass, clippy clean

---

### Phase 10 — Remove old protocol dead code from tessera-protocol

#### Cycle 10.1 — Identify and clean up obsolete exports

21. [ ] Audit usage of old protocol types across the workspace (10 min)
    - Run: `nice cargo check --workspace 2>&1 | grep -E 'unused import|dead_code|ClientMessage|ServerMessage|FramedReader|FramedWriter'`
    - Document which crates (if any) outside `tessera-cli` still use
      `ClientMessage` / `ServerMessage` / `FramedReader` / `FramedWriter`.
    - File: `crates/tessera-protocol/src/lib.rs`
    - If no other crate uses them: add `#[cfg(feature = "legacy-json-protocol")]`
      gates around `frame`, `message` modules and their re-exports, and remove
      the feature from all Cargo.toml files. If other crates still use them,
      leave them gated and open a follow-up issue.
    - **Decision point**: if removing them breaks nothing, remove them outright
      to reduce the maintenance surface. The old `Session`, `FramedReader`,
      `FramedWriter`, `ClientMessage`, `ServerMessage` serve no purpose once M6
      is complete.
    - Output: `cargo check --workspace` clean, no warnings

---

### Phase 11 — Wiring Verification (mandatory final cycle)

22. [ ] Full workspace build (5 min)
    - Command: `nice cargo build --workspace`
    - Assert: zero errors, zero warnings.

23. [ ] Full test suite (15 min)
    - Command: `nice cargo test --workspace`
    - Assert: all tests pass, including:
      - New `bolt_client_test.rs` tests (handshake, HELLO, run_query, goodbye, reset)
      - Migrated unit tests in `connection.rs`, `auth.rs`, `query.rs`
      - New integration tests in `crates/tessera-cli/tests/`
      - All pre-existing `tessera-server` integration tests (M1–M5 must still pass)
      - All `tessera-protocol` unit tests

24. [ ] Clippy clean (5 min)
    - Command: `nice cargo clippy --workspace -- -D warnings`
    - Assert: zero lints.

25. [ ] Manual smoke test against a running server (10 min)
    - Start server locally with TLS (self-signed cert, `--tls-skip-verify` on client).
    - `tessera-cli ping` → should print `OK`.
    - `tessera-cli query "MATCH (n) RETURN n"` → should print table (empty or not).
    - `tessera-cli query "CREATE (n:SmokeTest {run: 'M6'})"` → should succeed.
    - Verify REPL starts and accepts a query without crashing.
    - This step is manual; document result in error-log if anything is wrong.

---

## Files Created / Modified Summary

### New files
- `crates/tessera-protocol/src/bolt_client.rs`
- `crates/tessera-protocol/tests/bolt_client_test.rs`
- `crates/tessera-cli/tests/bolt_integration_test.rs`

### Modified files
- `crates/tessera-protocol/src/lib.rs` — add `bolt_client` module + re-exports
- `crates/tessera-protocol/src/error.rs` — add `BoltAuthFailure`, `BoltQueryFailure`
- `crates/tessera-cli/src/connection.rs` — replace `Session<R,W>` with `BoltSession<R,W>`
- `crates/tessera-cli/src/auth.rs` — rewrite `login` to use HELLO
- `crates/tessera-cli/src/query.rs` — rewrite `execute_query` to use RUN/PULL; `QueryOutput.rows` type changes
- `crates/tessera-cli/src/output/` — update `render` to accept `PackStreamValue` rows
- `crates/tessera-cli/src/export.rs` — update `format_export` to accept `PackStreamValue` rows
- `crates/tessera-cli/src/main.rs` — wire Bolt handshake, extract `run_bolt`, replace ping, remove old protocol imports
- `crates/tessera-cli/Cargo.toml` — no new deps needed (tessera-protocol already present)

### Possibly deleted (if no other consumer found in Phase 10)
- `crates/tessera-protocol/src/frame.rs` (or gated behind feature)
- `crates/tessera-protocol/src/message.rs` (or gated behind feature)
- Old re-exports in `tessera-protocol/src/lib.rs`

---

## Estimación Total

- Phases 1–4 (BoltClient in tessera-protocol): ~2h
- Phases 5–7 (connection, auth, query migration): ~1.5h
- Phase 8 (output/export adaptation): ~1h
- Phases 9–10 (main.rs wiring + dead code removal): ~1h
- Phase 11 (verification): ~0.5h
- **Total: ~6–7h**

---

## Criterios de Éxito

- [ ] `cargo build --workspace` — zero errors, zero warnings
- [ ] `cargo test --workspace` — all tests pass (old M1–M5 tests must not regress)
- [ ] `cargo clippy --workspace -- -D warnings` — zero lints
- [ ] `tessera-cli ping` connects to a real server and prints `OK`
- [ ] `tessera-cli query "MATCH (n) RETURN n"` executes without error
- [ ] REPL mode starts and accepts queries over Bolt
- [ ] No usage of `ClientMessage`, `ServerMessage`, `FramedReader`, `FramedWriter`
  remains in `tessera-cli` (verified by `cargo check`)
- [ ] `BoltClient` is publicly exported from `tessera-protocol` and usable by any
  future crate without duplicating handshake logic
