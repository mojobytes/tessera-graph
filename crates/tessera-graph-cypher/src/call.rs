// SPDX-License-Identifier: LicenseRef-TesseraGraph-Proprietary

//! CALL statement parser.
//!
//! Parses `CALL <namespace>.<proc>() YIELD <col> [UNWIND <col> AS <var>] [RETURN <item>]`
//! as a single [`CallStatement`]. The grammar is a hand-written prefix matcher,
//! same style as `admin.rs` and `ddl.rs`. Only the subset issued by the pilot
//! client is handled.

use tessera_graph::Result;
use tessera_graph::gql::{CallStatement, Expr, Literal, ReturnClause, ReturnItem, UnwindClause};

use crate::parse_util::syntax_err;

/// Try to parse `input` as a `CALL` statement.
///
/// Returns `Ok(None)` when the input does not start with `CALL ` — the caller
/// then falls through to the DDL, admin, and GQL parsers in that order.
///
/// # Errors
///
/// Returns [`tessera_graph::Error::GqlSyntaxError`] when the input begins with
/// `CALL ` but the remainder is malformed (missing `.`, missing `()`, missing
/// `YIELD`, or malformed `UNWIND`/`RETURN`).
pub fn try_parse_call(input: &str) -> Result<Option<CallStatement>> {
    let trimmed = input.trim_end_matches(';').trim();
    if !trimmed.to_ascii_uppercase().starts_with("CALL ") {
        return Ok(None);
    }
    let rest = trimmed[5..].trim_start();

    // <namespace>.<procedure>
    let dot = rest
        .find('.')
        .ok_or_else(|| syntax_err("CALL: expected '<namespace>.<procedure>()', missing '.'"))?;
    let namespace = rest[..dot].trim().to_owned();
    if namespace.is_empty() {
        return Err(syntax_err("CALL: empty namespace before '.'"));
    }
    let rest = rest[dot + 1..].trim_start();

    // procedure name up to '('
    let lparen = rest
        .find('(')
        .ok_or_else(|| syntax_err("CALL: expected '(' after procedure name"))?;
    let procedure = rest[..lparen].trim().to_owned();
    if procedure.is_empty() {
        return Err(syntax_err("CALL: empty procedure name"));
    }

    // Argument list inside '(...)'. The introspection procedures take none;
    // admin procedures (snapshot/restore) take positional string literals.
    let after_lparen = &rest[lparen + 1..];
    let rparen = after_lparen
        .find(')')
        .ok_or_else(|| syntax_err("CALL: unclosed '(' in argument list"))?;
    let args = parse_call_args(after_lparen[..rparen].trim())?;
    let rest = after_lparen[rparen + 1..].trim_start();

    // YIELD <col> — optional. Introspection procedures carry it; admin
    // procedures whose result the server handler shapes do not. A bare
    // `CALL ns.proc()` with neither YIELD nor args is still rejected below
    // (it would resolve to nothing useful).
    let (yield_col, unwind, return_clause) = if rest.to_ascii_uppercase().starts_with("YIELD ") {
        let rest = rest[6..].trim_start();
        let (yield_col, rest) = split_at_keyword(rest, &["UNWIND", "RETURN"]);
        let yield_col = yield_col.trim().to_owned();
        if yield_col.is_empty() {
            return Err(syntax_err("CALL: empty YIELD column name"));
        }
        let (unwind, rest) = parse_optional_unwind(rest)?;
        let return_clause = parse_optional_return(rest)?;
        (yield_col, unwind, return_clause)
    } else if rest.is_empty() {
        // No YIELD and no trailing clauses: only valid when the call carries
        // arguments (an admin procedure). A bare `CALL ns.proc()` with no
        // YIELD and no args is the unsupported introspection-without-YIELD case.
        if args.is_empty() {
            return Err(syntax_err(
                "CALL: expected 'YIELD <col>' after '()'; bare CALL without YIELD or arguments is not supported",
            ));
        }
        (String::new(), None, None)
    } else {
        return Err(syntax_err(&format!(
            "CALL: unexpected trailing tokens after '()': {rest:?}"
        )));
    };

    Ok(Some(CallStatement {
        namespace: Some(namespace),
        procedure,
        args,
        yield_col,
        unwind,
        return_clause,
    }))
}

/// Parses the comma-separated positional arguments inside a CALL's parentheses.
///
/// Each argument must be a single- or double-quoted string literal (the only
/// argument form the admin procedures accept). An empty argument list yields an
/// empty `Vec`.
fn parse_call_args(inner: &str) -> Result<Vec<Expr>> {
    if inner.is_empty() {
        return Ok(vec![]);
    }
    inner
        .split(',')
        .map(|raw| {
            let tok = raw.trim();
            let bytes = tok.as_bytes();
            let quoted = tok.len() >= 2
                && (bytes[0] == b'\'' || bytes[0] == b'"')
                && bytes[bytes.len() - 1] == bytes[0];
            if !quoted {
                return Err(syntax_err(&format!(
                    "CALL: argument must be a quoted string literal, got {tok:?}"
                )));
            }
            let s = tok[1..tok.len() - 1].to_owned();
            Ok(Expr::Literal(Literal::Str(s)))
        })
        .collect()
}

/// Splits `s` at the first whitespace-bounded occurrence of any `keywords`
/// entry (case-insensitive). Returns `(before, from_keyword_trimmed)`; if no
/// keyword is found, returns `(s, "")`.
fn split_at_keyword<'a>(s: &'a str, keywords: &[&str]) -> (&'a str, &'a str) {
    let upper = s.to_ascii_uppercase();
    let bytes = upper.as_bytes();
    let mut earliest: Option<usize> = None;
    for kw in keywords {
        let mut search = 0;
        while let Some(rel) = upper[search..].find(kw) {
            let abs = search + rel;
            let before_ok = abs == 0 || bytes[abs - 1].is_ascii_whitespace();
            let after = abs + kw.len();
            let after_ok = after >= bytes.len() || bytes[after].is_ascii_whitespace();
            if before_ok && after_ok {
                if earliest.is_none_or(|e| abs < e) {
                    earliest = Some(abs);
                }
                break;
            }
            search = abs + 1;
        }
    }
    earliest.map_or((s, ""), |pos| (&s[..pos], s[pos..].trim_start()))
}

/// Parses an optional `UNWIND <col> AS <var>` clause at the start of `s`.
fn parse_optional_unwind(s: &str) -> Result<(Option<UnwindClause>, &str)> {
    if !s.to_ascii_uppercase().starts_with("UNWIND ") {
        return Ok((None, s));
    }
    let rest = s[7..].trim_start();
    let (expr_str, rest) = split_at_keyword(rest, &["RETURN"]);
    let expr_str = expr_str.trim();

    let as_pos = expr_str
        .to_ascii_uppercase()
        .find(" AS ")
        .ok_or_else(|| syntax_err("CALL UNWIND: expected '<col> AS <var>'"))?;
    let col = expr_str[..as_pos].trim().to_owned();
    let var = expr_str[as_pos + 4..].trim().to_owned();
    if col.is_empty() || var.is_empty() {
        return Err(syntax_err("CALL UNWIND: empty column or variable name"));
    }
    Ok((
        Some(UnwindClause {
            expr: Expr::Var(col),
            var,
        }),
        rest,
    ))
}

/// Parses an optional `RETURN <item>[, <item>]` clause. The pilot only ever
/// issues `RETURN vl` / `RETURN et` (a single bare var).
fn parse_optional_return(s: &str) -> Result<Option<ReturnClause>> {
    let s = s.trim();
    if s.is_empty() {
        return Ok(None);
    }
    if !s.to_ascii_uppercase().starts_with("RETURN ") {
        return Err(syntax_err(&format!(
            "CALL: unexpected trailing tokens after YIELD/UNWIND: {s:?}"
        )));
    }
    let rest = s[7..].trim();
    if rest.is_empty() {
        return Err(syntax_err("CALL RETURN: expected at least one item"));
    }

    let items: Vec<ReturnItem> = rest
        .split(',')
        .map(|item| {
            let item = item.trim();
            if let Some(as_pos) = item.to_ascii_uppercase().find(" AS ") {
                ReturnItem {
                    expr: Expr::Var(item[..as_pos].trim().to_owned()),
                    alias: Some(item[as_pos + 4..].trim().to_owned()),
                }
            } else {
                ReturnItem {
                    expr: Expr::Var(item.to_owned()),
                    alias: None,
                }
            }
        })
        .collect();

    Ok(Some(ReturnClause {
        distinct: false,
        items,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_mg_vertex_labels_full() {
        let stmt = try_parse_call(
            "CALL mg.vertex_labels() YIELD vertex_labels UNWIND vertex_labels AS vl RETURN vl",
        )
        .unwrap()
        .unwrap();
        assert_eq!(stmt.namespace.as_deref(), Some("mg"));
        assert_eq!(stmt.procedure, "vertex_labels");
        assert_eq!(stmt.yield_col, "vertex_labels");
        let unwind = stmt.unwind.as_ref().expect("expected UNWIND clause");
        assert_eq!(unwind.var, "vl");
        assert!(stmt.return_clause.is_some());
    }

    #[test]
    fn parse_mg_edge_types_full() {
        let stmt = try_parse_call(
            "CALL mg.edge_types() YIELD edge_types UNWIND edge_types AS et RETURN et",
        )
        .unwrap()
        .unwrap();
        assert_eq!(stmt.namespace.as_deref(), Some("mg"));
        assert_eq!(stmt.procedure, "edge_types");
        assert_eq!(stmt.yield_col, "edge_types");
        assert_eq!(stmt.unwind.as_ref().unwrap().var, "et");
        assert!(stmt.return_clause.is_some());
    }

    #[test]
    fn parse_tessera_vertex_labels_full() {
        let stmt = try_parse_call(
            "CALL tessera.vertex_labels() YIELD vertex_labels UNWIND vertex_labels AS vl RETURN vl",
        )
        .unwrap()
        .unwrap();
        assert_eq!(stmt.namespace.as_deref(), Some("tessera"));
    }

    #[test]
    fn parse_call_with_trailing_semicolon() {
        let stmt = try_parse_call(
            "CALL mg.vertex_labels() YIELD vertex_labels UNWIND vertex_labels AS vl RETURN vl;",
        )
        .unwrap()
        .unwrap();
        assert_eq!(stmt.procedure, "vertex_labels");
    }

    #[test]
    fn parse_call_case_insensitive() {
        let stmt = try_parse_call(
            "call mg.vertex_labels() yield vertex_labels unwind vertex_labels as vl return vl",
        )
        .unwrap()
        .unwrap();
        assert_eq!(stmt.procedure, "vertex_labels");
        assert_eq!(stmt.unwind.as_ref().unwrap().var, "vl");
    }

    #[test]
    fn parse_call_yield_only_no_unwind() {
        let stmt = try_parse_call("CALL mg.vertex_labels() YIELD vertex_labels")
            .unwrap()
            .unwrap();
        assert_eq!(stmt.yield_col, "vertex_labels");
        assert!(stmt.unwind.is_none());
        assert!(stmt.return_clause.is_none());
    }

    #[test]
    fn non_call_returns_none() {
        assert!(try_parse_call("MATCH (n) RETURN n").unwrap().is_none());
    }

    #[test]
    fn create_node_returns_none() {
        assert!(try_parse_call("CREATE (n:Person {id: 1})").unwrap().is_none());
    }

    #[test]
    fn malformed_call_missing_yield_errors() {
        let result = try_parse_call("CALL mg.vertex_labels()");
        assert!(result.is_err(), "expected Err for CALL without YIELD, got {result:?}");
    }

    #[test]
    fn malformed_call_missing_dot_errors() {
        // CALL vertex_labels() without namespace.proc form must not succeed.
        let result = try_parse_call("CALL vertex_labels() YIELD vertex_labels");
        match result {
            Ok(None) | Err(_) => {}
            Ok(Some(_)) => panic!("must not succeed without namespace.proc"),
        }
    }

    // --- Block 3 Feature B: snapshot/restore with positional string args (B-6) ---

    #[test]
    fn parse_tessera_snapshot_two_string_args_no_yield() {
        use tessera_graph::gql::Literal;
        let stmt = try_parse_call("CALL tessera.snapshot('mydb', '/snapshots/mydb-1')")
            .unwrap()
            .unwrap();
        assert_eq!(stmt.namespace.as_deref(), Some("tessera"));
        assert_eq!(stmt.procedure, "snapshot");
        assert_eq!(
            stmt.args,
            vec![
                Expr::Literal(Literal::Str("mydb".to_owned())),
                Expr::Literal(Literal::Str("/snapshots/mydb-1".to_owned())),
            ]
        );
        // No YIELD clause on an admin procedure.
        assert!(stmt.yield_col.is_empty());
        assert!(stmt.unwind.is_none());
        assert!(stmt.return_clause.is_none());
    }

    #[test]
    fn parse_tessera_restore_double_quoted_args() {
        use tessera_graph::gql::Literal;
        let stmt = try_parse_call("CALL tessera.restore(\"mydb\", \"/snapshots/mydb-1\")")
            .unwrap()
            .unwrap();
        assert_eq!(stmt.procedure, "restore");
        assert_eq!(
            stmt.args,
            vec![
                Expr::Literal(Literal::Str("mydb".to_owned())),
                Expr::Literal(Literal::Str("/snapshots/mydb-1".to_owned())),
            ]
        );
    }

    #[test]
    fn introspection_args_remain_empty() {
        // vertex_labels still parses with empty args and a YIELD column.
        let stmt = try_parse_call("CALL mg.vertex_labels() YIELD vertex_labels")
            .unwrap()
            .unwrap();
        assert!(stmt.args.is_empty());
        assert_eq!(stmt.yield_col, "vertex_labels");
    }
}
