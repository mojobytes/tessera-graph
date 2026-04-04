# TDD Plan — Quality Review Round 4 (11 Findings)

## Contexto

Round 4 of quality review. 11 findings: 5 pure refactors, 2 test strengthenings, 1 conditional, 3 non-actions.

**Afecta hot path**: No.

## Plan

### Fase 0: Verify Finding 2 (5 min)
1. [ ] Check if clippy flags `secure_node` (returns Result) for missing `#[must_use]`

### Fase 1: Test Strengthening (20 min)
2. [ ] Finding 6 — add `len() == 1` to `node_projected_strips_security_properties`
3. [ ] Finding 7 — add `len() == 1` to `ref_node_projected_strips_security_properties`

### Fase 2: Pure Refactors (30 min)
4. [ ] Finding 1 — `#[must_use]` on `secure_node_projected`
5. [ ] Finding 3 — `let/else unreachable!` in `write_endpoint_match`
6. [ ] Finding 8 — HashSet in `secure_node_projected` retain
7. [ ] Finding 9 — remove trailing blank line in config.rs
8. [ ] Finding 11 — add serde_json ordering comment

### Fase 3: Full Verification (15 min)
9. [ ] clippy --workspace
10. [ ] test --workspace

### Fase 4: Non-Actions (documented)
11. [ ] Finding 4 — WAL tests: unsafe_code forbid blocks env var tests. Wiring verified via grep.
12. [ ] Finding 5 — TESSERA_DEFAULT_TENANT: String::FromStr is infallible, parse_env_or_warn misleading.
13. [ ] Finding 10 — "source" vs "from": test is correct, plan is narrative.

## Estimación: ~75 min
