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
//! | `` `backtick identifiers` ``      | Rewritten  | Converted to GQL delimited identifiers (`"..."`) |
//! | `STARTS WITH`, `ENDS WITH`        | Passed     | Parsed by core under `enterprise-helpers`     |
//! | `CONTAINS`                        | Passed     | Parsed by core under `enterprise-helpers`     |
//! | `IN [list]`                       | Passed     | Parsed by core under `enterprise-helpers`     |
//! | `id(n)`, `type(r)`, `labels(n)`   | Passed     | Parsed by core under `enterprise-helpers`     |
//! | `REMOVE n.prop`                   | Rewritten  | Rewritten to `SET n.prop = null`              |
//! | `OPTIONAL MATCH`                  | Partial    | Rewritten to `MATCH` (no null-fill semantics) |
//! | `WITH *`                          | Rewritten  | Stripped (pass-through only)                  |
//! | `WITH expr AS alias`              | Deferred   | Requires enterprise multi-stage executor      |
//! | `UNWIND list AS var`              | Deferred   | Requires enterprise multi-stage executor      |

use tessera_graph::Error;

/// Pre-processes a Cypher-compatible query string into GQL-compatible form.
///
/// Transformations applied in order:
/// 1. `/* block comments */` → stripped (replaced with spaces to preserve positions)
/// 2. `` `backtick identifiers` `` → converted to GQL delimited identifiers (`"..."`)
/// 3. `OPTIONAL MATCH` → rewritten to `MATCH` (partial compat — no null-fill)
/// 4. `REMOVE n.prop` → rewritten to `SET n.prop = null`
/// 5. `WITH *` → stripped (pass-through only)
/// 6. Unsupported clauses (`WITH expr AS alias`, `UNWIND`) → informative error
///
/// Cypher operators (`STARTS WITH`, `ENDS WITH`, `CONTAINS`, `IN`, scalar
/// functions `id`, `type`, `labels`) are passed through unchanged and handled
/// by the core parser when compiled with the `enterprise-helpers` feature.
///
/// # Errors
///
/// Returns `GqlSyntaxError` for unclosed block comments, backtick sequences,
/// or unsupported Cypher clauses (`WITH` with projection, `UNWIND`).
pub fn cypher_to_gql(input: &str) -> tessera_graph::Result<String> {
    let after_comments = strip_block_comments(input)?;
    let after_backticks = convert_backtick_idents(&after_comments)?;
    let after_optional = rewrite_optional_match(&after_backticks);
    let after_remove = rewrite_remove_clauses(&after_optional)?;
    let after_with_star = rewrite_with_star(&after_remove);
    detect_unsupported_clauses(&after_with_star)?;
    Ok(after_with_star)
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
#[allow(clippy::too_many_lines)] // Intentional: one block per rejected construct for readability.
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

    // OPTIONAL MATCH
    if let Some(pos) = find_cypher_operator(&upper, "OPTIONAL MATCH") {
        return Err(Error::GqlSyntaxError {
            line: find_line(input, pos),
            col: find_col(input, pos),
            message: "OPTIONAL MATCH is a Cypher clause not valid in strict GQL mode; \
                      switch to cypher-compat mode"
                .into(),
        });
    }

    // WITH keyword — but NOT part of "STARTS WITH" or "ENDS WITH" operators.
    if let Some(pos) = find_word(&upper, "WITH") {
        if is_standalone_with(&upper, pos) {
            return Err(Error::GqlSyntaxError {
                line: find_line(input, pos),
                col: find_col(input, pos),
                message: "WITH is a Cypher clause not valid in strict GQL mode; \
                          switch to cypher-compat mode"
                    .into(),
            });
        }
    }

    // UNWIND keyword
    if let Some(pos) = find_word(&upper, "UNWIND") {
        return Err(Error::GqlSyntaxError {
            line: find_line(input, pos),
            col: find_col(input, pos),
            message: "UNWIND is a Cypher keyword not valid in strict GQL mode; \
                      switch to cypher-compat mode"
                .into(),
        });
    }

    Ok(())
}

// ── String-literal scanning ──────────────────────────────────────────────────

/// Scan past a string literal starting at `chars[i]` (the opening quote).
///
/// Two modes controlled by `blank`:
/// - `blank = false`: copies content verbatim (for `strip_block_comments`, `convert_backtick_idents`)
/// - `blank = true`: replaces content with spaces preserving byte length (for `blank_string_literals`)
///
/// Returns the index of the first character after the closing quote.
/// The opening and closing quotes are always pushed to `result`.
///
/// # Byte-length invariant
///
/// In `blank = true` mode, the output has the exact same byte length as the
/// input: each character (including multibyte UTF-8) is replaced by the same
/// number of space bytes (`c.len_utf8()` spaces). This guarantees that byte
/// offsets computed on the blanked string are valid indices into the original.
fn scan_string_literal(
    chars: &[char],
    start: usize,
    result: &mut String,
    blank: bool,
) -> usize {
    let quote = chars[start];
    result.push(quote);
    let mut i = start + 1;
    while i < chars.len() {
        let c = chars[i];
        if c == '\\' && i + 1 < chars.len() {
            if blank {
                for _ in 0..c.len_utf8() {
                    result.push(' ');
                }
                i += 1;
                for _ in 0..chars[i].len_utf8() {
                    result.push(' ');
                }
            } else {
                result.push(c);
                i += 1;
                result.push(chars[i]);
            }
            i += 1;
            continue;
        }
        if c == quote {
            // GQL doubled-quote escape: '' or "" inside a string.
            if i + 1 < chars.len() && chars[i + 1] == quote {
                if blank {
                    result.push(' ');
                    result.push(' ');
                } else {
                    result.push(c);
                    result.push(c);
                }
                i += 2;
                continue;
            }
            result.push(c);
            i += 1;
            break;
        }
        if blank {
            for _ in 0..c.len_utf8() {
                result.push(' ');
            }
        } else {
            result.push(c);
        }
        i += 1;
    }
    i
}

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
        if chars[i] == '\'' || chars[i] == '"' {
            i = scan_string_literal(&chars, i, &mut result, true);
        } else {
            result.push(chars[i]);
            i += 1;
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

/// Returns `true` if the `WITH` at `pos` is a standalone Cypher clause,
/// not part of `STARTS WITH` or `ENDS WITH` comparison operators.
///
/// If `pos == 0` (or preceded only by whitespace), there can be no preceding
/// operator word, so returns `true`.
fn is_standalone_with(upper: &str, pos: usize) -> bool {
    let before = upper[..pos].trim_end();
    !before.ends_with("STARTS") && !before.ends_with("ENDS")
}

/// Searches for a whole-word occurrence of `word` in an already upper-cased
/// input string.
///
/// Returns the byte offset of the first match, or `None`.
fn find_word(upper_input: &str, word: &str) -> Option<usize> {
    let mut search_from = 0;
    while let Some(idx) = upper_input[search_from..].find(word) {
        let abs = search_from + idx;
        let before_ok = abs == 0 || !upper_input.as_bytes()[abs - 1].is_ascii_alphanumeric();
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
        let after_ok = after_in >= bytes.len() || !bytes[after_in].is_ascii_alphanumeric();
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
            i = scan_string_literal(&chars, i, &mut result, false);
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

/// Converts backtick-quoted identifiers to GQL delimited identifiers.
///
/// Backtick-quoted identifiers (Cypher syntax) are converted to
/// double-quoted delimited identifiers (GQL ISO standard).
/// E.g. `` `Average Pyranometer` `` → `"Average Pyranometer"`.
/// Backtick characters inside string literals are passed through unchanged.
fn convert_backtick_idents(input: &str) -> tessera_graph::Result<String> {
    let mut result = String::with_capacity(input.len());
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        // Skip string literals — backticks inside strings are not identifiers.
        if chars[i] == '\'' || chars[i] == '"' {
            i = scan_string_literal(&chars, i, &mut result, false);
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
                ident.push(chars[i]);
                i += 1;
            }
            let byte_offset: usize = chars[..start].iter().map(|c| c.len_utf8()).sum();
            if !found_close {
                return Err(Error::GqlSyntaxError {
                    line: find_line(input, byte_offset),
                    col: find_col(input, byte_offset),
                    message: "unclosed backtick identifier".into(),
                });
            }
            if ident.is_empty() {
                return Err(Error::GqlSyntaxError {
                    line: find_line(input, byte_offset),
                    col: find_col(input, byte_offset),
                    message: "empty backtick identifier".into(),
                });
            }
            result.push('"');
            for ch in ident.chars() {
                if ch == '"' {
                    result.push('"'); // GQL doubled-quote escape
                }
                result.push(ch);
            }
            result.push('"');
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

// ── REMOVE → SET null rewrite ────────────────────────────────────────────────

/// Rewrites `REMOVE var.prop1, var.prop2` to `SET var.prop1 = null, var.prop2 = null`.
///
/// Uses string-literal masking to avoid transforming REMOVE inside string constants.
fn rewrite_remove_clauses(input: &str) -> tessera_graph::Result<String> {
    let masked = blank_string_literals(input);
    let upper = masked.to_ascii_uppercase();

    let mut result = input.to_owned();
    // Process from end to start so byte offsets remain valid after replacements.
    let mut search_from = upper.len();
    while search_from > 0 {
        // Find last occurrence of REMOVE as a whole word.
        let prefix = &upper[..search_from];
        let Some(idx) = find_word(prefix, "REMOVE") else {
            break;
        };

        let remove_end = idx + "REMOVE".len();
        // Collect the property list after REMOVE: "n.x, n.y" up to next keyword or end.
        let rest = &input[remove_end..];
        let rest_upper = &upper[remove_end..];

        // Find where the property list ends — at the next GQL keyword, semicolon, or end.
        // `rest_upper` is a slice of `upper` (already uppercased by blank_string_literals + to_ascii_uppercase).
        let keywords = [
            "RETURN", "MATCH", "CREATE", "SET", "DELETE", "DETACH", "MERGE", "WHERE", "ORDER",
            "LIMIT", "WITH", "SKIP", "UNWIND",
        ];
        let mut list_end = rest.len();
        // Semicolon is also a terminator.
        if let Some(semi) = rest.find(';') {
            list_end = list_end.min(semi);
        }
        for kw in &keywords {
            if let Some(pos) = find_word(rest_upper, kw) {
                if pos < list_end {
                    list_end = pos;
                }
            }
        }

        // Use the original input (not masked) to extract the prop list.
        // Safe: byte offsets from `upper`/`masked` are valid into `input` because
        // `blank_string_literals` preserves byte lengths for every character.
        let prop_list_str = rest[..list_end].trim();
        if prop_list_str.is_empty() {
            return Err(Error::GqlSyntaxError {
                line: find_line(input, idx),
                col: find_col(input, idx),
                message: "REMOVE clause has no property references; \
                          expected 'REMOVE n.prop' or 'REMOVE n.prop1, n.prop2'"
                    .into(),
            });
        }

        // Split on commas and rewrite each "var.prop" to "var.prop = null".
        let props: Vec<&str> = prop_list_str.split(',').map(str::trim).collect();
        let set_parts: Vec<String> = props.iter().map(|p| format!("{p} = null")).collect();
        let replacement = format!("SET {}", set_parts.join(", "));

        // Replace in the original string.
        result = format!(
            "{}{}{}",
            &result[..idx],
            replacement,
            &result[remove_end + list_end..]
        );

        search_from = idx;
    }

    Ok(result)
}

// ── OPTIONAL MATCH → MATCH rewrite ──────────────────────────────────────────

/// Rewrites `OPTIONAL MATCH` to `MATCH` (partial Cypher compat — no null-fill semantics).
///
/// Uses string-literal masking to avoid transforming inside string constants.
///
/// # Invariant
///
/// `result` and `result_upper` are byte-isomorphic: `blank_string_literals`
/// replaces each character with the same number of space bytes, so byte offsets
/// computed on `result_upper` are valid indices into `result`.
fn rewrite_optional_match(input: &str) -> String {
    let masked = blank_string_literals(input);
    let upper = masked.to_ascii_uppercase();

    let mut result = input.to_owned();
    let mut result_upper = upper;
    // Process iteratively: each replacement shortens the string, so re-search from the start.
    while let Some(pos) = find_cypher_operator(&result_upper, "OPTIONAL MATCH") {
        // Find where MATCH starts (skip "OPTIONAL" + whitespace).
        let after_optional = pos + "OPTIONAL".len();
        let match_start = result_upper[after_optional..]
            .find("MATCH")
            .map_or(after_optional, |off| after_optional + off);

        // Remove "OPTIONAL " (everything from pos to match_start).
        result = format!("{}{}", &result[..pos], &result[match_start..]);
        result_upper = format!("{}{}", &result_upper[..pos], &result_upper[match_start..]);
    }

    result
}

// ── WITH * strip ────────────────────────────────────────────────────────────

/// Strips all `WITH *` (pass-through) occurrences from the query. `WITH *`
/// projects all variables without transformation, so removing it does not
/// change semantics.
///
/// Non-star `WITH` (projection) is left intact for `detect_unsupported_clauses`.
///
/// # Invariant
///
/// `result` and `result_upper` are byte-isomorphic: `blank_string_literals`
/// replaces each character with the same number of space bytes, so byte offsets
/// computed on `result_upper` are valid indices into `result`.
fn rewrite_with_star(input: &str) -> String {
    let masked = blank_string_literals(input);
    let upper = masked.to_ascii_uppercase();

    let mut result = input.to_owned();
    let mut result_upper = upper;
    // Safe: WITH and * are always single-byte ASCII; spaces preserve byte offsets.
    let mut search_from = 0;

    while search_from < result_upper.len() {
        let Some(pos) = find_word(&result_upper[search_from..], "WITH") else {
            break;
        };
        let abs_pos = search_from + pos;
        let after_with = abs_pos + "WITH".len();

        // Skip "STARTS WITH" and "ENDS WITH" — these are comparison operators, not clauses.
        if !is_standalone_with(&result_upper, abs_pos) {
            search_from = after_with;
            continue;
        }

        // Skip whitespace after WITH.
        let rest = &result_upper[after_with..];
        let trimmed_offset = rest.len() - rest.trim_start().len();
        let star_pos = after_with + trimmed_offset;

        if star_pos < result_upper.len() && result_upper.as_bytes()[star_pos] == b'*' {
            let after_star = star_pos + 1;
            let is_bare_star = after_star >= result_upper.len()
                || !result_upper.as_bytes()[after_star].is_ascii_alphanumeric();

            if is_bare_star {
                let removed_len = after_star - abs_pos;
                let spaces = " ".repeat(removed_len);
                result = format!("{}{}{}", &result[..abs_pos], spaces, &result[after_star..]);
                result_upper = format!(
                    "{}{}{}",
                    &result_upper[..abs_pos],
                    spaces,
                    &result_upper[after_star..]
                );
                // Continue searching from the same position (spaces replaced WITH *).
                search_from = abs_pos + removed_len;
                continue;
            }
        }
        // Not "WITH *" — skip past this WITH and keep searching for more.
        search_from = after_with;
    }

    result
}

// ── Unsupported clause detection ────────────────────────────────────────────

/// Detects Cypher clauses not yet supported in `CypherCompat` mode and returns
/// an informative error.
///
/// Called AFTER rewrite passes, so `WITH *` and `OPTIONAL MATCH` are already gone.
/// This catches remaining `WITH expr AS alias` and `UNWIND`.
fn detect_unsupported_clauses(input: &str) -> tessera_graph::Result<()> {
    let masked = blank_string_literals(input);
    let upper = masked.to_ascii_uppercase();

    if let Some(pos) = find_word(&upper, "UNWIND") {
        return Err(Error::GqlSyntaxError {
            line: find_line(input, pos),
            col: find_col(input, pos),
            message: "UNWIND is not yet supported in CypherCompat mode; \
                      requires the enterprise multi-stage executor (planned for Phase 1.5.6)"
                .into(),
        });
    }

    if let Some(pos) = find_word(&upper, "WITH") {
        if is_standalone_with(&upper, pos) {
            return Err(Error::GqlSyntaxError {
                line: find_line(input, pos),
                col: find_col(input, pos),
                message: "WITH (with projection) is not yet supported in CypherCompat mode; \
                          only WITH * (pass-through) is supported. \
                          Full WITH requires the enterprise multi-stage executor (planned for Phase 1.5.6)"
                    .into(),
            });
        }
    }

    Ok(())
}

// ── GQL-native fast-path detection ───────────────────────────────────────────

/// Returns `true` if `input` contains any Cypher-specific construct that
/// requires preprocessing before parsing as GQL.
///
/// Conservative: returns `true` on doubt. A `false` return guarantees
/// the input is pure GQL and can bypass the preprocessor entirely.
///
/// Detected constructs:
/// - Block comments (`/* ... */`)
/// - Backtick identifiers (`` `ident` ``)
/// - `OPTIONAL MATCH`
/// - `REMOVE` clause
/// - `WITH *` pass-through
#[must_use]
pub fn contains_cypher_constructs(input: &str) -> bool {
    let masked = blank_string_literals(input);
    let upper = masked.to_ascii_uppercase();

    // Fast byte-level checks first (cheapest).
    if masked.contains('`') || masked.contains("/*") {
        return true;
    }

    // Word-boundary keyword checks.
    if find_word(&upper, "OPTIONAL").is_some() || find_word(&upper, "REMOVE").is_some() {
        return true;
    }

    contains_with_star(&upper)
}

/// Checks for `WITH *` (not `STARTS WITH` / `ENDS WITH`).
fn contains_with_star(upper: &str) -> bool {
    let mut search_from = 0;
    while let Some(idx) = upper[search_from..].find("WITH") {
        let abs = search_from + idx;
        // Ensure word boundary before WITH.
        if abs > 0 && upper.as_bytes()[abs - 1].is_ascii_alphanumeric() {
            search_from = abs + 4;
            continue;
        }
        // Ensure word boundary after WITH.
        let after = abs + 4;
        if after < upper.len() && upper.as_bytes()[after].is_ascii_alphanumeric() {
            search_from = abs + 4;
            continue;
        }
        // Skip if it's part of STARTS WITH or ENDS WITH.
        if !is_standalone_with(upper, abs) {
            search_from = abs + 4;
            continue;
        }
        // Check for * after optional whitespace.
        let rest = upper[after..].trim_start();
        if rest.starts_with('*') {
            return true;
        }
        search_from = abs + 4;
    }
    false
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

    // ── REMOVE rewrite ──────────────────────────────────────────────────────

    #[test]
    fn remove_simple_property_rewritten_to_set_null() {
        let input = "MATCH (n:Person) WHERE n.name = 'Alice' REMOVE n.age";
        let output = cypher_to_gql(input).expect("parse"); // OK: test
        assert!(output.contains("SET n.age = null"), "got: {output}");
        assert!(!output.contains("REMOVE"), "got: {output}");
    }

    #[test]
    fn remove_multiple_properties_rewritten() {
        let input = "MATCH (n) REMOVE n.x, n.y";
        let output = cypher_to_gql(input).expect("parse"); // OK: test
        assert!(output.contains("SET n.x = null"), "got: {output}");
        assert!(output.contains("n.y = null"), "got: {output}");
    }

    #[test]
    fn remove_inside_string_not_transformed() {
        let input = "MATCH (n) WHERE n.desc = 'REMOVE not a keyword' RETURN n";
        let output = cypher_to_gql(input).expect("parse"); // OK: test
        assert!(output.contains("'REMOVE not a keyword'"), "got: {output}");
    }

    #[test]
    fn remove_clause_parses_as_set_mutation_in_cypher_compat_mode() {
        use crate::parse_with_mode;
        use tessera_config::QueryLanguage;

        let cypher = "MATCH (n:Person) WHERE n.name = 'Alice' REMOVE n.age";
        let stmt = parse_with_mode(cypher, QueryLanguage::CypherCompat).expect("parse"); // OK: test
        assert!(
            matches!(stmt, tessera_graph::GqlStatement::Mutation(_)),
            "expected Mutation, got: {stmt:?}"
        );
    }

    // ── OPTIONAL MATCH rewrite ──────────────────────────────────────────────

    #[test]
    fn optional_match_rewritten_to_match() {
        let input = "OPTIONAL MATCH (n:Person) RETURN n.name";
        let output = cypher_to_gql(input).expect("parse"); // OK: test
        let upper = output.to_ascii_uppercase();
        assert!(!upper.contains("OPTIONAL"), "OPTIONAL should be removed, got: {output}");
        assert!(upper.contains("MATCH"), "MATCH must remain, got: {output}");
    }

    #[test]
    fn optional_match_in_cypher_compat_parses_as_query() {
        use crate::parse_with_mode;
        use tessera_config::QueryLanguage;

        let input = "OPTIONAL MATCH (n:Person) RETURN n.name";
        let stmt = parse_with_mode(input, QueryLanguage::CypherCompat).expect("parse"); // OK: test
        assert!(matches!(stmt, tessera_graph::GqlStatement::Query(_)));
    }

    #[test]
    fn optional_inside_string_not_transformed() {
        let input = "MATCH (n) WHERE n.q = 'OPTIONAL MATCH syntax' RETURN n";
        let output = cypher_to_gql(input).expect("parse"); // OK: test
        assert!(output.contains("'OPTIONAL MATCH syntax'"), "got: {output}");
    }

    #[test]
    fn optional_match_rejected_in_strict_gql_mode() {
        use crate::parse_with_mode;
        use tessera_config::QueryLanguage;

        let input = "OPTIONAL MATCH (n:Person) RETURN n.name";
        let err = parse_with_mode(input, QueryLanguage::StrictGql).expect_err("should reject"); // OK: test
        let msg = format!("{err:?}");
        assert!(msg.contains("OPTIONAL"), "got: {msg}");
    }

    // ── WITH handling ───────────────────────────────────────────────────────

    #[test]
    fn with_star_passthrough_is_stripped() {
        let input = "MATCH (n:Person) WITH * RETURN n.name";
        let output = cypher_to_gql(input).expect("parse"); // OK: test
        assert!(!output.to_ascii_uppercase().contains("WITH"), "got: {output}");
        assert!(output.contains("MATCH"), "got: {output}");
        assert!(output.contains("RETURN"), "got: {output}");
    }

    #[test]
    fn with_projection_returns_informative_error() {
        let input = "MATCH (n) WITH n.name AS name RETURN name";
        let err = cypher_to_gql(input).expect_err("should reject"); // OK: test
        let msg = format!("{err:?}");
        assert!(msg.contains("WITH") && msg.contains("not yet supported"), "got: {msg}");
    }

    #[test]
    fn with_inside_string_not_flagged() {
        let input = "MATCH (n) WHERE n.query = 'use WITH for chaining' RETURN n";
        assert!(cypher_to_gql(input).is_ok());
    }

    #[test]
    fn with_rejected_in_strict_gql_mode() {
        use crate::parse_with_mode;
        use tessera_config::QueryLanguage;

        let input = "MATCH (n) WITH n.name AS name RETURN name";
        let err = parse_with_mode(input, QueryLanguage::StrictGql).expect_err("should reject"); // OK: test
        let msg = format!("{err:?}");
        assert!(msg.contains("WITH"), "got: {msg}");
    }

    // ── UNWIND handling ─────────────────────────────────────────────────────

    #[test]
    fn unwind_in_cypher_compat_returns_informative_error() {
        let input = "UNWIND [1, 2, 3] AS x RETURN x";
        let err = cypher_to_gql(input).expect_err("should reject"); // OK: test
        let msg = format!("{err:?}");
        assert!(msg.contains("UNWIND") && msg.contains("not yet supported"), "got: {msg}");
    }

    #[test]
    fn unwind_inside_string_literal_is_not_flagged() {
        let input = "MATCH (n) WHERE n.desc = 'UNWIND is a keyword' RETURN n";
        assert!(cypher_to_gql(input).is_ok());
    }

    #[test]
    fn unwind_rejected_in_strict_gql_mode() {
        use crate::parse_with_mode;
        use tessera_config::QueryLanguage;

        let input = "UNWIND [1,2,3] AS x RETURN x";
        let err = parse_with_mode(input, QueryLanguage::StrictGql).expect_err("should reject"); // OK: test
        let msg = format!("{err:?}");
        assert!(msg.contains("UNWIND"), "got: {msg}");
    }

    // ── Edge case tests (quality review) ────────────────────────────────────

    #[test]
    fn remove_with_trailing_semicolon() {
        let input = "MATCH (n:Person) WHERE n.name = 'Alice' REMOVE n.age;";
        let output = cypher_to_gql(input).expect("parse"); // OK: test
        assert!(output.contains("SET n.age = null"), "got: {output}");
        assert!(output.contains(';'), "semicolon should be preserved, got: {output}");
    }

    #[test]
    fn remove_followed_by_with_star() {
        let input = "MATCH (n) REMOVE n.age WITH * RETURN n";
        let output = cypher_to_gql(input).expect("parse"); // OK: test
        assert!(output.contains("SET n.age = null"), "got: {output}");
        assert!(!output.contains("REMOVE"), "got: {output}");
    }

    #[test]
    fn multiple_with_star_in_same_query() {
        let input = "MATCH (n) WITH * MATCH (m) WITH * RETURN n, m";
        let output = cypher_to_gql(input).expect("parse"); // OK: test
        assert!(
            !output.to_ascii_uppercase().contains("WITH"),
            "all WITH * should be stripped, got: {output}"
        );
    }

    #[test]
    fn with_star_after_with_projection_both_handled() {
        // WITH projection appears first, WITH * second — both must be detected.
        let input = "MATCH (n) WITH n.name AS x MATCH (m) WITH * RETURN m";
        let err = cypher_to_gql(input).expect_err("should reject WITH projection"); // OK: test
        let msg = format!("{err:?}");
        assert!(msg.contains("WITH"), "got: {msg}");
    }

    #[test]
    fn remove_empty_prop_list_returns_error() {
        let input = "MATCH (n) REMOVE RETURN n";
        let err = cypher_to_gql(input).expect_err("should fail"); // OK: test
        let msg = format!("{err:?}");
        assert!(msg.contains("no property references"), "got: {msg}");
    }

    #[test]
    fn optional_match_with_tab_between_words() {
        let input = "OPTIONAL\tMATCH (n:Person) RETURN n.name";
        let output = cypher_to_gql(input).expect("parse"); // OK: test
        let upper = output.to_ascii_uppercase();
        assert!(!upper.contains("OPTIONAL"), "got: {output}");
        assert!(upper.contains("MATCH"), "got: {output}");
    }

    // ── R3: Multibyte byte-length invariant ─────────────────────────────────

    #[test]
    fn blank_string_literals_preserves_byte_length_with_multibyte() {
        let input = "MATCH (n) WHERE n.name = 'caf\u{00E9}' RETURN n";
        let blanked = blank_string_literals(input);
        assert_eq!(
            blanked.len(),
            input.len(),
            "byte length must be preserved: input={}, blanked={}",
            input.len(),
            blanked.len()
        );
        // é is 2 bytes in UTF-8 → 2 spaces replace it.
        // 'café' body is c(1) + a(1) + f(1) + é(2) = 5 bytes → 5 spaces.
        let quote_start = blanked.find('\'').expect("opening quote"); // OK: test
        let quote_end = blanked[quote_start + 1..].find('\'').expect("closing quote"); // OK: test
        let inner = &blanked[quote_start + 1..quote_start + 1 + quote_end];
        assert!(
            inner.chars().all(|c| c == ' '),
            "literal content must be all spaces, got: {inner:?}"
        );
        assert_eq!(inner.len(), 5, "café body = 5 bytes → 5 spaces");
    }

    // ── R4: is_standalone_with edge cases ───────────────────────────────────

    #[test]
    fn is_standalone_with_at_start_of_string() {
        // WITH at pos=0 — no preceding operator possible.
        assert!(is_standalone_with("WITH n AS x", 0));
    }

    #[test]
    fn is_standalone_with_preceded_by_spaces_only() {
        // WITH preceded by whitespace — still standalone.
        assert!(is_standalone_with("   WITH n AS x", 3));
    }

    #[test]
    fn is_standalone_with_after_starts_is_not_standalone() {
        assert!(!is_standalone_with("N.NAME STARTS WITH 'A'", 14));
    }

    #[test]
    fn is_standalone_with_after_ends_is_not_standalone() {
        assert!(!is_standalone_with("N.NAME ENDS WITH 'Z'", 12));
    }

    // ── backtick → delimited identifier conversion ──────────────────────

    #[test]
    fn backtick_identifier_converted_to_delimited() {
        let input = "MATCH (n:`My Label`) RETURN n";
        let output = cypher_to_gql(input).unwrap(); // OK: test
        assert!(output.contains("\"My Label\""), "got: {output}");
        assert!(!output.contains('`'), "backticks should be removed, got: {output}");
    }

    #[test]
    fn backtick_property_key_converted_to_delimited() {
        let input = "MATCH (n) WHERE n.`Average Pyranometer` = 'x' RETURN n";
        let output = cypher_to_gql(input).unwrap(); // OK: test
        assert!(output.contains("\"Average Pyranometer\""), "got: {output}");
    }

    #[test]
    fn backtick_identifier_with_embedded_double_quote_is_escaped() {
        let input = r#"MATCH (n:`col"name`) RETURN n"#;
        let output = cypher_to_gql(input).unwrap(); // OK: test
        assert!(output.contains(r#""col""name""#), "got: {output}");
    }

    #[test]
    fn empty_backtick_identifier_is_error() {
        let result = cypher_to_gql("MATCH (n:``) RETURN n");
        assert!(result.is_err());
    }

    #[test]
    fn backtick_inside_string_literal_not_converted() {
        let input = "MATCH (n) WHERE n.name = 'use `backticks` here' RETURN n";
        let output = cypher_to_gql(input).unwrap(); // OK: test
        assert!(output.contains("`backticks`"), "got: {output}");
    }

    // ── contains_cypher_constructs ──────────────────────────────────────────

    #[test]
    fn detects_backtick_identifier() {
        assert!(contains_cypher_constructs("MATCH (`n`) RETURN n"));
    }

    #[test]
    fn detects_block_comment() {
        assert!(contains_cypher_constructs("/* comment */ RETURN 1"));
    }

    #[test]
    fn detects_optional_match() {
        assert!(contains_cypher_constructs("OPTIONAL MATCH (n) RETURN n"));
    }

    #[test]
    fn detects_remove_clause() {
        assert!(contains_cypher_constructs("MATCH (n) REMOVE n.prop RETURN n"));
    }

    #[test]
    fn detects_with_star() {
        assert!(contains_cypher_constructs("MATCH (n) WITH * RETURN n"));
    }

    #[test]
    fn pure_gql_match_return_is_not_cypher() {
        assert!(!contains_cypher_constructs("MATCH (n) RETURN n"));
    }

    #[test]
    fn pure_gql_create_is_not_cypher() {
        assert!(!contains_cypher_constructs("CREATE (n {name: 'Alice'}) RETURN n"));
    }

    #[test]
    fn with_star_in_string_literal_no_false_positive() {
        assert!(!contains_cypher_constructs("RETURN 'WITH *' AS x"));
    }

    #[test]
    fn backtick_in_string_literal_no_false_positive() {
        assert!(!contains_cypher_constructs("RETURN 'use `backtick` here'"));
    }

    #[test]
    fn starts_with_operator_no_false_positive() {
        assert!(!contains_cypher_constructs(
            "MATCH (n) WHERE n.name STARTS WITH 'A' RETURN n"
        ));
    }

    #[test]
    fn ends_with_operator_no_false_positive() {
        assert!(!contains_cypher_constructs(
            "MATCH (n) WHERE n.name ENDS WITH 'z' RETURN n"
        ));
    }
}
