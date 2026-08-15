// SPDX-License-Identifier: MIT

//! GQL recursive-descent parser.
//!
//! Converts a flat [`Vec<SpannedToken>`] produced by the lexer into a
//! fully-typed [`GqlQuery`] AST.  All parsing is done in a single pass with
//! bounded look-ahead (at most 2 tokens) and explicit precedence levels for
//! the expression sub-grammar.

use crate::Error;
use crate::gql::ast::{
    AggFunc, AstDirection, BinOp, ConstReturnQuery, CreateClause, CreatePattern, DeleteClause,
    EdgeLength, EdgePattern, Expr, GqlQuery, GqlStatement, LimitClause, ListPredKind, Literal,
    MatchClause, MergeClause, MutationClause, MutationStatement, NodePattern, OrderByClause,
    OrderItem, ParamRef, PathPattern, ReturnClause, ReturnItem, SetAssignment, SetClause, UnaryOp,
    WhereClause,
};
use crate::gql::token::{self, Span, SpannedToken, Token};

// ── Internal struct for edge bracket content ──────────────────────────────────

/// Parsed contents of an edge bracket `[var? :Label* {props}? *range?]`.
struct EdgeBracketContent {
    var: Option<String>,
    labels: Vec<String>,
    props: Vec<(String, Literal)>,
    length: EdgeLength,
}

// ── Parser struct ────────────────────────────────────────────────────────────

/// Maximum expression nesting depth to prevent stack overflow on
/// deeply nested inputs.
const MAX_EXPR_DEPTH: usize = 128;

/// Error emitted when a variable-reference node in CREATE has inline properties.
const VAR_REF_PROPS_ERROR: &str = "variable reference in CREATE cannot have inline properties; \
     add a label to create a new node";

/// Recursive-descent parser for the GQL read-only query subset.
///
/// Construct with [`Parser::new`] and consume with [`Parser::parse`].
pub struct Parser {
    tokens: Vec<SpannedToken>,
    pos: usize,
    expr_depth: usize,
}

/// Maps a lowercased identifier to its [`ListPredKind`], or `None` if it is not
/// a list-predicate keyword. Used by the parser to route `ALL`/`ANY`/`NONE`/
/// `SINGLE` to the list-predicate grammar instead of a generic function call.
fn list_pred_kind(name_lower: &str) -> Option<ListPredKind> {
    match name_lower {
        "all" => Some(ListPredKind::All),
        "any" => Some(ListPredKind::Any),
        "none" => Some(ListPredKind::None),
        "single" => Some(ListPredKind::Single),
        _ => None,
    }
}

impl Parser {
    /// Creates a new `Parser` from a token stream produced by the lexer.
    ///
    /// The token stream **must** end with [`Token::Eof`]; the lexer always
    /// appends one.
    ///
    /// # Panics
    ///
    /// In debug builds, panics if the token stream is empty or does not end
    /// with `Eof`. In release builds the assertion is elided; a malformed
    /// stream will produce a [`Error::GqlSyntaxError`] during parsing.
    #[must_use]
    pub fn new(tokens: Vec<SpannedToken>) -> Self {
        debug_assert!(
            tokens.last().is_some_and(|t| t.token == Token::Eof),
            "token stream must end with Eof"
        );
        Self {
            tokens,
            pos: 0,
            expr_depth: 0,
        }
    }

    /// Returns `true` if the token stream is well-formed: non-empty and
    /// ending with [`Token::Eof`]. Called at the start of every public
    /// `parse` entry point so that malformed streams fail loudly in
    /// release builds instead of relying on `peek()`'s Eof fallback.
    fn stream_is_terminated(&self) -> bool {
        self.tokens.last().is_some_and(|t| t.token == Token::Eof)
    }

    // ── Token stream primitives ──────────────────────────────────────────────

    /// Returns a reference to the current token without advancing.
    ///
    /// Returns `&Token::Eof` when the position is past the end of the stream.
    fn peek(&self) -> &Token {
        self.tokens
            .get(self.pos)
            .map_or(&Token::Eof, |st| &st.token)
    }

    /// Returns a reference to the token `offset` positions ahead without consuming anything.
    ///
    /// Returns `&Token::Eof` when the look-ahead position is past the end of the stream.
    fn peek_ahead(&self, offset: usize) -> &Token {
        self.tokens
            .get(self.pos + offset)
            .map_or(&Token::Eof, |st| &st.token)
    }

    /// Returns `true` if the token `offset` positions ahead is the `WITH`
    /// keyword. The lexer reserves `WITH` as [`Token::With`] (needed for
    /// `WITH x AS y` pipelines), so the second token of `STARTS WITH` /
    /// `ENDS WITH` arrives as `Token::With`, not `Token::Ident("WITH")`. The
    /// `Ident` arm is a defensive fallback in case the lexer ever changes.
    fn peek_is_with_keyword(&self, offset: usize) -> bool {
        let tok = self.peek_ahead(offset);
        matches!(tok, Token::With)
            || matches!(tok, Token::Ident(s) if s.eq_ignore_ascii_case("WITH"))
    }

    /// Returns the source [`Span`] of the current (not yet consumed) token.
    fn current_span(&self) -> Span {
        self.tokens.get(self.pos).map_or(
            Span {
                start: 0,
                end: 0,
                line: 1,
                col: 1,
            },
            |st| st.span,
        )
    }

    /// Builds a [`Error::GqlSyntaxError`] anchored at the current token
    /// position.
    fn syntax_error(&self, msg: impl Into<String>) -> Error {
        let span = self.current_span();
        Error::GqlSyntaxError {
            line: span.line,
            col: span.col,
            message: msg.into(),
        }
    }

    /// Builds an [`Error::GqlUnsupported`] for features not currently implemented.
    fn unsupported_feature(feature: &str) -> Error {
        Error::GqlUnsupported(format!("{feature} are not supported"))
    }

    /// Consumes the current token and advances the position by one.
    ///
    /// Returns a reference to the **consumed** token.
    ///
    /// # Panics
    ///
    /// Panics if called when the position is already at or past the end of
    /// the token stream. Callers must always check [`peek()`](Self::peek)
    /// before calling this method.
    fn advance(&mut self) -> &SpannedToken {
        let tok = self
            .tokens
            .get(self.pos)
            .expect("advance() called past end of token stream");
        self.pos += 1;
        tok
    }

    /// Consumes the current token if its discriminant matches `expected`,
    /// otherwise returns a syntax error.
    ///
    /// This uses discriminant-only comparison so it works for tokens that
    /// carry inner values (e.g. `Token::Ident(_)`).
    fn expect(&mut self, expected: &Token) -> crate::Result<()> {
        if token::same_discriminant(self.peek(), expected) {
            self.advance();
            Ok(())
        } else {
            Err(self.syntax_error(format!("expected {expected}, found {}", self.peek())))
        }
    }

    /// Consumes the current token and returns its inner `String` value if it
    /// is a [`Token::Ident`], otherwise returns a syntax error.
    fn expect_ident(&mut self) -> crate::Result<String> {
        if let Token::Ident(name) = self.peek() {
            let name = name.clone();
            self.advance();
            Ok(name)
        } else {
            Err(self.syntax_error(format!("expected identifier, found {}", self.peek())))
        }
    }

    /// Consumes the current token and returns its inner `i64` value if it is
    /// a [`Token::IntLit`], otherwise returns a syntax error.
    fn expect_int(&mut self) -> crate::Result<i64> {
        if let Token::IntLit(v) = self.peek() {
            let v = *v;
            self.advance();
            Ok(v)
        } else {
            Err(self.syntax_error(format!("expected integer literal, found {}", self.peek())))
        }
    }

    /// Increments the expression nesting depth, returning an error if the
    /// maximum is exceeded.
    fn enter_expr(&mut self) -> crate::Result<()> {
        self.expr_depth += 1;
        if self.expr_depth > MAX_EXPR_DEPTH {
            return Err(self.syntax_error(format!(
                "expression nesting exceeds maximum depth of {MAX_EXPR_DEPTH}"
            )));
        }
        Ok(())
    }

    /// Decrements the expression nesting depth.
    const fn exit_expr(&mut self) {
        self.expr_depth = self.expr_depth.saturating_sub(1);
    }

    // ── Entry point ──────────────────────────────────────────────────────────

    /// Parses the full token stream into a [`GqlQuery`].
    ///
    /// # Errors
    ///
    /// Returns [`Error::GqlSyntaxError`] on any malformed input.
    pub fn parse(mut self) -> crate::Result<GqlQuery> {
        if !self.stream_is_terminated() {
            return Err(self.syntax_error("malformed token stream: missing terminating Eof"));
        }

        let unwind_clause = self.parse_unwind_clause()?;

        let match_clause = self.parse_match_clause()?;
        let where_clause = self.parse_where_clause()?;
        let return_clause = self.parse_return_clause()?;
        let group_by = self.parse_group_by_clause()?;
        let order_by = self.parse_order_by_clause()?;
        let limit = self.parse_limit_clause()?;

        if *self.peek() != Token::Eof {
            return Err(self.syntax_error("unexpected tokens after query"));
        }

        Ok(GqlQuery {
            unwind_clause,
            match_clause,
            where_clause,
            return_clause,
            group_by,
            order_by,
            limit,
        })
    }

    // ── Unified entry point ──────────────────────────────────────────────────

    /// Parses a GQL statement — either a read-only query or a mutation.
    ///
    /// Dispatches on the leading keyword:
    /// - `MATCH` — could be a read query (`MATCH...RETURN`) or a mutation (`MATCH...DELETE`/`SET`).
    /// - `CREATE` — a CREATE mutation.
    /// - `MERGE` — a MERGE mutation.
    /// - `DELETE`/`DETACH` without a preceding `MATCH` — a syntax error.
    ///
    /// Use `parse()` for backward-compatible read-only query parsing.
    ///
    /// # Errors
    ///
    /// Returns [`Error::GqlSyntaxError`] on malformed input.
    pub fn parse_statement(mut self) -> crate::Result<GqlStatement> {
        if !self.stream_is_terminated() {
            return Err(self.syntax_error("malformed token stream: missing terminating Eof"));
        }

        // Parse optional UNWIND before the main clause dispatch.
        let unwind_clause = self.parse_unwind_clause()?;

        match self.peek().clone() {
            Token::Match => {
                // Support consecutive MATCH clauses: `MATCH (a:X) MATCH (b:Y) ...`
                // Each MATCH may have its own WHERE; all patterns are merged into one MatchClause.
                let mut match_clause = self.parse_match_clause()?;
                let mut where_clause = self.parse_where_clause()?;
                while *self.peek() == Token::Match {
                    let next = self.parse_match_clause()?;
                    match_clause.patterns.extend(next.patterns);
                    let next_where = self.parse_where_clause()?;
                    where_clause = match (where_clause, next_where) {
                        (None, w) | (w, None) => w,
                        (Some(a), Some(b)) => Some(WhereClause {
                            predicate: Expr::BinaryOp {
                                left: Box::new(a.predicate),
                                op: BinOp::And,
                                right: Box::new(b.predicate),
                            },
                        }),
                    };
                }

                // Pipeline detection: WITH (optionally after a leading UNWIND)
                // switches to the multi-stage AST.
                if *self.peek() == Token::With {
                    return self.parse_pipeline_after_match(
                        unwind_clause,
                        match_clause,
                        where_clause,
                    );
                }

                let result = self.parse_after_match(match_clause, where_clause)?;
                let result = inject_unwind(result, unwind_clause);
                Ok(result)
            }
            Token::Create => {
                let create = self.parse_create_clause()?;
                let return_clause = self.parse_optional_trailing_return()?;
                if *self.peek() != Token::Eof {
                    return Err(self.syntax_error("unexpected tokens after CREATE"));
                }
                Ok(GqlStatement::Mutation(MutationStatement {
                    unwind_clause,
                    match_clause: None,
                    mutation: MutationClause::Create(create),
                    set_clause: None,
                    return_clause,
                }))
            }
            Token::Merge => {
                let merge = self.parse_merge_clause()?;
                if *self.peek() != Token::Eof {
                    return Err(self.syntax_error("unexpected tokens after MERGE"));
                }
                Ok(GqlStatement::Mutation(MutationStatement {
                    unwind_clause,
                    match_clause: None,
                    mutation: MutationClause::Merge(merge),
                    set_clause: None,
                    return_clause: None,
                }))
            }
            Token::Delete | Token::Detach => {
                Err(self.syntax_error("DELETE requires a preceding MATCH clause to bind variables"))
            }
            Token::Set => {
                Err(self.syntax_error("SET requires a preceding MATCH clause to bind variables"))
            }
            Token::Return => {
                // `RETURN <expr-list>` as a root statement, with no MATCH/
                // UNWIND/CREATE prefix. Evaluated in an empty binding
                // context and yields exactly one row. Covers the standard
                // Bolt driver keep-alive (`RETURN 1`) and any constant-row
                // projection used by drivers and benchmarks.
                if unwind_clause.is_some() {
                    return Err(self.syntax_error(
                        "UNWIND before RETURN requires a binding scope (MATCH or pipeline WITH)",
                    ));
                }
                let const_return = self.parse_const_return_query()?;
                if *self.peek() != Token::Eof {
                    return Err(self.syntax_error("unexpected tokens after RETURN"));
                }
                Ok(GqlStatement::ConstReturn(const_return))
            }
            other => Err(self.syntax_error(format!(
                "expected MATCH, CREATE, MERGE, RETURN, or mutation keyword, found {other}"
            ))),
        }
    }

    /// Parses a `RETURN <expr-list>` root statement once `Token::Return` is
    /// at the head of the stream. Accepts optional `DISTINCT`, optional
    /// `SKIP <expr>` and `LIMIT <expr>` (driver compatibility — both are
    /// no-ops on a one-row stream when zero, and yield zero rows when
    /// `SKIP >= 1`).
    ///
    /// Unlike `parse_skip_clause` / `parse_limit_clause` used by normal
    /// queries (which take an `IntLit`), the SKIP/LIMIT here accept any
    /// expression so that drivers can parametrise them with `$n`. The
    /// executor rejects non-Int values at its own seam.
    fn parse_const_return_query(&mut self) -> crate::Result<ConstReturnQuery> {
        self.expect(&Token::Return)?;
        let (distinct, items) = self.parse_return_items_after_keyword()?;

        // SKIP comes before LIMIT in openCypher.
        let skip = if *self.peek() == Token::Skip {
            self.advance();
            Some(self.parse_expr()?)
        } else {
            None
        };
        let limit = if *self.peek() == Token::Limit {
            self.advance();
            Some(self.parse_expr()?)
        } else {
            None
        };

        Ok(ConstReturnQuery {
            items,
            distinct,
            limit,
            skip,
        })
    }

    /// Parses an optional trailing `RETURN <items>` after a mutation
    /// (`… SET … RETURN n`, `CREATE (…) RETURN n`). Returns `None` when the
    /// next token is not `RETURN`, leaving the stream untouched so the caller's
    /// `Eof` check still applies. The official Neo4j driver emits this form for
    /// the idiomatic upsert ("mutate, then give me the node back"); without it
    /// the driver surfaces `Neo.ClientError.Statement.SyntaxError`.
    fn parse_optional_trailing_return(&mut self) -> crate::Result<Option<Box<ReturnClause>>> {
        if *self.peek() != Token::Return {
            return Ok(None);
        }
        self.advance(); // consume RETURN
        let (distinct, items) = self.parse_return_items_after_keyword()?;
        Ok(Some(Box::new(ReturnClause { distinct, items })))
    }

    /// Dispatches the continuation after one or more consecutive MATCH clauses.
    ///
    /// `match_clause` contains the merged patterns from all preceding MATCH statements.
    /// `where_clause` is the AND-merged predicates from all preceding WHERE clauses,
    /// or `None` if no MATCH had a WHERE.
    ///
    /// Handles: `RETURN` (produces `Query`), `DELETE`/`DETACH`/`SET`/`CREATE` (produce `Mutation`).
    fn parse_after_match(
        &mut self,
        match_clause: MatchClause,
        where_clause: Option<WhereClause>,
    ) -> crate::Result<GqlStatement> {
        match self.peek() {
            Token::Return => {
                let return_clause = self.parse_return_clause()?;
                let group_by = self.parse_group_by_clause()?;
                let order_by = self.parse_order_by_clause()?;
                let limit = self.parse_limit_clause()?;
                if *self.peek() != Token::Eof {
                    return Err(self.syntax_error("unexpected tokens after query"));
                }
                Ok(GqlStatement::Query(GqlQuery {
                    unwind_clause: None,
                    match_clause,
                    where_clause,
                    return_clause,
                    group_by,
                    order_by,
                    limit,
                }))
            }
            Token::Delete | Token::Detach => {
                let delete = self.parse_delete_clause()?;
                if *self.peek() != Token::Eof {
                    return Err(self.syntax_error("unexpected tokens after DELETE"));
                }
                Ok(GqlStatement::Mutation(MutationStatement {
                    unwind_clause: None,
                    match_clause: Some(match_clause),
                    mutation: MutationClause::Delete(delete),
                    set_clause: None,
                    return_clause: None,
                }))
            }
            Token::Set => {
                let set = self.parse_set_clause()?;
                let return_clause = self.parse_optional_trailing_return()?;
                if *self.peek() != Token::Eof {
                    return Err(self.syntax_error("unexpected tokens after SET"));
                }
                Ok(GqlStatement::Mutation(MutationStatement {
                    unwind_clause: None,
                    match_clause: Some(match_clause),
                    mutation: MutationClause::Set(set),
                    set_clause: None,
                    return_clause,
                }))
            }
            Token::Create => {
                let create = self.parse_create_clause()?;
                let return_clause = self.parse_optional_trailing_return()?;
                if *self.peek() != Token::Eof {
                    return Err(self.syntax_error("unexpected tokens after CREATE"));
                }
                Ok(GqlStatement::Mutation(MutationStatement {
                    unwind_clause: None,
                    match_clause: Some(match_clause),
                    mutation: MutationClause::Create(create),
                    set_clause: None,
                    return_clause,
                }))
            }
            other => Err(self.syntax_error(format!(
                "expected RETURN, DELETE, SET, or CREATE after MATCH, found {other}"
            ))),
        }
    }

    // ── Mutation clauses ─────────────────────────────────────────────────────

    /// Parses `CREATE pattern1, pattern2, ...`.
    fn parse_create_clause(&mut self) -> crate::Result<CreateClause> {
        self.expect(&Token::Create)?;
        let mut patterns = Vec::with_capacity(4);
        self.parse_create_pattern_multi(&mut patterns)?;
        while *self.peek() == Token::Comma {
            self.advance(); // consume ','
            self.parse_create_pattern_multi(&mut patterns)?;
        }
        Ok(CreateClause { patterns })
    }

    /// Parses one create item, which may expand into multiple patterns.
    ///
    /// A simple node `(n:Label {p})` appends one `CreatePattern::Node`.
    /// An inline path `(a:L1)-[:R]->(b:L2)` appends two `CreatePattern::Node`
    /// entries and one `CreatePattern::Edge` entry (in that order so that
    /// the executor can resolve source and target variables before processing
    /// the edge).
    fn parse_create_pattern_multi(&mut self, out: &mut Vec<CreatePattern>) -> crate::Result<()> {
        self.expect(&Token::LParen)?;

        let var = if let Token::Ident(_) = self.peek() {
            Some(self.expect_ident()?)
        } else {
            None
        };

        // Source var-ref detection uses `peek() == RParen && peek_ahead(1) == Minus|ArrowLeft`
        // because the RParen has not yet been consumed — we need two-token lookahead.
        // (Contrast with target detection in `parse_create_edge_continuation`, which uses
        // `peek() != Colon` because LParen was already consumed at that point.)
        // A bare `(n)` without label and without edge continuation is still an error.
        let is_var_ref = var.is_some()
            && *self.peek() == Token::RParen
            && matches!(*self.peek_ahead(1), Token::Minus | Token::ArrowLeft);

        if var.is_some() && *self.peek() == Token::LBrace {
            return Err(self.syntax_error(VAR_REF_PROPS_ERROR));
        }

        let (label, props, prop_map) = if is_var_ref {
            (String::new(), Vec::new(), None)
        } else {
            self.expect(&Token::Colon)?;
            let label = self.expect_ident()?;
            // Property source: inline `{key: expr, ...}` OR bare `$param`.
            let (props, prop_map) = if *self.peek() == Token::LBrace {
                (self.parse_create_props()?, None)
            } else if *self.peek() == Token::Dollar {
                self.advance(); // consume '$'
                let param_name = self.expect_ident()?;
                (
                    Vec::new(),
                    Some(Expr::ParamRef(ParamRef::Named(param_name))),
                )
            } else {
                (Vec::new(), None)
            };
            (label, props, prop_map)
        };

        self.expect(&Token::RParen)?;

        let source_var_name = var
            .clone()
            .unwrap_or_else(|| format!("_anon_{}", out.len()));
        let node_idx = out.len();
        if !is_var_ref {
            out.push(CreatePattern::Node {
                var,
                label,
                props,
                prop_map,
            });
        }

        // Incoming edges are not supported in CREATE.
        if *self.peek() == Token::ArrowLeft {
            return Err(self.syntax_error(
                "CREATE only supports outgoing edges (-[r:REL]->); \
                 incoming edge (<-[r:REL]-) is not valid in CREATE",
            ));
        }

        // Check for an outgoing edge continuation: `-[...]->`
        if *self.peek() == Token::Minus {
            self.parse_create_edge_continuation(out, source_var_name, is_var_ref, node_idx)?;
        }

        Ok(())
    }

    /// Parses the `-[rel_label {props}]->(target)` continuation of a CREATE pattern,
    /// appending `CreatePattern::Node` (if the target is new) and `CreatePattern::Edge`
    /// to `out`.
    ///
    /// Precondition: the caller has confirmed `self.peek() == Token::Minus` but has
    /// not consumed it yet.
    fn parse_create_edge_continuation(
        &mut self,
        out: &mut Vec<CreatePattern>,
        source_var_name: String,
        is_source_var_ref: bool,
        node_idx: usize,
    ) -> crate::Result<()> {
        self.advance(); // consume '-'
        self.expect(&Token::LBracket)?;

        // Optional edge variable (discarded — CreatePattern::Edge does not carry
        // an edge variable; the executor references edges by source/target).
        if let Token::Ident(_) = self.peek() {
            self.expect_ident()?;
        }
        self.expect(&Token::Colon)?;
        let rel_label = self.expect_ident()?;
        let rel_props = if *self.peek() == Token::LBrace {
            self.parse_create_props()?
        } else {
            Vec::new()
        };
        self.expect(&Token::RBracket)?;

        // Only outgoing directed edges (`->`) are supported in CREATE.
        if *self.peek() != Token::ArrowRight {
            let found = self.peek().clone();
            return Err(self.syntax_error(format!(
                "CREATE only supports outgoing edges (-[r:REL]->), found {found}"
            )));
        }
        self.advance(); // consume '->'

        // Parse the target node.
        // Target var-ref detection uses `peek() != Colon` because the LParen has
        // already been consumed — we are positioned at the first token inside the
        // target parens (contrast with the source, which checks via peek_ahead).
        self.expect(&Token::LParen)?;
        let target_var = if let Token::Ident(_) = self.peek() {
            Some(self.expect_ident()?)
        } else {
            None
        };

        let target_is_var_ref = target_var.is_some() && *self.peek() != Token::Colon;

        if target_is_var_ref && *self.peek() == Token::LBrace {
            return Err(self.syntax_error(VAR_REF_PROPS_ERROR));
        }

        let (target_label, target_props) = if target_is_var_ref {
            (String::new(), Vec::new())
        } else {
            self.expect(&Token::Colon)?;
            let lbl = self.expect_ident()?;
            let prp = if *self.peek() == Token::LBrace {
                self.parse_create_props()?
            } else {
                Vec::new()
            };
            (lbl, prp)
        };
        self.expect(&Token::RParen)?;

        let target_var_name = target_var
            .clone()
            .unwrap_or_else(|| format!("_anon_{}", out.len()));

        // Determine the actual source variable name from the already-pushed node
        // or from the variable reference.
        let edge_source = if is_source_var_ref {
            source_var_name
        } else {
            out[node_idx].var().map_or(source_var_name, String::from)
        };

        if !target_is_var_ref {
            out.push(CreatePattern::Node {
                var: target_var,
                label: target_label,
                props: target_props,
                prop_map: None,
            });
        }
        out.push(CreatePattern::Edge {
            source_var: edge_source,
            rel_label,
            rel_props,
            target_var: target_var_name,
        });

        Ok(())
    }

    /// Parses `DELETE var1, var2` or `DETACH DELETE var1, var2`.
    fn parse_delete_clause(&mut self) -> crate::Result<DeleteClause> {
        let detach = if *self.peek() == Token::Detach {
            self.advance(); // consume DETACH
            true
        } else {
            false
        };
        self.expect(&Token::Delete)?;

        let mut vars = Vec::with_capacity(4);
        vars.push(self.expect_ident()?);
        while *self.peek() == Token::Comma {
            self.advance(); // consume ','
            vars.push(self.expect_ident()?);
        }

        Ok(DeleteClause { detach, vars })
    }

    /// Parses `SET var.prop = expr, var.prop = expr`.
    fn parse_set_clause(&mut self) -> crate::Result<SetClause> {
        self.expect(&Token::Set)?;

        let mut assignments = Vec::with_capacity(4);
        assignments.push(self.parse_set_assignment()?);
        while *self.peek() == Token::Comma {
            self.advance(); // consume ','
            assignments.push(self.parse_set_assignment()?);
        }

        Ok(SetClause { assignments })
    }

    /// Parses a single SET assignment in one of three forms:
    /// `var.prop = expr`, `var = $map` (overwrite), or `var += $map` (merge).
    fn parse_set_assignment(&mut self) -> crate::Result<SetAssignment> {
        let var = self.expect_ident()?;

        // Whole-entity forms have no `.prop` after the variable name.
        match self.peek() {
            Token::Eq => {
                self.advance(); // consume '='
                let map_expr = self.parse_expr()?;
                return Ok(SetAssignment::EntityOverwrite { var, map_expr });
            }
            Token::PlusEq => {
                self.advance(); // consume '+='
                let map_expr = self.parse_expr()?;
                return Ok(SetAssignment::EntityMerge { var, map_expr });
            }
            _ => {}
        }

        // Per-property form: `var.prop = expr`.
        self.expect(&Token::Dot)?;
        let prop = self.expect_ident()?;
        self.expect(&Token::Eq)?;
        let value = self.parse_expr()?;
        Ok(SetAssignment::Property { var, prop, value })
    }

    /// Parses `MERGE (var:Label {props})` with optional `ON CREATE SET ...`,
    /// `ON MATCH SET ...`, and a trailing `RETURN var`.
    fn parse_merge_clause(&mut self) -> crate::Result<MergeClause> {
        self.expect(&Token::Merge)?;
        self.expect(&Token::LParen)?;

        let var = if let Token::Ident(_) = self.peek() {
            Some(self.expect_ident()?)
        } else {
            None
        };

        // A label is mandatory for MERGE.
        self.expect(&Token::Colon)?;
        let label = self.expect_ident()?;

        // Inline props as expressions (so `$param` is accepted as a value).
        let props = if *self.peek() == Token::LBrace {
            self.parse_create_props()?
        } else {
            Vec::new()
        };

        self.expect(&Token::RParen)?;

        // Optional `ON CREATE SET ...`.
        let on_create = if self.peek_is_on_then(&Token::Create) {
            self.advance(); // ON
            self.advance(); // CREATE
            Some(self.parse_set_clause()?)
        } else {
            None
        };

        // Optional `ON MATCH SET ...`.
        let on_match = if self.peek_is_on_then(&Token::Match) {
            self.advance(); // ON
            self.advance(); // MATCH
            Some(self.parse_set_clause()?)
        } else {
            None
        };

        // Optional trailing `RETURN var`.
        let return_var = if *self.peek() == Token::Return {
            self.advance(); // RETURN
            Some(self.expect_ident()?)
        } else {
            None
        };

        Ok(MergeClause {
            var,
            label,
            props,
            on_create,
            on_match,
            return_var,
        })
    }

    /// True when the current token is the identifier `ON` (case-insensitive,
    /// not a reserved keyword) and the next token is `expected` (e.g. CREATE
    /// or MATCH). Used to recognise `ON CREATE` / `ON MATCH` after MERGE.
    fn peek_is_on_then(&self, expected: &Token) -> bool {
        matches!(self.peek(), Token::Ident(s) if s.eq_ignore_ascii_case("on"))
            && self.peek_ahead(1) == expected
    }

    // ── MATCH clause ─────────────────────────────────────────────────────────

    /// Parses `MATCH pattern1, pattern2, ...`.
    fn parse_match_clause(&mut self) -> crate::Result<MatchClause> {
        self.expect(&Token::Match)?;

        // Optional path-variable binding: `MATCH p = (…)`. Recognised by the
        // `IDENT =` prefix; without it, the clause has no bound path.
        let path_var = if matches!(self.peek(), Token::Ident(_)) && *self.peek_ahead(1) == Token::Eq
        {
            let name = self.expect_ident()?;
            self.expect(&Token::Eq)?;
            Some(name)
        } else {
            None
        };

        let mut patterns = Vec::with_capacity(4);
        patterns.push(self.parse_path_pattern()?);
        while *self.peek() == Token::Comma {
            self.advance(); // consume ','
            patterns.push(self.parse_path_pattern()?);
        }

        Ok(MatchClause { patterns, path_var })
    }

    /// Parses a single path pattern starting at a node pattern and followed
    /// by zero or more alternating edge + node hops.
    fn parse_path_pattern(&mut self) -> crate::Result<PathPattern> {
        let start = self.parse_node_pattern()?;
        let mut hops = Vec::with_capacity(4);

        while let Token::Minus | Token::ArrowLeft | Token::ArrowRight = self.peek() {
            let (edge, node) = self.parse_edge_and_node()?;
            hops.push((edge, node));
        }

        Ok(PathPattern { start, hops })
    }

    /// Parses `(` [var] [:Label]* [{props}] `)`.
    fn parse_node_pattern(&mut self) -> crate::Result<NodePattern> {
        self.expect(&Token::LParen)?;

        // Optional variable name.
        let var = if let Token::Ident(_) = self.peek() {
            Some(self.expect_ident()?)
        } else {
            None
        };

        // Zero or more `:Label` labels.
        let mut labels = Vec::with_capacity(4);
        while *self.peek() == Token::Colon {
            self.advance(); // consume ':'
            labels.push(self.expect_ident()?);
        }

        // Multi-label nodes are not yet supported.
        if labels.len() > 1 {
            return Err(Self::unsupported_feature("multi-label nodes"));
        }

        // Optional inline properties `{key: literal, ...}`.
        let props = if *self.peek() == Token::LBrace {
            self.parse_inline_props()?
        } else {
            Vec::new()
        };

        self.expect(&Token::RParen)?;

        Ok(NodePattern { var, labels, props })
    }

    /// Parses `{key: literal, key: literal, ...}`.
    fn parse_inline_props(&mut self) -> crate::Result<Vec<(String, Literal)>> {
        self.expect(&Token::LBrace)?;
        let mut props = Vec::with_capacity(4);

        if *self.peek() != Token::RBrace {
            props.push(self.parse_prop_entry()?);
            while *self.peek() == Token::Comma {
                self.advance(); // consume ','
                props.push(self.parse_prop_entry()?);
            }
        }

        self.expect(&Token::RBrace)?;
        Ok(props)
    }

    /// Parses `{ key: expr, key: expr }` for CREATE patterns.
    fn parse_create_props(&mut self) -> crate::Result<Vec<(String, Expr)>> {
        self.expect(&Token::LBrace)?;
        let mut props = Vec::with_capacity(4);
        if *self.peek() != Token::RBrace {
            props.push(self.parse_create_prop_entry()?);
            while *self.peek() == Token::Comma {
                self.advance();
                props.push(self.parse_create_prop_entry()?);
            }
        }
        self.expect(&Token::RBrace)?;
        Ok(props)
    }

    /// Parses a single `key: expr` property entry for CREATE patterns.
    fn parse_create_prop_entry(&mut self) -> crate::Result<(String, Expr)> {
        let key = self.expect_ident()?;
        self.expect(&Token::Colon)?;
        self.enter_expr()?;
        let value = self.parse_expr()?;
        self.exit_expr();
        Ok((key, value))
    }

    /// Parses a single `key: literal` property entry.
    fn parse_prop_entry(&mut self) -> crate::Result<(String, Literal)> {
        let key = self.expect_ident()?;
        self.expect(&Token::Colon)?;
        let value = self.parse_literal_value()?;
        Ok((key, value))
    }

    /// Parses a literal value: integer, float, string, boolean, or NULL.
    fn parse_literal_value(&mut self) -> crate::Result<Literal> {
        match self.peek() {
            Token::IntLit(v) => {
                let v = *v;
                self.advance();
                Ok(Literal::Int(v))
            }
            Token::FloatLit(v) => {
                let v = *v;
                self.advance();
                Ok(Literal::Float(v))
            }
            Token::StringLit(s) => {
                let s = s.clone();
                self.advance();
                Ok(Literal::Str(s))
            }
            Token::BoolLit(b) => {
                let b = *b;
                self.advance();
                Ok(Literal::Bool(b))
            }
            Token::Null => {
                self.advance();
                Ok(Literal::Null)
            }
            other => Err(self.syntax_error(format!("expected literal value, found {other}"))),
        }
    }

    // ── Edge patterns ────────────────────────────────────────────────────────

    /// Parses one edge pattern followed by the destination node pattern.
    ///
    /// Handles all three syntactic forms:
    /// - `-[content]->` (outgoing)
    /// - `<-[content]-` (incoming)
    /// - `-[content]-` (both/undirected)
    fn parse_edge_and_node(&mut self) -> crate::Result<(EdgePattern, NodePattern)> {
        match self.peek() {
            Token::ArrowLeft => {
                // `<-[content]-`
                self.advance(); // consume `<-`
                if *self.peek() != Token::LBracket {
                    let found = self.peek().clone();
                    return Err(self.syntax_error(format!(
                        "edge pattern requires brackets, e.g. <-[r:TYPE]- \
                         (found `{found}` where `[` was expected)"
                    )));
                }
                self.expect(&Token::LBracket)?;
                let EdgeBracketContent {
                    var,
                    labels,
                    props,
                    length,
                } = self.parse_edge_bracket_content()?;
                self.expect(&Token::RBracket)?;
                self.expect(&Token::Minus)?;
                let node = self.parse_node_pattern()?;
                Ok((
                    EdgePattern {
                        var,
                        labels,
                        props,
                        direction: AstDirection::Incoming,
                        length,
                    },
                    node,
                ))
            }
            Token::Minus => {
                // `-[content]->` or `-[content]-`
                self.advance(); // consume `-`
                if *self.peek() != Token::LBracket {
                    let found = self.peek().clone();
                    return Err(self.syntax_error(format!(
                        "edge pattern requires brackets, e.g. -[r:TYPE]-> \
                         (found `{found}` where `[` was expected)"
                    )));
                }
                self.expect(&Token::LBracket)?;
                let EdgeBracketContent {
                    var,
                    labels,
                    props,
                    length,
                } = self.parse_edge_bracket_content()?;
                self.expect(&Token::RBracket)?;

                let direction = match self.peek() {
                    Token::ArrowRight => {
                        self.advance(); // consume `->`
                        AstDirection::Outgoing
                    }
                    Token::Minus => {
                        self.advance(); // consume `-`
                        AstDirection::Both
                    }
                    other => {
                        return Err(self.syntax_error(format!(
                            "expected -> or - after edge bracket, found {other}"
                        )));
                    }
                };
                let node = self.parse_node_pattern()?;
                Ok((
                    EdgePattern {
                        var,
                        labels,
                        props,
                        direction,
                        length,
                    },
                    node,
                ))
            }
            Token::ArrowRight => Err(self.syntax_error(
                "edge pattern requires brackets, e.g. -[r:TYPE]-> \
                     (found `->` without preceding `[...]`)"
                    .to_string(),
            )),
            other => Err(self.syntax_error(format!("expected edge pattern, found {other}"))),
        }
    }

    /// Parses the content inside `[...]` of an edge pattern.
    ///
    /// Grammar: `[var? :Label* {props}? *range?]`
    fn parse_edge_bracket_content(&mut self) -> crate::Result<EdgeBracketContent> {
        // Optional variable name.
        let var = if let Token::Ident(_) = self.peek() {
            Some(self.expect_ident()?)
        } else {
            None
        };

        // Zero or more `:Label` labels.
        let mut labels = Vec::with_capacity(4);
        while *self.peek() == Token::Colon {
            self.advance(); // consume ':'
            labels.push(self.expect_ident()?);
        }

        // Optional inline properties.
        let props = if *self.peek() == Token::LBrace {
            self.parse_inline_props()?
        } else {
            Vec::new()
        };

        // Variable-length paths (`*`, `*1..5`, etc.).
        let length = if *self.peek() == Token::Star {
            self.advance(); // consume `*`
            let min = if let Token::IntLit(n) = self.peek() {
                let n = *n;
                self.advance();
                let v = u32::try_from(n).map_err(|_| {
                    self.syntax_error(format!(
                        "variable-length min bound must be non-negative, got {n}"
                    ))
                })?;
                Some(v)
            } else {
                None
            };
            let max = if *self.peek() == Token::DotDot {
                self.advance(); // consume `..`
                if let Token::IntLit(n) = self.peek() {
                    let n = *n;
                    self.advance();
                    let v = u32::try_from(n).map_err(|_| {
                        self.syntax_error(format!(
                            "variable-length max bound must be non-negative, got {n}"
                        ))
                    })?;
                    Some(v)
                } else {
                    None
                }
            } else if min.is_some() {
                // `*3` without `..` means exactly 3 hops: min=3, max=3
                min
            } else {
                None
            };
            EdgeLength::Variable { min, max }
        } else {
            EdgeLength::Fixed
        };

        Ok(EdgeBracketContent {
            var,
            labels,
            props,
            length,
        })
    }

    // ── WHERE clause ─────────────────────────────────────────────────────────

    /// Parses the optional `WHERE expr` clause.
    fn parse_where_clause(&mut self) -> crate::Result<Option<WhereClause>> {
        if *self.peek() != Token::Where {
            return Ok(None);
        }
        self.advance(); // consume WHERE
        let predicate = self.parse_expr()?;
        Ok(Some(WhereClause { predicate }))
    }

    // ── RETURN clause ────────────────────────────────────────────────────────

    /// Parses `RETURN [DISTINCT] expr [AS alias], ...`.
    fn parse_return_clause(&mut self) -> crate::Result<ReturnClause> {
        self.expect(&Token::Return)?;
        let (distinct, items) = self.parse_return_items_after_keyword()?;
        Ok(ReturnClause { distinct, items })
    }

    /// Parses the body of a `RETURN` clause once the `Token::Return` keyword
    /// has already been consumed: an optional `DISTINCT` flag followed by a
    /// comma-separated list of return items.
    ///
    /// Shared between `parse_return_clause` (for `RETURN` after a MATCH)
    /// and `parse_const_return_query` (for `RETURN` as a root statement).
    fn parse_return_items_after_keyword(&mut self) -> crate::Result<(bool, Vec<ReturnItem>)> {
        let distinct = if *self.peek() == Token::Distinct {
            self.advance();
            true
        } else {
            false
        };

        let mut items = Vec::with_capacity(4);
        items.push(self.parse_return_item()?);
        while *self.peek() == Token::Comma {
            self.advance(); // consume ','
            items.push(self.parse_return_item()?);
        }

        Ok((distinct, items))
    }

    /// Parses a single `expr [AS alias]` return item.
    fn parse_return_item(&mut self) -> crate::Result<ReturnItem> {
        let expr = self.parse_expr()?;
        let alias = if *self.peek() == Token::As {
            self.advance(); // consume AS
            Some(self.expect_ident()?)
        } else {
            None
        };
        Ok(ReturnItem { expr, alias })
    }

    // ── GROUP BY clause ──────────────────────────────────────────────────────

    /// Parses the optional `GROUP BY expr, ...` clause.
    fn parse_group_by_clause(&mut self) -> crate::Result<Option<super::ast::GroupByClause>> {
        if *self.peek() != Token::Group {
            return Ok(None);
        }
        self.advance(); // consume GROUP
        self.expect(&Token::By)?;

        let mut keys = vec![{
            self.enter_expr()?;
            let e = self.parse_expr()?;
            self.exit_expr();
            e
        }];
        while *self.peek() == Token::Comma {
            self.advance();
            self.enter_expr()?;
            let e = self.parse_expr()?;
            self.exit_expr();
            keys.push(e);
        }
        Ok(Some(super::ast::GroupByClause { keys }))
    }

    // ── ORDER BY clause ──────────────────────────────────────────────────────

    /// Parses the optional `ORDER BY expr [ASC|DESC], ...` clause.
    fn parse_order_by_clause(&mut self) -> crate::Result<Option<OrderByClause>> {
        if *self.peek() != Token::Order {
            return Ok(None);
        }
        self.advance(); // consume ORDER
        self.expect(&Token::By)?;

        let mut items = Vec::with_capacity(4);
        items.push(self.parse_order_item()?);
        while *self.peek() == Token::Comma {
            self.advance(); // consume ','
            items.push(self.parse_order_item()?);
        }

        Ok(Some(OrderByClause { items }))
    }

    /// Parses a single `expr [ASC|DESC]` order item.
    fn parse_order_item(&mut self) -> crate::Result<OrderItem> {
        let expr = self.parse_expr()?;
        let ascending = match self.peek() {
            Token::Desc => {
                self.advance();
                false
            }
            Token::Asc => {
                self.advance();
                true
            }
            _ => true,
        };
        Ok(OrderItem { expr, ascending })
    }

    // ── LIMIT clause ─────────────────────────────────────────────────────────

    /// Parses the optional `LIMIT integer` clause.
    fn parse_limit_clause(&mut self) -> crate::Result<Option<LimitClause>> {
        if *self.peek() != Token::Limit {
            return Ok(None);
        }
        self.advance(); // consume LIMIT
        let raw = self.expect_int()?;
        let count = u64::try_from(raw)
            .map_err(|_| self.syntax_error("LIMIT value must be a non-negative integer"))?;
        Ok(Some(LimitClause { count }))
    }

    /// Parses `UNWIND expr AS var` if the current token is `UNWIND`.
    /// Returns `Ok(None)` if the current token is not `UNWIND`.
    fn parse_unwind_clause(&mut self) -> crate::Result<Option<super::ast::UnwindClause>> {
        if *self.peek() != Token::Unwind {
            return Ok(None);
        }
        self.parse_unwind_body().map(Some)
    }

    /// Parses the body of an `UNWIND expr AS var` clause. The caller must
    /// have verified that `peek()` is `Token::Unwind`; this function
    /// consumes the `UNWIND` keyword and everything after it.
    fn parse_unwind_body(&mut self) -> crate::Result<super::ast::UnwindClause> {
        debug_assert_eq!(*self.peek(), Token::Unwind);
        self.advance(); // consume UNWIND

        let expr = self.parse_expr()?;

        if *self.peek() != Token::As {
            return Err(self.syntax_error("expected AS after UNWIND expression"));
        }
        self.advance(); // consume AS

        let var = self.expect_ident()?;

        Ok(super::ast::UnwindClause { expr, var })
    }

    // ── Expression parser with precedence ────────────────────────────────────

    /// Entry point for expression parsing — delegates to the lowest-precedence
    /// level (`OR`).
    fn parse_expr(&mut self) -> crate::Result<Expr> {
        self.parse_or()
    }

    /// Parses `parse_and() (OR parse_and())*`.
    fn parse_or(&mut self) -> crate::Result<Expr> {
        let mut left = self.parse_and()?;
        while *self.peek() == Token::Or {
            self.advance();
            let right = self.parse_and()?;
            left = Expr::BinaryOp {
                left: Box::new(left),
                op: BinOp::Or,
                right: Box::new(right),
            };
        }
        Ok(left)
    }

    /// Parses `parse_not() (AND parse_not())*`.
    fn parse_and(&mut self) -> crate::Result<Expr> {
        let mut left = self.parse_not()?;
        while *self.peek() == Token::And {
            self.advance();
            let right = self.parse_not()?;
            left = Expr::BinaryOp {
                left: Box::new(left),
                op: BinOp::And,
                right: Box::new(right),
            };
        }
        Ok(left)
    }

    /// Parses `NOT parse_not() | parse_comparison()`.
    fn parse_not(&mut self) -> crate::Result<Expr> {
        if *self.peek() == Token::Not {
            self.advance();
            self.enter_expr()?;
            let expr = self.parse_not()?;
            self.exit_expr();
            return Ok(Expr::UnaryOp {
                op: UnaryOp::Not,
                expr: Box::new(expr),
            });
        }
        self.parse_comparison()
    }

    /// Parses comparisons and `IS [NOT] NULL`.
    ///
    /// Grammar:
    /// ```text
    /// parse_addition() ((= | <> | < | > | <= | >=) parse_addition())?
    /// | parse_addition() IS [NOT] NULL
    /// ```
    fn parse_comparison(&mut self) -> crate::Result<Expr> {
        let left = self.parse_addition()?;

        // IS [NOT] NULL
        if *self.peek() == Token::Is {
            self.advance(); // consume IS
            let negated = if *self.peek() == Token::Not {
                self.advance(); // consume NOT
                true
            } else {
                false
            };
            self.expect(&Token::Null)?;
            return Ok(Expr::IsNull {
                expr: Box::new(left),
                negated,
            });
        }

        // Cypher string/list operators: STARTS WITH, ENDS WITH, CONTAINS, IN.
        // STARTS / ENDS / CONTAINS / IN tokenise as Ident(_) (not reserved
        // keywords), but WITH is reserved (Token::With), so the second token of
        // STARTS WITH / ENDS WITH is matched via peek_is_with_keyword.
        // `STARTS WITH expr`
        if matches!(self.peek(), Token::Ident(s) if s.eq_ignore_ascii_case("STARTS"))
            && self.peek_is_with_keyword(1)
        {
            self.advance(); // consume STARTS
            self.advance(); // consume WITH
            let right = self.parse_addition()?;
            return Ok(Expr::BinaryOp {
                left: Box::new(left),
                op: BinOp::StartsWith,
                right: Box::new(right),
            });
        }

        // `ENDS WITH expr`
        if matches!(self.peek(), Token::Ident(s) if s.eq_ignore_ascii_case("ENDS"))
            && self.peek_is_with_keyword(1)
        {
            self.advance(); // consume ENDS
            self.advance(); // consume WITH
            let right = self.parse_addition()?;
            return Ok(Expr::BinaryOp {
                left: Box::new(left),
                op: BinOp::EndsWith,
                right: Box::new(right),
            });
        }

        // `CONTAINS expr`
        if matches!(self.peek(), Token::Ident(s) if s.eq_ignore_ascii_case("CONTAINS")) {
            self.advance(); // consume CONTAINS
            let right = self.parse_addition()?;
            return Ok(Expr::BinaryOp {
                left: Box::new(left),
                op: BinOp::Contains,
                right: Box::new(right),
            });
        }

        // `IN [list]`
        if matches!(self.peek(), Token::Ident(s) if s.eq_ignore_ascii_case("IN")) {
            self.advance(); // consume IN
            if *self.peek() != Token::LBracket {
                let span = self.current_span();
                return Err(Error::GqlSyntaxError {
                    line: span.line,
                    col: span.col,
                    message: "IN operator requires a literal list [...]; \
                              use IN ['value1', 'value2'] or IN [1, 2]"
                        .into(),
                });
            }
            let right = self.parse_list_literal()?;
            return Ok(Expr::BinaryOp {
                left: Box::new(left),
                op: BinOp::In,
                right: Box::new(right),
            });
        }

        let op = match self.peek() {
            Token::Eq => BinOp::Eq,
            Token::NotEq => BinOp::NotEq,
            Token::Lt => BinOp::Lt,
            Token::Gt => BinOp::Gt,
            Token::LtEq => BinOp::LtEq,
            Token::GtEq => BinOp::GtEq,
            _ => return Ok(left),
        };
        self.advance();
        let right = self.parse_addition()?;
        Ok(Expr::BinaryOp {
            left: Box::new(left),
            op,
            right: Box::new(right),
        })
    }

    /// Parses a list literal `[lit, lit, ...]`.
    ///
    /// Used for the right-hand side of `IN` predicates.
    ///
    /// Accepts arbitrary expressions as elements. Returns
    /// `Expr::Literal(Literal::List(_))` when every element is itself a
    /// literal (for backwards compatibility with existing tests and the
    /// `IN [1, 2, 3]` idiom), or `Expr::ListLit(_)` when any element is a
    /// non-literal expression (e.g. a variable reference).
    fn parse_list_literal(&mut self) -> crate::Result<Expr> {
        self.expect(&Token::LBracket)?;
        let mut items: Vec<Expr> = Vec::with_capacity(4);

        if *self.peek() != Token::RBracket {
            self.enter_expr()?;
            let first = self.parse_expr();
            self.exit_expr();
            items.push(first?);
            while *self.peek() == Token::Comma {
                self.advance(); // consume ','
                self.enter_expr()?;
                let next = self.parse_expr();
                self.exit_expr();
                items.push(next?);
            }
        }

        self.expect(&Token::RBracket)?;

        // Try to preserve `Literal::List` when every element is itself a
        // literal — several existing tests and the IN operator depend on it.
        let all_literal = items.iter().all(|e| matches!(e, Expr::Literal(_)));
        if all_literal {
            let lits: Vec<Literal> = items
                .into_iter()
                .map(|e| match e {
                    Expr::Literal(l) => l,
                    _ => unreachable!("checked by all_literal above"),
                })
                .collect();
            Ok(Expr::Literal(Literal::List(lits)))
        } else {
            Ok(Expr::ListLit(items))
        }
    }

    /// Parses `parse_multiplication() ((+ | -) parse_multiplication())*`.
    fn parse_addition(&mut self) -> crate::Result<Expr> {
        let mut left = self.parse_multiplication()?;
        loop {
            let op = match self.peek() {
                Token::Plus => BinOp::Add,
                Token::Minus => BinOp::Sub,
                _ => break,
            };
            self.advance();
            let right = self.parse_multiplication()?;
            left = Expr::BinaryOp {
                left: Box::new(left),
                op,
                right: Box::new(right),
            };
        }
        Ok(left)
    }

    /// Parses `parse_unary() ((* | /) parse_unary())*`.
    fn parse_multiplication(&mut self) -> crate::Result<Expr> {
        let mut left = self.parse_unary()?;
        loop {
            let op = match self.peek() {
                Token::Star => BinOp::Mul,
                Token::Slash => BinOp::Div,
                _ => break,
            };
            self.advance();
            let right = self.parse_unary()?;
            left = Expr::BinaryOp {
                left: Box::new(left),
                op,
                right: Box::new(right),
            };
        }
        Ok(left)
    }

    /// Parses `- parse_unary() | parse_primary()`.
    ///
    /// Postfix subscripts `primary '[' expr ']'` are parsed by
    /// `parse_primary` itself for forms that produce list-typed values
    /// (identifier, function call, parenthesised expression, list literal),
    /// so the common path stays a tail call to `parse_primary` and adds
    /// no stack frame. This preserves the `MAX_EXPR_DEPTH = 128` stack
    /// guard used by `aggregate_nesting_at_limit_is_accepted` and
    /// `deeply_nested_parens_returns_error`.
    fn parse_unary(&mut self) -> crate::Result<Expr> {
        if *self.peek() == Token::Minus {
            self.advance();
            self.enter_expr()?;
            let expr = self.parse_unary()?;
            self.exit_expr();
            return Ok(Expr::UnaryOp {
                op: UnaryOp::Neg,
                expr: Box::new(expr),
            });
        }
        self.parse_primary()
    }

    /// Parses an identifier primary: function call, property access, or
    /// bare variable reference, plus any trailing postfix subscript chain.
    fn parse_primary_ident(&mut self) -> crate::Result<Expr> {
        let name = self.expect_ident()?;

        if *self.peek() == Token::LParen {
            return self.parse_function_call(&name);
        }

        let expr = if *self.peek() == Token::Dot {
            self.advance(); // consume '.'
            let prop = self.expect_ident()?;
            Expr::PropAccess { var: name, prop }
        } else {
            Expr::Var(name)
        };

        self.parse_primary_suffix(expr)
    }

    /// Parses the arguments and closing paren of a function call.
    /// Caller must have consumed the function name and verified `peek()`
    /// is `Token::LParen`. Handles the special `shortestPath((a)-[*]->(b))`
    /// pattern form in addition to the generic argument list.
    fn parse_function_call(&mut self, name: &str) -> crate::Result<Expr> {
        let name_lower = name.to_ascii_lowercase();

        // List predicates `ALL`/`ANY`/`NONE`/`SINGLE` take the special form
        // `kind(var IN list WHERE predicate)`, not a generic argument list.
        if let Some(kind) = list_pred_kind(&name_lower) {
            return self.parse_list_predicate(kind);
        }

        // Cypher-style shortestPath((a)-[*..N]->(b)) — argument is a path
        // pattern, not a generic expression list.
        if name_lower == "shortestpath" {
            self.advance(); // consume '('
            if *self.peek() == Token::LParen {
                let pattern = self.parse_path_pattern()?;
                self.expect(&Token::RParen)?;
                let expr = Expr::ShortestPath {
                    pattern: Box::new(pattern),
                };
                return self.parse_primary_suffix(expr);
            }
            // Legacy two-arg form `shortestPath(a, b)`.
            self.enter_expr()?;
            let mut args = vec![self.parse_expr()?];
            while *self.peek() == Token::Comma {
                self.advance();
                args.push(self.parse_expr()?);
            }
            self.exit_expr();
            self.expect(&Token::RParen)?;
            let expr = Expr::FunctionCall {
                name: name_lower,
                args,
            };
            return self.parse_primary_suffix(expr);
        }

        // Generic scalar function call: `name(arg, ...)`.
        self.advance(); // consume '('
        let mut args = Vec::new();
        if *self.peek() != Token::RParen {
            self.enter_expr()?;
            let first = self.parse_expr();
            self.exit_expr();
            args.push(first?);
            while *self.peek() == Token::Comma {
                self.advance();
                self.enter_expr()?;
                let next = self.parse_expr();
                self.exit_expr();
                args.push(next?);
            }
        }
        self.expect(&Token::RParen)?;
        let expr = Expr::FunctionCall {
            name: name_lower,
            args,
        };
        self.parse_primary_suffix(expr)
    }

    /// Parses a list predicate `kind(var IN list WHERE predicate)`.
    ///
    /// Caller (`parse_function_call`) has consumed the predicate keyword and
    /// verified `peek()` is `Token::LParen`. `IN` is a bare (non-reserved)
    /// identifier keyword, matched case-insensitively like elsewhere in the
    /// grammar; `WHERE` is the reserved `Token::Where`.
    fn parse_list_predicate(&mut self, kind: ListPredKind) -> crate::Result<Expr> {
        self.expect(&Token::LParen)?;
        let var = self.expect_ident()?;
        if !matches!(self.peek(), Token::Ident(s) if s.eq_ignore_ascii_case("IN")) {
            return Err(self.syntax_error("expected IN in list predicate"));
        }
        self.advance(); // consume IN
        self.enter_expr()?;
        let list = self.parse_expr();
        self.exit_expr();
        let list = list?;
        self.expect(&Token::Where)?;
        self.enter_expr()?;
        let predicate = self.parse_expr();
        self.exit_expr();
        let predicate = predicate?;
        self.expect(&Token::RParen)?;
        let expr = Expr::ListPredicate {
            kind,
            var,
            list: Box::new(list),
            predicate: Box::new(predicate),
        };
        self.parse_primary_suffix(expr)
    }

    /// Applies the postfix suffix grammar (currently just subscript chain)
    /// to a primary `expr`.
    fn parse_primary_suffix(&mut self, expr: Expr) -> crate::Result<Expr> {
        if *self.peek() == Token::LBracket {
            return self.parse_subscript_chain(expr);
        }
        Ok(expr)
    }

    /// Consumes one or more postfix `[index]` subscripts, wrapping the
    /// accumulated expression in `Expr::Subscript`. Caller must have already
    /// checked that `peek()` is `Token::LBracket`.
    fn parse_subscript_chain(&mut self, mut expr: Expr) -> crate::Result<Expr> {
        while *self.peek() == Token::LBracket {
            self.advance(); // consume '['
            self.enter_expr()?;
            let index = self.parse_expr();
            self.exit_expr();
            let index = index?;
            self.expect(&Token::RBracket)?;
            expr = Expr::Subscript {
                list: Box::new(expr),
                index: Box::new(index),
            };
        }
        Ok(expr)
    }

    /// Parses the highest-precedence expressions: literals, variables,
    /// property accesses, aggregation calls, and parenthesised expressions.
    fn parse_primary(&mut self) -> crate::Result<Expr> {
        match self.peek() {
            // Aggregation functions.
            Token::Count => self.parse_aggregate(AggFunc::Count),
            Token::Sum => self.parse_aggregate(AggFunc::Sum),
            Token::Avg => self.parse_aggregate(AggFunc::Avg),
            Token::Min => self.parse_aggregate(AggFunc::Min),
            Token::Max => self.parse_aggregate(AggFunc::Max),
            Token::Collect => self.parse_aggregate(AggFunc::Collect),

            // Literals.
            Token::IntLit(v) => {
                let v = *v;
                self.advance();
                Ok(Expr::Literal(Literal::Int(v)))
            }
            Token::FloatLit(v) => {
                let v = *v;
                self.advance();
                Ok(Expr::Literal(Literal::Float(v)))
            }
            Token::StringLit(s) => {
                let s = s.clone();
                self.advance();
                Ok(Expr::Literal(Literal::Str(s)))
            }
            Token::BoolLit(b) => {
                let b = *b;
                self.advance();
                Ok(Expr::Literal(Literal::Bool(b)))
            }
            Token::Null => {
                self.advance();
                Ok(Expr::Literal(Literal::Null))
            }

            // Identifier: either `func(args)` (FunctionCall), `var.prop` (PropAccess),
            // or bare `var` (Var). Delegated to a helper to keep `parse_primary`
            // under the clippy `too_many_lines` limit.
            Token::Ident(_) => self.parse_primary_ident(),

            // List literal: [expr, expr, ...]
            Token::LBracket => {
                let lst = self.parse_list_literal()?;
                self.parse_primary_suffix(lst)
            }

            // Parenthesised expression.
            Token::LParen => {
                self.advance(); // consume '('
                self.enter_expr()?;
                let expr = self.parse_expr()?;
                self.exit_expr();
                self.expect(&Token::RParen)?;
                Ok(expr)
            }

            // Parameter placeholder: `$name` (named) or `$1` (positional).
            // The lexer emits `Dollar` unconditionally; this arm validates
            // that an `Ident` or positive `IntLit` follows and produces the
            // matching `Expr::ParamRef`. Resolution to a literal value is
            // deferred to `param_substitution::apply` (cycle 6).
            Token::Dollar => self.parse_param_ref(),

            _ => Err(self.syntax_error(format!("unexpected token in expression: {}", self.peek()))),
        }
    }

    /// Parses a parameter placeholder. Caller has peeked `Token::Dollar`.
    ///
    /// Surface forms:
    /// - `$name` — produces `Expr::ParamRef(ParamRef::Named(name))`.
    /// - `$<n>` where `n >= 1` — produces `Expr::ParamRef(ParamRef::Positional(n))`.
    ///
    /// Errors:
    /// - `$0` is rejected: positional indices are 1-based per Bolt spec.
    /// - Negative literals (`$-1`) are not possible here — the lexer emits
    ///   `Minus` and `IntLit` as separate tokens; this arm only sees the
    ///   following token, which would be `Minus`, and yields a generic
    ///   "parameter name or positional index after '$'" error.
    /// - Anything else after `$` produces the same generic error.
    fn parse_param_ref(&mut self) -> crate::Result<Expr> {
        self.advance(); // consume Dollar
        match self.peek().clone() {
            Token::Ident(name) => {
                self.advance();
                Ok(Expr::ParamRef(ParamRef::Named(name)))
            }
            Token::IntLit(n) if n >= 1 => {
                // SAFETY(cast): IntLit holds i64 but positional indices are
                // capped at the maximum number of positional parameters a
                // driver can sensibly send (well under u32::MAX). Larger
                // values are not rejected here; the resolver will surface
                // a MissingPositionalParameter error if the map has no
                // matching key.
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let idx = n as u32;
                self.advance();
                Ok(Expr::ParamRef(ParamRef::Positional(idx)))
            }
            Token::IntLit(_) => Err(self.syntax_error(
                "positional parameter index must be >= 1 (Bolt uses 1-based indexing)",
            )),
            other => Err(self.syntax_error(format!(
                "expected parameter name or positional index after '$', found {other}"
            ))),
        }
    }

    /// Parses an aggregation call: `FUNC(expr)` or `COUNT(*)`.
    fn parse_aggregate(&mut self, func: AggFunc) -> crate::Result<Expr> {
        self.advance(); // consume the function keyword
        self.expect(&Token::LParen)?;

        // COUNT(*) special case.
        if func == AggFunc::Count && *self.peek() == Token::Star {
            self.advance(); // consume '*'
            self.expect(&Token::RParen)?;
            return Ok(Expr::Aggregate { func, arg: None });
        }

        // Guard against deeply nested aggregate calls (e.g. COUNT(COUNT(COUNT(...)))).
        self.enter_expr()?;
        let arg_result = self.parse_expr();
        self.exit_expr();
        let arg = arg_result?;
        self.expect(&Token::RParen)?;
        Ok(Expr::Aggregate {
            func,
            arg: Some(Box::new(arg)),
        })
    }

    // ── Pipeline parser (WITH clause) ────────────────────────────────────────

    /// Parses a pipeline statement once the first MATCH stage has been
    /// consumed and the next token is known to be `WITH`.
    ///
    /// `leading_unwind` preserves UNWIND that appeared before the first MATCH.
    /// Once inside a pipeline, further UNWIND clauses are treated as explicit
    /// `PipelineStage::Unwind` entries, not leading clauses.
    fn parse_pipeline_after_match(
        &mut self,
        leading_unwind: Option<super::ast::UnwindClause>,
        match_clause: MatchClause,
        match_where: Option<WhereClause>,
    ) -> crate::Result<GqlStatement> {
        use super::ast::{PipelineQuery, PipelineStage};

        let mut stages: Vec<PipelineStage> = Vec::with_capacity(4);
        if let Some(u) = leading_unwind {
            stages.push(PipelineStage::Unwind(u));
        }
        stages.push(PipelineStage::Match {
            clause: match_clause,
            where_clause: match_where,
        });

        // First WITH is mandatory (caller checked).
        stages.push(PipelineStage::With(self.parse_with_clause()?));

        // Zero or more additional WITH / UNWIND stages.
        loop {
            match self.peek() {
                Token::With => {
                    stages.push(PipelineStage::With(self.parse_with_clause()?));
                }
                Token::Unwind => {
                    let u = self.parse_unwind_body()?;
                    stages.push(PipelineStage::Unwind(u));
                }
                _ => break,
            }
        }

        let terminal = self.parse_pipeline_terminal()?;
        if *self.peek() != Token::Eof {
            return Err(self.syntax_error("unexpected tokens after pipeline terminal"));
        }

        Ok(GqlStatement::Pipeline(PipelineQuery { stages, terminal }))
    }

    /// Parses `WITH [DISTINCT] item (, item)* [WHERE ...] [ORDER BY ...]
    /// [SKIP n] [LIMIT n]`. Caller must have peeked `Token::With`.
    fn parse_with_clause(&mut self) -> crate::Result<super::ast::WithClause> {
        self.expect(&Token::With)?;

        let distinct = if *self.peek() == Token::Distinct {
            self.advance();
            true
        } else {
            false
        };

        let mut items = Vec::with_capacity(4);
        items.push(self.parse_return_item()?);
        while *self.peek() == Token::Comma {
            self.advance();
            items.push(self.parse_return_item()?);
        }

        let where_clause = self.parse_where_clause()?;
        let order_by = self.parse_order_by_clause()?;
        let skip = self.parse_skip_clause()?;
        let limit = self.parse_limit_clause()?;

        Ok(super::ast::WithClause {
            distinct,
            items,
            where_clause,
            order_by,
            skip,
            limit,
        })
    }

    /// Parses the optional `SKIP integer` clause.
    fn parse_skip_clause(&mut self) -> crate::Result<Option<super::ast::SkipClause>> {
        if *self.peek() != Token::Skip {
            return Ok(None);
        }
        self.advance();
        let raw = self.expect_int()?;
        let count = u64::try_from(raw)
            .map_err(|_| self.syntax_error("SKIP value must be a non-negative integer"))?;
        Ok(Some(super::ast::SkipClause { count }))
    }

    /// Parses the terminal clause of a pipeline:
    /// `RETURN ... | SET ... | CREATE ... | DELETE ...`.
    fn parse_pipeline_terminal(&mut self) -> crate::Result<super::ast::PipelineTerminal> {
        use super::ast::PipelineTerminal;

        match self.peek() {
            Token::Return => {
                let clause = self.parse_return_clause()?;
                let order_by = self.parse_order_by_clause()?;
                let skip = self.parse_skip_clause()?;
                let limit = self.parse_limit_clause()?;
                Ok(PipelineTerminal::Return {
                    clause,
                    order_by,
                    skip,
                    limit,
                })
            }
            Token::Set => {
                let set = self.parse_set_clause()?;
                Ok(PipelineTerminal::Set(set))
            }
            Token::Create => {
                let create = self.parse_create_clause()?;
                Ok(PipelineTerminal::Create(create))
            }
            Token::Delete | Token::Detach => {
                let del = self.parse_delete_clause()?;
                Ok(PipelineTerminal::Delete(del))
            }
            other => Err(self.syntax_error(format!(
                "expected RETURN, SET, CREATE, or DELETE after pipeline, found {other}"
            ))),
        }
    }
}

/// Injects a parsed `UnwindClause` into a `GqlStatement` returned by
/// `parse_after_match`, which always sets `unwind_clause: None`.
fn inject_unwind(
    stmt: super::ast::GqlStatement,
    unwind: Option<super::ast::UnwindClause>,
) -> super::ast::GqlStatement {
    if unwind.is_none() {
        return stmt;
    }

    match stmt {
        super::ast::GqlStatement::Query(mut q) => {
            q.unwind_clause = unwind;
            super::ast::GqlStatement::Query(q)
        }
        super::ast::GqlStatement::Mutation(mut m) => {
            m.unwind_clause = unwind;
            super::ast::GqlStatement::Mutation(m)
        }
        // Pipeline statements carry UNWIND as an explicit stage, not as a
        // leading clause, so `inject_unwind` is a no-op for them.
        super::ast::GqlStatement::Pipeline(p) => super::ast::GqlStatement::Pipeline(p),
        // Admin statements have no UNWIND; pass through unchanged.
        super::ast::GqlStatement::Admin(a) => super::ast::GqlStatement::Admin(a),
        // ConstReturn has no MATCH/UNWIND context — it evaluates against
        // an empty binding. Pass through unchanged.
        super::ast::GqlStatement::ConstReturn(c) => super::ast::GqlStatement::ConstReturn(c),
        // DDL has no MATCH/UNWIND context; pass through unchanged.
        super::ast::GqlStatement::Ddl(d) => super::ast::GqlStatement::Ddl(d),
        // CALL is parsed by ermya-graph-cypher and never reaches the native
        // GQL `inject_unwind` path; pass through unchanged for exhaustiveness.
        super::ast::GqlStatement::Call(c) => super::ast::GqlStatement::Call(c),
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::match_wildcard_for_single_variants)]
mod tests {
    use super::*;
    use crate::gql::ast::{
        AggFunc, AstDirection, BinOp, ConstReturnQuery, CreatePattern, Expr, GqlStatement, Literal,
        MutationClause, ParamRef, UnaryOp,
    };
    use crate::gql::lexer::Lexer;

    fn parse_query(input: &str) -> crate::Result<GqlQuery> {
        let tokens = Lexer::new(input).tokenize()?;
        Parser::new(tokens).parse()
    }

    // ── Infrastructure tests ─────────────────────────────────────────────────

    #[test]
    fn parser_empty_input_fails() {
        assert!(parse_query("").is_err());
    }

    #[test]
    fn parser_garbage_fails() {
        let err = parse_query("GARBAGE INPUT").unwrap_err();
        assert!(err.to_string().contains("GQL syntax error"));
    }

    #[test]
    #[should_panic(expected = "advance() called past end of token stream")]
    fn parser_advance_past_end_panics_with_clear_message() {
        use crate::gql::token::{Span, SpannedToken, Token};
        let eof_span = Span {
            start: 0,
            end: 0,
            line: 1,
            col: 1,
        };
        let tokens = vec![SpannedToken {
            token: Token::Eof,
            span: eof_span,
        }];
        let mut p = Parser::new(tokens);
        p.advance(); // consumes Eof
        p.advance(); // past end — should panic
    }

    // ── MATCH clause tests ───────────────────────────────────────────────────

    #[test]
    fn parse_single_anon_node() {
        let q = parse_query("MATCH (a) RETURN a").unwrap();
        assert_eq!(q.match_clause.patterns.len(), 1);
        let pat = &q.match_clause.patterns[0];
        assert_eq!(pat.start.var, Some("a".into()));
        assert!(pat.start.labels.is_empty());
        assert!(pat.hops.is_empty());
    }

    #[test]
    fn parse_node_with_label() {
        let q = parse_query("MATCH (a:Person) RETURN a").unwrap();
        assert_eq!(
            q.match_clause.patterns[0].start.labels,
            vec!["Person".to_string()]
        );
    }

    #[test]
    fn multi_label_node_returns_gql_unsupported() {
        let err = parse_query("MATCH (a:Person:Employee) RETURN a").unwrap_err();
        assert!(
            matches!(err, crate::Error::GqlUnsupported(_)),
            "expected GqlUnsupported, got: {err:?}"
        );
    }

    #[test]
    fn parse_node_with_inline_property() {
        let q = parse_query("MATCH (a:Person {name: 'Alice'}) RETURN a").unwrap();
        let node = &q.match_clause.patterns[0].start;
        assert_eq!(
            node.props,
            vec![("name".into(), Literal::Str("Alice".into()))]
        );
    }

    #[test]
    fn parse_node_with_multiple_inline_props() {
        let q = parse_query("MATCH (a {age: 30, active: true}) RETURN a").unwrap();
        let props = &q.match_clause.patterns[0].start.props;
        assert_eq!(props.len(), 2);
    }

    #[test]
    fn parse_anonymous_node() {
        let q = parse_query("MATCH () RETURN 1").unwrap();
        assert!(q.match_clause.patterns[0].start.var.is_none());
    }

    // ── Edge pattern tests ───────────────────────────────────────────────────

    #[test]
    fn parse_outgoing_edge() {
        let q = parse_query("MATCH (a)-[r:KNOWS]->(b) RETURN a").unwrap();
        let pat = &q.match_clause.patterns[0];
        assert_eq!(pat.hops.len(), 1);
        let (edge, node_b) = &pat.hops[0];
        assert_eq!(edge.direction, AstDirection::Outgoing);
        assert_eq!(edge.var, Some("r".into()));
        assert_eq!(edge.labels, vec!["KNOWS".to_string()]);
        assert_eq!(node_b.var, Some("b".into()));
    }

    #[test]
    fn parse_incoming_edge() {
        let q = parse_query("MATCH (a)<-[r:KNOWS]-(b) RETURN a").unwrap();
        let (edge, _) = &q.match_clause.patterns[0].hops[0];
        assert_eq!(edge.direction, AstDirection::Incoming);
    }

    #[test]
    fn parse_undirected_edge() {
        let q = parse_query("MATCH (a)-[r]-(b) RETURN a").unwrap();
        let (edge, _) = &q.match_clause.patterns[0].hops[0];
        assert_eq!(edge.direction, AstDirection::Both);
    }

    #[test]
    fn parse_anonymous_edge() {
        let q = parse_query("MATCH (a)-[]->(b) RETURN a").unwrap();
        let (edge, _) = &q.match_clause.patterns[0].hops[0];
        assert!(edge.var.is_none());
        assert!(edge.labels.is_empty());
        assert_eq!(edge.direction, AstDirection::Outgoing);
    }

    #[test]
    fn parse_three_node_path() {
        let q = parse_query("MATCH (a)-[r1:KNOWS]->(b)-[r2:LIKES]->(c) RETURN a").unwrap();
        assert_eq!(q.match_clause.patterns[0].hops.len(), 2);
    }

    #[test]
    fn edge_bracket_content_fields_accessible() {
        let q = parse_query("MATCH (a)-[r:KNOWS {since: 2020}]->(b) RETURN a").unwrap();
        let (edge, _) = &q.match_clause.patterns[0].hops[0];
        assert_eq!(edge.var.as_deref(), Some("r"));
        assert_eq!(edge.labels, vec!["KNOWS".to_string()]);
        assert_eq!(edge.props, vec![("since".into(), Literal::Int(2020))]);
        assert_eq!(edge.length, EdgeLength::Fixed);
    }

    // ── RETURN clause tests ──────────────────────────────────────────────────

    #[test]
    fn parse_return_single_var() {
        let q = parse_query("MATCH (a) RETURN a").unwrap();
        assert_eq!(q.return_clause.items.len(), 1);
        assert_eq!(q.return_clause.items[0].expr, Expr::Var("a".into()));
        assert!(q.return_clause.items[0].alias.is_none());
    }

    #[test]
    fn parse_return_property_access() {
        let q = parse_query("MATCH (a) RETURN a.name").unwrap();
        assert_eq!(
            q.return_clause.items[0].expr,
            Expr::PropAccess {
                var: "a".into(),
                prop: "name".into()
            }
        );
    }

    #[test]
    fn parse_return_with_alias() {
        let q = parse_query("MATCH (a) RETURN a.name AS nombre").unwrap();
        assert_eq!(q.return_clause.items[0].alias, Some("nombre".into()));
    }

    #[test]
    fn parse_return_multiple_items() {
        let q = parse_query("MATCH (a) RETURN a.name, a.age").unwrap();
        assert_eq!(q.return_clause.items.len(), 2);
    }

    #[test]
    fn parse_return_count_star() {
        let q = parse_query("MATCH (a) RETURN COUNT(*)").unwrap();
        assert_eq!(
            q.return_clause.items[0].expr,
            Expr::Aggregate {
                func: AggFunc::Count,
                arg: None
            }
        );
    }

    #[test]
    fn parse_return_aggregation_with_arg() {
        let q = parse_query("MATCH (a) RETURN SUM(a.score)").unwrap();
        match &q.return_clause.items[0].expr {
            Expr::Aggregate { func, arg } => {
                assert_eq!(*func, AggFunc::Sum);
                assert!(arg.is_some());
            }
            _ => panic!("expected aggregate"),
        }
    }

    #[test]
    fn parse_return_distinct() {
        let q = parse_query("MATCH (a) RETURN DISTINCT a.name").unwrap();
        assert!(q.return_clause.distinct);
    }

    // ── WHERE, ORDER BY, LIMIT tests ─────────────────────────────────────────

    #[test]
    fn parse_where_simple_comparison() {
        let q = parse_query("MATCH (a) WHERE a.age > 30 RETURN a").unwrap();
        assert!(q.where_clause.is_some());
        match &q.where_clause.as_ref().unwrap().predicate {
            Expr::BinaryOp { op, .. } => assert_eq!(*op, BinOp::Gt),
            _ => panic!("expected BinaryOp"),
        }
    }

    #[test]
    fn parse_where_and() {
        let q = parse_query("MATCH (a) WHERE a.age > 30 AND a.name = 'Alice' RETURN a").unwrap();
        match &q.where_clause.as_ref().unwrap().predicate {
            Expr::BinaryOp { op, .. } => assert_eq!(*op, BinOp::And),
            _ => panic!("expected AND"),
        }
    }

    #[test]
    fn parse_where_is_null() {
        let q = parse_query("MATCH (a) WHERE a.email IS NULL RETURN a").unwrap();
        match &q.where_clause.as_ref().unwrap().predicate {
            Expr::IsNull { negated, .. } => assert!(!negated),
            _ => panic!("expected IsNull"),
        }
    }

    #[test]
    fn parse_where_is_not_null() {
        let q = parse_query("MATCH (a) WHERE a.email IS NOT NULL RETURN a").unwrap();
        match &q.where_clause.as_ref().unwrap().predicate {
            Expr::IsNull { negated, .. } => assert!(*negated),
            _ => panic!("expected IsNull negated"),
        }
    }

    #[test]
    fn parse_limit() {
        let q = parse_query("MATCH (a) RETURN a LIMIT 10").unwrap();
        assert_eq!(q.limit.unwrap().count, 10);
    }

    #[test]
    fn parse_order_by_asc() {
        let q = parse_query("MATCH (a) RETURN a ORDER BY a.name ASC").unwrap();
        let ob = q.order_by.unwrap();
        assert_eq!(ob.items.len(), 1);
        assert!(ob.items[0].ascending);
    }

    #[test]
    fn parse_order_by_desc() {
        let q = parse_query("MATCH (a) RETURN a ORDER BY a.age DESC").unwrap();
        assert!(!q.order_by.unwrap().items[0].ascending);
    }

    #[test]
    fn parse_order_by_default_asc() {
        let q = parse_query("MATCH (a) RETURN a ORDER BY a.name").unwrap();
        assert!(q.order_by.unwrap().items[0].ascending);
    }

    #[test]
    fn parse_order_by_multiple() {
        let q = parse_query("MATCH (a) RETURN a ORDER BY a.name ASC, a.age DESC").unwrap();
        assert_eq!(q.order_by.unwrap().items.len(), 2);
    }

    // ── Expression precedence tests ──────────────────────────────────────────

    #[test]
    fn expr_precedence_and_before_or() {
        let q = parse_query("MATCH (n) WHERE n.a = 1 OR n.b = 2 AND n.c = 3 RETURN n").unwrap();
        let pred = &q.where_clause.as_ref().unwrap().predicate;
        match pred {
            Expr::BinaryOp { op, right, .. } => {
                assert_eq!(*op, BinOp::Or);
                match right.as_ref() {
                    Expr::BinaryOp { op: inner_op, .. } => assert_eq!(*inner_op, BinOp::And),
                    _ => panic!("expected AND on right"),
                }
            }
            _ => panic!("expected OR at top"),
        }
    }

    #[test]
    fn expr_not_unary() {
        let q = parse_query("MATCH (n) WHERE NOT n.active = true RETURN n").unwrap();
        match &q.where_clause.as_ref().unwrap().predicate {
            Expr::UnaryOp { op, .. } => assert_eq!(*op, UnaryOp::Not),
            _ => panic!("expected NOT"),
        }
    }

    #[test]
    fn expr_arithmetic_mul_before_add() {
        let q = parse_query("MATCH (n) RETURN n.x + n.y * 2").unwrap();
        match &q.return_clause.items[0].expr {
            Expr::BinaryOp { op, right, .. } => {
                assert_eq!(*op, BinOp::Add);
                match right.as_ref() {
                    Expr::BinaryOp { op: inner, .. } => assert_eq!(*inner, BinOp::Mul),
                    _ => panic!("expected Mul on right"),
                }
            }
            _ => panic!("expected Add at top"),
        }
    }

    #[test]
    fn parse_return_literal_int() {
        let q = parse_query("MATCH (a) RETURN 42").unwrap();
        assert_eq!(
            q.return_clause.items[0].expr,
            Expr::Literal(Literal::Int(42))
        );
    }

    #[test]
    fn parse_return_literal_string() {
        let q = parse_query("MATCH (a) RETURN 'hello'").unwrap();
        assert_eq!(
            q.return_clause.items[0].expr,
            Expr::Literal(Literal::Str("hello".into()))
        );
    }

    #[test]
    fn parse_parenthesized_expr() {
        let q = parse_query("MATCH (n) WHERE (n.a = 1 OR n.b = 2) AND n.c = 3 RETURN n").unwrap();
        let pred = &q.where_clause.as_ref().unwrap().predicate;
        match pred {
            Expr::BinaryOp { op, left, .. } => {
                assert_eq!(*op, BinOp::And);
                match left.as_ref() {
                    Expr::BinaryOp { op: inner, .. } => assert_eq!(*inner, BinOp::Or),
                    _ => panic!("expected OR on left"),
                }
            }
            _ => panic!("expected AND at top"),
        }
    }

    #[test]
    fn parse_full_query_all_clauses() {
        let q = parse_query(
            "MATCH (a:Person)-[:KNOWS]->(b) WHERE a.age > 25 RETURN a.name, b.name ORDER BY a.name ASC LIMIT 10"
        ).unwrap();
        assert_eq!(q.match_clause.patterns.len(), 1);
        assert!(q.where_clause.is_some());
        assert_eq!(q.return_clause.items.len(), 2);
        assert!(q.order_by.is_some());
        assert_eq!(q.limit.unwrap().count, 10);
    }

    #[test]
    fn parse_multiple_match_patterns() {
        let q = parse_query("MATCH (a), (b) RETURN a, b").unwrap();
        assert_eq!(q.match_clause.patterns.len(), 2);
    }

    // ── C2: Depth limit tests ────────────────────────────────────────────────

    /// Runs `body` on a dedicated thread with an 8 MiB stack. The AST
    /// includes large variants (`Expr::Subscript`, `Expr::ListLit`) and
    /// `Expr` is large enough that the recursive descent parser's per-frame
    /// footprint exceeds what the default 2 MiB test-thread stack provides at
    /// `MAX_EXPR_DEPTH` nesting. The parser still rejects nesting beyond
    /// `MAX_EXPR_DEPTH` via `enter_expr`; these tests specifically exercise
    /// the boundary and need a larger stack to avoid OS-level overflow.
    fn run_on_fat_stack<F: FnOnce() + Send + 'static>(body: F) {
        std::thread::Builder::new()
            .stack_size(8 * 1024 * 1024)
            .spawn(body)
            .expect("spawn test thread")
            .join()
            .expect("test thread panicked");
    }

    #[test]
    fn deeply_nested_not_returns_error_not_stack_overflow() {
        run_on_fat_stack(|| {
            let nots = "NOT ".repeat(200);
            let query = format!("MATCH (n) WHERE {nots}true RETURN n");
            let err = parse_query(&query).unwrap_err();
            assert!(
                matches!(err, crate::Error::GqlSyntaxError { .. }),
                "expected syntax error for excessive nesting, got: {err:?}"
            );
        });
    }

    #[test]
    fn deeply_nested_neg_returns_error_not_stack_overflow() {
        run_on_fat_stack(|| {
            let negs = "-".repeat(200);
            let query = format!("MATCH (n) RETURN {negs}1");
            let err = parse_query(&query).unwrap_err();
            assert!(matches!(err, crate::Error::GqlSyntaxError { .. }));
        });
    }

    #[test]
    fn nesting_at_limit_is_accepted() {
        run_on_fat_stack(|| {
            let nots = "NOT ".repeat(MAX_EXPR_DEPTH);
            let query = format!("MATCH (n) WHERE {nots}true RETURN n");
            assert!(parse_query(&query).is_ok());
        });
    }

    #[test]
    fn deeply_nested_parens_returns_error() {
        run_on_fat_stack(|| {
            let open = "(".repeat(200);
            let close = ")".repeat(200);
            let query = format!("MATCH (n) RETURN {open}1{close}");
            let err = parse_query(&query).unwrap_err();
            assert!(matches!(err, crate::Error::GqlSyntaxError { .. }));
        });
    }

    // ── R1/R5: Clone elimination and Display tests ───────────────────────────

    #[test]
    fn expect_ident_returns_correct_string() {
        let q = parse_query("MATCH (myVar) RETURN myVar").unwrap();
        assert_eq!(
            q.match_clause.patterns[0].start.var.as_deref(),
            Some("myVar")
        );
    }

    #[test]
    fn expect_int_returns_correct_value() {
        let q = parse_query("MATCH (n) RETURN n LIMIT 42").unwrap();
        assert_eq!(q.limit.unwrap().count, 42);
    }

    #[test]
    fn parser_error_message_uses_display_not_debug() {
        let err = parse_query("MATCH (a) WHERE RETURN a").unwrap_err();
        let msg = err.to_string();
        assert!(!msg.contains("Token::"), "error exposed Debug repr: {msg}");
    }

    // ── R2: Capacity hint does not affect output ─────────────────────────────

    #[test]
    fn hops_vec_capacity_does_not_affect_output() {
        let q = parse_query("MATCH (a)-[:R1]->(b)-[:R2]->(c)-[:R3]->(d) RETURN a").unwrap();
        assert_eq!(q.match_clause.patterns[0].hops.len(), 3);
    }

    #[test]
    fn parse_return_five_items() {
        let q = parse_query("MATCH (n) RETURN n.a, n.b, n.c, n.d, n.e").unwrap();
        assert_eq!(q.return_clause.items.len(), 5);
    }

    #[test]
    fn parse_order_by_five_items() {
        let q = parse_query("MATCH (n) RETURN n ORDER BY n.a, n.b, n.c, n.d, n.e").unwrap();
        assert_eq!(q.order_by.unwrap().items.len(), 5);
    }

    // ── C3: FunctionCall is not produced ─────────────────────────────────────

    #[test]
    fn aggregate_keyword_not_parsed_as_generic_function_call() {
        // COUNT / SUM / AVG / MIN / MAX / COLLECT are dedicated keyword
        // tokens; they must dispatch to `Expr::Aggregate`, NOT the
        // generic `Expr::FunctionCall` path opened by the WITH-clause
        // work (which accepts arbitrary identifiers followed by `(`).
        let q = parse_query("MATCH (n) RETURN COUNT(*)").unwrap();
        let expr = &q.return_clause.items[0].expr;
        assert!(matches!(expr, Expr::Aggregate { .. }));
    }

    // ── C2-BYPASS: Aggregate depth limit ─────────────────────────────────────

    #[test]
    fn deeply_nested_aggregate_returns_error_not_stack_overflow() {
        run_on_fat_stack(|| {
            let open = "COUNT(".repeat(200);
            let close = ")".repeat(200);
            let query = format!("MATCH (n) RETURN {open}1{close}");
            let err = parse_query(&query).unwrap_err();
            assert!(
                matches!(err, crate::Error::GqlSyntaxError { .. }),
                "expected syntax error for excessive aggregate nesting, got: {err:?}"
            );
        });
    }

    #[test]
    fn aggregate_nesting_at_limit_is_accepted() {
        run_on_fat_stack(|| {
            let open = "COUNT(".repeat(MAX_EXPR_DEPTH);
            let close = ")".repeat(MAX_EXPR_DEPTH);
            let query = format!("MATCH (n) RETURN {open}1{close}");
            assert!(parse_query(&query).is_ok());
        });
    }

    // ── Cycle 13: ParamRef stack-overflow regression ─────────────────────────

    /// Guards the size of `Expr` after adding `Expr::ParamRef`. If `Expr`
    /// grows enough to push the `parse_unary` chain past 8 MiB at depth 128,
    /// this test will overflow the fat-stack thread rather than return an
    /// error — flagging the size regression at CI time.
    #[test]
    fn param_ref_at_max_expr_depth_returns_error_not_stack_overflow() {
        run_on_fat_stack(|| {
            // 200 NOTs around `$id` deliberately exceeds MAX_EXPR_DEPTH so
            // the depth guard fires inside parse_primary's Dollar arm.
            let nots = "NOT ".repeat(200);
            let query = format!("MATCH (n) WHERE {nots}$id RETURN n");
            let err = parse_query(&query).unwrap_err();
            assert!(
                matches!(err, crate::Error::GqlSyntaxError { .. }),
                "expected syntax error for excessive ParamRef nesting, got: {err:?}",
            );
        });
    }

    // ── O2-LIMIT: Edge cases ─────────────────────────────────────────────────

    #[test]
    fn parse_limit_zero() {
        let q = parse_query("MATCH (a) RETURN a LIMIT 0").unwrap();
        assert_eq!(q.limit.unwrap().count, 0);
    }

    #[test]
    fn parse_limit_negative_returns_error() {
        let err = parse_query("MATCH (a) RETURN a LIMIT -1").unwrap_err();
        assert!(matches!(err, crate::Error::GqlSyntaxError { .. }));
    }

    // ── C1: Parser::new debug_assert for Eof ────────────────────────────────

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "token stream must end with Eof")]
    fn parser_new_panics_on_empty_stream_in_debug() {
        let _ = Parser::new(vec![]);
    }

    // ── Cycle 5: parse_statement() + CREATE node ─────────────────────────────

    #[test]
    fn parse_create_single_node_with_label_and_props() {
        let tokens = Lexer::new("CREATE (n:Person {name: 'Alice', age: 30})")
            .tokenize()
            .unwrap();
        let stmt = Parser::new(tokens).parse_statement().unwrap();
        match stmt {
            GqlStatement::Mutation(ms) => match ms.mutation {
                MutationClause::Create(c) => {
                    assert_eq!(c.patterns.len(), 1);
                    match &c.patterns[0] {
                        CreatePattern::Node {
                            var, label, props, ..
                        } => {
                            assert_eq!(var.as_deref(), Some("n"));
                            assert_eq!(label, "Person");
                            assert_eq!(props.len(), 2);
                        }
                        _ => panic!("expected Node pattern"),
                    }
                }
                _ => panic!("expected Create mutation"),
            },
            _ => panic!("expected Mutation statement"),
        }
    }

    #[test]
    fn parse_create_anonymous_node_no_props() {
        let tokens = Lexer::new("CREATE (:Thing)").tokenize().unwrap();
        let stmt = Parser::new(tokens).parse_statement().unwrap();
        match stmt {
            GqlStatement::Mutation(ms) => match ms.mutation {
                MutationClause::Create(c) => match &c.patterns[0] {
                    CreatePattern::Node {
                        var, label, props, ..
                    } => {
                        assert!(var.is_none());
                        assert_eq!(label, "Thing");
                        assert!(props.is_empty());
                    }
                    _ => panic!("expected Node"),
                },
                _ => panic!("expected Create"),
            },
            _ => panic!("expected Mutation"),
        }
    }

    #[test]
    fn parse_match_query_still_works_via_parse_statement() {
        let tokens = Lexer::new("MATCH (a) RETURN a").tokenize().unwrap();
        let stmt = Parser::new(tokens).parse_statement().unwrap();
        assert!(matches!(stmt, GqlStatement::Query(_)));
    }

    #[test]
    fn parse_create_node_missing_label_is_error() {
        // CREATE (n) — no label — should be a syntax error
        let tokens = Lexer::new("CREATE (n)").tokenize().unwrap();
        let result = Parser::new(tokens).parse_statement();
        assert!(result.is_err());
    }

    // ── Cycle 6: CREATE edge ─────────────────────────────────────────────────

    #[test]
    fn parse_create_edge_between_two_vars() {
        let tokens = Lexer::new(
            "CREATE (a:Person {name: 'Alice'})-[:KNOWS {since: 2020}]->(b:Person {name: 'Bob'})",
        )
        .tokenize()
        .unwrap();
        let stmt = Parser::new(tokens).parse_statement().unwrap();
        match stmt {
            GqlStatement::Mutation(ms) => match ms.mutation {
                MutationClause::Create(c) => {
                    assert_eq!(c.patterns.len(), 3, "expected Node a + Node b + Edge");
                    match &c.patterns[0] {
                        CreatePattern::Node {
                            var, label, props, ..
                        } => {
                            assert_eq!(var.as_deref(), Some("a"));
                            assert_eq!(label, "Person");
                            assert_eq!(props.len(), 1);
                            assert_eq!(props[0].0, "name");
                        }
                        other => panic!("expected Node for pattern[0], got {other:?}"),
                    }
                    match &c.patterns[1] {
                        CreatePattern::Node {
                            var, label, props, ..
                        } => {
                            assert_eq!(var.as_deref(), Some("b"));
                            assert_eq!(label, "Person");
                            assert_eq!(props.len(), 1);
                            assert_eq!(props[0].0, "name");
                        }
                        other => panic!("expected Node for pattern[1], got {other:?}"),
                    }
                    match &c.patterns[2] {
                        CreatePattern::Edge {
                            source_var,
                            rel_label,
                            rel_props,
                            target_var,
                        } => {
                            assert_eq!(source_var, "a");
                            assert_eq!(rel_label, "KNOWS");
                            assert_eq!(rel_props.len(), 1);
                            assert_eq!(rel_props[0].0, "since");
                            assert_eq!(target_var, "b");
                        }
                        other => panic!("expected Edge for pattern[2], got {other:?}"),
                    }
                }
                _ => panic!("expected Create"),
            },
            _ => panic!("expected Mutation"),
        }
    }

    #[test]
    fn parse_create_edge_with_previously_bound_vars() {
        let tokens = Lexer::new("CREATE (a:Person)-[:KNOWS]->(b:Person)")
            .tokenize()
            .unwrap();
        let stmt = Parser::new(tokens).parse_statement().unwrap();
        match stmt {
            GqlStatement::Mutation(ms) => {
                assert!(ms.match_clause.is_none());
                match ms.mutation {
                    MutationClause::Create(_) => {}
                    _ => panic!("expected Create"),
                }
            }
            _ => panic!("expected Mutation"),
        }
    }

    #[test]
    fn parse_create_edge_missing_arrow_is_error() {
        let tokens = Lexer::new("CREATE (a:Person)-[:KNOWS]-(b:Person)")
            .tokenize()
            .unwrap();
        let result = Parser::new(tokens).parse_statement();
        assert!(
            result.is_err(),
            "undirected CREATE edge should be a syntax error"
        );
    }

    #[test]
    fn parse_create_incoming_edge_produces_meaningful_error() {
        let tokens = Lexer::new("CREATE (a)<-[:KNOWS]-(b:Person)")
            .tokenize()
            .unwrap();
        let result = Parser::new(tokens).parse_statement();
        assert!(
            result.is_err(),
            "incoming CREATE edge must be a syntax error"
        );
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("outgoing"),
            "expected 'outgoing edges' error, got: {msg}"
        );
    }

    // ── Cycle 6b: MATCH...CREATE edge (variable references) ────────────────

    #[test]
    fn parse_match_then_create_edge() {
        let tokens = Lexer::new(
            "MATCH (a:Person {name: 'Alice'}), (b:Person {name: 'Bob'}) \
             CREATE (a)-[:KNOWS]->(b)",
        )
        .tokenize()
        .unwrap();
        let stmt = Parser::new(tokens).parse_statement().unwrap();
        match stmt {
            GqlStatement::Mutation(ms) => {
                assert!(ms.match_clause.is_some(), "must have MATCH clause");
                match ms.mutation {
                    MutationClause::Create(c) => {
                        assert_eq!(c.patterns.len(), 1, "patterns: {c:?}");
                        match &c.patterns[0] {
                            CreatePattern::Edge {
                                source_var,
                                rel_label,
                                target_var,
                                ..
                            } => {
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

    #[test]
    fn parse_match_create_edge_with_properties() {
        let tokens = Lexer::new(
            "MATCH (a:Person), (b:Person) \
             CREATE (a)-[:KNOWS {since: 2024}]->(b)",
        )
        .tokenize()
        .unwrap();
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

    /// The parser accepts `CREATE (a)-[:KNOWS]->(b)` (var-refs without MATCH) as
    /// syntactically valid. Binding validation — confirming that `a` and `b` are
    /// already bound — is the executor's responsibility, not the parser's.
    #[test]
    fn parse_create_var_ref_without_label() {
        let tokens = Lexer::new("CREATE (a)-[:KNOWS]->(b)").tokenize().unwrap();
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

    // ── Cycle 6c: Consecutive MATCH clauses ──────────────────────────────────

    #[test]
    fn parse_consecutive_match_return() {
        let tokens = Lexer::new("MATCH (a:Person) MATCH (b:Company) RETURN a, b")
            .tokenize()
            .unwrap();
        let stmt = Parser::new(tokens).parse_statement().unwrap();
        match stmt {
            GqlStatement::Query(q) => {
                assert_eq!(q.match_clause.patterns.len(), 2);
                assert_eq!(q.return_clause.items.len(), 2);
            }
            other => panic!("expected Query, got {other:?}"),
        }
    }

    #[test]
    fn parse_consecutive_match_merges_where_clauses() {
        let tokens = Lexer::new(
            "MATCH (a:Person) WHERE a.age > 18 \
             MATCH (b:Company) WHERE b.size > 100 \
             RETURN a, b",
        )
        .tokenize()
        .unwrap();
        let stmt = Parser::new(tokens).parse_statement().unwrap();
        match stmt {
            GqlStatement::Query(q) => {
                assert_eq!(q.match_clause.patterns.len(), 2);
                // The two WHERE predicates must be merged with AND.
                let w = q.where_clause.expect("must have where clause");
                assert!(
                    matches!(w.predicate, Expr::BinaryOp { op: BinOp::And, .. }),
                    "expected AND, got {:?}",
                    w.predicate,
                );
            }
            other => panic!("expected Query, got {other:?}"),
        }
    }

    #[test]
    fn parse_consecutive_match_where_only_first() {
        let tokens = Lexer::new(
            "MATCH (a:Person) WHERE a.age > 18 \
             MATCH (b:Company) \
             RETURN a, b",
        )
        .tokenize()
        .unwrap();
        let stmt = Parser::new(tokens).parse_statement().unwrap();
        match stmt {
            GqlStatement::Query(q) => {
                assert_eq!(q.match_clause.patterns.len(), 2);
                let w = q.where_clause.expect("must have where clause");
                // Single WHERE — should be BinaryOp(>, age, 18), not AND.
                assert!(
                    matches!(w.predicate, Expr::BinaryOp { op: BinOp::Gt, .. }),
                    "expected Gt, got {:?}",
                    w.predicate,
                );
            }
            other => panic!("expected Query, got {other:?}"),
        }
    }

    #[test]
    fn parse_consecutive_match_then_create() {
        let tokens = Lexer::new("MATCH (a:Person) MATCH (b:Person) CREATE (a)-[:KNOWS]->(b)")
            .tokenize()
            .unwrap();
        let stmt = Parser::new(tokens).parse_statement().unwrap();
        match stmt {
            GqlStatement::Mutation(ms) => {
                let mc = ms.match_clause.expect("must have MATCH");
                assert_eq!(mc.patterns.len(), 2);
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

    #[test]
    fn parse_consecutive_match_then_delete() {
        let tokens = Lexer::new("MATCH (a:Person) MATCH (b:Company) DELETE a, b")
            .tokenize()
            .unwrap();
        let stmt = Parser::new(tokens).parse_statement().unwrap();
        match stmt {
            GqlStatement::Mutation(ms) => {
                let mc = ms.match_clause.expect("must have MATCH");
                assert_eq!(mc.patterns.len(), 2);
                match ms.mutation {
                    MutationClause::Delete(d) => {
                        assert_eq!(d.vars.len(), 2);
                    }
                    other => panic!("expected Delete, got {other:?}"),
                }
            }
            other => panic!("expected Mutation, got {other:?}"),
        }
    }

    #[test]
    fn parse_three_consecutive_match_where_builds_and_tree() {
        let tokens = Lexer::new(
            "MATCH (a:Person)  WHERE a.age > 18 \
             MATCH (b:Company) WHERE b.size > 10 \
             MATCH (c:Role)    WHERE c.level > 3 \
             RETURN a, b, c",
        )
        .tokenize()
        .unwrap();
        let stmt = Parser::new(tokens).parse_statement().unwrap();
        match stmt {
            GqlStatement::Query(q) => {
                assert_eq!(q.match_clause.patterns.len(), 3, "must merge 3 patterns");
                let w = q.where_clause.expect("must have WHERE clause");
                // Left-associative: ((pred_A AND pred_B) AND pred_C)
                match w.predicate {
                    Expr::BinaryOp {
                        op: BinOp::And,
                        left,
                        right,
                    } => {
                        // Left branch: (pred_A AND pred_B)
                        assert!(
                            matches!(*left, Expr::BinaryOp { op: BinOp::And, .. }),
                            "left of outer AND should be inner AND, got: {left:?}"
                        );
                        // Right branch: pred_C (c.level > 3)
                        assert!(
                            matches!(*right, Expr::BinaryOp { op: BinOp::Gt, .. }),
                            "right of outer AND should be last predicate (Gt), got: {right:?}"
                        );
                    }
                    other => panic!("expected top-level AND, got: {other:?}"),
                }
            }
            other => panic!("expected Query, got {other:?}"),
        }
    }

    // ── Cycle 6d: var-ref edge cases ───────────────────────────────────────

    #[test]
    fn parse_create_var_ref_source_with_props_is_error() {
        let tokens = Lexer::new("CREATE (a {name: 'Alice'})-[:KNOWS]->(b:Person)")
            .tokenize()
            .unwrap();
        let result = Parser::new(tokens).parse_statement();
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("variable reference"),
            "expected var-ref error, got: {msg}"
        );
    }

    #[test]
    fn parse_create_var_ref_target_with_props_is_error() {
        let tokens = Lexer::new("CREATE (a:Person)-[:KNOWS]->(b {since: 2024})")
            .tokenize()
            .unwrap();
        let result = Parser::new(tokens).parse_statement();
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("variable reference"),
            "expected var-ref error, got: {msg}"
        );
    }

    // ── Cycle 7: DELETE / DETACH DELETE ─────────────────────────────────────

    #[test]
    fn parse_match_then_delete() {
        let tokens = Lexer::new("MATCH (n:Person) DELETE n").tokenize().unwrap();
        let stmt = Parser::new(tokens).parse_statement().unwrap();
        match stmt {
            GqlStatement::Mutation(ms) => {
                assert!(ms.match_clause.is_some());
                match ms.mutation {
                    MutationClause::Delete(d) => {
                        assert!(!d.detach);
                        assert_eq!(d.vars, vec!["n"]);
                    }
                    _ => panic!("expected Delete"),
                }
            }
            _ => panic!("expected Mutation"),
        }
    }

    #[test]
    fn parse_match_then_detach_delete() {
        let tokens = Lexer::new("MATCH (n:Person) DETACH DELETE n")
            .tokenize()
            .unwrap();
        let stmt = Parser::new(tokens).parse_statement().unwrap();
        match stmt {
            GqlStatement::Mutation(ms) => match ms.mutation {
                MutationClause::Delete(d) => {
                    assert!(d.detach);
                    assert_eq!(d.vars, vec!["n"]);
                }
                _ => panic!("expected Delete"),
            },
            _ => panic!("expected Mutation"),
        }
    }

    #[test]
    fn parse_delete_multiple_vars() {
        let tokens = Lexer::new("MATCH (a:Person)-[:KNOWS]->(b:Person) DETACH DELETE a, b")
            .tokenize()
            .unwrap();
        let stmt = Parser::new(tokens).parse_statement().unwrap();
        match stmt {
            GqlStatement::Mutation(ms) => match ms.mutation {
                MutationClause::Delete(d) => {
                    assert!(d.detach);
                    assert_eq!(d.vars.len(), 2);
                    assert!(d.vars.contains(&"a".to_string()));
                    assert!(d.vars.contains(&"b".to_string()));
                }
                _ => panic!("expected Delete"),
            },
            _ => panic!("expected Mutation"),
        }
    }

    #[test]
    fn parse_delete_without_match_is_error() {
        let tokens = Lexer::new("DELETE n").tokenize().unwrap();
        let result = Parser::new(tokens).parse_statement();
        assert!(result.is_err(), "DELETE without MATCH should fail");
    }

    #[test]
    fn parse_set_without_match_is_error() {
        let tokens = Lexer::new("SET n.age = 30").tokenize().unwrap();
        let result = Parser::new(tokens).parse_statement();
        assert!(result.is_err(), "SET without MATCH should fail");
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("SET") && msg.contains("MATCH"),
            "expected 'SET requires MATCH' error, got: {msg}"
        );
    }

    // ── Cycle 8: SET ─────────────────────────────────────────────────────────

    #[test]
    fn parse_match_then_set_single_prop() {
        let tokens = Lexer::new("MATCH (n:Person {name: 'Alice'}) SET n.age = 31")
            .tokenize()
            .unwrap();
        let stmt = Parser::new(tokens).parse_statement().unwrap();
        match stmt {
            GqlStatement::Mutation(ms) => {
                assert!(ms.match_clause.is_some());
                match ms.mutation {
                    MutationClause::Set(s) => {
                        assert_eq!(s.assignments.len(), 1);
                        match &s.assignments[0] {
                            SetAssignment::Property { var, prop, value } => {
                                assert_eq!(var, "n");
                                assert_eq!(prop, "age");
                                assert_eq!(*value, Expr::Literal(Literal::Int(31)));
                            }
                            other => panic!("expected Property, got {other:?}"),
                        }
                    }
                    _ => panic!("expected Set"),
                }
            }
            _ => panic!("expected Mutation"),
        }
    }

    #[test]
    fn parse_set_multiple_assignments() {
        let tokens = Lexer::new("MATCH (n:Person) SET n.age = 25, n.city = 'Madrid'")
            .tokenize()
            .unwrap();
        let stmt = Parser::new(tokens).parse_statement().unwrap();
        match stmt {
            GqlStatement::Mutation(ms) => match ms.mutation {
                MutationClause::Set(s) => {
                    assert_eq!(s.assignments.len(), 2);
                    let props: Vec<&str> = s
                        .assignments
                        .iter()
                        .map(|a| match a {
                            SetAssignment::Property { prop, .. } => prop.as_str(),
                            other => panic!("expected Property, got {other:?}"),
                        })
                        .collect();
                    assert_eq!(props, vec!["age", "city"]);
                }
                _ => panic!("expected Set"),
            },
            _ => panic!("expected Mutation"),
        }
    }

    #[test]
    fn parse_set_value_is_expression_not_just_literal() {
        // Use a non-keyword property name to avoid keyword-as-ident ambiguity.
        let tokens = Lexer::new("MATCH (n:Counter) SET n.score = n.score + 1")
            .tokenize()
            .unwrap();
        let stmt = Parser::new(tokens).parse_statement().unwrap();
        match stmt {
            GqlStatement::Mutation(ms) => match ms.mutation {
                MutationClause::Set(s) => {
                    assert!(matches!(
                        &s.assignments[0],
                        SetAssignment::Property {
                            value: Expr::BinaryOp { .. },
                            ..
                        }
                    ));
                }
                _ => panic!("expected Set"),
            },
            _ => panic!("expected Mutation"),
        }
    }

    // ── Cycle 9: MERGE ───────────────────────────────────────────────────────

    #[test]
    fn parse_merge_node_with_label_and_props() {
        let tokens = Lexer::new("MERGE (n:Person {name: 'Alice'})")
            .tokenize()
            .unwrap();
        let stmt = Parser::new(tokens).parse_statement().unwrap();
        match stmt {
            GqlStatement::Mutation(ms) => match ms.mutation {
                MutationClause::Merge(m) => {
                    assert_eq!(m.label, "Person");
                    assert_eq!(m.var.as_deref(), Some("n"));
                    assert_eq!(m.props.len(), 1);
                    assert_eq!(m.props[0].0, "name");
                }
                _ => panic!("expected Merge"),
            },
            _ => panic!("expected Mutation"),
        }
    }

    #[test]
    fn parse_merge_node_minimal_label_only() {
        let tokens = Lexer::new("MERGE (:Config)").tokenize().unwrap();
        let stmt = Parser::new(tokens).parse_statement().unwrap();
        match stmt {
            GqlStatement::Mutation(ms) => match ms.mutation {
                MutationClause::Merge(m) => {
                    assert_eq!(m.label, "Config");
                    assert!(m.var.is_none());
                    assert!(m.props.is_empty());
                }
                _ => panic!("expected Merge"),
            },
            _ => panic!("expected Mutation"),
        }
    }

    #[test]
    fn parse_merge_without_label_is_error() {
        let tokens = Lexer::new("MERGE (n)").tokenize().unwrap();
        let result = Parser::new(tokens).parse_statement();
        assert!(
            result.is_err(),
            "MERGE without label should be a syntax error"
        );
    }

    // ── Variable-length path parsing ─────────────────────────────────────────

    #[test]
    fn parse_var_len_star_only() {
        let q = parse_query("MATCH (a)-[*]->(b) RETURN a").unwrap();
        let hop = &q.match_clause.patterns[0].hops[0].0;
        assert!(matches!(
            hop.length,
            EdgeLength::Variable {
                min: None,
                max: None
            }
        ));
    }

    #[test]
    fn parse_var_len_bounded() {
        let q = parse_query("MATCH (a)-[*1..5]->(b) RETURN a").unwrap();
        let hop = &q.match_clause.patterns[0].hops[0].0;
        assert!(matches!(
            hop.length,
            EdgeLength::Variable {
                min: Some(1),
                max: Some(5)
            }
        ));
    }

    #[test]
    fn parse_var_len_min_only() {
        let q = parse_query("MATCH (a)-[*2..]->(b) RETURN a").unwrap();
        let hop = &q.match_clause.patterns[0].hops[0].0;
        assert!(matches!(
            hop.length,
            EdgeLength::Variable {
                min: Some(2),
                max: None
            }
        ));
    }

    #[test]
    fn parse_var_len_max_only() {
        let q = parse_query("MATCH (a)-[*..3]->(b) RETURN a").unwrap();
        let hop = &q.match_clause.patterns[0].hops[0].0;
        assert!(matches!(
            hop.length,
            EdgeLength::Variable {
                min: None,
                max: Some(3)
            }
        ));
    }

    #[test]
    fn parse_var_len_with_label() {
        let q = parse_query("MATCH (a)-[:KNOWS*1..3]->(b) RETURN a").unwrap();
        let hop = &q.match_clause.patterns[0].hops[0].0;
        assert_eq!(hop.labels, vec!["KNOWS"]);
        assert!(matches!(
            hop.length,
            EdgeLength::Variable {
                min: Some(1),
                max: Some(3)
            }
        ));
    }

    #[test]
    fn parse_var_len_exact_hops() {
        let q = parse_query("MATCH (a)-[*3]->(b) RETURN a").unwrap();
        let hop = &q.match_clause.patterns[0].hops[0].0;
        assert!(matches!(
            hop.length,
            EdgeLength::Variable {
                min: Some(3),
                max: Some(3)
            }
        ));
    }

    #[test]
    fn parse_group_by_clause() {
        let query =
            parse_query("MATCH (p:Person) RETURN p.dept, COUNT(*) AS cnt GROUP BY p.dept").unwrap();
        assert!(query.group_by.is_some());
        let group_by = query.group_by.unwrap();
        assert_eq!(group_by.keys.len(), 1);
        assert!(matches!(
            &group_by.keys[0],
            Expr::PropAccess { var, prop } if var == "p" && prop == "dept"
        ));
    }

    #[test]
    fn parse_group_by_multiple_keys() {
        let query = parse_query(
            "MATCH (p:Person) RETURN p.dept, p.role, COUNT(*) AS cnt GROUP BY p.dept, p.role",
        )
        .unwrap();
        assert!(query.group_by.is_some());
        assert_eq!(query.group_by.unwrap().keys.len(), 2);
    }

    #[test]
    fn parse_no_group_by_returns_none() {
        let query = parse_query("MATCH (p:Person) RETURN p.name").unwrap();
        assert!(query.group_by.is_none());
    }

    // ── Cycle 4: Token::Dollar arm in parse_primary ──────────────────────────
    //
    // These tests pin that `$name` and `$<n>` (n >= 1) are parsed as
    // `Expr::ParamRef` in every expression position the parser threads
    // expressions through. Resolution to a literal happens in cycle 6's
    // `param_substitution::apply` — these tests check the parser only.
    //
    // Not covered here: `LIMIT $n` against a normal `GqlQuery`. The
    // existing `parse_limit_clause` uses `expect_int()`, which only
    // accepts IntLit — that contract is unaffected by this cycle. The
    // `LIMIT <expr>` form belongs to `ConstReturnQuery.limit` (cycle 5).

    #[test]
    fn parser_dollar_named_param_in_return() {
        let tokens = Lexer::new("MATCH (n) RETURN $id").tokenize().unwrap();
        let stmt = Parser::new(tokens).parse_statement().unwrap();
        let q = match stmt {
            GqlStatement::Query(q) => q,
            other => panic!("expected Query, got {other:?}"),
        };
        assert_eq!(q.return_clause.items.len(), 1);
        assert_eq!(
            q.return_clause.items[0].expr,
            Expr::ParamRef(ParamRef::Named("id".into())),
        );
    }

    #[test]
    fn parser_dollar_positional_param_in_where() {
        let tokens = Lexer::new("MATCH (n) WHERE n.age > $1 RETURN n")
            .tokenize()
            .unwrap();
        let stmt = Parser::new(tokens).parse_statement().unwrap();
        let q = match stmt {
            GqlStatement::Query(q) => q,
            other => panic!("expected Query, got {other:?}"),
        };
        let predicate = q.where_clause.expect("expected WHERE").predicate;
        // Predicate is `n.age > $1` — a BinaryOp with PropAccess on the
        // left and ParamRef on the right.
        match predicate {
            Expr::BinaryOp { left, op, right } => {
                assert!(matches!(*left, Expr::PropAccess { ref var, ref prop }
                    if var == "n" && prop == "age"));
                assert_eq!(op, BinOp::Gt);
                assert_eq!(*right, Expr::ParamRef(ParamRef::Positional(1)));
            }
            other => panic!("expected BinaryOp at WHERE predicate root, got {other:?}"),
        }
    }

    #[test]
    fn parser_dollar_named_param_in_set() {
        let tokens = Lexer::new("MATCH (n) SET n.x = $val").tokenize().unwrap();
        let stmt = Parser::new(tokens).parse_statement().unwrap();
        let assigns = match stmt {
            GqlStatement::Mutation(ms) => match ms.mutation {
                MutationClause::Set(s) => s.assignments,
                other => panic!("expected Set, got {other:?}"),
            },
            other => panic!("expected Mutation, got {other:?}"),
        };
        assert_eq!(assigns.len(), 1);
        match &assigns[0] {
            SetAssignment::Property { var, prop, value } => {
                assert_eq!(var, "n");
                assert_eq!(prop, "x");
                assert_eq!(*value, Expr::ParamRef(ParamRef::Named("val".into())));
            }
            other => panic!("expected Property, got {other:?}"),
        }
    }

    #[test]
    fn parser_dollar_named_param_in_create_props() {
        let tokens = Lexer::new("CREATE (n:Foo {id: $id})").tokenize().unwrap();
        let stmt = Parser::new(tokens).parse_statement().unwrap();
        let create = match stmt {
            GqlStatement::Mutation(ms) => match ms.mutation {
                MutationClause::Create(c) => c,
                other => panic!("expected Create, got {other:?}"),
            },
            other => panic!("expected Mutation, got {other:?}"),
        };
        assert_eq!(create.patterns.len(), 1);
        match &create.patterns[0] {
            CreatePattern::Node { label, props, .. } => {
                assert_eq!(label, "Foo");
                assert_eq!(props.len(), 1);
                assert_eq!(props[0].0, "id");
                assert_eq!(props[0].1, Expr::ParamRef(ParamRef::Named("id".into())));
            }
            other => panic!("expected Node pattern, got {other:?}"),
        }
    }

    #[test]
    fn parser_dollar_param_in_list_element() {
        // `IN` predicate with a list literal containing two params. The
        // list path goes through `parse_list_literal` which in turn calls
        // `parse_expr`, so ParamRef must be visible there.
        let tokens = Lexer::new("MATCH (n) WHERE n.id IN [$a, $b] RETURN n")
            .tokenize()
            .unwrap();
        let stmt = Parser::new(tokens).parse_statement().unwrap();
        let q = match stmt {
            GqlStatement::Query(q) => q,
            other => panic!("expected Query, got {other:?}"),
        };
        let predicate = q.where_clause.expect("expected WHERE").predicate;
        match predicate {
            Expr::BinaryOp {
                left: _,
                op: BinOp::In,
                right,
            } => match *right {
                Expr::ListLit(items) => {
                    assert_eq!(items.len(), 2);
                    assert_eq!(items[0], Expr::ParamRef(ParamRef::Named("a".into())));
                    assert_eq!(items[1], Expr::ParamRef(ParamRef::Named("b".into())));
                }
                other => panic!("expected ListLit on rhs of IN, got {other:?}"),
            },
            other => panic!("expected IN BinaryOp, got {other:?}"),
        }
    }

    #[test]
    fn parser_dollar_zero_positional_is_rejected() {
        // $0 is not a valid positional placeholder — Bolt and openCypher
        // use 1-based indexing.
        let tokens = Lexer::new("MATCH (n) RETURN $0").tokenize().unwrap();
        let err = Parser::new(tokens).parse_statement().unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("positional parameter") || msg.contains(">= 1"),
            "error should mention positional 1-based requirement, got: {msg}"
        );
    }

    // ── Cycle 5: RETURN as root statement (ConstReturn) ──────────────────────
    //
    // These tests pin that `RETURN <expr-list>` is accepted as a top-level
    // statement with no preceding MATCH/UNWIND/CREATE, parsing into
    // `GqlStatement::ConstReturn`. Execution belongs to cycle 7.

    fn parse_const_return(input: &str) -> ConstReturnQuery {
        let tokens = Lexer::new(input).tokenize().unwrap();
        let stmt = Parser::new(tokens).parse_statement().unwrap();
        match stmt {
            GqlStatement::ConstReturn(q) => q,
            other => panic!("expected ConstReturn, got {other:?}"),
        }
    }

    #[test]
    fn parser_return_standalone_literal_int() {
        let q = parse_const_return("RETURN 1");
        assert!(!q.distinct);
        assert!(q.limit.is_none());
        assert!(q.skip.is_none());
        assert_eq!(q.items.len(), 1);
        assert_eq!(q.items[0].expr, Expr::Literal(Literal::Int(1)));
        assert!(q.items[0].alias.is_none());
    }

    #[test]
    fn parser_return_standalone_arithmetic() {
        // Confirms that the standalone form runs the full expression
        // grammar — `1 + 2 * 3` must parse with correct precedence.
        let q = parse_const_return("RETURN 1 + 2 * 3");
        assert_eq!(q.items.len(), 1);
        match &q.items[0].expr {
            Expr::BinaryOp {
                left,
                op: BinOp::Add,
                right,
            } => {
                assert_eq!(left.as_ref(), &Expr::Literal(Literal::Int(1)));
                // Right side must be `2 * 3` (precedence respected).
                match right.as_ref() {
                    Expr::BinaryOp {
                        left: l2,
                        op: BinOp::Mul,
                        right: r2,
                    } => {
                        assert_eq!(l2.as_ref(), &Expr::Literal(Literal::Int(2)));
                        assert_eq!(r2.as_ref(), &Expr::Literal(Literal::Int(3)));
                    }
                    other => panic!("expected `2 * 3` on rhs, got {other:?}"),
                }
            }
            other => panic!("expected Add at root, got {other:?}"),
        }
    }

    #[test]
    fn parser_return_standalone_list() {
        // `parse_list_literal` collapses an all-literal list to
        // `Expr::Literal(Literal::List(_))` (covers the common
        // `IN [1, 2, 3]` idiom). `Expr::ListLit` is only produced when
        // at least one element is a non-literal expression.
        let q = parse_const_return("RETURN [1, 2, 3]");
        assert_eq!(q.items.len(), 1);
        assert_eq!(
            q.items[0].expr,
            Expr::Literal(Literal::List(vec![
                Literal::Int(1),
                Literal::Int(2),
                Literal::Int(3),
            ])),
        );
    }

    #[test]
    fn parser_return_standalone_list_with_non_literal_uses_listlit() {
        // When the list contains a non-literal (here: a ParamRef), the
        // parser produces `Expr::ListLit` so each element can be an
        // arbitrary expression.
        let q = parse_const_return("RETURN [1, $x, 3]");
        assert_eq!(q.items.len(), 1);
        match &q.items[0].expr {
            Expr::ListLit(items) => {
                assert_eq!(items.len(), 3);
                assert_eq!(items[0], Expr::Literal(Literal::Int(1)));
                assert_eq!(items[1], Expr::ParamRef(ParamRef::Named("x".into())));
                assert_eq!(items[2], Expr::Literal(Literal::Int(3)));
            }
            other => panic!("expected ListLit (heterogeneous), got {other:?}"),
        }
    }

    #[test]
    fn parser_return_standalone_with_alias() {
        let q = parse_const_return("RETURN 1 AS one");
        assert_eq!(q.items.len(), 1);
        assert_eq!(q.items[0].expr, Expr::Literal(Literal::Int(1)));
        assert_eq!(q.items[0].alias.as_deref(), Some("one"));
    }

    #[test]
    fn parser_return_standalone_distinct() {
        // RETURN DISTINCT 1 — one-row distinct is degenerate but the
        // parser must accept the flag for driver-compat fidelity.
        let q = parse_const_return("RETURN DISTINCT 1");
        assert!(q.distinct);
        assert_eq!(q.items.len(), 1);
        assert_eq!(q.items[0].expr, Expr::Literal(Literal::Int(1)));
    }

    #[test]
    fn parser_return_standalone_with_skip_and_limit_expressions() {
        // `SKIP $s LIMIT $l` exercises the param-aware expression-form
        // SKIP/LIMIT path that is exclusive to ConstReturnQuery.
        let q = parse_const_return("RETURN 1 SKIP $s LIMIT $l");
        assert_eq!(q.skip, Some(Expr::ParamRef(ParamRef::Named("s".into()))));
        assert_eq!(q.limit, Some(Expr::ParamRef(ParamRef::Named("l".into()))));
    }

    #[test]
    fn parser_return_standalone_multiple_items() {
        let q = parse_const_return("RETURN 1, 'hello', true");
        assert_eq!(q.items.len(), 3);
        assert_eq!(q.items[0].expr, Expr::Literal(Literal::Int(1)));
        assert_eq!(q.items[1].expr, Expr::Literal(Literal::Str("hello".into())));
        assert_eq!(q.items[2].expr, Expr::Literal(Literal::Bool(true)));
    }

    // ── Issue #12: STARTS WITH / ENDS WITH string predicates ─────────────────
    //
    // The lexer tokenises `WITH` as the reserved `Token::With` (for pipelines),
    // but the parser used to look for `Token::Ident("WITH")` as the second
    // token of `STARTS WITH` / `ENDS WITH`, so those branches were dead code.
    // These tests pin the parse result to the correct `BinOp`.

    #[test]
    fn parse_where_starts_with_uppercase() {
        let q = parse_query("MATCH (a:Person) WHERE a.name STARTS WITH 'Al' RETURN a").unwrap();
        let predicate = q.where_clause.expect("expected WHERE").predicate;
        match predicate {
            Expr::BinaryOp { left, op, right } => {
                assert_eq!(op, BinOp::StartsWith);
                assert!(
                    matches!(*left, Expr::PropAccess { ref var, ref prop }
                        if var == "a" && prop == "name"),
                    "left must be a.name, got {left:?}"
                );
                assert_eq!(*right, Expr::Literal(Literal::Str("Al".into())));
            }
            other => panic!("expected BinaryOp(StartsWith), got {other:?}"),
        }
    }

    #[test]
    fn parse_where_starts_with_lowercase() {
        // `starts` → Ident("starts"); `with` → Token::With. Both must match.
        let q = parse_query("MATCH (a:Person) WHERE a.name starts with 'Al' RETURN a").unwrap();
        match q.where_clause.expect("expected WHERE").predicate {
            Expr::BinaryOp { op, .. } => assert_eq!(op, BinOp::StartsWith),
            other => panic!("expected StartsWith, got {other:?}"),
        }
    }

    #[test]
    fn parse_where_starts_with_mixed_case() {
        let q = parse_query("MATCH (a:Person) WHERE a.name Starts With 'Al' RETURN a").unwrap();
        match q.where_clause.expect("expected WHERE").predicate {
            Expr::BinaryOp { op, .. } => assert_eq!(op, BinOp::StartsWith),
            other => panic!("expected StartsWith, got {other:?}"),
        }
    }

    #[test]
    fn parse_where_ends_with_uppercase() {
        let q = parse_query("MATCH (a:Person) WHERE a.email ENDS WITH '@example.com' RETURN a")
            .unwrap();
        match q.where_clause.expect("expected WHERE").predicate {
            Expr::BinaryOp { left, op, right } => {
                assert_eq!(op, BinOp::EndsWith);
                assert!(
                    matches!(*left, Expr::PropAccess { ref var, ref prop }
                        if var == "a" && prop == "email"),
                    "left must be a.email, got {left:?}"
                );
                assert_eq!(*right, Expr::Literal(Literal::Str("@example.com".into())));
            }
            other => panic!("expected BinaryOp(EndsWith), got {other:?}"),
        }
    }

    #[test]
    fn parse_where_ends_with_lowercase() {
        let q = parse_query("MATCH (a:Person) WHERE a.email ends with '@example.com' RETURN a")
            .unwrap();
        match q.where_clause.expect("expected WHERE").predicate {
            Expr::BinaryOp { op, .. } => assert_eq!(op, BinOp::EndsWith),
            other => panic!("expected EndsWith, got {other:?}"),
        }
    }

    #[test]
    fn parse_where_starts_with_and_condition() {
        // STARTS WITH must compose with AND: root predicate is AND, its left
        // operand is the StartsWith node.
        let q =
            parse_query("MATCH (a:Person) WHERE a.name STARTS WITH 'Al' AND a.age > 30 RETURN a")
                .unwrap();
        match q.where_clause.expect("expected WHERE").predicate {
            Expr::BinaryOp {
                op: BinOp::And,
                left,
                ..
            } => match *left {
                Expr::BinaryOp { op, .. } => assert_eq!(op, BinOp::StartsWith),
                other => panic!("expected StartsWith on left of AND, got {other:?}"),
            },
            other => panic!("expected AND at root, got {other:?}"),
        }
    }

    #[test]
    fn parse_where_starts_with_or_ends_with() {
        let q = parse_query(
            "MATCH (a:Person) WHERE a.name STARTS WITH 'Al' OR a.name ENDS WITH 'son' RETURN a",
        )
        .unwrap();
        match q.where_clause.expect("expected WHERE").predicate {
            Expr::BinaryOp {
                op: BinOp::Or,
                left,
                right,
            } => {
                match *left {
                    Expr::BinaryOp { op, .. } => assert_eq!(op, BinOp::StartsWith),
                    other => panic!("expected StartsWith on left, got {other:?}"),
                }
                match *right {
                    Expr::BinaryOp { op, .. } => assert_eq!(op, BinOp::EndsWith),
                    other => panic!("expected EndsWith on right, got {other:?}"),
                }
            }
            other => panic!("expected OR at root, got {other:?}"),
        }
    }

    #[test]
    fn parse_where_starts_with_and_in_list() {
        // Exercises StartsWith on the left and IN [...] on the right of AND.
        let q = parse_query(
            "MATCH (a:Person) WHERE a.name STARTS WITH 'Al' \
             AND a.status IN ['active', 'pending'] RETURN a",
        )
        .unwrap();
        match q.where_clause.expect("expected WHERE").predicate {
            Expr::BinaryOp {
                op: BinOp::And,
                left,
                right,
            } => {
                match *left {
                    Expr::BinaryOp { op, .. } => assert_eq!(op, BinOp::StartsWith),
                    other => panic!("expected StartsWith on left, got {other:?}"),
                }
                match *right {
                    Expr::BinaryOp { op, .. } => assert_eq!(op, BinOp::In),
                    other => panic!("expected In on right, got {other:?}"),
                }
            }
            other => panic!("expected AND at root, got {other:?}"),
        }
    }

    #[test]
    fn parse_where_contains_sanity() {
        // CONTAINS is a single Ident keyword (not reserved), so it was never
        // affected by the WITH-token bug. Pins that it still parses correctly.
        let q = parse_query("MATCH (a:Person) WHERE a.bio CONTAINS 'engineer' RETURN a").unwrap();
        match q.where_clause.expect("expected WHERE").predicate {
            Expr::BinaryOp { left, op, right } => {
                assert_eq!(op, BinOp::Contains);
                assert!(
                    matches!(*left, Expr::PropAccess { ref var, ref prop }
                        if var == "a" && prop == "bio"),
                    "left must be a.bio, got {left:?}"
                );
                assert_eq!(*right, Expr::Literal(Literal::Str("engineer".into())));
            }
            other => panic!("expected BinaryOp(Contains), got {other:?}"),
        }
    }

    #[test]
    fn parse_where_in_list_sanity() {
        // IN is a single Ident keyword (not reserved); re-verified in isolation.
        let q = parse_query("MATCH (a:Person) WHERE a.status IN ['active', 'inactive'] RETURN a")
            .unwrap();
        match q.where_clause.expect("expected WHERE").predicate {
            Expr::BinaryOp { left, op, .. } => {
                assert_eq!(op, BinOp::In);
                assert!(
                    matches!(*left, Expr::PropAccess { ref var, ref prop }
                        if var == "a" && prop == "status"),
                    "left must be a.status, got {left:?}"
                );
            }
            other => panic!("expected BinaryOp(In), got {other:?}"),
        }
    }

    // ── Fase 2: MERGE + parameterised maps (cycles 3.3, 3.4, 3.5, 4.2) ───────

    fn parse_mut(input: &str) -> MutationStatement {
        let tokens = Lexer::new(input).tokenize().unwrap();
        match Parser::new(tokens).parse_statement().unwrap() {
            GqlStatement::Mutation(m) => m,
            other => panic!("expected Mutation, got {other:?}"),
        }
    }

    /// Extracts the SET clause from a `MATCH … SET` mutation (where it lands in
    /// `mutation: MutationClause::Set`, not the `set_clause` field).
    fn set_of(m: &MutationStatement) -> &SetClause {
        match &m.mutation {
            MutationClause::Set(s) => s,
            other => panic!("expected Set mutation, got {other:?}"),
        }
    }

    #[test]
    fn parse_set_entity_overwrite_from_param() {
        let m = parse_mut("MATCH (n:Person {name: 'Alice'}) SET n = $props");
        let set = set_of(&m);
        assert_eq!(set.assignments.len(), 1);
        assert!(matches!(
            &set.assignments[0],
            SetAssignment::EntityOverwrite { var, map_expr: Expr::ParamRef(ParamRef::Named(p)) }
                if var == "n" && p == "props"
        ));
    }

    #[test]
    fn parse_set_entity_merge_from_param() {
        let m = parse_mut("MATCH (n:Person {name: 'Alice'}) SET n += $props");
        let set = set_of(&m);
        assert_eq!(set.assignments.len(), 1);
        assert!(matches!(
            &set.assignments[0],
            SetAssignment::EntityMerge { var, map_expr: Expr::ParamRef(ParamRef::Named(p)) }
                if var == "n" && p == "props"
        ));
    }

    #[test]
    fn parse_set_property_still_works() {
        // Regression: per-property form must still parse to Property.
        let m = parse_mut("MATCH (n:Person) SET n.age = 30");
        let set = set_of(&m);
        assert!(matches!(
            &set.assignments[0],
            SetAssignment::Property { var, prop, .. } if var == "n" && prop == "age"
        ));
    }

    #[test]
    fn parse_merge_inline_prop_dollar_param() {
        let m = parse_mut("MERGE (n:AssetNode {id: $id})");
        let merge = match &m.mutation {
            MutationClause::Merge(mc) => mc,
            other => panic!("expected Merge, got {other:?}"),
        };
        assert_eq!(merge.props.len(), 1);
        assert!(matches!(
            &merge.props[0],
            (k, Expr::ParamRef(ParamRef::Named(p))) if k == "id" && p == "id"
        ));
    }

    #[test]
    fn parse_create_node_with_bare_map_param() {
        let m = parse_mut("CREATE (n:Label $props)");
        let create = match &m.mutation {
            MutationClause::Create(c) => c,
            other => panic!("expected Create, got {other:?}"),
        };
        match &create.patterns[0] {
            CreatePattern::Node {
                props, prop_map, ..
            } => {
                assert!(props.is_empty(), "bare $map uses prop_map, not props");
                assert!(matches!(
                    prop_map,
                    Some(Expr::ParamRef(ParamRef::Named(p))) if p == "props"
                ));
            }
            other => panic!("expected Node, got {other:?}"),
        }
    }

    #[test]
    fn parse_merge_with_return() {
        let m = parse_mut("MERGE (n:AssetNode {id: 'x'}) RETURN n");
        let merge = match &m.mutation {
            MutationClause::Merge(mc) => mc,
            other => panic!("expected Merge, got {other:?}"),
        };
        assert_eq!(merge.return_var, Some("n".into()));
    }

    #[test]
    fn parse_merge_with_on_create_set() {
        let m = parse_mut("MERGE (n:Label {id: 'x'}) ON CREATE SET n.x = 1");
        let merge = match &m.mutation {
            MutationClause::Merge(mc) => mc,
            other => panic!("{other:?}"),
        };
        assert!(merge.on_create.is_some());
        assert!(merge.on_match.is_none());
    }

    #[test]
    fn parse_merge_full_client_pattern() {
        let m = parse_mut(
            "MERGE (n:AssetNode {id: $id}) ON CREATE SET n = $props ON MATCH SET n += $props RETURN n",
        );
        let merge = match &m.mutation {
            MutationClause::Merge(mc) => mc,
            other => panic!("{other:?}"),
        };
        assert!(merge.on_create.is_some());
        assert!(merge.on_match.is_some());
        assert_eq!(merge.return_var, Some("n".into()));
    }

    // ── 3f C3.1: list-predicate grammar (ALL/ANY/NONE/SINGLE) ────────────────

    use crate::gql::ast::ListPredKind;

    fn list_pred_of(input: &str) -> Expr {
        let q = parse_query(input).unwrap();
        q.where_clause.expect("expected WHERE").predicate
    }

    #[test]
    fn parse_all_predicate() {
        let pred = list_pred_of("MATCH (n) WHERE ALL(x IN [1, 2, 3] WHERE x > 0) RETURN n");
        match pred {
            Expr::ListPredicate {
                kind,
                var,
                list,
                predicate,
            } => {
                assert_eq!(kind, ListPredKind::All);
                assert_eq!(var, "x");
                // An all-constant `[1, 2, 3]` parses to `Literal(List(_))`; a
                // list with non-constant elements would be `ListLit(_)`. Either
                // is a valid list expression for the predicate's source.
                assert!(
                    matches!(*list, Expr::Literal(Literal::List(_)) | Expr::ListLit(_)),
                    "list must be a list expression, got {list:?}"
                );
                assert!(
                    matches!(*predicate, Expr::BinaryOp { op: BinOp::Gt, .. }),
                    "predicate must be x > 0"
                );
            }
            other => panic!("expected ListPredicate, got {other:?}"),
        }
    }

    #[test]
    fn parse_any_predicate() {
        let pred = list_pred_of("MATCH (n) WHERE ANY(x IN [1, 2, 3] WHERE x = 2) RETURN n");
        assert!(matches!(
            pred,
            Expr::ListPredicate {
                kind: ListPredKind::Any,
                ..
            }
        ));
    }

    #[test]
    fn parse_none_predicate() {
        let pred = list_pred_of("MATCH (n) WHERE NONE(x IN [1, 2, 3] WHERE x > 5) RETURN n");
        assert!(matches!(
            pred,
            Expr::ListPredicate {
                kind: ListPredKind::None,
                ..
            }
        ));
    }

    #[test]
    fn parse_single_predicate() {
        let pred = list_pred_of("MATCH (n) WHERE SINGLE(x IN [1, 2, 3] WHERE x = 2) RETURN n");
        assert!(matches!(
            pred,
            Expr::ListPredicate {
                kind: ListPredKind::Single,
                ..
            }
        ));
    }

    #[test]
    fn parse_list_pred_over_property_list() {
        // The list expression can be any expression, e.g. a property holding a
        // list — exercised here as a bare variable reference to confirm the
        // grammar does not hard-require a literal.
        let pred = list_pred_of("MATCH (n) WHERE ALL(t IN n.tags WHERE t = 'x') RETURN n");
        match pred {
            Expr::ListPredicate { var, list, .. } => {
                assert_eq!(var, "t");
                assert!(matches!(*list, Expr::PropAccess { .. }));
            }
            other => panic!("expected ListPredicate, got {other:?}"),
        }
    }

    #[test]
    fn parse_list_pred_missing_where_errors() {
        // `ALL(x IN list)` without a WHERE predicate is a syntax error.
        assert!(parse_query("MATCH (n) WHERE ALL(x IN [1, 2]) RETURN n").is_err());
    }

    // ── Fase B C5: `MATCH p = (…)` path binding ───────────────────────────

    /// Parses `input` and returns the MATCH clause of the resulting read query.
    fn first_match_clause(input: &str) -> MatchClause {
        let tokens = Lexer::new(input).tokenize().unwrap();
        match Parser::new(tokens).parse_statement().unwrap() {
            GqlStatement::Query(q) => q.match_clause,
            other => panic!("expected Query, got {other:?}"),
        }
    }

    #[test]
    fn match_with_path_binding_parses() {
        let mc = first_match_clause("MATCH p = (a)-[r]->(b) RETURN p");
        assert_eq!(mc.path_var.as_deref(), Some("p"));
        assert_eq!(mc.patterns.len(), 1, "the pattern is still parsed");
    }

    #[test]
    fn match_without_path_binding_has_no_path_var() {
        let mc = first_match_clause("MATCH (a)-[r]->(b) RETURN a");
        assert_eq!(mc.path_var, None);
    }

    #[test]
    fn match_var_length_path_binding_parses() {
        let mc = first_match_clause("MATCH p = (a)-[*1..3]->(b) RETURN p");
        assert_eq!(mc.path_var.as_deref(), Some("p"));
    }
}
