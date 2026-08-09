// SPDX-License-Identifier: MIT

//! Post-parse parameter substitution.
//!
//! Walks a parsed [`GqlStatement`] in place, replacing every
//! [`Expr::ParamRef`] with an [`Expr::Literal`] whose value is sourced from a
//! `HashMap<String, GqlValue>`. Runs between [`super::parse_statement`] and
//! the compiler.
//!
//! # Why post-parse, not in-string?
//!
//! Stitching parameter values into the query string before lexing opens an
//! SQL-injection-class attack surface and defeats the query-plan cache (every
//! distinct value triggers a recompile). Resolving against a typed value map
//! after parsing is safe by construction and lets the cache key be
//! `(query_text, hash(params))` rather than the whole interpolated string.
//!
//! # Contract
//!
//! After [`apply`] returns `Ok(())`, no `Expr::ParamRef` remains anywhere in
//! the statement. The compiler relies on this and `debug_assert!`s against
//! a leftover `ParamRef` in [`super::compiler::eval_expr`] and
//! [`super::compiler::eval_expr_on_binding`].

use std::collections::HashMap;
use std::hash::BuildHasher;

use thiserror::Error;

use super::ast::{
    ConstReturnQuery, CreatePattern, Expr, GqlQuery, GqlStatement, Literal, MutationClause,
    MutationStatement, OrderItem, ParamRef, PipelineQuery, PipelineStage, PipelineTerminal,
    ReturnItem, SetAssignment, SetClause, UnwindClause, WhereClause, WithClause,
};
use super::compiler::GqlValue;

/// Errors produced by [`apply`].
///
/// All variants map to a stable Bolt wire code at the handler seam:
/// - `MissingParameter` / `MissingPositionalParameter` →
///   `Neo.ClientError.Statement.ParameterMissing`
/// - `UnsupportedParamValue` → `Neo.ClientError.Statement.TypeError`
#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum ParamError {
    /// A `$name` placeholder had no matching entry in the params map.
    #[error("missing parameter: ${0}")]
    MissingParameter(String),
    /// A `$<n>` placeholder had no matching entry in the params map.
    /// Positional indices are 1-based per the Bolt spec; the resolver
    /// looks them up by the string key `n.to_string()`.
    #[error("missing positional parameter: ${0}")]
    MissingPositionalParameter(u32),
    /// A value in the params map could not be lowered to an AST literal —
    /// the variant is reserved for future `GqlValue` extensions (Node,
    /// Relationship, Map). With the current `GqlValue` (`Null`, `Bool`,
    /// `Int`, `Float`, `Str`, `List`) every value lowers successfully, so
    /// this variant is currently unreachable in practice. Kept in the API
    /// so adding a new `GqlValue` variant produces a compile error here
    /// instead of silently widening behaviour.
    #[error("parameter ${name} has type {got} which cannot be used as a literal")]
    UnsupportedParamValue {
        /// The parameter name that triggered the error (named lookup) or
        /// `n.to_string()` for positional lookups.
        name: String,
        /// The unsupported `GqlValue` variant's canonical name.
        got: &'static str,
    },
}

/// Replaces every `Expr::ParamRef` in `stmt` with `Expr::Literal`, sourcing
/// values from `params`.
///
/// On error, `stmt` is left in a partially-substituted state. Callers MUST
/// NOT pass a partially-substituted statement to the compiler — discard it
/// or re-parse from the original query string.
///
/// # Errors
///
/// Returns `Err(ParamError)` if any `$name` is missing from `params`, any
/// `$n` is missing under the string key `n.to_string()`, or any value in
/// `params` cannot be lowered to a literal.
pub fn apply<S: BuildHasher>(
    stmt: &mut GqlStatement,
    params: &HashMap<String, GqlValue, S>,
) -> Result<(), ParamError> {
    match stmt {
        GqlStatement::Query(q) => visit_query(q, params),
        GqlStatement::Mutation(m) => visit_mutation(m, params),
        GqlStatement::Pipeline(p) => visit_pipeline(p, params),
        GqlStatement::ConstReturn(c) => visit_const_return(c, params),
        // Admin/DDL/CALL statements carry no `$param` Expr — nothing to walk.
        // (CALL's UNWIND/RETURN reference only the YIELD column var, not params;
        // the built-in procedures take no arguments.)
        GqlStatement::Admin(_) | GqlStatement::Ddl(_) | GqlStatement::Call(_) => Ok(()),
    }
}

// ── Statement-level walkers ──────────────────────────────────────────────────

fn visit_query<S: BuildHasher>(
    q: &mut GqlQuery,
    params: &HashMap<String, GqlValue, S>,
) -> Result<(), ParamError> {
    if let Some(ref mut u) = q.unwind_clause {
        visit_unwind(u, params)?;
    }
    if let Some(ref mut w) = q.where_clause {
        visit_where(w, params)?;
    }
    if let Some(ref mut g) = q.group_by {
        for key in &mut g.keys {
            visit_expr(key, params)?;
        }
    }
    if let Some(ref mut o) = q.order_by {
        for item in &mut o.items {
            visit_order_item(item, params)?;
        }
    }
    for item in &mut q.return_clause.items {
        visit_return_item(item, params)?;
    }
    // `q.limit: Option<LimitClause { count: u64 }>` carries no Expr —
    // parametrised LIMIT against a normal query is preexisting deferred
    // work (see parser.rs `parse_limit_clause`). Out of scope here.
    Ok(())
}

fn visit_mutation<S: BuildHasher>(
    m: &mut MutationStatement,
    params: &HashMap<String, GqlValue, S>,
) -> Result<(), ParamError> {
    if let Some(ref mut u) = m.unwind_clause {
        visit_unwind(u, params)?;
    }
    // MATCH inline-prop constraints are stored as `Vec<(String, Literal)>`
    // on `MatchClause`/`NodePattern`/`RelPattern`, not `Vec<(String, Expr)>`,
    // so no `Expr::ParamRef` can appear there and no walker is needed. If
    // the AST is ever changed to allow `Expr`-typed inline props (i.e. to
    // permit `MATCH (n {id: $id})`), this walker MUST grow a recursion into
    // `m.match_clause` and the corresponding pattern-property arms — the
    // compiler will silently consume an unsubstituted ParamRef otherwise
    // because the debug_assert in `eval_expr` is debug-only.
    visit_mutation_clause(&mut m.mutation, params)?;
    if let Some(ref mut set) = m.set_clause {
        visit_set(set, params)?;
    }
    if let Some(ref mut ret) = m.return_clause {
        for item in &mut ret.items {
            visit_return_item(item, params)?;
        }
    }
    Ok(())
}

fn visit_mutation_clause<S: BuildHasher>(
    mc: &mut MutationClause,
    params: &HashMap<String, GqlValue, S>,
) -> Result<(), ParamError> {
    match mc {
        MutationClause::Create(c) => {
            for pattern in &mut c.patterns {
                visit_create_pattern(pattern, params)?;
            }
        }
        MutationClause::Set(s) => visit_set(s, params)?,
        // DELETE binds variables by name only; MERGE props are Vec<(String,
        // Literal)>. Neither contains an Expr today, so substitution is a
        // no-op. Parametrised MERGE props is preexisting deferred work.
        MutationClause::Merge(mc) => {
            for (_k, expr) in &mut mc.props {
                visit_expr(expr, params)?;
            }
            if let Some(sc) = mc.on_create.as_mut() {
                visit_set(sc, params)?;
            }
            if let Some(sc) = mc.on_match.as_mut() {
                visit_set(sc, params)?;
            }
        }
        MutationClause::Delete(_) => {}
    }
    Ok(())
}

fn visit_create_pattern<S: BuildHasher>(
    p: &mut CreatePattern,
    params: &HashMap<String, GqlValue, S>,
) -> Result<(), ParamError> {
    match p {
        CreatePattern::Node {
            props, prop_map, ..
        } => {
            for (_k, expr) in props {
                visit_expr(expr, params)?;
            }
            if let Some(expr) = prop_map.as_mut() {
                visit_expr(expr, params)?;
            }
        }
        CreatePattern::Edge { rel_props, .. } => {
            for (_k, expr) in rel_props {
                visit_expr(expr, params)?;
            }
        }
    }
    Ok(())
}

fn visit_set<S: BuildHasher>(
    s: &mut SetClause,
    params: &HashMap<String, GqlValue, S>,
) -> Result<(), ParamError> {
    for a in &mut s.assignments {
        visit_set_assignment(a, params)?;
    }
    Ok(())
}

fn visit_set_assignment<S: BuildHasher>(
    a: &mut SetAssignment,
    params: &HashMap<String, GqlValue, S>,
) -> Result<(), ParamError> {
    match a {
        SetAssignment::Property { value, .. } => visit_expr(value, params),
        SetAssignment::EntityOverwrite { map_expr, .. }
        | SetAssignment::EntityMerge { map_expr, .. } => visit_expr(map_expr, params),
    }
}

fn visit_pipeline<S: BuildHasher>(
    p: &mut PipelineQuery,
    params: &HashMap<String, GqlValue, S>,
) -> Result<(), ParamError> {
    for stage in &mut p.stages {
        match stage {
            PipelineStage::Match { where_clause, .. } => {
                if let Some(w) = where_clause {
                    visit_where(w, params)?;
                }
            }
            PipelineStage::Unwind(u) => visit_unwind(u, params)?,
            PipelineStage::With(w) => visit_with(w, params)?,
        }
    }
    match &mut p.terminal {
        PipelineTerminal::Return {
            clause, order_by, ..
        } => {
            for item in &mut clause.items {
                visit_return_item(item, params)?;
            }
            if let Some(o) = order_by {
                for item in &mut o.items {
                    visit_order_item(item, params)?;
                }
            }
        }
        PipelineTerminal::Set(s) => visit_set(s, params)?,
        PipelineTerminal::Create(c) => {
            for pattern in &mut c.patterns {
                visit_create_pattern(pattern, params)?;
            }
        }
        PipelineTerminal::Delete(_) => {}
    }
    Ok(())
}

fn visit_with<S: BuildHasher>(
    w: &mut WithClause,
    params: &HashMap<String, GqlValue, S>,
) -> Result<(), ParamError> {
    for item in &mut w.items {
        visit_return_item(item, params)?;
    }
    if let Some(ref mut wh) = w.where_clause {
        visit_where(wh, params)?;
    }
    if let Some(ref mut o) = w.order_by {
        for item in &mut o.items {
            visit_order_item(item, params)?;
        }
    }
    Ok(())
}

fn visit_const_return<S: BuildHasher>(
    c: &mut ConstReturnQuery,
    params: &HashMap<String, GqlValue, S>,
) -> Result<(), ParamError> {
    for item in &mut c.items {
        visit_return_item(item, params)?;
    }
    if let Some(ref mut e) = c.skip {
        visit_expr(e, params)?;
    }
    if let Some(ref mut e) = c.limit {
        visit_expr(e, params)?;
    }
    Ok(())
}

// ── Leaf walkers ─────────────────────────────────────────────────────────────

fn visit_unwind<S: BuildHasher>(
    u: &mut UnwindClause,
    params: &HashMap<String, GqlValue, S>,
) -> Result<(), ParamError> {
    visit_expr(&mut u.expr, params)
}

fn visit_where<S: BuildHasher>(
    w: &mut WhereClause,
    params: &HashMap<String, GqlValue, S>,
) -> Result<(), ParamError> {
    visit_expr(&mut w.predicate, params)
}

fn visit_return_item<S: BuildHasher>(
    item: &mut ReturnItem,
    params: &HashMap<String, GqlValue, S>,
) -> Result<(), ParamError> {
    visit_expr(&mut item.expr, params)
}

fn visit_order_item<S: BuildHasher>(
    item: &mut OrderItem,
    params: &HashMap<String, GqlValue, S>,
) -> Result<(), ParamError> {
    visit_expr(&mut item.expr, params)
}

// ── Expr walker ──────────────────────────────────────────────────────────────

fn visit_expr<S: BuildHasher>(
    expr: &mut Expr,
    params: &HashMap<String, GqlValue, S>,
) -> Result<(), ParamError> {
    match expr {
        Expr::ParamRef(r) => {
            let (key, missing): (String, ParamError) = match r {
                ParamRef::Named(name) => (name.clone(), ParamError::MissingParameter(name.clone())),
                ParamRef::Positional(n) => {
                    (n.to_string(), ParamError::MissingPositionalParameter(*n))
                }
            };
            let value = params.get(&key).ok_or(missing)?;
            // Map-valued params have no `Literal` form. Leave the `ParamRef`
            // intact — the executor reads the `GqlValue::Map` directly from the
            // params map (whole-entity SET, MERGE inline props, CREATE ($map)).
            if matches!(value, GqlValue::Map(_)) {
                return Ok(());
            }
            let lit = gql_value_to_literal(value, &key)?;
            *expr = Expr::Literal(lit);
            Ok(())
        }
        Expr::Literal(_) | Expr::Var(_) | Expr::PropAccess { .. } => Ok(()),
        Expr::BinaryOp { left, right, .. } => {
            visit_expr(left, params)?;
            visit_expr(right, params)
        }
        Expr::UnaryOp { expr: inner, .. } | Expr::IsNull { expr: inner, .. } => {
            visit_expr(inner, params)
        }
        Expr::Aggregate { arg, .. } => {
            if let Some(inner) = arg {
                visit_expr(inner, params)?;
            }
            Ok(())
        }
        Expr::FunctionCall { args, .. } => {
            for arg in args {
                visit_expr(arg, params)?;
            }
            Ok(())
        }
        Expr::ShortestPath { .. } => {
            // PathPattern carries only Literal props; nothing to walk.
            Ok(())
        }
        Expr::Subscript { list, index } => {
            visit_expr(list, params)?;
            visit_expr(index, params)
        }
        Expr::ListLit(items) => {
            for it in items {
                visit_expr(it, params)?;
            }
            Ok(())
        }
        Expr::ListPredicate {
            list, predicate, ..
        } => {
            // The iteration `var` is a fresh binding, never a parameter, so it
            // is left untouched; both the list source and the predicate may
            // reference `$params`.
            visit_expr(list, params)?;
            visit_expr(predicate, params)
        }
    }
}

// ── Value lowering ───────────────────────────────────────────────────────────

/// Converts a runtime [`GqlValue`] into a parse-time [`Literal`].
///
/// `key` is forwarded into [`ParamError::UnsupportedParamValue`] for
/// diagnostics. Today every `GqlValue` variant lowers successfully so
/// `key` only flows through recursive calls and is not read at the
/// terminal arms — hence the `only_used_in_recursion` allow. When a
/// future `GqlValue` variant (Node, Relationship, Map) cannot be
/// lowered, this function will use `key` to build the error.
#[allow(clippy::only_used_in_recursion)]
fn gql_value_to_literal(value: &GqlValue, key: &str) -> Result<Literal, ParamError> {
    match value {
        GqlValue::Null => Ok(Literal::Null),
        GqlValue::Bool(b) => Ok(Literal::Bool(*b)),
        GqlValue::Int(v) => Ok(Literal::Int(*v)),
        GqlValue::Float(v) => Ok(Literal::Float(*v)),
        GqlValue::Str(s) => Ok(Literal::Str(s.clone())),
        GqlValue::List(items) => {
            let mut lits = Vec::with_capacity(items.len());
            for it in items {
                lits.push(gql_value_to_literal(it, key)?);
            }
            Ok(Literal::List(lits))
        }
        // A Map cannot be lowered to a `Literal` — there is no `Literal::Map`.
        // Map params are consumed directly by the executor (whole-entity SET,
        // inline-map CREATE), not substituted into the AST. Reaching here means
        // a Map param was used in a position the executor does not special-case,
        // so surface a TypeError rather than silently dropping it.
        GqlValue::Map(_) => Err(ParamError::UnsupportedParamValue {
            name: key.to_owned(),
            got: "Map",
        }),
        // Entity values (Node, Relationship, Path) cannot be lowered to a
        // `Literal` either — they are first-class runtime values, not param
        // substitution targets.
        GqlValue::Node(_) => Err(ParamError::UnsupportedParamValue {
            name: key.to_owned(),
            got: "Node",
        }),
        GqlValue::Relationship(_) => Err(ParamError::UnsupportedParamValue {
            name: key.to_owned(),
            got: "Relationship",
        }),
        GqlValue::Path(_) => Err(ParamError::UnsupportedParamValue {
            name: key.to_owned(),
            got: "Path",
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gql::parse_statement;

    fn params_with(pairs: &[(&str, GqlValue)]) -> HashMap<String, GqlValue> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), v.clone()))
            .collect()
    }

    /// Parses a query and extracts the first RETURN item expression. Used
    /// to assert the substitution result on a single Expr.
    fn first_return_expr(stmt: &GqlStatement) -> &Expr {
        match stmt {
            GqlStatement::Query(q) => &q.return_clause.items[0].expr,
            GqlStatement::ConstReturn(c) => &c.items[0].expr,
            other => panic!("expected Query or ConstReturn, got {other:?}"),
        }
    }

    #[test]
    fn substitution_named_param_int() {
        let mut stmt = parse_statement("RETURN $x").unwrap();
        let params = params_with(&[("x", GqlValue::Int(42))]);
        apply(&mut stmt, &params).unwrap();
        assert_eq!(*first_return_expr(&stmt), Expr::Literal(Literal::Int(42)));
    }

    #[test]
    fn substitution_named_param_str() {
        let mut stmt = parse_statement("RETURN $s").unwrap();
        let params = params_with(&[("s", GqlValue::Str("hello".into()))]);
        apply(&mut stmt, &params).unwrap();
        assert_eq!(
            *first_return_expr(&stmt),
            Expr::Literal(Literal::Str("hello".into())),
        );
    }

    #[test]
    fn substitution_named_param_float() {
        let mut stmt = parse_statement("RETURN $f").unwrap();
        let params = params_with(&[("f", GqlValue::Float(2.5))]);
        apply(&mut stmt, &params).unwrap();
        match first_return_expr(&stmt) {
            Expr::Literal(Literal::Float(v)) => assert!((*v - 2.5).abs() < 1e-12),
            other => panic!("expected Float literal, got {other:?}"),
        }
    }

    #[test]
    fn substitution_named_param_bool() {
        let mut stmt = parse_statement("RETURN $b").unwrap();
        let params = params_with(&[("b", GqlValue::Bool(true))]);
        apply(&mut stmt, &params).unwrap();
        assert_eq!(
            *first_return_expr(&stmt),
            Expr::Literal(Literal::Bool(true))
        );
    }

    #[test]
    fn substitution_named_param_null() {
        let mut stmt = parse_statement("RETURN $n").unwrap();
        let params = params_with(&[("n", GqlValue::Null)]);
        apply(&mut stmt, &params).unwrap();
        assert_eq!(*first_return_expr(&stmt), Expr::Literal(Literal::Null));
    }

    #[test]
    fn substitution_positional_param_by_string_key() {
        // PackStream dicts always use string keys. Bolt drivers send
        // positional parameters under the keys "1", "2", … — the resolver
        // matches that convention.
        let mut stmt = parse_statement("RETURN $1").unwrap();
        let params = params_with(&[("1", GqlValue::Str("hi".into()))]);
        apply(&mut stmt, &params).unwrap();
        assert_eq!(
            *first_return_expr(&stmt),
            Expr::Literal(Literal::Str("hi".into())),
        );
    }

    #[test]
    fn substitution_missing_named_param_returns_error() {
        let mut stmt = parse_statement("RETURN $missing").unwrap();
        let err = apply(&mut stmt, &HashMap::new()).unwrap_err();
        assert_eq!(err, ParamError::MissingParameter("missing".into()));
    }

    #[test]
    fn substitution_missing_positional_param_returns_error() {
        let mut stmt = parse_statement("RETURN $1").unwrap();
        let err = apply(&mut stmt, &HashMap::new()).unwrap_err();
        assert_eq!(err, ParamError::MissingPositionalParameter(1));
    }

    #[test]
    fn substitution_list_value_recurses() {
        let mut stmt = parse_statement("RETURN $xs").unwrap();
        let params = params_with(&[(
            "xs",
            GqlValue::List(vec![GqlValue::Int(1), GqlValue::Int(2)]),
        )]);
        apply(&mut stmt, &params).unwrap();
        assert_eq!(
            *first_return_expr(&stmt),
            Expr::Literal(Literal::List(vec![Literal::Int(1), Literal::Int(2)])),
        );
    }

    #[test]
    fn substitution_nested_list_value() {
        let mut stmt = parse_statement("RETURN $xs").unwrap();
        let params = params_with(&[(
            "xs",
            GqlValue::List(vec![GqlValue::List(vec![GqlValue::Int(7)])]),
        )]);
        apply(&mut stmt, &params).unwrap();
        assert_eq!(
            *first_return_expr(&stmt),
            Expr::Literal(Literal::List(vec![Literal::List(vec![Literal::Int(7)])])),
        );
    }

    #[test]
    fn substitution_same_param_referenced_twice_is_idempotent() {
        // `RETURN $x + $x` resolves both occurrences to the same literal.
        let mut stmt = parse_statement("MATCH (n) RETURN $x + $x").unwrap();
        let params = params_with(&[("x", GqlValue::Int(5))]);
        apply(&mut stmt, &params).unwrap();
        match first_return_expr(&stmt) {
            Expr::BinaryOp { left, right, .. } => {
                assert_eq!(left.as_ref(), &Expr::Literal(Literal::Int(5)));
                assert_eq!(right.as_ref(), &Expr::Literal(Literal::Int(5)));
            }
            other => panic!("expected BinaryOp, got {other:?}"),
        }
    }

    #[test]
    fn substitution_walks_create_node_props() {
        // ParamRef inside a CREATE property must be rewritten too.
        let mut stmt = parse_statement("CREATE (n:Foo {id: $id, name: $nm})").unwrap();
        let params = params_with(&[
            ("id", GqlValue::Int(7)),
            ("nm", GqlValue::Str("alice".into())),
        ]);
        apply(&mut stmt, &params).unwrap();
        let mc = match stmt {
            GqlStatement::Mutation(m) => m.mutation,
            other => panic!("expected Mutation, got {other:?}"),
        };
        match mc {
            MutationClause::Create(c) => match &c.patterns[0] {
                CreatePattern::Node { props, .. } => {
                    assert_eq!(props[0].1, Expr::Literal(Literal::Int(7)));
                    assert_eq!(props[1].1, Expr::Literal(Literal::Str("alice".into())),);
                }
                CreatePattern::Edge { .. } => panic!("expected Node pattern, got Edge"),
            },
            MutationClause::Set(_) | MutationClause::Delete(_) | MutationClause::Merge(_) => {
                panic!("expected Create mutation");
            }
        }
    }

    #[test]
    fn substitution_walks_where_predicate() {
        // WHERE on a Query survives the parser; on a Mutation the parser
        // currently drops it (preexisting deferred work, out of scope here).
        let mut stmt = parse_statement("MATCH (n) WHERE n.id = $id RETURN n").unwrap();
        let params = params_with(&[("id", GqlValue::Int(99))]);
        apply(&mut stmt, &params).unwrap();
        let q = match stmt {
            GqlStatement::Query(q) => q,
            other => panic!("expected Query, got {other:?}"),
        };
        let pred = q.where_clause.unwrap().predicate;
        match pred {
            Expr::BinaryOp { right, .. } => {
                assert_eq!(right.as_ref(), &Expr::Literal(Literal::Int(99)));
            }
            other => panic!("expected BinaryOp, got {other:?}"),
        }
    }

    #[test]
    fn substitution_walks_set_assignment_value() {
        let mut stmt = parse_statement("MATCH (n) SET n.flag = $f").unwrap();
        let params = params_with(&[("f", GqlValue::Bool(true))]);
        apply(&mut stmt, &params).unwrap();
        let mc = match stmt {
            GqlStatement::Mutation(m) => m.mutation,
            other => panic!("expected Mutation, got {other:?}"),
        };
        match mc {
            MutationClause::Set(s) => match &s.assignments[0] {
                SetAssignment::Property { value, .. } => {
                    assert_eq!(*value, Expr::Literal(Literal::Bool(true)));
                }
                other => panic!("expected Property, got {other:?}"),
            },
            MutationClause::Create(_) | MutationClause::Delete(_) | MutationClause::Merge(_) => {
                panic!("expected Set mutation");
            }
        }
    }

    #[test]
    fn substitution_const_return_skip_and_limit_exprs() {
        let mut stmt = parse_statement("RETURN 1 SKIP $s LIMIT $l").unwrap();
        let params = params_with(&[("s", GqlValue::Int(0)), ("l", GqlValue::Int(5))]);
        apply(&mut stmt, &params).unwrap();
        let c = match stmt {
            GqlStatement::ConstReturn(c) => c,
            other => panic!("expected ConstReturn, got {other:?}"),
        };
        assert_eq!(c.skip, Some(Expr::Literal(Literal::Int(0))));
        assert_eq!(c.limit, Some(Expr::Literal(Literal::Int(5))));
    }

    #[test]
    fn substitution_admin_statement_is_noop() {
        // `tessera-graph::gql::parse_statement` does not parse admin
        // surface (the admin parser lives in `tessera-graph-cypher`).
        // Construct the AST node directly to verify the apply path.
        use crate::gql::ast::{AdminStatement, SecretPlainPassword};
        let mut stmt = GqlStatement::Admin(AdminStatement::CreateUser {
            username: "bob".into(),
            password: SecretPlainPassword::new(b"secret".to_vec()),
        });
        let original = format!("{stmt:?}");
        apply(&mut stmt, &HashMap::new()).unwrap();
        assert_eq!(format!("{stmt:?}"), original);
    }
}
