# tessera-cli Design Document

**Created**: 2026-03-20
**Status**: Approved, pending implementation

---

## 1. Overview

CLI admin tool for TesseraGraph Enterprise. Primary interface for interacting with a running server. Think `psql` for PostgreSQL.

**Crate location**: `crates/tessera-cli` inside the workspace (shares `tessera-protocol`).
**Binary only**: `[[bin]]`, no lib target.

---

## 2. Module Structure

```
crates/tessera-cli/src/
├── main.rs          — entry point, dispatch
├── cli.rs           — clap args/commands (pure data, no I/O)
├── config.rs        — connection config: flags > env > config file > defaults
├── connection.rs    — TLS client, Session (send/recv framed messages)
├── auth.rs          — login flow, password prompt (rpassword)
├── repl.rs          — rustyline REPL, multi-line (;), history, meta-commands
├── query.rs         — single-shot query execution
├── import.rs        — file → GQL CREATE batches (client-side translation)
├── export.rs        — query + format output
├── output/
│   ├── mod.rs       — OutputFormat enum, dispatch
│   ├── table.rs     — comfy-table renderer
│   ├── json.rs      — NDJSON output
│   └── csv.rs       — RFC 4180 CSV output
└── error.rs         — CliError enum, exit codes
```

---

## 3. Command-Line Interface

### Top-level

```
tessera-cli [CONNECTION OPTIONS] [COMMAND]
```

No command → REPL starts (like `psql`).

### Connection options (global)

```
-H, --host <HOST>        [default: 127.0.0.1]
-p, --port <PORT>        [default: 7687]
-u, --username <USER>    [default: admin]
    --password <PASS>    (prefer env var or prompt)
    --ca-cert <PATH>     PEM CA certificate for self-signed certs
    --tls-skip-verify    Dev only, prints warning every time
    --connect-timeout <SECS>  [default: 10]
    --format <FORMAT>    table | json | csv [default: table]
```

Password precedence: `--password` > `TESSERA_PASSWORD` env > interactive prompt.

### Subcommands

```
tessera-cli query <QUERY_STRING>
    -l, --language <LANG>    gql | cypher [default: gql]
    --no-headers             Omit headers in table/CSV

tessera-cli exec <FILE>
    -l, --language <LANG>

tessera-cli import <FILE>
    --format <FORMAT>        csv-nodes | csv-edges | json | gql [inferred from extension]
    --dry-run                Print generated queries without executing

tessera-cli export
    --format <FORMAT>        gql | json | csv [default: gql]
    --output <FILE>          Write to file instead of stdout

tessera-cli ping
    Health check. Exit 0 on success.

tessera-cli version
```

### Examples

```bash
tessera-cli -H db.prod.internal -u admin                              # REPL
tessera-cli --format json query "MATCH (n:Person) RETURN n.name"      # scripting
tessera-cli exec schema.gql                                           # file
tessera-cli import data/people.csv --format csv-nodes                 # import
tessera-cli export --format gql --output backup.gql                   # export
tessera-cli ping || exit 1                                            # CI health check
cat people.csv | tessera-cli import - --format csv-nodes              # stdin
```

---

## 4. Configuration Resolution

Precedence: CLI flags > env vars > config file > defaults.

**Env vars**: `TESSERA_HOST`, `TESSERA_PORT`, `TESSERA_USER`, `TESSERA_PASSWORD`, `TESSERA_CA_CERT`

**Config file** (first found): `./tessera.toml` > `~/.config/tessera/tessera.toml` > `~/.tessera.toml`

```toml
[connection]
host = "db.prod.internal"
port = 7687
username = "admin"
ca_cert = "/etc/tessera/ca.pem"

[defaults]
language = "gql"
format = "table"
```

Passwords explicitly NOT stored in config file — CLI refuses to load if `password` key present.

---

## 5. TLS Client

Three modes:

1. **System CA** (default): `rustls-native-certs` loads OS trust store.
2. **Custom CA** (`--ca-cert`): Load PEM as only trusted CA. For internal CAs.
3. **Skip verify** (`--tls-skip-verify`): Prints bold warning to stderr every time. `dangerous_no_verify()` prefix is intentional.

---

## 6. Connection and Auth Flow

```
1. Resolve config (host, port, TLS mode)
2. TCP connect with timeout
3. TLS handshake
4. Split into FramedReader + FramedWriter
5. Send Ping → expect Pong (validate protocol before password prompt)
6. Prompt for password if not provided
7. Send Login { username, password }
8. Receive AuthOk { token } or AuthError
9. Return connected Session
```

Session struct holds `FramedWriter` + `FramedReader` + token. Exposes `send(ClientMessage) -> Result<ServerMessage>`.

---

## 7. REPL Design

**Library**: `rustyline 14`

**Prompt**: `tessera[admin@localhost:7687]> `
**Continuation**: `                        -> ` (aligned with `>`)

**Multi-line**: Query complete when it ends with `;` or empty line after input. No GQL parsing — semicolon convention like `psql`.

**History**: `~/.tessera_history`, max 2000, written after each command.

**Meta-commands**:
```
\q              Quit
\h or \?        Help
\format <fmt>   Change output format
\l <lang>       Change language
\timing on|off  Show query time
\clear          Clear screen
```

**Tab completion**: Phase 2 (GQL keywords only, no server round-trips).

---

## 8. Output Formatting

### Table (comfy-table)
```
+--------+-----+-----------+
| name   | age | city      |
+--------+-----+-----------+
| Alice  | 30  | Tallinn   |
+--------+-----+-----------+
1 row (1.3 ms)
```

### JSON (NDJSON, pipe-friendly)
```json
{"name":"Alice","age":30,"city":"Tallinn"}
```

### CSV (RFC 4180, via `csv` crate)
```
name,age,city
Alice,30,Tallinn
```

---

## 9. Import/Export Strategy

### Import: Client-side translation (Phase 1)

CLI reads file locally, generates GQL CREATE statements, sends as Query messages in batches (default 100).

**Limitation**: No atomicity. Crash = partial import. Documented.

**Future (Phase 2+)**: Server-side `ClientMessage::Import` for atomic bulk loads.

### Export: Query + format

CLI sends `MATCH (n) RETURN n`, formats result as GQL/JSON/CSV.

**Limitation**: Edge export depends on query capabilities.

---

## 10. Error UX

### Exit codes
```
0   Success
1   Connection error (TCP, TLS)
2   Authentication error
3   Query error
4   Import/export error
5   Configuration error
```

### Error format
```
Error: Cannot connect to localhost:7687
  Caused by: Connection refused (os error 111)

Hint: Is the server running? Check TESSERA_HOST and TESSERA_PORT.
```

In REPL: errors print to stderr, return to prompt (no exit).

---

## 11. Dependencies

```toml
[dependencies]
tessera-protocol    = { workspace = true }
tokio               = { version = "1", features = ["rt-multi-thread", "macros", "net", "io-util", "time"] }
tokio-rustls        = "0.26"
rustls              = "0.23"
rustls-pemfile       = "2"
rustls-native-certs = "0.8"
clap                = { version = "4", features = ["derive", "env"] }
rpassword           = "7"
rustyline           = "14"
comfy-table         = "7"
csv                 = "1"
serde               = { workspace = true }
serde_json          = { workspace = true }
thiserror           = { workspace = true }
```

**Not included**: reqwest (TCP not HTTP), indicatif (premature), colored (breaks pipes), tracing (short-lived process).

---

## 12. Implementation Phases

### Phase 1 — Working CLI (2-3 days)
1. `error.rs` — CliError, exit codes
2. `config.rs` — flags + env + defaults (no config file yet)
3. `connection.rs` — TLS client (system roots + custom CA)
4. `auth.rs` — login + rpassword prompt
5. `output/table.rs` — comfy-table renderer
6. `query.rs` — single-shot query
7. `main.rs` + `cli.rs` — clap, dispatch
8. `ping` subcommand

**Deliverable**: `tessera-cli query "MATCH (n) RETURN n"` works end-to-end.

### Phase 2 — REPL (1-2 days)
1. `repl.rs` — rustyline, multi-line, meta-commands
2. History persistence
3. `--tls-skip-verify` mode
4. JSON + CSV output formats
5. Config file (TOML)

**Deliverable**: `tessera-cli` starts interactive REPL.

### Phase 3 — Import/Export (1-2 days)
1. GQL file import (split on `;`, batch send)
2. CSV nodes import (generate CREATE)
3. Export (query + GQL/JSON/CSV formatting)
4. `--dry-run`, stdin support

**Deliverable**: `tessera-cli import data.gql` works.

### Phase 4 — Polish (ongoing)
- `exec` subcommand
- `--verbose` flag
- Tab completion (GQL keywords)
- Shell completion scripts (clap)
- Connection URL shorthand (`tessera://user@host:port`)

---

## 13. Known Limitations (documented, not hidden)

- Import without atomicity (each query independent)
- No schema introspection (protocol doesn't expose it)
- No query cancellation (Ctrl+C closes connection)
- Edge export limited by query capabilities
- `--password` flag visible in `ps aux` (use env var or prompt)
- mTLS client certs not supported yet (server uses `with_no_client_auth`)
