# TDD Plan: GQL Block 3 — Advanced Features

**Date**: 2026-04-03
**Branch**: `feature/gql-block3-advanced` (from `develop`)
**Repo**: MIT core — `/Volumes/WD_BLACK/repos/MojoBytes/tessera-ecosystem/tessera-graph`

---

## Context

Block 3 implements 11 advanced GQL features on top of a complete Block 1 (variable-length
paths, shortestPath) and Block 2 (SKIP, CASE WHEN, OPTIONAL MATCH, single WITH). The GQL engine
lives in six files under `tessera-graph/crates/tessera-graph/src/gql/`. All work is in the MIT
core repo unless noted.

**Stack detected**: Rust 2024, `clippy::all = deny`, `clippy::pedantic = warn`,
`unsafe_code = "forbid"`, feature flag `extended-gql`.

**Conventions observed** (from source reading):
- Unit tests inline under `#[cfg(test)] mod tests` in the same file as the code under test.
- Integration tests in `crates/tessera-graph/tests/integration/gql_compiler.rs`, using the
  `run(&graph, "GQL")` helper.
- All new features gated under `#[cfg(feature = "extended-gql")]` unless genuinely core.
- `Error::GqlSyntaxError { line, col, message }` for parser errors.
- `Error::GqlCompileError(String)` for semantic/runtime errors.
- `expr_depth` guard at `MAX_EXPR_DEPTH = 128` is already present in the parser; new recursive
  expression forms must call `enter_expr()` / `exit_expr()`.
- `GqlQuery` is currently a flat struct (match, where, return, order_by, limit). Block 2 adds
  `skip` and `with`. Block 3 must extend or replace this flat representation for pipelines.

**Affects hot path**: Sub-block 3A (UNWIND, UNION) and 3D (FOREACH) affect result-set
cardinality and mutation pipelines respectively. 3C (EXPLAIN/PROFILE) adds no runtime cost to
normal queries. No sub-block modifies `compile_path_pattern` directly, so MATCH throughput
does not regress. Performance tests required only for UNWIND (row explosion) and FOREACH
(mutation loop).

---

## Architectural Decisions Required Before Implementation

**BLOCKER — Decide pipeline representation before writing any Block 3 code.**

The flat `GqlQuery` struct cannot represent multi-stage pipelines (`MATCH...WITH...UNWIND...RETURN`)
or UNION. Two options exist:

### Option A: Statement-level pipeline (recommended)

Replace the dispatch model with a `GqlPipeline` top-level type that holds a `Vec<PipelineStage>`
where each stage is one of:

```
enum PipelineStage {
    Match(MatchClause),
    Where(WhereClause),
    With(WithClause),           // from Block 2
    OptionalMatch(MatchClause), // from Block 2
    Unwind(UnwindClause),
    Return(ReturnClause),
    OrderBy(OrderByClause),
    Skip(SkipClause),
    Limit(LimitClause),
}
```

`GqlQuery` becomes `GqlPipeline { stages: Vec<PipelineStage> }`. The compiler executes stages
sequentially against a running `Vec<GqlRow>` binding environment instead of a single `PatternMatch`.

UNION sits above the pipeline level: `GqlStatement::Query(UnionQuery { branches: Vec<GqlPipeline>,
all: bool })`.

### Option B: Extend the flat struct incrementally (not recommended)

Adds `unwind`, `union_branches`, etc. as optional fields. Becomes unmaintainable beyond 3-4
features and produces O(n) None-checks on every compile.

**Recommendation**: Adopt Option A. It is the only design that naturally accommodates FOREACH,
multi-stage WITH, EXISTS subqueries, and list comprehensions without structural rewrites mid-block.

**This decision must be made and approved before starting Sub-block 3A.**

---

## Sub-block 3A — Data Manipulation (medium complexity)

**Estimated time**: 6–8 hours total.
**Features**: UNWIND, UNION, EXISTS subqueries.
**Priority order within sub-block**: UNWIND first (unblocks list pipelines), then UNION
(independent result sets), then EXISTS (subquery evaluation, most complex).
**Dependency**: `Literal::List` and `GqlValue::List` already exist under `extended-gql`.
List literals in expressions (`[1, 2, 3]`) are already parseable via `parse_list_literal`.
UNWIND and list comprehensions in 3C can share the list-iteration infrastructure built here.

---

### Phase 3A.1 — UNWIND (3–4 hours)

#### Grammar
```
UNWIND expr AS var
```
`expr` is any expression evaluating to `GqlValue::List`. Each list element binds `var` in a new
row, exploding the result set. `UNWIND [1,2,3] AS x RETURN x` produces 3 rows.

#### 3A.1.1 — AST (20 min)

File: `src/gql/ast.rs`

Add to the pipeline stage vocabulary (if Option A is adopted) or as a field on `GqlQuery`:

```rust
/// An UNWIND clause: explodes a list expression into rows.
#[derive(Debug, Clone, PartialEq)]
pub struct UnwindClause {
    /// The list-valued expression to iterate.
    pub list_expr: Expr,
    /// The variable name bound to each element.
    pub element_var: String,
}
```

Add `PipelineStage::Unwind(UnwindClause)` to the stage enum.

Key tests (inline, `ast.rs`):
- `unwind_clause_stores_expr_and_var` — construct, assert fields.

#### 3A.1.2 — Token + Lexer (10 min)

File: `src/gql/token.rs`

Add `Token::Unwind` to the Keywords section. Add display `"UNWIND"`. Add `"UNWIND" =>
Some(Token::Unwind)` to `keyword_from_str` in `lexer.rs`.

Key tests (inline, `token.rs`): `token_display_unwind`.

#### 3A.1.3 — Parser (30 min)

File: `src/gql/parser.rs`

Add `parse_unwind_clause`:
```
Token::Unwind consumed → parse_expr() (must produce a list) → Token::As → expect_ident()
```
Wire into `parse_statement` / `parse_pipeline` so `UNWIND` is accepted between `WITH` and `RETURN`.

The parser must accept `UNWIND [1,2,3] AS x RETURN x` (UNWIND without a preceding MATCH).

Key tests (inline, `parser.rs`, under `#[cfg(feature = "extended-gql")]`):
- `parse_unwind_literal_list` — `UNWIND [1,2,3] AS x RETURN x` parses to correct AST.
- `parse_unwind_variable` — `MATCH (n) WITH collect(n.age) AS ages UNWIND ages AS a RETURN a`
  parses without error.
- `parse_unwind_missing_as_fails` — `UNWIND [1] x RETURN x` returns `GqlSyntaxError`.

#### 3A.1.4 — Compiler (45 min)

File: `src/gql/compiler.rs`

Add a pipeline-aware execution loop. When the stage sequence contains an `UnwindClause`:
1. Evaluate `list_expr` against each current row to get a `GqlValue::List`.
2. For each element, clone the current row and bind `element_var` to the element value.
3. Non-list values produce zero rows (consistent with Cypher `UNWIND null AS x` → 0 rows).

```rust
fn apply_unwind(rows: Vec<GqlRow>, clause: &UnwindClause) -> Vec<GqlRow> {
    rows.into_iter().flat_map(|row| {
        let val = eval_expr_on_row(&clause.list_expr, &row);
        match val {
            GqlValue::List(items) => items.into_iter().map(move |item| {
                let mut new_row = row.clone();
                new_row.insert(clause.element_var.clone(), item);
                new_row
            }).collect::<Vec<_>>(),
            GqlValue::Null => vec![],
            _ => vec![], // non-list → zero rows
        }
    }).collect()
}
```

Note: `eval_expr_on_row` is a new helper that evaluates expressions against a `GqlRow` (the
output binding environment) rather than a `PatternMatch` (the input graph binding). This helper
is also needed for WITH, CASE WHEN from Block 2.

Key tests (integration, `tests/integration/gql_compiler.rs`):
- `unwind_literal_list_produces_rows` — `UNWIND [1,2,3] AS x RETURN x` → 3 rows with values 1, 2, 3.
- `unwind_null_produces_no_rows` — `UNWIND null AS x RETURN x` → 0 rows.
- `unwind_non_list_produces_no_rows` — `UNWIND 42 AS x RETURN x` → 0 rows.
- `unwind_after_collect` — `MATCH (n:Person) WITH collect(n.age) AS ages UNWIND ages AS a RETURN a`
  → one row per person with their age.
- `unwind_with_filter` — `UNWIND [1,2,3,4,5] AS x WHERE x > 3 RETURN x` → rows with 4 and 5.

#### 3A.1.5 — Wiring Verification (15 min)

`cargo test --features extended-gql -p tessera-graph` passes with zero new failures.
`cargo clippy --features extended-gql -p tessera-graph -- -D warnings` is clean.

---

### Phase 3A.2 — UNION (2–3 hours)

#### Grammar
```
query1 UNION query2
query1 UNION ALL query2
```
`UNION` deduplicates (by row content); `UNION ALL` retains duplicates. Each branch is an
independent pipeline. Column names must match (compile-time check: same set of return aliases).

#### 3A.2.1 — AST (20 min)

File: `src/gql/ast.rs`

```rust
/// A UNION query combining two or more pipelines.
#[derive(Debug, Clone, PartialEq)]
pub struct UnionQuery {
    /// The ordered branches to combine.
    pub branches: Vec<GqlPipeline>,
    /// `true` for UNION ALL (no deduplication).
    pub all: bool,
}
```

Update `GqlStatement::Query` to hold `UnionQuery` (which for the single-pipeline case wraps a
single-element `branches` vec).

#### 3A.2.2 — Token (10 min)

`Token::Union` already covered by `Ident` detection in the parser (UNION is not a reserved word
in most implementations), but promoting it to a dedicated token is cleaner and avoids
case-sensitivity issues. Add `Token::Union` and `"UNION" => Some(Token::Union)` in the lexer.
Add `Token::All` and `"ALL" => Some(Token::All)` (also needed for UNION ALL; `ALL` does not
conflict with existing keywords).

#### 3A.2.3 — Parser (45 min)

File: `src/gql/parser.rs`

After parsing the first pipeline, check for `Token::Union`. If found:
1. Consume `Token::Union`.
2. Consume optional `Token::All` → set `all = true`.
3. Parse the next pipeline.
4. Repeat while `Token::Union` appears.
5. Column validation: all branches must return the same column names (check at parse time using
   alias names from each `ReturnClause`; reject with `GqlSyntaxError` if mismatched).

Key tests (inline, `parser.rs`):
- `parse_union_two_branches` — parses correctly into `UnionQuery { branches: [_,_], all: false }`.
- `parse_union_all` — `UNION ALL` sets `all = true`.
- `parse_union_column_mismatch_fails` — branches with different RETURN aliases produce a parse error.
- `parse_union_without_return_on_first_branch_fails`.

#### 3A.2.4 — Compiler (30 min)

File: `src/gql/compiler.rs`

```rust
fn execute_union(graph, query: &UnionQuery) -> GqlResult {
    let mut combined: GqlResult = Vec::new();
    for branch in &query.branches {
        combined.extend(execute_pipeline(graph, branch)?);
    }
    if !query.all {
        deduplicate(&mut combined); // same logic as DISTINCT
    }
    Ok(combined)
}
```

Key tests (integration):
- `union_merges_two_labels` — `MATCH (a:Person) RETURN a.name UNION MATCH (b:Company) RETURN b.name`
  → distinct names from both labels.
- `union_all_preserves_duplicates` — if the same name appears in both branches, `UNION` removes it,
  `UNION ALL` keeps both.
- `union_empty_branch` — one branch returns zero rows, other returns 2 → result is 2 rows.
- `union_ordering_is_branch_order` — rows from first branch appear before rows from second branch
  (before any ORDER BY).

---

### Phase 3A.3 — EXISTS Subqueries (1–2 hours)

#### Grammar
```
WHERE EXISTS { MATCH (n)-[:KNOWS]->(m) }
```
`EXISTS { ... }` evaluates to `true` if the inner MATCH returns at least one row given the outer
binding context. Variables bound in the outer query are visible inside the subquery; variables
bound only inside the subquery are not visible outside.

#### 3A.3.1 — AST (15 min)

File: `src/gql/ast.rs`

Add to `Expr`:
```rust
/// An existential subquery check (only under `extended-gql`).
#[cfg(feature = "extended-gql")]
ExistsSubquery {
    /// The inner MATCH pattern to test for existence.
    inner_match: Box<MatchClause>,
    /// Optional WHERE predicate inside the subquery.
    inner_where: Option<Box<WhereClause>>,
},
```

#### 3A.3.2 — Token (5 min)

`EXISTS` can be treated as a keyword. Add `Token::Exists` and `"EXISTS" => Some(Token::Exists)`.
`{` and `}` are already `Token::LBrace` / `Token::RBrace`.

#### 3A.3.3 — Parser (30 min)

File: `src/gql/parser.rs`

In `parse_primary` (under `extended-gql`), add:
```
Token::Exists → advance → Token::LBrace → parse_match_clause → optional parse_where_clause
→ Token::RBrace → return Expr::ExistsSubquery { ... }
```

The inner parser context must allow referencing outer variables (no scope check inside the
subquery at parse time; scope is validated at compile time).

Key tests (inline, `parser.rs`, under `#[cfg(feature = "extended-gql")]`):
- `parse_exists_basic` — parses `WHERE EXISTS { MATCH (n)-[:KNOWS]->(m) }` without error.
- `parse_exists_with_inner_where` — parses `WHERE EXISTS { MATCH (n)-[:KNOWS]->(m) WHERE m.age > 25 }`.
- `parse_exists_missing_brace_fails`.

#### 3A.3.4 — Compiler (30 min)

File: `src/gql/compiler.rs`

In `eval_expr`, add the `ExistsSubquery` arm:
```rust
Expr::ExistsSubquery { inner_match, inner_where } => {
    // Compile the inner MATCH against the graph.
    let mut inner_matches = compile_match(graph, inner_match)?;
    // Filter by inner WHERE if present.
    if let Some(wc) = inner_where {
        inner_matches.retain(|pm| eval_as_tribool(&eval_expr(&wc.predicate, pm)) == Some(true));
    }
    GqlValue::Bool(!inner_matches.is_empty())
}
```

Note: `eval_expr` currently takes `(expr, pm: &PatternMatch)`. The pipeline refactor must pass
the graph reference through as well, since `ExistsSubquery` needs it. This means `eval_expr`
signature becomes `(expr, pm: &PatternMatch, graph: &dyn GraphAccess)`.

Key tests (integration):
- `exists_true_when_pattern_matches` — node with outgoing edge, EXISTS returns true.
- `exists_false_when_no_match` — isolated node, EXISTS returns false.
- `exists_with_inner_filter_true` — EXISTS with inner WHERE that passes.
- `exists_with_inner_filter_false` — EXISTS with inner WHERE that filters everything out.
- `exists_references_outer_variable` — `WHERE EXISTS { MATCH (n)-[:KNOWS]->(m) }` where `n` is
  bound in the outer MATCH.

#### 3A Wiring Verification

`cargo test --features extended-gql -p tessera-graph` passes.
All three features (UNWIND, UNION, EXISTS) work together in one pipeline test:
```
MATCH (n:Person) WITH collect(n.name) AS names UNWIND names AS name
WHERE EXISTS { MATCH (p:Person {name: name}) } RETURN name
```

---

## Sub-block 3B — Expression Enhancements (low–medium complexity)

**Estimated time**: 4–5 hours total.
**Features**: Regex matching, map projections, path variables.
**Priority order**: Regex first (self-contained `BinOp` addition), then path variables (new
`GqlValue` variant needed by 3E), then map projections (syntax convenience).
**Dependency**: Path variables require `GqlValue::Path`; map projections require no new runtime
types.

---

### Phase 3B.1 — Regex Matching (1.5 hours)

#### Grammar
```
WHERE n.name =~ '.*pattern.*'
```
`=~` is the regex operator. The right-hand side must be a string literal at parse time.
At runtime, the left-hand side is evaluated and matched against the compiled regex.
Runtime regex compilation is cached per-query using a `HashMap<String, Regex>` built in the
compiler before execution begins.

#### 3B.1.1 — Token (10 min)

The `~` character is currently rejected by the lexer (falls through to the `unknown` error arm).

File: `src/gql/token.rs` — add `Token::TildaEq` (or `Token::RegexMatch`).
File: `src/gql/lexer.rs` — add `b'=' then b'~'` as a two-character token in the `=` arm
(extend `lex_greater_than`-style): when `b` is `=` and next is `~`, emit `Token::RegexMatch`.

Wait — current lexer handles `=` as a single character `Token::Eq`. To lex `=~` we must peek
at the next byte after `=`. Modify the `b'='` match arm:
```rust
b'=' => {
    self.advance();
    if self.peek() == Some(b'~') {
        self.advance();
        Token::RegexMatch
    } else {
        Token::Eq
    }
}
```

#### 3B.1.2 — BinOp + AST (10 min)

File: `src/gql/ast.rs`

Add under `extended-gql`:
```rust
/// `=~` — regex match operator.
#[cfg(feature = "extended-gql")]
Regex,
```

#### 3B.1.3 — Parser (20 min)

File: `src/gql/parser.rs`

In `parse_comparison`, add a check for `Token::RegexMatch` (under `extended-gql`):
```rust
#[cfg(feature = "extended-gql")]
if *self.peek() == Token::RegexMatch {
    self.advance();
    let right = self.parse_addition()?;
    return Ok(Expr::BinaryOp {
        left: Box::new(left),
        op: BinOp::Regex,
        right: Box::new(right),
    });
}
```

Key tests (inline):
- `parse_regex_operator` — `n.name =~ '.*Alice.*'` parses to `BinaryOp { op: BinOp::Regex, ... }`.
- `parse_regex_on_non_string_rhs_parses_ok` — parse-time does not reject non-literal RHS; that is
  a runtime concern.

#### 3B.1.4 — Compiler (30 min)

**Dependency**: The `regex` crate must be added to `tessera-graph`'s `[dependencies]` section under
`optional = true, features = []`, activated by the `extended-gql` feature.

File: `tessera-graph/Cargo.toml`:
```toml
[features]
extended-gql = ["dep:regex"]

[dependencies]
regex = { version = "1", optional = true }
```

File: `src/gql/compiler.rs`

In `eval_binary_op`, add the `BinOp::Regex` arm:
```rust
#[cfg(feature = "extended-gql")]
BinOp::Regex => {
    match (lv, rv) {
        (GqlValue::Str(text), GqlValue::Str(pattern)) => {
            match regex::Regex::new(pattern) {
                Ok(re) => GqlValue::Bool(re.is_match(text)),
                Err(_) => GqlValue::Null, // invalid pattern → NULL (not an error)
            }
        }
        _ => GqlValue::Null,
    }
}
```

Note: compiling a regex on every row evaluation is O(n*m). For a follow-up quality pass,
introduce a `QueryContext` struct passed through execution that caches compiled `Regex`
instances keyed by pattern string. That optimisation is not required for correctness and can
be deferred to a quality-fixes plan.

Key tests (integration):
- `regex_match_basic` — `WHERE n.name =~ '.*lic.*'` matches `'Alice'`.
- `regex_match_no_match` — `WHERE n.name =~ '^Z'` matches nothing.
- `regex_null_lhs_returns_null` — `WHERE null =~ '.*'` evaluates to NULL, row excluded.
- `regex_invalid_pattern_returns_null` — `WHERE n.name =~ '[invalid'` produces NULL, row excluded.

---

### Phase 3B.2 — Path Variables (2 hours)

#### Grammar
```
MATCH p = (a)-[r]->(b) RETURN p
```
`p` is bound to the entire matched path. `RETURN p` returns a `GqlValue::Path` containing the
sequence of (node_id, edge_id, node_id, ...) or equivalent representation.

This feature is required by Sub-block 3E (multi-stage WITH chains that pass path values).
It is also the natural companion to Block 1's variable-length paths.

#### 3B.2.1 — GqlValue::Path (20 min)

File: `src/gql/compiler.rs`

Add to `GqlValue` (under `extended-gql`):
```rust
/// A matched path — an alternating sequence of node IDs and edge IDs.
#[cfg(feature = "extended-gql")]
Path(Vec<PathElement>),
```

```rust
/// One element in a path value.
#[cfg(feature = "extended-gql")]
#[derive(Debug, Clone, PartialEq)]
pub enum PathElement {
    Node(crate::error::NodeId),
    Edge(crate::error::EdgeId),
}
```

Note: `EdgeId` may not exist yet in `crate::error`. Check the actual storage/error types and
use the concrete ID types available (e.g., `u64` wrappers). Use the types that `PatternMatch`
exposes via `get_node` and `get_edge`.

#### 3B.2.2 — AST (20 min)

File: `src/gql/ast.rs`

Path variable assignment is a prefix modifier on a `PathPattern`:
```rust
/// A MATCH path pattern with an optional path variable binding.
#[derive(Debug, Clone, PartialEq)]
pub struct PathPattern {
    /// Optional `p = ` prefix binding the entire path to a variable.
    pub path_var: Option<String>,   // NEW
    /// The leftmost node in the pattern.
    pub start: NodePattern,
    /// Each subsequent (edge, node) hop along the path.
    pub hops: Vec<(EdgePattern, NodePattern)>,
}
```

All existing `PathPattern` construction sites must add `path_var: None`. Count: at least
`parse_path_pattern` in parser.rs and direct constructions in tests.

#### 3B.2.3 — Parser (25 min)

File: `src/gql/parser.rs`

In `parse_path_pattern`, before parsing the first node:
```
if peek() == Token::Ident(_) && peek_ahead(1) == Token::Eq {
    path_var = Some(expect_ident()?);
    advance(); // consume '='
}
```

Key tests (inline, under `extended-gql`):
- `parse_path_variable_binding` — `p = (a)-[r]->(b)` parses with `path_var = Some("p")`.
- `parse_no_path_variable` — `(a)-[r]->(b)` parses with `path_var = None`.

#### 3B.2.4 — Compiler (30 min)

File: `src/gql/compiler.rs`

In `compile_path_pattern`, after collecting matches, if `pp.path_var` is `Some(name)`:
1. For each `PatternMatch`, reconstruct the path from the bound node and edge variables in order.
2. Build `GqlValue::Path(elements)` and insert it into the match's extra bindings under the
   path variable name.

This requires `PatternMatch` to expose the ordered sequence of bound node/edge IDs. Check whether
`PatternMatch::merge` preserves ordering — likely not for cross-joins, but path variables are
only meaningful for single connected patterns (one continuous `PathPattern`).

Key tests (integration):
- `path_variable_captures_nodes_and_edges` — `MATCH p = (a)-[r]->(b) RETURN p` returns a
  `GqlValue::Path` with alternating node/edge elements.
- `path_variable_length` — path for a 2-hop pattern has 5 elements (node, edge, node, edge, node).
- `path_variable_in_return_length_function` — `RETURN length(p)` where `length` is a new built-in
  that counts hops (edges) in a path value.

---

### Phase 3B.3 — Map Projections (1 hour)

#### Grammar
```
RETURN n { .name, .age }
RETURN n { .name, score: n.score * 2 }
```
A map projection `n { ... }` returns a map (which maps to `GqlValue::Map`) containing the
specified properties from the node bound to `n`.

#### 3B.3.1 — GqlValue::Map (15 min)

File: `src/gql/compiler.rs`

Add to `GqlValue` (under `extended-gql`):
```rust
#[cfg(feature = "extended-gql")]
Map(std::collections::HashMap<String, Box<Self>>),
```

#### 3B.3.2 — AST (15 min)

File: `src/gql/ast.rs`

Add to `Expr` (under `extended-gql`):
```rust
/// A map projection: `n { .name, .age, alias: expr }`.
#[cfg(feature = "extended-gql")]
MapProjection {
    /// The variable holding the node or map to project from.
    var: String,
    /// Each projected field: either `.prop` (shorthand) or `alias: expr`.
    fields: Vec<MapProjectionField>,
},
```

```rust
#[cfg(feature = "extended-gql")]
#[derive(Debug, Clone, PartialEq)]
pub enum MapProjectionField {
    /// `.propName` — shorthand for `propName: var.propName`.
    Property(String),
    /// `alias: expr` — full form.
    Aliased { alias: String, expr: Expr },
}
```

#### 3B.3.3 — Parser (20 min)

File: `src/gql/parser.rs`

In `parse_primary`, after resolving an identifier and checking for `Token::LParen` (function call)
and `Token::Dot` (property access), add a check for `Token::LBrace` (map projection):

```rust
if *self.peek() == Token::LBrace {
    // parse map projection fields
    self.advance(); // consume '{'
    let mut fields = Vec::new();
    if *self.peek() != Token::RBrace {
        fields.push(self.parse_map_projection_field()?);
        while *self.peek() == Token::Comma { ... }
    }
    self.expect(&Token::RBrace)?;
    return Ok(Expr::MapProjection { var: name, fields });
}
```

Key tests: `parse_map_projection_shorthand`, `parse_map_projection_aliased`,
`parse_map_projection_mixed`.

#### 3B.3.4 — Compiler (15 min)

In `eval_expr`, the `MapProjection` arm looks up `var` in `pm`, evaluates each field, and
returns `GqlValue::Map(HashMap)`.

Key tests (integration):
- `map_projection_shorthand` — `RETURN n { .name, .age }` returns a map with `name` and `age`.
- `map_projection_aliased` — `RETURN n { display: n.name }` returns a map with key `display`.
- `map_projection_unknown_prop_is_null` — property not on node returns `null` in map.

#### 3B Wiring Verification

All three features independently pass. Integration test combining path variable and map projection:
`MATCH p = (a:Person)-[r:KNOWS]->(b:Person) RETURN a { .name }, b { .name }, length(p)`.

---

## Sub-block 3C — Procedural / Operational (medium–high complexity)

**Estimated time**: 6–8 hours total.
**Features**: CALL procedures, EXPLAIN/PROFILE, list comprehensions.
**Priority order**: List comprehensions first (pure expression, no infrastructure), then
EXPLAIN/PROFILE (no new runtime types), then CALL (requires procedure registry).

---

### Phase 3C.1 — List Comprehensions (1.5 hours)

#### Grammar
```
[x IN list WHERE x > 5]
[x IN list | x * 2]
[x IN list WHERE x > 5 | x * 2]
```
Evaluates to `GqlValue::List` filtered and/or transformed from the source list.
This is purely an expression form — no new pipeline stages.

#### 3C.1.1 — AST (20 min)

File: `src/gql/ast.rs`

Add to `Expr` (under `extended-gql`):
```rust
/// An inline list comprehension: `[x IN expr WHERE cond | projection]`.
#[cfg(feature = "extended-gql")]
ListComprehension {
    /// The iteration variable name.
    element_var: String,
    /// The source list expression.
    list_expr: Box<Self>,
    /// Optional filter predicate (the `WHERE cond` part).
    filter: Option<Box<Self>>,
    /// Optional projection (the `| expr` part). None means identity (keep elements as-is).
    projection: Option<Box<Self>>,
},
```

#### 3C.1.2 — Parser (45 min)

File: `src/gql/parser.rs`

List comprehensions start with `[` followed by an identifier followed by `IN`. This conflicts
with `parse_list_literal` which also starts with `[`. Disambiguation via 2-token look-ahead:

```
peek() == LBracket
  peek_ahead(1) == Ident  →  could be list comprehension or [ident, ...]
    peek_ahead(2) == Ident("IN")  →  list comprehension
    else  →  list literal
  else  →  list literal
```

Extract a `parse_list_or_comprehension` method in `parse_primary` that performs this dispatch.

Key tests (inline, under `extended-gql`):
- `parse_list_comprehension_filter_only` — `[x IN [1,2,3] WHERE x > 1]`.
- `parse_list_comprehension_projection_only` — `[x IN [1,2,3] | x * 2]`.
- `parse_list_comprehension_both` — `[x IN [1,2,3] WHERE x > 1 | x * 2]`.
- `parse_list_literal_not_confused_with_comprehension` — `[1,2,3]` still parses as a literal.

#### 3C.1.3 — Compiler (30 min)

File: `src/gql/compiler.rs`

In `eval_expr`, the `ListComprehension` arm:
1. Evaluate `list_expr` → must be `GqlValue::List`.
2. For each element, create a temporary row with `element_var` bound to the element.
3. Evaluate `filter` (if present); skip element if false.
4. Evaluate `projection` (if present) to get the output value; otherwise use element directly.
5. Collect results into `GqlValue::List`.

Key tests (integration):
- `list_comprehension_filter_reduces_list` — `[x IN [1,2,3,4,5] WHERE x > 3]` → `[4, 5]`.
- `list_comprehension_projection_transforms` — `[x IN [1,2,3] | x * 2]` → `[2, 4, 6]`.
- `list_comprehension_filter_and_projection` — `[x IN [1,2,3,4] WHERE x > 1 | x * 10]` → `[20, 30, 40]`.
- `list_comprehension_on_collected_values` — used after `collect()` in a WITH clause.
- `list_comprehension_empty_source` — `[x IN [] WHERE x > 0]` → `[]`.

---

### Phase 3C.2 — EXPLAIN / PROFILE (2 hours)

#### Grammar
```
EXPLAIN MATCH (n) RETURN n
PROFILE MATCH (n) RETURN n
```
`EXPLAIN` returns the query plan without executing it. `PROFILE` executes and returns execution
statistics alongside results.

This feature does not require any new GQL expression types. It is a statement-level prefix that
wraps any `GqlStatement::Query` or `GqlStatement::Mutation`.

#### 3C.2.1 — AST (15 min)

File: `src/gql/ast.rs`

Add to `GqlStatement` (at the top level, not under `extended-gql` since it is a utility):
```rust
/// An EXPLAIN wrapper — returns a query plan without executing.
Explain(Box<GqlStatement>),
/// A PROFILE wrapper — executes and returns plan + statistics.
Profile(Box<GqlStatement>),
```

Define a query plan structure:
```rust
/// A textual query plan, produced by EXPLAIN.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryPlan {
    /// Human-readable plan stages.
    pub stages: Vec<String>,
}
```

#### 3C.2.2 — Token (10 min)

Add `Token::Explain` and `Token::Profile`. Add to `keyword_from_str` and `Display`.

#### 3C.2.3 — Parser (20 min)

In `parse_statement`, handle `Token::Explain` and `Token::Profile` as prefix wrappers:
```rust
Token::Explain => {
    self.advance();
    let inner = self.parse_statement_inner()?; // factored-out non-EXPLAIN parse
    Ok(GqlStatement::Explain(Box::new(inner)))
}
```

Key tests (inline): `parse_explain_match`, `parse_profile_match`,
`parse_explain_create`, `parse_double_explain_fails` (EXPLAIN EXPLAIN is rejected).

#### 3C.2.4 — Plan Builder (30 min)

File: `src/gql/compiler.rs`

Add `build_query_plan(query: &GqlQuery) -> QueryPlan` that introspects the AST and returns
a human-readable plan. Initial implementation:

```rust
fn build_query_plan(query: &GqlQuery) -> QueryPlan {
    let mut stages = Vec::new();
    stages.push(format!("NodeScan(patterns={})", query.match_clause.patterns.len()));
    if query.where_clause.is_some() { stages.push("Filter".into()); }
    if is_aggregate { stages.push("Aggregate".into()); }
    stages.push("Project".into());
    if query.order_by.is_some() { stages.push("Sort".into()); }
    if query.skip.is_some() { stages.push("Skip".into()); }
    if query.limit.is_some() { stages.push("Limit".into()); }
    QueryPlan { stages }
}
```

Add `execute_with_profile(graph, query) -> (GqlResult, ExecutionStats)` that wraps `execute`
with timing measurements. `ExecutionStats` contains `rows_produced: u64, duration_us: u64`.

`GqlResult` returned for `EXPLAIN` is `vec![row]` where the row has a `"plan"` column
containing `GqlValue::Str(plan_text)`.
`PROFILE` returns the full result plus an extra trailing row with statistics (matching Cypher
profile output convention).

Key tests (integration):
- `explain_returns_plan_not_rows` — `EXPLAIN MATCH (n) RETURN n` returns exactly 1 row with key `plan`.
- `explain_plan_mentions_node_scan` — plan text contains "NodeScan".
- `profile_returns_rows_plus_stats` — `PROFILE MATCH (n) RETURN n` returns the query rows.
- `explain_does_not_modify_graph` — a CREATE inside an EXPLAIN block leaves the graph unchanged.

---

### Phase 3C.3 — CALL Procedures (2.5 hours)

#### Grammar
```
CALL db.indexes() YIELD name
CALL db.labels() YIELD label, count
CALL myproc(arg1, arg2) YIELD result
```
A procedure call invokes a named procedure from a registry, optionally yielding named output
columns. This requires an extensible procedure registry with a well-defined interface.

#### 3C.3.1 — Procedure Registry Interface (30 min)

File: `src/gql/procedures.rs` (new file, under `extended-gql`)

```rust
/// The result of a single procedure invocation.
pub type ProcedureResult = Vec<HashMap<String, GqlValue>>;

/// A registered GQL procedure.
pub trait Procedure: Send + Sync {
    fn name(&self) -> &str;
    fn call(&self, args: &[GqlValue]) -> crate::Result<ProcedureResult>;
}

/// Global procedure registry.
pub struct ProcedureRegistry {
    procedures: HashMap<String, Box<dyn Procedure>>,
}

impl ProcedureRegistry {
    pub fn new() -> Self { ... }
    pub fn register(&mut self, proc: Box<dyn Procedure>) { ... }
    pub fn get(&self, name: &str) -> Option<&dyn Procedure> { ... }
    pub fn default_registry() -> Self {
        let mut r = Self::new();
        r.register(Box::new(DbIndexesProcedure));
        r.register(Box::new(DbLabelsProcedure));
        r
    }
}
```

Built-in procedures (minimal):
- `db.indexes()` — returns `[{ name: "label_index" }]`.
- `db.labels()` — returns distinct labels in the graph as `[{ label: "Person" }, ...]`.

These require passing the graph to the procedure. The `Procedure` trait needs a `call` method
that accepts `&dyn GraphAccess`:

```rust
fn call(&self, args: &[GqlValue], graph: &dyn GraphAccess) -> crate::Result<ProcedureResult>;
```

#### 3C.3.2 — AST (20 min)

File: `src/gql/ast.rs`

```rust
/// A CALL clause invoking a procedure.
#[cfg(feature = "extended-gql")]
#[derive(Debug, Clone, PartialEq)]
pub struct CallClause {
    /// The procedure name (e.g. `"db.indexes"`).
    pub proc_name: String,
    /// The argument expressions passed to the procedure.
    pub args: Vec<Expr>,
    /// The columns to yield from the procedure result.
    /// If empty, all columns are yielded.
    pub yield_columns: Vec<String>,
}
```

Add `PipelineStage::Call(CallClause)` to the pipeline stage enum.

#### 3C.3.3 — Token (5 min)

Add `Token::Call` and `Token::Yield`. Add to `keyword_from_str` and `Display`.

#### 3C.3.4 — Parser (30 min)

File: `src/gql/parser.rs`

```
CALL → proc_name (dotted identifier: ident (. ident)*) → LParen → [args] → RParen
→ optional: YIELD ident, ident, ...
```

Key tests (inline, under `extended-gql`):
- `parse_call_no_args_no_yield` — `CALL db.indexes()`.
- `parse_call_with_yield` — `CALL db.indexes() YIELD name`.
- `parse_call_with_args` — `CALL myproc(n, 42) YIELD result`.
- `parse_call_dotted_name` — `db.indexes` parsed as a single procedure name `"db.indexes"`.

#### 3C.3.5 — Compiler (30 min)

File: `src/gql/compiler.rs`

Add a `compile_call` function in the pipeline executor that:
1. Looks up the procedure in the registry.
2. Calls it with evaluated arguments.
3. Filters columns by `yield_columns` if non-empty.
4. Merges result rows into the current pipeline binding context (cross-join with existing rows,
   or replace if CALL is the first stage).

The registry is passed as part of a `CompileContext` struct:
```rust
pub struct CompileContext<'g, G: GraphAccess + ?Sized> {
    pub graph: &'g G,
    pub procedures: &'g ProcedureRegistry,
}
```

Key tests (integration):
- `call_db_labels_returns_labels` — after adding Person nodes, `CALL db.labels() YIELD label`
  returns a row with `label = "Person"`.
- `call_db_indexes_returns_label_index` — `CALL db.indexes() YIELD name` returns a row with
  `name = "label_index"`.
- `call_unknown_procedure_fails` — `CALL nonexistent()` returns `GqlCompileError`.
- `call_yield_nonexistent_column_fails` — `CALL db.labels() YIELD nonexistent` returns error.

#### 3C Wiring Verification

All three features pass independently. Compound test:
`CALL db.labels() YIELD label WITH label MATCH (n {name: 'Alice'}) RETURN n.name, label`.

---

## Sub-block 3D — FOREACH (medium–high complexity)

**Estimated time**: 3–4 hours.
**Feature**: FOREACH loop mutations.

### Grammar
```
FOREACH (x IN [1,2,3] | CREATE (:N {val: x}))
FOREACH (x IN list | SET x.visited = true)
```
`FOREACH` iterates a list and applies a mutation (CREATE, SET, DELETE, MERGE) for each element.
The element variable is bound only inside the body. This is a mutation context, not a RETURN
context, so it does not produce output rows.

#### Dependency

`Literal::List`, list expressions, and `GqlValue::List` are already available. FOREACH does
not require UNWIND. However, it shares the concept of "bind a variable and evaluate a body"
with list comprehensions from 3C.1.

---

### Phase 3D.1 — AST (20 min)

File: `src/gql/ast.rs`

```rust
/// A FOREACH clause: iterates a list and applies mutations.
#[derive(Debug, Clone, PartialEq)]
pub struct ForeachClause {
    /// The loop variable name.
    pub element_var: String,
    /// The source list expression.
    pub list_expr: Expr,
    /// The mutation body applied for each element.
    pub body: ForeachBody,
}

/// The allowed mutation forms inside FOREACH.
#[derive(Debug, Clone, PartialEq)]
pub enum ForeachBody {
    Create(CreateClause),
    Set(SetClause),
    Delete(DeleteClause),
    Merge(MergeClause),
}
```

Add `Token::Foreach` and `"FOREACH" => Some(Token::Foreach)` in lexer.

---

### Phase 3D.2 — Parser (45 min)

File: `src/gql/parser.rs`

Grammar:
```
FOREACH ( ident IN expr | mutation_clause )
```
The `|` separator is already `Token::Pipe`.

```rust
fn parse_foreach_clause(&mut self) -> crate::Result<ForeachClause> {
    self.expect(&Token::Foreach)?;
    self.expect(&Token::LParen)?;
    let element_var = self.expect_ident()?;
    // "IN" is not a keyword token; check as Ident case-insensitively
    self.expect_keyword_ident("IN")?;
    let list_expr = self.parse_expr()?;
    self.expect(&Token::Pipe)?;
    let body = self.parse_foreach_body()?;
    self.expect(&Token::RParen)?;
    Ok(ForeachClause { element_var, list_expr, body })
}
```

`parse_foreach_body` dispatches on `Token::Create | Token::Set | Token::Delete | Token::Detach | Token::Merge`.

Key tests (inline):
- `parse_foreach_create` — `FOREACH (x IN [1,2,3] | CREATE (:N {val: x}))`.
- `parse_foreach_set` — `FOREACH (x IN list | SET x.done = true)`.
- `parse_foreach_missing_pipe_fails`.
- `parse_foreach_missing_in_fails`.

---

### Phase 3D.3 — Compiler (45 min)

File: `src/gql/compiler.rs`

FOREACH is a mutation statement. It can follow a MATCH or stand alone. The compiler:
1. Evaluates `list_expr` against the current binding context (if after MATCH, uses the matched rows).
2. For each element:
   a. Bind `element_var` to the element value.
   b. Evaluate `body` as a mutation, substituting `element_var` for parameter references.

The main challenge is substituting the loop variable into the mutation body. For `CREATE (:N {val: x})`,
`x` is a variable reference inside the `Literal` position of an inline prop — but inline props
only accept `Literal`, not `Expr`. This is a structural constraint in the current AST.

**Two resolution strategies**:
1. Promote inline props to accept `Expr` (large refactor, blocks 3D).
2. Require FOREACH body to use SET for variable-derived properties: `FOREACH (x IN list | CREATE (n:N) SET n.val = x)`.
   This is more conservative but avoids an AST refactor.

**Recommendation**: Use strategy 2 for Block 3. FOREACH body allows `CREATE` (label only, no
inline props with variables), followed by an optional `SET`. Document this constraint explicitly.
Promote inline props to `Expr` in a future quality pass.

Key tests (integration):
- `foreach_create_nodes` — `FOREACH (x IN [1,2,3] | CREATE (:N))` creates 3 nodes.
- `foreach_set_properties` — `MATCH (n:Person) FOREACH (x IN [1] | SET n.visited = true)` sets
  `visited` on all Person nodes.
- `foreach_empty_list_no_mutations` — `FOREACH (x IN [] | CREATE (:N))` creates 0 nodes.
- `foreach_nested_list_from_collect` — `MATCH (n:Person) WITH collect(n) AS nodes FOREACH (n IN nodes | SET n.processed = true)`.

#### 3D Wiring Verification

`cargo test --features extended-gql -p tessera-graph`. Verify no regressions in existing mutation tests.

---

## Sub-block 3E — Multi-Stage WITH Chains (low complexity given pipeline refactor)

**Estimated time**: 2–3 hours.
**Feature**: Arbitrary-depth WITH pipelines.

### Grammar
```
MATCH (n:Person)
WITH n.name AS name, n.age AS age
WHERE age > 25
WITH name, count(*) AS count
RETURN name, count ORDER BY count DESC
```

Block 2 implements a single WITH clause. Sub-block 3E extends the pipeline to accept any
number of WITH stages in sequence. This is the simplest sub-block **after** the Option A
pipeline refactor is in place, because a pipeline already holds `Vec<PipelineStage>` and
parsing multiple WITH clauses is just a loop.

---

### Phase 3E.1 — Parser (30 min)

File: `src/gql/parser.rs`

In `parse_pipeline`, after parsing a WITH clause, check if the next token is another pipeline
clause keyword (`WITH`, `MATCH`, `OPTIONAL`, `UNWIND`, `WHERE`, `RETURN`). If `WITH` again,
parse another WITH stage and push it onto the pipeline. Continue until a terminating clause.

The parse loop becomes:
```
while peek() != Token::Return && peek() != Token::Eof {
    match peek() {
        Token::Match    → push MatchStage
        Token::Where    → push WhereStage
        Token::With     → push WithStage
        Token::Optional → push OptionalMatchStage
        Token::Unwind   → push UnwindStage
        Token::Call     → push CallStage
        Token::Foreach  → push ForeachStage (terminates non-RETURN pipelines)
        _ → error
    }
}
push ReturnStage
```

Key tests (inline):
- `parse_two_with_stages` — `MATCH ... WITH ... WITH ... RETURN` parses to 5 stages.
- `parse_three_with_stages`.
- `parse_with_where_between_withs` — `MATCH ... WITH ... WHERE ... WITH ... RETURN`.

---

### Phase 3E.2 — Compiler (1 hour)

File: `src/gql/compiler.rs`

Executing a second WITH stage means:
1. Current binding context is `Vec<GqlRow>` (from previous stage, not raw `PatternMatch`).
2. Evaluate the WITH projections against each `GqlRow` (using `eval_expr_on_row`).
3. Apply any WHERE that follows (also against `GqlRow`).
4. Pass the new `Vec<GqlRow>` to the next stage.

The key design point: after the first WITH stage, the execution context transitions from
`Vec<PatternMatch>` to `Vec<GqlRow>`. The pipeline executor must carry a unified binding
context type. Define:

```rust
enum BindingContext {
    Matches(Vec<PatternMatch>),
    Rows(Vec<GqlRow>),
}
```

Any subsequent WITH, UNWIND, WHERE, RETURN, ORDER BY, SKIP, LIMIT operates on `BindingContext::Rows`.
MATCH and OPTIONAL MATCH always operate against the graph and reset or extend the context.

Key tests (integration):
- `two_with_stages_chain` — `MATCH (n:Person) WITH n.name AS name WITH name AS result RETURN result`
  → each person's name returned.
- `with_where_chain` — `MATCH (n:Person) WITH n.name AS name, n.age AS age WHERE age > 25 WITH name RETURN name`
  → only people over 25.
- `with_aggregate_chain` — `MATCH (n:Person)-[r:KNOWS]->(m) WITH n.name AS knower, count(r) AS cnt WHERE cnt > 1 RETURN knower`
  → people who know more than one other person.
- `three_with_stages` — three sequential WITH stages each projecting a subset of columns.

---

### Phase 3E.3 — Wiring Verification (30 min)

End-to-end query combining features from all sub-blocks:
```gql
MATCH (a:Person)-[:KNOWS]->(b:Person)
WITH a.name AS name, collect(b.name) AS friends
UNWIND friends AS friend
WHERE EXISTS { MATCH (p:Person {name: friend}) WHERE p.age > 25 }
WITH name, friend
RETURN name, friend ORDER BY name, friend
```

`cargo test --features extended-gql -p tessera-graph` passes with zero failures.
`cargo clippy --features extended-gql -p tessera-graph -- -D warnings` is clean.

---

## Performance Notes

### UNWIND (3A.1) — row explosion monitoring

UNWIND is not on the hot MATCH path, but large list explosions can produce pathological memory
growth. Add a guard in `apply_unwind`:

```rust
const MAX_UNWIND_ROWS: usize = 1_000_000;

if result.len() > MAX_UNWIND_ROWS {
    return Err(Error::GqlCompileError(format!(
        "UNWIND produced more than {MAX_UNWIND_ROWS} rows; add a LIMIT"
    )));
}
```

This is a safety boundary, not a performance test. No benchmark required since UNWIND is
post-MATCH pipeline work, not graph traversal.

### FOREACH (3D) — mutation loop

FOREACH on large lists calls the mutation executor repeatedly. Add a test:
- `foreach_large_list_creates_many_nodes` — `FOREACH (x IN [1..1000] | CREATE (:N))` completes
  in under 2 seconds (wall clock assertion in the test using `std::time::Instant`).

This is a regression guard (not a throughput benchmark) because FOREACH is not the primary
write path.

### Regex (3B.1) — no benchmark required

Regex compilation per-row is a known inefficiency, deferred to a quality pass. No throughput
guard needed at Block 3 stage because regex matching is never on the MATCH hot path.

---

## Dependency Map and Shipping Order

```
3A.1 UNWIND          ← independent (list literals already exist)
3A.2 UNION           ← independent (requires pipeline refactor)
3A.3 EXISTS          ← independent (requires eval_expr to accept graph)
3B.1 Regex           ← independent (pure expression)
3B.2 Path Variables  ← independent (extends PathPattern)
3B.3 Map Projection  ← independent (pure expression)
3C.1 List Compr.     ← independent (pure expression; shares concept with UNWIND)
3C.2 EXPLAIN/PROFILE ← independent (statement prefix)
3C.3 CALL            ← requires procedure registry (new infrastructure)
3D   FOREACH         ← independent mutation; shares list-iteration with 3C.1
3E   Multi-WITH      ← requires BindingContext from 3A.1 compiler work
```

**Critical path**: Pipeline refactor (Option A) → 3A.1 UNWIND → 3E Multi-WITH.
All other sub-blocks can ship in any order after the pipeline refactor.

---

## Estimation Summary

| Sub-block | Features           | Estimated Hours |
|-----------|--------------------|----------------|
| 3A        | UNWIND, UNION, EXISTS | 6–8 h        |
| 3B        | Regex, Path Vars, Map Proj | 4–5 h  |
| 3C        | List Compr., EXPLAIN, CALL | 6–8 h  |
| 3D        | FOREACH            | 3–4 h           |
| 3E        | Multi-WITH chains  | 2–3 h           |
| **Total** |                    | **21–28 h**     |

---

## Criteria de Éxito (Block 3 Complete)

- [ ] `cargo test --features extended-gql -p tessera-graph` passes with zero failures.
- [ ] `cargo clippy --features extended-gql -p tessera-graph -- -D warnings` is clean.
- [ ] `cargo clippy -p tessera-graph -- -D warnings` (without `extended-gql`) is clean.
- [ ] All 11 features have integration tests in `gql_compiler.rs` or dedicated test files.
- [ ] FOREACH large-list regression guard test passes under 2 seconds.
- [ ] UNWIND row-count guard rejects lists larger than 1,000,000 elements with a clear error.
- [ ] EXPLAIN does not execute mutations (verified by test).
- [ ] No existing passing test is broken by any sub-block.
