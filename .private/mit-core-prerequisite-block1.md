# MIT Core — GQL Feature Completion (Blocks 1-3)

**Date**: 2026-04-04
**Target repo**: `tessera-graph` (MIT core)
**Decision**: The complete GQL language lives in the MIT core. Enterprise
differentiates through production infrastructure (Bolt, TLS, auth, etc.),
not language restrictions. See enterprise `docs/architecture/ROADMAP.md`.

---

## Scope

All GQL language features are implemented in the MIT core. Three blocks
ordered by priority:

### Block 1 — Benchmark enablement (~5h)
- Variable-length paths `[*1..N]` (parser + compiler)
- `shortestPath(a, b)` function (compiler)

### Block 2 — Core functionality (~8.5h)
- SKIP clause
- CASE WHEN expression
- OPTIONAL MATCH (left-join semantics)
- WITH clause (multi-stage queries)

### Block 3 — Advanced features (~25h)
- UNWIND, UNION, EXISTS subqueries
- Regex matching (`=~`), path variables, map projections
- CALL procedures, EXPLAIN/PROFILE
- List comprehensions, FOREACH
- Multi-stage WITH chains

---

## Detailed TDD plans

The enterprise repo has detailed TDD plans for each block:
- `.private/tdd-plan-gql-block1-benchmark.md` — needs updating for core-only implementation
- `.private/tdd-plan-gql-block2-core.md`
- `.private/tdd-plan-gql-block3-advanced.md`

These plans contain exact file paths, line numbers, test specifications,
and architectural decisions. Copy or reference them from the MIT core session.

---

## Implementation approach

All changes go in `crates/tessera-graph/src/gql/`:
- `token.rs` — new keyword tokens
- `lexer.rs` — keyword recognition in `keyword_from_str`
- `ast.rs` — new AST types (clauses, expressions)
- `parser.rs` — syntax recognition and AST construction
- `compiler.rs` — execution logic
- `mod.rs` — public API re-exports

The `extended-gql` feature flag is used for **in-development** features only.
Once a feature is complete and tested, it graduates to always-on (remove the
`#[cfg]` gate).

---

## Start with Block 1

Block 1 is the highest priority — it enables the benchmark comparison with
Memgraph that is currently blocked.
