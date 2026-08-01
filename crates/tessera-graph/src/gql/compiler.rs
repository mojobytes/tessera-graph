// SPDX-License-Identifier: Apache-2.0

//! GQL-to-PatternBuilder compiler.
//!
//! Lowers a [`GqlQuery`] AST into Layer 2 (`PatternBuilder`) operations,
//! evaluates WHERE predicates, projects RETURN items (with aggregation),
//! applies ORDER BY sorting and LIMIT truncation, and produces a [`GqlResult`].

use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::BuildHasher;
use std::time::Instant;

use crate::access::GraphAccess;
use crate::error::{EdgeId, Error, NodeId};
use crate::property::Property;
use crate::Direction;
use crate::query::pattern::PatternMatch;

use super::ast::{
    AggFunc, AstDirection, BinOp, ConstReturnQuery, EdgeLength, EdgePattern, Expr, GqlQuery,
    Literal, MatchClause, NodePattern,
    OrderByClause, ParamRef, PathPattern, ReturnItem, SetAssignment, UnaryOp,
};

// ── Runtime value types ─────────────────────────────────────────────────────

/// A graph node as a first-class runtime value (Bolt Node, tag `0x4E`).
#[derive(Debug, Clone, PartialEq)]
pub struct GqlNode {
    /// Stable node id.
    pub id: i64,
    /// Node labels.
    pub labels: Vec<String>,
    /// Node properties.
    pub props: std::collections::HashMap<String, GqlValue>,
}

/// A graph relationship as a first-class runtime value (Bolt Relationship, tag `0x52`).
#[derive(Debug, Clone, PartialEq)]
pub struct GqlRelationship {
    /// Stable relationship id.
    pub id: i64,
    /// Start node id.
    pub start_id: i64,
    /// End node id.
    pub end_id: i64,
    /// Relationship type.
    pub rel_type: String,
    /// Relationship properties.
    pub props: std::collections::HashMap<String, GqlValue>,
}

/// A path as a first-class runtime value (Bolt Path, tag `0x50`).
///
/// Upholds the Neo4j invariant `nodes.len() == rels.len() + 1`. Construct only
/// via the `path_materialization` module (added in a later task), which
/// validates the invariant.
#[derive(Debug, Clone, PartialEq)]
pub struct GqlPath {
    /// Path nodes in traversal order.
    pub nodes: Vec<GqlNode>,
    /// Path relationships in traversal order.
    pub rels: Vec<GqlRelationship>,
}

/// A runtime value produced by GQL expression evaluation.
#[derive(Debug, Clone, PartialEq)]
pub enum GqlValue {
    /// The SQL/GQL NULL value.
    Null,
    /// A boolean value.
    Bool(bool),
    /// A 64-bit signed integer.
    Int(i64),
    /// A 64-bit IEEE 754 float.
    Float(f64),
    /// A UTF-8 string.
    Str(String),
    /// A list of values (produced by `COLLECT`).
    List(Vec<Self>),
    /// A property map, produced by `$map` params (a `Dict` on the wire).
    ///
    /// Cannot be stored directly as a graph `Property` — used only as an
    /// intermediate during `SET n = $map` / `SET n += $map` and inline-map
    /// expansion (`CREATE (n $map)`, `MERGE (n {…: $p})`). When projected
    /// back over Bolt it serialises as a `Dict`.
    Map(std::collections::HashMap<String, Self>),
    /// A graph node (Bolt struct `0x4E`).
    Node(GqlNode),
    /// A graph relationship (Bolt struct `0x52`).
    Relationship(GqlRelationship),
    /// A path (Bolt struct `0x50`).
    Path(GqlPath),
}

/// A single result row: column name → value.
pub type GqlRow = HashMap<String, GqlValue>;

/// The complete result of a GQL query execution.
pub type GqlResult = Vec<GqlRow>;

/// Marker for result-cap aborts.
///
/// Embedded in the [`Error::GqlCompileError`] message when a query is
/// aborted by the defensive result-row cap (`max_rows` arg of [`execute`]).
/// The server side (`graph_accessor::engine_err_to_string`) matches this
/// marker to remap the error to the wire code
/// `Neo.ClientError.General.ResultExhausted`. Keep in sync with the
/// server-side sentinel.
pub const RESULT_CAP_MSG_PREFIX: &str = "__result_cap__: ";

/// Marker prefix for query-timeout aborts (v0.6.0 Fase 2 Task 6).
///
/// Embedded in the [`Error::GqlCompileError`] message when a query is aborted
/// by the cooperative deadline checked inside the engine's hot loops. The
/// server boundary (`graph_accessor::engine_err_to_string`) matches this
/// marker to remap the error to the wire code
/// `Neo.ClientError.Statement.ExecutionFailed` — a non-retryable `ClientError`,
/// so the driver does not re-run the same expensive query. Mirrors
/// [`RESULT_CAP_MSG_PREFIX`]. Keep in sync with the server-side sentinel.
pub const TIMEOUT_MSG_PREFIX: &str = "__query_timeout__: ";

/// Bitmask controlling how often [`check_deadline`] reads the wall clock.
///
/// The clock is read only when `counter & DEADLINE_CHECK_MASK == 0`, i.e. once
/// every 1024 iterations. Chosen so a runaway loop (millions of iterations) is
/// cut promptly while the per-iteration cost — a single mask + branch on the
/// disabled path — stays negligible. The resolution (≤1024 iterations of slack)
/// is far finer than any timeout a runaway query needs.
const DEADLINE_CHECK_MASK: u64 = 0x3FF;

/// Cooperative deadline check for the engine's hot loops.
///
/// Called as `check_deadline(deadline, i)?` with the loop's iteration index
/// `i`. When `deadline` is `None` (timeout disabled) the only work done is the
/// bitmask test, so the disabled path costs one mask + branch per call and
/// never touches the clock. When a deadline is set, the clock is read once
/// every 1024 iterations; on expiry it returns an [`Error::GqlCompileError`]
/// carrying [`TIMEOUT_MSG_PREFIX`].
/// The single canonical timeout error carrying [`TIMEOUT_MSG_PREFIX`].
fn timeout_error() -> Error {
    Error::GqlCompileError(format!("{TIMEOUT_MSG_PREFIX}query exceeded time budget"))
}

#[inline]
fn check_deadline(deadline: Option<Instant>, counter: u64) -> crate::Result<()> {
    // Bitmask first: with `deadline == None` this is the only work done.
    if counter & DEADLINE_CHECK_MASK != 0 {
        return Ok(());
    }
    if let Some(d) = deadline {
        if Instant::now() >= d {
            return Err(timeout_error());
        }
    }
    Ok(())
}

/// Out-of-band abort signal for deadline checks inside infallible code paths.
///
/// Most of the engine's hot loops return [`crate::Result`], so they abort by
/// propagating an `Err` from [`check_deadline`]. The `shortestPath` BFS
/// (`shortest_path_bfs_constrained`) is reached through the infallible
/// `eval_expr` chain (which returns [`GqlValue`], not `Result`), so it cannot
/// propagate an `Err` without making the entire expression evaluator fallible.
///
/// Instead, the BFS checks the deadline every 1024 iterations and, on expiry,
/// sets this flag and returns early. The materialization loop in [`execute`]
/// owns the `DeadlineAbort`, passes `&self` down the `eval_expr` chain, and
/// after projecting each row inspects [`DeadlineAbort::is_aborted`]; if set, it
/// returns the timeout `Err`. This keeps the abort an explicit parameter (no
/// thread-local / ambient state) while covering the one runaway loop that lives
/// behind an infallible boundary.
#[derive(Debug, Default)]
pub struct DeadlineAbort {
    deadline: Option<Instant>,
    aborted: std::cell::Cell<bool>,
}

impl DeadlineAbort {
    /// A no-deadline cell: every check is a no-op. Used by callers without a
    /// query timeout and by the disabled path.
    #[must_use]
    pub const fn none() -> Self {
        Self {
            deadline: None,
            aborted: std::cell::Cell::new(false),
        }
    }

    /// A cell carrying the cooperative `deadline` for the infallible
    /// `eval_expr` path. `None` behaves like [`DeadlineAbort::none`].
    #[must_use]
    pub const fn new(deadline: Option<Instant>) -> Self {
        Self {
            deadline,
            aborted: std::cell::Cell::new(false),
        }
    }

    /// Trips the abort flag. Called by the BFS when its deadline expires.
    fn mark(&self) {
        self.aborted.set(true);
    }

    /// Returns `true` once the deadline has tripped this cell.
    fn is_aborted(&self) -> bool {
        self.aborted.get()
    }

    /// Checks the carried deadline every 1024 iterations (via the shared
    /// [`check_deadline`] cadence) and, on expiry, trips the abort flag and
    /// returns `true`. Infallible callers (the `shortestPath` BFS) use this to
    /// signal a timeout that the materialization loop later turns into an
    /// `Err`. `false` means "keep going".
    pub(crate) fn tripped(&self, counter: u64) -> bool {
        if self.aborted.get() {
            return true;
        }
        if check_deadline(self.deadline, counter).is_err() {
            self.mark();
            return true;
        }
        false
    }
}

// ── Conversion helpers ──────────────────────────────────────────────────────

/// Converts an AST [`Literal`] into a runtime [`GqlValue`].
fn compile_literal(lit: &Literal) -> GqlValue {
    match lit {
        Literal::Int(v) => GqlValue::Int(*v),
        Literal::Float(v) => GqlValue::Float(*v),
        Literal::Str(s) => GqlValue::Str(s.clone()),
        Literal::Bool(b) => GqlValue::Bool(*b),
        Literal::Null => GqlValue::Null,
        Literal::List(items) => GqlValue::List(items.iter().map(compile_literal).collect()),
    }
}

/// Converts a graph [`Property`] into a runtime [`GqlValue`].
#[must_use]
pub fn gql_value_from_property(p: &Property) -> GqlValue {
    match p {
        Property::String(s) => GqlValue::Str(s.clone()),
        Property::I64(v) => GqlValue::Int(*v),
        Property::F64(v) => GqlValue::Float(*v),
        Property::Bool(b) => GqlValue::Bool(*b),
        Property::Bytes(_) => GqlValue::Null,
    }
}

/// Converts an AST [`Literal`] into a [`Property`] for use in
/// `PatternBuilder::where_prop` calls. Returns `None` for `Null` and `List`
/// (since `Property` has no null or list variant).
#[doc(hidden)]
#[must_use]
pub fn literal_to_property(lit: &Literal) -> Option<Property> {
    match lit {
        Literal::Int(v) => Some(Property::I64(*v)),
        Literal::Float(v) => Some(Property::F64(*v)),
        Literal::Str(s) => Some(Property::String(s.clone())),
        Literal::Bool(b) => Some(Property::Bool(*b)),
        Literal::Null | Literal::List(_) => None,
    }
}

/// Converts a runtime [`GqlValue`] into a [`Property`].
///
/// Returns `None` for `Null` and `List` (since `Property` has no null or list
/// variant).
#[must_use]
pub fn gql_value_to_property(val: &GqlValue) -> Option<Property> {
    match val {
        GqlValue::Int(v) => Some(Property::I64(*v)),
        GqlValue::Float(v) => Some(Property::F64(*v)),
        GqlValue::Str(s) => Some(Property::String(s.clone())),
        GqlValue::Bool(b) => Some(Property::Bool(*b)),
        GqlValue::Null
        | GqlValue::List(_)
        | GqlValue::Map(_)
        | GqlValue::Node(_)
        | GqlValue::Relationship(_)
        | GqlValue::Path(_) => None,
    }
}

/// Resolves CREATE pattern property expressions to concrete `(key, Property)` pairs.
///
/// Evaluates each [`Expr`] against the pattern match context and optional UNWIND
/// variable. Null and List values are silently skipped (no corresponding
/// [`Property`] variant).
pub fn resolve_create_props<G: GraphAccess + ?Sized>(
    props: &[(String, Expr)],
    pm: &PatternMatch,
    graph: &G,
    unwind_var: Option<(&str, &GqlValue)>,
) -> HashMap<String, Property> {
    let mut result = HashMap::with_capacity(props.len());
    for (key, expr) in props {
        let val = if let Some((uvar, uelem)) = unwind_var {
            eval_expr_with_unwind_var(expr, pm, graph, uvar, uelem)
        } else {
            // CREATE prop expressions are not a runaway-BFS site; no deadline.
            // No path binding is in scope for CREATE props.
            eval_expr(expr, pm, &PathBindings::new(), graph, &DeadlineAbort::none())
        };
        if let Some(prop) = gql_value_to_property(&val) {
            result.insert(key.clone(), prop);
        }
    }
    result
}

/// Overwrites all properties on `node_id` with the entries from `map`.
///
/// Existing properties not present in `map` are removed (overwrite semantics,
/// `SET n = $map`). Map entries whose values cannot lower to a scalar
/// `Property` (nested Map / Null / List) are skipped.
///
/// # Errors
///
/// Propagates a storage error if the node cannot be read or written.
pub fn apply_map_to_node_overwrite<S: BuildHasher>(
    graph: &mut crate::Graph,
    node_id: crate::NodeId,
    map: &HashMap<String, GqlValue, S>,
) -> crate::Result<()> {
    let mut node = graph.node(node_id)?;
    node.properties_mut().clear();
    for (k, v) in map {
        if let Some(prop) = gql_value_to_property(v) {
            node.properties_mut().insert(k.clone(), prop);
        }
    }
    graph.update_node(node_id, &node)
}

/// Merges entries from `map` into the properties of `node_id`.
///
/// Existing properties not present in `map` are preserved (merge semantics,
/// `SET n += $map`). Map entries whose values cannot lower to a scalar
/// `Property` are skipped.
///
/// # Errors
///
/// Propagates a storage error if the node cannot be read or written.
pub fn apply_map_to_node_merge<S: BuildHasher>(
    graph: &mut crate::Graph,
    node_id: crate::NodeId,
    map: &HashMap<String, GqlValue, S>,
) -> crate::Result<()> {
    let mut node = graph.node(node_id)?;
    for (k, v) in map {
        if let Some(prop) = gql_value_to_property(v) {
            node.properties_mut().insert(k.clone(), prop);
        }
    }
    graph.update_node(node_id, &node)
}

/// Evaluates a GQL expression against a pattern match context.
///
/// Public wrapper around the internal `eval_expr` function for use by
/// external crates (e.g., the server) that need to evaluate UNWIND list
/// expressions.
pub fn execute_expr<G: GraphAccess + ?Sized>(
    expr: &Expr,
    pm: &PatternMatch,
    graph: &G,
) -> GqlValue {
    // External callers (server UNWIND list eval) evaluate bounded list
    // expressions, not runaway BFS; no deadline is threaded here. No path
    // binding is in scope for these external entry points.
    eval_expr(expr, pm, &PathBindings::new(), graph, &DeadlineAbort::none())
}

/// Maps an AST direction to a query direction.
const fn compile_direction(d: AstDirection) -> Direction {
    match d {
        AstDirection::Outgoing => Direction::Outgoing,
        AstDirection::Incoming => Direction::Incoming,
        AstDirection::Both => Direction::Both,
    }
}

// ── Expression evaluation ───────────────────────────────────────────────────

/// Evaluates an AST [`Expr`] against a [`PatternMatch`] row, returning a
/// runtime [`GqlValue`].
//
// Two arms return `GqlValue::Null` for unrelated reasons: `Aggregate` because
// aggregates resolve at the group level (this scalar evaluator only handles
// row-level expressions), and `ParamRef` because reaching it means
// substitution was skipped (a defensive recovery — the debug_assert above
// fires first in debug builds). Keeping them as separate arms makes the
// intent explicit; merging them with `|` would obscure that.
/// Per-row materialised paths, keyed by the `MATCH p = (…)` variable name.
///
/// Lives in the GQL layer (not `query::pattern`) so `PatternMatch` stays free
/// of compiler-layer value types. Threaded alongside `pm` through `eval_expr`
/// so `nodes(p)`/`relationships(p)`/`length(p)` and `RETURN p` resolve. Empty
/// for queries without a path binding.
type PathBindings = HashMap<String, GqlPath>;

/// Single-source implementation of the path functions, shared by both
/// dispatchers (`eval_function_call` and `eval_builtin_function_call`) so the
/// two never drift (cf. the issue-#15 single-dispatcher bug). `name` is the
/// lowercased function name; `path` is the materialised path for the argument.
fn compute_path_function(name: &str, path: &GqlPath) -> GqlValue {
    match name {
        "nodes" => {
            GqlValue::List(path.nodes.iter().cloned().map(GqlValue::Node).collect())
        }
        "relationships" => GqlValue::List(
            path.rels.iter().cloned().map(GqlValue::Relationship).collect(),
        ),
        #[allow(clippy::cast_possible_wrap)]
        "length" => GqlValue::Int(path.rels.len() as i64),
        // Not a path function — caller must not reach here.
        _ => GqlValue::Null,
    }
}

#[allow(clippy::match_same_arms)]
fn eval_expr<G: GraphAccess + ?Sized>(
    expr: &Expr,
    pm: &PatternMatch,
    paths: &PathBindings,
    graph: &G,
    abort: &DeadlineAbort,
) -> GqlValue {
    // Defensive: `param_substitution::apply` must have run between parse
    // and compile, replacing every `Expr::ParamRef` with a `Literal`.
    // Reaching here means a programming error in the caller. Wired in
    // cycle 6 of the parser fix.
    debug_assert!(
        !matches!(expr, Expr::ParamRef(_)),
        "unsubstituted ParamRef reached eval_expr — param_substitution::apply was skipped",
    );

    match expr {
        Expr::Literal(lit) => compile_literal(lit),
        Expr::ParamRef(_) => GqlValue::Null,

        Expr::Var(name) => {
            // ISO GQL / Cypher: a bare variable reference yields the full entity
            // (node/edge with id, labels/type and properties). Since Fase B,
            // `GqlValue` has Node/Relationship variants, so we project the
            // first-class entity. COUNT(n) still counts non-null bindings (a
            // Node is never Null); ORDER BY n orders by identity (entities are
            // not orderable scalars, so the comparator yields None — stable).
            // A `MATCH p = (…)` path variable resolves to the materialised
            // `GqlValue::Path` (checked first; a path var never collides with a
            // node/edge var).
            #[allow(clippy::option_if_let_else)]
            if let Some(path) = paths.get(name) {
                GqlValue::Path(path.clone())
            } else if let Ok(node) = pm.get_node(name) {
                GqlValue::Node(gql_node_from_entity(node))
            } else if let Ok(edge) = pm.get_edge(name) {
                GqlValue::Relationship(gql_relationship_from_entity(edge))
            } else {
                GqlValue::Null
            }
        }

        Expr::Aggregate { .. } => GqlValue::Null,

        Expr::PropAccess { var, prop } => pm
            .get_node(var)
            .map_or_else(
                |_| {
                    pm.get_edge(var).map_or(GqlValue::Null, |edge| {
                        edge.properties()
                            .get(prop)
                            .map_or(GqlValue::Null, gql_value_from_property)
                    })
                },
                |node| {
                    node.properties()
                        .get(prop)
                        .map_or(GqlValue::Null, gql_value_from_property)
                },
            ),

        Expr::BinaryOp { left, op, right } => {
            let lv = eval_expr(left, pm, paths, graph, abort);
            let rv = eval_expr(right, pm, paths, graph, abort);
            eval_binary_op(&lv, *op, &rv)
        }

        Expr::UnaryOp { op, expr: inner } => {
            let v = eval_expr(inner, pm, paths, graph, abort);
            eval_unary_op(*op, &v)
        }

        Expr::IsNull { expr: inner, negated } => {
            let v = eval_expr(inner, pm, paths, graph, abort);
            let is_null = v == GqlValue::Null;
            GqlValue::Bool(if *negated { !is_null } else { is_null })
        }

        Expr::FunctionCall { name, args } => {
            eval_function_call(name, args, pm, paths, graph, abort)
        }

        Expr::ShortestPath { pattern } => eval_shortest_path_pattern(pattern, pm, graph, abort),

        // `ListLit` and `Subscript` are evaluated recursively against the
        // `PatternMatch`. They are primarily used inside pipeline stages
        // (via `eval_expr_on_binding`), but WHERE / ORDER BY in legacy
        // `MATCH ... RETURN` queries can also contain them, so the
        // evaluator must return actual values here rather than silently
        // collapsing to `Null`.
        Expr::ListLit(items) => {
            let vals: Vec<GqlValue> =
                items.iter().map(|e| eval_expr(e, pm, paths, graph, abort)).collect();
            GqlValue::List(vals)
        }
        Expr::Subscript { list, index } => {
            let list_val = eval_expr(list, pm, paths, graph, abort);
            let index_val = eval_expr(index, pm, paths, graph, abort);
            eval_subscript(&list_val, &index_val)
        }
        Expr::ListPredicate { kind, var, list, predicate } => {
            eval_list_predicate(*kind, var, list, predicate, pm, paths, graph, abort)
        }
    }
}

/// Evaluates a scalar function call against a [`PatternMatch`] row.
///
/// Supported functions:
/// - `id(var)` — returns `GqlValue::Int(node_id)` for nodes.
/// - `type(var)` — returns `GqlValue::Str(edge_label)` for edges.
/// - `labels(var)` — returns `GqlValue::List([GqlValue::Str(label)])` for nodes.
/// - `shortestPath(a, b)` — returns `GqlValue::List` of node IDs or `Null`.
fn eval_function_call<G: GraphAccess + ?Sized>(
    name: &str,
    args: &[super::ast::Expr],
    pm: &PatternMatch,
    paths: &PathBindings,
    graph: &G,
    abort: &DeadlineAbort,
) -> GqlValue {
    if name == "shortestpath" {
        return eval_shortest_path(args, pm, graph, abort);
    }

    // Path functions: `nodes(p)`/`relationships(p)`/`length(p)` over a
    // `MATCH p = (…)` binding. The argument must be the path variable; the
    // path is materialised per-row in `paths`. Shares `compute_path_function`
    // with the pipeline dispatcher (single-source, no drift).
    if matches!(name, "nodes" | "relationships" | "length") {
        let Some(super::ast::Expr::Var(pv)) = args.first() else {
            return GqlValue::Null;
        };
        return paths
            .get(pv)
            .map_or(GqlValue::Null, |path| compute_path_function(name, path));
    }

    // Scalar builtins whose arguments are ordinary expressions (not entity
    // variables). They must resolve in EVERY evaluation context that reaches
    // `eval_expr` — read UNWIND source, mutation UNWIND source, CREATE prop
    // expressions, WHERE/RETURN — not only the pipeline binding evaluator.
    // Args are evaluated against the `PatternMatch` row via `eval_expr`. See
    // the mirror in `eval_builtin_function_call`, which shares the same
    // `compute_*` helpers but evaluates args against a pipeline `Binding`.
    match name {
        "range" => {
            if args.len() != 2 {
                return GqlValue::Null;
            }
            let start = eval_expr(&args[0], pm, paths, graph, abort);
            let end = eval_expr(&args[1], pm, paths, graph, abort);
            return compute_range(&start, &end);
        }
        "size" => {
            let Some(arg) = args.first() else {
                return GqlValue::Null;
            };
            let val = eval_expr(arg, pm, paths, graph, abort);
            return compute_size(&val);
        }
        "tolower" => {
            let Some(arg) = args.first() else {
                return GqlValue::Null;
            };
            let val = eval_expr(arg, pm, paths, graph, abort);
            return compute_to_lower(&val);
        }
        "toupper" => {
            let Some(arg) = args.first() else {
                return GqlValue::Null;
            };
            let val = eval_expr(arg, pm, paths, graph, abort);
            return compute_to_upper(&val);
        }
        "coalesce" => {
            let evaluated: Vec<GqlValue> =
                args.iter().map(|a| eval_expr(a, pm, paths, graph, abort)).collect();
            return compute_coalesce(&evaluated);
        }
        _ => {}
    }

    // Entity-bound builtins (`id`, `type`, `labels`) require an `Expr::Var`
    // argument that names a node/edge bound by a prior MATCH.
    let Some(arg) = args.first() else {
        return GqlValue::Null;
    };

    let var_name = match arg {
        super::ast::Expr::Var(v) => v.as_str(),
        _ => return GqlValue::Null,
    };

    match name {
        "id" => {
            #[allow(clippy::cast_possible_wrap)]
            pm.get_node(var_name)
                .map_or(GqlValue::Null, |node| GqlValue::Int(node.id().as_u64() as i64))
        }
        "type" => {
            pm.get_edge(var_name)
                .map_or(GqlValue::Null, |edge| GqlValue::Str(edge.label().to_owned()))
        }
        "labels" => {
            pm.get_node(var_name).map_or(GqlValue::Null, |node| {
                GqlValue::List(vec![GqlValue::Str(node.label().to_owned())])
            })
        }
        "properties" => pm.get_node(var_name).map_or_else(
            |_| {
                pm.get_edge(var_name)
                    .map_or(GqlValue::Null, |edge| compute_properties(edge.properties()))
            },
            |node| compute_properties(node.properties()),
        ),
        // Unknown function — return NULL rather than panic.
        _ => GqlValue::Null,
    }
}

/// Applies list-predicate quantifier semantics to a list of already-bound
/// elements, evaluating `predicate_of` for each.
///
/// `predicate_of` returns `Some(true)`/`Some(false)` for a boolean result, or
/// `None` when the predicate evaluated to a non-boolean / `Null` for that
/// element. The empty-list base cases follow Cypher: `ALL`/`NONE` are
/// vacuously `true`, `ANY`/`SINGLE` are `false`.
///
/// `None`-valued elements are treated as not-satisfied for the purpose of the
/// `ALL`/`ANY`/`NONE`/`SINGLE` counts. (Full SQL-style three-valued null
/// propagation is a future refinement; the pilot's predicates are total.)
fn apply_list_quantifier(
    kind: super::ast::ListPredKind,
    items: &[GqlValue],
    mut predicate_of: impl FnMut(&GqlValue) -> Option<bool>,
) -> GqlValue {
    use super::ast::ListPredKind::{All, Any, None, Single};
    let mut matched = 0_usize;
    let mut failed = 0_usize;
    for item in items {
        if predicate_of(item) == Some(true) {
            matched += 1;
        } else {
            failed += 1;
        }
    }
    let result = match kind {
        All => failed == 0,
        Any => matched > 0,
        None => matched == 0,
        Single => matched == 1,
    };
    GqlValue::Bool(result)
}

/// Evaluates a list predicate (`ALL`/`ANY`/`NONE`/`SINGLE`) against a
/// [`PatternMatch`] row.
///
/// Binds `var` to each element of the evaluated `list` and tests `predicate`
/// with `var` in scope alongside the outer MATCH bindings, then applies the
/// quantifier. A non-list (or `Null`) source yields `Null`.
#[allow(clippy::too_many_arguments)]
fn eval_list_predicate<G: GraphAccess + ?Sized>(
    kind: super::ast::ListPredKind,
    var: &str,
    list: &Expr,
    predicate: &Expr,
    pm: &PatternMatch,
    paths: &PathBindings,
    graph: &G,
    abort: &DeadlineAbort,
) -> GqlValue {
    // The list source may itself be `relationships(p)` / `nodes(p)`, so it must
    // see the path bindings. The predicate runs per element against a pipeline
    // `Binding` (iteration var in scope) — it never references the path var.
    let GqlValue::List(items) = eval_expr(list, pm, paths, graph, abort) else {
        return GqlValue::Null;
    };
    // Carry the outer MATCH bindings so the predicate can reference them
    // (e.g. `x > n.threshold`) alongside the iteration variable.
    let mut binding = Binding { pm: pm.clone(), vals: HashMap::new() };
    apply_list_quantifier(kind, &items, |item| {
        binding.vals.insert(var.to_owned(), item.clone());
        match eval_expr_on_binding(predicate, &binding, graph) {
            GqlValue::Bool(b) => Some(b),
            _ => Option::None,
        }
    })
}

/// Evaluates `shortestPath(a, b)` by running BFS from node `a` to node `b`.
///
/// Returns `GqlValue::List` of node IDs (as integers) or `GqlValue::Null` if
/// unreachable. Same-node returns a single-element list.
fn eval_shortest_path<G: GraphAccess + ?Sized>(
    args: &[super::ast::Expr],
    pm: &PatternMatch,
    graph: &G,
    abort: &DeadlineAbort,
) -> GqlValue {
    let (Some(first), Some(second)) = (args.first(), args.get(1)) else {
        return GqlValue::Null;
    };
    let (Expr::Var(from_var), Expr::Var(to_var)) = (first, second) else {
        return GqlValue::Null;
    };
    let (Ok(from_node), Ok(to_node)) = (pm.get_node(from_var), pm.get_node(to_var)) else {
        return GqlValue::Null;
    };
    let from_id = from_node.id();
    let to_id = to_node.id();

    shortest_path_bfs(graph, from_id, to_id, abort).map_or(GqlValue::Null, |path| {
        #[allow(clippy::cast_possible_wrap)]
        let ids: Vec<GqlValue> = path
            .into_iter()
            .map(|nid| GqlValue::Int(nid.as_u64() as i64))
            .collect();
        GqlValue::List(ids)
    })
}

/// BFS from `from` to `to`, returning the shortest path as a list of node IDs
/// (including both endpoints), or `None` if unreachable.
///
/// Follows outgoing edges only (consistent with Cypher's default directed
/// `shortestPath` semantics).
fn shortest_path_bfs<G: GraphAccess + ?Sized>(
    graph: &G,
    from: NodeId,
    to: NodeId,
    abort: &DeadlineAbort,
) -> Option<Vec<NodeId>> {
    if from == to {
        return Some(vec![from]);
    }

    let mut visited: HashSet<NodeId> = HashSet::new();
    visited.insert(from);
    let mut parent: HashMap<NodeId, NodeId> = HashMap::new();
    let mut queue: VecDeque<NodeId> = VecDeque::new();
    queue.push_back(from);
    let mut counter = 0_u64;

    while let Some(current) = queue.pop_front() {
        // Cooperative deadline: this BFS runs inside the infallible eval_expr
        // path, so on expiry we trip the abort cell and bail; the
        // materialization loop turns the tripped cell into a timeout `Err`.
        if abort.tripped(counter) {
            return None;
        }
        counter += 1;
        let Ok(edges) = graph.outgoing_edges(current) else { continue };
        for edge in &edges {
            let next = edge.target();
            if visited.contains(&next) {
                continue;
            }
            visited.insert(next);
            parent.insert(next, current);
            if next == to {
                // Reconstruct path
                let mut path = vec![to];
                let mut node = to;
                while node != from {
                    node = parent[&node];
                    path.push(node);
                }
                path.reverse();
                return Some(path);
            }
            queue.push_back(next);
        }
    }

    None
}

/// Evaluates a Cypher-style `shortestPath((a)-[*..N]->(b))` expression.
///
/// Resolves start and end node IDs from the MATCH bindings, extracts hop-limit,
/// label filter, and direction from the path pattern, then runs a constrained BFS.
fn eval_shortest_path_pattern<G: GraphAccess + ?Sized>(
    pattern: &PathPattern,
    pm: &PatternMatch,
    graph: &G,
    abort: &DeadlineAbort,
) -> GqlValue {
    // Must have exactly one hop
    if pattern.hops.len() != 1 {
        return GqlValue::Null;
    }
    let (ref edge_pattern, ref end_node) = pattern.hops[0];

    // Resolve start/end from MATCH bindings
    let Some(start_var) = pattern.start.var.as_deref() else {
        return GqlValue::Null;
    };
    let Some(end_var) = end_node.var.as_deref() else {
        return GqlValue::Null;
    };
    let Ok(from_node) = pm.get_node(start_var) else {
        return GqlValue::Null;
    };
    let Ok(to_node) = pm.get_node(end_var) else {
        return GqlValue::Null;
    };
    let from_id = from_node.id();
    let to_id = to_node.id();

    // Extract constraints
    let max_depth = match edge_pattern.length {
        EdgeLength::Variable { max, .. } => max,
        EdgeLength::Fixed => Some(1),
    };
    let label_filter = edge_pattern.labels.first().map(String::as_str);
    let direction = compile_direction(edge_pattern.direction);

    // Run constrained BFS
    shortest_path_bfs_constrained(graph, from_id, to_id, max_depth, label_filter, direction, abort)
        .map_or(GqlValue::Null, |path| {
            #[allow(clippy::cast_possible_wrap)]
            let ids: Vec<GqlValue> = path
                .into_iter()
                .map(|nid| GqlValue::Int(nid.as_u64() as i64))
                .collect();
            GqlValue::List(ids)
        })
}

/// BFS from `from` to `to` with optional depth limit, label filter, and direction.
///
/// Returns the shortest path as a list of node IDs (including both endpoints),
/// or `None` if unreachable within the constraints.
#[allow(clippy::too_many_arguments)]
fn shortest_path_bfs_constrained<G: GraphAccess + ?Sized>(
    graph: &G,
    from: NodeId,
    to: NodeId,
    max_depth: Option<u32>,
    label_filter: Option<&str>,
    direction: Direction,
    abort: &DeadlineAbort,
) -> Option<Vec<NodeId>> {
    if from == to {
        return Some(vec![from]);
    }

    let depth_limit = max_depth.unwrap_or(VARIABLE_HOP_SAFETY_CAP);
    let mut visited: HashSet<NodeId> = HashSet::new();
    visited.insert(from);
    let mut parent: HashMap<NodeId, NodeId> = HashMap::new();
    let mut depth: HashMap<NodeId, u32> = HashMap::new();
    depth.insert(from, 0);
    let mut queue: VecDeque<NodeId> = VecDeque::new();
    queue.push_back(from);
    let mut counter = 0_u64;

    while let Some(current) = queue.pop_front() {
        // Cooperative deadline (infallible path): trip the abort cell on expiry
        // and bail; the materialization loop maps the tripped cell to a timeout.
        if abort.tripped(counter) {
            return None;
        }
        counter += 1;
        let current_depth = depth[&current];
        if current_depth >= depth_limit {
            continue;
        }

        // Collect neighbor node IDs from edges based on direction
        let mut neighbors: Vec<NodeId> = Vec::new();

        if matches!(direction, Direction::Outgoing | Direction::Both) {
            if let Ok(edges) = graph.outgoing_edges(current) {
                for edge in &edges {
                    if let Some(lf) = label_filter {
                        if edge.label() != lf {
                            continue;
                        }
                    }
                    neighbors.push(edge.target());
                }
            }
        }

        if matches!(direction, Direction::Incoming | Direction::Both) {
            if let Ok(edges) = graph.incoming_edges(current) {
                for edge in &edges {
                    if let Some(lf) = label_filter {
                        if edge.label() != lf {
                            continue;
                        }
                    }
                    neighbors.push(edge.source());
                }
            }
        }

        for next in neighbors {
            if visited.contains(&next) {
                continue;
            }
            visited.insert(next);
            parent.insert(next, current);
            depth.insert(next, current_depth + 1);
            if next == to {
                // Reconstruct path
                let mut path = vec![to];
                let mut node = to;
                while node != from {
                    node = parent[&node];
                    path.push(node);
                }
                path.reverse();
                return Some(path);
            }
            queue.push_back(next);
        }
    }

    None
}

/// Evaluates a binary operation on two [`GqlValue`]s.
fn eval_binary_op(lv: &GqlValue, op: BinOp, rv: &GqlValue) -> GqlValue {
    match op {
        // SQL/GQL three-valued logic for AND/OR
        BinOp::And => match (eval_as_tribool(lv), eval_as_tribool(rv)) {
            (Some(false), _) | (_, Some(false)) => GqlValue::Bool(false),
            (Some(true), Some(true)) => GqlValue::Bool(true),
            _ => GqlValue::Null,
        },
        BinOp::Or => match (eval_as_tribool(lv), eval_as_tribool(rv)) {
            (Some(true), _) | (_, Some(true)) => GqlValue::Bool(true),
            (Some(false), Some(false)) => GqlValue::Bool(false),
            _ => GqlValue::Null,
        },

        BinOp::Eq => GqlValue::Bool(gql_value_eq(lv, rv)),
        BinOp::NotEq => GqlValue::Bool(!gql_value_eq(lv, rv)),

        BinOp::Lt | BinOp::Gt | BinOp::LtEq | BinOp::GtEq => {
            eval_comparison(lv, op, rv)
        }

        BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div => {
            eval_arithmetic(lv, op, rv)
        }

        BinOp::StartsWith => {
            match (lv, rv) {
                (GqlValue::Str(s), GqlValue::Str(prefix)) => GqlValue::Bool(s.starts_with(prefix.as_str())),
                _ => GqlValue::Null,
            }
        }

        BinOp::EndsWith => {
            match (lv, rv) {
                (GqlValue::Str(s), GqlValue::Str(suffix)) => GqlValue::Bool(s.ends_with(suffix.as_str())),
                _ => GqlValue::Null,
            }
        }

        BinOp::Contains => {
            match (lv, rv) {
                (GqlValue::Str(s), GqlValue::Str(needle)) => GqlValue::Bool(s.contains(needle.as_str())),
                _ => GqlValue::Null,
            }
        }

        BinOp::In => {
            match rv {
                GqlValue::List(list) => GqlValue::Bool(list.iter().any(|item| gql_value_eq(lv, item))),
                _ => GqlValue::Null,
            }
        }
    }
}

/// Coerces a [`GqlValue`] to a ternary boolean for WHERE predicates.
///
/// GQL/SQL three-valued logic: `Bool(b)` → `Some(b)`, everything else
/// (including `Null`, integers, strings) → `None` (NULL in boolean context).
const fn eval_as_tribool(v: &GqlValue) -> Option<bool> {
    match v {
        GqlValue::Bool(b) => Some(*b),
        _ => None,
    }
}

/// Equality comparison with NULL propagation: NULL = anything → false.
fn gql_value_eq(a: &GqlValue, b: &GqlValue) -> bool {
    if *a == GqlValue::Null || *b == GqlValue::Null {
        return false;
    }
    // Numeric coercion: Int vs Float
    match (a, b) {
        (GqlValue::Int(i), GqlValue::Float(f)) | (GqlValue::Float(f), GqlValue::Int(i)) => {
            #[allow(clippy::cast_precision_loss)]
            let fi = *i as f64;
            fi == *f
        }
        _ => a == b,
    }
}

/// Comparison operators (<, >, <=, >=) with NULL propagation.
fn eval_comparison(lv: &GqlValue, op: BinOp, rv: &GqlValue) -> GqlValue {
    if *lv == GqlValue::Null || *rv == GqlValue::Null {
        return GqlValue::Null;
    }
    let Some(ord) = gql_value_cmp(lv, rv) else {
        return GqlValue::Null;
    };
    let result = match op {
        BinOp::Lt => ord.is_lt(),
        BinOp::Gt => ord.is_gt(),
        BinOp::LtEq => ord.is_le(),
        BinOp::GtEq => ord.is_ge(),
        _ => unreachable!(),
    };
    GqlValue::Bool(result)
}

/// Arithmetic operations with NULL propagation and div-by-zero → Null.
fn eval_arithmetic(lv: &GqlValue, op: BinOp, rv: &GqlValue) -> GqlValue {
    if *lv == GqlValue::Null || *rv == GqlValue::Null {
        return GqlValue::Null;
    }
    match (lv, rv) {
        (GqlValue::Int(a), GqlValue::Int(b)) => {
            match op {
                BinOp::Add => GqlValue::Int(a.wrapping_add(*b)),
                BinOp::Sub => GqlValue::Int(a.wrapping_sub(*b)),
                BinOp::Mul => GqlValue::Int(a.wrapping_mul(*b)),
                BinOp::Div => {
                    if *b == 0 { GqlValue::Null } else { GqlValue::Int(a / b) }
                }
                _ => unreachable!(),
            }
        }
        (GqlValue::Float(a), GqlValue::Float(b)) => {
            eval_float_arithmetic(*a, op, *b)
        }
        (GqlValue::Int(i), GqlValue::Float(f)) => {
            #[allow(clippy::cast_precision_loss)]
            let fi = *i as f64;
            eval_float_arithmetic(fi, op, *f)
        }
        (GqlValue::Float(f), GqlValue::Int(i)) => {
            #[allow(clippy::cast_precision_loss)]
            let fi = *i as f64;
            eval_float_arithmetic(*f, op, fi)
        }
        _ => GqlValue::Null,
    }
}

/// Float arithmetic with div-by-zero → Null.
fn eval_float_arithmetic(a: f64, op: BinOp, b: f64) -> GqlValue {
    match op {
        BinOp::Add => GqlValue::Float(a + b),
        BinOp::Sub => GqlValue::Float(a - b),
        BinOp::Mul => GqlValue::Float(a * b),
        BinOp::Div => {
            if b == 0.0 { GqlValue::Null } else { GqlValue::Float(a / b) }
        }
        _ => unreachable!(),
    }
}

/// Evaluates a unary operation.
fn eval_unary_op(op: UnaryOp, v: &GqlValue) -> GqlValue {
    match op {
        UnaryOp::Not => eval_as_tribool(v)
            .map_or(GqlValue::Null, |b| GqlValue::Bool(!b)),
        UnaryOp::Neg => match v {
            // checked_neg returns None for i64::MIN (not representable) → Null
            GqlValue::Int(i) => i.checked_neg().map_or(GqlValue::Null, GqlValue::Int),
            GqlValue::Float(f) => GqlValue::Float(-f),
            _ => GqlValue::Null,
        },
    }
}

/// Compares two [`GqlValue`]s for ordering, returning `None` for incomparable
/// types (e.g. `Int` vs `Str`). NULL sorts last (returns `None` here — caller
/// handles it).
///
/// **Note on total ordering**: when used as a `sort_by` comparator, callers
/// map `None` to `Ordering::Equal`. This is technically not antisymmetric
/// (a column mixing `Int` and `Str` values will have pairs where both
/// `cmp(a,b)` and `cmp(b,a)` return `Equal` without `a == b`). In practice
/// this means the relative order of incomparable values is unspecified but
/// the sort will not panic or loop.
fn gql_value_cmp(a: &GqlValue, b: &GqlValue) -> Option<std::cmp::Ordering> {
    match (a, b) {
        (GqlValue::Int(x), GqlValue::Int(y)) => Some(x.cmp(y)),
        (GqlValue::Float(x), GqlValue::Float(y)) => Some(x.total_cmp(y)),
        (GqlValue::Str(x), GqlValue::Str(y)) => Some(x.cmp(y)),
        (GqlValue::Bool(x), GqlValue::Bool(y)) => Some(x.cmp(y)),
        // Numeric coercion
        (GqlValue::Int(i), GqlValue::Float(f)) => {
            #[allow(clippy::cast_precision_loss)]
            let fi = *i as f64;
            Some(fi.total_cmp(f))
        }
        (GqlValue::Float(f), GqlValue::Int(i)) => {
            #[allow(clippy::cast_precision_loss)]
            let fi = *i as f64;
            Some(f.total_cmp(&fi))
        }
        _ => None,
    }
}

// ── Surface name for expressions ────────────────────────────────────────────

/// Produces a display name for an expression (used as default column name
/// when no `AS alias` is provided).
///
/// Function names are stored lowercase by the parser (e.g. `shortestPath`
/// becomes `"shortestpath"`). Use `AS` to assign a custom column name.
///
/// Public so external consumers (e.g. the Bolt server) can derive the
/// same column ordering the executor uses.
pub fn expr_surface_name(expr: &Expr) -> String {
    match expr {
        Expr::Literal(Literal::Int(v)) => v.to_string(),
        Expr::Literal(Literal::Float(v)) => v.to_string(),
        Expr::Literal(Literal::Str(s)) => format!("'{s}'"),
        Expr::Literal(Literal::Bool(b)) => b.to_string(),
        Expr::Literal(Literal::Null) => "NULL".to_string(),
        Expr::Literal(Literal::List(items)) => {
            let inner: Vec<String> = items
                .iter()
                .map(|it| expr_surface_name(&Expr::Literal(it.clone())))
                .collect();
            format!("[{}]", inner.join(", "))
        }
        Expr::Var(v) => v.clone(),
        Expr::PropAccess { var, prop } => format!("{var}.{prop}"),
        Expr::Aggregate { func, arg } => {
            let func_name = agg_func_name(*func);
            arg.as_ref().map_or_else(
                || format!("{func_name}(*)"),
                |inner| format!("{func_name}({})", expr_surface_name(inner)),
            )
        }
        Expr::BinaryOp { left, op, right } => {
            format!(
                "({} {} {})",
                expr_surface_name(left),
                bin_op_symbol(*op),
                expr_surface_name(right)
            )
        }
        Expr::UnaryOp { op, expr: inner } => {
            let prefix = match op {
                UnaryOp::Not => "NOT ",
                UnaryOp::Neg => "-",
            };
            format!("{prefix}{}", expr_surface_name(inner))
        }
        Expr::IsNull { expr: inner, negated } => {
            let suffix = if *negated { "IS NOT NULL" } else { "IS NULL" };
            format!("{} {suffix}", expr_surface_name(inner))
        }
        Expr::FunctionCall { name, args } => {
            let arg_strs: Vec<String> = args.iter().map(expr_surface_name).collect();
            format!("{}({})", name, arg_strs.join(", "))
        }
        Expr::ShortestPath { .. } => "shortestPath(...)".to_string(),
        Expr::Subscript { list, index } => format!(
            "{}[{}]",
            expr_surface_name(list),
            expr_surface_name(index)
        ),
        Expr::ListLit(items) => {
            let parts: Vec<String> = items.iter().map(expr_surface_name).collect();
            format!("[{}]", parts.join(", "))
        }
        Expr::ListPredicate { kind, var, list, predicate } => {
            let kw = match kind {
                super::ast::ListPredKind::All => "ALL",
                super::ast::ListPredKind::Any => "ANY",
                super::ast::ListPredKind::None => "NONE",
                super::ast::ListPredKind::Single => "SINGLE",
            };
            format!(
                "{kw}({var} IN {} WHERE {})",
                expr_surface_name(list),
                expr_surface_name(predicate)
            )
        }
        // ParamRef is rendered with its surface syntax so column names
        // remain meaningful when substitution has not yet run (e.g. when
        // a caller prepares an unsubstituted statement for diagnostics).
        // After substitution this branch is unreachable in normal flow
        // because the variant is replaced by `Expr::Literal`.
        Expr::ParamRef(ParamRef::Named(name)) => format!("${name}"),
        Expr::ParamRef(ParamRef::Positional(n)) => format!("${n}"),
    }
}

const fn agg_func_name(func: AggFunc) -> &'static str {
    match func {
        AggFunc::Count => "COUNT",
        AggFunc::Sum => "SUM",
        AggFunc::Avg => "AVG",
        AggFunc::Min => "MIN",
        AggFunc::Max => "MAX",
        AggFunc::Collect => "COLLECT",
    }
}

const fn bin_op_symbol(op: BinOp) -> &'static str {
    match op {
        BinOp::Eq => "=",
        BinOp::NotEq => "<>",
        BinOp::Lt => "<",
        BinOp::Gt => ">",
        BinOp::LtEq => "<=",
        BinOp::GtEq => ">=",
        BinOp::And => "AND",
        BinOp::Or => "OR",
        BinOp::Add => "+",
        BinOp::Sub => "-",
        BinOp::Mul => "*",
        BinOp::Div => "/",
        BinOp::StartsWith => "STARTS WITH",
        BinOp::EndsWith => "ENDS WITH",
        BinOp::Contains => "CONTAINS",
        BinOp::In => "IN",
    }
}

// ── Scope validation ────────────────────────────────────────────────────────

/// Collects all variable names bound in MATCH patterns.
fn collect_bound_vars(mc: &MatchClause) -> HashSet<String> {
    let mut vars = HashSet::new();
    // `MATCH p = (…)` binds the path variable `p` itself, consumable by
    // `nodes(p)`/`relationships(p)`/`length(p)` and `RETURN p`.
    if let Some(ref pv) = mc.path_var {
        vars.insert(pv.clone());
    }
    for pp in &mc.patterns {
        if let Some(ref v) = pp.start.var {
            vars.insert(v.clone());
        }
        for (ep, np) in &pp.hops {
            if let Some(ref v) = ep.var {
                vars.insert(v.clone());
            }
            if let Some(ref v) = np.var {
                vars.insert(v.clone());
            }
        }
    }
    vars
}

/// Collects all variable references used in an expression.
fn collect_expr_vars(expr: &Expr, out: &mut HashSet<String>) {
    match expr {
        Expr::Var(v) => {
            out.insert(v.clone());
        }
        Expr::PropAccess { var, .. } => {
            out.insert(var.clone());
        }
        Expr::BinaryOp { left, right, .. } => {
            collect_expr_vars(left, out);
            collect_expr_vars(right, out);
        }
        Expr::UnaryOp { expr: inner, .. } | Expr::IsNull { expr: inner, .. } => {
            collect_expr_vars(inner, out);
        }
        Expr::Aggregate { arg, .. } => {
            if let Some(inner) = arg {
                collect_expr_vars(inner, out);
            }
        }
        // Literal carries no variable references. ParamRef is resolved to
        // a Literal by `param_substitution::apply` before scope validation,
        // so it likewise binds nothing.
        Expr::Literal(_) | Expr::ParamRef(_) => {}
        Expr::FunctionCall { args, .. } => {
            for arg in args {
                collect_expr_vars(arg, out);
            }
        }
        Expr::ShortestPath { pattern } => {
            if let Some(ref v) = pattern.start.var {
                out.insert(v.clone());
            }
            for (_, end_node) in &pattern.hops {
                if let Some(ref v) = end_node.var {
                    out.insert(v.clone());
                }
            }
        }
        Expr::Subscript { list, index } => {
            collect_expr_vars(list, out);
            collect_expr_vars(index, out);
        }
        Expr::ListLit(items) => {
            for item in items {
                collect_expr_vars(item, out);
            }
        }
        Expr::ListPredicate { var, list, predicate, .. } => {
            // The list expression is evaluated in the outer scope, so its
            // variables are genuine external references.
            collect_expr_vars(list, out);
            // The predicate is evaluated with `var` bound to each element, so
            // `var` is local — it must not be reported as an unbound outer
            // reference. Collect the predicate's references, then remove the
            // iteration variable (if the predicate didn't already reference it
            // from an outer scope, removal is a no-op on a name nothing else
            // introduced).
            let mut inner = HashSet::new();
            collect_expr_vars(predicate, &mut inner);
            inner.remove(var);
            out.extend(inner);
        }
    }
}

/// Validates that all variables referenced in WHERE and RETURN are bound in MATCH.
/// Validates that all variables referenced in WHERE/RETURN/ORDER BY are bound
/// by MATCH. Assumes queries with UNWIND have already been dispatched to
/// `execute_with_unwind` — the unwind variable is NOT included in `bound`.
fn validate_scope(query: &GqlQuery, bound: &HashSet<String>) -> crate::Result<()> {
    let mut referenced = HashSet::new();

    if let Some(ref wc) = query.where_clause {
        collect_expr_vars(&wc.predicate, &mut referenced);
    }

    for item in &query.return_clause.items {
        collect_expr_vars(&item.expr, &mut referenced);
    }

    if let Some(ref ob) = query.order_by {
        for item in &ob.items {
            collect_expr_vars(&item.expr, &mut referenced);
        }
    }

    if let Some(ref gb) = query.group_by {
        for key in &gb.keys {
            collect_expr_vars(key, &mut referenced);
        }
    }

    for var in &referenced {
        if !bound.contains(var) {
            return Err(Error::GqlCompileError(format!(
                "variable '{var}' is not bound in MATCH"
            )));
        }
    }

    Ok(())
}

// ── Aggregation detection ───────────────────────────────────────────────────

/// Returns `true` if the expression contains an aggregate function.
fn expr_has_aggregate(expr: &Expr) -> bool {
    match expr {
        Expr::Aggregate { .. } => true,
        Expr::BinaryOp { left, right, .. } => {
            expr_has_aggregate(left) || expr_has_aggregate(right)
        }
        Expr::UnaryOp { expr: inner, .. } | Expr::IsNull { expr: inner, .. } => {
            expr_has_aggregate(inner)
        }
        Expr::FunctionCall { args, .. } => args.iter().any(expr_has_aggregate),
        _ => false,
    }
}

/// Validates that RETURN items don't mix aggregate and non-aggregate expressions.
///
/// With GROUP BY: non-aggregate RETURN expressions must appear in the GROUP BY
/// keys. Without GROUP BY: either all or none of the RETURN items may be
/// aggregates.
fn validate_aggregation(
    items: &[ReturnItem],
    group_by: Option<&super::ast::GroupByClause>,
) -> crate::Result<bool> {
    let any_agg = items.iter().any(|i| expr_has_aggregate(&i.expr));

    if let Some(gb) = group_by {
        // With GROUP BY: non-aggregate RETURN exprs must appear in GROUP BY keys
        for item in items {
            if !expr_has_aggregate(&item.expr)
                && !gb.keys.iter().any(|k| k == &item.expr)
            {
                return Err(Error::GqlCompileError(format!(
                    "non-aggregate expression '{}' in RETURN must appear in GROUP BY",
                    expr_surface_name(&item.expr),
                )));
            }
        }
        return Ok(true); // GROUP BY always triggers grouped execution
    }

    let all_agg = items.iter().all(|i| expr_has_aggregate(&i.expr));

    if any_agg && !all_agg {
        return Err(Error::GqlCompileError(
            "cannot mix aggregate and non-aggregate expressions in RETURN without GROUP BY"
                .into(),
        ));
    }

    Ok(any_agg)
}

// ── MATCH compilation ───────────────────────────────────────────────────────

/// Compiles a MATCH clause into pattern matches using `PatternBuilder`.
///
/// `deadline` (v0.6.0 Fase 2 Task 6) bounds the cross-join cartesian explosion
/// and the variable-length expansion reached through `compile_path_pattern`.
/// `None` disables every check. See [`check_deadline`].
fn compile_match<G: GraphAccess + ?Sized>(
    graph: &G,
    mc: &MatchClause,
    deadline: Option<Instant>,
) -> crate::Result<Vec<PatternMatch>> {
    if mc.patterns.is_empty() {
        return Ok(Vec::new());
    }

    // Execute each path pattern independently, then cross-join.
    let mut result_sets: Vec<Vec<PatternMatch>> = Vec::with_capacity(mc.patterns.len());

    for pp in &mc.patterns {
        let matches = compile_path_pattern(graph, pp, deadline)?;
        result_sets.push(matches);
    }

    // Single pattern → no cross-join needed.
    if result_sets.len() == 1 {
        return Ok(result_sets.into_iter().next()
            .expect("result_sets.len() == 1 verified above"));
    }

    // Multi-pattern MATCH: cross-join all result sets. The inner loop is the
    // cartesian explosion the deadline guards against — `O(prod(|set|))`.
    let mut joined = result_sets.remove(0);
    let mut counter = 0_u64;
    for right_set in result_sets {
        let mut new_joined = Vec::with_capacity(joined.len() * right_set.len());
        for left in &joined {
            for right in &right_set {
                check_deadline(deadline, counter)?;
                counter += 1;
                new_joined.push(left.merge(right));
            }
        }
        joined = new_joined;
    }
    Ok(joined)
}

/// Materialises the [`GqlPath`] bound by `MATCH p = (…)` for a single matched
/// row, choosing the strategy from the pattern shape:
///
/// - **Fixed segments with every edge named** (`p = (a)-[r1]->(b)-[r2]->(c)`):
///   reconstructs directly from the row's bound vars — exact, no re-traversal.
/// - **Variable-length or anonymous-edge segments** (`p = (a)-[:R*1..N]->(c)`):
///   runs [`materialise_varlen_path`], a constraint-aware edge-capturing BFS
///   from the row's start node to its end node. It respects the same
///   label/direction/property filters the MATCH applied, so `relationships(p)`
///   exposes only edges the pattern authorised — the `ReBAC` correctness
///   guarantee (an unfiltered BFS could surface a different, unauthorised
///   chain).
///
/// Returns `None` when the path cannot be reconstructed (unbound endpoint,
/// unreachable). The caller treats `None` as "no path bound for this row".
fn materialise_path_for_match<G: GraphAccess + ?Sized>(
    graph: &G,
    pm: &PatternMatch,
    pp: &PathPattern,
) -> Option<GqlPath> {
    let all_fixed_named = !has_variable_length_hop(pp)
        && pp.hops.iter().all(|(ep, _)| ep.var.is_some());

    if all_fixed_named {
        // Reconstruct from named vars in traversal order. Start node and each
        // hop's end node must be named for this branch to be reached safely;
        // fall back to the BFS branch if any node var is anonymous.
        if let (Some(start_var), node_vars_named) = (
            pp.start.var.as_deref(),
            pp.hops.iter().all(|(_, np)| np.var.is_some()),
        ) {
            if node_vars_named {
                let mut node_vars: Vec<&str> = vec![start_var];
                let mut edge_vars: Vec<&str> = Vec::with_capacity(pp.hops.len());
                for (ep, np) in &pp.hops {
                    edge_vars.push(ep.var.as_deref()?);
                    node_vars.push(np.var.as_deref()?);
                }
                return crate::gql::path_materialization::materialise_fixed_path(
                    pm, &node_vars, &edge_vars,
                );
            }
        }
    }

    // Variable-length (or anonymous-edge) path: BFS from start to end, capturing
    // the traversed edges, constrained to the single hop's edge pattern. The
    // pilot's ReBAC/GraphRAG patterns are single-hop var-length
    // (`(a)-[:LINK*1..N]->(c)`); multi-hop var-length chains fall back to using
    // the first hop's constraints for every step (the common label case).
    let start_var = pp.start.var.as_deref()?;
    let (final_edge, final_node) = pp.hops.last()?;
    let end_var = final_node.var.as_deref()?;
    let start_id = pm.get_node(start_var).ok()?.id();
    let end_id = pm.get_node(end_var).ok()?.id();
    materialise_varlen_path(graph, start_id, end_id, final_edge)
}

/// Builds the per-row [`PathBindings`] for a match: empty when the clause has
/// no `MATCH p = (…)` binding (zero-cost — `HashMap::new` does not allocate),
/// otherwise the single `pv → GqlPath` entry materialised from this row.
fn materialise_path_bindings<G: GraphAccess + ?Sized>(
    graph: &G,
    mc: &MatchClause,
    pm: &PatternMatch,
) -> PathBindings {
    let Some(ref pv) = mc.path_var else {
        return PathBindings::new();
    };
    // A path binding implies exactly one pattern (the parser only accepts
    // `MATCH p = <single-pattern>`); guard defensively all the same.
    mc.patterns
        .first()
        .and_then(|pp| materialise_path_for_match(graph, pm, pp))
        .map_or_else(PathBindings::new, |path| {
            let mut m = PathBindings::new();
            m.insert(pv.clone(), path);
            m
        })
}

/// Constraint-aware edge-capturing BFS from `start_id` to `end_id`, following
/// only edges that satisfy `ep` (label/direction/properties). Returns the
/// shortest such path as a [`GqlPath`] with real edges, or `None` if no
/// constrained path exists.
///
/// Unlike a raw shortest-path BFS, every traversed edge is recorded so
/// `relationships(p)` yields the actual relationships — and only those matching
/// the pattern, which is what makes the `ReBAC` `ALL(rel IN relationships(p) …)`
/// predicate sound.
fn materialise_varlen_path<G: GraphAccess + ?Sized>(
    graph: &G,
    start_id: NodeId,
    end_id: NodeId,
    ep: &EdgePattern,
) -> Option<GqlPath> {
    if start_id == end_id {
        let node = graph.node(start_id).ok()?;
        return Some(GqlPath { nodes: vec![gql_node_from_entity(&node)], rels: vec![] });
    }

    let mut visited: HashSet<NodeId> = HashSet::from([start_id]);
    // Each discovered node → (predecessor, edge traversed to reach it).
    let mut parent: HashMap<NodeId, (NodeId, crate::Edge)> = HashMap::new();
    let mut queue: VecDeque<NodeId> = VecDeque::from([start_id]);

    while let Some(current) = queue.pop_front() {
        let Ok(edges) = get_edges_for_direction(graph, current, ep.direction) else {
            continue;
        };
        for edge in edges {
            if !edge_matches_pattern(&edge, ep) {
                continue;
            }
            let Some(next) = edge_neighbor(&edge, current, ep.direction) else {
                continue;
            };
            if !visited.insert(next) {
                continue;
            }
            parent.insert(next, (current, edge));
            if next == end_id {
                return reconstruct_varlen_path(graph, start_id, end_id, &parent);
            }
            queue.push_back(next);
        }
    }
    None
}

/// Walks `parent` from `end_id` back to `start_id`, then builds the forward
/// [`GqlPath`] (nodes and edges in traversal order).
fn reconstruct_varlen_path<G: GraphAccess + ?Sized>(
    graph: &G,
    start_id: NodeId,
    end_id: NodeId,
    parent: &HashMap<NodeId, (NodeId, crate::Edge)>,
) -> Option<GqlPath> {
    let mut rev_edges: Vec<&crate::Edge> = Vec::new();
    let mut node = end_id;
    while node != start_id {
        let (pred, edge) = parent.get(&node)?;
        rev_edges.push(edge);
        node = *pred;
    }
    rev_edges.reverse();

    let mut nodes = vec![gql_node_from_entity(&graph.node(start_id).ok()?)];
    let mut prev = start_id;
    for edge in &rev_edges {
        // The path advances to whichever endpoint is NOT the node we came from
        // (handles undirected `Both` hops and reverse-stored edges).
        let next = if edge.source() == prev { edge.target() } else { edge.source() };
        nodes.push(gql_node_from_entity(&graph.node(next).ok()?));
        prev = next;
    }
    Some(GqlPath {
        nodes,
        rels: rev_edges.iter().map(|e| gql_relationship_from_entity(e)).collect(),
    })
}

/// Safety cap for unbounded variable-length traversals (`[*]`).
///
/// When no upper bound is specified (e.g. `MATCH (a)-[*]->(b)`), the BFS
/// depth is limited to this value. Paths longer than 100 hops are silently
/// ignored. Use an explicit upper bound (e.g. `[*1..10]`) for queries where
/// the cap might affect results.
const VARIABLE_HOP_SAFETY_CAP: u32 = 100;

/// Returns `true` if the path pattern contains any variable-length hops.
fn has_variable_length_hop(pp: &PathPattern) -> bool {
    pp.hops.iter().any(|(ep, _)| matches!(ep.length, EdgeLength::Variable { .. }))
}

/// Compiles a single path pattern into `PatternBuilder` calls.
///
/// When all hops are fixed-length, the existing `PatternBuilder` path is used.
/// When any hop is variable-length, a manual BFS-based expansion is used instead.
fn compile_path_pattern<G: GraphAccess + ?Sized>(
    graph: &G,
    pp: &PathPattern,
    deadline: Option<Instant>,
) -> crate::Result<Vec<PatternMatch>> {
    if has_variable_length_hop(pp) {
        compile_path_pattern_with_varlen(graph, pp, deadline)
    } else {
        compile_path_pattern_fixed(graph, pp)
    }
}

/// Fixed-hop path pattern compilation via `PatternBuilder` (original path).
fn compile_path_pattern_fixed<G: GraphAccess + ?Sized>(graph: &G, pp: &PathPattern) -> crate::Result<Vec<PatternMatch>> {
    let mut builder = crate::query::pattern::PatternBuilder::new(graph);
    let mut anon_counter = 0_u32;

    builder = compile_node_to_builder(builder, &pp.start, &mut anon_counter);

    for (ep, np) in &pp.hops {
        builder = compile_edge_to_builder(builder, ep);
        builder = compile_node_to_builder(builder, np, &mut anon_counter);
    }

    builder.execute()?.collect::<crate::Result<Vec<_>>>()
}

/// Variable-length path compilation using manual BFS expansion.
///
/// Seeds from the start node pattern, then processes each hop sequentially:
/// - Fixed hops: expand one step (like `PatternBuilder` but manual).
/// - Variable hops: BFS expansion within `[min..=max]` range.
fn compile_path_pattern_with_varlen<G: GraphAccess + ?Sized>(
    graph: &G,
    pp: &PathPattern,
    deadline: Option<Instant>,
) -> crate::Result<Vec<PatternMatch>> {
    let mut anon_counter = 0_u32;

    // Seed: match start node pattern against all graph nodes.
    let (mut current, start_var_name) = seed_start_node(graph, &pp.start, &mut anon_counter);

    // Track the previous node variable for each hop.
    let mut prev_node_var = start_var_name;

    // Process each hop sequentially.
    for (ep, np) in &pp.hops {
        let end_var = np.var.clone().unwrap_or_else(|| {
            let name = format!("_anon_{anon_counter}");
            anon_counter += 1;
            name
        });

        match ep.length {
            EdgeLength::Fixed => {
                current = expand_fixed_hop(graph, &current, &prev_node_var, &end_var, ep, np)?;
            }
            EdgeLength::Variable { min, max } => {
                let min = min.unwrap_or(0);
                let max = max.unwrap_or(VARIABLE_HOP_SAFETY_CAP);
                current = expand_variable_hop(
                    graph, &current, &prev_node_var, &end_var, ep, np, min, max, deadline,
                )?;
            }
        }

        prev_node_var = end_var;
    }

    Ok(current)
}

/// Seeds the initial set of `PatternMatch` rows from the start node pattern.
///
/// Returns `(matches, assigned_variable_name)` so the caller uses the actual
/// variable name rather than guessing.
///
/// # Performance
///
/// When the start node has a label, uses `nodes_by_label()` (index lookup).
/// Otherwise, falls back to `node_ids()` (full scan).
fn seed_start_node<G: GraphAccess + ?Sized>(
    graph: &G,
    np: &NodePattern,
    anon_counter: &mut u32,
) -> (Vec<PatternMatch>, String) {
    let var = np.var.clone().unwrap_or_else(|| {
        let name = format!("_anon_{anon_counter}");
        *anon_counter += 1;
        name
    });

    let mut results = Vec::new();

    // When both a label and at least one inline property constraint exist,
    // narrow the initial candidate set using the property index (O(1)).
    // The full `node_matches_pattern` check still runs to validate any
    // additional property constraints beyond the first one used for lookup.
    let candidates = if let (Some(label), Some((key, lit))) =
        (np.labels.first(), np.props.first())
    {
        literal_to_property(lit).map_or_else(
            || graph.nodes_by_label(label),
            |value| graph.nodes_by_label_and_property(label, key, &value),
        )
    } else {
        np.labels.first().map_or_else(
            || graph.node_ids(),
            |label| graph.nodes_by_label(label),
        )
    };

    for node_id in candidates {
        let Ok(node) = graph.node(node_id) else { continue };

        if !node_matches_pattern(&node, np) {
            continue;
        }

        let mut nodes = HashMap::new();
        nodes.insert(var.clone(), node);
        results.push(PatternMatch::new(nodes, HashMap::new()));
    }

    (results, var)
}

/// Checks if a node matches a `NodePattern`'s label and property constraints.
fn node_matches_pattern(node: &crate::Node, np: &NodePattern) -> bool {
    debug_assert!(np.labels.len() <= 1, "multi-label patterns not yet supported in matching");
    if let Some(label) = np.labels.first() {
        if node.label() != label {
            return false;
        }
    }
    for (key, lit) in &np.props {
        let Some(expected) = literal_to_property(lit) else { continue };
        match node.properties().get(key) {
            Some(actual) if *actual == expected => {}
            _ => return false,
        }
    }
    true
}

/// Checks if an edge matches an `EdgePattern`'s label and property constraints.
fn edge_matches_pattern(edge: &crate::Edge, ep: &EdgePattern) -> bool {
    if let Some(label) = ep.labels.first() {
        if edge.label() != label {
            return false;
        }
    }
    for (key, lit) in &ep.props {
        let Some(expected) = literal_to_property(lit) else { continue };
        match edge.properties().get(key) {
            Some(actual) if *actual == expected => {}
            _ => return false,
        }
    }
    true
}

/// Returns the "next" node ID from an edge given the current node and direction.
fn edge_neighbor(edge: &crate::Edge, from: NodeId, direction: AstDirection) -> Option<NodeId> {
    match direction {
        AstDirection::Outgoing => {
            if edge.source() == from { Some(edge.target()) } else { None }
        }
        AstDirection::Incoming => {
            if edge.target() == from { Some(edge.source()) } else { None }
        }
        AstDirection::Both => {
            if edge.source() == from {
                Some(edge.target())
            } else if edge.target() == from {
                Some(edge.source())
            } else {
                None
            }
        }
    }
}

/// Gets edges from a node according to the given direction.
fn get_edges_for_direction<G: GraphAccess + ?Sized>(
    graph: &G,
    node_id: NodeId,
    direction: AstDirection,
) -> crate::Result<Vec<crate::Edge>> {
    match direction {
        AstDirection::Outgoing => graph.outgoing_edges(node_id),
        AstDirection::Incoming => graph.incoming_edges(node_id),
        AstDirection::Both => {
            let mut edges = graph.outgoing_edges(node_id)?;
            edges.extend(graph.incoming_edges(node_id)?);
            Ok(edges)
        }
    }
}

/// Expands a single fixed-length hop from the current match set.
fn expand_fixed_hop<G: GraphAccess + ?Sized>(
    graph: &G,
    current: &[PatternMatch],
    start_var: &str,
    end_var: &str,
    ep: &EdgePattern,
    np: &NodePattern,
) -> crate::Result<Vec<PatternMatch>> {
    let mut results = Vec::new();

    for pm in current {
        let Ok(start_node) = pm.get_node(start_var) else { continue };
        let start_id = start_node.id();

        let edges = get_edges_for_direction(graph, start_id, ep.direction)?;
        for edge in &edges {
            if !edge_matches_pattern(edge, ep) {
                continue;
            }
            let Some(next_id) = edge_neighbor(edge, start_id, ep.direction) else { continue };
            let Ok(next_node) = graph.node(next_id) else { continue };

            if !node_matches_pattern(&next_node, np) {
                continue;
            }

            let mut nodes = pm.nodes_clone();
            let mut edges_map = pm.edges_clone();
            nodes.insert(end_var.to_string(), next_node);
            if let Some(ref evar) = ep.var {
                edges_map.insert(evar.clone(), edge.clone());
            }
            results.push(PatternMatch::new(nodes, edges_map));
        }
    }

    Ok(results)
}

/// Expands a variable-length hop using BFS from each start node in `current`.
///
/// For each row in `current`, extracts the node bound to `start_var`, runs BFS
/// following edges that match `ep`'s direction/label/properties, and for each
/// reached node at depth in `[min..=max]`, produces a new row binding `end_var`
/// to that node (and optionally `ep.var` to the last edge).
///
/// # Edge variable semantics
///
/// When an edge variable is present (e.g. `[r*1..3]`), `r` is bound to the
/// **last edge** traversed in the path, not a collection of all edges.
#[allow(clippy::too_many_arguments)]
fn expand_variable_hop<G: GraphAccess + ?Sized>(
    graph: &G,
    current: &[PatternMatch],
    start_var: &str,
    end_var: &str,
    ep: &EdgePattern,
    np: &NodePattern,
    min: u32,
    max: u32,
    deadline: Option<Instant>,
) -> crate::Result<Vec<PatternMatch>> {
    let mut results = Vec::new();
    let mut counter = 0_u64;

    for pm in current {
        let Ok(start_node) = pm.get_node(start_var) else { continue };
        let start_id = start_node.id();

        // BFS: (node_id, depth, last_edge)
        let mut queue: VecDeque<(NodeId, u32, Option<crate::Edge>)> = VecDeque::new();
        let mut visited = HashSet::new();
        visited.insert(start_id);

        // Emit start node at depth 0 when min == 0
        if min == 0 {
            let Ok(start_as_end) = graph.node(start_id) else { continue };
            if node_matches_pattern(&start_as_end, np) {
                let mut nodes = pm.nodes_clone();
                nodes.insert(end_var.to_string(), start_as_end);
                results.push(PatternMatch::new(nodes, pm.edges_clone()));
            }
        }

        // Seed BFS with direct neighbors — mark visited at enqueue time
        let edges = get_edges_for_direction(graph, start_id, ep.direction)?;
        for edge in edges {
            if !edge_matches_pattern(&edge, ep) {
                continue;
            }
            let Some(next_id) = edge_neighbor(&edge, start_id, ep.direction) else { continue };
            if !visited.insert(next_id) {
                continue;
            }
            queue.push_back((next_id, 1, Some(edge)));
        }

        while let Some((node_id, depth, last_edge)) = queue.pop_front() {
            // Cooperative deadline: `[*1..N]` expansion can enqueue an
            // exponential number of nodes; abort promptly when it overruns.
            check_deadline(deadline, counter)?;
            counter += 1;

            if depth > max {
                continue;
            }

            // Emit if within [min..=max] range and end node matches pattern
            if depth >= min {
                let Ok(end_node) = graph.node(node_id) else { continue };
                if node_matches_pattern(&end_node, np) {
                    let mut nodes = pm.nodes_clone();
                    let mut edges_map = pm.edges_clone();
                    nodes.insert(end_var.to_string(), end_node);
                    if let Some(ref evar) = ep.var {
                        if let Some(ref le) = last_edge {
                            edges_map.insert(evar.clone(), le.clone());
                        }
                    }
                    results.push(PatternMatch::new(nodes, edges_map));
                }
            }

            // Continue BFS if we haven't reached max depth
            if depth < max {
                let Ok(next_edges) = get_edges_for_direction(graph, node_id, ep.direction)
                    else { continue };
                for edge in next_edges {
                    if !edge_matches_pattern(&edge, ep) {
                        continue;
                    }
                    let Some(next_id) = edge_neighbor(&edge, node_id, ep.direction) else { continue };
                    if !visited.insert(next_id) {
                        continue;
                    }
                    queue.push_back((next_id, depth + 1, Some(edge)));
                }
            }
        }
    }

    Ok(results)
}

/// Adds a node constraint to the pattern builder.
///
/// Anonymous nodes (no variable in the AST) get synthetic names `_anon_0`,
/// `_anon_1`, etc. to avoid collisions in `PatternBuilder`'s uniqueness check.
fn compile_node_to_builder<'g, G: GraphAccess + ?Sized>(
    mut builder: crate::query::pattern::PatternBuilder<'g, G>,
    np: &NodePattern,
    anon_counter: &mut u32,
) -> crate::query::pattern::PatternBuilder<'g, G> {
    let var: String = np.var.clone().unwrap_or_else(|| {
        let name = format!("_anon_{anon_counter}");
        *anon_counter += 1;
        name
    });
    builder = builder.node(var);

    if let Some(label) = np.labels.first() {
        builder = builder.label(label.clone());
    }

    for (key, lit) in &np.props {
        if let Some(prop) = literal_to_property(lit) {
            builder = builder.where_prop(key.clone(), prop);
        }
    }

    builder
}

/// Adds an edge constraint to the pattern builder.
fn compile_edge_to_builder<'g, G: GraphAccess + ?Sized>(
    mut builder: crate::query::pattern::PatternBuilder<'g, G>,
    ep: &EdgePattern,
) -> crate::query::pattern::PatternBuilder<'g, G> {
    let direction = compile_direction(ep.direction);

    builder = if let Some(ref var) = ep.var {
        builder.edge_var(var.clone(), direction)
    } else {
        builder.edge(direction)
    };

    if let Some(label) = ep.labels.first() {
        builder = builder.label(label.clone());
    }

    for (key, lit) in &ep.props {
        if let Some(prop) = literal_to_property(lit) {
            builder = builder.where_edge_prop(key.clone(), prop);
        }
    }

    builder
}

// ── RETURN projection ───────────────────────────────────────────────────────

/// Projects a single `PatternMatch` row into a `GqlRow` using RETURN items.
///
/// `abort` carries the cooperative deadline for any `shortestPath` BFS reached
/// through the projected expressions (the one runaway loop behind the
/// infallible `eval_expr` boundary). See [`DeadlineAbort`].
fn project_row<G: GraphAccess + ?Sized>(
    pm: &PatternMatch,
    paths: &PathBindings,
    items: &[ReturnItem],
    graph: &G,
    abort: &DeadlineAbort,
) -> GqlRow {
    let mut row = HashMap::with_capacity(items.len());
    for item in items {
        let value = eval_expr(&item.expr, pm, paths, graph, abort);
        let col_name = item
            .alias
            .as_deref()
            .map_or_else(|| expr_surface_name(&item.expr), String::from);
        row.insert(col_name, value);
    }
    row
}

// ── Aggregation ─────────────────────────────────────────────────────────────

/// Applies aggregation across all matches, producing a single result row.
fn apply_aggregation<G: GraphAccess + ?Sized>(
    matches: &[PatternMatch],
    items: &[ReturnItem],
    graph: &G,
) -> crate::Result<GqlRow> {
    let mut row = HashMap::with_capacity(items.len());

    for item in items {
        let col_name = item
            .alias
            .as_deref()
            .map_or_else(|| expr_surface_name(&item.expr), String::from);
        let value = eval_aggregate(&item.expr, matches, graph)?;
        row.insert(col_name, value);
    }

    Ok(row)
}

/// Core of aggregate evaluation — takes already-evaluated per-row values plus
/// the row count (for `COUNT(*)`).
///
/// Callers are responsible for producing the `Vec<GqlValue>` from their own
/// binding representation ([`PatternMatch`], `(PatternMatch, GqlValue)`, etc.).
fn eval_aggregate_core(
    func: AggFunc,
    values: &[GqlValue],
    row_count: usize,
    arg_is_none: bool,
) -> GqlValue {
    match func {
        AggFunc::Count => {
            if arg_is_none {
                // COUNT(*) counts all rows
                #[allow(clippy::cast_possible_wrap)]
                GqlValue::Int(row_count as i64)
            } else {
                // COUNT(expr) counts non-NULL values
                #[allow(clippy::cast_possible_wrap)]
                GqlValue::Int(
                    values.iter().filter(|v| **v != GqlValue::Null).count() as i64,
                )
            }
        }
        AggFunc::Sum => aggregate_sum(values),
        AggFunc::Avg => aggregate_avg(values),
        AggFunc::Min => aggregate_min_max(values, false),
        AggFunc::Max => aggregate_min_max(values, true),
        AggFunc::Collect => {
            let non_null: Vec<GqlValue> = values
                .iter()
                .filter(|v| **v != GqlValue::Null)
                .cloned()
                .collect();
            GqlValue::List(non_null)
        }
    }
}

/// Evaluates an aggregate expression across all matches.
fn eval_aggregate<G: GraphAccess + ?Sized>(expr: &Expr, matches: &[PatternMatch], graph: &G) -> crate::Result<GqlValue> {
    match expr {
        Expr::Aggregate { func, arg } => {
            let values: Vec<GqlValue> = arg.as_ref().map_or_else(
                Vec::new, // COUNT(*) uses match count, not values
                |inner| matches.iter().map(|pm| eval_expr(inner, pm, &PathBindings::new(), graph, &DeadlineAbort::none())).collect(),
            );
            Ok(eval_aggregate_core(*func, &values, matches.len(), arg.is_none()))
        }
        _ => {
            Err(Error::GqlCompileError(
                "expected aggregate expression".into(),
            ))
        }
    }
}

/// SUM: integers stay int, floats stay float, mixed → float.
fn aggregate_sum(values: &[GqlValue]) -> GqlValue {
    let has_non_null = values.iter().any(|v| !matches!(v, GqlValue::Null));
    if !has_non_null {
        return GqlValue::Null;
    }

    let has_float = values.iter().any(|v| matches!(v, GqlValue::Float(_)));

    if has_float {
        let sum: f64 = values
            .iter()
            .filter_map(|v| match v {
                GqlValue::Int(i) => {
                    #[allow(clippy::cast_precision_loss)]
                    Some(*i as f64)
                }
                GqlValue::Float(f) => Some(*f),
                _ => None,
            })
            .sum();
        GqlValue::Float(sum)
    } else {
        let sum: i64 = values
            .iter()
            .filter_map(|v| match v {
                GqlValue::Int(i) => Some(*i),
                _ => None,
            })
            .sum();
        GqlValue::Int(sum)
    }
}

/// AVG always returns Float. Single-pass fold avoids intermediate allocation.
fn aggregate_avg(values: &[GqlValue]) -> GqlValue {
    let (sum, count) = values.iter().fold((0.0_f64, 0_u64), |(s, n), v| match v {
        GqlValue::Int(i) => {
            #[allow(clippy::cast_precision_loss)]
            (s + *i as f64, n + 1)
        }
        GqlValue::Float(f) => (s + f, n + 1),
        _ => (s, n),
    });

    if count == 0 {
        return GqlValue::Null;
    }

    #[allow(clippy::cast_precision_loss)]
    let avg = sum / count as f64;
    GqlValue::Float(avg)
}

/// MIN/MAX using `gql_value_cmp`.
fn aggregate_min_max(values: &[GqlValue], is_max: bool) -> GqlValue {
    let mut iter = values.iter().filter(|v| **v != GqlValue::Null);
    let Some(first) = iter.next() else {
        return GqlValue::Null;
    };
    let result = iter.fold(first, |acc, v| {
        gql_value_cmp(v, acc).map_or(acc, |ord| {
            if (is_max && ord.is_gt()) || (!is_max && ord.is_lt()) { v } else { acc }
        })
    });
    result.clone()
}

// ── ORDER BY ────────────────────────────────────────────────────────────────

/// Compares two [`GqlValue`]s for sort ordering, matching the semantics used
/// by [`apply_order_by`]: NULL sorts last, incomparable types are treated as
/// equal.
fn compare_sort_keys(a: &GqlValue, b: &GqlValue) -> std::cmp::Ordering {
    match (a, b) {
        (GqlValue::Null, GqlValue::Null) => std::cmp::Ordering::Equal,
        (GqlValue::Null, _) => std::cmp::Ordering::Greater, // NULL sorts last
        (_, GqlValue::Null) => std::cmp::Ordering::Less,
        _ => gql_value_cmp(a, b).unwrap_or(std::cmp::Ordering::Equal),
    }
}

/// Applies a permutation to `data` in-place using a cycle-follower algorithm.
///
/// `perm[i]` is the index in the *original* slice that should end up at
/// position `i` after the operation.  Every element is moved at most once, so
/// this is O(N) time and O(N) extra space (the `done` bitvector).
fn apply_permutation<T>(data: &mut [T], perm: &[usize]) {
    let mut done = vec![false; data.len()];
    for i in 0..data.len() {
        if done[i] || perm[i] == i {
            done[i] = true;
            continue;
        }
        let mut j = i;
        loop {
            let next = perm[j];
            done[j] = true;
            if next == i {
                break;
            }
            data.swap(j, next);
            j = next;
        }
    }
}

/// Sorts result rows by ORDER BY clauses using a Schwartzian transform:
/// sort keys are pre-computed once per row (O(N) evaluations) and then
/// indices are sorted by comparing the cached keys (O(N log N) comparisons).
///
/// This replaces the naïve approach that called `eval_expr` inside the
/// comparator, which triggered O(N log N) expression evaluations.
///
/// Incomparable values (e.g. `Int` vs `Str` in the same column) are treated
/// as equal — see [`gql_value_cmp`] for details on the ordering semantics.
fn apply_order_by<G: GraphAccess + ?Sized>(
    pairs: &mut [(PatternMatch, GqlRow)],
    order: &OrderByClause,
    graph: &G,
) {
    if pairs.is_empty() || order.items.is_empty() {
        return;
    }

    // Step 1 — pre-compute sort keys: one Vec<GqlValue> per row.
    let keys: Vec<Vec<GqlValue>> = pairs
        .iter()
        .map(|(pm, _)| {
            order
                .items
                .iter()
                .map(|item| eval_expr(&item.expr, pm, &PathBindings::new(), graph, &DeadlineAbort::none()))
                .collect()
        })
        .collect();

    // Step 2 — sort an index array by the pre-computed keys.
    let mut indices: Vec<usize> = (0..pairs.len()).collect();
    indices.sort_by(|&a, &b| {
        for (idx, item) in order.items.iter().enumerate() {
            let cmp = compare_sort_keys(&keys[a][idx], &keys[b][idx]);
            let directed = if item.ascending { cmp } else { cmp.reverse() };
            if directed != std::cmp::Ordering::Equal {
                return directed;
            }
        }
        std::cmp::Ordering::Equal
    });

    // Step 3 — reorder `pairs` according to the sorted permutation.
    apply_permutation(pairs, &indices);
}

// ── Aggregate pushdown optimization ────────────────────────────────────────

/// Returns `true` when the query is eligible for aggregate pushdown:
/// aggregate-only RETURN, single zero-hop pattern, no WHERE/ORDER BY/LIMIT/DISTINCT.
fn is_pushdown_eligible(query: &GqlQuery) -> bool {
    if query.unwind_clause.is_some() {
        return false;
    }
    if query.where_clause.is_some()
        || query.order_by.is_some()
        || query.limit.is_some()
        || query.return_clause.distinct
    {
        return false;
    }
    // Single pattern, zero-hop or single fixed-hop with no edge variable
    let patterns = &query.match_clause.patterns;
    if patterns.len() != 1 {
        return false;
    }
    let pp = &patterns[0];
    pp.hops.is_empty()
        || (pp.hops.len() == 1
            && matches!(pp.hops[0].0.length, EdgeLength::Fixed)
            && pp.hops[0].0.var.is_none())
}

/// Extracts the aggregate argument's target: the variable name and optional
/// property key from the expression inside an aggregate function.
///
/// Returns `(var_name, None)` for `COUNT(n)` or `(var_name, Some(prop))` for
/// `SUM(n.age)`.  Returns `None` for unsupported expression shapes.
fn aggregate_target(expr: &Expr) -> Option<(&str, Option<&str>)> {
    match expr {
        Expr::Aggregate { arg: None, .. } => Some(("*", None)),
        Expr::Aggregate { arg: Some(inner), .. } => match inner.as_ref() {
            Expr::Var(v) => Some((v.as_str(), None)),
            Expr::PropAccess { var, prop } => Some((var.as_str(), Some(prop.as_str()))),
            _ => None,
        },
        _ => None,
    }
}

/// Attempts to compute aggregate results without materializing `PatternMatch`
/// objects.  Returns `None` when the query is not eligible, causing the caller
/// to fall back to the standard `compile_match` → `apply_aggregation` path.
///
/// When eligible, obtains candidate IDs via `narrow_candidates` (index-only,
/// no node loading for pure COUNT) and streams property values through
/// per-aggregate accumulators.
// allow: cohesive state machine, splitting would fragment logic
#[allow(clippy::too_many_lines)]
fn try_aggregate_pushdown<G: GraphAccess + ?Sized>(
    graph: &G,
    query: &GqlQuery,
) -> Option<crate::Result<GqlResult>> {
    if !is_pushdown_eligible(query) {
        return None;
    }

    let pp = &query.match_clause.patterns[0];
    let np = &pp.start;

    // Build the node constraint from the AST NodePattern.
    let var_name = np.var.as_deref().unwrap_or("_anon_0");
    let constraint = crate::query::pattern::NodeConstraint {
        var: var_name.to_owned(),
        label: np.labels.first().cloned(),
        props: np
            .props
            .iter()
            .filter_map(|(k, lit)| literal_to_property(lit).map(|p| (k.clone(), p)))
            .collect(),
    };

    let ids = crate::query::pattern::narrow_candidates(graph, &constraint);

    // Check that all RETURN items are aggregates we can handle.
    // Bail to fallback if any aggregate target is unsupported.
    for item in &query.return_clause.items {
        aggregate_target(&item.expr)?;
    }

    // Determine whether we need to load nodes at all.
    // If every aggregate is COUNT(*) or COUNT(var), we only need the count.
    let needs_node_load = query.return_clause.items.iter().any(|item| {
        aggregate_target(&item.expr)
            .is_some_and(|(_, prop)| prop.is_some())
    });

    // ── One-hop aggregate pushdown (COUNT-only) ────────────────────────────
    if !pp.hops.is_empty() {
        // Only handle COUNT-only for 1-hop; fall through for property-based aggregates.
        if needs_node_load {
            return None;
        }

        let (ep, end_np) = &pp.hops[0];
        let direction = match ep.direction {
            AstDirection::Outgoing => Direction::Outgoing,
            AstDirection::Incoming => Direction::Incoming,
            AstDirection::Both => Direction::Both,
        };
        let edge_label = ep.labels.first().map(String::as_str);
        let end_label = end_np.labels.first().map(String::as_str);

        let mut total_count: i64 = 0;
        for &start_id in &ids {
            let edges = match (direction, edge_label) {
                (Direction::Outgoing, Some(l)) => graph.outgoing_edges_by_label(start_id, l),
                (Direction::Outgoing, None) => graph.outgoing_edges(start_id),
                (Direction::Incoming, Some(l)) => graph.incoming_edges_by_label(start_id, l),
                (Direction::Incoming, None) => graph.incoming_edges(start_id),
                (Direction::Both, Some(l)) => {
                    match graph.outgoing_edges_by_label(start_id, l) {
                        Ok(mut e) => match graph.incoming_edges_by_label(start_id, l) {
                            Ok(inc) => { e.extend(inc); Ok(e) }
                            Err(err) => Err(err),
                        },
                        Err(err) => Err(err),
                    }
                }
                (Direction::Both, None) => {
                    match graph.outgoing_edges(start_id) {
                        Ok(mut e) => match graph.incoming_edges(start_id) {
                            Ok(inc) => { e.extend(inc); Ok(e) }
                            Err(err) => Err(err),
                        },
                        Err(err) => Err(err),
                    }
                }
            };
            let edges = match edges {
                Ok(e) => e,
                Err(err) => return Some(Err(err)),
            };
            for edge in &edges {
                let neighbor_id = if edge.source() == start_id {
                    edge.target()
                } else {
                    edge.source()
                };
                if let Some(required_label) = end_label {
                    let actual = match graph.node_label(neighbor_id) {
                        Ok(l) => l,
                        Err(err) => return Some(Err(err)),
                    };
                    if actual != required_label {
                        continue;
                    }
                }
                total_count += 1;
            }
        }

        let mut row = HashMap::with_capacity(query.return_clause.items.len());
        let count = GqlValue::Int(total_count);
        for item in &query.return_clause.items {
            let col = item
                .alias
                .as_deref()
                .map_or_else(|| expr_surface_name(&item.expr), String::from);
            row.insert(col, count.clone());
        }
        return Some(Ok(vec![row]));
    }

    // ── Zero-hop fast path ───────────────────────────────────────────────────
    // All aggregates are COUNT(*) or COUNT(var) — no node loading.
    if !needs_node_load {
        let mut row = HashMap::with_capacity(query.return_clause.items.len());
        #[allow(clippy::cast_possible_wrap)]
        let visible = ids.iter().filter(|id| graph.node_visible(**id)).count() as i64;
        let count = GqlValue::Int(visible);
        for item in &query.return_clause.items {
            let col = item
                .alias
                .as_deref()
                .map_or_else(|| expr_surface_name(&item.expr), String::from);
            row.insert(col, count.clone());
        }
        return Some(Ok(vec![row]));
    }

    // Streaming path: load each node once, feed all accumulators.
    Some(pushdown_with_node_load(graph, &ids, &query.return_clause.items))
}

/// Accumulator state for a single aggregate column during pushdown.
enum AggAccum {
    Count(i64),
    Sum { int_sum: i64, float_sum: f64, has_float: bool, has_any: bool },
    Avg { sum: f64, count: u64 },
    Min(Option<GqlValue>),
    Max(Option<GqlValue>),
    Collect(Vec<GqlValue>),
}

impl AggAccum {
    const fn from_func(func: AggFunc) -> Self {
        match func {
            AggFunc::Count => Self::Count(0),
            AggFunc::Sum => Self::Sum { int_sum: 0, float_sum: 0.0, has_float: false, has_any: false },
            AggFunc::Avg => Self::Avg { sum: 0.0, count: 0 },
            AggFunc::Min => Self::Min(None),
            AggFunc::Max => Self::Max(None),
            AggFunc::Collect => Self::Collect(Vec::new()),
        }
    }

    fn feed(&mut self, value: &GqlValue) {
        if *value == GqlValue::Null {
            return;
        }
        match self {
            Self::Count(n) => *n += 1,
            Self::Sum { int_sum, float_sum, has_float, has_any } => {
                *has_any = true;
                match value {
                    GqlValue::Int(i) => *int_sum = int_sum.saturating_add(*i),
                    GqlValue::Float(f) => { *float_sum += f; *has_float = true; }
                    _ => {}
                }
            }
            Self::Avg { sum, count } => {
                match value {
                    #[allow(clippy::cast_precision_loss)]
                    GqlValue::Int(i) => { *sum += *i as f64; *count += 1; }
                    GqlValue::Float(f) => { *sum += f; *count += 1; }
                    _ => {}
                }
            }
            Self::Min(current) => {
                let replace = current.as_ref().is_none_or(|c| {
                    gql_value_cmp(value, c).is_some_and(std::cmp::Ordering::is_lt)
                });
                if replace { *current = Some(value.clone()); }
            }
            Self::Max(current) => {
                let replace = current.as_ref().is_none_or(|c| {
                    gql_value_cmp(value, c).is_some_and(std::cmp::Ordering::is_gt)
                });
                if replace { *current = Some(value.clone()); }
            }
            Self::Collect(items) => items.push(value.clone()),
        }
    }

    fn finalize(self) -> GqlValue {
        match self {
            Self::Count(n) => GqlValue::Int(n),
            Self::Sum { int_sum, float_sum, has_float, has_any } => {
                if !has_any { return GqlValue::Null; }
                if has_float {
                    #[allow(clippy::cast_precision_loss)]
                    GqlValue::Float(float_sum + int_sum as f64)
                } else {
                    GqlValue::Int(int_sum)
                }
            }
            #[allow(clippy::cast_precision_loss)]
            Self::Avg { sum, count } => {
                if count == 0 { GqlValue::Null } else { GqlValue::Float(sum / count as f64) }
            }
            Self::Min(v) | Self::Max(v) => v.unwrap_or(GqlValue::Null),
            Self::Collect(items) => GqlValue::List(items),
        }
    }
}

/// Streaming aggregation with node loading — used when at least one aggregate
/// needs property access (e.g. `SUM(n.age)`).
fn pushdown_with_node_load<G: GraphAccess + ?Sized>(
    graph: &G,
    ids: &[NodeId],
    items: &[ReturnItem],
) -> crate::Result<GqlResult> {
    // Build accumulators and extract property keys.
    let mut accums: Vec<(String, AggAccum, Option<String>)> = Vec::with_capacity(items.len());

    for item in items {
        let col = item
            .alias
            .as_deref()
            .map_or_else(|| expr_surface_name(&item.expr), String::from);

        let Expr::Aggregate { func, arg } = &item.expr else {
            return Err(Error::GqlCompileError("expected aggregate in pushdown".into()));
        };

        let prop_key = match arg.as_deref() {
            // COUNT(*) or COUNT(n) — just needs existence
            None | Some(Expr::Var(_)) => None,
            Some(Expr::PropAccess { prop, .. }) => Some(prop.clone()),
            _ => return Err(Error::GqlCompileError("unsupported aggregate argument in pushdown".into())),
        };

        accums.push((col, AggAccum::from_func(*func), prop_key));
    }

    // Single pass over candidates.
    for &id in ids {
        let node = graph.node(id)?;
        for (_, accum, prop_key) in &mut accums {
            let value = prop_key.as_ref().map_or_else(
                || {
                    // COUNT(*) or COUNT(var) — node exists, so non-null
                    #[allow(clippy::cast_possible_wrap)]
                    GqlValue::Int(node.id().as_u64() as i64)
                },
                |key| {
                    node.properties()
                        .get(key.as_str())
                        .map_or(GqlValue::Null, gql_value_from_property)
                },
            );
            accum.feed(&value);
        }
    }

    // Finalize all accumulators into a single result row.
    let mut row = HashMap::with_capacity(accums.len());
    for (col, accum, _) in accums {
        row.insert(col, accum.finalize());
    }
    Ok(vec![row])
}

// ── Main entry point ────────────────────────────────────────────────────────

/// Executes a parsed GQL query against a graph, returning the result set.
///
/// # Errors
///
/// Returns [`Error::GqlCompileError`] if:
/// - A variable referenced in WHERE/RETURN/ORDER BY is not bound in MATCH.
/// - Aggregate and non-aggregate expressions are mixed in RETURN.
///
/// May also return storage errors from the underlying `PatternBuilder`.
/// Executes a query that has an UNWIND clause.
///
/// Evaluates the list expression, then for each element creates a synthetic
/// binding and cross-joins with the MATCH results. WHERE/RETURN/ORDER BY/LIMIT
/// are applied as normal.
// allow: cohesive state machine, splitting would fragment logic
#[allow(clippy::too_many_lines)]
fn execute_with_unwind<G: GraphAccess + ?Sized>(
    graph: &G,
    query: &GqlQuery,
    max_rows: u64,
    deadline: Option<Instant>,
    abort: &DeadlineAbort,
) -> crate::Result<GqlResult> {
    use super::ast::UnwindClause;
    // Cap A is not applied here: UNWIND expands rows post-match, so the
    // meaningful guard is the output-row Cap B at the GraphAccessor
    // boundary. The param exists for signature symmetry with `execute`.
    let _ = max_rows;
    // `abort` is reserved for the shortestPath BFS that may run during UNWIND
    // projection; the cross-join below is bounded by the deadline directly.
    let _ = abort;

    let unwind: &UnwindClause = query.unwind_clause.as_ref().ok_or_else(|| {
        Error::GqlCompileError(
            "execute_with_unwind invoked without UNWIND clause".into(),
        )
    })?;

    // Evaluate the list expression in an empty context (no MATCH bindings yet).
    let empty = PatternMatch::empty();
    let list_value = eval_expr(&unwind.expr, &empty, &PathBindings::new(), graph, &DeadlineAbort::none());

    let elements: Vec<GqlValue> = match list_value {
        GqlValue::List(items) => items,
        GqlValue::Null => vec![],
        other => vec![other], // singleton — wrap non-list
    };

    if elements.is_empty() {
        return Ok(vec![]);
    }

    // Compile MATCH once — produces the base set of PatternMatch rows.
    let base_matches = compile_match(graph, &query.match_clause, deadline)?;

    // Cross-join: for each unwind element × each MATCH row, produce a combined row.
    // The unwind variable is injected as a synthetic node-like binding so that
    // eval_expr can resolve `Expr::Var(unwind_var)` in WHERE/RETURN.
    let is_aggregate = validate_aggregation(
        &query.return_clause.items,
        query.group_by.as_ref(),
    )?;

    let mut combined: Vec<(PatternMatch, GqlValue)> = Vec::with_capacity(
        elements.len() * base_matches.len().max(1),
    );

    if base_matches.is_empty() {
        // UNWIND without matching nodes — no cross-join partners.
        // TODO(cypher-compat): In Cypher, `UNWIND [1,2] AS x RETURN x` (no
        // MATCH) produces rows. Our grammar requires MATCH, so zero matches
        // yields zero rows. To support standalone UNWIND, make MATCH optional
        // in the parser and handle the no-MATCH case here.
        return Ok(vec![]);
    }

    let mut counter = 0_u64;
    for elem in &elements {
        for pm in &base_matches {
            check_deadline(deadline, counter)?;
            counter += 1;
            combined.push((pm.clone(), elem.clone()));
        }
    }

    // WHERE filtering — need to evaluate predicates with access to the unwind var.
    let filtered: Vec<(PatternMatch, GqlValue)> = if let Some(ref wc) = query.where_clause {
        combined
            .into_iter()
            .filter(|(pm, elem)| {
                let val = eval_expr_with_unwind_var(
                    &wc.predicate, pm, graph, &unwind.var, elem,
                );
                eval_as_tribool(&val) == Some(true)
            })
            .collect()
    } else {
        combined
    };

    // RETURN projection
    if is_aggregate {
        let mut row = HashMap::with_capacity(query.return_clause.items.len());
        // Collect values for each aggregate across all rows
        for item in &query.return_clause.items {
            let col = item
                .alias
                .as_deref()
                .map_or_else(|| expr_surface_name(&item.expr), String::from);
            let value = eval_aggregate_with_unwind(
                &item.expr, &filtered, graph, &unwind.var,
            )?;
            row.insert(col, value);
        }
        return Ok(vec![row]);
    }

    // Non-aggregate: project each row
    let mut rows: GqlResult = filtered
        .iter()
        .map(|(pm, elem)| {
            let mut row = HashMap::new();
            for item in &query.return_clause.items {
                let col = item
                    .alias
                    .as_deref()
                    .map_or_else(|| expr_surface_name(&item.expr), String::from);
                let val = eval_expr_with_unwind_var(
                    &item.expr, pm, graph, &unwind.var, elem,
                );
                row.insert(col, val);
            }
            row
        })
        .collect();

    // ORDER BY (simplified — only supports expressions that don't need PatternMatch)
    // Full ORDER BY support with unwind vars would require keeping (pm, elem) pairs.
    // For now, sort by the projected row values.
    //
    // Matching ORDER BY expressions back to the projected column keys mirrors
    // the logic in `execute_grouped`: aliases take precedence over surface
    // names so `ORDER BY p.name` still finds `AS name`.
    if let Some(ref order) = query.order_by {
        rows.sort_by(|a, b| {
            for item in &order.items {
                let col = query
                    .return_clause
                    .items
                    .iter()
                    .find(|ri| ri.expr == item.expr)
                    .map_or_else(
                        || expr_surface_name(&item.expr),
                        |ri| {
                            ri.alias
                                .as_deref()
                                .map_or_else(|| expr_surface_name(&ri.expr), String::from)
                        },
                    );
                let va = a.get(&col).unwrap_or(&GqlValue::Null);
                let vb = b.get(&col).unwrap_or(&GqlValue::Null);
                if let Some(ord) = gql_value_cmp(va, vb) {
                    let ord = if item.ascending { ord } else { ord.reverse() };
                    if ord != std::cmp::Ordering::Equal {
                        return ord;
                    }
                }
            }
            std::cmp::Ordering::Equal
        });
    }

    // DISTINCT
    if query.return_clause.distinct {
        let mut seen = HashSet::new();
        rows.retain(|row| {
            let mut sorted_cols: Vec<(&String, &GqlValue)> = row.iter().collect();
            sorted_cols.sort_unstable_by_key(|(k, _)| k.as_str());
            let key = format!("{sorted_cols:?}");
            seen.insert(key)
        });
    }

    // LIMIT
    if let Some(ref limit) = query.limit {
        // `limit.count` is u64 and usize is 64-bit on every target this crate
        // is built for, so the conversion is lossless in practice. On a 32-bit
        // target it would wrap to `count % 2^32` and cut the result SHORT —
        // fewer rows than asked, not more. Left as a cast rather than a checked
        // conversion because no 32-bit target is supported; if one ever is,
        // this needs a real bound, not this note.
        #[allow(clippy::cast_possible_truncation)]
        rows.truncate(limit.count as usize);
    }

    Ok(rows)
}

/// Evaluates an expression with an additional unwind variable in scope.
/// If the expression references the unwind variable, returns the element value.
fn eval_expr_with_unwind_var<G: GraphAccess + ?Sized>(
    expr: &Expr,
    pm: &PatternMatch,
    graph: &G,
    unwind_var: &str,
    unwind_elem: &GqlValue,
) -> GqlValue {
    match expr {
        Expr::Var(name) if name == unwind_var => unwind_elem.clone(),
        Expr::PropAccess { var, prop: _ } if var == unwind_var => {
            // Property access on unwind variable — only meaningful for map
            // values (Phase 2). For primitive unwind values there is no
            // property to resolve, so we return Null per Cypher semantics
            // (property access on a non-map evaluates to NULL rather than
            // raising an error). Map values are not yet supported.
            // TODO(unwind-maps): implement map element access when
            // Literal::Map is added.
            GqlValue::Null
        }
        Expr::BinaryOp { left, op, right } => {
            let lv = eval_expr_with_unwind_var(left, pm, graph, unwind_var, unwind_elem);
            let rv = eval_expr_with_unwind_var(right, pm, graph, unwind_var, unwind_elem);
            eval_binary_op(&lv, *op, &rv)
        }
        Expr::UnaryOp { op, expr: inner } => {
            let v = eval_expr_with_unwind_var(inner, pm, graph, unwind_var, unwind_elem);
            eval_unary_op(*op, &v)
        }
        Expr::IsNull { expr: inner, negated } => {
            let v = eval_expr_with_unwind_var(inner, pm, graph, unwind_var, unwind_elem);
            let is_null = v == GqlValue::Null;
            GqlValue::Bool(if *negated { !is_null } else { is_null })
        }
        // For everything else, fall back to the standard evaluator. UNWIND
        // expression context is not a runaway-BFS site; no deadline threaded.
        _ => eval_expr(expr, pm, &PathBindings::new(), graph, &DeadlineAbort::none()),
    }
}

/// Evaluates an aggregate expression with unwind variable support.
fn eval_aggregate_with_unwind<G: GraphAccess + ?Sized>(
    expr: &Expr,
    rows: &[(PatternMatch, GqlValue)],
    graph: &G,
    unwind_var: &str,
) -> crate::Result<GqlValue> {
    match expr {
        Expr::Aggregate { func, arg } => {
            let values: Vec<GqlValue> = arg.as_ref().map_or_else(
                Vec::new,
                |inner| {
                    rows.iter()
                        .map(|(pm, elem)| {
                            eval_expr_with_unwind_var(inner, pm, graph, unwind_var, elem)
                        })
                        .collect()
                },
            );
            Ok(eval_aggregate_core(*func, &values, rows.len(), arg.is_none()))
        }
        _ => Err(Error::GqlCompileError("expected aggregate expression".into())),
    }
}

/// Evaluates an aggregate expression across a group of pattern-match references.
///
/// This is the by-reference counterpart of [`eval_aggregate`], used by
/// [`execute_grouped`] to avoid cloning `PatternMatch` values per group.
fn eval_aggregate_refs<G: GraphAccess + ?Sized>(
    expr: &Expr,
    matches: &[&PatternMatch],
    graph: &G,
) -> crate::Result<GqlValue> {
    match expr {
        Expr::Aggregate { func, arg } => {
            let values: Vec<GqlValue> = arg.as_ref().map_or_else(
                Vec::new,
                |inner| matches.iter().map(|pm| eval_expr(inner, pm, &PathBindings::new(), graph, &DeadlineAbort::none())).collect(),
            );
            Ok(eval_aggregate_core(*func, &values, matches.len(), arg.is_none()))
        }
        _ => Err(Error::GqlCompileError("expected aggregate expression".into())),
    }
}

/// Executes a query with GROUP BY: partitions matches by key values, then
/// evaluates aggregate and non-aggregate RETURN expressions per group.
fn execute_grouped<G: GraphAccess + ?Sized>(
    graph: &G,
    matches: &[PatternMatch],
    group_by: &super::ast::GroupByClause,
    return_clause: &super::ast::ReturnClause,
    order_by: Option<&OrderByClause>,
    limit: Option<&super::ast::LimitClause>,
) -> crate::Result<GqlResult> {
    // 1. Partition matches by GROUP BY key values.
    let mut groups: Vec<(Vec<GqlValue>, Vec<&PatternMatch>)> = Vec::new();
    let mut key_index: HashMap<String, usize> = HashMap::new();

    for pm in matches {
        let key_values: Vec<GqlValue> = group_by
            .keys
            .iter()
            .map(|expr| eval_expr(expr, pm, &PathBindings::new(), graph, &DeadlineAbort::none()))
            .collect();
        let key_str = format!("{key_values:?}");

        if let Some(&idx) = key_index.get(&key_str) {
            groups[idx].1.push(pm);
        } else {
            let idx = groups.len();
            key_index.insert(key_str, idx);
            groups.push((key_values, vec![pm]));
        }
    }

    // 2. For each group, project RETURN items.
    let mut rows: GqlResult = Vec::with_capacity(groups.len());
    for (group_keys, group_matches) in &groups {
        let mut row = HashMap::with_capacity(return_clause.items.len());

        for item in &return_clause.items {
            let col = item
                .alias
                .as_deref()
                .map_or_else(|| expr_surface_name(&item.expr), String::from);

            let value = if expr_has_aggregate(&item.expr) {
                eval_aggregate_refs(&item.expr, group_matches, graph)?
            } else {
                // Non-aggregate: find matching GROUP BY key.
                let key_idx = group_by
                    .keys
                    .iter()
                    .position(|k| k == &item.expr)
                    .expect("validated by validate_aggregation");
                group_keys[key_idx].clone()
            };

            row.insert(col, value);
        }
        rows.push(row);
    }

    // 3. ORDER BY — sort by projected column values.
    //
    // `order.items[i].expr` may reference either a surface expression (e.g.
    // `p.name`) or a RETURN alias (`AS name`). The row map keys are built
    // from the RETURN item's alias (falling back to the surface name), so
    // we must resolve the ORDER BY expression back to the exact key that
    // was inserted. Matching against `return_clause.items` by structural
    // equality (`ret.expr == order.expr`) takes care of both cases; if the
    // user writes an ORDER BY expression that was not projected, we fall
    // back to `expr_surface_name`.
    if let Some(order) = order_by {
        rows.sort_by(|a, b| {
            for item in &order.items {
                let col = return_clause
                    .items
                    .iter()
                    .find(|ri| ri.expr == item.expr)
                    .map_or_else(
                        || expr_surface_name(&item.expr),
                        |ri| {
                            ri.alias
                                .as_deref()
                                .map_or_else(|| expr_surface_name(&ri.expr), String::from)
                        },
                    );
                let va = a.get(&col).unwrap_or(&GqlValue::Null);
                let vb = b.get(&col).unwrap_or(&GqlValue::Null);
                if let Some(ord) = gql_value_cmp(va, vb) {
                    let ord = if item.ascending { ord } else { ord.reverse() };
                    if ord != std::cmp::Ordering::Equal {
                        return ord;
                    }
                }
            }
            std::cmp::Ordering::Equal
        });
    }

    // 4. DISTINCT
    if return_clause.distinct {
        let mut seen = HashSet::new();
        rows.retain(|row| {
            let mut sorted_cols: Vec<(&String, &GqlValue)> = row.iter().collect();
            sorted_cols.sort_unstable_by_key(|(k, _)| k.as_str());
            let key = format!("{sorted_cols:?}");
            seen.insert(key)
        });
    }

    // 5. LIMIT
    if let Some(limit) = limit {
        // `limit.count` is u64 and usize is 64-bit on every target this crate
        // is built for, so the conversion is lossless in practice. On a 32-bit
        // target it would wrap to `count % 2^32` and cut the result SHORT —
        // fewer rows than asked, not more. Left as a cast rather than a checked
        // conversion because no 32-bit target is supported; if one ever is,
        // this needs a real bound, not this note.
        #[allow(clippy::cast_possible_truncation)]
        rows.truncate(limit.count as usize);
    }

    Ok(rows)
}

/// Executes a `RETURN <expr-list>` root statement against an empty context.
///
/// Produces exactly one record unless `SKIP >= 1` or `LIMIT == 0`, which
/// produce zero records (driver-compat fidelity — `SKIP` / `LIMIT` are
/// accepted for parity with normal queries).
///
/// No transaction is opened; no graph access is performed for the row
/// itself (only for evaluating function calls whose arguments name graph
/// entities — none of which can be bound here, so this stays a fast path).
///
/// # Errors
///
/// Returns `Err` when:
/// - `SKIP` or `LIMIT` evaluates to a non-integer value, a negative value,
///   or an expression that depends on graph state.
/// - Any item expression contains `Expr::ParamRef` — substitution must have
///   run before compilation. Caught by `debug_assert` in `eval_expr` plus
///   an explicit check here for release builds.
pub fn execute_const_return<G: GraphAccess + ?Sized>(
    graph: &G,
    query: &ConstReturnQuery,
    max_rows: u64,
    deadline: Option<Instant>,
) -> crate::Result<GqlResult> {
    let _ = max_rows; // const return is always one row; cap is a no-op.
    // const return performs no MATCH and no runaway loop; the deadline is a
    // no-op here, accepted only for signature symmetry with `execute`.
    let _ = deadline;
    // Defensive: param substitution must have run.
    for item in &query.items {
        if contains_param_ref(&item.expr) {
            return Err(Error::GqlCompileError(
                "internal error: unsubstituted parameter reached compiler — please report"
                    .into(),
            ));
        }
    }
    if let Some(ref e) = query.skip {
        if contains_param_ref(e) {
            return Err(Error::GqlCompileError(
                "internal error: unsubstituted parameter in SKIP".into(),
            ));
        }
    }
    if let Some(ref e) = query.limit {
        if contains_param_ref(e) {
            return Err(Error::GqlCompileError(
                "internal error: unsubstituted parameter in LIMIT".into(),
            ));
        }
    }

    let empty = PatternMatch::empty();

    // const return runs against an empty context — `shortestPath` cannot bind
    // nodes here, so no deadline is threaded.
    let no_abort = DeadlineAbort::none();

    // Resolve SKIP. A SKIP of 1 or more on a one-row stream yields zero rows.
    if let Some(ref e) = query.skip {
        let v = eval_expr(e, &empty, &PathBindings::new(), graph, &no_abort);
        let n = expect_nonneg_int(&v, "SKIP")?;
        if n >= 1 {
            return Ok(vec![]);
        }
    }

    // Resolve LIMIT. A LIMIT of 0 yields zero rows.
    if let Some(ref e) = query.limit {
        let v = eval_expr(e, &empty, &PathBindings::new(), graph, &no_abort);
        let n = expect_nonneg_int(&v, "LIMIT")?;
        if n == 0 {
            return Ok(vec![]);
        }
    }

    let row = project_row(&empty, &PathBindings::new(), &query.items, graph, &no_abort);
    // `DISTINCT` is parsed for fidelity but ignored here — one row is
    // already distinct from itself.
    Ok(vec![row])
}

/// Returns `true` when `expr` contains any `Expr::ParamRef` anywhere in its
/// subtree. Used as a defensive check in `execute_const_return` to surface
/// a structured error rather than a silent panic if substitution was
/// skipped.
fn contains_param_ref(expr: &Expr) -> bool {
    match expr {
        Expr::ParamRef(_) => true,
        // Leaf variants that bind no inner Expr.
        Expr::Literal(_)
        | Expr::Var(_)
        | Expr::PropAccess { .. }
        | Expr::ShortestPath { .. } => false,
        Expr::BinaryOp { left, right, .. } => {
            contains_param_ref(left) || contains_param_ref(right)
        }
        Expr::UnaryOp { expr: inner, .. } | Expr::IsNull { expr: inner, .. } => {
            contains_param_ref(inner)
        }
        Expr::Aggregate { arg, .. } => arg.as_deref().is_some_and(contains_param_ref),
        Expr::FunctionCall { args, .. } | Expr::ListLit(args) => {
            args.iter().any(contains_param_ref)
        }
        Expr::Subscript { list, index } => {
            contains_param_ref(list) || contains_param_ref(index)
        }
        Expr::ListPredicate { list, predicate, .. } => {
            contains_param_ref(list) || contains_param_ref(predicate)
        }
    }
}

/// Converts a `GqlValue` into a non-negative `u64` for SKIP/LIMIT
/// resolution, surfacing a structured compile error when the value is
/// not a valid count.
fn expect_nonneg_int(v: &GqlValue, ctx: &str) -> crate::Result<u64> {
    match v {
        GqlValue::Int(i) if *i >= 0 => {
            #[allow(clippy::cast_sign_loss)] // checked >= 0 above
            Ok(*i as u64)
        }
        GqlValue::Int(_) => Err(Error::GqlCompileError(format!(
            "{ctx} value must be a non-negative integer"
        ))),
        other => Err(Error::GqlCompileError(format!(
            "{ctx} must evaluate to an integer, got {other:?}"
        ))),
    }
}

/// Executes a read-only `GqlQuery` against `graph` and returns the result rows.
///
/// # Errors
///
/// Returns `Err` when the query fails to compile (invalid AST, unsupported
/// construct, or semantic errors), when graph access fails (I/O, missing
/// node/edge pages), or when expression evaluation fails at runtime (type
/// errors, unknown variables, unsupported function calls).
pub fn execute<G: GraphAccess + ?Sized>(
    graph: &G,
    query: &GqlQuery,
    max_rows: u64,
) -> crate::Result<GqlResult> {
    execute_with_deadline(graph, query, max_rows, None)
}

/// Deadline-aware variant of [`execute`] (v0.6.0 Fase 2 Task 6).
///
/// Identical to [`execute`] but threads a cooperative `deadline` into the
/// engine's runaway loops (cross-join, `[*1..N]` expansion, `shortestPath`
/// BFS, result materialization). `deadline == None` disables every check at the
/// cost of one bitmask + branch per iteration. On expiry the call returns an
/// [`Error::GqlCompileError`] carrying [`TIMEOUT_MSG_PREFIX`].
///
/// The [`DeadlineAbort`] cell (constructed here from `deadline`) carries the
/// out-of-band abort used by the infallible `shortestPath` BFS path; the
/// materialization loop inspects it after each projected row.
///
/// # Errors
///
/// Returns [`Error::GqlCompileError`] on a compile/scope error, on a result-row
/// cap overflow (carrying [`RESULT_CAP_MSG_PREFIX`]), or on a deadline overrun
/// (carrying [`TIMEOUT_MSG_PREFIX`]); propagates any storage error from the
/// underlying graph access.
pub fn execute_with_deadline<G: GraphAccess + ?Sized>(
    graph: &G,
    query: &GqlQuery,
    max_rows: u64,
    deadline: Option<Instant>,
) -> crate::Result<GqlResult> {
    let abort = DeadlineAbort::new(deadline);
    // 0. UNWIND dispatch — if present, delegate to specialized executor.
    //    `execute_with_unwind` relies on the GraphAccessor-boundary Cap B
    //    for its expanded output; it takes `max_rows` only for symmetry.
    if query.unwind_clause.is_some() {
        return execute_with_unwind(graph, query, max_rows, deadline, &abort);
    }

    // 1. Scope validation
    let bound_vars = collect_bound_vars(&query.match_clause);
    validate_scope(query, &bound_vars)?;

    // 2. Aggregation validation
    let is_aggregate = validate_aggregation(
        &query.return_clause.items,
        query.group_by.as_ref(),
    )?;

    // 2b. Aggregate pushdown: skip full materialization when possible.
    //     GROUP BY queries are not eligible for pushdown (they need per-group
    //     aggregation, not a single global aggregate).
    let pushdown_eligible = is_aggregate && query.group_by.is_none();

    if pushdown_eligible {
        if let Some(result) = try_aggregate_pushdown(graph, query) {
            return result;
        }
    }

    // 3. MATCH compilation
    let matches = compile_match(graph, &query.match_clause, deadline)?;

    // 3a. Path-binding materialisation: when `MATCH p = (…)` is present, build
    //     the per-row `PathBindings` so `nodes(p)`/`relationships(p)`/`length(p)`
    //     and `RETURN p` resolve. See `materialise_path_bindings` (zero-cost
    //     for the no-binding case).

    // 4. WHERE filtering — only include rows where predicate is `Some(true)`
    let filtered: Vec<PatternMatch> = if let Some(ref wc) = query.where_clause {
        matches
            .into_iter()
            .filter(|pm| {
                let paths = materialise_path_bindings(graph, &query.match_clause, pm);
                eval_as_tribool(&eval_expr(&wc.predicate, pm, &paths, graph, &abort))
                    == Some(true)
            })
            .collect()
    } else {
        matches
    };
    // A `shortestPath` in WHERE may have tripped the abort cell.
    if abort.is_aborted() {
        return Err(timeout_error());
    }

    // 4a. Cap A — guard against cross-join cartesian explosion. `filtered`
    //     is the post-WHERE match set; projection (the heavy per-row
    //     allocation) has not happened yet, so aborting here caps peak
    //     memory before it grows unbounded. `0` disables the cap.
    if max_rows > 0 && filtered.len() as u64 > max_rows {
        return Err(Error::GqlCompileError(format!(
            "{RESULT_CAP_MSG_PREFIX}query matched {} rows, exceeds max_result_rows={max_rows}",
            filtered.len()
        )));
    }

    // 4b. GROUP BY dispatch
    if let Some(ref gb) = query.group_by {
        return execute_grouped(
            graph,
            &filtered,
            gb,
            &query.return_clause,
            query.order_by.as_ref(),
            query.limit.as_ref(),
        );
    }

    // 5. RETURN projection
    if is_aggregate {
        // Aggregation: produce a single row
        let row = apply_aggregation(&filtered, &query.return_clause.items, graph)?;
        return Ok(vec![row]);
    }

    // Non-aggregate: project each match.
    // When ORDER BY is present we must retain the PatternMatch alongside the
    // projected row so that apply_order_by can re-evaluate sort-key expressions.
    // When there is no ORDER BY we can project and discard immediately, halving
    // peak memory usage (no need to keep both PatternMatch and GqlRow alive at
    // the same time).
    // Materialization is the last runaway loop: one allocation-heavy
    // `project_row` per matched row. The per-row `check_deadline` bounds it,
    // and after each projection we inspect the abort cell that a `shortestPath`
    // BFS inside the row may have tripped.
    let mut rows: GqlResult = if query.order_by.is_some() {
        let mut pairs: Vec<(PatternMatch, GqlRow)> = Vec::with_capacity(filtered.len());
        for (i, pm) in filtered.into_iter().enumerate() {
            check_deadline(deadline, i as u64)?;
            let paths = materialise_path_bindings(graph, &query.match_clause, &pm);
            let row = project_row(&pm, &paths, &query.return_clause.items, graph, &abort);
            if abort.is_aborted() {
                return Err(timeout_error());
            }
            pairs.push((pm, row));
        }

        // 6. ORDER BY
        if let Some(ref order) = query.order_by {
            apply_order_by(&mut pairs, order, graph);
        }

        // 7. Extract rows (drop PatternMatch)
        pairs.into_iter().map(|(_, row)| row).collect()
    } else {
        // No ORDER BY — project directly, PatternMatch dropped after each row.
        let mut out: GqlResult = Vec::with_capacity(filtered.len());
        for (i, pm) in filtered.into_iter().enumerate() {
            check_deadline(deadline, i as u64)?;
            let paths = materialise_path_bindings(graph, &query.match_clause, &pm);
            let row = project_row(&pm, &paths, &query.return_clause.items, graph, &abort);
            if abort.is_aborted() {
                return Err(timeout_error());
            }
            out.push(row);
        }
        out
    };

    // 8. DISTINCT — sort keys for deterministic deduplication.
    //    HashMap iteration order is not guaranteed, so we sort by column name
    //    before formatting. The Debug-based key is safe for the current GqlValue
    //    variants (Null, Bool, Int, Float, Str, List) because none of them
    //    produce ambiguous Debug output. If compound types (Map, etc.) are added,
    //    this should be replaced with a canonical hash-based approach.
    if query.return_clause.distinct {
        let mut seen = HashSet::new();
        rows.retain(|row| {
            let mut sorted_cols: Vec<(&String, &GqlValue)> = row.iter().collect();
            sorted_cols.sort_unstable_by_key(|(k, _)| k.as_str());
            let key = format!("{sorted_cols:?}");
            seen.insert(key)
        });
    }

    // 9. LIMIT
    if let Some(ref limit) = query.limit {
        // `limit.count` is u64 and usize is 64-bit on every target this crate
        // is built for, so the conversion is lossless in practice. On a 32-bit
        // target it would wrap to `count % 2^32` and cut the result SHORT —
        // fewer rows than asked, not more. Left as a cast rather than a checked
        // conversion because no 32-bit target is supported; if one ever is,
        // this needs a real bound, not this note.
        #[allow(clippy::cast_possible_truncation)]
        rows.truncate(limit.count as usize);
    }

    Ok(rows)
}

// ── Mutation result ──────────────────────────────────────────────────────────

/// The result of executing a GQL mutation statement.
///
/// Counts the number of graph elements created, deleted, or updated.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GqlMutationResult {
    /// Number of nodes created.
    pub nodes_created: u64,
    /// Number of edges created.
    pub edges_created: u64,
    /// Number of nodes deleted.
    pub nodes_deleted: u64,
    /// Number of edges deleted (includes cascaded removals).
    pub edges_deleted: u64,
    /// Number of individual property assignments applied.
    pub properties_set: u64,
    /// Number of labels added across all created or updated nodes.
    pub labels_added: u64,
    /// Number of elements changed by a pipeline mutation terminal.
    ///
    /// Populated only by pipeline `SET` / `CREATE` / `DELETE` terminals; left
    /// at `0` for plain mutations, which report their effect through the other
    /// counters instead.
    pub elements_changed: u64,
}

impl GqlMutationResult {
    /// Reports whether this mutation changed the graph in any observable way.
    ///
    /// Returns `true` when any counter is non-zero. Mirrors the Neo4j
    /// `contains-updates` flag surfaced in the Bolt `SUCCESS` `stats` metadata:
    /// a driver reads it to decide whether a statement had write effects.
    #[must_use]
    pub const fn contains_updates(&self) -> bool {
        self.nodes_created > 0
            || self.edges_created > 0
            || self.nodes_deleted > 0
            || self.edges_deleted > 0
            || self.properties_set > 0
            || self.labels_added > 0
            || self.elements_changed > 0
    }
}


/// Resolves MATCH variables for use in mutations.
///
/// Returns a flat list of `(variable_name, NodeId)` pairs from ALL match rows.
/// The caller builds the multi-value map from this list.
///
/// # Errors
///
/// Returns [`crate::Error::GqlCompileError`] if the MATCH clause cannot be compiled.
pub fn compile_match_for_mutation<G: GraphAccess + ?Sized>(
    graph: &G,
    mc: &MatchClause,
    deadline: Option<Instant>,
) -> crate::Result<Vec<(String, NodeId)>> {
    let matches = compile_match(graph, mc, deadline)?;
    let mut result = Vec::new();
    for pm in &matches {
        for pp in &mc.patterns {
            if let Some(ref v) = pp.start.var {
                if let Ok(node) = pm.get_node(v) {
                    result.push((v.clone(), node.id()));
                }
            }
            for (_, np) in &pp.hops {
                if let Some(ref v) = np.var {
                    if let Ok(node) = pm.get_node(v) {
                        result.push((v.clone(), node.id()));
                    }
                }
            }
        }
    }
    Ok(result)
}

/// Resolves MATCH variables into per-row binding maps for use in mutations.
///
/// Returns one `HashMap<variable_name, NodeId>` per matched row, preserving
/// the cross-join semantics of multi-pattern MATCH clauses. This is the
/// preferred API for MATCH…CREATE edge execution where each row must be
/// processed independently.
///
/// # Errors
///
/// Returns [`crate::Error::GqlCompileError`] if the MATCH clause cannot be compiled.
pub fn compile_match_bindings<G: GraphAccess + ?Sized>(
    graph: &G,
    mc: &MatchClause,
    deadline: Option<Instant>,
) -> crate::Result<Vec<HashMap<String, NodeId>>> {
    let matches = compile_match(graph, mc, deadline)?;
    let mut rows = Vec::with_capacity(matches.len());
    for pm in &matches {
        let mut row: HashMap<String, NodeId> = HashMap::new();
        for pp in &mc.patterns {
            if let Some(ref v) = pp.start.var {
                if let Ok(node) = pm.get_node(v) {
                    row.insert(v.clone(), node.id());
                }
            }
            for (_, np) in &pp.hops {
                if let Some(ref v) = np.var {
                    if let Ok(node) = pm.get_node(v) {
                        row.insert(v.clone(), node.id());
                    }
                }
            }
        }
        rows.push(row);
    }
    Ok(rows)
}

/// One matched MATCH row that distinguishes bound nodes from bound edges.
///
/// [`compile_match_bindings`] flattens every bound pattern variable to a
/// `NodeId`, which is enough for SET and CREATE (which only bind and write
/// nodes). `DELETE`/`DETACH DELETE` may target a relationship variable
/// (`MATCH ()-[r]->() DELETE r`), so its executor needs to tell a node
/// variable from an edge variable and resolve each to the right id. This
/// type carries both maps for a single matched row.
#[derive(Debug, Clone, Default)]
pub struct MatchRow {
    /// Node-variable name → bound node id.
    pub nodes: HashMap<String, NodeId>,
    /// Edge-variable name → bound edge id.
    pub edges: HashMap<String, crate::EdgeId>,
}

/// Edge-aware sibling of [`compile_match_bindings`]: returns one [`MatchRow`]
/// per matched pattern, resolving both node variables and relationship
/// variables declared in the `MATCH` patterns.
///
/// Used by the `DELETE`/`DETACH DELETE` write path, which must resolve a
/// relationship variable to an [`crate::EdgeId`]. The node-only
/// [`compile_match_bindings`] is retained for the SET/CREATE paths and the
/// benchmark shims that never bind edges.
///
/// # Errors
///
/// Propagates any error from the underlying pattern match (timeout, storage).
pub fn compile_match_rows<G: GraphAccess + ?Sized>(
    graph: &G,
    mc: &MatchClause,
    deadline: Option<Instant>,
) -> crate::Result<Vec<MatchRow>> {
    let matches = compile_match(graph, mc, deadline)?;
    let mut rows = Vec::with_capacity(matches.len());
    for pm in &matches {
        let mut row = MatchRow::default();
        for pp in &mc.patterns {
            if let Some(ref v) = pp.start.var {
                if let Ok(node) = pm.get_node(v) {
                    row.nodes.insert(v.clone(), node.id());
                }
            }
            for (ep, np) in &pp.hops {
                if let Some(ref v) = ep.var {
                    if let Ok(edge) = pm.get_edge(v) {
                        row.edges.insert(v.clone(), edge.id);
                    }
                }
                if let Some(ref v) = np.var {
                    if let Ok(node) = pm.get_node(v) {
                        row.nodes.insert(v.clone(), node.id());
                    }
                }
            }
        }
        rows.push(row);
    }
    Ok(rows)
}

/// Evaluates a SET value expression when only a literal RHS is acceptable.
///
/// This helper is kept for callers that want the literal-only guard. The
/// active `SET` pipeline (`apply_pipeline_set`) evaluates full expressions
/// via `eval_expr_on_binding`, so new code should prefer that path.
///
/// # Errors
///
/// Returns [`crate::Error::GqlMutationError`] if the expression is not a
/// supported literal.
#[doc(hidden)]
#[deprecated(
    since = "0.2.3",
    note = "Kept for API compatibility. SET now evaluates full expressions via `apply_pipeline_set`; this helper only accepts literals and is not used by the current pipeline."
)]
pub fn eval_set_value(expr: &Expr) -> crate::Result<Property> {
    match expr {
        Expr::Literal(lit) => literal_to_property(lit).ok_or_else(|| {
            Error::GqlMutationError(
                "cannot SET a property to NULL; use REMOVE (not yet supported)".into(),
            )
        }),
        other => Err(Error::GqlMutationError(format!(
            "SET value must be a literal; got expression: {other:?}. \
             Use `apply_pipeline_set` for full expression support."
        ))),
    }
}

// ── Pipeline executor (WITH clause) ──────────────────────────────────────────

/// A pipeline binding threaded through `PipelineStage`s.
///
/// `pm` preserves any node/edge bindings established by a prior `MATCH`
/// stage so that property access (`a.name`) and ordering (`ORDER BY a.id`)
/// work unchanged. `vals` holds scalar values introduced by `WITH`
/// projections, `UNWIND` variables, and other non-entity bindings.
///
/// A name that appears in `vals` shadows the same name in `pm` when
/// resolved by `eval_expr_on_binding`.
#[derive(Debug, Clone)]
struct Binding {
    pm: PatternMatch,
    vals: HashMap<String, GqlValue>,
}

impl Binding {
    fn empty() -> Self {
        Self { pm: PatternMatch::empty(), vals: HashMap::new() }
    }

    fn from_pattern_match(pm: PatternMatch) -> Self {
        Self { pm, vals: HashMap::new() }
    }
}

/// Validates that every variable referenced in a pipeline stage or its
/// terminal is bound by a previous stage. Without this check, out-of-scope
/// references evaluate to `GqlValue::Null` silently, which makes typos
/// and renaming errors very hard to diagnose in complex pipelines.
///
/// The bound set is threaded through the pipeline: each stage produces a
/// new set of bound variables that is visible to the following stages and
/// to the terminal.
// allow: cohesive state machine, splitting would fragment logic
#[allow(clippy::too_many_lines)]
fn validate_pipeline_scope(pq: &super::ast::PipelineQuery) -> crate::Result<()> {
    use super::ast::{PipelineStage, PipelineTerminal};

    fn check(referenced: &HashSet<String>, bound: &HashSet<String>) -> crate::Result<()> {
        for var in referenced {
            if !bound.contains(var) {
                return Err(Error::GqlCompileError(format!(
                    "variable '{var}' is not bound in the current pipeline scope"
                )));
            }
        }
        Ok(())
    }

    let mut bound: HashSet<String> = HashSet::new();

    for stage in &pq.stages {
        match stage {
            PipelineStage::Match { clause, where_clause } => {
                // MATCH introduces its own bindings; WHERE sees them.
                let new_bound = collect_bound_vars(clause);
                if let Some(wc) = where_clause {
                    let mut referenced = HashSet::new();
                    collect_expr_vars(&wc.predicate, &mut referenced);
                    check(&referenced, &new_bound)?;
                }
                bound = new_bound;
            }
            PipelineStage::Unwind(u) => {
                let mut referenced = HashSet::new();
                collect_expr_vars(&u.expr, &mut referenced);
                check(&referenced, &bound)?;
                bound.insert(u.var.clone());
            }
            PipelineStage::With(w) => {
                // The projection sees the current bound set; after the stage
                // the bound set becomes exactly the aliases/surface names of
                // the WITH items.
                let mut referenced = HashSet::new();
                for item in &w.items {
                    collect_expr_vars(&item.expr, &mut referenced);
                }
                check(&referenced, &bound)?;

                let new_bound: HashSet<String> = w
                    .items
                    .iter()
                    .map(|item| {
                        item.alias
                            .clone()
                            .unwrap_or_else(|| expr_surface_name(&item.expr))
                    })
                    .collect();

                // WHERE / ORDER BY run after projection, so they see the
                // new bound set.
                if let Some(wc) = w.where_clause.as_ref() {
                    let mut wc_refs = HashSet::new();
                    collect_expr_vars(&wc.predicate, &mut wc_refs);
                    check(&wc_refs, &new_bound)?;
                }
                if let Some(ob) = w.order_by.as_ref() {
                    let mut ob_refs = HashSet::new();
                    for item in &ob.items {
                        collect_expr_vars(&item.expr, &mut ob_refs);
                    }
                    check(&ob_refs, &new_bound)?;
                }

                bound = new_bound;
            }
        }
    }

    // Terminal clause — references must resolve against the final bound set.
    let mut referenced = HashSet::new();
    match &pq.terminal {
        PipelineTerminal::Return { clause, order_by, .. } => {
            for item in &clause.items {
                collect_expr_vars(&item.expr, &mut referenced);
            }
            // ORDER BY in the terminal sees the projected aliases plus the
            // incoming bindings, so validate against the union.
            let projected: HashSet<String> = clause
                .items
                .iter()
                .map(|item| {
                    item.alias
                        .clone()
                        .unwrap_or_else(|| expr_surface_name(&item.expr))
                })
                .collect();
            let mut union: HashSet<String> = bound.clone();
            union.extend(projected);
            if let Some(ob) = order_by {
                let mut ob_refs = HashSet::new();
                for item in &ob.items {
                    collect_expr_vars(&item.expr, &mut ob_refs);
                }
                check(&ob_refs, &union)?;
            }
        }
        PipelineTerminal::Set(sc) => {
            for a in &sc.assignments {
                match a {
                    SetAssignment::Property { var, value, .. } => {
                        referenced.insert(var.clone());
                        collect_expr_vars(value, &mut referenced);
                    }
                    SetAssignment::EntityOverwrite { var, map_expr }
                    | SetAssignment::EntityMerge { var, map_expr } => {
                        referenced.insert(var.clone());
                        collect_expr_vars(map_expr, &mut referenced);
                    }
                }
            }
        }
        PipelineTerminal::Create(cc) => {
            for pat in &cc.patterns {
                match pat {
                    super::ast::CreatePattern::Node { props, .. } => {
                        for (_, expr) in props {
                            collect_expr_vars(expr, &mut referenced);
                        }
                    }
                    super::ast::CreatePattern::Edge {
                        source_var,
                        target_var,
                        rel_props,
                        ..
                    } => {
                        referenced.insert(source_var.clone());
                        referenced.insert(target_var.clone());
                        for (_, expr) in rel_props {
                            collect_expr_vars(expr, &mut referenced);
                        }
                    }
                }
            }
        }
        PipelineTerminal::Delete(dc) => {
            for v in &dc.vars {
                referenced.insert(v.clone());
            }
        }
    }

    check(&referenced, &bound)
}

/// Executes a `PipelineQuery` and returns a `GqlResult`.
///
/// This is the entry point for statements that contain one or more `WITH`
/// stages. The executor walks stages left-to-right, threading a
/// `Vec<Binding>` between them, and projects the final bindings through
/// the terminal clause.
///
/// # Errors
///
/// Returns `Error::GqlSyntaxError` on malformed pipelines or
/// `Error::GqlUnsupported` for features not yet implemented.
pub fn execute_pipeline<G: GraphAccess + ?Sized>(
    graph: &G,
    pq: &super::ast::PipelineQuery,
    max_rows: u64,
) -> crate::Result<GqlResult> {
    execute_pipeline_with_deadline(graph, pq, max_rows, None)
}

/// Deadline-aware variant of [`execute_pipeline`] (v0.6.0 Fase 2 Task 6).
///
/// Threads the cooperative `deadline` into the MATCH stage's `compile_match`
/// (cross-join + `[*1..N]` expansion). `None` disables the checks. Pipeline
/// `shortestPath` projection is not deadline-instrumented (out of scope:
/// pipeline RETURN terminals do not currently accept `shortestPath`).
///
/// # Errors
///
/// Returns [`Error::GqlCompileError`] on a compile/scope error or on a deadline
/// overrun (carrying [`TIMEOUT_MSG_PREFIX`]), [`Error::GqlUnsupported`] for
/// unimplemented pipeline features, and propagates any storage error from the
/// underlying graph access.
pub fn execute_pipeline_with_deadline<G: GraphAccess + ?Sized>(
    graph: &G,
    pq: &super::ast::PipelineQuery,
    max_rows: u64,
    deadline: Option<Instant>,
) -> crate::Result<GqlResult> {
    use super::ast::{PipelineStage, PipelineTerminal};

    // Pipeline output expansion is guarded by Cap B at the GraphAccessor
    // boundary; the param exists for signature symmetry with `execute`.
    let _ = max_rows;
    validate_pipeline_scope(pq)?;

    // Start with a single empty binding; the first MATCH stage replaces it.
    let mut bindings: Vec<Binding> = vec![Binding::empty()];

    for stage in &pq.stages {
        bindings = match stage {
            PipelineStage::Match { clause, where_clause } => {
                execute_match_stage(graph, clause, where_clause.as_ref(), &bindings, deadline)?
            }
            PipelineStage::With(w) => execute_with_stage(graph, w, &bindings),
            PipelineStage::Unwind(u) => execute_unwind_stage(graph, u, &bindings),
        };
    }

    match &pq.terminal {
        PipelineTerminal::Return { clause, order_by, skip, limit } => {
            Ok(execute_pipeline_return(graph, &bindings, clause, order_by.as_ref(), *skip, *limit))
        }
        PipelineTerminal::Set(_)
        | PipelineTerminal::Create(_)
        | PipelineTerminal::Delete(_) => Err(Error::GqlUnsupported(
            "pipeline mutation terminals are not yet implemented (Phase 9)".into(),
        )),
    }
}

/// Executes a `CALL` procedure's result through optional `UNWIND` and `RETURN`
/// stages, reusing the existing pipeline stage helpers.
///
/// `procedure_list` is the [`GqlValue::List`] produced by the built-in
/// procedure (e.g. the node-label or edge-type strings). `yield_col` is the
/// name the list is bound to in the seed binding (e.g. `"vertex_labels"`).
///
/// - With `unwind`, the list is expanded into one binding per element.
/// - With `return_clause`, the bindings are projected through it.
/// - With neither, one row per binding is emitted, keyed by `yield_col`
///   (the `CALL … YIELD col` form with no trailing UNWIND).
///
/// Returns `GqlResult` (= `Vec<GqlRow>`), the same shape `execute_pipeline`
/// returns, so the server converts it identically.
#[must_use]
pub fn execute_call_result<G: GraphAccess + ?Sized>(
    graph: &G,
    procedure_list: GqlValue,
    yield_col: &str,
    unwind: Option<&super::ast::UnwindClause>,
    return_clause: Option<&super::ast::ReturnClause>,
) -> GqlResult {
    // Seed: one binding with `yield_col` bound to the list.
    let mut seed = Binding::empty();
    seed.vals.insert(yield_col.to_owned(), procedure_list);
    let mut bindings = vec![seed];

    if let Some(u) = unwind {
        bindings = execute_unwind_stage(graph, u, &bindings);
    }

    if let Some(clause) = return_clause {
        // Skip/limit are None — the pilot's CALL query carries neither; cap B
        // is enforced at the GraphAccessor boundary, as for pipelines.
        return execute_pipeline_return(graph, &bindings, clause, None, None, None);
    }

    // No RETURN: one GqlRow per binding, single column = bound name
    // (UNWIND var if present, else the yield col).
    let col = unwind.map_or_else(|| yield_col.to_owned(), |u| u.var.clone());
    bindings
        .iter()
        .map(|b| {
            let val = b.vals.get(&col).cloned().unwrap_or(GqlValue::Null);
            let mut row: GqlRow = std::collections::HashMap::new();
            row.insert(col.clone(), val);
            row
        })
        .collect()
}

/// Evaluates an `UNWIND expr AS var` stage inside a pipeline.
///
/// For each incoming binding:
/// - Evaluates `expr` against the binding.
/// - If the result is a `GqlValue::List`, emits one output binding per
///   element, with the element bound to `var` in `vals`.
/// - If the result is not a list (including `Null`), emits zero output
///   bindings for that incoming row. Matches Cypher semantics where
///   `UNWIND null AS x` yields no rows.
///
/// Each output binding preserves the incoming `PatternMatch` (so entity
/// references from prior MATCH stages remain resolvable) and inherits
/// the incoming `vals`, overriding `var` with the per-element value.
fn execute_unwind_stage<G: GraphAccess + ?Sized>(
    graph: &G,
    u: &super::ast::UnwindClause,
    incoming: &[Binding],
) -> Vec<Binding> {
    let mut out: Vec<Binding> = Vec::with_capacity(incoming.len());
    for b in incoming {
        let value = eval_expr_on_binding(&u.expr, b, graph);
        let GqlValue::List(items) = value else {
            continue;
        };
        for item in items {
            let mut new_vals = b.vals.clone();
            new_vals.insert(u.var.clone(), item);
            out.push(Binding {
                pm: b.pm.clone(),
                vals: new_vals,
            });
        }
    }
    out
}

/// Evaluates a `MATCH` stage against the incoming bindings.
///
/// Currently only supports MATCH as the FIRST stage (ignores incoming
/// bindings beyond verifying there is exactly one empty binding). A second
/// MATCH inside a pipeline is out of scope per the spec.
fn execute_match_stage<G: GraphAccess + ?Sized>(
    graph: &G,
    clause: &MatchClause,
    where_clause: Option<&super::ast::WhereClause>,
    incoming: &[Binding],
    deadline: Option<Instant>,
) -> crate::Result<Vec<Binding>> {
    // The parser guarantees MATCH is always the first pipeline stage, so
    // `incoming` here is the single empty seed binding produced by
    // `execute_pipeline`. Enforce this in release too — a malformed
    // pipeline must fail loudly rather than silently drop incoming bindings.
    if incoming.len() != 1 || !incoming[0].vals.is_empty() {
        return Err(Error::GqlCompileError(
            "MATCH must be the first pipeline stage and receive the empty seed binding".into(),
        ));
    }

    let matches = compile_match(graph, clause, deadline)?;
    let abort = DeadlineAbort::new(deadline);
    let filtered: Vec<PatternMatch> = if let Some(wc) = where_clause {
        let f: Vec<PatternMatch> = matches
            .into_iter()
            .filter(|pm| {
                eval_as_tribool(&eval_expr(&wc.predicate, pm, &PathBindings::new(), graph, &abort)) == Some(true)
            })
            .collect();
        if abort.is_aborted() {
            return Err(timeout_error());
        }
        f
    } else {
        matches
    };
    Ok(filtered.into_iter().map(Binding::from_pattern_match).collect())
}

/// Evaluates a `WITH` stage: projection (+ optional aggregation/DISTINCT)
/// followed by WHERE → ORDER BY → SKIP → LIMIT.
///
/// Aggregation is triggered when any projected item contains an aggregate
/// function; non-aggregate items become group keys. All-aggregate
/// projections produce a single output binding. ORDER BY / SKIP / LIMIT
/// are applied after projection so they can reference aliases introduced
/// by the WITH itself.
fn execute_with_stage<G: GraphAccess + ?Sized>(
    graph: &G,
    w: &super::ast::WithClause,
    incoming: &[Binding],
) -> Vec<Binding> {
    execute_with_stage_parts(
        graph,
        &w.items,
        w.distinct,
        w.where_clause.as_ref(),
        w.order_by.as_ref(),
        w.skip,
        w.limit,
        incoming,
    )
}

/// Component-level variant of [`execute_with_stage`] that avoids cloning a
/// whole [`WithClause`] when callers already have the individual parts by
/// reference (e.g. the RETURN-terminal executor).
#[allow(clippy::too_many_arguments)]
fn execute_with_stage_parts<G: GraphAccess + ?Sized>(
    graph: &G,
    items: &[ReturnItem],
    distinct: bool,
    where_clause: Option<&super::ast::WhereClause>,
    order_by: Option<&OrderByClause>,
    skip: Option<super::ast::SkipClause>,
    limit: Option<super::ast::LimitClause>,
    incoming: &[Binding],
) -> Vec<Binding> {
    let any_agg = items.iter().any(|it| expr_has_aggregate(&it.expr));
    let all_agg = any_agg && items.iter().all(|it| expr_has_aggregate(&it.expr));

    let projected: Vec<Binding> = if any_agg {
        if all_agg {
            // Global aggregate → single output binding.
            let mut vals: HashMap<String, GqlValue> = HashMap::new();
            for item in items {
                let alias = item.alias.clone().unwrap_or_else(|| expr_surface_name(&item.expr));
                let val = eval_aggregate_over_bindings(&item.expr, incoming, graph);
                vals.insert(alias, val);
            }
            vec![Binding { pm: PatternMatch::empty(), vals }]
        } else {
            // Grouping: non-aggregate items form the group key.
            execute_with_stage_grouped_items(graph, items, incoming)
        }
    } else {
        project_with_items_slice(graph, items, incoming)
    };

    // DISTINCT on the projected rows (applies to the full output tuple).
    let distinct_rows = if distinct {
        distinct_bindings(&projected)
    } else {
        projected
    };

    // WHERE: evaluate against the newly-bound aliases.
    let after_where: Vec<Binding> = if let Some(wc) = where_clause {
        distinct_rows
            .into_iter()
            .filter(|b| eval_as_tribool(&eval_expr_on_binding(&wc.predicate, b, graph)) == Some(true))
            .collect()
    } else {
        distinct_rows
    };

    // ORDER BY: sort by expressions evaluated against the new bindings.
    let mut ordered = after_where;
    if let Some(ob) = order_by {
        order_bindings_by(graph, &mut ordered, ob);
    }

    // SKIP then LIMIT.
    let skipped = match skip {
        #[allow(clippy::cast_possible_truncation)]
        Some(s) => ordered.into_iter().skip(s.count as usize).collect(),
        None => ordered,
    };
    match limit {
        #[allow(clippy::cast_possible_truncation)]
        Some(l) => skipped.into_iter().take(l.count as usize).collect(),
        None => skipped,
    }
}

/// Projects each incoming binding through the WITH items without
/// aggregating. Preserves node/edge bindings when the projection is a bare
/// variable reference so downstream stages can still do `alias.prop`.
///
/// Takes the item list as a slice so the RETURN terminal executor can
/// reuse this logic without synthesising a `WithClause`.
fn project_with_items_slice<G: GraphAccess + ?Sized>(
    graph: &G,
    items: &[ReturnItem],
    incoming: &[Binding],
) -> Vec<Binding> {
    let mut out = Vec::with_capacity(incoming.len());
    for b in incoming {
        let mut new_vals: HashMap<String, GqlValue> = HashMap::new();
        let mut remaining_nodes = b.pm.nodes_clone();
        let mut remaining_edges = b.pm.edges_clone();
        let mut next_nodes = HashMap::new();
        let mut next_edges = HashMap::new();

        for item in items {
            let alias = item.alias.clone().unwrap_or_else(|| expr_surface_name(&item.expr));

            if let Expr::Var(name) = &item.expr {
                if let Some(node) = remaining_nodes.remove(name) {
                    next_nodes.insert(alias.clone(), node);
                    continue;
                }
                if let Some(edge) = remaining_edges.remove(name) {
                    next_edges.insert(alias.clone(), edge);
                    continue;
                }
                if let Some(val) = b.vals.get(name) {
                    new_vals.insert(alias, val.clone());
                    continue;
                }
            }

            let val = eval_expr_on_binding(&item.expr, b, graph);
            // If the projection yields a first-class entity (e.g. `nodes[i]`
            // where `nodes = collect(a)`), rebind it in the PatternMatch so
            // downstream stages can still do `alias.prop` and SET. The real
            // engine entity is re-read by id, so the binding stays a live
            // `Node`/`Edge`, not a fabricated one.
            match rebind_entity_value(&val, graph) {
                Some(RebindEntity::Node(node)) => {
                    next_nodes.insert(alias, node);
                }
                Some(RebindEntity::Edge(edge)) => {
                    next_edges.insert(alias, edge);
                }
                None => {
                    new_vals.insert(alias, val);
                }
            }
        }

        out.push(Binding {
            pm: PatternMatch::new(next_nodes, next_edges),
            vals: new_vals,
        });
    }
    out
}

/// A live engine entity recovered from a projected first-class `GqlValue`.
enum RebindEntity {
    Node(crate::Node),
    Edge(crate::Edge),
}

/// If `val` is a first-class `GqlValue::Node`/`Relationship`, re-reads the
/// corresponding live engine entity by id so it can be rebound in a
/// downstream stage's [`PatternMatch`] (enabling `alias.prop` and `SET`).
///
/// Returns `None` for scalars and lists. Crucially, this keys off the *value
/// kind* (Node/Relationship), NOT off an integer happening to match a node id:
/// a bare `GqlValue::Int` (a `count`, `size`, or property value) is never
/// rehydrated into an entity, which previously caused scalars to collide with
/// node ids (Fase B C3).
fn rebind_entity_value<G: GraphAccess + ?Sized>(
    val: &GqlValue,
    graph: &G,
) -> Option<RebindEntity> {
    match val {
        #[allow(clippy::cast_sign_loss)]
        GqlValue::Node(n) if n.id >= 0 => {
            graph.node(NodeId(n.id as u64)).ok().map(RebindEntity::Node)
        }
        #[allow(clippy::cast_sign_loss)]
        GqlValue::Relationship(r) if r.id >= 0 => {
            graph.edge(EdgeId(r.id as u64)).ok().map(RebindEntity::Edge)
        }
        _ => None,
    }
}

/// Partitions `incoming` by the non-aggregate projection keys, then for
/// each partition evaluates the aggregate items to produce one output
/// binding per group. Slice-based so it can be reused by the RETURN
/// terminal without synthesising a `WithClause`.
fn execute_with_stage_grouped_items<G: GraphAccess + ?Sized>(
    graph: &G,
    items: &[ReturnItem],
    incoming: &[Binding],
) -> Vec<Binding> {
    // Build group key signatures from the non-aggregate items.
    let key_items: Vec<&ReturnItem> = items
        .iter()
        .filter(|it| !expr_has_aggregate(&it.expr))
        .collect();

    // Accumulate members per group while preserving first-occurrence order.
    // `group_idx` maps the key to its position in `ordered_groups`, so the
    // second pass iterates `ordered_groups` directly — no HashMap::remove
    // is needed and there is no lookup that could fail.
    let mut group_idx: HashMap<String, usize> = HashMap::new();
    let mut ordered_groups: Vec<Vec<usize>> = Vec::new();
    for (idx, b) in incoming.iter().enumerate() {
        let sig_vals: Vec<GqlValue> = key_items
            .iter()
            .map(|it| eval_expr_on_binding(&it.expr, b, graph))
            .collect();
        let key = gql_value_slice_to_key(&sig_vals);
        if let Some(&gi) = group_idx.get(&key) {
            ordered_groups[gi].push(idx);
        } else {
            let gi = ordered_groups.len();
            group_idx.insert(key, gi);
            ordered_groups.push(vec![idx]);
        }
    }

    let mut out = Vec::with_capacity(ordered_groups.len());
    for member_indices in ordered_groups {
        let members: Vec<Binding> =
            member_indices.iter().map(|&i| incoming[i].clone()).collect();

        let mut vals: HashMap<String, GqlValue> = HashMap::new();
        let mut next_nodes = HashMap::new();
        let mut next_edges = HashMap::new();
        let representative = &members[0];

        for item in items {
            let alias = item.alias.clone().unwrap_or_else(|| expr_surface_name(&item.expr));
            if expr_has_aggregate(&item.expr) {
                let val = eval_aggregate_over_bindings(&item.expr, &members, graph);
                vals.insert(alias, val);
            } else if let Expr::Var(name) = &item.expr {
                if let Ok(node) = representative.pm.get_node(name) {
                    next_nodes.insert(alias.clone(), node.clone());
                } else if let Ok(edge) = representative.pm.get_edge(name) {
                    next_edges.insert(alias.clone(), edge.clone());
                } else if let Some(v) = representative.vals.get(name) {
                    vals.insert(alias, v.clone());
                } else {
                    vals.insert(alias, GqlValue::Null);
                }
            } else {
                let v = eval_expr_on_binding(&item.expr, representative, graph);
                vals.insert(alias, v);
            }
        }

        out.push(Binding {
            pm: PatternMatch::new(next_nodes, next_edges),
            vals,
        });
    }
    out
}

/// Evaluates a single aggregate expression over a slice of bindings.
/// Supports COUNT(*) / COUNT(expr) / SUM / AVG / MIN / MAX / COLLECT.
fn eval_aggregate_over_bindings<G: GraphAccess + ?Sized>(
    expr: &Expr,
    bindings: &[Binding],
    graph: &G,
) -> GqlValue {
    // Function-of-aggregate: `size(collect(x))`, `size(collect(x)) + 1`,
    // etc. Resolve inner aggregates first, then evaluate the outer
    // expression in a synthetic binding where the aggregate subexpression
    // stands in for its computed value.
    if !matches!(expr, Expr::Aggregate { .. }) && expr_has_aggregate(expr) {
        return eval_nested_aggregate_expr(expr, bindings, graph);
    }

    let Expr::Aggregate { func, arg } = expr else {
        // Not an aggregate: evaluate against the first binding (fallback).
        return bindings
            .first()
            .map_or(GqlValue::Null, |b| eval_expr_on_binding(expr, b, graph));
    };

    let arg_vals: Vec<GqlValue> = match arg.as_deref() {
        None => {
            // COUNT(*) — number of rows.
            #[allow(clippy::cast_possible_wrap)]
            return GqlValue::Int(bindings.len() as i64);
        }
        Some(e) => bindings.iter().map(|b| eval_expr_on_binding(e, b, graph)).collect(),
    };

    match func {
        AggFunc::Count => {
            let c = arg_vals.iter().filter(|v| !matches!(v, GqlValue::Null)).count();
            #[allow(clippy::cast_possible_wrap)]
            GqlValue::Int(c as i64)
        }
        AggFunc::Sum => {
            let mut as_int: i64 = 0;
            let mut as_float: f64 = 0.0;
            let mut used_float = false;
            for v in &arg_vals {
                match v {
                    GqlValue::Int(i) => as_int += i,
                    GqlValue::Float(f) => {
                        used_float = true;
                        as_float += f;
                    }
                    _ => {}
                }
            }
            if used_float {
                #[allow(clippy::cast_precision_loss)]
                let total = as_float + as_int as f64;
                GqlValue::Float(total)
            } else {
                GqlValue::Int(as_int)
            }
        }
        AggFunc::Avg => {
            let mut count: i64 = 0;
            let mut sum: f64 = 0.0;
            for v in &arg_vals {
                match v {
                    #[allow(clippy::cast_precision_loss)]
                    GqlValue::Int(i) => {
                        sum += *i as f64;
                        count += 1;
                    }
                    GqlValue::Float(f) => {
                        sum += f;
                        count += 1;
                    }
                    _ => {}
                }
            }
            if count == 0 {
                GqlValue::Null
            } else {
                #[allow(clippy::cast_precision_loss)]
                let avg = sum / count as f64;
                GqlValue::Float(avg)
            }
        }
        AggFunc::Min => {
            arg_vals
                .iter()
                .filter(|v| !matches!(v, GqlValue::Null))
                .min_by(|a, b| compare_sort_keys(a, b))
                .cloned()
                .unwrap_or(GqlValue::Null)
        }
        AggFunc::Max => {
            arg_vals
                .iter()
                .filter(|v| !matches!(v, GqlValue::Null))
                .max_by(|a, b| compare_sort_keys(a, b))
                .cloned()
                .unwrap_or(GqlValue::Null)
        }
        AggFunc::Collect => {
            let items: Vec<GqlValue> =
                arg_vals.into_iter().filter(|v| !matches!(v, GqlValue::Null)).collect();
            GqlValue::List(items)
        }
    }
}

/// Evaluates an expression that contains aggregates but is not itself an
/// aggregate call (e.g. `size(collect(x))`, `collect(x)[0]`). Computes
/// the inner aggregates first, then substitutes their computed values
/// into a copy of the expression tree and evaluates the outer expression
/// against the first binding (or an empty one if none).
fn eval_nested_aggregate_expr<G: GraphAccess + ?Sized>(
    expr: &Expr,
    bindings: &[Binding],
    graph: &G,
) -> GqlValue {
    let substituted = substitute_aggregates(expr, bindings, graph);
    let default = Binding::empty();
    let ctx = bindings.first().unwrap_or(&default);
    eval_expr_on_binding(&substituted, ctx, graph)
}

/// Recursively walks `expr`, replacing every `Expr::Aggregate` subtree
/// with a computed `Expr::Literal(...)` value. Non-aggregate subtrees
/// are preserved verbatim.
fn substitute_aggregates<G: GraphAccess + ?Sized>(
    expr: &Expr,
    bindings: &[Binding],
    graph: &G,
) -> Expr {
    if let Expr::Aggregate { .. } = expr {
        let value = eval_aggregate_over_bindings(expr, bindings, graph);
        return gql_value_to_literal_expr(value);
    }
    match expr {
        Expr::BinaryOp { left, op, right } => Expr::BinaryOp {
            left: Box::new(substitute_aggregates(left, bindings, graph)),
            op: *op,
            right: Box::new(substitute_aggregates(right, bindings, graph)),
        },
        Expr::UnaryOp { op, expr: inner } => Expr::UnaryOp {
            op: *op,
            expr: Box::new(substitute_aggregates(inner, bindings, graph)),
        },
        Expr::IsNull { expr: inner, negated } => Expr::IsNull {
            expr: Box::new(substitute_aggregates(inner, bindings, graph)),
            negated: *negated,
        },
        Expr::FunctionCall { name, args } => Expr::FunctionCall {
            name: name.clone(),
            args: args.iter().map(|a| substitute_aggregates(a, bindings, graph)).collect(),
        },
        Expr::ListLit(items) => Expr::ListLit(
            items.iter().map(|e| substitute_aggregates(e, bindings, graph)).collect(),
        ),
        Expr::Subscript { list, index } => Expr::Subscript {
            list: Box::new(substitute_aggregates(list, bindings, graph)),
            index: Box::new(substitute_aggregates(index, bindings, graph)),
        },
        other => other.clone(),
    }
}

/// Converts a computed `GqlValue` into an `Expr::Literal` with matching
/// semantics. `GqlValue::List` becomes `Expr::ListLit` of literal items
/// (rather than `Literal::List`) so that the outer evaluator can use the
/// same code path as user-written list literals.
fn gql_value_to_literal_expr(value: GqlValue) -> Expr {
    match value {
        // `Map` shares the `Null` literal: it has no literal-expression form
        // (there is no `Expr::MapLit`), so callers must handle `GqlValue::Map`
        // before reaching here. `Null` is the safe fallback — the user-visible
        // gate is `gql_value_to_literal`'s `UnsupportedParamValue` error in
        // param_substitution.
        GqlValue::Null
        | GqlValue::Map(_)
        | GqlValue::Node(_)
        | GqlValue::Relationship(_)
        | GqlValue::Path(_) => Expr::Literal(Literal::Null),
        GqlValue::Bool(b) => Expr::Literal(Literal::Bool(b)),
        GqlValue::Int(i) => Expr::Literal(Literal::Int(i)),
        GqlValue::Float(f) => Expr::Literal(Literal::Float(f)),
        GqlValue::Str(s) => Expr::Literal(Literal::Str(s)),
        GqlValue::List(items) => {
            Expr::ListLit(items.into_iter().map(gql_value_to_literal_expr).collect())
        }
    }
}

/// Serialises a slice of `GqlValue` into a deterministic `String` key
/// suitable for use as a `HashMap`/`HashSet` key. `Float` is encoded via
/// its IEEE-754 bits so `NaN` and `-0.0 / 0.0` are treated as distinct
/// keys but stable within a run. Non-printable separators (`\x1f`, `\x1e`)
/// delimit components so that concatenated values cannot collide
/// across element boundaries.
fn gql_value_slice_to_key(values: &[GqlValue]) -> String {
    let mut out = String::with_capacity(values.len() * 8);
    for v in values {
        match v {
            GqlValue::Null => out.push_str("N\x1f"),
            GqlValue::Bool(b) => {
                out.push('B');
                out.push(if *b { 'T' } else { 'F' });
                out.push('\x1f');
            }
            GqlValue::Int(i) => {
                out.push('I');
                out.push_str(&i.to_string());
                out.push('\x1f');
            }
            GqlValue::Float(f) => {
                out.push('F');
                out.push_str(&f.to_bits().to_string());
                out.push('\x1f');
            }
            GqlValue::Str(s) => {
                // Escape the delimiter bytes inside the string so user data
                // cannot forge a DISTINCT/GROUP-BY collision by embedding the
                // raw separators. `\x1d` is the escape marker; any embedded
                // `\x1d`, `\x1e`, `\x1f` becomes `\x1d` followed by the byte.
                out.push('S');
                for ch in s.chars() {
                    match ch {
                        '\x1d' | '\x1e' | '\x1f' => {
                            out.push('\x1d');
                            out.push(ch);
                        }
                        _ => out.push(ch),
                    }
                }
                out.push('\x1f');
            }
            GqlValue::List(items) => {
                out.push('[');
                out.push_str(&gql_value_slice_to_key(items));
                out.push(']');
                out.push('\x1f');
            }
            GqlValue::Map(m) => {
                // Deterministic: sort keys so the same map yields the same key
                // regardless of HashMap iteration order. Mirrors the List
                // encoding's separator discipline.
                out.push('M');
                let mut sorted_keys: Vec<&String> = m.keys().collect();
                sorted_keys.sort();
                for k in sorted_keys {
                    out.push_str(k);
                    out.push('=');
                    out.push_str(&gql_value_slice_to_key(std::slice::from_ref(&m[k])));
                }
                out.push('}');
                out.push('\x1f');
            }
            // Entity values are keyed by their stable id so that DISTINCT/GROUP BY
            // on node/rel/path variables works correctly. Real serialization to
            // `PackStreamValue` is added in a later task.
            GqlValue::Node(n) => {
                // 'V' = Vertex; 'N' is already taken by Null's prefix.
                out.push('V');
                out.push_str(&n.id.to_string());
                out.push('\x1f');
            }
            GqlValue::Relationship(r) => {
                out.push('R');
                out.push_str(&r.id.to_string());
                out.push('\x1f');
            }
            GqlValue::Path(p) => {
                // Keyed on the full node+relationship id sequence: two paths
                // between the same nodes via different edges (multi-edges) must
                // not collide for DISTINCT/GROUP BY.
                out.push('P');
                for n in &p.nodes {
                    out.push_str(&n.id.to_string());
                    out.push(',');
                }
                out.push(';');
                for r in &p.rels {
                    out.push_str(&r.id.to_string());
                    out.push(',');
                }
                out.push('\x1f');
            }
        }
        out.push('\x1e');
    }
    out
}

/// Keeps the first occurrence of each binding's projected value vector.
/// Uses a deterministic string serialisation of the `vals` map and any
/// bound node/edge ids as the equality key, giving O(n) deduplication.
fn distinct_bindings(rows: &[Binding]) -> Vec<Binding> {
    let mut out: Vec<Binding> = Vec::with_capacity(rows.len());
    let mut seen: HashSet<String> = HashSet::with_capacity(rows.len());

    for b in rows {
        let key = binding_distinct_key(b);
        if seen.insert(key) {
            out.push(b.clone());
        }
    }
    out
}

/// Builds a deterministic DISTINCT/GROUP-BY key for a full binding
/// (all `vals` keys sorted + all node/edge bindings by id).
fn binding_distinct_key(b: &Binding) -> String {
    let mut sig: Vec<GqlValue> = Vec::with_capacity(b.vals.len() * 2);

    let mut keys: Vec<&String> = b.vals.keys().collect();
    keys.sort();
    for k in &keys {
        sig.push(GqlValue::Str((*k).clone()));
        sig.push(b.vals.get(*k).cloned().unwrap_or(GqlValue::Null));
    }

    // Iterate the variable names directly — no HashMap::clone needed.
    let mut node_vars: Vec<&str> = b.pm.node_vars().collect();
    node_vars.sort_unstable();
    for v in &node_vars {
        if let Ok(n) = b.pm.get_node(v) {
            sig.push(GqlValue::Str((*v).to_owned()));
            #[allow(clippy::cast_possible_wrap)]
            sig.push(GqlValue::Int(n.id().as_u64() as i64));
        }
    }

    let mut edge_vars: Vec<&str> = b.pm.edge_vars().collect();
    edge_vars.sort_unstable();
    for v in &edge_vars {
        if let Ok(e) = b.pm.get_edge(v) {
            sig.push(GqlValue::Str((*v).to_owned()));
            #[allow(clippy::cast_possible_wrap)]
            sig.push(GqlValue::Int(e.id().as_u64() as i64));
        }
    }

    gql_value_slice_to_key(&sig)
}

/// Sorts `bindings` in place by evaluating each `OrderItem`'s expression
/// against the (post-projection) binding. Mirrors the logic of
/// `apply_order_by` but operates on pipeline bindings.
fn order_bindings_by<G: GraphAccess + ?Sized>(
    graph: &G,
    bindings: &mut [Binding],
    order: &OrderByClause,
) {
    if bindings.is_empty() || order.items.is_empty() {
        return;
    }
    let keys: Vec<Vec<GqlValue>> = bindings
        .iter()
        .map(|b| {
            order
                .items
                .iter()
                .map(|item| eval_expr_on_binding(&item.expr, b, graph))
                .collect()
        })
        .collect();
    let mut indices: Vec<usize> = (0..bindings.len()).collect();
    indices.sort_by(|&a, &b| {
        for (idx, item) in order.items.iter().enumerate() {
            let cmp = compare_sort_keys(&keys[a][idx], &keys[b][idx]);
            let directed = if item.ascending { cmp } else { cmp.reverse() };
            if directed != std::cmp::Ordering::Equal {
                return directed;
            }
        }
        std::cmp::Ordering::Equal
    });
    // Reorder in place via the cycle-follower permutation used by
    // `apply_order_by`. Avoids cloning every `Binding` just to reorder.
    apply_permutation(bindings, &indices);
}

/// Projects final pipeline bindings through a RETURN terminal and applies
/// any post-projection ORDER BY / SKIP / LIMIT. Aggregates are treated as
/// a collapsing WITH: items with at least one aggregate produce a single
/// group (or one group per non-aggregate key).
fn execute_pipeline_return<G: GraphAccess + ?Sized>(
    graph: &G,
    bindings: &[Binding],
    clause: &super::ast::ReturnClause,
    order_by: Option<&OrderByClause>,
    skip: Option<super::ast::SkipClause>,
    limit: Option<super::ast::LimitClause>,
) -> GqlResult {
    // Reuse the WITH stage logic for aggregation / non-aggregate projection
    // via the component-level entry point so we don't allocate a synthetic
    // `WithClause` and clone every RETURN item.
    let final_bindings = execute_with_stage_parts(
        graph,
        &clause.items,
        clause.distinct,
        None,
        order_by,
        skip,
        limit,
        bindings,
    );

    let mut rows: GqlResult = Vec::with_capacity(final_bindings.len());
    for b in &final_bindings {
        let mut row = HashMap::with_capacity(clause.items.len());
        for item in &clause.items {
            let col = item
                .alias
                .clone()
                .unwrap_or_else(|| expr_surface_name(&item.expr));
            // Read from the binding produced by the virtual WITH: value goes
            // in `vals` for scalars/aggregates and the `PatternMatch` for
            // entities. Fall back to Null when neither resolves.
            let val = b.vals.get(&col).cloned().unwrap_or_else(|| {
                if b.pm.get_node(&col).is_ok() || b.pm.get_edge(&col).is_ok() {
                    eval_expr_on_binding(&Expr::Var(col.clone()), b, graph)
                } else {
                    GqlValue::Null
                }
            });
            row.insert(col, val);
        }
        rows.push(row);
    }
    rows
}

/// Executes a `PipelineQuery` whose terminal clause is a mutation
/// (`SET` at the moment; `CREATE` and `DELETE` terminals return
/// `GqlUnsupported` pending separate implementation).
///
/// Walks the read-only stages with `&*graph`, collects all final
/// bindings, and then applies the mutation terminal under `&mut graph`.
/// This mirrors the two-phase read/write separation used by
/// `tessera-graph-server::execute_match_mutation`.
///
/// # Errors
///
/// Returns `Error::GqlSyntaxError` or `Error::GqlMutationError` on
/// malformed input or an unsupported terminal, and propagates any
/// read/write errors from `Graph`.
pub fn execute_pipeline_mutation(
    graph: &mut crate::Graph,
    stmt: &super::ast::GqlStatement,
    txn_id: Option<u64>,
) -> crate::Result<GqlMutationResult> {
    use super::ast::{GqlStatement, PipelineStage, PipelineTerminal};

    let GqlStatement::Pipeline(pq) = stmt else {
        return Err(Error::GqlMutationError(
            "execute_pipeline_mutation requires a pipeline statement".into(),
        ));
    };

    validate_pipeline_scope(pq)?;

    // Phase 1 — read-only stages. Inside a transaction they run over the
    // transaction's snapshot (via `TxnView`, seeing its own pending writes);
    // otherwise over the committed graph. The read view is scoped and dropped
    // before the write phase so the two never hold conflicting borrows.
    let mut bindings: Vec<Binding> = vec![Binding::empty()];
    let run_stages = |view: &dyn GraphAccess,
                      mut bindings: Vec<Binding>|
     -> crate::Result<Vec<Binding>> {
        for stage in &pq.stages {
            bindings = match stage {
                PipelineStage::Match { clause, where_clause } => {
                    // Pipeline-mutation match phase is not deadline-wired yet
                    // (the read-side `execute_pipeline` carries the deadline);
                    // threaded as `None` here. See Task 6 design decision #6.
                    execute_match_stage(view, clause, where_clause.as_ref(), &bindings, None)?
                }
                PipelineStage::With(w) => execute_with_stage(view, w, &bindings),
                PipelineStage::Unwind(u) => execute_unwind_stage(view, u, &bindings),
            };
        }
        Ok(bindings)
    };
    bindings = match txn_id {
        Some(t) => {
            let view = super::txn_view::TxnView::new(graph, t);
            run_stages(&view, bindings)?
        }
        None => run_stages(&*graph, bindings)?,
    };

    // Phase 2 — apply mutation terminal under `&mut graph`.
    match &pq.terminal {
        PipelineTerminal::Set(set) => apply_pipeline_set(graph, &bindings, set, txn_id),
        PipelineTerminal::Delete(dc) => apply_pipeline_delete(graph, &bindings, dc, txn_id),
        PipelineTerminal::Return { .. } => Err(Error::GqlMutationError(
            "execute_pipeline_mutation called on a RETURN pipeline; \
             use execute_pipeline for read-only queries"
                .into(),
        )),
        PipelineTerminal::Create(_) => Err(Error::GqlUnsupported(
            "CREATE pipeline terminal is not yet implemented".into(),
        )),
    }
}

/// Applies a `DELETE` / `DETACH DELETE` terminal against each final pipeline
/// binding (`MATCH … WITH n DELETE n`, `… WITH r DELETE r`).
///
/// For each binding × each target variable, resolves the variable from the
/// binding's `PatternMatch` — a node first, then an edge — and delegates to the
/// shared engine delete helpers so the connected-node rule, the `DETACH`
/// cascade, the transactional routing, and the idempotent dedup are identical
/// to the MATCH and UNWIND delete paths. Nodes and edges are deduplicated
/// across all bindings so a variable that binds the same entity in several rows
/// is deleted (and counted) once.
fn apply_pipeline_delete(
    graph: &mut crate::Graph,
    bindings: &[Binding],
    dc: &super::ast::DeleteClause,
    txn_id: Option<u64>,
) -> crate::Result<GqlMutationResult> {
    let mut stats = GqlMutationResult::default();
    let mut deleted_nodes: std::collections::HashSet<NodeId> = std::collections::HashSet::new();
    let mut deleted_edges: std::collections::HashSet<crate::EdgeId> =
        std::collections::HashSet::new();

    for b in bindings {
        for var in &dc.vars {
            if let Ok(node) = b.pm.get_node(var) {
                super::mutation_exec::delete_node_row(
                    graph,
                    node.id(),
                    dc.detach,
                    txn_id,
                    &mut deleted_nodes,
                    &mut stats,
                )?;
            } else if let Ok(edge) = b.pm.get_edge(var) {
                super::mutation_exec::delete_edge_row(
                    graph,
                    edge.id,
                    txn_id,
                    &mut deleted_edges,
                    &mut stats,
                )?;
            } else {
                return Err(Error::GqlMutationError(format!(
                    "DELETE target variable '{var}' is not bound in the final \
                     pipeline stage (may have been dropped by an earlier WITH)",
                )));
            }
        }
    }
    Ok(stats)
}

/// Applies a `SetClause` against each final pipeline binding.
///
/// For each binding × each assignment:
/// - Resolves the target variable from the binding's `PatternMatch`
///   (which is the only place live node/edge bindings live after the
///   last pipeline stage). If the variable is not bound, returns an
///   error — scope must be preserved explicitly by WITH stages.
/// - Evaluates the RHS expression via `eval_expr_on_binding`.
/// - Converts the result to a `Property` and writes it via
///   `Graph::update_node` / `Graph::update_edge`. `Null` and `List`
///   values are skipped (no corresponding `Property` variant).
///
/// Counts total property assignments applied.
fn apply_pipeline_set(
    graph: &mut crate::Graph,
    bindings: &[Binding],
    set: &super::ast::SetClause,
    txn_id: Option<u64>,
) -> crate::Result<GqlMutationResult> {
    let mut properties_set: u64 = 0;

    for b in bindings {
        for assignment in &set.assignments {
            // Whole-entity map assignment (`SET n = $map` / `SET n += $map`) in
            // a pipeline terminal is not yet supported — the map-param SET forms
            // are wired in the MATCH and MERGE executors (graph_accessor), not
            // here. Surface a clear error rather than silently dropping.
            let (var, prop, value_expr) = match assignment {
                SetAssignment::Property { var, prop, value } => (var, prop, value),
                SetAssignment::EntityOverwrite { var, .. }
                | SetAssignment::EntityMerge { var, .. } => {
                    return Err(Error::GqlUnsupported(format!(
                        "SET {var} = $map / {var} += $map is not supported in a \
                         WITH-pipeline terminal (use MATCH … SET or MERGE)",
                    )));
                }
            };

            // Evaluate the RHS first using a read-only view. Inside a
            // transaction the RHS must see the txn's snapshot, so evaluate it
            // over a `TxnView`; otherwise over the committed graph.
            let value = if let Some(t) = txn_id {
                let view = super::txn_view::TxnView::new(graph, t);
                eval_expr_on_binding(value_expr, b, &view)
            } else {
                let view: &crate::Graph = graph;
                eval_expr_on_binding(value_expr, b, view)
            };
            let Some(prop_val) = gql_value_to_property(&value) else {
                // Null / List / Map — skip silently (no scalar Property variant).
                continue;
            };

            // Resolve target: prefer node binding, then edge.
            if let Ok(node) = b.pm.get_node(var) {
                let node_id = node.id();
                let mut updated = match txn_id {
                    Some(t) => graph.node_in_txn(t, node_id)?,
                    None => graph.node(node_id)?,
                };
                updated.properties_mut().insert(prop.clone(), prop_val);
                match txn_id {
                    Some(t) => graph.update_node_in_txn(t, node_id, &updated)?,
                    None => graph.update_node(node_id, &updated)?,
                }
                properties_set += 1;
                continue;
            }
            if b.pm.get_edge(var).is_ok() {
                return Err(Error::GqlUnsupported(
                    "SET on edge properties in pipeline mutation is not yet \
                     implemented"
                        .into(),
                ));
            }

            return Err(Error::GqlMutationError(format!(
                "SET target variable '{var}' is not bound in the final pipeline \
                 stage (may have been dropped by an earlier WITH)",
            )));
        }
    }

    Ok(GqlMutationResult {
        properties_set,
        ..GqlMutationResult::default()
    })
}

/// Evaluates an expression against a [`Binding`]. Looks up variables in
/// `vals` first (WITH-introduced scalars shadow entity bindings), then
/// falls back to the `PatternMatch` for entity-typed references.
///
/// Handles pipeline-specific forms (`ListLit`, `Subscript`, and builtin
/// function calls `range` / `size`) directly. All other forms are
/// evaluated recursively via `eval_expr_on_binding` on subexpressions.
///
/// **Exhaustiveness**: every variant of `Expr` has an explicit arm here.
/// The `match` has no wildcard — the Rust compiler enforces coverage,
/// so adding a new `Expr` variant in `ast.rs` forces a compile error
/// until a corresponding arm is added. Verified against `ast.rs` for
/// all 12 variants (`Literal`, `Var`, `PropAccess`, `BinaryOp`, `UnaryOp`,
/// `IsNull`, `Aggregate`, `FunctionCall`, `ShortestPath`, `Subscript`,
/// `ListLit`, `ListPredicate`).
fn eval_expr_on_binding<G: GraphAccess + ?Sized>(
    expr: &Expr,
    b: &Binding,
    graph: &G,
) -> GqlValue {
    match expr {
        Expr::Literal(lit) => compile_literal(lit),
        Expr::Var(name) => {
            if let Some(val) = b.vals.get(name) {
                return val.clone();
            }
            // Fall back to PatternMatch (returns int-encoded node/edge id).
            eval_expr(expr, &b.pm, &PathBindings::new(), graph, &DeadlineAbort::none())
        }
        Expr::BinaryOp { left, op, right } => {
            let lv = eval_expr_on_binding(left, b, graph);
            let rv = eval_expr_on_binding(right, b, graph);
            eval_binary_op(&lv, *op, &rv)
        }
        Expr::UnaryOp { op, expr: inner } => {
            let v = eval_expr_on_binding(inner, b, graph);
            eval_unary_op(*op, &v)
        }
        Expr::IsNull { expr: inner, negated } => {
            let v = eval_expr_on_binding(inner, b, graph);
            let is_null = matches!(v, GqlValue::Null);
            GqlValue::Bool(if *negated { !is_null } else { is_null })
        }
        Expr::Aggregate { .. } => {
            // Inside a binding-level evaluator, aggregates are opaque.
            // They're resolved by `execute_with_stage` at the group level.
            GqlValue::Null
        }
        Expr::ListLit(items) => {
            let vals: Vec<GqlValue> =
                items.iter().map(|e| eval_expr_on_binding(e, b, graph)).collect();
            GqlValue::List(vals)
        }
        Expr::Subscript { list, index } => {
            let list_val = eval_expr_on_binding(list, b, graph);
            let index_val = eval_expr_on_binding(index, b, graph);
            eval_subscript(&list_val, &index_val)
        }
        Expr::FunctionCall { name, args } => {
            eval_builtin_function_call(name, args, b, graph)
        }
        Expr::ListPredicate { kind, var, list, predicate } => {
            eval_list_predicate_on_binding(*kind, var, list, predicate, b, graph)
        }
        // `var.prop` where `var` is a pipeline binding holding a first-class
        // entity (Node/Relationship) or a Map — e.g. `rel` iterating over
        // `relationships(p)`, or `n` from a prior WITH. Read the property from
        // that value directly; only fall through to the PatternMatch when the
        // var isn't a binding-level entity. Without this, ReBAC predicates like
        // `ALL(rel IN relationships(p) WHERE rel.expired = false)` saw Null.
        Expr::PropAccess { var, prop } => match b.vals.get(var) {
            Some(GqlValue::Node(n)) => n.props.get(prop).cloned().unwrap_or(GqlValue::Null),
            Some(GqlValue::Relationship(r)) => {
                r.props.get(prop).cloned().unwrap_or(GqlValue::Null)
            }
            Some(GqlValue::Map(m)) => m.get(prop).cloned().unwrap_or(GqlValue::Null),
            // A non-entity binding (scalar) has no properties.
            Some(_) => GqlValue::Null,
            // Not a binding var — resolve against the MATCH bindings.
            None => eval_expr(expr, &b.pm, &PathBindings::new(), graph, &DeadlineAbort::none()),
        },
        // `shortestPath` resolves against the PatternMatch — it operates on
        // entity bindings established by a prior MATCH stage.
        Expr::ShortestPath { .. } => {
            // Pipeline-terminal `shortestPath` is not deadline-instrumented
            // (no deadline is threaded through the pipeline binding evaluator);
            // a documented limitation. See Task 6 design notes.
            eval_expr(expr, &b.pm, &PathBindings::new(), graph, &DeadlineAbort::none())
        }
        // Defensive: param substitution must have run before compile.
        // See `eval_expr` for the matching debug_assert.
        Expr::ParamRef(_) => {
            debug_assert!(
                false,
                "unsubstituted ParamRef reached eval_expr_on_binding",
            );
            GqlValue::Null
        }
    }
}

/// Evaluates a list predicate in pipeline-binding context.
///
/// Mirror of [`eval_list_predicate`] for the pipeline `Binding` evaluator: the
/// iteration `var` is bound in a clone of the binding's `vals` (shadowing any
/// same-named pipeline scalar for the duration of the predicate), preserving
/// the rest of the pipeline scope. A non-list source yields `Null`.
fn eval_list_predicate_on_binding<G: GraphAccess + ?Sized>(
    kind: super::ast::ListPredKind,
    var: &str,
    list: &Expr,
    predicate: &Expr,
    b: &Binding,
    graph: &G,
) -> GqlValue {
    let GqlValue::List(items) = eval_expr_on_binding(list, b, graph) else {
        return GqlValue::Null;
    };
    let mut local = Binding { pm: b.pm.clone(), vals: b.vals.clone() };
    apply_list_quantifier(kind, &items, |item| {
        local.vals.insert(var.to_owned(), item.clone());
        match eval_expr_on_binding(predicate, &local, graph) {
            GqlValue::Bool(value) => Some(value),
            _ => None,
        }
    })
}

/// Evaluates `list[index]` with Cypher semantics: `Null` on non-list value,
/// non-int index, or out-of-bounds access (including negative indices for
/// now — negative-index Python-style is out of scope).
fn eval_subscript(list: &GqlValue, index: &GqlValue) -> GqlValue {
    let GqlValue::List(items) = list else {
        return GqlValue::Null;
    };
    let GqlValue::Int(i) = index else {
        return GqlValue::Null;
    };
    if *i < 0 {
        return GqlValue::Null;
    }
    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
    let idx = *i as usize;
    items.get(idx).cloned().unwrap_or(GqlValue::Null)
}

/// Evaluates pipeline-aware function calls: `range`, `size`, `id`, `type`,
/// `labels`, `shortestpath`. Arguments are evaluated against the current
/// binding so that nested forms like `size(collect(a))` work.
fn eval_builtin_function_call<G: GraphAccess + ?Sized>(
    name: &str,
    args: &[Expr],
    b: &Binding,
    graph: &G,
) -> GqlValue {
    match name {
        "range" => {
            if args.len() != 2 {
                return GqlValue::Null;
            }
            // Args may reference pipeline bindings (e.g. `range(1, n)` where
            // `n` came from a prior UNWIND/WITH), so evaluate against `b`.
            let start = eval_expr_on_binding(&args[0], b, graph);
            let end = eval_expr_on_binding(&args[1], b, graph);
            compute_range(&start, &end)
        }
        "size" => {
            let Some(arg) = args.first() else {
                return GqlValue::Null;
            };
            let val = eval_expr_on_binding(arg, b, graph);
            compute_size(&val)
        }
        "tolower" => {
            let Some(arg) = args.first() else {
                return GqlValue::Null;
            };
            let val = eval_expr_on_binding(arg, b, graph);
            compute_to_lower(&val)
        }
        "toupper" => {
            let Some(arg) = args.first() else {
                return GqlValue::Null;
            };
            let val = eval_expr_on_binding(arg, b, graph);
            compute_to_upper(&val)
        }
        "coalesce" => {
            let evaluated: Vec<GqlValue> =
                args.iter().map(|a| eval_expr_on_binding(a, b, graph)).collect();
            compute_coalesce(&evaluated)
        }
        // Entity-bound builtins: delegate to the PatternMatch-based evaluator
        // which requires an `Expr::Var` argument pointing to a bound
        // node/edge. Works only when that variable is still in `pm`.
        //
        // Path functions (`nodes`/`relationships`/`length`) are mirrored here
        // for dispatcher parity (the issue-#15 single-dispatcher class of bug),
        // sharing `compute_path_function` via `eval_function_call`. A pipeline
        // `Binding` carries no materialised path — `MATCH p = (…)` does not flow
        // through the WITH pipeline — so they resolve to `Null` here (empty
        // `PathBindings`), which is the honest result for "no path bound in this
        // context". If path bindings ever cross into the pipeline, thread the
        // real `PathBindings` through `Binding` and pass it here.
        "id" | "type" | "labels" | "properties" | "shortestpath" | "nodes"
        | "relationships" | "length" => {
            // Pipeline-context `shortestPath` is not deadline-instrumented
            // (no deadline reaches the pipeline binding evaluator).
            eval_function_call(name, args, &b.pm, &PathBindings::new(), graph, &DeadlineAbort::none())
        }
        _ => GqlValue::Null,
    }
}

/// Soft cap on `range()` length, guarding against exhausting memory on
/// multi-billion-element ranges. Callers needing a longer range can UNWIND
/// chunks or use an explicit loop.
const RANGE_MAX_ELEMENTS: i64 = 1_000_000;

/// Computes `range(start, end)` from already-evaluated argument values.
///
/// Inclusive per Cypher semantics: `range(0, 2) == [0, 1, 2]`. Returns an
/// empty list when `start > end`, and `Null` when either argument is not an
/// integer or the length would exceed [`RANGE_MAX_ELEMENTS`]. `Null` (rather
/// than an error) is returned because the `eval_*` family yields `GqlValue`,
/// not `Result`.
///
/// Shared between the `PatternMatch`-based [`eval_function_call`] and the
/// pipeline-binding [`eval_builtin_function_call`] so both paths agree.
fn compute_range(start: &GqlValue, end: &GqlValue) -> GqlValue {
    let (GqlValue::Int(s), GqlValue::Int(e)) = (start, end) else {
        return GqlValue::Null;
    };
    if s > e {
        return GqlValue::List(Vec::new());
    }
    let len = e.saturating_sub(*s).saturating_add(1);
    if len > RANGE_MAX_ELEMENTS {
        return GqlValue::Null;
    }
    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
    let capacity = len as usize;
    let mut out = Vec::with_capacity(capacity);
    let mut cur = *s;
    while cur <= *e {
        out.push(GqlValue::Int(cur));
        cur = match cur.checked_add(1) {
            Some(n) => n,
            None => break, // defensive: can't reach here given cap, but safe
        };
    }
    GqlValue::List(out)
}

/// Converts a node's or edge's [`Properties`] into a bare
/// `HashMap<String, GqlValue>`. Single source for both `compute_properties`
/// (which wraps it in a `GqlValue::Map`) and the entity-materialisation helpers
/// ([`gql_node_from_entity`] / [`gql_relationship_from_entity`]), so the
/// property-conversion logic lives in exactly one place.
fn properties_to_gql_map(props: &crate::Properties) -> HashMap<String, GqlValue> {
    props
        .iter()
        .map(|(k, v)| (k.clone(), gql_value_from_property(v)))
        .collect()
}

/// Builds the `properties(entity)` map from a node's or edge's [`Properties`].
///
/// Each stored [`Property`] is converted to its `GqlValue` via
/// [`gql_value_from_property`], producing a `GqlValue::Map`. An entity with no
/// properties yields an empty map (not `Null`). Shared by the node and edge
/// arms of [`eval_function_call`] so both paths agree.
fn compute_properties(props: &crate::Properties) -> GqlValue {
    GqlValue::Map(properties_to_gql_map(props))
}

/// Projects a graph [`Node`] as a first-class [`GqlValue::Node`]. Single source
/// for the bare-`RETURN n` projection and path materialisation (Fase B).
#[allow(clippy::cast_possible_wrap)]
pub fn gql_node_from_entity(node: &crate::Node) -> GqlNode {
    // SAFETY(cast): ids are assigned sequentially from 1; reaching i64::MAX
    // would exceed the addressable u64 slot space (see eval_expr Var arm).
    GqlNode {
        id: node.id().as_u64() as i64,
        labels: vec![node.label().to_owned()],
        props: properties_to_gql_map(node.properties()),
    }
}

/// Projects a graph [`Edge`] as a first-class [`GqlValue::Relationship`].
/// Single source for the bare-`RETURN r` projection and path materialisation.
#[allow(clippy::cast_possible_wrap)]
pub fn gql_relationship_from_entity(edge: &crate::Edge) -> GqlRelationship {
    GqlRelationship {
        id: edge.id().as_u64() as i64,
        start_id: edge.source().as_u64() as i64,
        end_id: edge.target().as_u64() as i64,
        rel_type: edge.label().to_owned(),
        props: properties_to_gql_map(edge.properties()),
    }
}

/// Computes `coalesce(args…)` from already-evaluated argument values.
///
/// Returns the first non-`Null` value, or `Null` if every argument is `Null`
/// (or there are no arguments). Per Cypher semantics, arguments are evaluated
/// eagerly; the common idiom is `coalesce(n.maybeMissing, fallback)`. Shared
/// between [`eval_function_call`] and [`eval_builtin_function_call`].
fn compute_coalesce(args: &[GqlValue]) -> GqlValue {
    args.iter()
        .find(|v| !matches!(v, GqlValue::Null))
        .cloned()
        .unwrap_or(GqlValue::Null)
}

/// Computes `toLower(value)` from an already-evaluated argument value.
///
/// Returns the lowercased string for a `GqlValue::Str`, using the full Unicode
/// case mapping (`str::to_lowercase`); `Null` for any other value. Shared
/// between [`eval_function_call`] and [`eval_builtin_function_call`] so both
/// evaluation contexts agree.
fn compute_to_lower(val: &GqlValue) -> GqlValue {
    match val {
        GqlValue::Str(s) => GqlValue::Str(s.to_lowercase()),
        _ => GqlValue::Null,
    }
}

/// Computes `toUpper(value)` from an already-evaluated argument value.
///
/// Returns the uppercased string for a `GqlValue::Str`, using the full Unicode
/// case mapping (`str::to_uppercase`); `Null` for any other value. Shared
/// between [`eval_function_call`] and [`eval_builtin_function_call`].
fn compute_to_upper(val: &GqlValue) -> GqlValue {
    match val {
        GqlValue::Str(s) => GqlValue::Str(s.to_uppercase()),
        _ => GqlValue::Null,
    }
}

/// Computes `size(value)` from an already-evaluated argument value.
///
/// Returns the element count of a list or the character count of a string;
/// `Null` for any other value. Shared between [`eval_function_call`] and
/// [`eval_builtin_function_call`].
fn compute_size(val: &GqlValue) -> GqlValue {
    match val {
        #[allow(clippy::cast_possible_wrap)]
        GqlValue::List(items) => GqlValue::Int(items.len() as i64),
        #[allow(clippy::cast_possible_wrap)]
        GqlValue::Str(s) => {
            // ASCII fast-path: byte length equals character count and
            // avoids iterating the whole string.
            let len = if s.is_ascii() {
                s.len()
            } else {
                s.chars().count()
            };
            GqlValue::Int(len as i64)
        }
        _ => GqlValue::Null,
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gql::ast::UnaryOp;

    /// An already-expired `Instant` for deterministic deadline tests — the
    /// trick from the Task 6 design that sidesteps wall-clock fragility.
    fn expired_deadline() -> std::time::Instant {
        std::time::Instant::now()
            .checked_sub(std::time::Duration::from_secs(1))
            .expect("clock is well past process start")
    }

    // ── 3e C1.1: toLower / toUpper ───────────────────────────────────────────

    #[test]
    fn compute_to_lower_lowercases_string() {
        let v = GqlValue::Str("Hello WORLD".to_owned());
        assert_eq!(compute_to_lower(&v), GqlValue::Str("hello world".to_owned()));
    }

    #[test]
    fn compute_to_upper_uppercases_string() {
        let v = GqlValue::Str("Hello world".to_owned());
        assert_eq!(compute_to_upper(&v), GqlValue::Str("HELLO WORLD".to_owned()));
    }

    #[test]
    fn compute_to_lower_non_string_returns_null() {
        assert_eq!(compute_to_lower(&GqlValue::Int(7)), GqlValue::Null, "int -> Null");
        assert_eq!(compute_to_lower(&GqlValue::Null), GqlValue::Null, "Null -> Null");
    }

    #[test]
    fn compute_to_upper_non_string_returns_null() {
        assert_eq!(compute_to_upper(&GqlValue::Bool(true)), GqlValue::Null);
    }

    #[test]
    fn compute_to_lower_unicode() {
        // Non-ASCII must lowercase via the full Unicode mapping, not byte ops.
        let v = GqlValue::Str("ÀÉÎ".to_owned());
        assert_eq!(compute_to_lower(&v), GqlValue::Str("àéî".to_owned()));
    }

    /// `toLower` resolves through the real `eval_expr` → `eval_function_call`
    /// path (`PatternMatch` context), the same path a `RETURN`/`WHERE` expression
    /// takes. Exercising the dispatcher (not just the helper) proves the arm is
    /// wired into the evaluator.
    #[test]
    fn to_lower_via_function_call_pm() {
        use crate::Graph;
        use crate::gql::ast::{Expr, Literal};
        let g = Graph::new();
        let pm = PatternMatch::empty();
        let expr = Expr::FunctionCall {
            name: "tolower".to_owned(),
            args: vec![Expr::Literal(Literal::Str("ABC".to_owned()))],
        };
        assert_eq!(
            eval_expr(&expr, &pm, &PathBindings::new(), &g, &DeadlineAbort::none()),
            GqlValue::Str("abc".to_owned())
        );
    }

    /// `toUpper` resolves through the same dispatcher arm.
    #[test]
    fn to_upper_via_function_call_pm() {
        use crate::Graph;
        use crate::gql::ast::{Expr, Literal};
        let g = Graph::new();
        let pm = PatternMatch::empty();
        let expr = Expr::FunctionCall {
            name: "toupper".to_owned(),
            args: vec![Expr::Literal(Literal::Str("abc".to_owned()))],
        };
        assert_eq!(
            eval_expr(&expr, &pm, &PathBindings::new(), &g, &DeadlineAbort::none()),
            GqlValue::Str("ABC".to_owned())
        );
    }

    // ── 3e C1.2: coalesce ────────────────────────────────────────────────────

    #[test]
    fn compute_coalesce_returns_first_non_null() {
        let args = vec![GqlValue::Null, GqlValue::Int(5), GqlValue::Int(9)];
        assert_eq!(compute_coalesce(&args), GqlValue::Int(5));
    }

    #[test]
    fn compute_coalesce_first_arg_non_null_wins() {
        let args = vec![GqlValue::Str("a".to_owned()), GqlValue::Int(1)];
        assert_eq!(compute_coalesce(&args), GqlValue::Str("a".to_owned()));
    }

    #[test]
    fn compute_coalesce_all_null_returns_null() {
        let args = vec![GqlValue::Null, GqlValue::Null];
        assert_eq!(compute_coalesce(&args), GqlValue::Null);
    }

    #[test]
    fn compute_coalesce_empty_args_returns_null() {
        assert_eq!(compute_coalesce(&[]), GqlValue::Null);
    }

    #[test]
    fn compute_coalesce_single_non_null() {
        assert_eq!(compute_coalesce(&[GqlValue::Int(3)]), GqlValue::Int(3));
    }

    /// `coalesce` resolves through the dispatcher, and the common .NET idiom
    /// `coalesce(n.missingProp, 'default')` — where the first argument is an
    /// absent property that evaluates to `Null` — returns the fallback.
    #[test]
    fn coalesce_via_function_call_missing_prop_falls_back() {
        use crate::Graph;
        use crate::gql::ast::{Expr, Literal};
        let g = Graph::new();
        let pm = PatternMatch::empty();
        // First arg evaluates to Null (no binding named `n`), second is the
        // literal fallback.
        let expr = Expr::FunctionCall {
            name: "coalesce".to_owned(),
            args: vec![
                Expr::PropAccess { var: "n".to_owned(), prop: "missing".to_owned() },
                Expr::Literal(Literal::Str("default".to_owned())),
            ],
        };
        assert_eq!(
            eval_expr(&expr, &pm, &PathBindings::new(), &g, &DeadlineAbort::none()),
            GqlValue::Str("default".to_owned())
        );
    }

    // ── 3e C2.1: properties(node) / properties(edge) ─────────────────────────

    #[test]
    fn properties_of_node_returns_map_of_all_props() {
        use crate::{Graph, props};
        let mut g = Graph::new();
        g.add_node("Person", props! { "name" => "Alice", "age" => 30i64 }).unwrap();

        let query = crate::gql::parse("MATCH (n:Person) RETURN properties(n) AS p").unwrap();
        let result = execute(&g, &query, 0).unwrap();
        assert_eq!(result.len(), 1);
        match result[0].get("p") {
            Some(GqlValue::Map(m)) => {
                assert_eq!(m.get("name"), Some(&GqlValue::Str("Alice".to_owned())));
                assert_eq!(m.get("age"), Some(&GqlValue::Int(30)));
                assert_eq!(m.len(), 2, "exactly the two stored props");
            }
            other => panic!("expected Map, got {other:?}"),
        }
    }

    #[test]
    fn properties_of_node_with_no_props_returns_empty_map() {
        use crate::Graph;
        let mut g = Graph::new();
        g.add_node("Empty", crate::Properties::new()).unwrap();

        let query = crate::gql::parse("MATCH (n:Empty) RETURN properties(n) AS p").unwrap();
        let result = execute(&g, &query, 0).unwrap();
        assert_eq!(result[0].get("p"), Some(&GqlValue::Map(std::collections::HashMap::new())));
    }

    #[test]
    fn properties_of_edge_returns_map() {
        use crate::{Graph, props};
        let mut g = Graph::new();
        let a = g.add_node("N", crate::Properties::new()).unwrap();
        let b = g.add_node("N", crate::Properties::new()).unwrap();
        g.add_edge("LINKS", a, b, props! { "weight" => 5i64 }).unwrap();

        let query =
            crate::gql::parse("MATCH (a:N)-[r:LINKS]->(b:N) RETURN properties(r) AS p").unwrap();
        let result = execute(&g, &query, 0).unwrap();
        match result[0].get("p") {
            Some(GqlValue::Map(m)) => {
                assert_eq!(m.get("weight"), Some(&GqlValue::Int(5)));
            }
            other => panic!("expected Map, got {other:?}"),
        }
    }

    #[test]
    fn properties_of_unbound_var_returns_null() {
        use crate::Graph;
        use crate::gql::ast::Expr;
        let g = Graph::new();
        let pm = PatternMatch::empty();
        let expr = Expr::FunctionCall {
            name: "properties".to_owned(),
            args: vec![Expr::Var("ghost".to_owned())],
        };
        assert_eq!(eval_expr(&expr, &pm, &PathBindings::new(), &g, &DeadlineAbort::none()), GqlValue::Null);
    }

    // ── Fase B C3: RETURN n -> struct Node / RETURN r -> Relationship ────────

    #[test]
    fn return_node_yields_node_value_not_int() {
        use crate::{Graph, props};
        let mut g = Graph::new();
        g.add_node("Person", props! { "name" => "Alice", "age" => 30i64 }).unwrap();

        let query = crate::gql::parse("MATCH (n:Person) RETURN n").unwrap();
        let result = execute(&g, &query, 0).unwrap();
        assert_eq!(result.len(), 1);
        match result[0].get("n") {
            Some(GqlValue::Node(node)) => {
                assert_eq!(node.labels, vec!["Person".to_owned()]);
                assert_eq!(node.props.get("name"), Some(&GqlValue::Str("Alice".to_owned())));
                assert_eq!(node.props.get("age"), Some(&GqlValue::Int(30)));
            }
            other => panic!("expected GqlValue::Node, got {other:?}"),
        }
    }

    #[test]
    fn return_relationship_yields_relationship_value() {
        use crate::{Graph, props};
        let mut g = Graph::new();
        let a = g.add_node("N", crate::Properties::new()).unwrap();
        let b = g.add_node("N", crate::Properties::new()).unwrap();
        g.add_edge("LINKS", a, b, props! { "weight" => 5i64 }).unwrap();

        let query = crate::gql::parse("MATCH (a:N)-[r:LINKS]->(b:N) RETURN r").unwrap();
        let result = execute(&g, &query, 0).unwrap();
        match result[0].get("r") {
            Some(GqlValue::Relationship(rel)) => {
                assert_eq!(rel.rel_type, "LINKS");
                assert_eq!(rel.props.get("weight"), Some(&GqlValue::Int(5)));
                assert_eq!(u64::try_from(rel.start_id).unwrap(), a.as_u64());
                assert_eq!(u64::try_from(rel.end_id).unwrap(), b.as_u64());
            }
            other => panic!("expected GqlValue::Relationship, got {other:?}"),
        }
    }

    #[test]
    fn count_of_bare_node_still_counts_rows() {
        // Regression: COUNT(n) over a bare node must keep counting non-null
        // bindings now that `n` projects as a Node (a Node is never Null).
        use crate::Graph;
        let mut g = Graph::new();
        g.add_node("P", crate::Properties::new()).unwrap();
        g.add_node("P", crate::Properties::new()).unwrap();

        let query = crate::gql::parse("MATCH (n:P) RETURN count(n) AS c").unwrap();
        let result = execute(&g, &query, 0).unwrap();
        assert_eq!(result[0].get("c"), Some(&GqlValue::Int(2)));
    }

    #[test]
    fn count_star_counts_committed_visible_nodes() {
        use crate::Graph;
        let mut g = Graph::new();
        g.enable_mvcc();
        let t = g.begin_txn().unwrap();
        g.add_node_in_txn(t, "N", crate::Properties::new()).unwrap();
        g.add_node_in_txn(t, "N", crate::Properties::new()).unwrap();
        g.commit_txn(t).unwrap();

        // The zero-hop COUNT pushdown filters superset ids by snapshot
        // visibility, so it counts exactly the two committed nodes.
        let query = crate::gql::parse("MATCH (n) RETURN count(n) AS c").unwrap();
        let result = execute(&g, &query, 0).unwrap();
        assert_eq!(result[0].get("c"), Some(&GqlValue::Int(2)));
    }

    #[test]
    fn properties_function_still_returns_map_after_node_migration() {
        // `properties(n)` keeps its Map shape; only bare `RETURN n` changed.
        use crate::{Graph, props};
        let mut g = Graph::new();
        g.add_node("Person", props! { "name" => "Bob" }).unwrap();

        let query = crate::gql::parse("MATCH (n:Person) RETURN properties(n) AS p").unwrap();
        let result = execute(&g, &query, 0).unwrap();
        assert!(matches!(result[0].get("p"), Some(GqlValue::Map(_))));
    }

    // ── 3f C4: list-predicate evaluation (ALL/ANY/NONE/SINGLE) ───────────────

    /// Builds a single-node graph and returns whether a `WHERE <pred>` over it
    /// keeps the node (`true`) or filters it out (`false`).
    fn where_keeps_node(predicate: &str) -> bool {
        use crate::Graph;
        let mut g = Graph::new();
        g.add_node("N", crate::Properties::new()).unwrap();
        let query = crate::gql::parse(&format!("MATCH (n:N) WHERE {predicate} RETURN n")).unwrap();
        !execute(&g, &query, 0).unwrap().is_empty()
    }

    #[test]
    fn all_pred_every_element_matches_is_true() {
        assert!(where_keeps_node("ALL(x IN [1, 2, 3] WHERE x > 0)"));
    }

    #[test]
    fn all_pred_one_element_fails_is_false() {
        assert!(!where_keeps_node("ALL(x IN [1, 2, 3] WHERE x > 1)"));
    }

    #[test]
    fn all_pred_empty_list_is_vacuously_true() {
        assert!(where_keeps_node("ALL(x IN [] WHERE x > 0)"));
    }

    #[test]
    fn any_pred_one_element_matches_is_true() {
        assert!(where_keeps_node("ANY(x IN [1, 2, 3] WHERE x = 2)"));
    }

    #[test]
    fn any_pred_no_element_matches_is_false() {
        assert!(!where_keeps_node("ANY(x IN [1, 2, 3] WHERE x > 5)"));
    }

    #[test]
    fn any_pred_empty_list_is_false() {
        assert!(!where_keeps_node("ANY(x IN [] WHERE x > 0)"));
    }

    #[test]
    fn none_pred_no_element_matches_is_true() {
        assert!(where_keeps_node("NONE(x IN [1, 2, 3] WHERE x > 5)"));
    }

    #[test]
    fn none_pred_one_element_matches_is_false() {
        assert!(!where_keeps_node("NONE(x IN [1, 2, 3] WHERE x = 2)"));
    }

    #[test]
    fn none_pred_empty_list_is_vacuously_true() {
        assert!(where_keeps_node("NONE(x IN [] WHERE x > 0)"));
    }

    #[test]
    fn single_pred_exactly_one_matches_is_true() {
        assert!(where_keeps_node("SINGLE(x IN [1, 2, 3] WHERE x = 2)"));
    }

    #[test]
    fn single_pred_two_match_is_false() {
        assert!(!where_keeps_node("SINGLE(x IN [1, 2, 3] WHERE x > 1)"));
    }

    #[test]
    fn single_pred_none_match_is_false() {
        assert!(!where_keeps_node("SINGLE(x IN [1, 2, 3] WHERE x > 5)"));
    }

    #[test]
    fn single_pred_empty_list_is_false() {
        assert!(!where_keeps_node("SINGLE(x IN [] WHERE x > 0)"));
    }

    /// The predicate can reference an outer binding (the node's property)
    /// alongside the iteration variable — the ReBAC/GraphRAG-style usage.
    #[test]
    fn list_pred_predicate_references_outer_and_iteration_var() {
        use crate::{Graph, props};
        let mut g = Graph::new();
        g.add_node("N", props! { "threshold" => 2i64 }).unwrap();
        // ANY(x IN [1,2,3] WHERE x > n.threshold) → 3 > 2 → true, node kept.
        let q = crate::gql::parse(
            "MATCH (n:N) WHERE ANY(x IN [1, 2, 3] WHERE x > n.threshold) RETURN n",
        )
        .unwrap();
        assert_eq!(execute(&g, &q, 0).unwrap().len(), 1);
    }

    #[test]
    fn list_pred_non_list_source_is_null_filtered_out() {
        // A non-list source (here an integer) yields Null → WHERE drops the row.
        assert!(!where_keeps_node("ALL(x IN 5 WHERE x > 0)"));
    }

    // ── C5: Overflow in unary negation ──────────────────────────────────────

    #[test]
    fn unary_neg_i64_min_returns_null() {
        let v = GqlValue::Int(i64::MIN);
        let result = eval_unary_op(UnaryOp::Neg, &v);
        assert_eq!(result, GqlValue::Null);
    }

    #[test]
    fn unary_neg_normal_int() {
        let v = GqlValue::Int(42);
        assert_eq!(eval_unary_op(UnaryOp::Neg, &v), GqlValue::Int(-42));
    }

    #[test]
    fn unary_neg_zero() {
        let v = GqlValue::Int(0);
        assert_eq!(eval_unary_op(UnaryOp::Neg, &v), GqlValue::Int(0));
    }

    #[test]
    fn unary_neg_i64_max() {
        let v = GqlValue::Int(i64::MAX);
        assert_eq!(eval_unary_op(UnaryOp::Neg, &v), GqlValue::Int(-i64::MAX));
    }

    #[test]
    fn unary_neg_float() {
        let v = GqlValue::Float(1.5);
        assert_eq!(eval_unary_op(UnaryOp::Neg, &v), GqlValue::Float(-1.5));
    }

    #[test]
    fn unary_neg_null_returns_null() {
        assert_eq!(eval_unary_op(UnaryOp::Neg, &GqlValue::Null), GqlValue::Null);
    }

    // ── C3: aggregate_sum correctness ───────────────────────────────────────

    #[test]
    fn aggregate_sum_all_null_returns_null() {
        let vals = vec![GqlValue::Null, GqlValue::Null];
        assert_eq!(aggregate_sum(&vals), GqlValue::Null);
    }

    #[test]
    fn aggregate_sum_empty_returns_null() {
        assert_eq!(aggregate_sum(&[]), GqlValue::Null);
    }

    #[test]
    fn aggregate_sum_mixed_null_ignores_nulls() {
        let vals = vec![GqlValue::Int(10), GqlValue::Null, GqlValue::Int(20)];
        assert_eq!(aggregate_sum(&vals), GqlValue::Int(30));
    }

    // ── C10: aggregate_avg single-pass fold ─────────────────────────────────

    #[test]
    fn aggregate_avg_ints() {
        let vals = vec![GqlValue::Int(10), GqlValue::Int(20)];
        assert_eq!(aggregate_avg(&vals), GqlValue::Float(15.0));
    }

    #[test]
    fn aggregate_avg_mixed_null() {
        let vals = vec![GqlValue::Int(10), GqlValue::Null, GqlValue::Int(30)];
        assert_eq!(aggregate_avg(&vals), GqlValue::Float(20.0));
    }

    #[test]
    fn aggregate_avg_all_null_returns_null() {
        let vals = vec![GqlValue::Null, GqlValue::Null];
        assert_eq!(aggregate_avg(&vals), GqlValue::Null);
    }

    #[test]
    fn aggregate_avg_empty_returns_null() {
        assert_eq!(aggregate_avg(&[]), GqlValue::Null);
    }

    #[test]
    fn match_label_and_property_uses_index() {
        use crate::{Graph, props};
        let mut g = Graph::new();

        // Add 1000 nodes: 999 with name "Other<i>", 1 with name "Target"
        for i in 0..999_u64 {
            // allow: test fixture
            #[allow(clippy::cast_possible_wrap)]
            let score = i as i64;
            g.add_node("Person", props! { "name" => format!("Other{i}"), "score" => score })
                .unwrap();
        }
        g.add_node("Person", props! { "name" => "Target", "score" => 42i64 }).unwrap();

        // MATCH (p:Person {name: "Target"}) RETURN p.score
        // The property index narrows candidates to the single node, then
        // node_matches_pattern confirms it; only one result row is produced.
        let query =
            crate::gql::parse("MATCH (p:Person {name: 'Target'}) RETURN p.score").unwrap();
        let result = execute(&g, &query, 0).unwrap();
        // GqlResult = Vec<GqlRow>
        assert_eq!(result.len(), 1, "expected exactly one result row");
        let row = &result[0];
        let score_val = row.get("p.score").expect("p.score binding must exist");
        assert!(
            matches!(score_val, GqlValue::Int(42)),
            "expected score 42, got {score_val:?}"
        );
    }

    // ── Cycle 7: execute_const_return ────────────────────────────────────────
    //
    // Exercises the empty-binding fast path for `RETURN <expr-list>` root
    // statements. No graph state is required — passing `Graph::new()` is
    // sufficient.

    fn const_return_query(input: &str) -> ConstReturnQuery {
        match crate::gql::parse_statement(input).unwrap() {
            crate::gql::GqlStatement::ConstReturn(q) => q,
            other => panic!("expected ConstReturn, got {other:?}"),
        }
    }

    #[test]
    fn const_return_emits_single_row_with_literal_int() {
        use crate::Graph;
        let g = Graph::new();
        let q = const_return_query("RETURN 1");
        let rows = execute_const_return(&g, &q, 0, None).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get("1"), Some(&GqlValue::Int(1)));
    }

    #[test]
    fn const_return_emits_multiple_fields_with_alias() {
        use crate::Graph;
        let g = Graph::new();
        let q = const_return_query("RETURN 1 AS one, 'hello' AS greeting");
        let rows = execute_const_return(&g, &q, 0, None).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get("one"), Some(&GqlValue::Int(1)));
        assert_eq!(rows[0].get("greeting"), Some(&GqlValue::Str("hello".into())));
    }

    #[test]
    fn const_return_arithmetic_evaluates() {
        use crate::Graph;
        let g = Graph::new();
        let q = const_return_query("RETURN 1 + 2 * 3");
        let rows = execute_const_return(&g, &q, 0, None).unwrap();
        assert_eq!(rows.len(), 1);
        // Precedence preserved: 1 + (2*3) = 7. Column name is the surface
        // form of the expression when no alias is supplied.
        let v = rows[0].values().next().unwrap();
        assert_eq!(*v, GqlValue::Int(7));
    }

    #[test]
    fn const_return_skip_one_yields_zero_rows() {
        use crate::Graph;
        let g = Graph::new();
        let q = const_return_query("RETURN 1 SKIP 1");
        let rows = execute_const_return(&g, &q, 0, None).unwrap();
        assert!(rows.is_empty());
    }

    #[test]
    fn const_return_limit_zero_yields_zero_rows() {
        use crate::Graph;
        let g = Graph::new();
        let q = const_return_query("RETURN 1 LIMIT 0");
        let rows = execute_const_return(&g, &q, 0, None).unwrap();
        assert!(rows.is_empty());
    }

    #[test]
    fn const_return_limit_one_yields_one_row() {
        use crate::Graph;
        let g = Graph::new();
        let q = const_return_query("RETURN 1 LIMIT 1");
        let rows = execute_const_return(&g, &q, 0, None).unwrap();
        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn const_return_unsubstituted_param_returns_compile_error() {
        // Defensive: the compiler must NOT see Expr::ParamRef. If it does
        // (programming error in the caller — substitution skipped), the
        // executor returns a structured error rather than panicking.
        use crate::Graph;
        let g = Graph::new();
        let mut q = const_return_query("RETURN 1");
        // Manually inject a ParamRef into the projection — simulates a
        // bug where the handler bypassed param_substitution::apply.
        q.items[0].expr = Expr::ParamRef(crate::gql::ast::ParamRef::Named("x".into()));
        let err = execute_const_return(&g, &q, 0, None).unwrap_err();
        assert!(
            err.to_string().contains("unsubstituted parameter"),
            "got: {err}"
        );
    }

    // ── Task 4 C2: Cap A — match-count guard in execute() ───────────────────

    #[test]
    fn execute_cap_a_aborts_when_match_count_exceeds_max_rows() {
        use crate::{Graph, props};
        let mut g = Graph::new();
        // 3 A-nodes × 3 B-nodes = 9 cartesian matches.
        for i in 0_i64..3 {
            g.add_node("A", props! { "i" => i }).unwrap();
            g.add_node("B", props! { "i" => i }).unwrap();
        }
        let q = crate::gql::parse("MATCH (a:A), (b:B) RETURN a, b").unwrap();
        // cap = 5 < 9 cartesian matches → Cap A fires before projection.
        let err = execute(&g, &q, 5).expect_err("must abort over cap");
        let msg = err.to_string();
        assert!(
            msg.contains(RESULT_CAP_MSG_PREFIX),
            "error must carry the result-cap marker, got: {msg}"
        );
        assert!(msg.contains("matched"), "Cap A message mentions matched rows: {msg}");
    }

    #[test]
    fn execute_cap_disabled_with_zero_allows_large_match() {
        use crate::{Graph, props};
        let mut g = Graph::new();
        for i in 0_i64..3 {
            g.add_node("A", props! { "i" => i }).unwrap();
            g.add_node("B", props! { "i" => i }).unwrap();
        }
        let q = crate::gql::parse("MATCH (a:A), (b:B) RETURN a, b").unwrap();
        let rows = execute(&g, &q, 0).expect("cap=0 disables guard");
        assert_eq!(rows.len(), 9, "all 9 cartesian rows returned when cap disabled");
    }

    #[test]
    fn execute_under_cap_passes_unchanged() {
        use crate::{Graph, props};
        let mut g = Graph::new();
        for i in 0_i64..3 {
            g.add_node("A", props! { "i" => i }).unwrap();
        }
        let q = crate::gql::parse("MATCH (a:A) RETURN a").unwrap();
        let rows = execute(&g, &q, 100).expect("under cap");
        assert_eq!(rows.len(), 3);
    }

    // ── Task 4 C3: max_rows param on execute_pipeline + execute_const_return ──

    #[test]
    fn execute_pipeline_accepts_max_rows_param() {
        use crate::{Graph, props};
        let mut g = Graph::new();
        g.add_node("A", props! { "i" => 1_i64 }).unwrap();
        let stmt = crate::gql::parse_statement("MATCH (a:A) WITH a RETURN a").unwrap();
        let crate::gql::GqlStatement::Pipeline(ref pq) = stmt else {
            panic!("expected Pipeline");
        };
        // cap=0 disabled; proves the new 3-arg signature compiles and runs.
        let rows = execute_pipeline(&g, pq, 0).expect("run");
        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn execute_const_return_accepts_max_rows_param() {
        use crate::Graph;
        let g = Graph::new();
        let q = const_return_query("RETURN 1 AS x");
        let rows = execute_const_return(&g, &q, 0, None).expect("run");
        assert_eq!(rows.len(), 1);
    }

    // ── Task 6 C1: check_deadline helper + TIMEOUT_MSG_PREFIX ──

    #[test]
    fn check_deadline_none_never_aborts() {
        // Disabled path (deadline == None): no abort regardless of counter,
        // including the counter == 0 slot where the clock would be read.
        for counter in [0_u64, 1, 1023, 1024, u64::MAX] {
            check_deadline(None, counter).expect("None deadline never aborts");
        }
    }

    #[test]
    fn check_deadline_expired_aborts_on_check_slot() {
        // Already-expired deadline. counter == 0 lands on the check slot
        // (0 & 0x3FF == 0) so the clock is read and the abort fires.
        let expired = expired_deadline();
        let err = check_deadline(Some(expired), 0).expect_err("expired deadline must abort");
        let Error::GqlCompileError(msg) = err else {
            panic!("expected GqlCompileError, got {err:?}");
        };
        assert!(
            msg.starts_with(TIMEOUT_MSG_PREFIX),
            "abort message must carry TIMEOUT_MSG_PREFIX, got {msg:?}"
        );
    }

    #[test]
    fn check_deadline_skips_clock_off_check_slot() {
        // Even with an expired deadline, counters whose low 10 bits are
        // non-zero skip the clock read and do not abort. This is what makes
        // the per-iteration cost negligible on the hot path.
        let expired = expired_deadline();
        for counter in [1_u64, 2, 1023, 1025] {
            check_deadline(Some(expired), counter)
                .expect("off-slot counters skip the clock and never abort");
        }
    }

    // ── Task 6 C2: deadline threaded into execute + compile_match cross-join ──

    #[test]
    fn execute_with_expired_deadline_aborts_cross_join() {
        use crate::{Graph, props};
        // Two-pattern MATCH → cross-join. With an already-expired deadline the
        // first deadline check inside compile_match's cross-join (counter == 0)
        // reads the clock and aborts.
        let mut g = Graph::new();
        for i in 0_i64..5 {
            g.add_node("A", props! { "i" => i }).unwrap();
            g.add_node("B", props! { "i" => i }).unwrap();
        }
        let q = crate::gql::parse("MATCH (a:A), (b:B) RETURN a, b").unwrap();
        let expired = expired_deadline();
        let err = execute_with_deadline(&g, &q, 0, Some(expired))
            .expect_err("expired deadline must abort the cross-join");
        let Error::GqlCompileError(msg) = err else {
            panic!("expected GqlCompileError, got {err:?}");
        };
        assert!(
            msg.starts_with(TIMEOUT_MSG_PREFIX),
            "abort must carry TIMEOUT_MSG_PREFIX, got {msg:?}"
        );
    }

    #[test]
    fn execute_with_no_deadline_completes_normally() {
        use crate::{Graph, props};
        // deadline == None must never abort — the cross-join runs to completion.
        let mut g = Graph::new();
        for i in 0_i64..3 {
            g.add_node("A", props! { "i" => i }).unwrap();
            g.add_node("B", props! { "i" => i }).unwrap();
        }
        let q = crate::gql::parse("MATCH (a:A), (b:B) RETURN a, b").unwrap();
        let rows = execute_with_deadline(&g, &q, 0, None)
            .expect("None deadline never aborts");
        assert_eq!(rows.len(), 9, "3×3 cross-join produces 9 rows");
    }

    // ── Task 6 C3: deadline threaded into expand_variable_hop BFS ──

    #[test]
    fn execute_with_expired_deadline_aborts_variable_hop() {
        use crate::{Graph, props};
        // A chain a→b→c→… so a `[*1..N]` expansion does real BFS work. With an
        // already-expired deadline the first BFS dequeue (counter == 0) reads
        // the clock and aborts.
        let mut g = Graph::new();
        let mut prev = g.add_node("N", props! { "i" => 0_i64 }).unwrap();
        for i in 1_i64..20 {
            let cur = g.add_node("N", props! { "i" => i }).unwrap();
            g.add_edge("E", prev, cur, props! {}).unwrap();
            prev = cur;
        }
        let q = crate::gql::parse("MATCH (a:N)-[*1..10]->(b:N) RETURN a, b").unwrap();
        let expired = expired_deadline();
        let err = execute_with_deadline(&g, &q, 0, Some(expired))
            .expect_err("expired deadline must abort variable-length expansion");
        let Error::GqlCompileError(msg) = err else {
            panic!("expected GqlCompileError, got {err:?}");
        };
        assert!(
            msg.starts_with(TIMEOUT_MSG_PREFIX),
            "abort must carry TIMEOUT_MSG_PREFIX, got {msg:?}"
        );
    }

    #[test]
    fn execute_variable_hop_with_no_deadline_completes() {
        use crate::{Graph, props};
        // deadline == None: the [*1..N] expansion runs to completion.
        let mut g = Graph::new();
        let mut prev = g.add_node("N", props! { "i" => 0_i64 }).unwrap();
        for i in 1_i64..5 {
            let cur = g.add_node("N", props! { "i" => i }).unwrap();
            g.add_edge("E", prev, cur, props! {}).unwrap();
            prev = cur;
        }
        let q = crate::gql::parse("MATCH (a:N)-[*1..4]->(b:N) RETURN a, b").unwrap();
        let rows = execute_with_deadline(&g, &q, 0, None)
            .expect("None deadline never aborts");
        assert!(!rows.is_empty(), "var-length expansion should produce rows");
    }

    // ── Task 6 C4: shortestPath BFS deadline via the abort cell ──
    //
    // The BFS runs inside the infallible `eval_expr` path. With an
    // already-expired deadline, an end-to-end query would abort in the
    // materialization per-row check (C5) BEFORE the BFS ever runs, so these
    // tests exercise `shortest_path_bfs_constrained` / `shortest_path_bfs`
    // and the `DeadlineAbort` cell directly to isolate the BFS path.

    #[test]
    fn shortest_path_bfs_constrained_trips_abort_cell_on_expired_deadline() {
        use crate::{Graph, props};
        // A chain so the BFS would do real work if not aborted.
        let mut g = Graph::new();
        let mut ids = Vec::new();
        let mut prev = g.add_node("N", props! { "i" => 0_i64 }).unwrap();
        ids.push(prev);
        for i in 1_i64..30 {
            let cur = g.add_node("N", props! { "i" => i }).unwrap();
            g.add_edge("E", prev, cur, props! {}).unwrap();
            ids.push(cur);
            prev = cur;
        }
        let expired = expired_deadline();
        let abort = DeadlineAbort::new(Some(expired));
        // counter starts at 0 → first dequeue lands on the check slot and trips.
        let path = shortest_path_bfs_constrained(
            &g, ids[0], ids[29], Some(40), Some("E"), Direction::Outgoing, &abort,
        );
        assert!(path.is_none(), "expired deadline must abort the BFS (returns None)");
        assert!(abort.is_aborted(), "the BFS must trip the abort cell on expiry");
    }

    #[test]
    fn shortest_path_bfs_unconstrained_trips_abort_cell_on_expired_deadline() {
        use crate::{Graph, props};
        let mut g = Graph::new();
        let mut ids = Vec::new();
        let mut prev = g.add_node("N", props! { "i" => 0_i64 }).unwrap();
        ids.push(prev);
        for i in 1_i64..30 {
            let cur = g.add_node("N", props! { "i" => i }).unwrap();
            g.add_edge("E", prev, cur, props! {}).unwrap();
            ids.push(cur);
            prev = cur;
        }
        let expired = expired_deadline();
        let abort = DeadlineAbort::new(Some(expired));
        let path = shortest_path_bfs(&g, ids[0], ids[29], &abort);
        assert!(path.is_none(), "expired deadline must abort the unconstrained BFS");
        assert!(abort.is_aborted(), "the BFS must trip the abort cell on expiry");
    }

    // `{i:0}` is Cypher inline-property syntax, not a format placeholder.
    #[allow(clippy::literal_string_with_formatting_args)]
    #[test]
    fn materialization_maps_tripped_abort_cell_to_timeout_err() {
        use crate::{Graph, props};
        // End-to-end: an expired deadline + a shortestPath projection aborts
        // with the timeout prefix. (Here the materialization per-row check and
        // the BFS abort cell both point at the same expired deadline; this
        // confirms the *whole pipeline* surfaces the timeout Err.)
        let mut g = Graph::new();
        let mut prev = g.add_node("N", props! { "i" => 0_i64 }).unwrap();
        for i in 1_i64..10 {
            let cur = g.add_node("N", props! { "i" => i }).unwrap();
            g.add_edge("E", prev, cur, props! {}).unwrap();
            prev = cur;
        }
        let q = crate::gql::parse(
            "MATCH (a:N {i:0}), (b:N {i:9}) \
             RETURN shortestPath((a)-[*1..40]->(b)) AS p",
        )
        .unwrap();
        let expired = expired_deadline();
        let err = execute_with_deadline(&g, &q, 0, Some(expired))
            .expect_err("expired deadline must abort the shortestPath query");
        let Error::GqlCompileError(msg) = err else {
            panic!("expected GqlCompileError, got {err:?}");
        };
        assert!(
            msg.starts_with(TIMEOUT_MSG_PREFIX),
            "abort must carry TIMEOUT_MSG_PREFIX, got {msg:?}"
        );
    }

    // `{i:0}` is Cypher inline-property syntax, not a format placeholder.
    #[allow(clippy::literal_string_with_formatting_args)]
    #[test]
    fn execute_shortest_path_with_no_deadline_completes() {
        use crate::{Graph, props};
        // deadline == None: shortestPath resolves to a path.
        let mut g = Graph::new();
        let mut prev = g.add_node("N", props! { "i" => 0_i64 }).unwrap();
        for i in 1_i64..6 {
            let cur = g.add_node("N", props! { "i" => i }).unwrap();
            g.add_edge("E", prev, cur, props! {}).unwrap();
            prev = cur;
        }
        let q = crate::gql::parse(
            "MATCH (a:N {i:0}), (b:N {i:5}) \
             RETURN shortestPath((a)-[*1..10]->(b)) AS p",
        )
        .unwrap();
        let rows = execute_with_deadline(&g, &q, 0, None)
            .expect("None deadline never aborts");
        assert_eq!(rows.len(), 1);
        assert!(
            matches!(rows[0].get("p"), Some(GqlValue::List(_))),
            "shortestPath should resolve to a node-id list, got {:?}",
            rows[0].get("p")
        );
    }

    // ── Task 6 C5: deadline in the materialization loop ──

    #[test]
    fn execute_with_expired_deadline_aborts_materialization() {
        use crate::{Graph, props};
        // A single-pattern MATCH over enough rows that materialization is the
        // active loop (no cross-join, no var-hop). Row 0 lands on the
        // check_deadline slot (i == 0) and aborts immediately.
        let mut g = Graph::new();
        for i in 0_i64..50 {
            g.add_node("A", props! { "i" => i }).unwrap();
        }
        let q = crate::gql::parse("MATCH (a:A) RETURN a").unwrap();
        let expired = expired_deadline();
        let err = execute_with_deadline(&g, &q, 0, Some(expired))
            .expect_err("expired deadline must abort materialization");
        let Error::GqlCompileError(msg) = err else {
            panic!("expected GqlCompileError, got {err:?}");
        };
        assert!(
            msg.starts_with(TIMEOUT_MSG_PREFIX),
            "abort must carry TIMEOUT_MSG_PREFIX, got {msg:?}"
        );
    }

    #[test]
    fn execute_materialization_with_no_deadline_returns_all_rows() {
        use crate::{Graph, props};
        let mut g = Graph::new();
        for i in 0_i64..50 {
            g.add_node("A", props! { "i" => i }).unwrap();
        }
        let q = crate::gql::parse("MATCH (a:A) RETURN a").unwrap();
        let rows = execute_with_deadline(&g, &q, 0, None)
            .expect("None deadline never aborts");
        assert_eq!(rows.len(), 50, "all 50 rows materialized when deadline is None");
    }

    // ── Cycle 1.1: GqlValue::Map variant ────────────────────────────────────

    #[test]
    fn gql_value_map_variant_exists() {
        use std::collections::HashMap;
        let m = GqlValue::Map(HashMap::from([
            ("x".to_owned(), GqlValue::Int(1)),
            ("y".to_owned(), GqlValue::Str("hi".into())),
        ]));
        assert!(matches!(m, GqlValue::Map(_)));
    }

    #[test]
    fn gql_value_to_property_map_returns_none() {
        use std::collections::HashMap;
        let m = GqlValue::Map(HashMap::new());
        assert_eq!(gql_value_to_property(&m), None);
    }

    #[test]
    fn gql_value_slice_to_key_map_has_stable_prefix() {
        use std::collections::HashMap;
        // Map must not panic inside gql_value_slice_to_key — it encodes as 'M…'.
        let m = GqlValue::Map(HashMap::from([("a".to_owned(), GqlValue::Int(1))]));
        let key = gql_value_slice_to_key(std::slice::from_ref(&m));
        assert!(key.starts_with('M'));
    }

    // ── Cycle 5.1: apply_map_to_node helpers ─────────────────────────────────

    #[test]
    fn apply_map_to_node_overwrite_replaces_all_props() {
        use std::collections::HashMap;
        use crate::{Graph, props};

        let mut g = Graph::new();
        let id = g.add_node("Person", props! { "name" => "Alice", "age" => 35_i64 }).unwrap();

        let map = HashMap::from([
            ("name".to_owned(), GqlValue::Str("Bob".into())),
            ("score".to_owned(), GqlValue::Int(99)),
        ]);
        apply_map_to_node_overwrite(&mut g, id, &map).unwrap();

        let node = g.node(id).unwrap();
        assert!(node.properties().get("age").is_none(), "overwrite must clear old props");
        assert_eq!(node.properties().get("name"), Some(&crate::property::Property::String("Bob".into())));
        assert_eq!(node.properties().get("score"), Some(&crate::property::Property::I64(99)));
    }

    #[test]
    fn apply_map_to_node_merge_preserves_existing_props() {
        use std::collections::HashMap;
        use crate::{Graph, props};

        let mut g = Graph::new();
        let id = g.add_node("Person", props! { "name" => "Alice", "age" => 35_i64 }).unwrap();

        let map = HashMap::from([("score".to_owned(), GqlValue::Int(42))]);
        apply_map_to_node_merge(&mut g, id, &map).unwrap();

        let node = g.node(id).unwrap();
        assert_eq!(node.properties().get("name"), Some(&crate::property::Property::String("Alice".into())));
        assert_eq!(node.properties().get("age"), Some(&crate::property::Property::I64(35)));
        assert_eq!(node.properties().get("score"), Some(&crate::property::Property::I64(42)));
    }

    // ── Cycle 1.2: GqlValue Node/Relationship/Path variants (Fase B C1) ────────

    #[test]
    fn gql_path_holds_nodes_and_rels_with_invariant() {
        use std::collections::HashMap;
        let n0 = GqlNode { id: 1, labels: vec!["User".to_owned()], props: HashMap::new() };
        let n1 = GqlNode { id: 2, labels: vec!["Resource".to_owned()], props: HashMap::new() };
        let r0 = GqlRelationship {
            id: 10, start_id: 1, end_id: 2, rel_type: "OWNS".to_owned(), props: HashMap::new(),
        };
        let path = GqlPath { nodes: vec![n0, n1], rels: vec![r0] };
        assert_eq!(path.nodes.len(), path.rels.len() + 1); // Neo4j path invariant
        let v = GqlValue::Path(path);
        assert!(matches!(v, GqlValue::Path(_)));
    }

    #[test]
    fn gql_node_is_a_gqlvalue_variant() {
        use std::collections::HashMap;
        let v = GqlValue::Node(GqlNode { id: 7, labels: vec![], props: HashMap::new() });
        assert!(matches!(v, GqlValue::Node(_)));
    }

    // ── Cycle 6: path functions over a bound path (nodes/relationships/length) ──
    //
    // These run the FULL query from text through `execute`, exactly the path
    // the Bolt server takes. They are the in-process mirror of the C7 .NET
    // probe: `length(p)` counts edges, `relationships(p)` yields a list of
    // `GqlValue::Relationship`, and the ReBAC predicate `ALL(rel IN
    // relationships(p) WHERE …)` rejects a chain with one bad link.

    /// Seeds `(a)-[:LINK {expired}]->(b)-[:LINK {expired}]->(c)` over label `N`,
    /// keyed by `k` so the query can pin endpoints. `b_c_expired` flips the
    /// second link's `expired` flag for the ReBAC-fail case.
    fn seed_two_link_chain(b_c_expired: bool) -> crate::Graph {
        use crate::{props, Graph};
        let mut g = Graph::new();
        let a = g.add_node("N", props! { "k" => "a" }).unwrap();
        let b = g.add_node("N", props! { "k" => "b" }).unwrap();
        let c = g.add_node("N", props! { "k" => "c" }).unwrap();
        g.add_edge("LINK", a, b, props! { "expired" => false }).unwrap();
        g.add_edge("LINK", b, c, props! { "expired" => b_c_expired }).unwrap();
        g
    }

    #[test]
    fn length_over_bound_var_length_path_counts_edges() {
        let g = seed_two_link_chain(false);
        let q = crate::gql::parse(
            "MATCH p = (a:N {k:'a'})-[:LINK*1..3]->(c:N {k:'c'}) RETURN length(p) AS len",
        )
        .unwrap();
        let rows = execute(&g, &q, 0).unwrap();
        assert_eq!(rows.len(), 1, "one a→c path");
        assert_eq!(rows[0].get("len"), Some(&GqlValue::Int(2)), "edges, not nodes");
    }

    #[test]
    fn relationships_over_bound_path_yields_relationship_values() {
        let g = seed_two_link_chain(false);
        let q = crate::gql::parse(
            "MATCH p = (a:N {k:'a'})-[:LINK*1..3]->(c:N {k:'c'}) RETURN relationships(p) AS rels",
        )
        .unwrap();
        let rows = execute(&g, &q, 0).unwrap();
        assert_eq!(rows.len(), 1);
        match rows[0].get("rels") {
            Some(GqlValue::List(rels)) => {
                assert_eq!(rels.len(), 2, "two LINK edges");
                assert!(matches!(rels[0], GqlValue::Relationship(_)));
                if let GqlValue::Relationship(r) = &rels[0] {
                    assert_eq!(r.rel_type, "LINK");
                    assert_eq!(r.props.get("expired"), Some(&GqlValue::Bool(false)));
                }
            }
            other => panic!("expected list of relationships, got {other:?}"),
        }
    }

    #[test]
    fn return_bare_path_yields_path_value() {
        // Probe variant P4: `RETURN p` projects the bound path as a first-class
        // GqlValue::Path (3 nodes, 2 rels), which the Bolt layer serialises as
        // the 0x50 struct. The driver's As<IPath>() is the wire gate; this is
        // the in-process mirror that the value is a Path with the right shape.
        let g = seed_two_link_chain(false);
        let q = crate::gql::parse(
            "MATCH p = (a:N {k:'a'})-[:LINK*1..3]->(c:N {k:'c'}) RETURN p AS r",
        )
        .unwrap();
        let rows = execute(&g, &q, 0).unwrap();
        assert_eq!(rows.len(), 1);
        match rows[0].get("r") {
            Some(GqlValue::Path(p)) => {
                assert_eq!(p.nodes.len(), 3, "a, b, c");
                assert_eq!(p.rels.len(), 2, "two LINK edges");
                assert_eq!(p.nodes.len(), p.rels.len() + 1, "Neo4j path invariant");
            }
            other => panic!("expected GqlValue::Path, got {other:?}"),
        }
    }

    #[test]
    fn nodes_over_bound_path_yields_node_values() {
        let g = seed_two_link_chain(false);
        let q = crate::gql::parse(
            "MATCH p = (a:N {k:'a'})-[:LINK*1..3]->(c:N {k:'c'}) RETURN nodes(p) AS ns",
        )
        .unwrap();
        let rows = execute(&g, &q, 0).unwrap();
        assert_eq!(rows.len(), 1);
        match rows[0].get("ns") {
            Some(GqlValue::List(ns)) => {
                assert_eq!(ns.len(), 3, "a, b, c");
                assert!(ns.iter().all(|n| matches!(n, GqlValue::Node(_))));
            }
            other => panic!("expected list of nodes, got {other:?}"),
        }
    }

    #[test]
    fn rebac_all_over_relationships_passes_when_no_link_expired() {
        let g = seed_two_link_chain(false);
        let q = crate::gql::parse(
            "MATCH p = (a:N {k:'a'})-[:LINK*1..3]->(c:N {k:'c'}) \
             WHERE ALL(rel IN relationships(p) WHERE rel.expired = false) RETURN c.k AS k",
        )
        .unwrap();
        let rows = execute(&g, &q, 0).unwrap();
        assert_eq!(rows.len(), 1, "all links valid → chain authorised");
        assert_eq!(rows[0].get("k"), Some(&GqlValue::Str("c".to_owned())));
    }

    #[test]
    fn rebac_all_over_relationships_rejects_when_a_link_expired() {
        // Second link expired:true → the ReBAC predicate must reject the chain.
        let g = seed_two_link_chain(true);
        let q = crate::gql::parse(
            "MATCH p = (a:N {k:'a'})-[:LINK*1..3]->(c:N {k:'c'}) \
             WHERE ALL(rel IN relationships(p) WHERE rel.expired = false) RETURN c.k AS k",
        )
        .unwrap();
        let rows = execute(&g, &q, 0).unwrap();
        assert_eq!(rows.len(), 0, "one expired link → chain rejected");
    }

    #[test]
    fn prop_access_on_relationship_iteration_var_reads_entity_prop() {
        // Regression: `rel.expired` where `rel` iterates over
        // `relationships(p)` (each a GqlValue::Relationship) must read the
        // relationship's own property, not fall through to the empty
        // PatternMatch. Before the fix, PropAccess in binding context ignored
        // `b.vals`, so the ReBAC predicate silently saw Null and rejected
        // every valid chain. `ANY(... rel.expired = false)` is true iff at
        // least one link reads back its real `expired=false` — which fails
        // (stays false) if PropAccess returns Null.
        let g = seed_two_link_chain(false);
        let q = crate::gql::parse(
            "MATCH p = (a:N {k:'a'})-[:LINK*1..3]->(c:N {k:'c'}) \
             WHERE ANY(rel IN relationships(p) WHERE rel.expired = false) RETURN c.k AS k",
        )
        .unwrap();
        let rows = execute(&g, &q, 0).unwrap();
        assert_eq!(
            rows.len(),
            1,
            "ANY must read rel.expired from the Relationship value, not Null",
        );
    }

    #[test]
    fn length_over_fixed_multi_segment_path_counts_edges() {
        let g = seed_two_link_chain(false);
        let q = crate::gql::parse(
            "MATCH p = (a:N {k:'a'})-[r1:LINK]->(b:N {k:'b'})-[r2:LINK]->(c:N {k:'c'}) \
             RETURN length(p) AS len",
        )
        .unwrap();
        let rows = execute(&g, &q, 0).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get("len"), Some(&GqlValue::Int(2)));
    }

    #[test]
    fn c7_seed_statements_parse() {
        // The C7 .NET probe seeds the two-link chains for the ReBAC gate. Two
        // engine limitations (each cost a Docker cycle to discover) shape the
        // seed, pinned here in-process so a future edit can't silently regress
        // the seed into an unparseable form:
        //   - the parser rejects `CREATE … CREATE …` and two-hop CREATE paths
        //     ("unexpected tokens after CREATE");
        //   - the executor rejects CREATE-edge unless source+target are bound by
        //     a MATCH ("edge creation in CREATE requires a MATCH clause");
        //   - inline props take LITERALS only, not `$param` ("expected literal
        //     value, found $") — so the seed uses literal keys, not params.
        // So the seed creates the three nodes individually, then MATCHes both
        // endpoints and CREATEs each edge. Mutation EXECUTION lives in the
        // server crate (not the engine), so this pins parse only; the probe
        // itself is the execution gate.
        for q in [
            "CREATE (n:N {k:'a'})",
            "MATCH (a:N {k:'a'}), (b:N {k:'b'}) CREATE (a)-[:LINK {expired:false}]->(b)",
            "MATCH (b:N {k:'b'}), (c:N {k:'c'}) CREATE (b)-[:LINK {expired:true}]->(c)",
        ] {
            let stmt = crate::gql::parse_statement(q).unwrap_or_else(|e| panic!("parse {q}: {e:?}"));
            assert!(
                matches!(stmt, crate::gql::GqlStatement::Mutation(_)),
                "{q} must parse as a mutation, got {stmt:?}",
            );
        }
    }

    #[test]
    fn apply_pipeline_set_reports_contains_updates_true_when_properties_changed() {
        let mut g = crate::Graph::new();
        let mut props = crate::Properties::new();
        props.insert("age".to_owned(), crate::Property::I64(0));
        g.add_node("Person", props).unwrap();
        let stmt = crate::gql::parse_statement("MATCH (n:Person) WITH n SET n.age = 1").unwrap();
        let stats = execute_pipeline_mutation(&mut g, &stmt, None).unwrap();
        assert_eq!(stats.properties_set, 1);
        assert!(stats.contains_updates());
    }

    // ── Issue #45: DELETE / DETACH DELETE pipeline terminal ───────────────────

    #[test]
    fn apply_pipeline_delete_removes_node() {
        let mut g = crate::Graph::new();
        g.add_node("Person", crate::Properties::new()).unwrap();
        let stmt = crate::gql::parse_statement("MATCH (n:Person) WITH n DELETE n").unwrap();
        let stats = execute_pipeline_mutation(&mut g, &stmt, None).unwrap();
        assert_eq!(stats.nodes_deleted, 1);
        assert_eq!(g.node_count(), 0);
    }

    #[test]
    fn apply_pipeline_detach_delete_node_with_edge() {
        let mut g = crate::Graph::new();
        let a = g.add_node("Solo", crate::Properties::new()).unwrap();
        let b = g.add_node("Other", crate::Properties::new()).unwrap();
        g.add_edge("KNOWS", a, b, crate::Properties::new()).unwrap();
        let stmt =
            crate::gql::parse_statement("MATCH (n:Solo) WITH n DETACH DELETE n").unwrap();
        let stats = execute_pipeline_mutation(&mut g, &stmt, None).unwrap();
        assert_eq!(stats.nodes_deleted, 1);
        assert_eq!(stats.edges_deleted, 1);
        assert_eq!(g.node_count(), 1, "the Other node survives");
        assert_eq!(g.edge_count(), 0);
    }

    #[test]
    fn apply_pipeline_delete_edge() {
        let mut g = crate::Graph::new();
        let a = g.add_node("A", crate::Properties::new()).unwrap();
        let b = g.add_node("B", crate::Properties::new()).unwrap();
        g.add_edge("KNOWS", a, b, crate::Properties::new()).unwrap();
        let stmt =
            crate::gql::parse_statement("MATCH ()-[r:KNOWS]->() WITH r DELETE r").unwrap();
        let stats = execute_pipeline_mutation(&mut g, &stmt, None).unwrap();
        assert_eq!(stats.edges_deleted, 1);
        assert_eq!(stats.nodes_deleted, 0);
        assert_eq!(g.node_count(), 2);
        assert_eq!(g.edge_count(), 0);
    }

    #[test]
    fn apply_pipeline_delete_in_txn_isolated_and_reversible() {
        let mut g = crate::Graph::new();
        g.enable_mvcc();
        let a = g.add_node("Person", crate::Properties::new()).unwrap();
        let txn = g.begin_txn().unwrap();
        let stmt = crate::gql::parse_statement("MATCH (n:Person) WITH n DELETE n").unwrap();
        let stats = execute_pipeline_mutation(&mut g, &stmt, Some(txn)).unwrap();
        assert_eq!(stats.nodes_deleted, 1);
        // Isolated before commit: a fresh auto-commit read still sees the node.
        assert!(g.node(a).is_ok(), "visible before commit");
        // Reversible: rollback restores it.
        g.rollback_txn(txn).unwrap();
        assert!(g.node(a).is_ok(), "rollback restores the node");
    }

    #[test]
    fn read_after_committed_txn_delete_omits_ghost_node() {
        // Repro for the post-commit read consistency gap surfaced by issue #45:
        // a node deleted in a committed transaction is still in the label index
        // (its category-B baja is the vacuum's job), so a MATCH by label must
        // still filter it out by visibility rather than error or return it.
        let mut g = crate::Graph::new();
        g.enable_mvcc();
        g.add_node("Person", crate::Properties::new()).unwrap();
        let txn = g.begin_txn().unwrap();
        let del = crate::gql::parse_statement("MATCH (n:Person) WITH n DELETE n").unwrap();
        execute_pipeline_mutation(&mut g, &del, Some(txn)).unwrap();
        g.commit_txn(txn).unwrap();

        let read = crate::gql::parse_statement("MATCH (n:Person) RETURN n").unwrap();
        let crate::gql::GqlStatement::Query(q) = read else {
            panic!("expected query");
        };
        let rows = execute(&g, &q, 0).unwrap();
        assert_eq!(rows.len(), 0, "the committed-deleted node must not appear");
    }

    // sentinel: end of compiler unit tests
}
