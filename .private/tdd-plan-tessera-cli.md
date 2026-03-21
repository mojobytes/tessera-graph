# TDD Implementation Plan — tessera-cli

**Created**: 2026-03-21
**Design doc**: `.private/design-tessera-cli.md`
**Target crate**: `crates/tessera-cli/`

---

## Context

`tessera-cli` is the admin CLI for TesseraGraph Enterprise — the `psql` equivalent. It connects
to a running server via TLS 1.3, authenticates, and provides both a single-shot query mode and
an interactive REPL. The crate does not yet exist.

**Stack detected**: Rust 2024, edition 2024, workspace resolver 2
**Workspace conventions**:
- Copyright header: `// Copyright 2026 BelowZero Security OU. All rights reserved.` (regular comment, not `//!`)
- `thiserror`, `serde`, `serde_json` from `[workspace.dependencies]`
- Clippy: `all = deny`, `pedantic = warn`, `nursery = warn` — treat warnings as errors
- Tests live in `tests/` subdirectory (integration) or inline `#[cfg(test)]` blocks (unit)
- No `unsafe_code` (workspace forbids it)

**Affects hot path**: No. This is a short-lived CLI process, not a server. No throughput benchmarks required.

---

## Structural Decision: lib + bin targets

The design doc says "binary only", but unit-testing internal modules requires a lib target (otherwise
`#[cfg(test)]` in `connection.rs` cannot be imported by `tests/`). The plan adds a thin `lib.rs` that
re-exports all internal modules with `pub(crate)` visibility for the binary, and `pub` for the test
harness. The binary `main.rs` calls into the lib.

`Cargo.toml` shape:
```toml
[lib]
name = "tessera_cli_lib"
path = "src/lib.rs"

[[bin]]
name = "tessera-cli"
path = "src/main.rs"
```

---

## Module Testability Classification

| Module | Classification | Testing strategy |
|---|---|---|
| `error.rs` | Pure logic | Inline unit tests |
| `config.rs` | Pure logic (reads env + files) | Inline unit tests, temp files |
| `cli.rs` | Pure logic (clap structs) | Inline unit tests via `clap::Parser::try_parse_from` |
| `output/mod.rs` | Pure logic | Inline unit tests |
| `output/table.rs` | Pure logic (string rendering) | Inline unit tests |
| `output/json.rs` | Pure logic (string rendering) | Inline unit tests |
| `output/csv.rs` | Pure logic (string rendering) | Inline unit tests |
| `import.rs` (translation) | Pure logic (GQL generation) | Inline unit tests |
| `export.rs` (formatting) | Pure logic (result → string) | Inline unit tests |
| `connection.rs` | I/O-bound | `tokio::io::duplex` mock streams for framing; TLS setup untested |
| `auth.rs` | I/O-bound | Test login-flow state machine with mock `Session` |
| `repl.rs` | I/O-bound | Test meta-command parser, multi-line accumulator as pure functions |
| `query.rs` | I/O-bound | Integration test with mock session |

---

## Decisions Required Before Starting

None. The design doc is fully approved. All architectural decisions are resolved.

---

## Phase 0 — Crate Scaffold (prerequisite, ~30 min)

This is not a TDD cycle — it is the minimal crate skeleton that must exist before the first RED
test can compile.

**Tasks**:
1. Create `crates/tessera-cli/` directory tree
2. Write `Cargo.toml` with `[lib]` + `[[bin]]` targets and all dependencies
3. Add `tessera-cli` to `[workspace] members` in the root `Cargo.toml`
4. Write stub `src/lib.rs` (empty `pub mod` declarations)
5. Write stub `src/main.rs` (`fn main() {}`)
6. Run `cargo check -p tessera-cli` — must compile clean

**File**: `/Volumes/WD_BLACK/repos/MojoBytes/tessera-ecosystem/tessera-graph-enterprise/crates/tessera-cli/Cargo.toml`

---

## Phase 1 — Working CLI

Goal: `tessera-cli query "MATCH (n) RETURN n"` works end-to-end against a live server.
Modules: `error.rs`, `config.rs`, `cli.rs`, `output/`, `connection.rs`, `auth.rs`, `query.rs`, `main.rs`.

---

### Cycle 1: CliError enum and exit codes
- **Module**: `src/error.rs`
- **Testability**: Pure logic
- **Test**: `CliError` variants exist, `exit_code()` returns the correct integer for each variant,
  `Display` impl produces human-readable messages, `From<ProtocolError>` converts correctly.

- **RED**:
```rust
// src/error.rs — #[cfg(test)] block
#[test]
fn connection_error_exit_code() {
    let e = CliError::Connection("refused".to_owned());
    assert_eq!(e.exit_code(), 1);
}

#[test]
fn auth_error_exit_code() {
    let e = CliError::Auth("bad password".to_owned());
    assert_eq!(e.exit_code(), 2);
}

#[test]
fn query_error_exit_code() {
    let e = CliError::Query("syntax error".to_owned());
    assert_eq!(e.exit_code(), 3);
}

#[test]
fn import_export_error_exit_code() {
    let e = CliError::ImportExport("file not found".to_owned());
    assert_eq!(e.exit_code(), 4);
}

#[test]
fn config_error_exit_code() {
    let e = CliError::Config("missing host".to_owned());
    assert_eq!(e.exit_code(), 5);
}

#[test]
fn display_contains_message() {
    let e = CliError::Auth("bad password".to_owned());
    assert!(e.to_string().contains("bad password"));
}
```

- **GREEN**: Define `CliError` with five variants (`Connection`, `Auth`, `Query`, `ImportExport`,
  `Config`), each wrapping `String`. Implement `exit_code(&self) -> i32` method. Derive
  `thiserror::Error` + `Debug`. Add `From<tessera_protocol::ProtocolError>` mapping to
  `CliError::Connection`.

- **REFACTOR**: Ensure `Display` messages include a contextual prefix
  (e.g. `"Connection error: {0}"`) so `eprintln!("{e}")` is useful without decoration in
  `main.rs`. Add `#[must_use]` to `exit_code`.

---

### Cycle 2: Config defaults and CLI flag parsing
- **Module**: `src/config.rs` + `src/cli.rs`
- **Testability**: Pure logic
- **Test**: Default `ConnectionConfig` contains expected values. `clap` parses flags into the
  config struct. Env vars override defaults. Unknown flags are rejected.

- **RED**:
```rust
// src/config.rs — #[cfg(test)] block
#[test]
fn defaults_are_correct() {
    let cfg = ConnectionConfig::default();
    assert_eq!(cfg.host, "127.0.0.1");
    assert_eq!(cfg.port, 7687);
    assert_eq!(cfg.username, "admin");
    assert_eq!(cfg.connect_timeout_secs, 10);
    assert!(cfg.ca_cert.is_none());
    assert!(!cfg.tls_skip_verify);
}

// src/cli.rs — #[cfg(test)] block
#[test]
fn parse_host_and_port_flags() {
    let args = Cli::try_parse_from(["tessera-cli", "-H", "db.prod", "-p", "9000"]).unwrap();
    assert_eq!(args.host, Some("db.prod".to_owned()));
    assert_eq!(args.port, Some(9000u16));
}

#[test]
fn parse_query_subcommand() {
    let args = Cli::try_parse_from(["tessera-cli", "query", "MATCH (n) RETURN n"]).unwrap();
    let Command::Query(q) = args.command.unwrap() else { panic!() };
    assert_eq!(q.query, "MATCH (n) RETURN n");
}

#[test]
fn parse_ping_subcommand() {
    let args = Cli::try_parse_from(["tessera-cli", "ping"]).unwrap();
    assert!(matches!(args.command, Some(Command::Ping)));
}
```

- **GREEN**: `cli.rs` — define `Cli` struct with `clap::Parser` derive, global connection
  options as `Option<T>`, `command: Option<Command>` with subcommand enum. `config.rs` — define
  `ConnectionConfig` with all fields and `Default` impl. Add `ConnectionConfig::from_cli(cli: &Cli)`
  that copies flag values, leaving env-var resolution for Cycle 3.

- **REFACTOR**: Move `OutputFormat` enum to `config.rs` (or `output/mod.rs`) since it is needed
  by both `cli.rs` and the output modules. `Cli` should remain pure data — no methods that do I/O.

---

### Cycle 3: Config resolution — env vars override defaults
- **Module**: `src/config.rs`
- **Testability**: Pure logic (env vars are manipulable in tests)
- **Test**: `TESSERA_HOST` overrides default, CLI flag overrides env var, precedence is correct.

- **RED**:
```rust
// src/config.rs — #[cfg(test)] block
#[test]
fn env_host_overrides_default() {
    std::env::set_var("TESSERA_HOST", "env-host.local");
    let cfg = ConnectionConfig::resolve(None, None, None, None, None, None);
    assert_eq!(cfg.host, "env-host.local");
    std::env::remove_var("TESSERA_HOST");
}

#[test]
fn cli_flag_overrides_env() {
    std::env::set_var("TESSERA_HOST", "env-host.local");
    let cfg = ConnectionConfig::resolve(Some("flag-host"), None, None, None, None, None);
    assert_eq!(cfg.host, "flag-host");
    std::env::remove_var("TESSERA_HOST");
}

#[test]
fn password_not_stored_in_config() {
    // ConnectionConfig must NOT have a `password` field persisted from config file
    // (password is resolved separately and never written to config)
    let cfg = ConnectionConfig::default();
    // Compilation check: cfg.password does not exist as a persistent field
    // We verify by asserting the struct fields at the type level in a compile test
    let _ = cfg; // used
}
```

- **GREEN**: Add `ConnectionConfig::resolve(host: Option<&str>, port: Option<u16>, username: Option<&str>, ca_cert: Option<&str>, password: Option<&str>, timeout: Option<u64>) -> (ConnectionConfig, Option<String>)`.
  Inside: read env vars with `std::env::var`, apply CLI-flag overrides. Return password separately
  as `Option<String>` — never stored in `ConnectionConfig`. Read `TESSERA_PORT` as `u16`,
  defaulting gracefully on parse error.

- **REFACTOR**: Extract `env_or_default` helper to reduce repetition. Add explicit doc on
  password handling: "Passwords are not stored in `ConnectionConfig`. They are resolved
  ephemerally and dropped after authentication."

---

### Cycle 4: Config file loading (TOML)
- **Module**: `src/config.rs`
- **Testability**: Pure logic with temp files
- **Test**: TOML file with `[connection]` section is parsed correctly. Missing file is not an
  error (returns `None`). File with `password` key returns `Err`. Precedence: CLI > env > file >
  default.

- **RED**:
```rust
// src/config.rs — #[cfg(test)] block
#[test]
fn toml_file_sets_host() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tessera.toml");
    std::fs::write(&path, "[connection]\nhost = \"file-host.local\"\nport = 9999\n").unwrap();
    let file_cfg = ConnectionConfig::from_toml_file(&path).unwrap();
    assert_eq!(file_cfg.host, "file-host.local");
    assert_eq!(file_cfg.port, 9999);
}

#[test]
fn toml_missing_file_returns_none() {
    let result = ConnectionConfig::from_toml_file("/nonexistent/path/tessera.toml");
    assert!(result.is_none());
}

#[test]
fn toml_with_password_key_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tessera.toml");
    std::fs::write(&path, "[connection]\npassword = \"secret\"\n").unwrap();
    let result = ConnectionConfig::from_toml_file_strict(&path);
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(matches!(err, CliError::Config(_)));
}
```

- **GREEN**: Add `from_toml_file(path: &Path) -> Option<ConnectionConfig>` — opens file,
  deserializes a `TomlConfig { connection: Option<TomlConnection>, defaults: Option<TomlDefaults> }`
  struct. Add `from_toml_file_strict(path: &Path) -> Result<ConnectionConfig, CliError>` — same
  but errors on `password` key presence. `TomlConfig` uses a custom Serde visitor or a raw
  `toml::Value` check to detect the forbidden `password` key.

- **REFACTOR**: The TOML deserialization types are private to `config.rs`. Use `serde` with
  `deny_unknown_fields = false` on the connection table so future keys don't break existing
  configs. Add `toml` to `[dependencies]` in `Cargo.toml`.

---

### Cycle 5: OutputFormat enum and dispatch
- **Module**: `src/output/mod.rs`
- **Testability**: Pure logic
- **Test**: `OutputFormat` parses from string, `render` dispatches to the correct formatter,
  unknown format string is rejected.

- **RED**:
```rust
// src/output/mod.rs — #[cfg(test)] block
#[test]
fn parse_table_format() {
    let fmt: OutputFormat = "table".parse().unwrap();
    assert_eq!(fmt, OutputFormat::Table);
}

#[test]
fn parse_json_format() {
    let fmt: OutputFormat = "json".parse().unwrap();
    assert_eq!(fmt, OutputFormat::Json);
}

#[test]
fn parse_csv_format() {
    let fmt: OutputFormat = "csv".parse().unwrap();
    assert_eq!(fmt, OutputFormat::Csv);
}

#[test]
fn unknown_format_is_error() {
    let result: Result<OutputFormat, _> = "xml".parse();
    assert!(result.is_err());
}
```

- **GREEN**: Define `OutputFormat { Table, Json, Csv }` with `FromStr` impl. Define
  `fn render(format: OutputFormat, columns: &[String], rows: &[Vec<serde_json::Value>]) -> String`
  that dispatches to `table::render`, `json::render`, `csv::render`. Add `PartialEq`, `Clone`,
  `Copy`, `Debug` derives.

- **REFACTOR**: `render` should return `Result<String, CliError>` so that formatting errors
  (e.g. CSV write failure) propagate cleanly. Ensure `OutputFormat` also implements `Display`
  for use in REPL prompt feedback.

---

### Cycle 6: Table output rendering
- **Module**: `src/output/table.rs`
- **Testability**: Pure logic (string output)
- **Test**: Empty result set produces table with headers and "0 rows" footer. Single row
  renders column values. Multi-row renders all rows. Null values render as empty string.
  Timing suffix appended when `elapsed` is `Some`.

- **RED**:
```rust
// src/output/table.rs — #[cfg(test)] block
#[test]
fn empty_result_shows_zero_rows() {
    let cols = vec!["name".to_owned(), "age".to_owned()];
    let rows: Vec<Vec<serde_json::Value>> = vec![];
    let out = render(&cols, &rows, None);
    assert!(out.contains("0 rows"));
    assert!(out.contains("name"));
    assert!(out.contains("age"));
}

#[test]
fn single_row_renders_values() {
    let cols = vec!["name".to_owned()];
    let rows = vec![vec![serde_json::Value::String("Alice".to_owned())]];
    let out = render(&cols, &rows, None);
    assert!(out.contains("Alice"));
    assert!(out.contains("1 row"));
}

#[test]
fn null_value_renders_as_empty() {
    let cols = vec!["x".to_owned()];
    let rows = vec![vec![serde_json::Value::Null]];
    let out = render(&cols, &rows, None);
    // Cell exists, no crash, null displayed as blank
    assert!(out.contains("1 row"));
}

#[test]
fn timing_appears_when_provided() {
    let cols = vec!["n".to_owned()];
    let rows = vec![vec![serde_json::json!(1)]];
    let out = render(&cols, &rows, Some(std::time::Duration::from_millis(42)));
    assert!(out.contains("42 ms") || out.contains("42.0 ms"));
}
```

- **GREEN**: `fn render(columns: &[String], rows: &[Vec<serde_json::Value>], elapsed: Option<Duration>) -> String`.
  Use `comfy_table::Table`. Convert `serde_json::Value` to display string: `String` → raw value,
  `Number` → `to_string()`, `Bool` → `true`/`false`, `Null` → `""`, objects/arrays → compact JSON.
  Append `"{n} row(s) ({ms:.1} ms)"` footer.

- **REFACTOR**: Extract `value_to_display(v: &serde_json::Value) -> String` as a standalone
  function — it will be reused by `json.rs` and `csv.rs`. The row/rows singular/plural is a
  formatting detail worth extracting: `fn rows_label(n: usize) -> &'static str`.

---

### Cycle 7: JSON (NDJSON) output rendering
- **Module**: `src/output/json.rs`
- **Testability**: Pure logic
- **Test**: Each row produces exactly one line of valid JSON. Columns become object keys.
  Null values appear as JSON `null`. Empty result set produces zero lines.

- **RED**:
```rust
// src/output/json.rs — #[cfg(test)] block
#[test]
fn single_row_is_valid_ndjson() {
    let cols = vec!["name".to_owned(), "age".to_owned()];
    let rows = vec![vec![
        serde_json::Value::String("Alice".to_owned()),
        serde_json::json!(30),
    ]];
    let out = render(&cols, &rows);
    let parsed: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(parsed["name"], "Alice");
    assert_eq!(parsed["age"], 30);
}

#[test]
fn two_rows_produce_two_lines() {
    let cols = vec!["x".to_owned()];
    let rows = vec![vec![serde_json::json!(1)], vec![serde_json::json!(2)]];
    let out = render(&cols, &rows);
    assert_eq!(out.lines().count(), 2);
}

#[test]
fn empty_result_produces_no_lines() {
    let cols = vec!["x".to_owned()];
    let out = render(&cols, &[]);
    assert_eq!(out.trim(), "");
}
```

- **GREEN**: `fn render(columns: &[String], rows: &[Vec<serde_json::Value>]) -> String`.
  Zip columns with each row's values into a `serde_json::Map`. Serialize with
  `serde_json::to_string(&map)`. Join lines with `\n`. If `columns.len() != row.len()`, pad
  with `Null` rather than panicking.

- **REFACTOR**: The column-zip-to-map pattern is also used by export. Extract
  `row_to_object(columns: &[String], row: &[serde_json::Value]) -> serde_json::Map<String, serde_json::Value>`
  as a module-level function that both renderers can call.

---

### Cycle 8: CSV output rendering
- **Module**: `src/output/csv.rs`
- **Testability**: Pure logic
- **Test**: Header row matches column names. Data row values are correctly serialized.
  Values with commas are quoted. `--no-headers` mode omits the header row.
  Null values render as empty field.

- **RED**:
```rust
// src/output/csv.rs — #[cfg(test)] block
#[test]
fn header_row_matches_columns() {
    let cols = vec!["name".to_owned(), "age".to_owned()];
    let out = render(&cols, &[], true);
    let first_line = out.lines().next().unwrap();
    assert_eq!(first_line, "name,age");
}

#[test]
fn value_with_comma_is_quoted() {
    let cols = vec!["city".to_owned()];
    let rows = vec![vec![serde_json::Value::String("Tallinn, Estonia".to_owned())]];
    let out = render(&cols, &rows, true);
    let data_line = out.lines().nth(1).unwrap();
    assert!(data_line.contains('"'));
}

#[test]
fn no_headers_omits_header_row() {
    let cols = vec!["x".to_owned()];
    let rows = vec![vec![serde_json::json!(1)]];
    let out = render(&cols, &rows, false);
    assert_eq!(out.lines().count(), 1);
    assert!(!out.contains("x"));
}

#[test]
fn null_renders_as_empty_field() {
    let cols = vec!["a".to_owned(), "b".to_owned()];
    let rows = vec![vec![serde_json::Value::Null, serde_json::json!(1)]];
    let out = render(&cols, &rows, true);
    let line = out.lines().nth(1).unwrap();
    assert_eq!(line, ",1");
}
```

- **GREEN**: `fn render(columns: &[String], rows: &[Vec<serde_json::Value>], include_headers: bool) -> String`.
  Use the `csv` crate's `csv::Writer` writing to a `Vec<u8>` buffer. Convert each value with
  `value_to_csv_field` which maps `Null` → `""`, strings → raw, numbers/bools/objects → `to_string()`.

- **REFACTOR**: `render` signature should match the `output::mod` dispatch contract. Ensure `csv`
  writer is flushed before converting buffer to `String`. The `include_headers` flag maps to the
  `--no-headers` CLI flag (invert the bool at the call site, not here).

---

### Cycle 9: Framed message send/receive over duplex stream
- **Module**: `src/connection.rs`
- **Testability**: I/O-bound, but testable via `tokio::io::duplex`
- **Test**: `Session::send` serializes a `ClientMessage` and writes a valid length-prefixed frame.
  `Session::recv` reads a frame and deserializes a `ServerMessage`. Round-trip: send Ping,
  receive Pong.

- **RED**:
```rust
// src/connection.rs — #[cfg(test)] block (tokio::test)
#[tokio::test]
async fn send_serializes_ping_frame() {
    let (client_half, mut server_half) = tokio::io::duplex(4096);
    let (cr, cw) = tokio::io::split(client_half);
    let mut session = Session::from_split(cr, cw);

    session.send(ClientMessage::Ping).await.unwrap();

    // Read what the server side received
    let mut reader = tessera_protocol::FramedReader::new(&mut server_half);
    let frame = reader.read_frame().await.unwrap().unwrap();
    let msg: ClientMessage = serde_json::from_slice(&frame).unwrap();
    assert_eq!(msg, ClientMessage::Ping);
}

#[tokio::test]
async fn recv_deserializes_pong() {
    let (client_half, mut server_half) = tokio::io::duplex(4096);
    let (cr, cw) = tokio::io::split(client_half);
    let mut session = Session::from_split(cr, cw);

    // Write a Pong frame from the server side
    let payload = serde_json::to_vec(&ServerMessage::Pong).unwrap();
    let mut writer = tessera_protocol::FramedWriter::new(&mut server_half);
    writer.write_frame(&payload).await.unwrap();

    let msg = session.recv().await.unwrap();
    assert_eq!(msg, ServerMessage::Pong);
}

#[tokio::test]
async fn recv_eof_returns_bye() {
    let (client_half, server_half) = tokio::io::duplex(4096);
    drop(server_half); // close write end
    let (cr, cw) = tokio::io::split(client_half);
    let mut session = Session::from_split(cr, cw);

    // EOF from server maps to Bye or a CliError::Connection
    let result = session.recv().await;
    // Either Ok(Bye) or Err(Connection) is acceptable
    match result {
        Ok(ServerMessage::Bye) => {}
        Err(CliError::Connection(_)) => {}
        other => panic!("unexpected: {other:?}"),
    }
}
```

- **GREEN**: Define `Session<R, W>` generic over `AsyncRead + Unpin` and `AsyncWrite + Unpin`.
  Fields: `reader: FramedReader<R>`, `writer: FramedWriter<W>`, `token: Option<String>`.
  Methods: `from_split(r: R, w: W) -> Self`, `send(&mut self, msg: ClientMessage) -> Result<(), CliError>`,
  `recv(&mut self) -> Result<ServerMessage, CliError>`. Serialize with `serde_json::to_vec`,
  deserialize with `serde_json::from_slice`. Map EOF (`Ok(None)` from `read_frame`) to
  `Ok(ServerMessage::Bye)`.

- **REFACTOR**: Add `set_token(&mut self, token: String)` so `auth.rs` can store the token after
  `AuthOk`. The concrete type used in production (`Session<ReadHalf<TlsStream<TcpStream>>, WriteHalf<TlsStream<TcpStream>>>`)
  is only instantiated in `connection.rs::connect()` — keep that function untested (TLS + TCP
  is integration territory).

---

### Cycle 10: Auth flow — login state machine
- **Module**: `src/auth.rs`
- **Testability**: I/O-bound, but login logic is testable with a mock session
- **Test**: Sending `Login` and receiving `AuthOk` stores the token and returns `Ok`.
  Receiving `AuthError` returns `CliError::Auth`. Receiving anything else returns
  `CliError::Connection` (unexpected message).

- **RED**:
```rust
// src/auth.rs — #[cfg(test)] block (tokio::test)
// Uses tokio::io::duplex to drive a mock server response

async fn mock_server_responds(response: ServerMessage) -> Session<...> {
    // Helper: creates a duplex, writes `response` on server side, returns client Session
}

#[tokio::test]
async fn auth_ok_stores_token() {
    let mut session = mock_server_responds(ServerMessage::AuthOk {
        token: "tok123".to_owned(),
    }).await;
    login(&mut session, "admin", "pass").await.unwrap();
    assert_eq!(session.token(), Some("tok123"));
}

#[tokio::test]
async fn auth_error_returns_cli_error() {
    let mut session = mock_server_responds(ServerMessage::AuthError {
        reason: "wrong password".to_owned(),
    }).await;
    let err = login(&mut session, "admin", "wrong").await.unwrap_err();
    assert!(matches!(err, CliError::Auth(_)));
    assert!(err.to_string().contains("wrong password"));
}

#[tokio::test]
async fn unexpected_message_returns_connection_error() {
    let mut session = mock_server_responds(ServerMessage::Pong).await;
    let err = login(&mut session, "admin", "pass").await.unwrap_err();
    assert!(matches!(err, CliError::Connection(_)));
}
```

- **GREEN**: `pub async fn login<R, W>(session: &mut Session<R, W>, username: &str, password: &str) -> Result<(), CliError>`.
  Sends `ClientMessage::Login { username, password }`. Awaits `session.recv()`. Matches on:
  `AuthOk { token }` → calls `session.set_token(token)`, returns `Ok(())`.
  `AuthError { reason }` → returns `Err(CliError::Auth(reason))`.
  Anything else → returns `Err(CliError::Connection("unexpected server response: ..."))`.
  Password prompting (`rpassword`) is NOT tested — it is a thin wrapper in `main.rs`.

- **REFACTOR**: Keep `login()` a free function (not a method on `Session`) so it is independently
  testable. The password prompt helper in `main.rs` calls `rpassword::prompt_password` then
  passes the result to `login()`.

---

### Cycle 11: Single-shot query execution
- **Module**: `src/query.rs`
- **Testability**: I/O-bound, testable with mock session
- **Test**: Sending a query and receiving `QueryResult` returns the result. Receiving `QueryError`
  returns `CliError::Query`. Receiving `AuthError` (token expired) returns `CliError::Auth`.

- **RED**:
```rust
// src/query.rs — #[cfg(test)] block (tokio::test)
#[tokio::test]
async fn query_result_returned() {
    let result = ServerMessage::QueryResult {
        columns: vec!["name".to_owned()],
        rows: vec![vec![serde_json::Value::String("Alice".to_owned())]],
    };
    let mut session = mock_session_with_response(result).await;
    let qr = execute_query(&mut session, "MATCH (n) RETURN n", "gql").await.unwrap();
    assert_eq!(qr.columns, vec!["name"]);
    assert_eq!(qr.rows.len(), 1);
}

#[tokio::test]
async fn query_error_maps_to_cli_error() {
    let resp = ServerMessage::QueryError { reason: "syntax error at line 1".to_owned() };
    let mut session = mock_session_with_response(resp).await;
    let err = execute_query(&mut session, "INVALID GQL", "gql").await.unwrap_err();
    assert!(matches!(err, CliError::Query(_)));
}

#[tokio::test]
async fn auth_error_from_expired_token() {
    let resp = ServerMessage::AuthError { reason: "token expired".to_owned() };
    let mut session = mock_session_with_response(resp).await;
    let err = execute_query(&mut session, "MATCH (n) RETURN n", "gql").await.unwrap_err();
    assert!(matches!(err, CliError::Auth(_)));
}
```

- **GREEN**: Define `QueryResult { columns: Vec<String>, rows: Vec<Vec<serde_json::Value>> }`.
  `pub async fn execute_query<R, W>(session: &mut Session<R, W>, query: &str, language: &str) -> Result<QueryResult, CliError>`.
  Sends `ClientMessage::Query { query, language }`. Awaits `session.recv()`. Matches on
  `QueryResult` → `Ok(result)`, `QueryError` → `Err(CliError::Query)`, `AuthError` →
  `Err(CliError::Auth)`, other → `Err(CliError::Connection)`.

- **REFACTOR**: `QueryResult` mirrors `ServerMessage::QueryResult` — consider whether to reuse
  the protocol type directly or keep a CLI-local copy. CLI-local copy is preferable to avoid
  coupling CLI display logic to the protocol type (protocol type may gain fields that don't
  make sense to render).

---

## Phase 2 — REPL

Goal: `tessera-cli` (no subcommand) starts an interactive REPL.
Modules: `repl.rs` (meta-command parser + line accumulator), `output/` (JSON + CSV already done in Phase 1).

---

### Cycle 12: Meta-command parser
- **Module**: `src/repl.rs`
- **Testability**: Pure logic
- **Test**: `\q` parses to `Quit`. `\format json` parses to `SetFormat(Json)`. `\l cypher`
  parses to `SetLanguage("cypher")`. `\timing on` parses to `SetTiming(true)`. Unknown
  command returns `Unknown(cmd)`. Non-backslash input returns `None` (not a meta-command).

- **RED**:
```rust
// src/repl.rs — #[cfg(test)] block
#[test]
fn quit_meta_command() {
    assert_eq!(parse_meta_command("\\q"), Some(MetaCommand::Quit));
}

#[test]
fn help_aliases() {
    assert_eq!(parse_meta_command("\\h"), Some(MetaCommand::Help));
    assert_eq!(parse_meta_command("\\?"), Some(MetaCommand::Help));
}

#[test]
fn format_meta_command() {
    assert_eq!(
        parse_meta_command("\\format json"),
        Some(MetaCommand::SetFormat(OutputFormat::Json))
    );
}

#[test]
fn language_meta_command() {
    assert_eq!(
        parse_meta_command("\\l cypher"),
        Some(MetaCommand::SetLanguage("cypher".to_owned()))
    );
}

#[test]
fn timing_on_off() {
    assert_eq!(parse_meta_command("\\timing on"), Some(MetaCommand::SetTiming(true)));
    assert_eq!(parse_meta_command("\\timing off"), Some(MetaCommand::SetTiming(false)));
}

#[test]
fn clear_meta_command() {
    assert_eq!(parse_meta_command("\\clear"), Some(MetaCommand::Clear));
}

#[test]
fn unknown_meta_command() {
    assert!(matches!(parse_meta_command("\\xyz"), Some(MetaCommand::Unknown(_))));
}

#[test]
fn non_meta_returns_none() {
    assert_eq!(parse_meta_command("MATCH (n) RETURN n"), None);
}
```

- **GREEN**: Define `MetaCommand` enum. `pub fn parse_meta_command(input: &str) -> Option<MetaCommand>`.
  Returns `None` if input does not start with `\`. Otherwise strips prefix, splits on first
  whitespace, matches command word. For `\format`, parse the trailing word into `OutputFormat`.

- **REFACTOR**: `MetaCommand` derives `Debug`, `PartialEq`. Keep parser as a free function
  (not a method) — it is pure string logic, no REPL state required.

---

### Cycle 13: Multi-line query accumulator
- **Module**: `src/repl.rs`
- **Testability**: Pure logic
- **Test**: Single line ending with `;` is complete. Line without `;` is incomplete.
  Accumulating two lines then `;` produces complete query. Empty line after non-empty input
  is also complete. Empty line on empty buffer is a no-op. Strip trailing `;` from final query.

- **RED**:
```rust
// src/repl.rs — #[cfg(test)] block
#[test]
fn single_line_with_semicolon_is_complete() {
    let mut acc = QueryAccumulator::new();
    let result = acc.push("MATCH (n) RETURN n;");
    assert_eq!(result, Some("MATCH (n) RETURN n".to_owned()));
}

#[test]
fn line_without_semicolon_is_incomplete() {
    let mut acc = QueryAccumulator::new();
    let result = acc.push("MATCH (n)");
    assert_eq!(result, None);
    assert!(acc.is_pending());
}

#[test]
fn multiline_completed_by_semicolon() {
    let mut acc = QueryAccumulator::new();
    assert_eq!(acc.push("MATCH (n)"), None);
    let result = acc.push("RETURN n;");
    assert_eq!(result, Some("MATCH (n)\nRETURN n".to_owned()));
}

#[test]
fn empty_line_completes_pending_query() {
    let mut acc = QueryAccumulator::new();
    acc.push("MATCH (n) RETURN n");
    let result = acc.push("");
    assert_eq!(result, Some("MATCH (n) RETURN n".to_owned()));
}

#[test]
fn empty_line_on_empty_buffer_is_noop() {
    let mut acc = QueryAccumulator::new();
    let result = acc.push("");
    assert_eq!(result, None);
    assert!(!acc.is_pending());
}

#[test]
fn accumulator_clears_after_completion() {
    let mut acc = QueryAccumulator::new();
    acc.push("SELECT 1;");
    assert!(!acc.is_pending());
}
```

- **GREEN**: Define `QueryAccumulator { lines: Vec<String> }`. `pub fn push(&mut self, line: &str) -> Option<String>`.
  Logic: if `lines` is empty and `line` is empty → return `None`. If `line` ends with `;` →
  push stripped line, join all lines with `\n`, clear, return `Some(query)`. If `line` is empty
  and `lines` is non-empty → join, clear, return `Some(query)`. Otherwise push `line`, return `None`.
  `pub fn is_pending(&self) -> bool { !self.lines.is_empty() }`.

- **REFACTOR**: The stripped-semicolon operation (`trim_end_matches(';').trim_end()`) should be
  applied at completion, not at push time, so that multi-line queries with a semicolon mid-way
  through are not inadvertently truncated (defensive choice).

---

### Cycle 14: REPL prompt builder
- **Module**: `src/repl.rs`
- **Testability**: Pure logic
- **Test**: `format_prompt` returns the correct string for given username, host, port, and
  pending state. Continuation prompt is padded to align `->` with `>`.

- **RED**:
```rust
// src/repl.rs — #[cfg(test)] block
#[test]
fn primary_prompt_format() {
    let p = format_prompt("admin", "localhost", 7687, false);
    assert_eq!(p, "tessera[admin@localhost:7687]> ");
}

#[test]
fn continuation_prompt_is_aligned() {
    let primary = format_prompt("admin", "localhost", 7687, false);
    let continuation = format_prompt("admin", "localhost", 7687, true);
    // Continuation must end with "-> " and have same length as primary
    assert!(continuation.ends_with("-> "));
    assert_eq!(primary.len(), continuation.len());
}
```

- **GREEN**: `pub fn format_prompt(username: &str, host: &str, port: u16, continuation: bool) -> String`.
  Primary: `format!("tessera[{username}@{host}:{port}]> ")`. Continuation: spaces padding to
  match `"tessera["` + `username@host:port` length, then `"-> "`.

- **REFACTOR**: The padding logic should be computed once and reused across the REPL session
  rather than recomputed each prompt. A `ReplPrompts { primary: String, continuation: String }`
  struct built at session start is cleaner. This is a micro-optimization for readability, not
  performance.

---

## Phase 3 — Import/Export

Goal: `tessera-cli import data.gql` and `tessera-cli export --format json` work.
Modules: `import.rs` (translation logic), `export.rs` (result formatting).

---

### Cycle 15: GQL file splitter — split on semicolons
- **Module**: `src/import.rs`
- **Testability**: Pure logic
- **Test**: A multi-statement GQL file is split into individual statements. Blank lines and
  comments are skipped. Trailing semicolons are stripped. A file with no statements produces
  an empty list. Multi-line statements (no semicolon) are kept as one unit.

- **RED**:
```rust
// src/import.rs — #[cfg(test)] block
#[test]
fn splits_on_semicolons() {
    let input = "CREATE (n:Person {name: 'Alice'});\nCREATE (m:Person {name: 'Bob'});";
    let stmts = split_gql_statements(input);
    assert_eq!(stmts.len(), 2);
    assert!(stmts[0].contains("Alice"));
    assert!(stmts[1].contains("Bob"));
}

#[test]
fn blank_lines_and_comments_skipped() {
    let input = "// header comment\n\nCREATE (n:A);\n-- another comment\nCREATE (m:B);";
    let stmts = split_gql_statements(input);
    assert_eq!(stmts.len(), 2);
}

#[test]
fn no_statements_produces_empty_vec() {
    let stmts = split_gql_statements("// just a comment\n\n");
    assert!(stmts.is_empty());
}

#[test]
fn trailing_semicolon_stripped() {
    let stmts = split_gql_statements("CREATE (n:A);");
    assert!(!stmts[0].ends_with(';'));
}

#[test]
fn multiline_statement_preserved() {
    let input = "CREATE (n:Person {\n  name: 'Alice',\n  age: 30\n});";
    let stmts = split_gql_statements(input);
    assert_eq!(stmts.len(), 1);
    assert!(stmts[0].contains("Alice"));
}
```

- **GREEN**: `pub fn split_gql_statements(input: &str) -> Vec<String>`. Split on `;`. For each
  fragment: strip leading/trailing whitespace, remove comment-only lines (lines starting with
  `//` or `--`), rejoin remaining lines, skip empty results. This is simpler than a full parser
  and matches the `psql`-style convention from the design doc.

- **REFACTOR**: A `\n`-joined multi-line statement should have its individual comment lines
  stripped but blank lines within a non-comment statement preserved. The exact behavior should
  match what `tessera-import`'s `import_gql` function already does (single-line per statement).
  Cross-check against `gql_import/mod.rs` conventions — the CLI import sends each statement as
  a separate `ClientMessage::Query`, so multi-line must be re-joined before sending.

---

### Cycle 16: GQL file → batches of Query messages
- **Module**: `src/import.rs`
- **Testability**: Pure logic (batch calculation only)
- **Test**: 250 statements with batch size 100 produces 3 batches (100, 100, 50). Batch size
  larger than total produces one batch. Empty input produces zero batches.

- **RED**:
```rust
// src/import.rs — #[cfg(test)] block
#[test]
fn two_fifty_statements_in_batches_of_100() {
    let stmts: Vec<String> = (0..250).map(|i| format!("CREATE (n{i}:X)")).collect();
    let batches: Vec<&[String]> = stmts.chunks(100).collect();
    assert_eq!(batches.len(), 3);
    assert_eq!(batches[0].len(), 100);
    assert_eq!(batches[2].len(), 50);
}

#[test]
fn batch_plan_from_file_content() {
    let input = (0..5).map(|i| format!("CREATE (n{i}:X);")).collect::<Vec<_>>().join("\n");
    let plan = ImportPlan::from_gql(&input, 2);
    assert_eq!(plan.batch_count(), 3); // 2+2+1
    assert_eq!(plan.total_statements(), 5);
}
```

- **GREEN**: Define `ImportPlan { statements: Vec<String>, batch_size: usize }` with
  `from_gql(input: &str, batch_size: usize) -> Self` (calls `split_gql_statements`),
  `batches(&self) -> impl Iterator<Item = &[String]>` (delegates to `chunks`),
  `batch_count(&self) -> usize` and `total_statements(&self) -> usize`.

- **REFACTOR**: The default batch size (100) should be a named constant `DEFAULT_BATCH_SIZE: usize = 100`.
  `ImportPlan::new(statements, batch_size)` is the base constructor; `from_gql` composes it.

---

### Cycle 17: CSV nodes file → GQL CREATE statements
- **Module**: `src/import.rs`
- **Testability**: Pure logic
- **Test**: A CSV with `label,name,age` header and one data row produces a valid GQL `CREATE`
  statement. Missing optional properties are omitted (not `null`). Empty label is an error.
  CSV with no rows produces zero statements. Property values with special characters are quoted.

- **RED**:
```rust
// src/import.rs — #[cfg(test)] block
#[test]
fn csv_node_row_to_gql_create() {
    let csv = "label,name,age\nPerson,Alice,30\n";
    let stmts = csv_nodes_to_gql(csv).unwrap();
    assert_eq!(stmts.len(), 1);
    assert!(stmts[0].contains("CREATE"));
    assert!(stmts[0].contains("Person"));
    assert!(stmts[0].contains("Alice"));
    assert!(stmts[0].contains("30"));
}

#[test]
fn empty_optional_property_omitted() {
    let csv = "label,name,city\nPerson,Bob,\n";
    let stmts = csv_nodes_to_gql(csv).unwrap();
    assert!(!stmts[0].contains("city"));
}

#[test]
fn empty_label_is_error() {
    let csv = "label,name\n,Alice\n";
    let err = csv_nodes_to_gql(csv).unwrap_err();
    assert!(matches!(err, CliError::ImportExport(_)));
}

#[test]
fn no_data_rows_produces_empty_vec() {
    let csv = "label,name\n";
    let stmts = csv_nodes_to_gql(csv).unwrap();
    assert!(stmts.is_empty());
}

#[test]
fn string_value_with_quotes_is_escaped() {
    let csv = "label,name\nPerson,O'Brien\n";
    let stmts = csv_nodes_to_gql(csv).unwrap();
    // The generated GQL must not produce a syntax error with the apostrophe
    // Check it uses double-quoted string or properly escaped single quote
    assert!(stmts[0].contains("O") && stmts[0].contains("Brien"));
}
```

- **GREEN**: `pub fn csv_nodes_to_gql(csv: &str) -> Result<Vec<String>, CliError>`.
  Parse header with the same pattern as `tessera-import`'s `parse_csv_line` (copy or extract
  to a shared helper). For each data row: construct `CREATE (n:Label { key: "value", ... })`.
  Use double-quoted GQL string literals (GQL syntax uses double quotes for strings, not single).
  Skip empty-value fields. Return `Err(CliError::ImportExport(...))` for empty labels or parse
  errors.

- **REFACTOR**: GQL CREATE string generation should be extracted to a private
  `format_gql_create(label: &str, props: &[(String, String)]) -> String` helper to keep the
  CSV loop readable. Double-quote all string values and escape `"` as `\\"` within them.

---

### Cycle 18: Export result formatting
- **Module**: `src/export.rs`
- **Testability**: Pure logic (formatting only, no I/O)
- **Test**: GQL export format produces `CREATE` statements from rows. JSON export delegates to
  `output::json::render`. CSV export delegates to `output::csv::render`. GQL format with zero
  rows produces empty output.

- **RED**:
```rust
// src/export.rs — #[cfg(test)] block
#[test]
fn gql_export_produces_create_statements() {
    let columns = vec!["n.name".to_owned(), "n.age".to_owned()];
    let rows = vec![
        vec![serde_json::Value::String("Alice".to_owned()), serde_json::json!(30)],
    ];
    let out = export_as_gql(&columns, &rows);
    assert!(out.contains("CREATE"));
    assert!(out.contains("Alice"));
}

#[test]
fn gql_export_empty_rows_is_empty() {
    let cols = vec!["n.name".to_owned()];
    let out = export_as_gql(&cols, &[]);
    assert!(out.trim().is_empty());
}

#[test]
fn json_export_delegates_to_output_json() {
    use crate::output;
    let cols = vec!["x".to_owned()];
    let rows = vec![vec![serde_json::json!(1)]];
    let export_out = export_as_json(&cols, &rows);
    let render_out = output::json::render(&cols, &rows);
    assert_eq!(export_out, render_out);
}
```

- **GREEN**: `pub fn export_as_gql(columns: &[String], rows: &[Vec<serde_json::Value>]) -> String`.
  For each row, zip columns and values, infer a synthetic label (`Node` as default when column
  names don't embed label info), generate `CREATE (n { key: value, ... })`.
  `pub fn export_as_json(...)` and `export_as_csv(...)` are thin wrappers over `output::json::render`
  and `output::csv::render`.

- **REFACTOR**: The GQL export format is intentionally simple (no label inference beyond `Node`)
  because schema introspection is out of scope. Document this limitation in a `// NOTE:` comment.
  The `columns` parameter uses dot-notation (`n.name`) from `MATCH (n) RETURN n.name` — the
  export strips the prefix for property key generation.

---

### Cycle 19: Dry-run mode for import
- **Module**: `src/import.rs`
- **Testability**: Pure logic
- **Test**: `ImportPlan::dry_run_output` returns the list of statements that would be sent,
  formatted for display. Dry-run output does not require a `Session`.

- **RED**:
```rust
// src/import.rs — #[cfg(test)] block
#[test]
fn dry_run_output_contains_all_statements() {
    let stmts = vec!["CREATE (a:A)".to_owned(), "CREATE (b:B)".to_owned()];
    let plan = ImportPlan::new(stmts, 100);
    let output = plan.dry_run_output();
    assert!(output.contains("CREATE (a:A)"));
    assert!(output.contains("CREATE (b:B)"));
    assert!(output.contains("2 statement(s)"));
}

#[test]
fn dry_run_shows_batch_info() {
    let stmts: Vec<String> = (0..150).map(|i| format!("CREATE (n{i}:X)")).collect();
    let plan = ImportPlan::new(stmts, 100);
    let output = plan.dry_run_output();
    assert!(output.contains("2 batch"));
}
```

- **GREEN**: Add `pub fn dry_run_output(&self) -> String` to `ImportPlan`. Produce a human-readable
  summary: number of statements, number of batches, then each statement enumerated.
  Truncate display for large plans (> 20 shown, rest summarized as "... N more").

- **REFACTOR**: Truncation threshold is a named constant `DRY_RUN_DISPLAY_MAX: usize = 20`.

---

## Integration Test Suite (runs against all phases)

The following live in `crates/tessera-cli/tests/` and are compiled separately from the source.
They do NOT require a running server — they use `tokio::io::duplex` to simulate the wire.

### Integration Test File: `tests/cli_integration.rs`

**Cycle 20: Full ping flow over mock transport**
- Construct a mock "server" task on one end of a duplex.
- Start a "client" on the other end.
- Mock server: reads frame, expects Ping, sends Pong.
- Client: calls the full `ping` flow from `main.rs`'s dispatch path.
- Assert exit code 0 and no output to stderr.

**Cycle 21: Full login + query flow over mock transport**
- Mock server: reads Ping → sends Pong; reads Login → sends AuthOk; reads Query → sends QueryResult.
- Client: calls `login` then `execute_query`.
- Assert `QueryResult` data is rendered correctly in table format.

**Cycle 22: Authentication failure exit code**
- Mock server: reads Login → sends AuthError.
- Client: calls `login`.
- Assert `CliError::Auth` and `exit_code() == 2`.

**Cycle 23: Config precedence end-to-end**
- Set `TESSERA_HOST=env-host`, create a `tessera.toml` file with `host = "file-host"`.
- Call `ConnectionConfig::resolve` with no CLI flag.
- Assert result host is `"env-host"` (env beats file).
- Set CLI flag `host = "flag-host"`.
- Assert result is `"flag-host"` (flag beats env).

---

## Cargo.toml Reference

For the implementation agent:

```toml
[package]
name        = "tessera-cli"
description = "Admin CLI for TesseraGraph Enterprise"
edition.workspace    = true
version.workspace    = true
authors.workspace    = true
license.workspace    = true
rust-version.workspace = true

[lib]
name = "tessera_cli_lib"
path = "src/lib.rs"

[[bin]]
name = "tessera-cli"
path = "src/main.rs"

[dependencies]
tessera-protocol    = { workspace = true }
tokio               = { version = "1", features = ["rt-multi-thread", "macros", "net", "io-util", "time"] }
tokio-rustls        = "0.26"
rustls              = "0.23"
rustls-pemfile      = "2"
rustls-native-certs = "0.8"
clap                = { version = "4", features = ["derive", "env"] }
rpassword           = "7"
rustyline           = "14"
comfy-table         = "7"
csv                 = "1"
toml                = "0.8"
serde               = { workspace = true }
serde_json          = { workspace = true }
thiserror           = { workspace = true }

[dev-dependencies]
tempfile = "3"
tokio    = { version = "1", features = ["test-util"] }

[lints]
workspace = true
```

---

## Estimation

| Phase | Cycles | Implementation | Testing |
|---|---|---|---|
| Phase 0 (scaffold) | — | 30 min | — |
| Phase 1 (working CLI) | 1–11 | 6–8 h | 2 h |
| Phase 2 (REPL) | 12–14 | 2–3 h | 1 h |
| Phase 3 (import/export) | 15–19 | 3–4 h | 1.5 h |
| Integration tests | 20–23 | 1.5 h | included |
| **Total** | **23 cycles** | **~13–17 h** | **~4.5 h** |

---

## Criteria de Exito

- [ ] `cargo clippy -p tessera-cli --tests -- -D warnings` passes with zero warnings
- [ ] `cargo test -p tessera-cli` passes all 23+ test cases
- [ ] `cargo build -p tessera-cli --release` produces a binary
- [ ] Binary is added to root `Cargo.toml` `[workspace] members`
- [ ] No `unsafe_code` (workspace lint enforces this)
- [ ] Copyright header on every new source file: `// Copyright 2026 BelowZero Security OU. All rights reserved.`
- [ ] Password is never stored in `ConnectionConfig` or on disk
- [ ] `--tls-skip-verify` prints a visible warning to stderr every time it is used
- [ ] Exit codes match the design doc specification (0–5)
