//! String-level pre-processor for Cypher compatibility.
//!
//! Transforms Cypher-specific syntax in the input string into GQL-compatible
//! syntax before passing to the core parser.
//!
//! # Cypher constructs handled
//!
//! | Construct                         | Status     | Notes                                         |
//! |-----------------------------------|------------|-----------------------------------------------|
//! | `/* block comments */`            | Stripped   | Replaced with equivalent whitespace           |
//! | `` `backtick identifiers` ``      | Rewritten  | Spaces → underscores, backticks removed       |
//! | `STARTS WITH`, `ENDS WITH`        | Passed     | Parsed by core under `enterprise-helpers`     |
//! | `CONTAINS`                        | Passed     | Parsed by core under `enterprise-helpers`     |
//! | `IN [list]`                       | Passed     | Parsed by core under `enterprise-helpers`     |
//! | `id(n)`, `type(r)`, `labels(n)`   | Passed     | Parsed by core under `enterprise-helpers`     |
//! | `REMOVE n.prop`                   | Deferred   | TODO: Phase 1.5.5 — rewrite to SET n.prop=null|
//! | `OPTIONAL MATCH`                  | Deferred   | TODO: Phase 1.5.6                             |
//! | `WITH`, `UNWIND`                  | Deferred   | TODO: Phase 1.5.7                             |

use tessera_graph::Error;

/// Pre-processes a Cypher-compatible query string into GQL-compatible form.
///
/// Transformations applied in order:
/// 1. `/* block comments */` → stripped (replaced with spaces to preserve positions)
/// 2. `` `backtick identifiers` `` → spaces replaced with underscores, backticks removed
///
/// Cypher operators (`STARTS WITH`, `ENDS WITH`, `CONTAINS`, `IN`, scalar
/// functions `id`, `type`, `labels`) are passed through unchanged and handled
/// by the core parser when compiled with the `enterprise-helpers` feature.
///
/// # Errors
///
/// Returns `GqlSyntaxError` for unclosed block comments or backtick sequences.
pub fn cypher_to_gql(input: &str) -> tessera_graph::Result<String> {
    let after_comments = strip_block_comments(input)?;
    let after_backticks = convert_backtick_idents(&after_comments)?;
    Ok(after_backticks)
}

/// Scans for Cypher-only constructs and returns an error if any are found.
///
/// Used in `StrictGql` mode to provide clear diagnostics.
///
/// Rejected constructs:
/// - `/* block comments */`
/// - `` `backtick identifiers` ``
/// - `STARTS WITH` (two-word string operator)
/// - `ENDS WITH` (two-word string operator)
/// - `CONTAINS` (as a Cypher string operator; checked by word-boundary context)
/// - `IN [` (list membership operator with literal list)
/// - `id(`, `type(`, `labels(` (scalar function calls)
/// - `REMOVE` (Cypher property removal keyword)
///
/// # Errors
///
/// Returns `GqlSyntaxError` when any Cypher-only construct is detected.
pub fn reject_cypher_constructs(input: &str) -> tessera_graph::Result<()> {
    // Blank out string literals so keywords inside strings are not flagged.
    let masked = blank_string_literals(input);

    // Check for block comments
    if let Some(pos) = masked.find("/*") {
        return Err(Error::GqlSyntaxError {
            line: find_line(input, pos),
            col: find_col(input, pos),
            message: "block comments (/* ... */) are not valid in strict GQL mode; \
                      use -- for line comments or switch to cypher-compat mode"
                .into(),
        });
    }

    // Check for backtick identifiers
    if let Some(pos) = masked.find('`') {
        return Err(Error::GqlSyntaxError {
            line: find_line(input, pos),
            col: find_col(input, pos),
            message: "backtick-quoted identifiers are not valid in strict GQL mode; \
                      use standard identifiers or switch to cypher-compat mode"
                .into(),
        });
    }

    // Check for Cypher string/list operators (case-insensitive word-boundary scan).
    let upper = masked.to_ascii_uppercase();

    if let Some(pos) = find_cypher_operator(&upper, "STARTS WITH") {
        return Err(Error::GqlSyntaxError {
            line: find_line(input, pos),
            col: find_col(input, pos),
            message: "STARTS WITH is a Cypher operator not valid in strict GQL mode; \
                      switch to cypher-compat mode"
                .into(),
        });
    }

    if let Some(pos) = find_cypher_operator(&upper, "ENDS WITH") {
        return Err(Error::GqlSyntaxError {
            line: find_line(input, pos),
            col: find_col(input, pos),
            message: "ENDS WITH is a Cypher operator not valid in strict GQL mode; \
                      switch to cypher-compat mode"
                .into(),
        });
    }

    if let Some(pos) = find_word(&upper, "CONTAINS") {
        return Err(Error::GqlSyntaxError {
            line: find_line(input, pos),
            col: find_col(input, pos),
            message: "CONTAINS is a Cypher operator not valid in strict GQL mode; \
                      switch to cypher-compat mode"
                .into(),
        });
    }

    // `IN [` — list membership (we only flag `IN` followed by a literal list bracket
    // to avoid false positives on identifiers that happen to end in `IN`).
    if let Some(pos) = find_in_list_operator(&upper) {
        return Err(Error::GqlSyntaxError {
            line: find_line(input, pos),
            col: find_col(input, pos),
            message: "IN [...] list membership is a Cypher operator not valid in strict GQL mode; \
                      switch to cypher-compat mode"
                .into(),
        });
    }

    // Cypher scalar functions: id(, type(, labels(
    for func in &["ID(", "TYPE(", "LABELS("] {
        if let Some(pos) = find_function_call(&upper, func) {
            let func_lower = func.trim_end_matches('(').to_ascii_lowercase();
            return Err(Error::GqlSyntaxError {
                line: find_line(input, pos),
                col: find_col(input, pos),
                message: format!(
                    "{func_lower}() is a Cypher function not valid in strict GQL mode; \
                     switch to cypher-compat mode"
                ),
            });
        }
    }

    // REMOVE keyword
    if let Some(pos) = find_word(&upper, "REMOVE") {
        return Err(Error::GqlSyntaxError {
            line: find_line(input, pos),
            col: find_col(input, pos),
            message: "REMOVE is a Cypher keyword not valid in strict GQL mode; \
                      use SET n.prop = null or switch to cypher-compat mode"
                .into(),
        });
    }

    Ok(())
}

// ── String-literal masking ─────────────────────────────────────────────────

/// Returns a copy of `input` with the *contents* of single-quoted (`'...'`)
/// and double-quoted (`"..."`) string literals replaced by space characters,
/// preserving byte offsets of all non-literal characters.
///
/// The enclosing quote characters themselves are kept so that the surrounding
/// context is still recognisable (e.g. the parser can still see `'...'`).
/// Escape sequences (`\'`, `\"`, `\\`) are skipped so an escaped quote does
/// not terminate the literal prematurely.
fn blank_string_literals(input: &str) -> String {
    let mut result = String::with_capacity(input.len());
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let ch = chars[i];
        result.push(ch);
        i += 1;
        if ch == '\'' || ch == '"' {
            let quote = ch;
            while i < chars.len() {
                let c = chars[i];
                if c == '\\' && i + 1 < chars.len() {
                    // Consume the escape sequence as spaces.
                    for _ in 0..c.len_utf8() {
                        result.push(' ');
                    }
                    i += 1;
                    for _ in 0..chars[i].len_utf8() {
                        result.push(' ');
                    }
                    i += 1;
                    continue;
                }
                if c == quote {
                    result.push(c);
                    i += 1;
                    break;
                }
                // Replace literal content with spaces (preserving byte length).
                for _ in 0..c.len_utf8() {
                    result.push(' ');
                }
                i += 1;
            }
        }
    }
    result
}

// ── Operator detection helpers ────────────────────────────────────────────────

/// Searches for a multi-word Cypher operator (e.g. `STARTS WITH`) in an
/// already upper-cased input, treating both tokens as whole words.
///
/// Returns the byte offset of the first match, or `None`.
fn find_cypher_operator(upper_input: &str, operator: &str) -> Option<usize> {
    // The operator has a space between words; split on the first space.
    let (first, second) = operator.split_once(' ')?;

    let mut search_from = 0;
    while let Some(idx) = upper_input[search_from..].find(first) {
        let abs = search_from + idx;
        // Check word boundary before `first`.
        if abs > 0 && upper_input.as_bytes()[abs - 1].is_ascii_alphanumeric() {
            search_from = abs + 1;
            continue;
        }
        // Check that `first` ends on a word boundary.
        let after_first = abs + first.len();
        if after_first >= upper_input.len()
            || upper_input.as_bytes()[after_first].is_ascii_alphanumeric()
        {
            search_from = abs + 1;
            continue;
        }
        // Skip whitespace between the two words (tabs, spaces, etc.).
        let mut gap = after_first;
        while gap < upper_input.len() && upper_input.as_bytes()[gap].is_ascii_whitespace() {
            gap += 1;
        }
        // Check that `second` follows.
        if upper_input[gap..].starts_with(second) {
            let after_second = gap + second.len();
            // Confirm word boundary after `second`.
            if after_second >= upper_input.len()
                || !upper_input.as_bytes()[after_second].is_ascii_alphanumeric()
            {
                return Some(abs);
            }
        }
        search_from = abs + 1;
    }
    None
}

/// Searches for a whole-word occurrence of `word` in an already upper-cased
/// input string.
///
/// Returns the byte offset of the first match, or `None`.
fn find_word(upper_input: &str, word: &str) -> Option<usize> {
    let mut search_from = 0;
    while let Some(idx) = upper_input[search_from..].find(word) {
        let abs = search_from + idx;
        let before_ok =
            abs == 0 || !upper_input.as_bytes()[abs - 1].is_ascii_alphanumeric();
        let after_ok = abs + word.len() >= upper_input.len()
            || !upper_input.as_bytes()[abs + word.len()].is_ascii_alphanumeric();
        if before_ok && after_ok {
            return Some(abs);
        }
        search_from = abs + 1;
    }
    None
}

/// Searches for `IN` (whole word) followed by optional whitespace and then `[`.
///
/// Returns the byte offset of the `IN` keyword, or `None`.
fn find_in_list_operator(upper_input: &str) -> Option<usize> {
    let bytes = upper_input.as_bytes();
    let mut search_from = 0;
    while let Some(idx) = upper_input[search_from..].find("IN") {
        let abs = search_from + idx;
        // Word-boundary check.
        let before_ok = abs == 0 || !bytes[abs - 1].is_ascii_alphanumeric();
        let after_in = abs + 2;
        let after_ok =
            after_in >= bytes.len() || !bytes[after_in].is_ascii_alphanumeric();
        if before_ok && after_ok {
            // Skip whitespace to see if `[` follows.
            let mut cursor = after_in;
            while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
                cursor += 1;
            }
            if cursor < bytes.len() && bytes[cursor] == b'[' {
                return Some(abs);
            }
        }
        search_from = abs + 1;
    }
    None
}

/// Searches for a function call pattern `FUNC(` (word boundary before `FUNC`)
/// in an already upper-cased input string.
///
/// Returns the byte offset of the function name start, or `None`.
fn find_function_call(upper_input: &str, pattern: &str) -> Option<usize> {
    let mut search_from = 0;
    while let Some(idx) = upper_input[search_from..].find(pattern) {
        let abs = search_from + idx;
        // Word boundary check before the function name.
        let before_ok = abs == 0 || !upper_input.as_bytes()[abs - 1].is_ascii_alphanumeric();
        if before_ok {
            return Some(abs);
        }
        search_from = abs + 1;
    }
    None
}

/// Strips `/* ... */` block comments, replacing them with equivalent whitespace.
///
/// String literal content is not treated as comment markers — a `/*` inside
/// a string literal is passed through unchanged.
fn strip_block_comments(input: &str) -> tessera_graph::Result<String> {
    let mut result = String::with_capacity(input.len());
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        // Skip string literals without scanning for comment markers inside them.
        if chars[i] == '\'' || chars[i] == '"' {
            let quote = chars[i];
            result.push(quote);
            i += 1;
            while i < chars.len() {
                let c = chars[i];
                if c == '\\' && i + 1 < chars.len() {
                    result.push(c);
                    i += 1;
                    result.push(chars[i]);
                    i += 1;
                    continue;
                }
                result.push(c);
                i += 1;
                if c == quote {
                    break;
                }
            }
            continue;
        }

        if i + 1 < chars.len() && chars[i] == '/' && chars[i + 1] == '*' {
            let start = i;
            i += 2;
            let mut found_close = false;
            while i + 1 < chars.len() {
                if chars[i] == '*' && chars[i + 1] == '/' {
                    i += 2;
                    found_close = true;
                    break;
                }
                i += 1;
            }
            if !found_close {
                // Byte offset of the start of the comment
                let byte_offset: usize = chars[..start].iter().map(|c| c.len_utf8()).sum();
                return Err(Error::GqlSyntaxError {
                    line: find_line(input, byte_offset),
                    col: find_col(input, byte_offset),
                    message: "unclosed block comment".into(),
                });
            }
            // Replace with spaces to preserve relative byte positions.
            // Each character in the comment is replaced by exactly as many
            // space bytes as the character occupies (len_utf8 spaces).
            for c in &chars[start..i] {
                for _ in 0..c.len_utf8() {
                    result.push(' ');
                }
            }
        } else {
            result.push(chars[i]);
            i += 1;
        }
    }
    Ok(result)
}

/// Converts backtick-quoted identifiers to standard identifiers.
///
/// Spaces within backtick-quoted identifiers become underscores.
/// Backtick characters inside string literals are passed through unchanged.
fn convert_backtick_idents(input: &str) -> tessera_graph::Result<String> {
    let mut result = String::with_capacity(input.len());
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        // Skip string literals — backticks inside strings are not identifiers.
        if chars[i] == '\'' || chars[i] == '"' {
            let quote = chars[i];
            result.push(quote);
            i += 1;
            while i < chars.len() {
                let c = chars[i];
                if c == '\\' && i + 1 < chars.len() {
                    result.push(c);
                    i += 1;
                    result.push(chars[i]);
                    i += 1;
                    continue;
                }
                result.push(c);
                i += 1;
                if c == quote {
                    break;
                }
            }
            continue;
        }

        if chars[i] == '`' {
            let start = i;
            i += 1;
            let mut ident = String::new();
            let mut found_close = false;
            while i < chars.len() {
                if chars[i] == '`' {
                    found_close = true;
                    i += 1;
                    break;
                }
                if chars[i] == ' ' {
                    ident.push('_');
                } else {
                    ident.push(chars[i]);
                }
                i += 1;
            }
            if !found_close {
                let byte_offset: usize = chars[..start].iter().map(|c| c.len_utf8()).sum();
                return Err(Error::GqlSyntaxError {
                    line: find_line(input, byte_offset),
                    col: find_col(input, byte_offset),
                    message: "unclosed backtick identifier".into(),
                });
            }
            result.push_str(&ident);
        } else {
            result.push(chars[i]);
            i += 1;
        }
    }
    Ok(result)
}

/// Finds the 1-based line number for a byte offset.
fn find_line(input: &str, byte_offset: usize) -> u32 {
    let count = input[..byte_offset].chars().filter(|&c| c == '\n').count();
    // Safe cast: a query string won't have more than u32::MAX lines.
    #[allow(clippy::cast_possible_truncation)]
    {
        (count + 1) as u32
    }
}

/// Finds the 1-based column number for a byte offset.
fn find_col(input: &str, byte_offset: usize) -> u32 {
    let before = &input[..byte_offset];
    let col = before
        .rfind('\n')
        .map_or_else(|| byte_offset + 1, |nl| byte_offset - nl);
    // Safe cast: a query line won't exceed u32::MAX columns.
    #[allow(clippy::cast_possible_truncation)]
    {
        col as u32
    }
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── O3: Position helpers ──────────────────────────────────────────────────

    #[test]
    fn find_line_first_line() {
        assert_eq!(find_line("hello", 0), 1);
    }

    #[test]
    fn find_line_second_line() {
        assert_eq!(find_line("line1\nline2", 6), 2);
    }

    #[test]
    fn find_col_first_col() {
        assert_eq!(find_col("hello", 0), 1);
    }

    #[test]
    fn find_col_after_newline() {
        assert_eq!(find_col("abc\ndef", 4), 1);
    }

    #[test]
    fn find_cypher_operator_no_match() {
        assert!(find_cypher_operator("MATCH (N) RETURN N", "STARTS WITH").is_none());
    }

    // ── R5: Tab-blindness in find_cypher_operator ─────────────────────────────

    #[test]
    fn find_cypher_operator_detects_tab_separated_words() {
        let input = "N.NAME STARTS\tWITH 'AL'";
        assert!(find_cypher_operator(&input.to_ascii_uppercase(), "STARTS WITH").is_some());
    }

    #[test]
    fn find_cypher_operator_detects_multiple_spaces() {
        let input = "N.NAME STARTS   WITH 'AL'";
        assert!(find_cypher_operator(&input.to_ascii_uppercase(), "STARTS WITH").is_some());
    }
}
