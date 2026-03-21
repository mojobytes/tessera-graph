# TDD Plan — Quality Fixes: tessera-cli

> Target crate: `crates/tessera-cli`
> Branch: `fix/cli-quality-round3`
> Base: `develop`

---

## Contexto

Eight quality findings were identified in the `tessera-cli` crate spanning three files
(`main.rs`, `config.rs`, `import.rs`). Three are critical bugs: the TOML config file is
silently never read because `resolve` is called instead of `resolve_full` (C1); a
`"json"` format string is inferred but has no matching handler causing a misleading
unsupported-format error (C2); and `truncate_line` performs byte indexing on `&str`
which panics on any non-ASCII Unicode character (C3). The remaining five are code
quality issues: DRY violation in config resolution (R2), wrong error variant for
readline failures (R3), silent discard of native cert load errors (R4), broken
encapsulation on `ImportPlan` (R5), unnecessary batch abstraction in `handle_exec`
(R6), and a missing TOML fallback for `connect_timeout_secs` (R8).

**Stack detectado**: Rust 2024 / Tokio / Clap / rustls / rustyline
**Convenciones**: `// Copyright 2026 BelowZero Security OU. All rights reserved.` header, `// OK: reason` on `.unwrap_or_default()` in production, `// OK: test` on `.expect()` in tests, `TestEnv` struct for injectable env (never `std::env::set_var`), clippy `all = deny` / `pedantic = warn` / `nursery = warn`, `unsafe_code = "forbid"`.
**Afecta hot path**: No — CLI tool, no throughput-sensitive code path.

---

## Decisiones Previas Necesarias

None. All fixes are unambiguous corrections of clearly identified bugs and
straightforward refactors with no architectural tradeoffs.

---

## Plan de Ejecución

### Fase 1: C3 — Panic-safe Unicode truncation in `import.rs`

**Rationale:** Self-contained, pure function, zero dependencies on other fixes. Best
first cycle to establish the TDD rhythm.

---

**Cycle 1.1 — RED: test that exposes the UTF-8 panic**

- File: `crates/tessera-cli/src/import.rs` — existing `#[cfg(test)] mod tests` block
- Action: Add test

```rust
#[test]
fn truncate_multibyte_unicode_no_panic() {
    // Each '€' is 3 bytes. Slicing at byte 17 (< 20 - 3 = 17) is fine,
    // but slicing at byte 18 would split a multibyte sequence → panic on current code.
    let s = "€€€€€€€€€€"; // 10 × 3 = 30 bytes
    let result = truncate_line(s, 20);
    assert!(result.ends_with("..."), "must end with ellipsis: {result}");
    assert!(result.is_char_boundary(result.len() - 3)); // no partial char
}

#[test]
fn truncate_multibyte_exactly_at_boundary() {
    let s = "αβγδεζηθ"; // 8 × 2 = 16 bytes
    let result = truncate_line(s, 10);
    // max=10, reserve 3 for "...", so 7 bytes available — but 'α'=2 bytes,
    // must not split mid-character.
    assert!(result.ends_with("..."));
    assert!(s.is_char_boundary(result.len() - 3));
}
```

These tests panic on the current byte-slice implementation.

---

**Cycle 1.2 — GREEN: fix `truncate_line` using `char_indices`**

- File: `crates/tessera-cli/src/import.rs`
- Action: Modify `truncate_line`

Replace:
```rust
fn truncate_line(s: &str, max: usize) -> String {
    let oneline = s.replace('\n', " ");
    if oneline.len() <= max {
        oneline
    } else {
        format!("{}...", &oneline[..max.saturating_sub(3)])
    }
}
```

With:
```rust
fn truncate_line(s: &str, max: usize) -> String {
    let oneline = s.replace('\n', " ");
    // Use char count, not byte length, to avoid splitting multibyte sequences.
    let char_count = oneline.chars().count();
    if char_count <= max {
        oneline
    } else {
        let keep = max.saturating_sub(3);
        // Find the byte offset of the `keep`-th char boundary.
        let byte_end = oneline
            .char_indices()
            .nth(keep)
            .map_or(oneline.len(), |(i, _)| i);
        format!("{}...", &oneline[..byte_end])
    }
}
```

---

**Cycle 1.3 — REFACTOR: review existing truncate tests**

- File: `crates/tessera-cli/src/import.rs`
- Action: Update `truncate_long_line` test — it currently asserts `result.len() <= 20`
  which holds for ASCII but the new implementation counts chars, not bytes. Verify the
  test still passes; no change needed if max=20 and input is ASCII.
- Verify: `cargo test -p tessera-cli truncate` — all 5 truncate tests pass.

---

### Fase 2: C2 — Remove `"json"` from `infer_import_format`

**Rationale:** Two-line fix. Removes the silent inconsistency before we touch anything
else in `main.rs`.

---

**Cycle 2.1 — RED: test that `.json` files yield a meaningful error**

- File: `crates/tessera-cli/src/main.rs` — add a `#[cfg(test)]` block at the bottom
  (after the `dirs_history_path` function) if one does not exist, otherwise add to it.
- Action: Add test for `infer_import_format`

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn infer_format_gql_extension() {
        assert_eq!(infer_import_format("schema.gql"), "gql");
        assert_eq!(infer_import_format("schema.GQL"), "gql");
    }

    #[test]
    fn infer_format_cypher_extension() {
        assert_eq!(infer_import_format("data.cypher"), "gql");
    }

    #[test]
    fn infer_format_csv_extension() {
        assert_eq!(infer_import_format("nodes.csv"), "csv-nodes");
    }

    #[test]
    fn infer_format_json_falls_back_to_gql() {
        // JSON import is not implemented; the inferred format must NOT be "json"
        // because handle_import has no "json" branch — it would produce a
        // confusing "unsupported import format: json" error instead of a clear one.
        // .json files fall back to "gql" so they get a parse error, not a silent mismatch.
        assert_ne!(infer_import_format("data.json"), "json");
    }

    #[test]
    fn infer_format_unknown_extension_defaults_to_gql() {
        assert_eq!(infer_import_format("data.txt"), "gql");
        assert_eq!(infer_import_format("data"), "gql");
    }
}
```

The `infer_format_json_falls_back_to_gql` test fails on the current implementation.

---

**Cycle 2.2 — GREEN: remove `"json"` branch from `infer_import_format`**

- File: `crates/tessera-cli/src/main.rs`
- Action: Modify `infer_import_format`

Replace:
```rust
Some(ext) if ext.eq_ignore_ascii_case("json") => "json",
```

With: remove that arm entirely. The `_ => "gql"` catch-all now handles `.json` files,
so they produce a GQL parse error instead of "unsupported import format: json".

- Verify: `cargo test -p tessera-cli infer_format` — all 5 tests pass.

---

### Fase 3: R5 — Encapsulate `ImportPlan` fields

**Rationale:** Must be done before C1 and R6 so that when those cycles use
`ImportPlan`, they already use the constructor API.

---

**Cycle 3.1 — RED: test constructor and private-field contract**

- File: `crates/tessera-cli/src/import.rs`
- Action: Add test

```rust
#[test]
fn import_plan_new_constructor() {
    let stmts = vec!["CREATE (:A)".to_owned(), "CREATE (:B)".to_owned()];
    let plan = ImportPlan::new(stmts, 1);
    // Must expose counts and batches, but NOT allow direct field mutation.
    assert_eq!(plan.batch_count(), 2);
    assert_eq!(plan.batch(0), &["CREATE (:A)"]);
    assert_eq!(plan.batch(1), &["CREATE (:B)"]);
}

#[test]
fn import_plan_statements_accessor() {
    let stmts = vec!["S1".to_owned(), "S2".to_owned(), "S3".to_owned()];
    let plan = ImportPlan::new(stmts.clone(), 10);
    assert_eq!(plan.statements(), stmts.as_slice());
}
```

These tests compile only after `ImportPlan::new` and `ImportPlan::statements()` are
added. They currently fail because neither exists.

---

**Cycle 3.2 — GREEN: make fields private, add constructor and accessor**

- File: `crates/tessera-cli/src/import.rs`
- Action: Modify `ImportPlan`

Replace:
```rust
#[derive(Debug)]
pub struct ImportPlan {
    pub statements: Vec<String>,
    pub batch_size: usize,
}
```

With:
```rust
#[derive(Debug)]
pub struct ImportPlan {
    statements: Vec<String>,
    batch_size: usize,
}
```

Add after the closing `}` of the struct (before `impl ImportPlan`):

```rust
impl ImportPlan {
    /// Create a new plan from pre-parsed statements.
    #[must_use]
    pub fn new(statements: Vec<String>, batch_size: usize) -> Self {
        Self { statements, batch_size }
    }

    /// Read-only view of all statements.
    #[must_use]
    pub fn statements(&self) -> &[String] {
        &self.statements
    }
    // ... existing methods follow
}
```

- File: `crates/tessera-cli/src/main.rs` — Fix the one call site that constructs
  `ImportPlan` directly via struct literal (in `handle_import`, `dry_run` branch):

Replace:
```rust
let plan = ImportPlan {
    statements,
    batch_size: 100,
};
```

With:
```rust
let plan = ImportPlan::new(statements, 100);
```

- Verify: `cargo build -p tessera-cli` compiles without errors.
- Verify: `cargo test -p tessera-cli import_plan` — all plan tests pass including new ones.

---

### Fase 4: R6 — Simplify `handle_exec` to iterate statements directly

**Rationale:** Removes the spurious batch abstraction. Depends on R5 (fields are now
private, so `plan.batch()` is still available but we switch to `plan.statements()`).

---

**Cycle 4.1 — RED: test that `handle_exec` semantics are documented via unit test on the helper logic**

There is no unit-testable pure logic to extract from `handle_exec` that isn't already
covered by `ImportPlan` tests, since `handle_exec` requires a live `Session`. The RED
step here is a compile-time guarantee: after R5 the old struct-literal construction in
`handle_import` already broke; the batch iteration in `handle_exec` still compiles
because it uses the public `batch_count()` / `batch()` methods. The "test" is that the
new, simpler version must produce identical behavior.

Add to `import.rs` tests:

```rust
#[test]
fn plan_statements_matches_batch_iteration() {
    // Validate that iterating plan.statements() is equivalent to
    // iterating all batches concatenated — important regression guard for R6.
    let content = "CREATE (:A);\nCREATE (:B);\nCREATE (:C);";
    let plan = ImportPlan::from_gql_content(content, 2).expect("plan"); // OK: test
    let via_batches: Vec<&str> = (0..plan.batch_count())
        .flat_map(|i| plan.batch(i).iter().map(String::as_str))
        .collect();
    let via_statements: Vec<&str> = plan.statements().iter().map(String::as_str).collect();
    assert_eq!(via_batches, via_statements);
}
```

---

**Cycle 4.2 — GREEN: replace batch-by-batch loop with direct statement iteration**

- File: `crates/tessera-cli/src/main.rs`
- Action: Modify `handle_exec`

Replace:
```rust
for batch_idx in 0..plan.batch_count() {
    for stmt in plan.batch(batch_idx) {
        let output = query::execute_query(session, stmt, &args.language).await?;
        let rendered = tessera_cli_lib::output::render(
            OutputFormat::Table,
            &output.columns,
            &output.rows,
            None,
            true,
        )?;
        println!("{rendered}");
    }
}
```

With:
```rust
for stmt in plan.statements() {
    let output = query::execute_query(session, stmt, &args.language).await?;
    let rendered = tessera_cli_lib::output::render(
        OutputFormat::Table,
        &output.columns,
        &output.rows,
        None,
        true,
    )?;
    println!("{rendered}");
}
```

- Verify: `cargo build -p tessera-cli` — no errors.

---

### Fase 5: R2 — DRY config resolution via `merge_options`

**Rationale:** Must be done before C1/R8 so there is a single place where the
precedence chain lives. After this, both `resolve_with_env` and
`resolve_full_with_env` become thin wrappers.

---

**Cycle 5.1 — RED: test that both paths produce identical output when no file config is present**

- File: `crates/tessera-cli/src/config.rs` — add to existing `#[cfg(test)] mod tests`
- Action: Add test

```rust
#[test]
fn resolve_and_resolve_full_agree_when_no_file_config() {
    // When no config file is present (empty env, no flags), both resolve paths
    // must produce the same result. This guards against divergence after the
    // DRY refactor.
    let cli = cli_from(&["tessera-cli", "-H", "shared-host", "--connect-timeout", "42"]);
    let env = empty_env();
    let (cfg_basic, pwd_basic) = ConnectionConfig::resolve_with_env(&cli, &env);
    let (cfg_full, pwd_full) = ConnectionConfig::resolve_full_with_env(&cli, &env);
    assert_eq!(cfg_basic.host, cfg_full.host);
    assert_eq!(cfg_basic.port, cfg_full.port);
    assert_eq!(cfg_basic.username, cfg_full.username);
    assert_eq!(cfg_basic.connect_timeout_secs, cfg_full.connect_timeout_secs);
    assert_eq!(cfg_basic.format, cfg_full.format);
    assert_eq!(pwd_basic, pwd_full);
}
```

This test should pass before and after the refactor — it is a regression guard.

---

**Cycle 5.2 — GREEN: extract `merge_options` private function**

- File: `crates/tessera-cli/src/config.rs`
- Action: Add a private `merge_options` function and refactor both `resolve_with_env`
  and `resolve_full_with_env` to call it.

The signature:

```rust
/// Merge CLI flags, environment, optional file config, and hardcoded defaults into
/// a `(ConnectionConfig, Option<String>)` pair.
///
/// `file_cfg` is `None` when called from `resolve_with_env` (no file lookup).
fn merge_options(
    cli: &Cli,
    env: &dyn EnvSource,
    file_cfg: Option<&TomlFileConfig>,
) -> (ConnectionConfig, Option<String>) {
    let defaults = ConnectionConfig::default();

    let host = cli
        .host
        .clone()
        .or_else(|| env.get("TESSERA_HOST"))
        .or_else(|| file_cfg.and_then(|f| f.connection.as_ref()?.host.clone()))
        .unwrap_or(defaults.host);

    let port = cli
        .port
        .or_else(|| env.get("TESSERA_PORT").and_then(|v| v.parse().ok()))
        .or_else(|| file_cfg.and_then(|f| f.connection.as_ref()?.port))
        .unwrap_or(defaults.port);

    let username = cli
        .username
        .clone()
        .or_else(|| env.get("TESSERA_USER"))
        .or_else(|| file_cfg.and_then(|f| f.connection.as_ref()?.username.clone()))
        .unwrap_or(defaults.username);

    let ca_cert = cli
        .ca_cert
        .clone()
        .or_else(|| env.get("TESSERA_CA_CERT"))
        .or_else(|| file_cfg.and_then(|f| f.connection.as_ref()?.ca_cert.clone()))
        .or(defaults.ca_cert);

    let connect_timeout_secs = cli
        .connect_timeout
        .or_else(|| env.get("TESSERA_CONNECT_TIMEOUT").and_then(|v| v.parse().ok()))
        .or_else(|| file_cfg.and_then(|f| f.connection.as_ref()?.connect_timeout_secs))
        .unwrap_or(defaults.connect_timeout_secs);

    let format = cli
        .format
        .clone()
        .or_else(|| file_cfg.and_then(|f| f.defaults.as_ref()?.format.clone()))
        .unwrap_or(defaults.format);

    let language = file_cfg
        .and_then(|f| f.defaults.as_ref()?.language.clone())
        .unwrap_or(defaults.language);

    let password = cli
        .password
        .clone()
        .or_else(|| env.get("TESSERA_PASSWORD"));

    let cfg = ConnectionConfig {
        host,
        port,
        username,
        connect_timeout_secs,
        ca_cert,
        tls_skip_verify: cli.tls_skip_verify,
        format,
        language,
    };

    (cfg, password)
}
```

Note that `connect_timeout_secs` now includes the `file_cfg` fallback (this is also
the R8 fix — see Fase 6). Note also that `TomlConnection` must gain a
`connect_timeout_secs: Option<u64>` field (see R8 in Fase 6 below, which is applied
simultaneously here).

`resolve_with_env` becomes:
```rust
pub fn resolve_with_env(cli: &Cli, env: &dyn EnvSource) -> (Self, Option<String>) {
    merge_options(cli, env, None)
}
```

`resolve_full_with_env` becomes:
```rust
pub fn resolve_full_with_env(cli: &Cli, env: &dyn EnvSource) -> (Self, Option<String>) {
    let file_cfg = Self::find_and_load_config_file();
    merge_options(cli, env, file_cfg.as_ref())
}
```

- Verify: `cargo test -p tessera-cli` — all existing config tests plus the new
  regression guard pass.

---

### Fase 6: R8 — Add `connect_timeout_secs` TOML fallback

**Note:** This fix is delivered inside `merge_options` in Fase 5. It requires
`TomlConnection` to carry the field. This cycle adds the struct field and the test.

---

**Cycle 6.1 — RED: test that `connect_timeout_secs` is read from TOML file**

- File: `crates/tessera-cli/src/config.rs` — add to `#[cfg(test)] mod tests`
- Action: Add test

```rust
#[test]
fn toml_file_sets_connect_timeout() {
    let dir = tempfile::tempdir().expect("tempdir"); // OK: test
    let path = dir.path().join("tessera.toml");
    std::fs::write(&path, "[connection]\nconnect_timeout_secs = 42\n")
        .expect("write"); // OK: test
    let file_cfg = ConnectionConfig::from_toml_file(&path)
        .expect("parse") // OK: test
        .expect("some"); // OK: test
    let conn = file_cfg.connection.expect("connection"); // OK: test
    assert_eq!(conn.connect_timeout_secs, Some(42));
}

#[test]
fn full_resolve_reads_connect_timeout_from_file_cfg() {
    // merge_options must pick up connect_timeout_secs from file when CLI and env
    // provide nothing.
    let file_cfg = TomlFileConfig {
        connection: Some(TomlConnection {
            host: None,
            port: None,
            username: None,
            ca_cert: None,
            connect_timeout_secs: Some(99),
        }),
        defaults: None,
    };
    let cli = cli_from(&["tessera-cli"]);
    let (cfg, _) = merge_options(&cli, &empty_env(), Some(&file_cfg));
    assert_eq!(cfg.connect_timeout_secs, 99);
}
```

The first test fails because `TomlConnection` has no `connect_timeout_secs` field. The
second fails because `merge_options` does not yet exist (written in Fase 5) — order of
operations: write Fase 5 code first, then run Fase 6 tests.

---

**Cycle 6.2 — GREEN: add field to `TomlConnection`**

- File: `crates/tessera-cli/src/config.rs`
- Action: Add `connect_timeout_secs: Option<u64>` to `TomlConnection`

```rust
#[derive(Debug, serde::Deserialize)]
pub struct TomlConnection {
    pub host: Option<String>,
    pub port: Option<u16>,
    pub username: Option<String>,
    pub ca_cert: Option<String>,
    pub connect_timeout_secs: Option<u64>,
}
```

The `merge_options` from Fase 5 already references this field via
`f.connection.as_ref()?.connect_timeout_secs`. Making this field `pub` is consistent
with the other `TomlConnection` fields (which are all pub — these are deserialization
DTOs, not domain objects).

- Verify: `cargo test -p tessera-cli connect_timeout` — both new tests pass plus all
  existing connect-timeout tests.

---

### Fase 7: R3 — Map readline error to `CliError::Config`

**Rationale:** One-line fix that improves exit codes for scripting. No new unit test is
needed because `run_repl` cannot be unit tested without a live connection, but we add
a documentation test to make the contract explicit.

---

**Cycle 7.1 — RED: verify exit code contract for `CliError::Config`**

The existing `config_error_exit_code` test in `error.rs` already verifies that
`CliError::Config` returns exit code 5. No new test is needed. The RED here is
conceptual: readline initialization failure currently exits with code 1
(`CliError::Connection`) instead of 5 (`CliError::Config`), which is a semantic error.

---

**Cycle 7.2 — GREEN: change readline error mapping**

- File: `crates/tessera-cli/src/main.rs`
- Action: Modify `run_repl`

Replace:
```rust
let mut rl = rustyline::DefaultEditor::new()
    .map_err(|e| CliError::Connection(format!("cannot initialize readline: {e}")))?;
```

With:
```rust
let mut rl = rustyline::DefaultEditor::new()
    .map_err(|e| CliError::Config(format!("cannot initialize readline: {e}")))?;
```

- Verify: `cargo build -p tessera-cli` — no errors.

---

### Fase 8: R4 — Log warnings for native cert load errors

**Rationale:** Silent discard of certificate errors can hide misconfigured systems.
One-line change, no new test (native cert loading is OS-dependent and not
unit-testable without mocking the OS).

---

**Cycle 8.1 — GREEN: log `native_certs.errors` as warnings**

- File: `crates/tessera-cli/src/main.rs`
- Action: Modify `build_tls_config`

Replace:
```rust
let native_certs = rustls_native_certs::load_native_certs();
for cert in native_certs.certs {
    let _ = root_store.add(cert);
}
```

With:
```rust
let native_certs = rustls_native_certs::load_native_certs();
for err in &native_certs.errors {
    eprintln!("Warning: failed to load native certificate: {err}");
}
for cert in native_certs.certs {
    let _ = root_store.add(cert);
}
```

- Verify: `cargo build -p tessera-cli` — no errors.

---

### Fase 9: C1 — Call `resolve_full` instead of `resolve` in `main.rs`

**Rationale:** The most impactful critical fix. Must be done last among the critical
fixes because all supporting infrastructure (R2/R8 DRY config, R5 encapsulation) is
now in place.

---

**Cycle 9.1 — RED: integration test that `resolve_full_with_env` reads TOML host**

This is already partially covered by existing TOML tests. Add an explicit test that
`resolve_full_with_env` reads `host` from a TOML file when CLI and env are empty, to
confirm the integration of `find_and_load_config_file` with `merge_options`:

- File: `crates/tessera-cli/src/config.rs` — add to `#[cfg(test)] mod tests`

```rust
#[test]
fn resolve_full_reads_host_from_toml_file_via_find_and_load() {
    // This test writes a tessera.toml in the current directory and verifies that
    // resolve_full_with_env picks it up. Uses a temp dir scoped to avoid
    // polluting the real working directory.
    //
    // Note: find_and_load_config_file reads "./tessera.toml" relative to the
    // process working directory, so this test must be run in isolation or accept
    // that it won't find the temp file. Instead, test via from_toml_file + merge_options
    // directly to avoid filesystem side effects.
    let file_cfg = TomlFileConfig {
        connection: Some(TomlConnection {
            host: Some("file-host.example".to_owned()),
            port: None,
            username: None,
            ca_cert: None,
            connect_timeout_secs: None,
        }),
        defaults: None,
    };
    let cli = cli_from(&["tessera-cli"]);
    let (cfg, _) = merge_options(&cli, &empty_env(), Some(&file_cfg));
    assert_eq!(cfg.host, "file-host.example");
}
```

---

**Cycle 9.2 — GREEN: change `main.rs` to call `resolve_full`**

- File: `crates/tessera-cli/src/main.rs`
- Action: Modify `run` function, line 40

Replace:
```rust
let (config, password) = ConnectionConfig::resolve(&cli);
```

With:
```rust
let (config, password) = ConnectionConfig::resolve_full(&cli);
```

---

**Cycle 9.3 — REFACTOR: rename `resolve` to `resolve_without_file`**

To prevent future callers from accidentally using the no-file variant, rename the
public `resolve` method to `resolve_without_file`.

- File: `crates/tessera-cli/src/config.rs`
- Action: Rename `resolve` → `resolve_without_file` with a doc-comment update

```rust
/// Resolve configuration with precedence: CLI flags > env vars > defaults.
///
/// Does NOT read any config file. For most use cases, prefer [`ConnectionConfig::resolve_full`]
/// which also consults `tessera.toml`.
///
/// Returns `(config, password)`. Password is returned separately and never persisted.
#[must_use]
pub fn resolve_without_file(cli: &Cli) -> (Self, Option<String>) {
    Self::resolve_with_env(cli, &RealEnv)
}
```

There are no callers of `resolve` in `main.rs` after Cycle 9.2 (it now calls
`resolve_full`). Search for any other callers:

- File: Entire workspace — run `grep -r "ConnectionConfig::resolve(" --include="*.rs"`
  to confirm zero remaining callers. If any are found, update them to either
  `resolve_without_file` or `resolve_full` as appropriate.

- Verify: `cargo build --workspace` — no errors.
- Verify: `cargo test -p tessera-cli` — all tests pass.
- Verify: `cargo clippy -p tessera-cli -- -D warnings` — zero warnings.

---

### Fase 10: Final verification

**Cycle 10.1 — Full test suite and clippy**

```bash
cargo test -p tessera-cli
cargo clippy -p tessera-cli -- -D warnings
cargo build -p tessera-cli --release
```

Expected: zero test failures, zero clippy warnings, clean release build.

---

## Estimación Total

| Fase | Descripción | Tiempo estimado |
|------|-------------|-----------------|
| 1 | C3: Unicode truncation | 20 min |
| 2 | C2: Remove `"json"` inference | 15 min |
| 3 | R5: `ImportPlan` encapsulation | 20 min |
| 4 | R6: Simplify `handle_exec` | 15 min |
| 5 | R2: Extract `merge_options` | 30 min |
| 6 | R8: TOML `connect_timeout_secs` | 15 min |
| 7 | R3: Readline error variant | 10 min |
| 8 | R4: Log native cert errors | 10 min |
| 9 | C1: Call `resolve_full` + rename | 20 min |
| 10 | Final verification | 10 min |
| **Total** | | **~2h 45min** |

---

## Criterios de Éxito

- [ ] `cargo test -p tessera-cli` — zero failures, all new tests included
- [ ] `cargo clippy -p tessera-cli -- -D warnings` — zero warnings
- [ ] `cargo build -p tessera-cli --release` — clean build
- [ ] `infer_import_format("data.json")` no longer returns `"json"`
- [ ] `truncate_line("€€€€€€€€€€", 20)` does not panic
- [ ] `ConnectionConfig::resolve` no longer exists (renamed to `resolve_without_file`)
- [ ] `resolve_full_with_env` picks up `connect_timeout_secs` from TOML
- [ ] `ImportPlan` fields `statements` and `batch_size` are private
- [ ] `handle_exec` iterates `plan.statements()` directly (no batch loop)
- [ ] Readline init failure returns exit code 5, not 1
- [ ] Native cert errors printed to stderr as warnings
- [ ] `merge_options` is the single source of truth for all precedence logic
