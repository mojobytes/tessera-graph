# TDD Plan: MATCH...CREATE in GQL Parser + JSON Import in CLI

## Context

The GQL parser in the MIT core (`tessera-graph`) does not support `MATCH...CREATE` syntax.
The executor already supports it — `execute_create` resolves edge source/target variables
from a `node_vars: HashMap<String, NodeId>` that is populated from MATCH results. The only
missing piece is the parser dispatch.

However, the parser change is **not trivial**: `parse_create_pattern_multi` currently requires
a label on every node pattern (`self.expect(&Token::Colon)?` at line 333). For MATCH...CREATE,
the CREATE clause references already-bound variables WITHOUT labels: `CREATE (a)-[:REL]->(b)`.

The CLI (`tessera-cli`) only supports `gql` and `csv-nodes` import formats. JSON import
is needed for bulk loading from external graph databases (Memgraph, Neo4j).

**Stack detected**: Rust 1.93, workspace edition 2024
**Affects hot path**: No — parser runs once per query, not per-node
**MIT core change**: Yes — `tessera-graph/src/gql/parser.rs`

---

## Prior Decisions

### D1 — Variable-only CREATE patterns

When MATCH precedes CREATE, the CREATE pattern may reference already-bound variables
without labels or properties: `CREATE (a)-[:REL]->(b)`. The parser must distinguish:

- `(a:Label {props})` → new node (current behavior, pushes `CreatePattern::Node`)
- `(a)` → variable reference (new behavior, does NOT push `CreatePattern::Node`)

The distinction is simple: after parsing the variable name, if the next token is NOT
`:` (colon), it's a variable reference. The parser records the variable name but does
not emit a `CreatePattern::Node`.

### D2 — JSON import generates GQL statements

The CLI JSON import reads `{"nodes": [...], "edges": [...]}` and generates GQL
statements that are executed one-by-one over Bolt:

- Nodes → `CREATE (:Label {prop1: 'val1', prop2: val2})`
- Edges → `MATCH (a {id: 'src_id'}) MATCH (b {id: 'tgt_id'}) CREATE (a)-[:REL]->(b)`

The edge match property is the first key in the `"match"` object of each endpoint.
This is the same format as `tessera-import`'s JSON spec.

### D3 — Edge import requires the MATCH...CREATE parser change

The JSON import of edges depends on MATCH...CREATE working end-to-end. Therefore,
the MIT core parser change (Fase 1) must be completed before the CLI JSON edge import
(Fase 2, Cycle 4) can be tested.

---

## Execution Plan

### Fase 1: MIT Core — MATCH...CREATE Parser Support

All changes in `/Volumes/WD_BLACK/repos/MojoBytes/tessera-ecosystem/tessera-graph/src/gql/parser.rs`.

#### Cycle 1.1 — Parser: MATCH...CREATE dispatches to Create

**RED** (10 min)

- File: `tessera-graph/src/gql/parser.rs`, inside `mod tests`
- Action: Add test after the existing `parse_match_then_delete` tests (~line 1797)

```rust
// ── MATCH...CREATE ──────────────────────────────────────────────────

#[test]
fn parse_match_then_create_edge() {
    let tokens = Lexer::new(
        "MATCH (a:Person {name: 'Alice'}) MATCH (b:Person {name: 'Bob'}) \
         CREATE (a)-[:KNOWS]->(b)"
    ).tokenize().unwrap();
    let stmt = Parser::new(tokens).parse_statement().unwrap();
    match stmt {
        GqlStatement::Mutation(ms) => {
            assert!(ms.match_clause.is_some(), "must have MATCH clause");
            match ms.mutation {
                MutationClause::Create(c) => {
                    // Should have exactly 1 Edge pattern (no Node patterns —
                    // a and b are variable references, not new nodes).
                    assert_eq!(c.patterns.len(), 1, "patterns: {c:?}");
                    match &c.patterns[0] {
                        CreatePattern::Edge { source_var, rel_label, target_var, .. } => {
                            assert_eq!(source_var, "a");
                            assert_eq!(rel_label, "KNOWS");
                            assert_eq!(target_var, "b");
                        }
                        other => panic!("expected Edge, got {other:?}"),
                    }
                }
                other => panic!("expected Create, got {other:?}"),
            }
        }
        other => panic!("expected Mutation, got {other:?}"),
    }
}
```

- Execute: `cargo test -p tessera-graph parse_match_then_create_edge`
- Expected: FAILS — parser returns syntax error "expected RETURN, DELETE, or SET after MATCH"

**GREEN** (15 min)

- File: `tessera-graph/src/gql/parser.rs`, `parse_statement` method (~line 260)
- Action: Add `Token::Create` arm before the error case

Change:
```rust
                    other => Err(self.syntax_error(format!(
                        "expected RETURN, DELETE, or SET after MATCH, found {other}"
                    ))),
```

To:
```rust
                    Token::Create => {
                        let create = self.parse_create_clause()?;
                        if *self.peek() != Token::Eof {
                            return Err(self.syntax_error(
                                "unexpected tokens after CREATE",
                            ));
                        }
                        Ok(GqlStatement::Mutation(MutationStatement {
                            match_clause: Some(match_clause),
                            mutation: MutationClause::Create(create),
                            set_clause: None,
                        }))
                    }
                    other => Err(self.syntax_error(format!(
                        "expected RETURN, DELETE, SET, or CREATE after MATCH, found {other}"
                    ))),
```

- Execute: test still FAILS — but now with a different error: `parse_create_pattern_multi`
  expects a colon after variable name, but finds `)` (because `(a)` has no label).

**GREEN continued** — Modify `parse_create_pattern_multi` to handle variable-only references.

- File: `tessera-graph/src/gql/parser.rs`, `parse_create_pattern_multi` (~line 320)
- Action: After parsing the optional variable, make the colon+label optional.
  If no colon follows, treat `(var)` as a variable reference — don't push a Node pattern.

Replace lines 332-346:
```rust
        // A label is mandatory for CREATE.
        self.expect(&Token::Colon)?;
        let label = self.expect_ident()?;

        let props = if *self.peek() == Token::LBrace {
            self.parse_inline_props()?
        } else {
            Vec::new()
        };

        self.expect(&Token::RParen)?;

        let source_var_name = var.clone().unwrap_or_else(|| format!("_anon_{}", out.len()));
        let node_idx = out.len();
        out.push(CreatePattern::Node { var, label, props });
```

With:
```rust
        // If a colon follows, this is a new node pattern with a label.
        // Otherwise it's a variable reference to an already-bound node.
        let is_var_ref = var.is_some() && *self.peek() != Token::Colon;

        let source_var_name;
        let node_idx = out.len();

        if is_var_ref {
            // Variable reference — no label, no props, no Node pattern emitted.
            source_var_name = var.clone().unwrap(); // safe: checked is_some() above
            self.expect(&Token::RParen)?;
        } else {
            // New node — label is mandatory.
            self.expect(&Token::Colon)?;
            let label = self.expect_ident()?;
            let props = if *self.peek() == Token::LBrace {
                self.parse_inline_props()?
            } else {
                Vec::new()
            };
            self.expect(&Token::RParen)?;
            source_var_name = var.clone().unwrap_or_else(|| format!("_anon_{}", out.len()));
            out.push(CreatePattern::Node { var, label, props });
        }
```

And similarly for the target node inside the edge continuation (lines 378-392).
Replace lines 378-404:
```rust
            // Parse the target node.
            self.expect(&Token::LParen)?;
            let target_var = if let Token::Ident(_) = self.peek() {
                Some(self.expect_ident()?)
            } else {
                None
            };
            // Target label is mandatory for inline CREATE patterns.
            self.expect(&Token::Colon)?;
            let target_label = self.expect_ident()?;
            let target_props = if *self.peek() == Token::LBrace {
                self.parse_inline_props()?
            } else {
                Vec::new()
            };
            self.expect(&Token::RParen)?;

            let target_var_name =
                target_var.clone().unwrap_or_else(|| format!("_anon_{}", out.len()));

            // Determine the actual source variable name from the already-pushed node.
            let edge_source = out[node_idx].var().map_or(source_var_name, String::from);

            out.push(CreatePattern::Node {
                var: target_var,
                label: target_label,
                props: target_props,
            });
```

With:
```rust
            // Parse the target node (or variable reference).
            self.expect(&Token::LParen)?;
            let target_var = if let Token::Ident(_) = self.peek() {
                Some(self.expect_ident()?)
            } else {
                None
            };

            let target_is_var_ref = target_var.is_some() && *self.peek() != Token::Colon;
            let target_var_name;

            if target_is_var_ref {
                target_var_name = target_var.clone().unwrap();
                self.expect(&Token::RParen)?;
            } else {
                self.expect(&Token::Colon)?;
                let target_label = self.expect_ident()?;
                let target_props = if *self.peek() == Token::LBrace {
                    self.parse_inline_props()?
                } else {
                    Vec::new()
                };
                self.expect(&Token::RParen)?;
                target_var_name =
                    target_var.clone().unwrap_or_else(|| format!("_anon_{}", out.len()));
                out.push(CreatePattern::Node {
                    var: target_var,
                    label: target_label,
                    props: target_props,
                });
            }

            let edge_source = if is_var_ref {
                source_var_name.clone()
            } else {
                out[node_idx].var().map_or(source_var_name.clone(), String::from)
            };
```

- Execute: `cargo test -p tessera-graph parse_match_then_create_edge`
- Expected: GREEN

**REFACTOR** (5 min)

- Verify all existing CREATE tests still pass: `cargo test -p tessera-graph parse_create`
- Verify full crate: `cargo test -p tessera-graph`

---

#### Cycle 1.2 — Parser: variable-only CREATE without label is error when no MATCH

**RED** (5 min)

```rust
#[test]
fn parse_create_var_ref_without_match_is_valid_syntax() {
    // CREATE (a)-[:REL]->(b) without MATCH — parser accepts it.
    // The executor will error on unbound variables, but parser is syntax-only.
    let tokens = Lexer::new("CREATE (a)-[:KNOWS]->(b)")
        .tokenize().unwrap();
    let stmt = Parser::new(tokens).parse_statement().unwrap();
    match stmt {
        GqlStatement::Mutation(ms) => {
            assert!(ms.match_clause.is_none());
            match ms.mutation {
                MutationClause::Create(c) => {
                    assert_eq!(c.patterns.len(), 1);
                    assert!(matches!(&c.patterns[0], CreatePattern::Edge { .. }));
                }
                other => panic!("expected Create, got {other:?}"),
            }
        }
        other => panic!("expected Mutation, got {other:?}"),
    }
}
```

- Expected: should already PASS from Cycle 1.1 changes (parser is syntax-only, doesn't
  validate variable binding). If it passes → skip to Cycle 1.3.

---

#### Cycle 1.3 — Parser: MATCH...CREATE with edge properties

**RED** (5 min)

```rust
#[test]
fn parse_match_create_edge_with_properties() {
    let tokens = Lexer::new(
        "MATCH (a:Person) MATCH (b:Person) \
         CREATE (a)-[:KNOWS {since: 2024}]->(b)"
    ).tokenize().unwrap();
    let stmt = Parser::new(tokens).parse_statement().unwrap();
    match stmt {
        GqlStatement::Mutation(ms) => {
            assert!(ms.match_clause.is_some());
            match ms.mutation {
                MutationClause::Create(c) => {
                    assert_eq!(c.patterns.len(), 1);
                    match &c.patterns[0] {
                        CreatePattern::Edge { rel_props, .. } => {
                            assert_eq!(rel_props.len(), 1);
                            assert_eq!(rel_props[0].0, "since");
                        }
                        other => panic!("expected Edge, got {other:?}"),
                    }
                }
                other => panic!("expected Create, got {other:?}"),
            }
        }
        other => panic!("expected Mutation, got {other:?}"),
    }
}
```

- Expected: should already PASS from Cycle 1.1. If yes → skip GREEN.

---

#### Cycle 1.4 — Executor integration: MATCH...CREATE creates edge between existing nodes

**RED** (10 min)

- File: `tessera-graph-enterprise/crates/tessera-storage-enterprise/tests/gql_mutations_integration.rs`
- Action: Add test at end of file

```rust
// ── MATCH...CREATE edge between existing nodes ──────────────────────

#[test]
fn match_create_edge_between_existing_nodes() {
    let mut g = Graph::new();
    run_mutation(&mut g, "CREATE (:Person {name: 'Alice'})").unwrap();
    run_mutation(&mut g, "CREATE (:Person {name: 'Bob'})").unwrap();
    assert_eq!(g.node_count(), 2);
    assert_eq!(g.edge_count(), 0);

    let result = run_mutation(
        &mut g,
        "MATCH (a:Person {name: 'Alice'}) \
         MATCH (b:Person {name: 'Bob'}) \
         CREATE (a)-[:KNOWS]->(b)",
    ).unwrap();

    assert_eq!(result.nodes_created, 0, "no new nodes should be created");
    assert_eq!(result.edges_created, 1);
    assert_eq!(g.edge_count(), 1);
}

#[test]
fn match_create_edge_with_properties() {
    let mut g = Graph::new();
    run_mutation(&mut g, "CREATE (:Person {name: 'Alice'})").unwrap();
    run_mutation(&mut g, "CREATE (:Person {name: 'Bob'})").unwrap();

    let result = run_mutation(
        &mut g,
        "MATCH (a:Person {name: 'Alice'}) \
         MATCH (b:Person {name: 'Bob'}) \
         CREATE (a)-[:KNOWS {since: 2024}]->(b)",
    ).unwrap();

    assert_eq!(result.edges_created, 1);

    // Verify edge properties
    let alice_id = g.nodes_by_label("Person")[0];
    let edges = g.outgoing_edges(alice_id).unwrap();
    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0].label(), "KNOWS");
    assert_eq!(edges[0].properties().get("since").unwrap().as_i64(), Some(2024));
}

#[test]
fn match_create_edge_unbound_var_is_error() {
    let mut g = Graph::new();
    run_mutation(&mut g, "CREATE (:Person {name: 'Alice'})").unwrap();

    // Variable 'b' is not matched — executor should error.
    let err = run_mutation(
        &mut g,
        "MATCH (a:Person {name: 'Alice'}) CREATE (a)-[:KNOWS]->(b)",
    );
    assert!(err.is_err(), "unbound variable 'b' should cause error");
}
```

- Execute: `cargo test -p tessera-storage-enterprise match_create_edge`
- Expected: FAILS with parser error "expected RETURN, DELETE, or SET after MATCH"

**GREEN** (0 min)

- Already fixed in Cycle 1.1 — the parser change is in the MIT core which this
  crate depends on. Just rebuild.
- Execute: should PASS after Cycle 1.1 is implemented.

**REFACTOR** (5 min)

- Run full test suite: `cargo test -p tessera-storage-enterprise`
- Verify no regressions.

---

### Fase 2: CLI — JSON Import Format

All changes in `tessera-graph-enterprise/crates/tessera-cli/`.

#### Cycle 2.1 — JSON node conversion to GQL CREATE

**RED** (10 min)

- File: `tessera-graph-enterprise/crates/tessera-cli/src/import.rs`, inside `mod tests`

```rust
// ── JSON → GQL ──────────────────────────────────────────────────────

#[test]
fn json_nodes_to_gql_basic() {
    let json = r#"{"nodes": [
        {"label": "Person", "properties": {"name": "Alice", "age": 30}},
        {"label": "Person", "properties": {"name": "Bob"}}
    ], "edges": []}"#;
    let stmts = json_to_gql_statements(json).unwrap();
    assert_eq!(stmts.len(), 2);
    assert!(stmts[0].starts_with("CREATE (:Person"));
    assert!(stmts[0].contains("name: 'Alice'"));
    assert!(stmts[0].contains("age: 30"));
    assert!(stmts[1].contains("name: 'Bob'"));
}

#[test]
fn json_empty_nodes_is_error() {
    let json = r#"{"nodes": [], "edges": []}"#;
    let result = json_to_gql_statements(json);
    assert!(result.is_err());
}

#[test]
fn json_invalid_json_is_error() {
    let result = json_to_gql_statements("not json");
    assert!(result.is_err());
}
```

- Execute: `cargo test -p tessera-cli json_nodes_to_gql`
- Expected: FAILS — function `json_to_gql_statements` does not exist

**GREEN** (30 min)

- File: `tessera-graph-enterprise/crates/tessera-cli/src/import.rs`
- Action: Add `json_to_gql_statements` function and helpers

```rust
/// Generate GQL statements from a JSON string in tessera-import format.
///
/// Expected format:
/// ```json
/// {
///   "nodes": [{"label": "L", "properties": {"k": "v"}}],
///   "edges": [{"source": {"label": "L", "match": {"id": "x"}},
///              "target": {"label": "L", "match": {"id": "y"}},
///              "label": "REL", "properties": {}}]
/// }
/// ```
///
/// Nodes produce `CREATE (:Label {props})` statements.
/// Edges produce `MATCH (a:L {k: 'v'}) MATCH (b:L {k: 'v'}) CREATE (a)-[:REL]->(b)`
/// statements (requires MATCH...CREATE parser support in the MIT core).
///
/// # Errors
///
/// Returns `CliError::ImportExport` on invalid JSON, missing fields, or empty nodes.
pub fn json_to_gql_statements(json_text: &str) -> Result<Vec<String>, CliError> {
    let root: serde_json::Value = serde_json::from_str(json_text)
        .map_err(|e| CliError::ImportExport(format!("invalid JSON: {e}")))?;

    let obj = root.as_object()
        .ok_or_else(|| CliError::ImportExport("root must be a JSON object".into()))?;

    let nodes = obj.get("nodes")
        .and_then(|v| v.as_array())
        .ok_or_else(|| CliError::ImportExport("missing 'nodes' array".into()))?;

    let edges = obj.get("edges")
        .and_then(|v| v.as_array())
        .ok_or_else(|| CliError::ImportExport("missing 'edges' array".into()))?;

    if nodes.is_empty() && edges.is_empty() {
        return Err(CliError::ImportExport("no nodes or edges in JSON".into()));
    }

    let mut statements = Vec::with_capacity(nodes.len() + edges.len());

    // Nodes → CREATE (:Label {props})
    for node in nodes {
        let label = node.get("label")
            .and_then(|v| v.as_str())
            .ok_or_else(|| CliError::ImportExport("node missing 'label'".into()))?;
        let props = node.get("properties")
            .and_then(|v| v.as_object());
        let props_str = format_json_props_as_gql(props);
        statements.push(format!("CREATE (:{label}{props_str})"));
    }

    // Edges → MATCH...CREATE
    for edge in edges {
        let rel_label = edge.get("label")
            .and_then(|v| v.as_str())
            .ok_or_else(|| CliError::ImportExport("edge missing 'label'".into()))?;

        let source_match = format_endpoint_match(edge, "source")?;
        let target_match = format_endpoint_match(edge, "target")?;

        let rel_props = edge.get("properties")
            .and_then(|v| v.as_object());
        let rel_props_str = if rel_props.is_some_and(|p| !p.is_empty()) {
            format!(" {}", format_json_props_as_gql(rel_props))
        } else {
            String::new()
        };

        statements.push(format!(
            "MATCH (a{source_match}) MATCH (b{target_match}) \
             CREATE (a)-[:{rel_label}{rel_props_str}]->(b)"
        ));
    }

    Ok(statements)
}

/// Format a JSON properties object as a GQL property map string.
///
/// Returns `""` if empty, or ` {key: value, ...}` with appropriate escaping.
fn format_json_props_as_gql(props: Option<&serde_json::Map<String, serde_json::Value>>) -> String {
    let Some(props) = props else { return String::new() };
    if props.is_empty() { return String::new(); }

    let mut pairs = Vec::with_capacity(props.len());
    for (k, v) in props {
        pairs.push(format!("{k}: {}", json_value_to_gql_literal(v)));
    }
    format!(" {{{}}}", pairs.join(", "))
}

/// Convert a JSON value to a GQL literal string.
fn json_value_to_gql_literal(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::Null => "null".into(),
        serde_json::Value::Bool(b) => if *b { "true" } else { "false" }.into(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::String(s) => format!("'{}'", s.replace('\'', "\\'")),
        // Arrays and objects → serialize as JSON string
        other => format!("'{}'", other.to_string().replace('\'', "\\'")),
    }
}

/// Format a MATCH clause for an edge endpoint: `:Label {matchKey: 'matchVal'}`
fn format_endpoint_match(
    edge: &serde_json::Value,
    endpoint_key: &str,
) -> Result<String, CliError> {
    let ep = edge.get(endpoint_key)
        .and_then(|v| v.as_object())
        .ok_or_else(|| CliError::ImportExport(
            format!("edge missing '{endpoint_key}' object")
        ))?;

    let label = ep.get("label")
        .and_then(|v| v.as_str());

    let match_obj = ep.get("match")
        .and_then(|v| v.as_object())
        .ok_or_else(|| CliError::ImportExport(
            format!("edge {endpoint_key} missing 'match' object")
        ))?;

    let (match_key, match_val) = match_obj.iter().next()
        .ok_or_else(|| CliError::ImportExport(
            format!("edge {endpoint_key}.match is empty")
        ))?;

    let val_gql = json_value_to_gql_literal(match_val);
    if let Some(l) = label {
        Ok(format!(":{l} {{{match_key}: {val_gql}}}"))
    } else {
        Ok(format!(" {{{match_key}: {val_gql}}}"))
    }
}
```

- Execute: `cargo test -p tessera-cli json_nodes_to_gql`
- Expected: GREEN

---

#### Cycle 2.2 — JSON edge conversion to MATCH...CREATE

**RED** (10 min)

- File: `tessera-graph-enterprise/crates/tessera-cli/src/import.rs`, inside `mod tests`

```rust
#[test]
fn json_edges_to_match_create() {
    let json = r#"{"nodes": [
        {"label": "Person", "properties": {"id": "1", "name": "Alice"}},
        {"label": "Person", "properties": {"id": "2", "name": "Bob"}}
    ], "edges": [
        {
            "source": {"label": "Person", "match": {"id": "1"}},
            "target": {"label": "Person", "match": {"id": "2"}},
            "label": "KNOWS",
            "properties": {}
        }
    ]}"#;
    let stmts = json_to_gql_statements(json).unwrap();
    assert_eq!(stmts.len(), 3); // 2 nodes + 1 edge
    let edge_stmt = &stmts[2];
    assert!(edge_stmt.starts_with("MATCH (a:Person"), "got: {edge_stmt}");
    assert!(edge_stmt.contains("MATCH (b:Person"), "got: {edge_stmt}");
    assert!(edge_stmt.contains("CREATE (a)-[:KNOWS]->(b)"), "got: {edge_stmt}");
}

#[test]
fn json_edge_with_properties() {
    let json = r#"{"nodes": [
        {"label": "Person", "properties": {"id": "1"}},
        {"label": "Person", "properties": {"id": "2"}}
    ], "edges": [
        {
            "source": {"label": "Person", "match": {"id": "1"}},
            "target": {"label": "Person", "match": {"id": "2"}},
            "label": "KNOWS",
            "properties": {"since": 2024}
        }
    ]}"#;
    let stmts = json_to_gql_statements(json).unwrap();
    let edge_stmt = &stmts[2];
    assert!(edge_stmt.contains("{since: 2024}"), "got: {edge_stmt}");
}

#[test]
fn json_edge_missing_source_is_error() {
    let json = r#"{"nodes": [], "edges": [
        {"target": {"label": "X", "match": {"id": "1"}}, "label": "R", "properties": {}}
    ]}"#;
    let result = json_to_gql_statements(json);
    assert!(result.is_err());
}
```

- Expected: should PASS from Cycle 2.1 implementation. If yes → skip GREEN.

---

#### Cycle 2.3 — JSON string escaping edge cases

**RED** (10 min)

```rust
#[test]
fn json_property_with_single_quotes_escaped() {
    let json = r#"{"nodes": [
        {"label": "Person", "properties": {"name": "O'Brien"}}
    ], "edges": []}"#;
    let stmts = json_to_gql_statements(json).unwrap();
    assert!(stmts[0].contains("O\\'Brien"), "got: {}", stmts[0]);
}

#[test]
fn json_boolean_and_null_properties() {
    let json = r#"{"nodes": [
        {"label": "Item", "properties": {"active": true, "deleted": false, "note": null}}
    ], "edges": []}"#;
    let stmts = json_to_gql_statements(json).unwrap();
    assert!(stmts[0].contains("active: true"), "got: {}", stmts[0]);
    assert!(stmts[0].contains("deleted: false"), "got: {}", stmts[0]);
    assert!(stmts[0].contains("note: null"), "got: {}", stmts[0]);
}

#[test]
fn json_array_property_stored_as_json_string() {
    let json = r#"{"nodes": [
        {"label": "X", "properties": {"tags": ["a", "b"]}}
    ], "edges": []}"#;
    let stmts = json_to_gql_statements(json).unwrap();
    // Array should be stored as a JSON string
    assert!(stmts[0].contains("tags: '"), "got: {}", stmts[0]);
}
```

- Expected: should PASS from Cycle 2.1 implementation. If yes → skip GREEN.

---

#### Cycle 2.4 — Wire JSON format into CLI handle_import and infer_import_format

**RED** (10 min)

- File: `tessera-graph-enterprise/crates/tessera-cli/src/main.rs`, inside `mod tests`

```rust
#[test]
fn infer_format_json_extension() {
    assert_eq!(infer_import_format("data.json"), "json");
    assert_eq!(infer_import_format("data.JSON"), "json");
}
```

- Execute: `cargo test -p tessera-cli infer_format_json`
- Expected: FAILS — `infer_import_format` returns `"gql"` for `.json`

**GREEN** (10 min)

- File: `tessera-graph-enterprise/crates/tessera-cli/src/main.rs`
- Action 1: Update `infer_import_format` to recognize `.json`:

```rust
fn infer_import_format(file: &str) -> &str {
    let path = std::path::Path::new(file);
    match path.extension().and_then(|e| e.to_str()) {
        Some(ext) if ext.eq_ignore_ascii_case("gql") || ext.eq_ignore_ascii_case("cypher") => "gql",
        Some(ext) if ext.eq_ignore_ascii_case("csv") => "csv-nodes",
        Some(ext) if ext.eq_ignore_ascii_case("json") => "json",
        _ => "gql",
    }
}
```

- Action 2: Update `handle_import` to handle `"json"` format:

```rust
        let statements = match fmt {
            "gql" => import::split_gql_statements(&content),
            "csv-nodes" => import::csv_nodes_to_gql(&content)?,
            "json" => import::json_to_gql_statements(&content)?,
            other => {
                return Err(CliError::ImportExport(format!(
                    "unsupported import format: {other}"
                )));
            }
        };
```

- Action 3: Fix the existing test `infer_format_json_falls_back_to_gql` which asserts
  that `.json` falls back to `gql`. This test must be updated to expect `"json"`:

```rust
    #[test]
    fn infer_format_json_returns_json() {
        assert_eq!(infer_import_format("data.json"), "json");
    }
```

- Execute: `cargo test -p tessera-cli`
- Expected: GREEN

---

### Fase 3: Verification

#### Cycle 3.1 — Full workspace compilation

```
cargo check --workspace
```

Expected: zero errors, zero warnings.

#### Cycle 3.2 — MIT core tests

```
cargo test -p tessera-graph
```

Expected: all tests pass including new MATCH...CREATE parser tests.

#### Cycle 3.3 — Enterprise tests

```
cargo test -p tessera-storage-enterprise
cargo test -p tessera-cli
```

Expected: all tests pass.

#### Cycle 3.4 — Wiring verification

- [ ] `Token::Create` arm in `parse_statement` MATCH dispatch — called by any `MATCH...CREATE` query
- [ ] `json_to_gql_statements` — called from `handle_import` in `main.rs` via `"json"` format match
- [ ] `format_json_props_as_gql` — called from `json_to_gql_statements`
- [ ] `json_value_to_gql_literal` — called from `format_json_props_as_gql` and `format_endpoint_match`
- [ ] `format_endpoint_match` — called from `json_to_gql_statements`
- [ ] `infer_import_format` returns `"json"` for `.json` files — wired into `handle_import`
- [ ] Error message in parser updated from "RETURN, DELETE, or SET" to "RETURN, DELETE, SET, or CREATE"
- [ ] No stale test `infer_format_json_falls_back_to_gql` remains

---

## Estimation

| Phase | Task | Est. |
|-------|------|------|
| 1 | Parser: MATCH...CREATE + var-ref patterns | 35 min |
| 1 | Integration tests: executor edge creation | 15 min |
| 2 | JSON→GQL conversion function + tests | 40 min |
| 2 | CLI wiring (handle_import, infer_format) | 10 min |
| 3 | Verification | 15 min |
| **Total** | | **~2 h** |

---

## Criteria of Acceptance

- [ ] `MATCH (a:X) MATCH (b:Y) CREATE (a)-[:REL]->(b)` parses as `MutationStatement` with `match_clause: Some`
- [ ] `CREATE (a)-[:REL]->(b)` without MATCH parses as `MutationStatement` with `match_clause: None` and 1 Edge pattern
- [ ] Executor creates edge between MATCH-resolved nodes (integration test)
- [ ] Unbound variable in CREATE edge produces `GqlMutationError`
- [ ] `json_to_gql_statements` converts nodes to CREATEs and edges to MATCH...CREATEs
- [ ] CLI `import --format json` invokes `json_to_gql_statements`
- [ ] `infer_import_format("file.json")` returns `"json"`
- [ ] `cargo check --workspace` — zero errors, zero warnings
- [ ] `cargo test --workspace` — all tests pass

---

## Files Affected

| File | Change |
|------|--------|
| `tessera-graph/src/gql/parser.rs` | Add `Token::Create` arm in MATCH dispatch; make label optional in `parse_create_pattern_multi` for var refs |
| `tessera-graph-enterprise/crates/tessera-storage-enterprise/tests/gql_mutations_integration.rs` | Add MATCH...CREATE integration tests |
| `tessera-graph-enterprise/crates/tessera-cli/src/import.rs` | Add `json_to_gql_statements` + helpers |
| `tessera-graph-enterprise/crates/tessera-cli/src/main.rs` | Add `"json"` to `handle_import` + `infer_import_format` |
