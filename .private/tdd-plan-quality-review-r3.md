# TDD Plan — Quality Review Round 3 (11 Findings)

## Contexto

Eleven quality review findings from post-resilience+streaming batch. The findings span two categories: annotation/documentation fixes (findings 1, 2, 4, 9, 10) that require no tests, and behavioral correctness fixes (findings 3, 5, 6, 7, 8, 11) that follow strict TDD cycles.

**Stack detectado**: Rust 2024, workspace with 14 crates
**Convenciones**: `#[cfg(test)] mod tests` inline; `crates/<crate>/tests/*.rs` integration; `// OK: test` annotation; `clippy::all = deny`, `clippy::pedantic = warn`, `clippy::nursery = warn`; `unsafe_code = forbid`; `pub(crate)` by default
**Afecta hot path**: No — all findings are in LBAC filtering helpers, CLI import utilities, and server config parsing.

## Decisiones Previas Necesarias

None. All 11 findings have unambiguous fixes.

---

## Plan de Ejecución

### Fase 0 — Anotaciones y Documentación (REFACTOR puro)

**Estimación: 15 min**

1. [ ] Add `#[must_use]` to 5 functions in `filter` module
   - File: `crates/tessera-graph-storage/src/lbac.rs`
   - Lines: 64 (`secure_node_ids`), 77 (`secure_nodes_by_label`), 139 (`secure_node_exists`), 150 (`secure_edges_by_label`), 186 (`secure_edge_count`)

2. [ ] Add `#[must_use]` to `parse_env_or_warn`
   - File: `crates/tessera-graph-server/src/config.rs:19`

3. [ ] Revise I/O comment in `secure_node_projected`
   - File: `crates/tessera-graph-storage/src/lbac.rs:118-120`
   - Replace: "The I/O optimization of `node_projected` in the inner graph is bypassed..."
   - With: "The full node is always fetched first so the security label is available for the clearance check, regardless of which `keys` were requested. The in-memory `Graph` has no I/O distinction; in a page-level storage backend the page containing the security properties is loaded, but this is the minimum required to enforce the access control decision."

4. [ ] Add "silently ignored" doc line to `secure_node_projected`
   - File: `crates/tessera-graph-storage/src/lbac.rs:113`
   - Add: `/// Keys that do not exist in the node's properties are silently ignored.`

5. [ ] Add TODO comment at both `node_count` call sites
   - File: `crates/tessera-graph-storage/src/lbac.rs:343,491`
   - Comment: `// TODO(perf): replace self.node_ids().len() with a counting iterator to avoid Vec allocation`

**Verification**: `cargo clippy -p tessera-graph-storage -p tessera-graph-server -- -D warnings` — zero warnings

---

### Fase 1 — RED: Failing Tests for Behavioral Findings

**Estimación: 30 min**

#### 1a. Finding 3 — Missing COMPARTMENTS_KEY assertion

6. [ ] Strengthen `ref_node_projected_strips_security_properties`
   - File: `crates/tessera-graph-storage/tests/secure_graph_ref_reads_test.rs`
   - Add: `assert!(!node.properties().contains_key(SecurityPolicy::COMPARTMENTS_KEY));`
   - State: Should pass immediately (regression guard)

#### 1b. Findings 5, 6 — Weak content assertions

7. [ ] Strengthen `write_json_value_array_returns_ok`
   - File: `crates/tessera-graph-cli/src/import.rs`
   - Replace: `assert!(!buf.is_empty(), "array should produce output");`
   - With: `assert_eq!(buf, "'[\"a\",\"b\",\"c\"]'");`

8. [ ] Strengthen `write_json_value_object_returns_ok`
   - File: `crates/tessera-graph-cli/src/import.rs`
   - Replace: `assert!(!buf.is_empty());`
   - With: `assert_eq!(buf, "'{\"nested\":\"value\"}'");`

#### 1c. Finding 7 — New single-quote test

9. [ ] Add `write_json_value_array_with_single_quote_is_escaped`
   - File: `crates/tessera-graph-cli/src/import.rs` — `#[cfg(test)] mod tests`
   ```rust
   #[test]
   fn write_json_value_array_with_single_quote_is_escaped() {
       use serde_json::json;
       let mut buf = String::new();
       let val = json!(["O'Brien"]);
       write_json_value_to_buf(&val, &mut buf).unwrap(); // OK: test
       assert_eq!(buf, "'[\"O''Brien\"]'");
   }
   ```

#### 1d. Finding 8 — Empty match test

10. [ ] Add `write_endpoint_match_empty_match_is_error`
    - File: `crates/tessera-graph-cli/src/import.rs` — `#[cfg(test)] mod tests`
    ```rust
    #[test]
    fn write_endpoint_match_empty_match_is_error() {
        use serde_json::json;
        let edge = json!({
            "from": { "label": "Person", "match": {} }
        });
        let mut buf = String::new();
        let result = write_endpoint_match(&edge, "from", &mut buf);
        assert!(result.is_err(), "empty match object must be rejected");
    }
    ```

#### 1e. Finding 11 — WAL env var tests

11. [ ] Add 3 WAL tests in `config.rs` `#[cfg(test)] mod tests`
    ```rust
    #[test]
    fn wal_enabled_garbage_value_uses_default() {
        std::env::set_var("TESSERA_WAL_ENABLED", "yes");
        let cfg = PersistenceConfig::from_env();
        std::env::remove_var("TESSERA_WAL_ENABLED");
        assert!(cfg.graph_config.wal_enabled, "garbage must fall back to default true");
    }

    #[test]
    fn wal_enabled_false_disables_wal() {
        std::env::set_var("TESSERA_WAL_ENABLED", "false");
        let cfg = PersistenceConfig::from_env();
        std::env::remove_var("TESSERA_WAL_ENABLED");
        assert!(!cfg.graph_config.wal_enabled);
    }

    #[test]
    fn wal_enabled_true_enables_wal() {
        std::env::set_var("TESSERA_WAL_ENABLED", "true");
        let cfg = PersistenceConfig::from_env();
        std::env::remove_var("TESSERA_WAL_ENABLED");
        assert!(cfg.graph_config.wal_enabled);
    }
    ```

---

### Fase 2 — GREEN: Minimal Production Code Changes

**Estimación: 25 min**

#### 2a. Finding 7 — Verify single-quote escaping

12. [ ] Confirm `other =>` branch handles single-quote escaping
    - File: `crates/tessera-graph-cli/src/import.rs:618-633`
    - The branch already escapes `'` → `''`. Test should go GREEN with no change.

#### 2b. Finding 8 — Unify match validation

13. [ ] Replace two-step validation with single guard
    - File: `crates/tessera-graph-cli/src/import.rs:662-671`
    - Replace `if match_obj.len() > 1` + `ok_or_else` with:
    ```rust
    if match_obj.len() != 1 {
        return Err(CliError::ImportExport(format!(
            "edge {endpoint_key}.match must have exactly one key, got {}",
            match_obj.len()
        )));
    }
    let (match_key, match_val) = match_obj
        .iter()
        .next()
        .expect("match_obj.len() == 1 guaranteed above");
    ```

#### 2c. Finding 11 — Replace manual WAL parsing

14. [ ] Replace manual parsing with `parse_bool_env_or_warn`
    - File: `crates/tessera-graph-server/src/config.rs:83-85`
    - Replace:
    ```rust
    let wal_enabled = std::env::var("TESSERA_WAL_ENABLED")
        .map(|v| v.to_lowercase() != "false")
        .unwrap_or(true);
    ```
    - With:
    ```rust
    let wal_enabled = parse_bool_env_or_warn("TESSERA_WAL_ENABLED", true);
    ```

---

### Fase 3 — REFACTOR: Polish

**Estimación: 15 min**

15. [ ] Full workspace clippy: `cargo clippy --workspace -- -D warnings`
16. [ ] Full workspace tests: `cargo test --workspace`
17. [ ] Check if `expect` in Finding 8 triggers `clippy::expect_used`. If so, replace with `let/else unreachable!()`.

---

### Fase 4 — Wiring Verification (MANDATORY)

**Estimación: 10 min**

18. [ ] `grep -rn "TESSERA_WAL_ENABLED" crates/` — exactly 1 occurrence (in `parse_bool_env_or_warn` call)
19. [ ] Verify `#[must_use]` on all 6 functions (5 in filter + `parse_env_or_warn`)
20. [ ] Verify `COMPARTMENTS_KEY` in `ref_node_projected_strips_security_properties`
21. [ ] Verify TODO comments at both `node_count` sites
22. [ ] `cargo build --workspace` — zero errors
23. [ ] `cargo test --workspace` — zero failures
24. [ ] `cargo clippy --workspace -- -D warnings` — zero warnings

---

## Estimación Total

| Fase | Descripción | Tiempo |
|------|-------------|--------|
| 0 | Anotaciones y documentación | 15 min |
| 1 | RED — tests | 30 min |
| 2 | GREEN — production fixes | 25 min |
| 3 | REFACTOR — clippy + polish | 15 min |
| 4 | Wiring verification | 10 min |
| **Total** | | **~95 min** |

## Criterios de Éxito

- [ ] `cargo clippy --workspace -- -D warnings` zero diagnostics
- [ ] `cargo test --workspace` zero failures
- [ ] `write_json_value_array_returns_ok` and `write_json_value_object_returns_ok` assert exact GQL output
- [ ] `write_json_value_array_with_single_quote_is_escaped` exists and passes
- [ ] `write_endpoint_match_empty_match_is_error` exists and passes
- [ ] `ref_node_projected_strips_security_properties` asserts both LEVEL_KEY and COMPARTMENTS_KEY
- [ ] 3 WAL tests pass
- [ ] `TESSERA_WAL_ENABLED` parsed exclusively through `parse_bool_env_or_warn`
- [ ] `node_count` debt documented at both call sites
