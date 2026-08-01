// SPDX-License-Identifier: LicenseRef-TesseraGraph-Proprietary

//! Admin-statement parser — runs before the regular GQL parser so
//! `CREATE USER`, `DROP USER`, `ALTER USER ...`, and `SHOW USERS` are
//! recognised as top-level prefixes and routed to the server's admin
//! handler (Task 8) instead of the graph engine.
//!
//! The grammar is intentionally shallow — one line per statement, a
//! single-quoted string literal for password (with `''` as the escape
//! for a literal single quote), and a restricted identifier shape for
//! usernames (validated again at the store, so this parser only needs
//! to lex them).

use tessera_graph::gql::{
    AccessLevelAst, AdminStatement, DatabaseOptions, GrantTargetAst, SecretPlainPassword,
};
use tessera_graph::Result;

use crate::parse_util::{strip_ci, strip_ci_ws, syntax_err, take_identifier};

/// Database names reserved by the system. Mirror of the server-side
/// list in `tessera-graph-server/src/auth/system_graph.rs`; kept in
/// sync by convention (only 2 entries, spec §6.1). The parser rejects
/// early so malformed admin flows never reach the auth store.
const RESERVED_DATABASE_NAMES: &[&str] = &["system", "default"];

/// Try to parse `input` as an admin statement. Returns `Ok(None)` if the
/// input does not start with an admin keyword — the caller then falls
/// back to the regular GQL parser.
///
/// # Errors
///
/// Returns [`Error::GqlSyntaxError`] if the input begins with an admin
/// keyword but the remainder is malformed (bad identifier, missing
/// password literal, unknown `SET ...` subclause, unterminated string,
/// etc.).
pub fn try_parse_admin(input: &str) -> Result<Option<AdminStatement>> {
    let trimmed = input.trim_end_matches(';').trim();
    let upper = trimmed.to_ascii_uppercase();

    if upper.starts_with("CREATE USER ") {
        return parse_create_user(trimmed).map(Some);
    }
    if upper.starts_with("DROP USER ") {
        return parse_drop_user(trimmed).map(Some);
    }
    if upper.starts_with("ALTER USER ") {
        return parse_alter_user(trimmed).map(Some);
    }
    if upper == "SHOW USERS" {
        return Ok(Some(AdminStatement::ShowUsers));
    }
    if upper.starts_with("CREATE DATABASE ") {
        return parse_create_database(trimmed).map(Some);
    }
    if upper.starts_with("DROP DATABASE ") {
        return parse_drop_database(trimmed).map(Some);
    }
    if upper == "SHOW DATABASES" {
        return Ok(Some(AdminStatement::ShowDatabases));
    }
    if upper.starts_with("GRANT ") {
        return parse_grant(trimmed).map(Some);
    }
    if upper.starts_with("REVOKE ") {
        return parse_revoke(trimmed).map(Some);
    }
    if upper == "SHOW GRANTS" {
        return Ok(Some(AdminStatement::ShowGrants { filter_user: None }));
    }
    if upper.starts_with("SHOW GRANTS ") {
        return parse_show_grants(trimmed).map(Some);
    }
    Ok(None)
}

// ── per-statement parsers ────────────────────────────────────────────────────

fn parse_create_user(s: &str) -> Result<AdminStatement> {
    let rest = strip_ci(s, "CREATE USER")?;
    let (username, rest) = take_identifier(rest)?;
    let rest = strip_ci_ws(rest, "SET")?;
    let rest = strip_ci_ws(rest, "PASSWORD")?;
    let password = take_string_literal(rest.trim_start())?;
    Ok(AdminStatement::CreateUser {
        username,
        password: SecretPlainPassword::new(password.into_bytes()),
    })
}

fn parse_drop_user(s: &str) -> Result<AdminStatement> {
    let rest = strip_ci(s, "DROP USER")?;
    let (username, tail) = take_identifier(rest)?;
    if !tail.trim().is_empty() {
        return Err(syntax_err("unexpected tokens after DROP USER"));
    }
    Ok(AdminStatement::DropUser { username })
}

fn parse_alter_user(s: &str) -> Result<AdminStatement> {
    let rest = strip_ci(s, "ALTER USER")?;
    let (username, rest) = take_identifier(rest)?;
    let rest = strip_ci_ws(rest, "SET")?;
    let after_set = rest.trim_start();
    let upper = after_set.to_ascii_uppercase();

    if upper.starts_with("PASSWORD") {
        let rest = strip_ci_ws(rest, "PASSWORD")?;
        let password = take_string_literal(rest.trim_start())?;
        return Ok(AdminStatement::AlterUserPassword {
            username,
            password: SecretPlainPassword::new(password.into_bytes()),
        });
    }
    if upper.starts_with("STATUS") {
        let rest = strip_ci_ws(rest, "STATUS")?;
        let word = next_word(rest);
        let enabled = match word.to_ascii_uppercase().as_str() {
            "ACTIVE" => true,
            "SUSPENDED" => false,
            other => {
                return Err(syntax_err(&format!(
                    "SET STATUS expected ACTIVE|SUSPENDED, got {other}"
                )));
            }
        };
        return Ok(AdminStatement::AlterUserStatus { username, enabled });
    }
    if upper.starts_with("ADMIN") {
        let rest = strip_ci_ws(rest, "ADMIN")?;
        let word = next_word(rest);
        let is_admin = match word.to_ascii_uppercase().as_str() {
            "TRUE" => true,
            "FALSE" => false,
            other => {
                return Err(syntax_err(&format!(
                    "SET ADMIN expected TRUE|FALSE, got {other}"
                )));
            }
        };
        return Ok(AdminStatement::AlterUserAdmin { username, is_admin });
    }
    Err(syntax_err("ALTER USER ... SET expects PASSWORD|STATUS|ADMIN"))
}

fn parse_create_database(s: &str) -> Result<AdminStatement> {
    let rest = strip_ci(s, "CREATE DATABASE")?;
    let (name, tail) = take_database_name(rest)?;
    let tail = tail.trim_start();

    // Optional `IF NOT EXISTS` — case-insensitive, exact three tokens.
    let (if_not_exists, tail) = if starts_with_ci_word(tail, "IF") {
        let after_if = tail[2..].trim_start();
        if !starts_with_ci_word(after_if, "NOT") {
            return Err(syntax_err("CREATE DATABASE: expected IF NOT EXISTS"));
        }
        let after_not = after_if[3..].trim_start();
        if !starts_with_ci_word(after_not, "EXISTS") {
            return Err(syntax_err("CREATE DATABASE: expected IF NOT EXISTS"));
        }
        (true, after_not[6..].trim_start())
    } else {
        (false, tail)
    };

    // Optional `WITH OPTIONS { ... }`.
    let options = if tail.is_empty() {
        DatabaseOptions::default()
    } else if starts_with_ci_word(tail, "WITH") {
        let after_with = tail[4..].trim_start();
        if !starts_with_ci_word(after_with, "OPTIONS") {
            return Err(syntax_err("CREATE DATABASE: expected WITH OPTIONS"));
        }
        let after_opts = after_with[7..].trim_start();
        let body = after_opts
            .strip_prefix('{')
            .ok_or_else(|| syntax_err("expected '{' after WITH OPTIONS"))?
            .trim();
        let body = body
            .strip_suffix('}')
            .ok_or_else(|| syntax_err("expected '}' closing WITH OPTIONS"))?;
        parse_database_options(body.trim())?
    } else {
        return Err(syntax_err(&format!(
            "unexpected trailing tokens after CREATE DATABASE: {tail}"
        )));
    };

    Ok(AdminStatement::CreateDatabase {
        name,
        if_not_exists,
        options,
    })
}

fn parse_drop_database(s: &str) -> Result<AdminStatement> {
    let rest = strip_ci(s, "DROP DATABASE")?;
    let (name, tail) = take_database_name(rest)?;
    let tail = tail.trim();

    let if_exists = if tail.is_empty() {
        false
    } else if starts_with_ci_word(tail, "IF") {
        let after_if = tail[2..].trim_start();
        if !starts_with_ci_word(after_if, "EXISTS") {
            return Err(syntax_err("DROP DATABASE: expected IF EXISTS"));
        }
        let rest_after = after_if[6..].trim();
        if !rest_after.is_empty() {
            return Err(syntax_err(&format!(
                "unexpected trailing tokens after DROP DATABASE: {rest_after}"
            )));
        }
        true
    } else {
        return Err(syntax_err(&format!(
            "unexpected trailing tokens after DROP DATABASE: {tail}"
        )));
    };

    Ok(AdminStatement::DropDatabase { name, if_exists })
}

fn parse_grant(s: &str) -> Result<AdminStatement> {
    // Shape: GRANT {ACCESS|WRITE} ON DATABASE {<name>|*} TO <username>
    let rest = strip_ci(s, "GRANT")?;
    let rest = rest.trim_start();

    let level_word = next_word(rest);
    let (level, rest) = match level_word.to_ascii_uppercase().as_str() {
        "ACCESS" => (AccessLevelAst::Read, &rest[level_word.len()..]),
        "WRITE" => (AccessLevelAst::ReadWrite, &rest[level_word.len()..]),
        other => {
            return Err(syntax_err(&format!(
                "GRANT expected ACCESS|WRITE, got {other}"
            )));
        }
    };
    let rest = expect_keyword(rest, "ON")?;
    let rest = expect_keyword(rest, "DATABASE")?;
    let (target, rest) = take_grant_target(rest)?;
    let rest = expect_keyword(rest, "TO")?;
    let (username, tail) = take_identifier(rest)?;
    reject_trailing(tail, "GRANT")?;
    Ok(AdminStatement::Grant {
        username,
        target,
        level,
    })
}

fn parse_revoke(s: &str) -> Result<AdminStatement> {
    // Shape: REVOKE ACCESS ON DATABASE {<name>|*} FROM <username>
    //
    // We accept only the single level keyword `ACCESS` — a revoke
    // removes the whole edge regardless of which level it carried, so
    // `REVOKE WRITE` would be ambiguous against spec §6.2.
    let rest = strip_ci(s, "REVOKE")?;
    let rest = expect_keyword(rest, "ACCESS")?;
    let rest = expect_keyword(rest, "ON")?;
    let rest = expect_keyword(rest, "DATABASE")?;
    let (target, rest) = take_grant_target(rest)?;
    let rest = expect_keyword(rest, "FROM")?;
    let (username, tail) = take_identifier(rest)?;
    reject_trailing(tail, "REVOKE")?;
    Ok(AdminStatement::Revoke { username, target })
}

fn parse_show_grants(s: &str) -> Result<AdminStatement> {
    // Shape: SHOW GRANTS [FOR <username>] — caller already matched the
    // `SHOW GRANTS ` prefix, so tail is everything after.
    let rest = strip_ci(s, "SHOW GRANTS")?;
    let rest = rest.trim();
    if rest.is_empty() {
        return Ok(AdminStatement::ShowGrants { filter_user: None });
    }
    let after_for = expect_keyword(rest, "FOR")?;
    let (username, tail) = take_identifier(after_for)?;
    reject_trailing(tail, "SHOW GRANTS")?;
    Ok(AdminStatement::ShowGrants {
        filter_user: Some(username),
    })
}

/// Parse the target of a GRANT/REVOKE. Wildcard `*` is accepted without
/// running the database-name validator (it is not a name); any other
/// token goes through the same validation as `CREATE DATABASE`.
fn take_grant_target(s: &str) -> Result<(GrantTargetAst, &str)> {
    let s = s.trim_start();
    if let Some(after_star) = s.strip_prefix('*') {
        // Wildcard is a single-char token — enforce whitespace or EOF
        // afterwards so `*foo` is a syntax error, not "Wildcard then
        // trailing garbage".
        if !after_star.is_empty() && !after_star.starts_with(char::is_whitespace) {
            return Err(syntax_err(&format!(
                "unexpected token after wildcard target: {after_star}"
            )));
        }
        return Ok((GrantTargetAst::Wildcard, after_star));
    }
    let (name, tail) = take_database_name(s)?;
    Ok((GrantTargetAst::Named(name), tail))
}

/// Expect a specific case-insensitive keyword as the next token,
/// consuming it and the surrounding whitespace. Returns the slice
/// after the keyword, or a descriptive error.
fn expect_keyword<'a>(s: &'a str, keyword: &str) -> Result<&'a str> {
    let s = s.trim_start();
    if !starts_with_ci_word(s, keyword) {
        return Err(syntax_err(&format!(
            "expected {} keyword, got {:?}",
            keyword,
            next_word(s)
        )));
    }
    Ok(&s[keyword.len()..])
}

/// Reject any non-whitespace remainder, producing a uniform error
/// message across the GRANT/REVOKE/SHOW statements.
fn reject_trailing(s: &str, context: &str) -> Result<()> {
    let t = s.trim();
    if t.is_empty() {
        Ok(())
    } else {
        Err(syntax_err(&format!(
            "unexpected trailing tokens after {context}: {t}"
        )))
    }
}

fn parse_database_options(body: &str) -> Result<DatabaseOptions> {
    let mut opts = DatabaseOptions::default();
    if body.is_empty() {
        return Ok(opts);
    }
    for raw in body.split(',') {
        let kv = raw.trim();
        if kv.is_empty() {
            continue;
        }
        let (k, v) = kv
            .split_once(':')
            .ok_or_else(|| syntax_err(&format!("malformed option: {kv}")))?;
        let (k, v) = (k.trim(), v.trim());
        match k {
            "max_size_bytes" => {
                opts.max_size_bytes = Some(v.parse().map_err(|e| {
                    syntax_err(&format!("max_size_bytes must be u64: {e}"))
                })?);
            }
            "max_connections" => {
                opts.max_connections = Some(v.parse().map_err(|e| {
                    syntax_err(&format!("max_connections must be usize: {e}"))
                })?);
            }
            other => {
                return Err(syntax_err(&format!("unknown option: {other}")));
            }
        }
    }
    Ok(opts)
}

/// Consume a database-name token and validate it against
/// `^[a-zA-Z_][a-zA-Z0-9_-]{0,62}$` plus the reserved list. Mirrors
/// the server-side validation; kept at parse time as defence-in-depth
/// so malformed names never reach the auth store.
fn take_database_name(s: &str) -> Result<(String, &str)> {
    let s = s.trim_start();
    if s.is_empty() {
        return Err(syntax_err("invalid database name: missing"));
    }
    let end = s
        .find(char::is_whitespace)
        .unwrap_or(s.len());
    let candidate = &s[..end];
    validate_database_name(candidate)?;
    Ok((candidate.to_owned(), &s[end..]))
}

fn validate_database_name(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(syntax_err("invalid database name: empty"));
    }
    if name.len() > 63 {
        return Err(syntax_err(&format!(
            "invalid database name: too long ({}>63)",
            name.len()
        )));
    }
    let mut chars = name.chars();
    let first = chars.next().expect("non-empty");
    if !(first.is_ascii_alphabetic() || first == '_') {
        return Err(syntax_err(&format!(
            "invalid database name {name:?}: first char must be [a-zA-Z_]"
        )));
    }
    for c in chars {
        if !(c.is_ascii_alphanumeric() || c == '_' || c == '-') {
            return Err(syntax_err(&format!(
                "invalid database name {name:?}: char {c:?} not in [a-zA-Z0-9_-]"
            )));
        }
    }
    if RESERVED_DATABASE_NAMES.contains(&name) {
        return Err(syntax_err(&format!(
            "reserved database name: {name}"
        )));
    }
    Ok(())
}

/// Case-insensitive check: does `s` start with `word` followed by
/// whitespace or end-of-string? Used to match SQL-style keyword tokens
/// without treating prefixes of longer words as matches.
fn starts_with_ci_word(s: &str, word: &str) -> bool {
    let bytes = s.as_bytes();
    let w = word.as_bytes();
    if bytes.len() < w.len() {
        return false;
    }
    if !bytes[..w.len()]
        .iter()
        .zip(w)
        .all(|(a, b)| a.eq_ignore_ascii_case(b))
    {
        return false;
    }
    bytes
        .get(w.len())
        .is_none_or(u8::is_ascii_whitespace)
}

// ── lexer helpers ────────────────────────────────────────────────────────────

fn take_string_literal(s: &str) -> Result<String> {
    let bytes = s.as_bytes();
    if bytes.first() != Some(&b'\'') {
        return Err(syntax_err("expected single-quoted password literal"));
    }
    let mut out = Vec::new();
    let mut i = 1usize;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'\'' {
            // '' is the escape for a literal single quote.
            if bytes.get(i + 1) == Some(&b'\'') {
                out.push(b'\'');
                i += 2;
                continue;
            }
            // End of string — the remainder of the input must be empty
            // (the caller is expected to have stripped any trailing
            // whitespace / semicolon).
            let tail = std::str::from_utf8(&bytes[i + 1..])
                .unwrap_or("")
                .trim();
            if !tail.is_empty() {
                return Err(syntax_err(&format!(
                    "unexpected tokens after string: {tail}"
                )));
            }
            return String::from_utf8(out)
                .map_err(|e| syntax_err(&format!("password not UTF-8: {e}")));
        }
        out.push(b);
        i += 1;
    }
    Err(syntax_err("unterminated string literal"))
}

fn next_word(s: &str) -> String {
    s.split_whitespace().next().unwrap_or("").to_owned()
}
