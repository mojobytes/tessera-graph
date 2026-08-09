// SPDX-License-Identifier: MIT

//! GQL lexer (tokenizer).
//!
//! Converts a GQL source string into a flat sequence of [`SpannedToken`]s.
//! The lexer is byte-oriented for performance; all GQL keywords and operators
//! are ASCII, and string literal contents are stored as owned `String` values
//! (UTF-8 validated by Rust's `str` type).
//!
//! # Stability
//!
//! **This module is an internal implementation detail.** It is exposed as
//! `#[doc(hidden)] pub mod` so integration tests and diagnostic tooling
//! can drive the tokenizer directly, but it is NOT part of the crate's
//! semver-stable public API. [`Lexer`], its methods, and the full token
//! stream shape may change without notice. External crates should
//! depend only on [`crate::gql::parse`] / [`crate::gql::parse_statement`]
//! for stable behaviour.

use crate::Error;
use crate::gql::token::{Span, SpannedToken, Token};

/// Stateful GQL lexer.
///
/// Create with [`Lexer::new`] and call [`Lexer::tokenize`] to obtain the full
/// token stream.
pub struct Lexer<'a> {
    /// The original input as a UTF-8 string slice (avoids re-validation).
    str_input: &'a str,
    /// Byte view of the input for efficient single-byte operations.
    input: &'a [u8],
    pos: usize,
    line: u32,
    col: u32,
}

impl<'a> Lexer<'a> {
    /// Creates a new `Lexer` positioned at the start of `input`.
    #[must_use]
    pub const fn new(input: &'a str) -> Self {
        Self {
            str_input: input,
            input: input.as_bytes(),
            pos: 0,
            line: 1,
            col: 1,
        }
    }

    /// Tokenizes the entire input and returns all [`SpannedToken`]s.
    ///
    /// The last element of the returned `Vec` is always [`Token::Eof`].
    ///
    /// # Errors
    ///
    /// Returns [`Error::GqlSyntaxError`] on encountering an unrecognised
    /// character.
    pub fn tokenize(mut self) -> crate::Result<Vec<SpannedToken>> {
        // GQL tokens average ~5 bytes (keywords like MATCH, RETURN are 5-6 chars).
        // +8 ensures a reasonable minimum for short queries.
        let mut tokens = Vec::with_capacity(self.input.len() / 5 + 8);

        loop {
            self.skip_whitespace();

            let start_pos = self.pos;
            let start_line = self.line;
            let start_col = self.col;

            let Some(b) = self.peek() else {
                tokens.push(SpannedToken {
                    token: Token::Eof,
                    span: self.make_span(start_pos, start_line, start_col),
                });
                break;
            };

            let token = match b {
                b'(' => {
                    self.advance();
                    Token::LParen
                }
                b')' => {
                    self.advance();
                    Token::RParen
                }
                b'[' => {
                    self.advance();
                    Token::LBracket
                }
                b']' => {
                    self.advance();
                    Token::RBracket
                }
                b'{' => {
                    self.advance();
                    Token::LBrace
                }
                b'}' => {
                    self.advance();
                    Token::RBrace
                }
                b':' => {
                    self.advance();
                    Token::Colon
                }
                b',' => {
                    self.advance();
                    Token::Comma
                }
                b'*' => {
                    self.advance();
                    Token::Star
                }
                b'|' => {
                    self.advance();
                    Token::Pipe
                }
                b'$' => {
                    // Emit Dollar unconditionally. The parser composes
                    // `Dollar + Ident` (named parameter, e.g. `$id`) or
                    // `Dollar + IntLit` (positional parameter, e.g. `$1`)
                    // and reports the syntax error if neither follows.
                    // Keeping validation in the parser preserves the
                    // table-driven lexer with no lookahead.
                    self.advance();
                    Token::Dollar
                }
                b'+' => {
                    self.advance();
                    if self.peek() == Some(b'=') {
                        self.advance();
                        Token::PlusEq
                    } else {
                        Token::Plus
                    }
                }
                b'/' => {
                    self.advance();
                    Token::Slash
                }
                b'=' => {
                    self.advance();
                    Token::Eq
                }
                b'.' => self.lex_dot(),
                b'-' => self.lex_minus(),
                b'<' => self.lex_less_than(),
                b'>' => self.lex_greater_than(),
                b'\'' | b'"' | b'`' => self.lex_string()?,
                b'0'..=b'9' => self.lex_number()?,
                b'a'..=b'z' | b'A'..=b'Z' | b'_' => self.lex_ident_or_keyword(),
                unknown => {
                    return Err(Error::GqlSyntaxError {
                        line: self.line,
                        col: self.col,
                        message: format!("unexpected character '{}'", char::from(unknown)),
                    });
                }
            };

            tokens.push(SpannedToken {
                token,
                span: self.make_span(start_pos, start_line, start_col),
            });
        }

        Ok(tokens)
    }

    // ── Private helpers ──────────────────────────────────────────────────────

    /// Skips ASCII whitespace and `//`-style line comments.
    fn skip_whitespace(&mut self) {
        loop {
            match self.peek() {
                Some(b' ' | b'\t' | b'\r' | b'\n') => {
                    self.advance();
                }
                Some(b'/') if self.peek_next() == Some(b'/') => {
                    // Consume the rest of the line.
                    while self.peek().is_some_and(|c| c != b'\n') {
                        self.advance();
                    }
                }
                _ => break,
            }
        }
    }

    /// Returns the current byte without consuming it.
    fn peek(&self) -> Option<u8> {
        self.input.get(self.pos).copied()
    }

    /// Returns the byte one position ahead without consuming anything.
    fn peek_next(&self) -> Option<u8> {
        self.input.get(self.pos + 1).copied()
    }

    /// Consumes the current byte, advances `pos`/`line`/`col`, and returns
    /// the consumed byte.
    ///
    /// # Panics
    ///
    /// Panics if called past the end of input. Callers must always check
    /// [`peek()`](Self::peek) before calling this method.
    fn advance(&mut self) -> u8 {
        let b = *self
            .input
            .get(self.pos)
            .expect("advance() called past end of input");
        self.pos += 1;
        if b == b'\n' {
            self.line += 1;
            self.col = 1;
        } else {
            self.col += 1;
        }
        b
    }

    /// Consumes a full UTF-8 character from the input and pushes it onto
    /// `value`. Handles multi-byte sequences correctly. Uses the cached
    /// `str_input` slice to avoid re-validating UTF-8 on each character.
    fn advance_utf8_char(&mut self, value: &mut String) {
        let ch = self.str_input[self.pos..]
            .chars()
            .next()
            .expect("peeked byte guarantees at least one char");
        for _ in 0..ch.len_utf8() {
            self.advance();
        }
        value.push(ch);
    }

    /// Constructs a [`Span`] from a saved start position/line/col up to the
    /// current position.
    const fn make_span(&self, start: usize, start_line: u32, start_col: u32) -> Span {
        Span {
            start,
            end: self.pos,
            line: start_line,
            col: start_col,
        }
    }

    /// Maps an upper-cased identifier string to a keyword [`Token`], if any.
    fn keyword_from_str(s: &str) -> Option<Token> {
        match s {
            "MATCH" => Some(Token::Match),
            "RETURN" => Some(Token::Return),
            "WHERE" => Some(Token::Where),
            "ORDER" => Some(Token::Order),
            "BY" => Some(Token::By),
            "LIMIT" => Some(Token::Limit),
            "AND" => Some(Token::And),
            "OR" => Some(Token::Or),
            "NOT" => Some(Token::Not),
            "IS" => Some(Token::Is),
            "NULL" => Some(Token::Null),
            "ASC" => Some(Token::Asc),
            "DESC" => Some(Token::Desc),
            "AS" => Some(Token::As),
            "DISTINCT" => Some(Token::Distinct),
            "COUNT" => Some(Token::Count),
            "SUM" => Some(Token::Sum),
            "AVG" => Some(Token::Avg),
            "MIN" => Some(Token::Min),
            "MAX" => Some(Token::Max),
            "COLLECT" => Some(Token::Collect),
            "CREATE" => Some(Token::Create),
            "SET" => Some(Token::Set),
            "DELETE" => Some(Token::Delete),
            "DETACH" => Some(Token::Detach),
            "MERGE" => Some(Token::Merge),
            "UNWIND" => Some(Token::Unwind),
            "GROUP" => Some(Token::Group),
            "WITH" => Some(Token::With),
            "SKIP" => Some(Token::Skip),
            // TRUE and FALSE are mapped to BoolLit rather than dedicated keyword
            // tokens because they carry a boolean payload and are treated as literal
            // values by the parser, not as control-flow keywords like MATCH or RETURN.
            "TRUE" => Some(Token::BoolLit(true)),
            "FALSE" => Some(Token::BoolLit(false)),
            _ => None,
        }
    }

    /// Lexes `.` or `..`.
    fn lex_dot(&mut self) -> Token {
        self.advance(); // consume first '.'
        if self.peek() == Some(b'.') {
            self.advance(); // consume second '.'
            Token::DotDot
        } else {
            Token::Dot
        }
    }

    /// Lexes `-` or `->`.
    fn lex_minus(&mut self) -> Token {
        self.advance(); // consume '-'
        if self.peek() == Some(b'>') {
            self.advance(); // consume '>'
            Token::ArrowRight
        } else {
            Token::Minus
        }
    }

    /// Lexes `<`, `<=`, `<>`, or `<-`.
    fn lex_less_than(&mut self) -> Token {
        self.advance(); // consume '<'
        match self.peek() {
            Some(b'=') => {
                self.advance();
                Token::LtEq
            }
            Some(b'>') => {
                self.advance();
                Token::NotEq
            }
            Some(b'-') => {
                self.advance();
                Token::ArrowLeft
            }
            _ => Token::Lt,
        }
    }

    /// Lexes `>` or `>=`.
    fn lex_greater_than(&mut self) -> Token {
        self.advance(); // consume '>'
        if self.peek() == Some(b'=') {
            self.advance();
            Token::GtEq
        } else {
            Token::Gt
        }
    }

    /// Lexes a single-quoted string literal or a delimited identifier
    /// (double-quoted or backtick-quoted), per ISO GQL (ISO/IEC 39075)
    /// and Cypher compatibility.
    ///
    /// - Single quotes (`'`) produce [`Token::StringLit`].
    /// - Double quotes (`"`) produce [`Token::Ident`] (ISO GQL delimited identifier).
    /// - Backticks (`` ` ``) produce [`Token::Ident`] (Cypher-style escaped identifier).
    ///
    /// **Tessera extension**: All forms use backslash escape sequences
    /// (`\'`, `\"`, `` \` ``, `\\`, `\n`, `\t`, `\r`). ISO GQL §6.4.4 specifies
    /// doubled-quote (`""`) as the escape mechanism for delimited identifiers;
    /// Tessera accepts `\"` instead for consistency with single-quoted literals.
    ///
    /// Multi-byte UTF-8 characters are preserved correctly.
    fn lex_string(&mut self) -> crate::Result<Token> {
        let start_line = self.line;
        let start_col = self.col;
        let quote = self.advance(); // consume opening quote
        let is_delimited_ident = quote == b'"' || quote == b'`';
        let unterminated_msg = if is_delimited_ident {
            "unterminated delimited identifier"
        } else {
            "unterminated string literal"
        };
        // Most values in GQL queries are short (identifiers, labels).
        let mut value = String::with_capacity(16);

        loop {
            match self.peek() {
                None => {
                    return Err(Error::GqlSyntaxError {
                        line: start_line,
                        col: start_col,
                        message: unterminated_msg.into(),
                    });
                }
                Some(b'\\') => {
                    self.advance(); // consume backslash
                    match self.peek() {
                        Some(c) if c == quote || c == b'\\' => {
                            value.push(char::from(self.advance()));
                        }
                        Some(b'n') => {
                            self.advance();
                            value.push('\n');
                        }
                        Some(b't') => {
                            self.advance();
                            value.push('\t');
                        }
                        Some(b'r') => {
                            self.advance();
                            value.push('\r');
                        }
                        Some(_) => {
                            // Unknown escape: keep the backslash + char literally.
                            value.push('\\');
                            value.push(char::from(self.advance()));
                        }
                        None => {
                            return Err(Error::GqlSyntaxError {
                                line: start_line,
                                col: start_col,
                                message: "unterminated escape sequence".into(),
                            });
                        }
                    }
                }
                Some(c) if c == quote => {
                    self.advance(); // consume closing quote
                    break;
                }
                Some(_) => {
                    self.advance_utf8_char(&mut value);
                }
            }
        }

        if is_delimited_ident {
            if value.is_empty() {
                return Err(Error::GqlSyntaxError {
                    line: start_line,
                    col: start_col,
                    message: "empty delimited identifier".into(),
                });
            }
            Ok(Token::Ident(value))
        } else {
            Ok(Token::StringLit(value))
        }
    }

    /// Lexes an integer or floating-point number literal.
    ///
    /// Grammar:
    /// ```text
    /// number  = digits ('.' digits)? (('e' | 'E') digits)?
    /// digits  = [0-9]+
    /// ```
    fn lex_number(&mut self) -> crate::Result<Token> {
        let start = self.pos;
        let err_line = self.line;
        let err_col = self.col;

        while self.peek().is_some_and(|c| c.is_ascii_digit()) {
            self.advance();
        }

        // Check for fractional part: requires '.' followed by a digit
        // (to avoid treating `1..5` as a float).
        let is_float =
            self.peek() == Some(b'.') && self.peek_next().is_some_and(|c| c.is_ascii_digit());

        if is_float {
            self.advance(); // consume '.'
            while self.peek().is_some_and(|c| c.is_ascii_digit()) {
                self.advance();
            }
        }

        // Check for scientific notation.
        let has_exp = self.peek().is_some_and(|c| c == b'e' || c == b'E');
        if has_exp {
            self.advance(); // consume 'e'/'E'
            // Optional sign.
            if self.peek().is_some_and(|c| c == b'+' || c == b'-') {
                self.advance();
            }
            while self.peek().is_some_and(|c| c.is_ascii_digit()) {
                self.advance();
            }
        }

        let raw =
            std::str::from_utf8(&self.input[start..self.pos]).expect("digit bytes are valid UTF-8");

        if is_float || has_exp {
            let value: f64 = raw.parse().map_err(|_| Error::GqlSyntaxError {
                line: err_line,
                col: err_col,
                message: format!("invalid float literal '{raw}'"),
            })?;
            Ok(Token::FloatLit(value))
        } else {
            let value: i64 = raw.parse().map_err(|_| Error::GqlSyntaxError {
                line: err_line,
                col: err_col,
                message: format!("integer literal out of range '{raw}'"),
            })?;
            Ok(Token::IntLit(value))
        }
    }

    /// Lexes an identifier or keyword.
    ///
    /// Grammar: `[a-zA-Z_][a-zA-Z0-9_]*`
    fn lex_ident_or_keyword(&mut self) -> Token {
        let start = self.pos;

        while self
            .peek()
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == b'_')
        {
            self.advance();
        }

        let raw = std::str::from_utf8(&self.input[start..self.pos])
            .expect("alphanumeric bytes are valid UTF-8");
        let upper = raw.to_ascii_uppercase();

        Self::keyword_from_str(&upper).unwrap_or_else(|| Token::Ident(raw.to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gql::token::Token;

    fn tokens(input: &str) -> Vec<Token> {
        Lexer::new(input)
            .tokenize()
            .unwrap()
            .into_iter()
            .map(|s| s.token)
            .collect()
    }

    #[test]
    fn lex_empty_input() {
        assert_eq!(tokens(""), vec![Token::Eof]);
    }

    #[test]
    fn lex_keywords_case_insensitive() {
        let toks = tokens("MATCH RETURN WHERE ORDER BY LIMIT AND OR NOT AS");
        assert_eq!(toks[0], Token::Match);
        assert_eq!(toks[1], Token::Return);
        assert_eq!(toks[2], Token::Where);
        assert_eq!(toks[3], Token::Order);
        assert_eq!(toks[4], Token::By);
        assert_eq!(toks[5], Token::Limit);
        assert_eq!(toks[6], Token::And);
        assert_eq!(toks[7], Token::Or);
        assert_eq!(toks[8], Token::Not);
        assert_eq!(toks[9], Token::As);
    }

    #[test]
    fn lex_keywords_lowercase() {
        let toks = tokens("match return where");
        assert_eq!(toks[0], Token::Match);
        assert_eq!(toks[1], Token::Return);
        assert_eq!(toks[2], Token::Where);
    }

    #[test]
    fn lex_aggregation_keywords() {
        let toks = tokens("COUNT SUM AVG MIN MAX COLLECT");
        assert_eq!(toks[0], Token::Count);
        assert_eq!(toks[1], Token::Sum);
        assert_eq!(toks[2], Token::Avg);
        assert_eq!(toks[3], Token::Min);
        assert_eq!(toks[4], Token::Max);
        assert_eq!(toks[5], Token::Collect);
    }

    #[test]
    fn lex_mutation_keywords_case_insensitive() {
        // Uppercase
        let toks = tokens("CREATE SET DELETE DETACH MERGE");
        assert_eq!(toks[0], Token::Create);
        assert_eq!(toks[1], Token::Set);
        assert_eq!(toks[2], Token::Delete);
        assert_eq!(toks[3], Token::Detach);
        assert_eq!(toks[4], Token::Merge);

        // Lowercase
        let toks_lower = tokens("create set delete detach merge");
        assert_eq!(toks_lower[0], Token::Create);
        assert_eq!(toks_lower[1], Token::Set);
        assert_eq!(toks_lower[2], Token::Delete);
        assert_eq!(toks_lower[3], Token::Detach);
        assert_eq!(toks_lower[4], Token::Merge);
    }

    #[test]
    fn lex_null_is_asc_desc_distinct() {
        let toks = tokens("IS NOT NULL ASC DESC DISTINCT");
        assert_eq!(toks[0], Token::Is);
        assert_eq!(toks[1], Token::Not);
        assert_eq!(toks[2], Token::Null);
        assert_eq!(toks[3], Token::Asc);
        assert_eq!(toks[4], Token::Desc);
        assert_eq!(toks[5], Token::Distinct);
    }

    #[test]
    fn lex_punctuation() {
        let toks = tokens("( ) [ ] { } : , . * |");
        assert_eq!(toks[0], Token::LParen);
        assert_eq!(toks[1], Token::RParen);
        assert_eq!(toks[2], Token::LBracket);
        assert_eq!(toks[3], Token::RBracket);
        assert_eq!(toks[4], Token::LBrace);
        assert_eq!(toks[5], Token::RBrace);
        assert_eq!(toks[6], Token::Colon);
        assert_eq!(toks[7], Token::Comma);
        assert_eq!(toks[8], Token::Dot);
        assert_eq!(toks[9], Token::Star);
        assert_eq!(toks[10], Token::Pipe);
    }

    #[test]
    fn lex_comparison_operators() {
        let toks = tokens("= <> < > <= >=");
        assert_eq!(toks[0], Token::Eq);
        assert_eq!(toks[1], Token::NotEq);
        assert_eq!(toks[2], Token::Lt);
        assert_eq!(toks[3], Token::Gt);
        assert_eq!(toks[4], Token::LtEq);
        assert_eq!(toks[5], Token::GtEq);
    }

    #[test]
    fn lex_arithmetic_operators() {
        let toks = tokens("+ - * /");
        assert_eq!(toks[0], Token::Plus);
        assert_eq!(toks[1], Token::Minus);
        assert_eq!(toks[2], Token::Star);
        assert_eq!(toks[3], Token::Slash);
    }

    #[test]
    fn lex_arrows() {
        let toks = tokens("-> <-");
        assert_eq!(toks[0], Token::ArrowRight);
        assert_eq!(toks[1], Token::ArrowLeft);
    }

    #[test]
    fn lex_dash_standalone() {
        let toks = tokens("-");
        assert_eq!(toks[0], Token::Minus);
    }

    #[test]
    fn lex_dotdot_range() {
        let toks = tokens("1..5");
        assert_eq!(toks[0], Token::IntLit(1));
        assert_eq!(toks[1], Token::DotDot);
        assert_eq!(toks[2], Token::IntLit(5));
    }

    #[test]
    fn lex_identifier() {
        let toks = tokens("myVar _foo a123");
        assert_eq!(toks[0], Token::Ident("myVar".into()));
        assert_eq!(toks[1], Token::Ident("_foo".into()));
        assert_eq!(toks[2], Token::Ident("a123".into()));
    }

    #[test]
    fn lex_string_literal_single_quote() {
        let toks = tokens("'hello world'");
        assert_eq!(toks[0], Token::StringLit("hello world".into()));
    }

    #[test]
    fn lex_delimited_identifier_double_quote() {
        let toks = tokens("\"hello\"");
        assert_eq!(toks[0], Token::Ident("hello".into()));
    }

    #[test]
    fn lex_delimited_identifier_with_spaces() {
        let toks = tokens("\"Average Pyranometer\"");
        assert_eq!(toks[0], Token::Ident("Average Pyranometer".into()));
    }

    #[test]
    fn lex_delimited_identifier_with_keyword_name() {
        let toks = tokens("\"MATCH\"");
        assert_eq!(toks[0], Token::Ident("MATCH".into()));
    }

    #[test]
    fn lex_delimited_identifier_in_property_context() {
        let toks = tokens("\"full name\"");
        assert_eq!(toks[0], Token::Ident("full name".into()));
    }

    #[test]
    fn lex_string_literal_escaped_quote() {
        let toks = tokens(r"'it\'s fine'");
        assert_eq!(toks[0], Token::StringLit("it's fine".into()));
    }

    #[test]
    fn lex_integer_literal() {
        let toks = tokens("42 0");
        assert_eq!(toks[0], Token::IntLit(42));
        assert_eq!(toks[1], Token::IntLit(0));
    }

    #[test]
    fn lex_negative_is_separate_tokens() {
        let toks = tokens("-1");
        assert_eq!(toks[0], Token::Minus);
        assert_eq!(toks[1], Token::IntLit(1));
    }

    #[test]
    #[allow(clippy::approx_constant)]
    fn lex_float_literal() {
        let toks = tokens("3.14 0.0");
        assert_eq!(toks[0], Token::FloatLit(3.14));
        assert_eq!(toks[1], Token::FloatLit(0.0));
    }

    #[test]
    fn lex_float_scientific() {
        let toks = tokens("1.5e10");
        assert_eq!(toks[0], Token::FloatLit(1.5e10));
    }

    #[test]
    fn lex_bool_literals() {
        let toks = tokens("true false TRUE FALSE");
        assert_eq!(toks[0], Token::BoolLit(true));
        assert_eq!(toks[1], Token::BoolLit(false));
        assert_eq!(toks[2], Token::BoolLit(true));
        assert_eq!(toks[3], Token::BoolLit(false));
    }

    #[test]
    fn lex_tracks_line_col() {
        let spanned = Lexer::new("MATCH\n(a)").tokenize().unwrap();
        let lparen = spanned.iter().find(|s| s.token == Token::LParen).unwrap();
        assert_eq!(lparen.span.line, 2);
        assert_eq!(lparen.span.col, 1);
    }

    #[test]
    fn lex_unknown_char_returns_error() {
        let err = Lexer::new("@").tokenize().unwrap_err();
        assert!(err.to_string().contains("GQL syntax error"));
    }

    #[test]
    fn lex_full_match_query() {
        let toks = tokens("MATCH (a:Person {name: 'Alice'})-[:KNOWS]->(b) RETURN a.name");
        // Verify it lexes without error and produces reasonable token count.
        assert!(toks.len() > 15);
        assert_eq!(toks[0], Token::Match);
        assert_eq!(*toks.last().unwrap(), Token::Eof);
    }

    #[test]
    fn lex_line_comment_skipped() {
        let toks = tokens("MATCH // this is a comment\n(a)");
        assert_eq!(toks[0], Token::Match);
        assert_eq!(toks[1], Token::LParen);
    }

    #[test]
    fn lex_unterminated_string_returns_error() {
        let err = Lexer::new("'hello").tokenize().unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("unterminated string literal"), "got: {msg}");
        // Error should point to the opening quote, not end of input.
        assert!(msg.contains("col 1"), "got: {msg}");
    }

    #[test]
    fn lex_integer_overflow_returns_error() {
        let err = Lexer::new("9999999999999999999999").tokenize().unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("integer literal out of range"), "got: {msg}");
        // Error should point to the start of the number.
        assert!(msg.contains("col 1"), "got: {msg}");
    }

    #[test]
    fn lex_float_negative_exponent() {
        let toks = tokens("1.5e-3");
        assert_eq!(toks[0], Token::FloatLit(1.5e-3));
    }

    #[test]
    fn lex_utf8_in_string_literal() {
        let toks = tokens("'hello €'");
        assert_eq!(toks[0], Token::StringLit("hello €".into()));
    }

    #[test]
    fn lex_col_within_line() {
        let spanned = Lexer::new("MATCH (a)").tokenize().unwrap();
        // '(' is at byte offset 6, col 7 (1-based)
        let lparen = spanned.iter().find(|s| s.token == Token::LParen).unwrap();
        assert_eq!(lparen.span.line, 1);
        assert_eq!(lparen.span.col, 7);
    }

    #[test]
    #[should_panic(expected = "advance() called past end of input")]
    fn lexer_advance_past_end_panics_with_clear_message() {
        let mut lex = Lexer::new("x");
        lex.advance(); // consumes 'x'
        lex.advance(); // past end — should panic
    }

    #[test]
    fn lex_long_utf8_string_correct() {
        let inner = "€".repeat(100);
        let input = format!("'{inner}'");
        let toks = tokens(&input);
        assert_eq!(toks[0], Token::StringLit(inner));
    }

    #[test]
    fn tokenize_consumes_lexer() {
        let lex = Lexer::new("MATCH (a) RETURN a");
        let toks = lex.tokenize().unwrap();
        assert!(!toks.is_empty());
    }

    #[test]
    fn lex_error_message_not_duplicated() {
        let err = Lexer::new("@").tokenize().unwrap_err();
        let msg = err.to_string();
        // The prefix "GQL syntax error" should appear exactly once, not twice.
        assert_eq!(
            msg.matches("GQL syntax error").count(),
            1,
            "duplicated prefix in: {msg}"
        );
    }

    // ── Delimited identifier edge cases ──────────────────────────────────────

    #[test]
    fn lex_empty_delimited_identifier_is_error() {
        let err = Lexer::new("\"\"").tokenize().unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("empty delimited identifier"), "got: {msg}");
        assert!(msg.contains("col 1"), "got: {msg}");
    }

    #[test]
    fn lex_unterminated_delimited_identifier_returns_error() {
        let err = Lexer::new("\"hello").tokenize().unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("unterminated delimited identifier"),
            "got: {msg}"
        );
        assert!(msg.contains("col 1"), "got: {msg}");
    }

    #[test]
    fn lex_delimited_identifier_escaped_quote() {
        let toks = tokens(r#""say \"hello\"""#);
        assert_eq!(toks[0], Token::Ident("say \"hello\"".into()));
    }

    #[test]
    fn lex_utf8_in_delimited_identifier() {
        let toks = tokens("\"Température\"");
        assert_eq!(toks[0], Token::Ident("Température".into()));
    }

    // ── Backtick-quoted identifiers (Cypher compatibility) ──────────────────

    #[test]
    fn lex_backtick_identifier() {
        let toks = tokens("`createdAt`");
        assert_eq!(toks[0], Token::Ident("createdAt".into()));
    }

    #[test]
    fn lex_backtick_identifier_with_spaces() {
        let toks = tokens("`Average Pyranometer`");
        assert_eq!(toks[0], Token::Ident("Average Pyranometer".into()));
    }

    #[test]
    fn lex_backtick_identifier_with_keyword_name() {
        let toks = tokens("`MATCH`");
        assert_eq!(toks[0], Token::Ident("MATCH".into()));
    }

    #[test]
    fn lex_empty_backtick_identifier_is_error() {
        let err = Lexer::new("``").tokenize().unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("empty delimited identifier"), "got: {msg}");
    }

    #[test]
    fn lex_unterminated_backtick_identifier_is_error() {
        let err = Lexer::new("`hello").tokenize().unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("unterminated delimited identifier"),
            "got: {msg}"
        );
    }

    #[test]
    fn lex_unwind_keyword() {
        let toks = tokens("UNWIND");
        assert_eq!(toks[0], Token::Unwind);
    }

    #[test]
    fn lex_unwind_case_insensitive() {
        let toks = tokens("unwind");
        assert_eq!(toks[0], Token::Unwind);
    }

    #[test]
    fn lex_with_keyword_case_insensitive() {
        assert_eq!(tokens("WITH")[0], Token::With);
        assert_eq!(tokens("with")[0], Token::With);
        assert_eq!(tokens("With")[0], Token::With);
    }

    #[test]
    fn lex_skip_keyword_case_insensitive() {
        assert_eq!(tokens("SKIP")[0], Token::Skip);
        assert_eq!(tokens("skip")[0], Token::Skip);
    }

    #[test]
    fn lex_backtick_in_property_context() {
        let toks = tokens("{`full name`: 'Alice'}");
        assert_eq!(toks[0], Token::LBrace);
        assert_eq!(toks[1], Token::Ident("full name".into()));
        assert_eq!(toks[2], Token::Colon);
        assert_eq!(toks[3], Token::StringLit("Alice".into()));
        assert_eq!(toks[4], Token::RBrace);
    }

    // ── Token::Dollar (parameter placeholder sigil) ──────────────────────────
    //
    // The lexer emits `Dollar` unconditionally for each `$` it sees. It does
    // NOT validate what follows — the parser is responsible for composing
    // `Dollar + Ident` (named) or `Dollar + IntLit` (positional), and for
    // reporting syntax errors when neither follows. The tests below pin this
    // contract so a future refactor cannot accidentally move validation
    // back into the lexer.

    #[test]
    fn lexer_emits_dollar_for_named_param() {
        let toks = tokens("$name");
        assert_eq!(
            toks,
            vec![Token::Dollar, Token::Ident("name".into()), Token::Eof,]
        );
    }

    #[test]
    fn lexer_emits_dollar_for_positional_param() {
        let toks = tokens("$1");
        assert_eq!(toks, vec![Token::Dollar, Token::IntLit(1), Token::Eof,]);
    }

    #[test]
    fn lexer_emits_dollar_mid_expression() {
        // The dollar must appear at the correct position inside a longer
        // token stream (here: after `=`), not just at the start of input.
        let toks = tokens("n.id = $id");
        assert_eq!(
            toks,
            vec![
                Token::Ident("n".into()),
                Token::Dot,
                Token::Ident("id".into()),
                Token::Eq,
                Token::Dollar,
                Token::Ident("id".into()),
                Token::Eof,
            ]
        );
    }

    #[test]
    fn lexer_two_dollars_in_a_row_emit_two_dollar_tokens() {
        // `$$` is invalid GQL — the parser will reject it — but the lexer
        // does not validate the follower, so both `$` characters produce
        // a `Token::Dollar`. This test documents the lexer-vs-parser split:
        // any later change that adds validation to the lexer will break it.
        let toks = tokens("$$");
        assert_eq!(toks, vec![Token::Dollar, Token::Dollar, Token::Eof,]);
    }

    // ── Cycle 3.1: PlusEq token ──────────────────────────────────────────────

    #[test]
    fn lex_plus_eq() {
        let toks = tokens("+=");
        assert_eq!(toks[0], Token::PlusEq);
        assert_eq!(toks[1], Token::Eof);
    }

    #[test]
    fn lex_plus_not_followed_by_eq() {
        let toks = tokens("+ =");
        assert_eq!(toks[0], Token::Plus);
        assert_eq!(toks[1], Token::Eq);
    }
}
