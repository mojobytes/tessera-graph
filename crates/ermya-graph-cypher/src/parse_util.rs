// SPDX-License-Identifier: BSL-1.1

//! Shared prefix-parsing helpers for the hand-written statement parsers
//! (`admin`, `ddl`). These provide case-insensitive prefix stripping,
//! identifier extraction, and a uniform `GqlSyntaxError` constructor so the
//! admin and DDL grammars stay byte-for-byte consistent.

use ermya_graph::{Error, Result};

/// Builds a [`Error::GqlSyntaxError`] at line 1, col 1 (these parsers operate on
/// already-trimmed single statements, so a precise position is not tracked).
pub(crate) fn syntax_err(msg: &str) -> Error {
    Error::GqlSyntaxError {
        line: 1,
        col: 1,
        message: msg.to_owned(),
    }
}

/// Strips `prefix` from the start of `s`, comparing case-insensitively. Returns
/// the remainder on success, or a syntax error naming the expected prefix.
pub(crate) fn strip_ci<'a>(s: &'a str, prefix: &str) -> Result<&'a str> {
    if s.len() < prefix.len()
        || !s.as_bytes()[..prefix.len()]
            .iter()
            .zip(prefix.as_bytes())
            .all(|(a, b)| a.eq_ignore_ascii_case(b))
    {
        return Err(syntax_err(&format!("expected {prefix}")));
    }
    Ok(&s[prefix.len()..])
}

/// Like [`strip_ci`] but trims leading whitespace before matching.
pub(crate) fn strip_ci_ws<'a>(s: &'a str, prefix: &str) -> Result<&'a str> {
    strip_ci(s.trim_start(), prefix)
}

/// Takes a leading identifier (`[A-Za-z_][A-Za-z0-9_.-]*`) from `s`, returning
/// the identifier and the unconsumed remainder. Leading whitespace is skipped.
pub(crate) fn take_identifier(s: &str) -> Result<(String, &str)> {
    let s = s.trim_start();
    let mut end = 0usize;
    for (i, c) in s.char_indices() {
        let ok = if i == 0 {
            c.is_ascii_alphabetic() || c == '_'
        } else {
            c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-')
        };
        if !ok {
            end = i;
            break;
        }
        end = i + c.len_utf8();
    }
    if end == 0 {
        return Err(syntax_err("expected identifier"));
    }
    Ok((s[..end].to_owned(), &s[end..]))
}
