// SPDX-License-Identifier: Apache-2.0

//! Token types for the GQL lexer.
//!
//! # Stability
//!
//! **This module is an internal implementation detail.** Although it is
//! exposed as `#[doc(hidden)] pub mod` to let integration tests and
//! downstream diagnostic tooling inspect the token stream directly, its
//! contents are NOT part of the crate's semver-stable public API. The
//! `Token` enum may gain, rename, reorder, or remove variants in any
//! patch release without notice. External crates should depend only on
//! [`crate::gql::parse`] and [`crate::gql::parse_statement`] for stable
//! behaviour.

/// A half-open byte range `[start, end)` in the source string, with its
/// 1-based line and column of the first byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Span {
    /// Byte offset of the first character (inclusive).
    pub start: usize,
    /// Byte offset one past the last character (exclusive).
    pub end: usize,
    /// 1-based line number of `start`.
    pub line: u32,
    /// 1-based column number of `start`.
    pub col: u32,
}

impl Span {
    /// Number of bytes covered by this span.
    #[must_use]
    #[allow(dead_code)] // Intentional public API; used by downstream crates and tests.
    pub const fn len(self) -> usize {
        self.end - self.start
    }

    /// Returns `true` when the span covers zero bytes.
    #[must_use]
    #[allow(dead_code)] // Intentional public API; used by downstream crates and tests.
    pub const fn is_empty(self) -> bool {
        self.start == self.end
    }
}

/// A single lexical unit produced by the GQL lexer.
///
/// Keywords are case-insensitive; the lexer normalises them before
/// constructing this enum.  Identifiers preserve their original casing.
#[derive(Debug, Clone, PartialEq)]
pub enum Token {
    // ── Keywords ────────────────────────────────────────────────────────────
    /// `MATCH`
    Match,
    /// `RETURN`
    Return,
    /// `WHERE`
    Where,
    /// `ORDER`
    Order,
    /// `BY`
    By,
    /// `LIMIT`
    Limit,
    /// `AND`
    And,
    /// `OR`
    Or,
    /// `NOT`
    Not,
    /// `IS`
    Is,
    /// `NULL`
    Null,
    /// `ASC`
    Asc,
    /// `DESC`
    Desc,
    /// `AS`
    As,
    /// `DISTINCT`
    Distinct,
    /// `GROUP` (used in GROUP BY)
    Group,
    /// `WITH` — pipeline projection boundary (Cypher-style).
    With,
    /// `SKIP` — row skip, paired with `WITH` or `RETURN`.
    Skip,

    // ── Mutation keywords ────────────────────────────────────────────────────
    /// `CREATE`
    Create,
    /// `SET`
    Set,
    /// `DELETE`
    Delete,
    /// `DETACH`
    Detach,
    /// `MERGE`
    Merge,
    /// `UNWIND`
    Unwind,

    // ── Aggregation keywords ─────────────────────────────────────────────────
    /// `COUNT`
    Count,
    /// `SUM`
    Sum,
    /// `AVG`
    Avg,
    /// `MIN`
    Min,
    /// `MAX`
    Max,
    /// `COLLECT`
    Collect,

    // ── Punctuation ──────────────────────────────────────────────────────────
    /// `(`
    LParen,
    /// `)`
    RParen,
    /// `[`
    LBracket,
    /// `]`
    RBracket,
    /// `{`
    LBrace,
    /// `}`
    RBrace,
    /// `:`
    Colon,
    /// `,`
    Comma,
    /// `.`
    Dot,
    /// `*`
    Star,
    /// `|`
    Pipe,
    /// `$` — parameter placeholder sigil.
    ///
    /// The lexer emits `Dollar` unconditionally for every `$` it sees and
    /// performs no validation of what follows. The parser is responsible
    /// for composing `Dollar + Ident` (named, `$name`) or `Dollar + IntLit`
    /// (positional, `$1`). Keeping validation in the parser preserves the
    /// table-driven lexer with no lookahead.
    Dollar,

    // ── Arrows and dash ──────────────────────────────────────────────────────
    /// `->`
    ArrowRight,
    /// `<-`
    ArrowLeft,
    /// `-` as a standalone path-pattern dash (or arithmetic minus).
    ///
    /// The parser distinguishes between path-pattern usage and arithmetic
    /// subtraction by context; the lexer always emits `Minus`.
    Minus,

    // ── Comparison operators ─────────────────────────────────────────────────
    /// `=`
    Eq,
    /// `<>` (not equal)
    NotEq,
    /// `<`
    Lt,
    /// `>`
    Gt,
    /// `<=`
    LtEq,
    /// `>=`
    GtEq,

    // ── Arithmetic operators ─────────────────────────────────────────────────
    /// `+`
    Plus,
    /// `+=` — whole-entity map-merge assignment (`SET n += $map`).
    PlusEq,
    /// `/`
    Slash,

    // ── Range operator ───────────────────────────────────────────────────────
    /// `..`
    DotDot,

    // ── Literals ─────────────────────────────────────────────────────────────
    /// An identifier (variable name, label, property key, …).
    Ident(String),
    /// A single-quoted string value (e.g. `'hello'`).
    ///
    /// Double-quoted values produce [`Token::Ident`] (delimited identifier
    /// per ISO GQL).
    StringLit(String),
    /// A decimal integer value.
    IntLit(i64),
    /// A decimal floating-point value (optionally in scientific notation).
    FloatLit(f64),
    /// `true` or `false` (case-insensitive).
    BoolLit(bool),

    // ── Meta ─────────────────────────────────────────────────────────────────
    /// Signals the end of the token stream.
    Eof,
}

/// A [`Token`] together with its [`Span`] in the original source.
#[derive(Debug, Clone, PartialEq)]
pub struct SpannedToken {
    /// The token kind and (for literals/identifiers) its value.
    pub token: Token,
    /// Source position of this token.
    pub span: Span,
}

/// Returns `true` when `a` and `b` share the same enum discriminant,
/// regardless of any inner payload values.
#[must_use]
pub fn same_discriminant(a: &Token, b: &Token) -> bool {
    std::mem::discriminant(a) == std::mem::discriminant(b)
}

impl std::fmt::Display for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Match => f.write_str("MATCH"),
            Self::Return => f.write_str("RETURN"),
            Self::Where => f.write_str("WHERE"),
            Self::Order => f.write_str("ORDER"),
            Self::By => f.write_str("BY"),
            Self::Limit => f.write_str("LIMIT"),
            Self::And => f.write_str("AND"),
            Self::Or => f.write_str("OR"),
            Self::Not => f.write_str("NOT"),
            Self::Is => f.write_str("IS"),
            Self::Null => f.write_str("NULL"),
            Self::Asc => f.write_str("ASC"),
            Self::Desc => f.write_str("DESC"),
            Self::As => f.write_str("AS"),
            Self::Distinct => f.write_str("DISTINCT"),
            Self::Group => f.write_str("GROUP"),
            Self::With => f.write_str("WITH"),
            Self::Skip => f.write_str("SKIP"),
            Self::Create => f.write_str("CREATE"),
            Self::Set => f.write_str("SET"),
            Self::Delete => f.write_str("DELETE"),
            Self::Detach => f.write_str("DETACH"),
            Self::Merge => f.write_str("MERGE"),
            Self::Unwind => f.write_str("UNWIND"),
            Self::Count => f.write_str("COUNT"),
            Self::Sum => f.write_str("SUM"),
            Self::Avg => f.write_str("AVG"),
            Self::Min => f.write_str("MIN"),
            Self::Max => f.write_str("MAX"),
            Self::Collect => f.write_str("COLLECT"),
            Self::LParen => f.write_str("("),
            Self::RParen => f.write_str(")"),
            Self::LBracket => f.write_str("["),
            Self::RBracket => f.write_str("]"),
            Self::LBrace => f.write_str("{"),
            Self::RBrace => f.write_str("}"),
            Self::Colon => f.write_str(":"),
            Self::Comma => f.write_str(","),
            Self::Dot => f.write_str("."),
            Self::Star => f.write_str("*"),
            Self::Pipe => f.write_str("|"),
            Self::Dollar => f.write_str("$"),
            Self::ArrowRight => f.write_str("->"),
            Self::ArrowLeft => f.write_str("<-"),
            Self::Minus => f.write_str("-"),
            Self::Eq => f.write_str("="),
            Self::NotEq => f.write_str("<>"),
            Self::Lt => f.write_str("<"),
            Self::Gt => f.write_str(">"),
            Self::LtEq => f.write_str("<="),
            Self::GtEq => f.write_str(">="),
            Self::Plus => f.write_str("+"),
            Self::PlusEq => f.write_str("+="),
            Self::Slash => f.write_str("/"),
            Self::DotDot => f.write_str(".."),
            Self::Ident(s) => {
                // Re-quote identifiers that contain characters outside [a-zA-Z0-9_]
                // so that error messages are unambiguous for delimited identifiers.
                if s.chars().any(|c| !c.is_ascii_alphanumeric() && c != '_') {
                    write!(f, "\"{s}\"")
                } else {
                    f.write_str(s)
                }
            }
            Self::StringLit(s) => write!(f, "'{s}'"),
            Self::IntLit(v) => write!(f, "{v}"),
            Self::FloatLit(v) => write!(f, "{v}"),
            Self::BoolLit(b) => write!(f, "{b}"),
            Self::Eof => f.write_str("<end of input>"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_debug_and_eq() {
        assert_eq!(Token::Match, Token::Match);
        assert_ne!(Token::Match, Token::Return);
        assert_eq!(Token::Ident("foo".into()), Token::Ident("foo".into()));
        assert_ne!(Token::Ident("foo".into()), Token::Ident("bar".into()));
    }

    #[test]
    fn span_positions() {
        let s = Span { start: 0, end: 5, line: 1, col: 1 };
        assert_eq!(s.len(), 5);
        assert!(!s.is_empty());
    }

    #[test]
    fn span_empty() {
        let s = Span { start: 3, end: 3, line: 1, col: 4 };
        assert!(s.is_empty());
        assert_eq!(s.len(), 0);
    }

    #[test]
    fn span_is_hashable() {
        use std::collections::HashSet;
        let s = Span { start: 0, end: 5, line: 1, col: 1 };
        let mut set = HashSet::new();
        set.insert(s);
        assert!(set.contains(&s));
    }

    #[test]
    fn token_display_keywords() {
        assert_eq!(Token::Match.to_string(), "MATCH");
        assert_eq!(Token::Return.to_string(), "RETURN");
        assert_eq!(Token::Where.to_string(), "WHERE");
        assert_eq!(Token::Limit.to_string(), "LIMIT");
    }

    #[test]
    fn token_display_mutation_keywords() {
        assert_eq!(Token::Create.to_string(), "CREATE");
        assert_eq!(Token::Set.to_string(), "SET");
        assert_eq!(Token::Delete.to_string(), "DELETE");
        assert_eq!(Token::Detach.to_string(), "DETACH");
        assert_eq!(Token::Merge.to_string(), "MERGE");
    }

    #[test]
    fn token_display_punctuation() {
        assert_eq!(Token::LParen.to_string(), "(");
        assert_eq!(Token::RParen.to_string(), ")");
        assert_eq!(Token::Colon.to_string(), ":");
        assert_eq!(Token::ArrowRight.to_string(), "->");
        assert_eq!(Token::ArrowLeft.to_string(), "<-");
    }

    #[test]
    fn token_display_dollar() {
        // Dollar is rendered literally so parser error messages quoting the
        // offending token (e.g. "expected parameter name, found $") remain
        // readable. Pinning this lets future refactors detect accidental
        // changes to the Display impl.
        assert_eq!(Token::Dollar.to_string(), "$");
    }

    #[test]
    fn same_discriminant_ignores_inner_value() {
        assert!(same_discriminant(
            &Token::Ident("foo".into()),
            &Token::Ident("bar".into()),
        ));
        assert!(!same_discriminant(
            &Token::Ident("MATCH".into()),
            &Token::Match,
        ));
    }

    #[test]
    fn token_display_literals() {
        assert_eq!(Token::Ident("myVar".into()).to_string(), "myVar");
        assert_eq!(Token::IntLit(42).to_string(), "42");
        assert_eq!(Token::StringLit("hello".into()).to_string(), "'hello'");
        assert_eq!(Token::BoolLit(true).to_string(), "true");
        assert_eq!(Token::BoolLit(false).to_string(), "false");
        assert_eq!(Token::Null.to_string(), "NULL");
        assert_eq!(Token::Eof.to_string(), "<end of input>");
    }

    #[test]
    fn token_display_ident_delimited_re_quotes() {
        // Simple identifier: no quotes added.
        assert_eq!(Token::Ident("myVar".into()).to_string(), "myVar");
        // Delimited identifier with space: must be re-quoted.
        assert_eq!(
            Token::Ident("Average Pyranometer".into()).to_string(),
            "\"Average Pyranometer\""
        );
        // Keyword name without special chars: no re-quoting needed.
        assert_eq!(Token::Ident("MATCH".into()).to_string(), "MATCH");
    }
}
