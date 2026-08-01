// SPDX-License-Identifier: LicenseRef-TesseraGraph-Proprietary

//! DDL-statement parser — runs before the regular GQL parser so
//! `CREATE INDEX`, `DROP INDEX`, `CREATE CONSTRAINT`, `DROP CONSTRAINT`,
//! `SHOW INDEX INFO`, `SHOW CONSTRAINT INFO` are routed to the server's
//! DDL handler instead of the graph engine.
//!
//! The grammar is prefix-matched (identical approach to [`crate::admin`]) and is
//! intentionally minimal: we only parse the subset the pilot client issues.
//! Both legacy (`CREATE INDEX ON :L(p)`) and modern
//! (`CREATE INDEX FOR (n:L) ON (n.p)`) index syntaxes are supported.

use tessera_graph::gql::DdlStatement;
use tessera_graph::Result;

use crate::parse_util::{strip_ci, strip_ci_ws, syntax_err};

/// Try to parse `input` as a DDL statement. Returns `Ok(None)` if the input
/// does not start with a DDL keyword — the caller then falls back to the admin
/// parser and then to the regular GQL parser.
///
/// # Errors
///
/// Returns [`tessera_graph::Error::GqlSyntaxError`] if the input begins with a
/// DDL keyword but the remainder is malformed.
pub fn try_parse_ddl(input: &str) -> Result<Option<DdlStatement>> {
    let trimmed = input.trim_end_matches(';').trim();
    let upper = trimmed.to_ascii_uppercase();

    // ── CREATE INDEX ──────────────────────────────────────────────────────────
    if upper.starts_with("CREATE INDEX ON ") || upper.starts_with("CREATE INDEX ON:") {
        return parse_create_index_legacy(trimmed).map(Some);
    }
    if upper.starts_with("CREATE INDEX FOR ") {
        return parse_create_index_for(trimmed).map(Some);
    }
    // `CREATE INDEX` alone (without ON or FOR) is malformed DDL, not a node
    // mutation — surface a syntax error rather than falling through silently.
    if upper.starts_with("CREATE INDEX") {
        return Err(syntax_err(
            "CREATE INDEX requires ON :Label(prop) or FOR (n:Label) ON (n.prop)",
        ));
    }

    // ── DROP INDEX ────────────────────────────────────────────────────────────
    if upper.starts_with("DROP INDEX ON ") || upper.starts_with("DROP INDEX ON:") {
        return parse_drop_index(trimmed).map(Some);
    }

    // ── CREATE CONSTRAINT ─────────────────────────────────────────────────────
    if upper.starts_with("CREATE CONSTRAINT ON ") {
        return parse_create_constraint(trimmed).map(Some);
    }

    // ── DROP CONSTRAINT ───────────────────────────────────────────────────────
    if upper.starts_with("DROP CONSTRAINT ON ") {
        return parse_drop_constraint(trimmed).map(Some);
    }

    // ── ALTER LABEL … APPEND ONLY (issue #61) ────────────────────────────────
    if upper.starts_with("ALTER LABEL ") {
        return parse_alter_label(trimmed).map(Some);
    }
    // `ALTER LABEL` with nothing after it: malformed DDL, not a query.
    //
    // Only this exact form is claimed. The bare `ALTER` keyword must NOT be,
    // because `ALTER USER …` belongs to the admin parser that runs after this
    // one — claiming it swallows a valid admin statement and reports a DDL
    // syntax error for it.
    if upper == "ALTER LABEL" {
        return Err(syntax_err(
            "ALTER LABEL requires :Label SET APPEND ONLY or :Label REMOVE APPEND ONLY",
        ));
    }

    // ── SHOW ──────────────────────────────────────────────────────────────────
    if upper == "SHOW INDEX INFO" {
        return Ok(Some(DdlStatement::ShowIndexInfo));
    }
    if upper == "SHOW CONSTRAINT INFO" {
        return Ok(Some(DdlStatement::ShowConstraintInfo));
    }
    if upper == "SHOW APPEND ONLY INFO" {
        return Ok(Some(DdlStatement::ShowAppendOnlyInfo));
    }

    Ok(None)
}

/// `ALTER LABEL :Label SET APPEND ONLY` / `… REMOVE APPEND ONLY`
///
/// The leading colon is idiomatic but optional; the label is unambiguous
/// either way.
fn parse_alter_label(s: &str) -> Result<DdlStatement> {
    let rest = strip_ci(s, "ALTER LABEL")?.trim();
    let upper = rest.to_ascii_uppercase();

    let (action_kw, on) = if let Some(idx) = upper.find(" SET ") {
        (idx, true)
    } else if let Some(idx) = upper.find(" REMOVE ") {
        (idx, false)
    } else {
        return Err(syntax_err(
            "ALTER LABEL: expected SET APPEND ONLY or REMOVE APPEND ONLY",
        ));
    };

    let label = rest[..action_kw].trim().trim_start_matches(':').trim();
    if label.is_empty() {
        return Err(syntax_err("ALTER LABEL: empty label name"));
    }

    let kw_len = if on { " SET ".len() } else { " REMOVE ".len() };
    let tail = rest[action_kw + kw_len..].trim();
    if !tail.eq_ignore_ascii_case("APPEND ONLY") {
        return Err(syntax_err(&format!(
            "ALTER LABEL: expected APPEND ONLY, found {tail:?}"
        )));
    }

    Ok(DdlStatement::SetLabelAppendOnly {
        label: label.to_owned(),
        on,
    })
}

// ── per-statement parsers ──────────────────────────────────────────────────────

/// `CREATE INDEX ON :Label(prop)`
fn parse_create_index_legacy(s: &str) -> Result<DdlStatement> {
    let rest = strip_ci(s, "CREATE INDEX ON")?;
    let (label, prop) = parse_label_prop_colon(rest)?;
    Ok(DdlStatement::CreateIndexLegacy { label, prop })
}

/// `CREATE INDEX FOR (n:Label) ON (n.prop)`
fn parse_create_index_for(s: &str) -> Result<DdlStatement> {
    let rest = strip_ci(s, "CREATE INDEX FOR")?;
    let (label, var, rest) = parse_paren_var_label(rest, "CREATE INDEX FOR")?;
    let rest = strip_ci_ws(rest, "ON")?;
    let prop = parse_paren_dot_prop(rest, &var, "CREATE INDEX FOR")?;
    Ok(DdlStatement::CreateIndexFor { label, prop })
}

/// `DROP INDEX ON :Label(prop)`
fn parse_drop_index(s: &str) -> Result<DdlStatement> {
    let rest = strip_ci(s, "DROP INDEX ON")?;
    let (label, prop) = parse_label_prop_colon(rest)?;
    Ok(DdlStatement::DropIndex { label, prop })
}

/// `CREATE CONSTRAINT ON (n:Label) ASSERT n.prop IS UNIQUE`
fn parse_create_constraint(s: &str) -> Result<DdlStatement> {
    let (label, prop) = parse_constraint_body(s, "CREATE CONSTRAINT ON")?;
    Ok(DdlStatement::CreateUniqueConstraint { label, prop })
}

/// `DROP CONSTRAINT ON (n:Label) ASSERT n.prop IS UNIQUE`
fn parse_drop_constraint(s: &str) -> Result<DdlStatement> {
    let (label, prop) = parse_constraint_body(s, "DROP CONSTRAINT ON")?;
    Ok(DdlStatement::DropConstraint { label, prop })
}

// ── helpers ─────────────────────────────────────────────────────────────────────

/// Parses `:Label(prop)` (with optional leading whitespace).
fn parse_label_prop_colon(s: &str) -> Result<(String, String)> {
    let s = s.trim_start();
    let s = s
        .strip_prefix(':')
        .ok_or_else(|| syntax_err("expected ':Label(prop)'"))?;
    let lparen = s
        .find('(')
        .ok_or_else(|| syntax_err("expected '(' after label"))?;
    let label = s[..lparen].trim().to_owned();
    if label.is_empty() {
        return Err(syntax_err("empty label name"));
    }
    let rest = &s[lparen + 1..];
    let rparen = rest
        .find(')')
        .ok_or_else(|| syntax_err("unclosed '(' in :Label(prop)"))?;
    let prop = rest[..rparen].trim().to_owned();
    if prop.is_empty() {
        return Err(syntax_err("empty property name in :Label(prop)"));
    }
    Ok((label, prop))
}

/// Parses a leading `(var:Label)` group, returning `(label, var, remainder)`.
fn parse_paren_var_label<'a>(s: &'a str, ctx: &str) -> Result<(String, String, &'a str)> {
    let rest = s.trim_start();
    let rest = rest
        .strip_prefix('(')
        .ok_or_else(|| syntax_err(&format!("{ctx}: expected '('")))?;
    let (var_label, rest) = rest
        .split_once(')')
        .ok_or_else(|| syntax_err(&format!("{ctx}: unclosed '('")))?;
    let colon = var_label
        .find(':')
        .ok_or_else(|| syntax_err(&format!("{ctx}: expected ':Label'")))?;
    let label = var_label[colon + 1..].trim().to_owned();
    let var = var_label[..colon].trim().to_owned();
    if label.is_empty() {
        return Err(syntax_err(&format!("{ctx}: empty label name")));
    }
    Ok((label, var, rest))
}

/// Parses a leading `(var.prop)` group, validating the variable matches `expect_var`
/// (when non-empty) and returning the property name.
fn parse_paren_dot_prop(s: &str, expect_var: &str, ctx: &str) -> Result<String> {
    let rest = s.trim_start();
    let rest = rest
        .strip_prefix('(')
        .ok_or_else(|| syntax_err(&format!("{ctx}: expected '(' after ON")))?;
    let (dot_expr, _tail) = rest
        .split_once(')')
        .ok_or_else(|| syntax_err(&format!("{ctx}: unclosed '(' in ON clause")))?;
    let dot = dot_expr
        .find('.')
        .ok_or_else(|| syntax_err(&format!("{ctx}: expected 'var.prop'")))?;
    let used_var = dot_expr[..dot].trim();
    if !expect_var.is_empty() && used_var != expect_var {
        return Err(syntax_err(&format!(
            "{ctx}: variable mismatch: bound as {expect_var:?} but ON uses {used_var:?}"
        )));
    }
    let prop = dot_expr[dot + 1..].trim().to_owned();
    if prop.is_empty() {
        return Err(syntax_err(&format!("{ctx}: empty property name")));
    }
    Ok(prop)
}

/// Shared body parser for `(CREATE|DROP) CONSTRAINT ON (n:Label) ASSERT n.prop IS UNIQUE`.
fn parse_constraint_body(s: &str, prefix: &str) -> Result<(String, String)> {
    let rest = strip_ci(s, prefix)?;
    let (label, var, rest) = parse_paren_var_label(rest, "CONSTRAINT")?;
    let rest = strip_ci_ws(rest, "ASSERT")?;
    let rest = rest.trim_start();
    let dot = rest
        .find('.')
        .ok_or_else(|| syntax_err("CONSTRAINT ASSERT: expected 'var.prop'"))?;
    let assert_var = rest[..dot].trim();
    if !var.is_empty() && assert_var != var {
        return Err(syntax_err(&format!(
            "CONSTRAINT: variable mismatch: bound as {var:?}, ASSERT uses {assert_var:?}"
        )));
    }
    let rest = &rest[dot + 1..];
    let is_pos = rest
        .to_ascii_uppercase()
        .find(" IS UNIQUE")
        .ok_or_else(|| syntax_err("CONSTRAINT: expected 'IS UNIQUE'"))?;
    let prop = rest[..is_pos].trim().to_owned();
    if prop.is_empty() {
        return Err(syntax_err("CONSTRAINT: empty property name"));
    }
    let tail = rest[is_pos + " IS UNIQUE".len()..].trim();
    if !tail.is_empty() {
        return Err(syntax_err(&format!(
            "unexpected trailing tokens after CONSTRAINT: {tail}"
        )));
    }
    Ok((label, prop))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tessera_graph::gql::DdlStatement;

    // ── ALTER LABEL … APPEND ONLY (issue #61) ──────────────────────────────
    //
    // `ALTER` rather than `CREATE`: nothing is created, an existing label's
    // schema is changed. It also keeps the parser unambiguous — `CREATE` is
    // already the node-mutation keyword, so every DDL form under it has to
    // reserve its second word, whereas `ALTER` is otherwise unused.

    #[test]
    fn parse_alter_label_set_append_only() {
        let stmt = try_parse_ddl("ALTER LABEL :Event SET APPEND ONLY").unwrap().unwrap();
        assert_eq!(
            stmt,
            DdlStatement::SetLabelAppendOnly { label: "Event".to_owned(), on: true }
        );
    }

    #[test]
    fn parse_alter_label_remove_append_only() {
        let stmt = try_parse_ddl("ALTER LABEL :Event REMOVE APPEND ONLY").unwrap().unwrap();
        assert_eq!(
            stmt,
            DdlStatement::SetLabelAppendOnly { label: "Event".to_owned(), on: false }
        );
    }

    #[test]
    fn parse_alter_label_accepts_semicolon_and_mixed_case() {
        let stmt = try_parse_ddl("alter label :AuditEvent set append only;").unwrap().unwrap();
        assert_eq!(
            stmt,
            DdlStatement::SetLabelAppendOnly { label: "AuditEvent".to_owned(), on: true }
        );
    }

    #[test]
    fn parse_alter_label_accepts_label_without_colon() {
        // The colon is idiomatic Cypher but the label is unambiguous without it.
        let stmt = try_parse_ddl("ALTER LABEL Event SET APPEND ONLY").unwrap().unwrap();
        assert_eq!(
            stmt,
            DdlStatement::SetLabelAppendOnly { label: "Event".to_owned(), on: true }
        );
    }

    #[test]
    fn alter_label_without_a_known_action_is_a_syntax_error() {
        // Malformed DDL must surface as an error rather than falling through to
        // the GQL parser, which would report something unrelated.
        let err = try_parse_ddl("ALTER LABEL :Event SET SOMETHING ELSE").unwrap_err();
        assert!(
            format!("{err}").contains("APPEND ONLY"),
            "the error must name the expected form, got: {err}"
        );
    }

    #[test]
    fn alter_label_with_empty_label_is_a_syntax_error() {
        let err = try_parse_ddl("ALTER LABEL : SET APPEND ONLY").unwrap_err();
        assert!(
            format!("{err}").contains("label"),
            "the error must mention the missing label, got: {err}"
        );
    }

    #[test]
    fn alter_alone_is_a_syntax_error_not_a_fallthrough() {
        let err = try_parse_ddl("ALTER LABEL").unwrap_err();
        assert!(format!("{err}").contains("ALTER LABEL"), "got: {err}");
    }

    #[test]
    fn alter_user_is_left_to_the_admin_parser() {
        // Regression pin. Claiming the bare `ALTER` keyword here made every
        // `ALTER USER` statement fail with a DDL syntax error, because this
        // parser runs before the admin one. Returning `None` hands them on.
        for input in [
            "ALTER USER alice SET PASSWORD 'x'",
            "ALTER USER bob SET ADMIN true",
            "ALTER USER carol SET STATUS SUSPENDED",
        ] {
            assert!(
                try_parse_ddl(input).unwrap().is_none(),
                "{input:?} belongs to the admin parser, not DDL"
            );
        }
    }

    #[test]
    fn alter_of_an_unrelated_object_falls_through_rather_than_erroring() {
        // Anything other than ALTER LABEL is not this parser's business: it
        // must fall through, not claim the statement with a DDL error.
        for input in ["ALTER INDEX ON :Person(id)", "ALTER FOO", "ALTER"] {
            assert!(try_parse_ddl(input).unwrap().is_none(), "{input:?}");
        }
    }

    // Edge cases for the SET/REMOVE split. The parser finds the keyword by
    // scanning the uppercased remainder, so a label that merely contains those
    // letters must not confuse it.

    #[test]
    fn label_containing_the_action_keyword_parses_correctly() {
        let stmt = try_parse_ddl("ALTER LABEL :SetPoint SET APPEND ONLY").unwrap().unwrap();
        assert_eq!(
            stmt,
            DdlStatement::SetLabelAppendOnly { label: "SetPoint".to_owned(), on: true },
            "a label starting with SET must not be mistaken for the keyword"
        );

        let stmt = try_parse_ddl("ALTER LABEL :RemoveMe REMOVE APPEND ONLY").unwrap().unwrap();
        assert_eq!(
            stmt,
            DdlStatement::SetLabelAppendOnly { label: "RemoveMe".to_owned(), on: false }
        );
    }

    #[test]
    fn extra_whitespace_around_the_action_is_tolerated() {
        let stmt = try_parse_ddl("ALTER LABEL :Event  SET  APPEND ONLY").unwrap().unwrap();
        assert_eq!(
            stmt,
            DdlStatement::SetLabelAppendOnly { label: "Event".to_owned(), on: true }
        );
    }

    #[test]
    fn trailing_tokens_after_append_only_are_rejected() {
        // Must not silently accept a statement that says more than it means.
        let err = try_parse_ddl("ALTER LABEL :Event SET APPEND ONLY SET APPEND ONLY").unwrap_err();
        assert!(format!("{err}").contains("APPEND ONLY"), "got: {err}");
    }

    #[test]
    fn parse_show_append_only_info() {
        let stmt = try_parse_ddl("SHOW APPEND ONLY INFO").unwrap().unwrap();
        assert_eq!(stmt, DdlStatement::ShowAppendOnlyInfo);
    }

    #[test]
    fn a_node_pattern_starting_with_alter_is_not_ddl() {
        // Defence against over-eager prefix matching: `ALTER` only introduces
        // DDL when followed by LABEL.
        assert!(try_parse_ddl("MATCH (n:Alteration) RETURN n").unwrap().is_none());
    }

    #[test]
    fn parse_create_index_legacy() {
        let stmt = try_parse_ddl("CREATE INDEX ON :Person(id)").unwrap().unwrap();
        assert_eq!(
            stmt,
            DdlStatement::CreateIndexLegacy { label: "Person".to_owned(), prop: "id".to_owned() }
        );
    }

    #[test]
    fn parse_create_index_legacy_semicolon() {
        let stmt = try_parse_ddl("CREATE INDEX ON :AssetNode(organizationId);").unwrap().unwrap();
        assert_eq!(
            stmt,
            DdlStatement::CreateIndexLegacy {
                label: "AssetNode".to_owned(),
                prop: "organizationId".to_owned()
            }
        );
    }

    #[test]
    fn parse_create_index_for() {
        let stmt = try_parse_ddl("CREATE INDEX FOR (n:Asset) ON (n.status)").unwrap().unwrap();
        assert_eq!(
            stmt,
            DdlStatement::CreateIndexFor { label: "Asset".to_owned(), prop: "status".to_owned() }
        );
    }

    #[test]
    fn parse_drop_index() {
        let stmt = try_parse_ddl("DROP INDEX ON :Risk(id)").unwrap().unwrap();
        assert_eq!(
            stmt,
            DdlStatement::DropIndex { label: "Risk".to_owned(), prop: "id".to_owned() }
        );
    }

    #[test]
    fn parse_create_constraint_assert_unique() {
        let stmt = try_parse_ddl("CREATE CONSTRAINT ON (n:AssetNode) ASSERT n.id IS UNIQUE")
            .unwrap()
            .unwrap();
        assert_eq!(
            stmt,
            DdlStatement::CreateUniqueConstraint {
                label: "AssetNode".to_owned(),
                prop: "id".to_owned()
            }
        );
    }

    #[test]
    fn parse_drop_constraint() {
        let stmt = try_parse_ddl("DROP CONSTRAINT ON (n:AssetNode) ASSERT n.id IS UNIQUE")
            .unwrap()
            .unwrap();
        assert_eq!(
            stmt,
            DdlStatement::DropConstraint { label: "AssetNode".to_owned(), prop: "id".to_owned() }
        );
    }

    #[test]
    fn parse_show_index_info() {
        let stmt = try_parse_ddl("SHOW INDEX INFO").unwrap().unwrap();
        assert_eq!(stmt, DdlStatement::ShowIndexInfo);
    }

    #[test]
    fn parse_show_constraint_info() {
        let stmt = try_parse_ddl("SHOW CONSTRAINT INFO").unwrap().unwrap();
        assert_eq!(stmt, DdlStatement::ShowConstraintInfo);
    }

    #[test]
    fn non_ddl_returns_none() {
        let result = try_parse_ddl("MATCH (n) RETURN n").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn create_without_index_falls_through() {
        let result = try_parse_ddl("CREATE (n:Person {name:'Alice'})").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn parse_create_index_case_insensitive() {
        let stmt = try_parse_ddl("create index on :Person(id)").unwrap().unwrap();
        assert_eq!(
            stmt,
            DdlStatement::CreateIndexLegacy { label: "Person".to_owned(), prop: "id".to_owned() }
        );
    }

    #[test]
    fn parse_constraint_variable_name_ignored() {
        let stmt = try_parse_ddl("CREATE CONSTRAINT ON (v:AssetNode) ASSERT v.id IS UNIQUE")
            .unwrap()
            .unwrap();
        assert_eq!(
            stmt,
            DdlStatement::CreateUniqueConstraint {
                label: "AssetNode".to_owned(),
                prop: "id".to_owned()
            }
        );
    }
}
