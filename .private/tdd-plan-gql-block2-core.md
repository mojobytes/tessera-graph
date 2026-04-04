# TDD Plan: GQL Block 2 — Core Functionality

**Date**: 2026-04-03
**Branch**: `feature/gql-block2-core` (from `develop`)
**Repo**: MIT core — `/Volumes/WD_BLACK/repos/MojoBytes/tessera-ecosystem/tessera-graph`

---

## Context

TesseraGraph's GQL engine has a complete Block 1 (lexer, parser, compiler, MATCH/WHERE/RETURN/ORDER BY/LIMIT/aggregation). Block 2 implements the four clauses that users expect from any production graph database. The work lives entirely in the MIT core repo. The enterprise Cypher preprocessor (`tessera-graph-cypher`) must be updated in parallel for OPTIONAL MATCH and WITH to remove the rewrites/rejections it currently applies.

**Stack detectado**: Rust 2024, `tessera-graph` crate, MIT-licensed core, no `unsafe`, `clippy::all = deny`.

**Convenciones observadas**:
- Unit tests inline in the same file, under `#[cfg(test)] mod tests { ... }`.
- Integration tests in `tests/integration/gql_compiler.rs` using the `run(&graph, "GQL")` helper.
- Features gated with `#[cfg(feature = "extended-gql")]`.
- Errors via `Error::GqlSyntaxError` (parser) and `Error::GqlCompileError` (compiler).
- Each TDD cycle in `gql_compiler.rs` is labelled `// ── Cycle N: Name ──`.

**Afecta hot path**: No — query clauses add post-MATCH pipeline stages. MATCH itself (`compile_match` / `compile_path_pattern`) is unchanged in all four features.

**Estado inicial confirmado por lectura de fuentes**:
- `token.rs`: No tokens for `SKIP`, `WITH`, `OPTIONAL`, `CASE`, `WHEN`, `THEN`, `ELSE`, `END`.
- `ast.rs`: `GqlQuery` has `limit: Option<LimitClause>` but no `skip`. No `WithClause`, `OptionalMatchClause`, or `Expr::Case` variants.
- `parser.rs` (`parse`): parses `MATCH WHERE RETURN ORDER_BY LIMIT` in a fixed sequence. No `SKIP` after `LIMIT`. `parse_statement` handles consecutive `MATCH` clauses via AND-merge — but does not handle `WITH` mid-query.
- `compiler.rs` (`execute`): pipeline is scope-validate → aggregate-validate → compile_match → WHERE-filter → project → ORDER BY → DISTINCT → LIMIT (truncate). SKIP is absent.
- `preprocessor.rs`: `rewrite_optional_match` strips `OPTIONAL` keyword (loses NULL-fill). `detect_unsupported_clauses` rejects `WITH expr AS alias` with an informative error.

---

## Decisions Confirmed (no blockers)

- SKIP standard position: after ORDER BY, before LIMIT (GQL spec §8.6 / Cypher EBNF). This matches Neo4j and Memgraph.
- WITH implementation scope: single-stage (one WITH clause between MATCH and RETURN). Multi-WITH is deferred.
- OPTIONAL MATCH implementation scope: single optional pattern after a mandatory MATCH. Nested OPTIONAL MATCH is deferred.
- CASE WHEN implementation scope: searched form only (`CASE WHEN cond THEN val ... ELSE val END`). Simple form is deferred.

---

## Plan de Ejecución

### Feature 1: SKIP Clause

Complexity: Low. SKIP complements LIMIT; the compiler already does `rows.truncate(limit.count)` at line 962.

---

#### Phase 1.1 — AST + Token (20 min)

**RED**: Write a failing test that constructs a `GqlQuery` with `skip: Some(SkipClause { count: 5 })` — will not compile until `SkipClause` exists.

- File: `src/gql/ast.rs`
- Action: Add `SkipClause` struct (mirrors `LimitClause`) and `skip: Option<SkipClause>` field to `GqlQuery`.
- Output: `GqlQuery` has both `skip` and `limit` fields; existing construction sites break with missing-field errors.

```rust
// Add after LimitClause (line 76):
/// Row-offset following `SKIP`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SkipClause {
    /// Number of rows to skip before returning results.
    pub count: u64,
}
```

- File: `src/gql/token.rs`
- Action: Add `Token::Skip` variant in the Keywords section; add `"SKIP" => Some(Token::Skip)` to `keyword_from_str`; add `Self::Skip => f.write_str("SKIP")` to `Display`.

- Fix all existing `GqlQuery { ... }` construction sites that now lack `skip`:
  - `ast.rs` tests (line 437): add `skip: None`
  - `parser.rs` `parse` fn (line 202): add `skip: None`
  - `parser.rs` `parse_after_match` (line 303): add `skip: None`

**Estimated broken sites**: 3 construction sites in parser.rs + many in tests.

---

#### Phase 1.2 — Parser (20 min)

**RED**: Write a failing integration test:
```rust
// tests/integration/gql_compiler.rs — Cycle N: SKIP
#[test]
fn skip_skips_first_n_rows_after_order_by() { ... }
```

- File: `src/gql/parser.rs`
- Action: Add `parse_skip_clause()` method (mirrors `parse_limit_clause`, line 925–934). In `parse` (line 191) and `parse_after_match` (line 296), insert `let skip = self.parse_skip_clause()?;` between `parse_order_by_clause()` and `parse_limit_clause()`. Add `skip` to `GqlQuery` construction in both sites.

```rust
fn parse_skip_clause(&mut self) -> crate::Result<Option<SkipClause>> {
    if *self.peek() != Token::Skip {
        return Ok(None);
    }
    self.advance(); // consume SKIP
    let raw = self.expect_int()?;
    let count = u64::try_from(raw)
        .map_err(|_| self.syntax_error("SKIP value must be a non-negative integer"))?;
    Ok(Some(SkipClause { count }))
}
```

Import: add `SkipClause` to the `use crate::gql::ast::...` import block at the top of `parser.rs`.

---

#### Phase 1.3 — Compiler (20 min)

**GREEN**: Make the integration test pass.

- File: `src/gql/compiler.rs`
- Action: In `execute` (line 899), between step 8 (DISTINCT) and step 9 (LIMIT), insert SKIP:

```rust
// 8.5. SKIP
if let Some(ref skip) = query.skip {
    let n = skip.count as usize;
    if n >= rows.len() {
        rows.clear();
    } else {
        rows.drain(..n);
    }
}
```

Also add `SkipClause` to the `use super::ast::...` import block in `compiler.rs`.

**REFACTOR**: Add `skip` to `validate_scope` (line 534) — no vars to validate, but ensure the function signature still compiles. Add `skip` to `collect_bound_vars` — no change needed.

---

#### Phase 1.4 — Unit Tests for SKIP (15 min)

- File: `src/gql/ast.rs` — add `skip_clause_stores_value` test (mirrors `limit_clause_stores_value`, line 454).
- File: `src/gql/parser.rs` — add unit test for `parse_skip_clause` returning `None` when no `SKIP` token.
- File: `tests/integration/gql_compiler.rs` — add:
  - `skip_skips_first_n_rows_after_order_by`: `MATCH (a:Person) RETURN a.name ORDER BY a.name SKIP 1` → 3 rows (Bob, Carol, Dave).
  - `skip_and_limit_combined`: `MATCH (a:Person) RETURN a.name ORDER BY a.name SKIP 1 LIMIT 2` → [Bob, Carol].
  - `skip_larger_than_result_set_returns_empty`: `SKIP 999` → empty.
  - `skip_zero_returns_all`: `SKIP 0` → all rows.
  - `skip_without_order_by_is_valid`: `MATCH (a:Person) RETURN a.name SKIP 2` → 2 rows (order unspecified but count correct).

**Criterion for GREEN**: `cargo test --package tessera-graph` passes with no warnings.

---

### Feature 2: CASE WHEN (Searched Form)

Complexity: Medium. Pure expression extension — no changes to MATCH or the query pipeline. Requires new token variants, new AST variant, new parser sub-grammar, and `eval_expr` arm.

---

#### Phase 2.1 — Tokens (15 min)

**RED**: Write a lexer unit test that expects `Token::Case`, `Token::When`, `Token::Then`, `Token::Else`, `Token::End` from `CASE WHEN THEN ELSE END`.

- File: `src/gql/token.rs`
- Action: Add five keyword variants (after `Collect`):
  ```
  Case, When, Then, Else, End,
  ```
  Add to `keyword_from_str`: `"CASE"`, `"WHEN"`, `"THEN"`, `"ELSE"`, `"END"`.
  Add to `Display`.

---

#### Phase 2.2 — AST (15 min)

**RED**: Write an AST test constructing `Expr::Case { branches: vec![(cond, result)], else_expr: None }`.

- File: `src/gql/ast.rs`
- Action: Add `Case` variant to `Expr` enum:

```rust
/// A searched CASE expression: `CASE WHEN cond THEN result ... [ELSE default] END`.
Case {
    /// The list of (condition, result) branches evaluated in order.
    branches: Vec<(Box<Self>, Box<Self>)>,
    /// The optional ELSE expression; evaluates to NULL if absent and no branch matched.
    else_expr: Option<Box<Self>>,
},
```

No feature gate — searched CASE is standard GQL (ISO/IEC 39075 §7.9).

Update `expr_has_aggregate` (compiler.rs line 564), `collect_expr_vars` (compiler.rs line 503), `expr_surface_name` (compiler.rs line 391) — each `match expr` must become exhaustive. Tests will catch this via compilation errors.

---

#### Phase 2.3 — Parser (25 min)

**RED**: Write a parser unit test: parse `CASE WHEN n.age > 18 THEN 'adult' ELSE 'minor' END` produces `Expr::Case { branches: [...], else_expr: Some(...) }`.

- File: `src/gql/parser.rs`
- Action: Add `parse_case_expr` called from `parse_primary` (the bottom-most parser level, where literals and function calls are parsed). The searched form grammar:

```
CASE (WHEN expr THEN expr)+ [ELSE expr] END
```

Insert in `parse_primary` (find the match arm for `Token::LParen` and `Token::Ident`):
```rust
Token::Case => self.parse_case_expr(),
```

```rust
fn parse_case_expr(&mut self) -> crate::Result<Expr> {
    self.advance(); // consume CASE
    let mut branches = Vec::with_capacity(4);
    // Must have at least one WHEN branch.
    if *self.peek() != Token::When {
        return Err(self.syntax_error("expected WHEN after CASE"));
    }
    while *self.peek() == Token::When {
        self.advance(); // consume WHEN
        self.enter_expr()?;
        let cond = self.parse_expr()?;
        self.exit_expr();
        self.expect(&Token::Then)?; // ... but Token::Then doesn't exist yet — added in Phase 2.1
        self.enter_expr()?;
        let result = self.parse_expr()?;
        self.exit_expr();
        branches.push((Box::new(cond), Box::new(result)));
    }
    let else_expr = if *self.peek() == Token::Else {
        self.advance(); // consume ELSE
        self.enter_expr()?;
        let e = self.parse_expr()?;
        self.exit_expr();
        Some(Box::new(e))
    } else {
        None
    };
    self.expect(&Token::End)?; // Token::End added in Phase 2.1
    Ok(Expr::Case { branches, else_expr })
}
```

NOTE: `Token::Then`, `Token::Else`, `Token::End` must be added in Phase 2.1 before this phase. They must also be added to `parse_primary`'s "fall-through to Ident" logic so they don't shadow themselves (these tokens are not identifiers).

---

#### Phase 2.4 — Compiler Evaluation (20 min)

**GREEN**: Make integration test pass.

- File: `src/gql/compiler.rs`
- Action: Add `Expr::Case` arm to `eval_expr` (line 104):

```rust
Expr::Case { branches, else_expr } => {
    for (cond, result) in branches {
        let cv = eval_expr(cond, pm);
        if eval_as_tribool(&cv) == Some(true) {
            return eval_expr(result, pm);
        }
    }
    else_expr.as_ref().map_or(GqlValue::Null, |e| eval_expr(e, pm))
}
```

Update the three helper functions in compiler.rs:
- `expr_has_aggregate`: add `Expr::Case { branches, else_expr }` arm.
- `collect_expr_vars`: add `Expr::Case { branches, else_expr }` arm.
- `expr_surface_name`: add `Expr::Case { .. }` arm returning `"CASE"` (display name only).

---

#### Phase 2.5 — Integration Tests for CASE WHEN (15 min)

- File: `tests/integration/gql_compiler.rs`
- Add:
  - `case_when_adult_minor`: `MATCH (n:Person) RETURN n.name, CASE WHEN n.age > 30 THEN 'senior' ELSE 'junior' END AS category` → Alice/senior, Carol/junior, Dave/senior, Bob/junior.
  - `case_when_no_else_returns_null_when_no_branch_matches`: `CASE WHEN n.age > 100 THEN 'old' END` → NULL for all.
  - `case_when_multiple_branches_first_match_wins`: `CASE WHEN n.age > 40 THEN 'elder' WHEN n.age > 30 THEN 'senior' ELSE 'junior' END` — Dave(40): senior (not elder, 40 is not > 40); Alice(35): senior; Bob/Carol: junior.
  - `case_when_in_where_clause`: `MATCH (n:Person) WHERE CASE WHEN n.age > 30 THEN true ELSE false END RETURN n.name` → Alice, Dave.

**Criterion for GREEN**: `cargo test --package tessera-graph` passes.

---

### Feature 3: OPTIONAL MATCH

Complexity: High-Medium. Requires new AST structure, parser restructuring, and a left-join semantics in the compiler. The Cypher preprocessor must also stop stripping OPTIONAL MATCH.

---

#### Phase 3.1 — AST: OptionalMatchClause and NullablePatternMatch (25 min)

**RED**: Write an AST test constructing a `GqlQuery` that has both a mandatory `match_clause` and an `optional_match: Option<OptionalMatchClause>`.

The core challenge: when OPTIONAL MATCH finds no rows, the bound variables from the optional pattern must appear in the result row with `GqlValue::Null`. The compiler's `PatternMatch` struct (from `query::pattern`) cannot represent nulls natively; the compiler must synthesize null rows.

- File: `src/gql/ast.rs`
- Action:
  - Add `OptionalMatchClause` struct (same shape as `MatchClause` — a `Vec<PathPattern>`).
  - Add `optional_matches: Vec<OptionalMatchClause>` field to `GqlQuery` (Vec to allow future multi-OPTIONAL MATCH; scoped to one for now).

```rust
/// An OPTIONAL MATCH clause: left-join semantics (unmatched rows yield NULL).
#[derive(Debug, Clone, PartialEq)]
pub struct OptionalMatchClause {
    pub patterns: Vec<PathPattern>,
}
```

Fix all `GqlQuery` construction sites: add `optional_matches: vec![]`.

---

#### Phase 3.2 — Token + Lexer (10 min)

- File: `src/gql/token.rs`
- Action: Add `Token::Optional` variant. Add `"OPTIONAL" => Some(Token::Optional)` to `keyword_from_str`. Add to `Display`.

The two-word sequence `OPTIONAL MATCH` is recognized in the parser by peeking at the next token, not in the lexer (consistent with existing multi-word sequences like `ORDER BY`, `IS NOT NULL`).

---

#### Phase 3.3 — Parser: OPTIONAL MATCH sequence (30 min)

**RED**: Write a parser unit test that parses `MATCH (a:Person) OPTIONAL MATCH (a)-[:KNOWS]->(b) RETURN a.name, b.name` and produces a `GqlStatement::Query` with one mandatory match and one optional match.

- File: `src/gql/parser.rs`
- Action: In `parse_statement` → `Token::Match` branch (line 222), after the `while *self.peek() == Token::Match` loop for consecutive mandatory MATCHes, add a loop for OPTIONAL MATCH:

```rust
// After collecting mandatory MATCH clauses:
let mut optional_matches: Vec<OptionalMatchClause> = Vec::new();
while *self.peek() == Token::Optional {
    self.advance(); // consume OPTIONAL
    self.expect(&Token::Match)?;
    let mut patterns = Vec::with_capacity(4);
    patterns.push(self.parse_path_pattern()?);
    while *self.peek() == Token::Comma {
        self.advance();
        patterns.push(self.parse_path_pattern()?);
    }
    optional_matches.push(OptionalMatchClause { patterns });
    // OPTIONAL MATCH may have its own WHERE (deferred: for now no WHERE on OPTIONAL)
}
```

Then thread `optional_matches` through `parse_after_match` (add parameter or restructure to pass it to the `GqlQuery` construction site).

Also update `parse` (the legacy method, line 191): OPTIONAL MATCH is only valid in `parse_statement` context; `parse` is for the simplified read-only path and can remain without OPTIONAL MATCH support (it already returns `GqlQuery` directly).

Import: add `OptionalMatchClause` to the `use crate::gql::ast::...` import in `parser.rs`.

---

#### Phase 3.4 — Compiler: Left-Join Semantics (40 min)

**GREEN**: Make the integration test pass.

The logic: for each mandatory-match row, try to apply the optional-match patterns. If results are found, cross-join them (as in the normal multi-MATCH case). If no results are found, keep the mandatory row but set all optional-pattern variables to NULL.

- File: `src/gql/compiler.rs`
- Action: Add a new function `apply_optional_match`:

```rust
fn apply_optional_match<G: GraphAccess + ?Sized>(
    graph: &G,
    mandatory_rows: Vec<PatternMatch>,
    omc: &OptionalMatchClause,
) -> crate::Result<Vec<(PatternMatch, Option<PatternMatch>)>> {
    // For each mandatory row, collect the optional-side variables,
    // run the optional patterns, then left-join or yield None.
    let mut result = Vec::with_capacity(mandatory_rows.len());
    for mandatory_pm in mandatory_rows {
        let opt_matches = compile_match(graph, &MatchClause { patterns: omc.patterns.clone() })?;
        // Filter optional matches that are compatible with the mandatory row
        // (share bound variable values).
        let compatible: Vec<PatternMatch> = opt_matches
            .into_iter()
            .filter(|opt_pm| is_compatible(&mandatory_pm, opt_pm))
            .collect();
        if compatible.is_empty() {
            result.push((mandatory_pm, None));
        } else {
            for opt_pm in compatible {
                result.push((mandatory_pm.clone(), Some(opt_pm)));
            }
        }
    }
    Ok(result)
}
```

`is_compatible` checks that shared variable names refer to the same node ID. This requires `PatternMatch::get_node(var)` (already available).

Modify `execute` to use a new combined row type: after mandatory MATCH + WHERE filtering, apply each `optional_match` in `query.optional_matches`. Project `PatternMatch` rows keeping optional-side variables as NULL when no match was found. This requires `project_row` to handle a combined mandatory+optional PatternMatch.

The cleanest approach: after applying optional matches, merge the mandatory and optional `PatternMatch` into a single `PatternMatch` using the existing `PatternMatch::merge` (already in codebase at `compiler.rs` line 622). For null-side rows, create a synthetic "null" PatternMatch that returns `GqlValue::Null` for all variable references. Since `PatternMatch::get_node` returns `Err` for unknown vars, `eval_expr` already returns `GqlValue::Null` for missing vars — the synthetic null row approach means simply using the mandatory row alone (without merging the optional side), which already yields NULL for any reference to optional variables.

Simpler implementation: when `optional_match` has no compatible rows for a mandatory row, push `mandatory_pm` alone. When it does, push `mandatory_pm.merge(opt_pm)` for each compatible result. This works because `eval_expr` returns `GqlValue::Null` for any `PropAccess { var }` where `var` is not in `PatternMatch`.

Update `validate_scope` to include variables bound in `optional_matches` as valid (they produce NULL when unmatched, not an error). Add `optional_matches` variables to `bound_vars`:

```rust
// In collect_bound_vars, extend to also collect from optional_matches:
fn collect_all_bound_vars(query: &GqlQuery) -> HashSet<String> {
    let mut vars = collect_bound_vars(&query.match_clause);
    for omc in &query.optional_matches {
        for pp in &omc.patterns {
            // collect vars same as collect_bound_vars
        }
    }
    vars
}
```

---

#### Phase 3.5 — Preprocessor Update (15 min)

- File: `crates/tessera-graph-cypher/src/preprocessor.rs` (enterprise repo)
- Action: Remove `rewrite_optional_match` from the `cypher_to_gql` pipeline (line 45). Keep the function body but have `cypher_to_gql` pass through OPTIONAL MATCH unchanged. Update the `reject_cypher_constructs` function to NOT reject `OPTIONAL MATCH` (it is now supported). Update the `detect_unsupported_clauses` to not flag OPTIONAL MATCH.

NOTE: `rewrite_optional_match` must be kept in the file but removed from the call chain (do not delete — it is tested and the tests serve as documentation). Mark it `#[allow(dead_code)]` temporarily; after integration tests are green, remove it entirely.

---

#### Phase 3.6 — Integration Tests for OPTIONAL MATCH (20 min)

- File: `tests/integration/gql_compiler.rs`
- Add (social graph: Alice-KNOWS->Bob, Alice-KNOWS->Carol, Dave has no KNOWS edges outbound):
  - `optional_match_yields_null_when_no_pattern_match`:
    `MATCH (a:Person) OPTIONAL MATCH (a)-[:KNOWS]->(b) RETURN a.name, b.name`
    → 5 rows total: Alice/Bob, Alice/Carol, Dave/NULL, Bob/NULL, Carol/NULL.
  - `optional_match_with_mandatory_where`:
    `MATCH (a:Person) WHERE a.name = 'Alice' OPTIONAL MATCH (a)-[:KNOWS]->(b) RETURN a.name, b.name`
    → 2 rows: Alice/Bob, Alice/Carol.
  - `optional_match_result_count`:
    `MATCH (a:Person) OPTIONAL MATCH (a)-[:KNOWS]->(b) RETURN a.name` → 5 rows (Dave/Bob/Carol appear once each, Alice appears twice).

---

### Feature 4: WITH Clause (Single-Stage)

Complexity: High. WITH is a structural change that makes the query a two-stage pipeline. It cannot be shoehorned into the current flat `GqlQuery` — a new `GqlStatement::Pipeline` variant or a `stages: Vec<QueryStage>` design is needed.

---

#### Phase 4.1 — Architecture Decision

The cleanest approach that does not break the existing AST or API:

```
GqlQuery {
    match_clause: MatchClause,
    where_clause: Option<WhereClause>,
    with_clause: Option<WithClause>,       // NEW
    return_clause: ReturnClause,
    order_by: Option<OrderByClause>,
    skip: Option<SkipClause>,
    limit: Option<LimitClause>,
}

WithClause {
    items: Vec<ReturnItem>,                // reuse existing ReturnItem (expr + alias)
    where_clause: Option<WhereClause>,     // WHERE after WITH
}
```

This covers `MATCH ... WITH expr AS alias [WHERE ...] RETURN alias` with a single WITH. Multi-WITH is future work.

---

#### Phase 4.2 — Tokens (10 min)

- File: `src/gql/token.rs`
- Action: Add `Token::With` variant. Add `"WITH" => Some(Token::With)` to `keyword_from_str`. Add to `Display`.

NOTE: `STARTS WITH` / `ENDS WITH` are two-word operators parsed at a higher level — the parser already handles them in the `extended-gql` feature. Adding `Token::With` does not conflict because the parser for those operators looks ahead after `Token::Ident` matching `STARTS`/`ENDS`.

---

#### Phase 4.3 — AST: WithClause (15 min)

**RED**: Write an AST test constructing a `GqlQuery` with `with_clause: Some(WithClause { items: [...], where_clause: None })`.

- File: `src/gql/ast.rs`
- Action: Add `WithClause` struct. Add `with_clause: Option<WithClause>` field to `GqlQuery`:

```rust
/// An intermediate projection and filtering stage following `WITH`.
#[derive(Debug, Clone, PartialEq)]
pub struct WithClause {
    /// The projected expressions with optional aliases (same as RETURN items).
    pub items: Vec<ReturnItem>,
    /// An optional filter applied after projection.
    pub where_clause: Option<WhereClause>,
}
```

Fix all `GqlQuery` construction sites (by now ~6 sites including the new ones from Features 1–3): add `with_clause: None`.

---

#### Phase 4.4 — Parser: WITH clause (30 min)

**RED**: Write a parser unit test parsing `MATCH (n:Person) WITH n.name AS name RETURN name`.

- File: `src/gql/parser.rs`
- Action: Add `parse_with_clause()` method. Call it in `parse` (line 191) and `parse_after_match` between the WHERE clause and the RETURN clause:

```rust
fn parse_with_clause(&mut self) -> crate::Result<Option<WithClause>> {
    if *self.peek() != Token::With {
        return Ok(None);
    }
    self.advance(); // consume WITH
    // Parse projection items (reuse parse_return_item logic).
    let mut items = Vec::with_capacity(4);
    items.push(self.parse_return_item()?);
    while *self.peek() == Token::Comma {
        self.advance();
        items.push(self.parse_return_item()?);
    }
    // Optional WHERE after WITH.
    let where_clause = self.parse_where_clause()?;
    Ok(Some(WithClause { items, where_clause }))
}
```

Position in `parse` / `parse_after_match`:
```
parse_match → parse_where → parse_with → parse_return → parse_order_by → parse_skip → parse_limit
```

`parse_with_star` in the preprocessor (`WITH *` stripped) is already removed from the pipeline in Phase 3.5. The `detect_unsupported_clauses` rejection of `WITH expr AS alias` must also be removed in this phase.

---

#### Phase 4.5 — Compiler: Two-Stage Execution (45 min)

**GREEN**: Make integration test pass.

The execution model for `WITH`:

1. Execute MATCH + WHERE → `Vec<PatternMatch>` (stage 1 rows).
2. Project stage 1 rows using `with_clause.items` → `Vec<GqlRow>` (intermediate rows). These are concrete HashMap rows, not PatternMatch structs.
3. Apply `with_clause.where_clause` filter on the intermediate rows (predicate evaluated against the row's column values, not graph PatternMatch).
4. Feed intermediate rows into RETURN projection, ORDER BY, SKIP, LIMIT.

The critical design point: after WITH, variable references in the second stage (`name` in `RETURN name`) refer to columns in the intermediate `GqlRow`, not to graph nodes in `PatternMatch`. So `eval_expr` must be extended to accept either a `PatternMatch` or a `GqlRow` as context.

Implementation approach — introduce a new evaluation context:

```rust
enum EvalCtx<'a> {
    Pattern(&'a PatternMatch),
    Row(&'a GqlRow),
}
```

Add an `eval_expr_ctx` that dispatches to `eval_expr` for `Pattern` or to a new `eval_expr_row` for `Row`. The `eval_expr_row` handles `Expr::PropAccess { var, prop }` as a column lookup (since WITH aliases are flat column names — `n.name AS name` → column `"name"`), `Expr::Var(v)` as direct column lookup, and all non-data expressions (BinaryOp, UnaryOp, etc.) recursively.

In `execute` (line 899), after WHERE filtering (step 4), check if `query.with_clause` is `Some`:

```rust
if let Some(ref wc) = query.with_clause {
    // Stage 2: project through WITH
    let intermediate: Vec<GqlRow> = filtered
        .iter()
        .map(|pm| project_row(pm, &wc.items))
        .collect();

    // Apply WITH WHERE
    let stage2_rows: Vec<GqlRow> = if let Some(ref wwhere) = wc.where_clause {
        intermediate
            .into_iter()
            .filter(|row| {
                eval_as_tribool(&eval_expr_row(&wwhere.predicate, row)) == Some(true)
            })
            .collect()
    } else {
        intermediate
    };

    // RETURN, ORDER BY, SKIP, LIMIT operate on GqlRow directly
    // (ORDER BY must eval against GqlRow in this path)
    let mut result_rows = project_stage2_rows(&stage2_rows, &query.return_clause.items);
    // ORDER BY, SKIP, LIMIT on result_rows...
    return Ok(result_rows);
}
```

`eval_expr_row` for `Expr::Var(v)` and `Expr::PropAccess { var, prop: _ }`: look up `var` in the `GqlRow` HashMap. For `PropAccess`, the WITH alias shadows `var.prop` (if the user wrote `WITH n.name AS name`, then `RETURN name` uses `Expr::Var("name")` which maps to the column).

`project_stage2_rows` is identical to `project_row` but uses `eval_expr_row` instead of `eval_expr`.

---

#### Phase 4.6 — Scope Validation Update (15 min)

`validate_scope` currently checks all RETURN/WHERE/ORDER BY vars against MATCH-bound vars. After WITH, stage-2 variables are the WITH aliases, not MATCH variables. Two-phase scope validation is needed:

- Stage 1 scope: WHERE vars must be in MATCH-bound vars (existing check).
- Stage 2 scope: RETURN/ORDER BY vars must be in WITH aliases (when `with_clause` is Some).

- File: `src/gql/compiler.rs`
- Action: Extend `validate_scope` to short-circuit differently when `with_clause` is Some.

---

#### Phase 4.7 — Preprocessor Update (10 min)

- File: `crates/tessera-graph-cypher/src/preprocessor.rs` (enterprise repo)
- Action: Remove `WITH` rejection from `detect_unsupported_clauses` (line 756). Remove `WITH expr AS alias` error. Update the docstring table for `WITH` from "Deferred" to "Supported (single-stage)".

---

#### Phase 4.8 — Integration Tests for WITH (20 min)

- File: `tests/integration/gql_compiler.rs`
- Add:
  - `with_passes_projected_column_to_return`:
    `MATCH (n:Person) WITH n.name AS name RETURN name` → 4 rows with column `name`.
  - `with_where_filters_intermediate_rows`:
    `MATCH (n:Person) WITH n.name AS name WHERE name = 'Alice' RETURN name` → 1 row.
  - `with_aggregation_count`:
    `MATCH (n:Person) WITH count(*) AS cnt RETURN cnt` → 1 row, cnt = 4.
  - `with_and_return_different_alias`:
    `MATCH (n:Person) WITH n.age AS years WHERE years > 30 RETURN years` → 3 rows (Alice=35, Carol=30→excluded? wait: 30 > 30 is false; so Alice=35, Dave=40 → 2 rows).

---

### Phase 5: Mandatory Wiring Verification

After all features are green, run the full verification suite.

#### Phase 5.1 — Full Test Suite (10 min)

```bash
cd /Volumes/WD_BLACK/repos/MojoBytes/tessera-ecosystem/tessera-graph
cargo test --package tessera-graph
cargo test --package tessera-graph --features extended-gql
```

Both must pass with zero warnings (warnings are errors under the project's clippy config).

#### Phase 5.2 — Enterprise Cypher Preprocessor Tests (10 min)

```bash
cd /Volumes/WD_BLACK/repos/MojoBytes/tessera-ecosystem/tessera-graph-enterprise
cargo test --package tessera-graph-cypher
```

Ensure the preprocessor tests updated in Phases 3.5 and 4.7 pass.

#### Phase 5.3 — Clippy (10 min)

```bash
cd /Volumes/WD_BLACK/repos/MojoBytes/tessera-ecosystem/tessera-graph
cargo clippy --all-targets -- -D warnings
cargo clippy --all-targets --features extended-gql -- -D warnings
```

#### Phase 5.4 — Dead Code Audit (10 min)

Use the `verify-wiring` skill to detect:
- Any new token variants (`Skip`, `Optional`, `With`, `Case`, `When`, `Then`, `Else`, `End`) that are lexed but never matched in the parser.
- Any new AST variants (`SkipClause`, `OptionalMatchClause`, `WithClause`, `Expr::Case`) that are constructed in the parser but never matched in `eval_expr` or other compiler functions.
- Preprocessor functions marked `#[allow(dead_code)]` that should now be removed.

---

## Estimacion Total

| Feature | Impl | Unit Tests | Integration Tests | Total |
|---------|------|-----------|-------------------|-------|
| SKIP | 35 min | 20 min | 15 min | 70 min |
| CASE WHEN | 55 min | 20 min | 20 min | 95 min |
| OPTIONAL MATCH | 90 min | 20 min | 25 min | 135 min |
| WITH | 120 min | 20 min | 25 min | 165 min |
| Wiring Verification | — | — | 40 min | 40 min |
| **Total** | | | | **~8.4 h** |

---

## Criterios de Exito

- [ ] `cargo test --package tessera-graph` passes — zero failures, zero warnings.
- [ ] `cargo test --package tessera-graph --features extended-gql` passes.
- [ ] `cargo test --package tessera-graph-cypher` passes (preprocessor tests updated).
- [ ] `cargo clippy --all-targets -- -D warnings` clean on both feature sets.
- [ ] `MATCH (n) RETURN n.name ORDER BY n.name SKIP 10 LIMIT 5` parses and executes correctly.
- [ ] `MATCH (n) RETURN CASE WHEN n.age > 18 THEN 'adult' ELSE 'minor' END` produces correct values.
- [ ] `MATCH (a:Person) OPTIONAL MATCH (a)-[:KNOWS]->(b) RETURN a.name, b.name` yields NULL for unmatched optional variables.
- [ ] `MATCH (n:Person) WITH n.name AS name WHERE name = 'Alice' RETURN name` executes as two-stage pipeline.
- [ ] No new `#[allow(dead_code)]` annotations left in the codebase after Phase 5.4.

---

## Notas de Implementacion

### Ordering of Features

Implement in this exact order: SKIP (1) → CASE WHEN (2) → OPTIONAL MATCH (3) → WITH (4). Each feature adds tokens and AST variants. SKIP is the safest warm-up. CASE WHEN extends `Expr` without restructuring the query. OPTIONAL MATCH restructures MATCH compilation. WITH is the deepest change and benefits from all previous structural work being stable.

### GqlQuery Construction Sites Will Break on Each Feature

Every time a new field is added to `GqlQuery`, all construction sites must be fixed. At project start there are 3 sites (ast.rs tests, parser.rs `parse`, parser.rs `parse_after_match`). By Feature 4, there will be 7 fields. Use the compiler error list to find all sites; do not search manually.

### The `parse` vs `parse_statement` Duality

`parse` (line 191) is the legacy read-only API. `parse_statement` (line 220) is the full API used by the server. Both must be updated for each feature. The integration test helper `run(&g, query)` uses `gql::parse(query)` → `gql::execute(graph, &ast)` which routes through `parse`. Verify which API path is used before each phase by reading `src/gql/mod.rs`.

### PatternMatch::merge Compatibility

`PatternMatch::merge` is used in the multi-MATCH cross-join (compiler.rs line 623). The same merge is needed for OPTIONAL MATCH when a compatible optional row exists. Verify the merge function handles duplicate variable names (it should overwrite with the right-hand side — acceptable since compatible rows agree on shared variables by definition).

### WITH Scope Validation Is Non-Trivial

After WITH, `validate_scope` must know which stage a variable belongs to. The cleanest approach: when `with_clause` is Some, collect WITH aliases as the "second-stage bound vars", and validate RETURN/ORDER BY/SKIP/LIMIT expressions against those aliases only. Stage-1 WHERE is validated against MATCH-bound vars as usual. If a RETURN item references a MATCH variable directly (bypassing WITH), it should be a compile error: "variable 'n' is not in scope after WITH — use an alias".
