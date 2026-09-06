// SPDX-License-Identifier: MIT

//! Cypher write-path execution, unified in the engine.
//!
//! Historically the translation from a parsed write statement (`CREATE`, `SET`,
//! `MERGE`, `UNWIND … CREATE`) to concrete graph mutations lived in the server
//! crate (`ermya-graph-server`). That duplicated the write orchestration and
//! made transactional writes impossible to reach without re-implementing the
//! same logic a second time. This module owns that orchestration in the engine,
//! so a single code path serves both auto-commit and explicit-transaction
//! writes, and so the engine crate is self-contained for embedded consumers
//! that create in-memory graphs without the server.
//!
//! # Transaction awareness
//!
//! Each executor takes `txn_id: Option<u64>`. With `None` it performs an
//! auto-commit write (`Graph::add_node`/`update_node`/…); with `Some(t)` it
//! writes into the pending delta chain of transaction `t`
//! (`Graph::add_node_in_txn`/…), invisible to other readers until commit.
//!
//! # Lock discipline (two-lock decision)
//!
//! The MATCH-bearing executors are split into a read phase (compile bindings)
//! and a write phase (apply mutations) so the caller can hold a shared read
//! lock for the read phase, release it, then take an exclusive write lock for
//! the write phase — the measured "two-lock" mutation path that keeps tail
//! latency low under contention. A single `&mut Graph` signature would force
//! the caller to hold one exclusive lock across both phases (the rejected
//! "one-lock" variant), so those executors deliberately do *not* take
//! `&mut Graph` for their read phase.

use std::collections::HashMap;

use crate::gql::{self, GqlMutationResult, GqlValue};
use crate::{Graph, NodeId, PatternMatch, Properties};

/// A single result row: column name → value.
///
/// Lives in the engine because both the column name and [`GqlValue`] are engine
/// types; the server re-exports this alias rather than defining a mirror.
pub type ResultRow = HashMap<String, GqlValue>;

/// The read-phase output of an `UNWIND … CREATE`: the evaluated UNWIND list
/// elements and the compiled MATCH binding rows to cross-join with them.
pub type UnwindReadPhase = (Vec<GqlValue>, Vec<HashMap<String, NodeId>>);

/// Resolves a `CREATE` node pattern's properties, supporting both the per-key
/// form (`CREATE (n {a: 1, b: $x})`, stored in `props`) and the whole-entity
/// map form (`CREATE (n $map)`, stored in `prop_map`).
///
/// `prop_map` carries an [`gql::Expr`] that resolves to a [`GqlValue::Map`] — a
/// bare `$param` that survived `param_substitution` as a `ParamRef` (resolved
/// from `params`) or any map-valued expression. Map entries whose values cannot
/// lower to a scalar `Property` are skipped. The two forms are mutually
/// exclusive at parse time (`prop_map` is `Some` only when `props` is empty).
fn resolve_create_node_props(
    props: &[(String, gql::Expr)],
    prop_map: Option<&gql::Expr>,
    params: &HashMap<String, GqlValue>,
    graph: &Graph,
) -> crate::Result<Properties> {
    use crate::gql::gql_value_to_property;

    if let Some(map_expr) = prop_map {
        let map = resolve_map_expr(map_expr, params, graph, "CREATE (n $map)")?;
        return Ok(map
            .iter()
            .filter_map(|(k, v)| gql_value_to_property(v).map(|p| (k.clone(), p)))
            .collect());
    }
    let empty_pm = PatternMatch::empty();
    Ok(gql::resolve_create_props(props, &empty_pm, graph, None))
}

/// Resolves a whole-entity map expression (`n = $map` / `n += $map`) to a
/// concrete property map.
///
/// `$map` parameters survive `param_substitution::apply` as unsubstituted
/// `Expr::ParamRef` nodes because there is no `Literal::Map` to lower them to.
/// The runtime `params` map — the same `RUN.params` the handler threads through
/// — carries the actual [`GqlValue::Map`], so a bare `ParamRef` is resolved
/// against it here. Any other expression form is evaluated normally via
/// [`gql::execute_expr`], keeping the resolver total.
fn resolve_map_expr<'a>(
    map_expr: &gql::Expr,
    params: &'a HashMap<String, GqlValue>,
    graph: &Graph,
    op_label: &str,
) -> crate::Result<std::borrow::Cow<'a, HashMap<String, GqlValue>>> {
    use std::borrow::Cow;

    use crate::gql::{Expr, ParamRef};

    // Fast path: a bare `$param` that survived substitution as a Map.
    if let Expr::ParamRef(ParamRef::Named(name)) = map_expr {
        return match params.get(name) {
            Some(GqlValue::Map(m)) => Ok(Cow::Borrowed(m)),
            Some(other) => Err(crate::Error::GqlCompileError(format!(
                "{op_label}: parameter ${name} is not a Map (got {other:?})"
            ))),
            None => Err(crate::Error::GqlCompileError(format!(
                "{op_label}: missing Map parameter ${name}"
            ))),
        };
    }

    // General path: evaluate the expression; it must yield a Map.
    let empty_pm = PatternMatch::empty();
    match gql::execute_expr(map_expr, &empty_pm, graph) {
        GqlValue::Map(m) => Ok(Cow::Owned(m)),
        other => Err(crate::Error::GqlCompileError(format!(
            "{op_label}: expected a Map value, got {other:?}"
        ))),
    }
}

/// Projects a single node as a one-column [`ResultRow`] mapping `var` to a
/// [`GqlValue::Map`] of its properties — the shape a client receives for
/// `MERGE … RETURN n`, `MATCH … SET … RETURN n`, and `CREATE (n …) RETURN n`.
///
/// With `txn_id = Some(t)` the node is read at transaction `t`'s snapshot so a
/// just-created pending node (invisible to auto-commit readers before commit)
/// still projects; with `None` it reads the committed graph.
fn project_node_as_map_row(
    graph: &Graph,
    node_id: NodeId,
    var: &str,
    txn_id: Option<u64>,
) -> crate::Result<ResultRow> {
    let node = match txn_id {
        Some(t) => graph.node_in_txn(t, node_id)?,
        None => graph.node(node_id)?,
    };
    let props_map: HashMap<String, GqlValue> = node
        .properties()
        .iter()
        .map(|(k, v)| (k.clone(), gql::gql_value_from_property(v)))
        .collect();
    let mut row = ResultRow::new();
    row.insert(var.to_owned(), GqlValue::Map(props_map));
    Ok(row)
}

/// Projects the trailing `RETURN <vars>` of a `MATCH … SET … RETURN` /
/// `CREATE … RETURN` mutation. Each return item must be a bare variable bound
/// to a node id in `bindings` (the client pattern is `RETURN n`); any other
/// expression form is rejected, since whole-node projection is the only
/// contract exercised here. Produces one row per binding.
fn project_mutation_return(
    graph: &Graph,
    return_clause: &gql::ReturnClause,
    bindings: &[gql::MatchRow],
    txn_id: Option<u64>,
) -> crate::Result<Vec<ResultRow>> {
    use crate::gql::Expr;

    let mut rows = Vec::with_capacity(bindings.len());
    for binding in bindings {
        let mut row = ResultRow::new();
        for item in &return_clause.items {
            let Expr::Var(var) = &item.expr else {
                return Err(crate::Error::GqlCompileError(
                    "RETURN after a mutation supports only bare node variables \
                     (e.g. RETURN n)"
                        .to_owned(),
                ));
            };
            let key = item.alias.as_ref().unwrap_or(var);
            let node_id = binding.nodes.get(var).copied().ok_or_else(|| {
                crate::Error::GqlCompileError(format!(
                    "RETURN variable '{var}' not bound by the mutation"
                ))
            })?;
            let projected = project_node_as_map_row(graph, node_id, key, txn_id)?;
            row.extend(projected);
        }
        rows.push(row);
    }
    Ok(rows)
}

/// Executes a bare mutation statement (no MATCH context) against `graph`.
///
/// With `txn_id = None` the write is auto-committed; with `Some(t)` it is
/// written into transaction `t`'s pending delta chain, invisible until commit.
///
/// Only `CREATE` of node patterns is supported without a MATCH clause; a bare
/// edge `CREATE` is rejected because it has no bound source/target variables.
///
/// # Errors
///
/// Returns [`crate::Error`] if a write fails (storage/quota), a bare edge
/// `CREATE` is attempted, an unsupported mutation clause is given, or a trailing
/// `RETURN` references a variable the mutation did not bind.
// `implicit_hasher`: `params` is forwarded verbatim from the Bolt `RUN.params`
// map, whose `RandomState` hasher the signature fixes; generalizing it would
// force every caller to name the hasher for no benefit.
#[allow(clippy::implicit_hasher)]
pub fn execute_bare_mutation(
    graph: &mut Graph,
    mutation: &gql::MutationStatement,
    params: &HashMap<String, GqlValue>,
    txn_id: Option<u64>,
) -> crate::Result<(Vec<ResultRow>, GqlMutationResult)> {
    use crate::gql::{CreatePattern, MutationClause};

    let mut nodes_created: u64 = 0;
    let mut labels_added: u64 = 0;
    let mut properties_set: u64 = 0;
    let edges_created: u64 = 0;
    // Track each created node's variable binding so a trailing RETURN can
    // project it (`CREATE (n $map) RETURN n`). Anonymous nodes contribute no
    // binding and so cannot be RETURNed by name.
    let mut created_binding = gql::MatchRow::default();

    match &mutation.mutation {
        MutationClause::Create(create) => {
            for pattern in &create.patterns {
                match pattern {
                    CreatePattern::Node {
                        var,
                        label,
                        props,
                        prop_map,
                    } => {
                        let properties =
                            resolve_create_node_props(props, prop_map.as_ref(), params, &*graph)?;
                        properties_set += u64::try_from(properties.len()).unwrap_or(u64::MAX);
                        let id = match txn_id {
                            Some(t) => graph.add_node_in_txn(t, label, properties)?,
                            None => graph.add_node(label, properties)?,
                        };
                        if let Some(v) = var {
                            created_binding.nodes.insert(v.clone(), id);
                        }
                        nodes_created += 1;
                        labels_added += count_label(label);
                    }
                    CreatePattern::Edge { .. } => {
                        return Err(crate::Error::GqlCompileError(
                            "edge creation in CREATE requires a MATCH clause \
                             to bind source and target variables"
                                .to_owned(),
                        ));
                    }
                }
            }
        }
        MutationClause::Delete(_) => {
            return Err(crate::Error::GqlCompileError(
                "DELETE requires a variable bound by a preceding MATCH, UNWIND, \
                 or WITH clause; a bare DELETE has nothing to delete"
                    .to_owned(),
            ));
        }
        other => {
            return Err(crate::Error::GqlCompileError(format!(
                "mutation clause not yet supported: {other:?}"
            )));
        }
    }

    // Trailing RETURN projects the just-created nodes by their variable.
    let rows = match &mutation.return_clause {
        Some(rc) => project_mutation_return(
            &*graph,
            rc.as_ref(),
            std::slice::from_ref(&created_binding),
            txn_id,
        )?,
        None => Vec::new(),
    };

    Ok((
        rows,
        GqlMutationResult {
            nodes_created,
            edges_created,
            properties_set,
            labels_added,
            ..GqlMutationResult::default()
        },
    ))
}

/// Counts the labels contributed by a single created node.
///
/// A node carries at most one label in the `CREATE` grammar, so this returns
/// `1` for a labelled node and `0` for an anonymous one (`CREATE (n)`), matching
/// Neo4j's `labels-added` accounting.
fn count_label(label: &str) -> u64 {
    u64::from(!label.is_empty())
}

/// Returns the bound variable name targeted by a [`gql::SetAssignment`].
fn set_assignment_var(assignment: &gql::SetAssignment) -> &str {
    use crate::gql::SetAssignment;
    match assignment {
        SetAssignment::Property { var, .. }
        | SetAssignment::EntityOverwrite { var, .. }
        | SetAssignment::EntityMerge { var, .. } => var,
    }
}

/// Applies a single [`gql::SetAssignment`] to `node_id`, supporting per-property
/// (`n.prop = expr`), whole-entity overwrite (`n = $map`), and whole-entity
/// merge (`n += $map`) forms.
///
/// The assignment's `var` is **not** consulted here — the caller resolves the
/// variable to `node_id`. `params` supplies `$map` values for the whole-entity
/// forms. With `txn_id = Some(t)` the read and write both go through
/// transaction `t`'s snapshot/delta chain; with `None` they touch the committed
/// graph.
///
/// Returns the number of individual property assignments applied, so callers can
/// report an accurate `properties-set` count. A single-property `SET n.k = v`
/// counts `1`; a whole-entity `SET n = $map` / `SET n += $map` counts one per map
/// entry that lowers to a scalar property (matching Neo4j, which counts every
/// property write, not every clause).
fn apply_one_set_assignment(
    graph: &mut Graph,
    node_id: NodeId,
    assignment: &gql::SetAssignment,
    params: &HashMap<String, GqlValue>,
    txn_id: Option<u64>,
) -> crate::Result<u64> {
    use crate::gql::{gql_value_to_property, SetAssignment};

    let empty_pm = PatternMatch::empty();

    match assignment {
        SetAssignment::Property { prop, value, .. } => {
            let val = gql::execute_expr(value, &empty_pm, &*graph);
            if let Some(prop_val) = gql_value_to_property(&val) {
                let mut node = match txn_id {
                    Some(t) => graph.node_in_txn(t, node_id)?,
                    None => graph.node(node_id)?,
                };
                node.properties_mut().insert(prop.clone(), prop_val);
                match txn_id {
                    Some(t) => graph.update_node_in_txn(t, node_id, &node)?,
                    None => graph.update_node(node_id, &node)?,
                }
                return Ok(1);
            }
            Ok(0)
        }
        SetAssignment::EntityOverwrite { map_expr, .. } => {
            let map = resolve_map_expr(map_expr, params, &*graph, "SET n = $map")?;
            apply_map_to_node_overwrite_txn(graph, node_id, &map, txn_id)?;
            Ok(scalar_entry_count(&map))
        }
        SetAssignment::EntityMerge { map_expr, .. } => {
            let map = resolve_map_expr(map_expr, params, &*graph, "SET n += $map")?;
            apply_map_to_node_merge_txn(graph, node_id, &map, txn_id)?;
            Ok(scalar_entry_count(&map))
        }
    }
}

/// Counts the map entries that lower to a scalar [`Property`] — i.e. those a
/// whole-entity `SET`/`CREATE` actually writes. `Null` / nested-collection
/// values are skipped, so this matches the properties the graph stores.
fn scalar_entry_count(map: &HashMap<String, GqlValue>) -> u64 {
    let count = map
        .values()
        .filter(|v| crate::gql::gql_value_to_property(v).is_some())
        .count();
    u64::try_from(count).unwrap_or(u64::MAX)
}

/// Whole-entity overwrite (`SET n = $map`) that honours the transaction.
///
/// The committed-graph helper [`gql::apply_map_to_node_overwrite`] reads and
/// writes the committed node directly, so a transactional caller cannot use it
/// (it would neither see the txn's pending version nor record a delta). For a
/// transaction we replicate its semantics — replace all properties with the map
/// — over the txn snapshot and delta chain.
fn apply_map_to_node_overwrite_txn(
    graph: &mut Graph,
    node_id: NodeId,
    map: &HashMap<String, GqlValue>,
    txn_id: Option<u64>,
) -> crate::Result<()> {
    use crate::gql::gql_value_to_property;

    let Some(t) = txn_id else {
        return gql::apply_map_to_node_overwrite(graph, node_id, map);
    };
    let mut node = graph.node_in_txn(t, node_id)?;
    let props = node.properties_mut();
    props.clear();
    for (k, v) in map {
        if let Some(p) = gql_value_to_property(v) {
            props.insert(k.clone(), p);
        }
    }
    graph.update_node_in_txn(t, node_id, &node)
}

/// Whole-entity merge (`SET n += $map`) that honours the transaction. Mirrors
/// [`apply_map_to_node_overwrite_txn`] but keeps existing properties, adding or
/// replacing only the keys present in `map`.
fn apply_map_to_node_merge_txn(
    graph: &mut Graph,
    node_id: NodeId,
    map: &HashMap<String, GqlValue>,
    txn_id: Option<u64>,
) -> crate::Result<()> {
    use crate::gql::gql_value_to_property;

    let Some(t) = txn_id else {
        return gql::apply_map_to_node_merge(graph, node_id, map);
    };
    let mut node = graph.node_in_txn(t, node_id)?;
    let props = node.properties_mut();
    for (k, v) in map {
        if let Some(p) = gql_value_to_property(v) {
            props.insert(k.clone(), p);
        }
    }
    graph.update_node_in_txn(t, node_id, &node)
}

/// Applies an entire [`gql::SetClause`] to a single node (every assignment
/// targets `node_id`, ignoring each assignment's `var`). Used by the
/// `ON CREATE SET` / `ON MATCH SET` branches of a MERGE, where exactly one node
/// is in scope.
///
/// Returns the number of property assignments applied, so the caller can report
/// it as `properties-set`.
fn apply_on_set_clause(
    graph: &mut Graph,
    node_id: NodeId,
    set: &gql::SetClause,
    params: &HashMap<String, GqlValue>,
    txn_id: Option<u64>,
) -> crate::Result<u64> {
    let mut properties_set: u64 = 0;
    for assignment in &set.assignments {
        properties_set += apply_one_set_assignment(graph, node_id, assignment, params, txn_id)?;
    }
    Ok(properties_set)
}

/// Executes a bare `MERGE (n:Label {k: v, …}) [ON CREATE SET …] [ON MATCH SET …]
/// [RETURN var]` clause.
///
/// Semantics: look up an existing node of `label` matching every inline
/// property; if found, apply `ON MATCH SET` (0 created); otherwise create it
/// with the inline properties and apply `ON CREATE SET` (1 created). A trailing
/// `RETURN var` projects the merged node.
///
/// # Lock discipline
///
/// The lookup (read) and the create/update (write) are separated so the server
/// runs the lookup under a shared read lock, releases it, then takes an
/// exclusive write lock — the two-lock discipline. Inside the engine both
/// phases share the single `&mut Graph`, but the read view is scoped and dropped
/// before the write begins, mirroring that separation.
///
/// # Transaction awareness
///
/// With `txn_id = Some(t)` the lookup runs over transaction `t`'s snapshot
/// (seeing the txn's own pending nodes, so a repeated MERGE in one transaction
/// matches rather than duplicates — the property lookup falls back to a
/// snapshot-aware label scan through the txn view, not the committed property
/// index) and the write records deltas. With `None` both touch the committed
/// graph and use the committed property index.
/// Result of the MERGE lookup phase: the matched node id (if any) and the
/// resolved inline lookup properties (reused as initial properties on create,
/// so the write phase need not re-evaluate the merge-key expressions).
pub struct MergeLookup {
    /// The existing node matching every inline property, if one was found.
    pub existing_id: Option<NodeId>,
    /// The inline `MERGE (n {k: v, …})` properties, already resolved to values.
    pub lookup_props: Vec<(String, crate::Property)>,
}

/// MERGE phase 1 (read-only): find an existing node of `merge.label` matching
/// every inline property, over the read `view`.
///
/// Generic over [`GraphAccess`] so the caller can run it over a committed
/// `&Graph` (auto-commit) or a `TxnView` (inside a transaction, so a repeated
/// MERGE sees the transaction's own pending node). The server runs this under a
/// shared read lock, then releases it before taking the write lock — the
/// two-lock discipline.
pub fn merge_lookup<G: crate::access::GraphAccess + ?Sized>(
    view: &G,
    merge: &gql::MergeClause,
) -> MergeLookup {
    use crate::gql::gql_value_to_property;

    let empty_pm = PatternMatch::empty();
    let lookup_props: Vec<(String, crate::Property)> = merge
        .props
        .iter()
        .filter_map(|(k, expr)| {
            let val = gql::execute_expr(expr, &empty_pm, view);
            gql_value_to_property(&val).map(|p| (k.clone(), p))
        })
        .collect();

    let existing_id = if lookup_props.is_empty() {
        view.nodes_by_label(&merge.label).into_iter().next()
    } else {
        let mut candidates: Option<std::collections::HashSet<u64>> = None;
        for (key, prop) in &lookup_props {
            let ids: std::collections::HashSet<u64> = view
                .nodes_by_label_and_property(&merge.label, key, prop)
                .into_iter()
                .map(NodeId::as_u64)
                .collect();
            candidates = Some(match candidates {
                None => ids,
                Some(prev) => prev.intersection(&ids).copied().collect(),
            });
        }
        candidates
            .unwrap_or_default()
            .into_iter()
            .next()
            .map(NodeId::from_raw)
    };

    MergeLookup {
        existing_id,
        lookup_props,
    }
}

/// MERGE phase 2 (write): apply the create-or-match outcome of the lookup.
///
/// Either applies `ON MATCH SET` to the existing node (0 created) or creates the
/// node with the inline properties and applies `ON CREATE SET` (1 created), then
/// projects a trailing `RETURN var`. Honours `txn_id` for the writes and the
/// RETURN read.
///
/// # Errors
///
/// Returns [`crate::Error`] if a create/update fails (storage/quota) or the
/// trailing `RETURN` node cannot be read at the current snapshot.
#[allow(clippy::implicit_hasher)] // `params` forwarded verbatim; see `execute_bare_mutation`.
pub fn apply_merge_write(
    graph: &mut Graph,
    merge: &gql::MergeClause,
    lookup: MergeLookup,
    params: &HashMap<String, GqlValue>,
    txn_id: Option<u64>,
) -> crate::Result<(Vec<ResultRow>, GqlMutationResult)> {
    let MergeLookup {
        existing_id,
        lookup_props,
    } = lookup;

    let (merged_id, nodes_created, labels_added, properties_set) = if let Some(id) = existing_id {
        let props = match &merge.on_match {
            Some(on_match) => apply_on_set_clause(graph, id, on_match, params, txn_id)?,
            None => 0,
        };
        (id, 0_u64, 0_u64, props)
    } else {
        let init_props: Properties = lookup_props.into_iter().collect();
        let new_id = match txn_id {
            Some(t) => graph.add_node_in_txn(t, &merge.label, init_props)?,
            None => graph.add_node(&merge.label, init_props)?,
        };
        let props = match &merge.on_create {
            Some(on_create) => apply_on_set_clause(graph, new_id, on_create, params, txn_id)?,
            None => 0,
        };
        (new_id, 1_u64, count_label(&merge.label), props)
    };

    let rows = if let Some(ret_var) = &merge.return_var {
        vec![project_node_as_map_row(graph, merged_id, ret_var, txn_id)?]
    } else {
        Vec::new()
    };

    Ok((
        rows,
        GqlMutationResult {
            nodes_created,
            properties_set,
            labels_added,
            ..GqlMutationResult::default()
        },
    ))
}

/// Executes a bare `MERGE …` clause end-to-end over a single `&mut Graph`.
///
/// Runs the lookup then the write. This is the convenience entry point for the
/// embedded engine and the transactional path, where both phases share one
/// borrow. The server's auto-commit path instead calls [`merge_lookup`] and
/// [`apply_merge_write`] separately to keep its two-lock discipline.
///
/// With `txn_id = Some(t)` the lookup runs over transaction `t`'s snapshot
/// (seeing the txn's own pending nodes, so a repeated MERGE matches rather than
/// duplicates) and the write records deltas; with `None` both touch the
/// committed graph.
///
/// # Errors
///
/// Returns [`crate::Error`] if the create/update phase fails or the trailing
/// `RETURN` node cannot be read.
#[allow(clippy::implicit_hasher)] // `params` forwarded verbatim; see `execute_bare_mutation`.
pub fn execute_bare_merge(
    graph: &mut Graph,
    merge: &gql::MergeClause,
    params: &HashMap<String, GqlValue>,
    txn_id: Option<u64>,
) -> crate::Result<(Vec<ResultRow>, GqlMutationResult)> {
    // Phase 1 — lookup over the txn snapshot when present. The view borrows
    // `graph`; it is dropped before the write phase.
    let lookup = match txn_id {
        Some(t) => {
            let view = crate::gql::txn_view::TxnView::new(graph, t);
            merge_lookup(&view, merge)
        }
        None => merge_lookup(&*graph, merge),
    };
    // Phase 2 — create-or-apply.
    apply_merge_write(graph, merge, lookup, params, txn_id)
}

/// Applies the write phase of a `MATCH … CREATE/SET` mutation to `graph`, given
/// the rows already bound by the MATCH phase.
///
/// # Lock discipline
///
/// The caller owns the lock: in production the server compiles `rows` under a
/// shared read lock, releases it, then calls this with an exclusive `&mut Graph`
/// write lock — the two-lock discipline that keeps tail latency low. This
/// function is deliberately the *write half* only, so the read half can run
/// under a separate, cheaper lock.
///
/// With `txn_id = Some(t)` every create/update is written into transaction `t`'s
/// pending delta chain; with `None` it is auto-committed.
///
/// # Errors
///
/// Returns [`crate::Error`] if a create/update fails, `RETURN` follows a
/// `MATCH … CREATE` (unsupported), a `CREATE` edge references a variable not
/// bound by MATCH, or the mutation clause is unsupported in a MATCH context.
#[allow(clippy::implicit_hasher)] // `params` forwarded verbatim; see `execute_bare_mutation`.
pub fn apply_match_mutation_body(
    graph: &mut Graph,
    mutation: &gql::MutationStatement,
    rows: &[gql::MatchRow],
    params: &HashMap<String, GqlValue>,
    txn_id: Option<u64>,
) -> crate::Result<(Vec<ResultRow>, GqlMutationResult)> {
    use crate::gql::MutationClause;

    match &mutation.mutation {
        MutationClause::Create(create) => {
            if mutation.return_clause.is_some() {
                return Err(crate::Error::GqlCompileError(
                    "RETURN after MATCH … CREATE is not supported; the created \
                     nodes are not bound for projection"
                        .to_owned(),
                ));
            }
            Ok((Vec::new(), apply_match_create(graph, create, rows, txn_id)?))
        }
        MutationClause::Set(set_clause) => {
            // MATCH … SET: apply each assignment to its bound node, per row.
            let mut properties_set: u64 = 0;
            for row in rows {
                for assignment in &set_clause.assignments {
                    let var = set_assignment_var(assignment);
                    let node_id = row.nodes.get(var).copied().ok_or_else(|| {
                        crate::Error::GqlCompileError(format!(
                            "variable '{var}' not bound by MATCH clause"
                        ))
                    })?;
                    properties_set +=
                        apply_one_set_assignment(graph, node_id, assignment, params, txn_id)?;
                }
            }
            let projected = match &mutation.return_clause {
                Some(rc) => project_mutation_return(graph, rc.as_ref(), rows, txn_id)?,
                None => Vec::new(),
            };
            Ok((
                projected,
                GqlMutationResult {
                    properties_set,
                    ..GqlMutationResult::default()
                },
            ))
        }
        MutationClause::Delete(dc) => {
            // MATCH … DELETE / DETACH DELETE: delete each bound entity per row.
            // A variable may bind a node or a relationship; resolve node first,
            // then edge (issue #45, edge-aware rows).
            let mut stats = GqlMutationResult::default();
            let mut deleted_nodes: std::collections::HashSet<NodeId> =
                std::collections::HashSet::new();
            let mut deleted_edges: std::collections::HashSet<crate::EdgeId> =
                std::collections::HashSet::new();
            for row in rows {
                for var in &dc.vars {
                    if let Some(&node_id) = row.nodes.get(var) {
                        delete_node_row(
                            graph,
                            node_id,
                            dc.detach,
                            txn_id,
                            &mut deleted_nodes,
                            &mut stats,
                        )?;
                    } else if let Some(&edge_id) = row.edges.get(var) {
                        delete_edge_row(graph, edge_id, txn_id, &mut deleted_edges, &mut stats)?;
                    } else {
                        return Err(crate::Error::GqlCompileError(format!(
                            "variable '{var}' not bound by MATCH clause"
                        )));
                    }
                }
            }
            Ok((Vec::new(), stats))
        }
        other @ MutationClause::Merge(_) => Err(crate::Error::GqlCompileError(format!(
            "mutation clause not yet supported with MATCH context: {other:?}"
        ))),
    }
}

/// Counts the distinct edges incident to `node`, honouring the transaction
/// view when `txn_id` is set. Self-loops appear in both the outgoing and
/// incoming lists, so the result is deduplicated.
fn incident_edge_ids(
    graph: &Graph,
    node: NodeId,
    txn_id: Option<u64>,
) -> crate::Result<Vec<crate::EdgeId>> {
    let (out, inc) = match txn_id {
        Some(t) => (
            graph.outgoing_edges_in_txn(t, node)?,
            graph.incoming_edges_in_txn(t, node)?,
        ),
        None => (graph.outgoing_edges(node)?, graph.incoming_edges(node)?),
    };
    let mut ids: Vec<crate::EdgeId> = out.iter().chain(inc.iter()).map(|e| e.id).collect();
    ids.sort_unstable_by_key(|e| e.as_u64());
    ids.dedup();
    Ok(ids)
}

/// Deletes one bound node for a `DELETE` / `DETACH DELETE` clause, routing to
/// the auto-commit or transactional primitive by `txn_id` and enforcing the
/// no-detach-with-relationships rule.
///
/// - Without `detach`, a node that still has incident edges is rejected with
///   [`crate::Error::DeleteConnectedNode`] and nothing is removed.
/// - With `detach`, incident edges are removed first (the auto-commit
///   `remove_node` cascades them; the transactional path removes them one by
///   one because `remove_node_in_txn` does not cascade), then the node.
/// - `deleted_nodes` records already-removed nodes so repeated references to the
///   same node in one clause are idempotent (counted once, no `NodeNotFound`).
pub(crate) fn delete_node_row(
    graph: &mut Graph,
    node_id: NodeId,
    detach: bool,
    txn_id: Option<u64>,
    deleted_nodes: &mut std::collections::HashSet<NodeId>,
    stats: &mut GqlMutationResult,
) -> crate::Result<()> {
    if deleted_nodes.contains(&node_id) {
        return Ok(());
    }
    let incident = incident_edge_ids(graph, node_id, txn_id)?;
    if !detach && !incident.is_empty() {
        return Err(crate::Error::DeleteConnectedNode {
            node: node_id,
            relationships: incident.len(),
        });
    }
    if let Some(t) = txn_id {
        // The transactional primitive does not cascade — remove each incident
        // edge before the node so the txn stays consistent.
        for eid in &incident {
            graph.remove_edge_in_txn(t, *eid)?;
            stats.edges_deleted += 1;
        }
        graph.remove_node_in_txn(t, node_id)?;
    } else {
        // Auto-commit `remove_node` cascades incident edges itself; count them
        // here since the primitive does not report the cascade.
        stats.edges_deleted += incident.len() as u64;
        graph.remove_node(node_id)?;
    }
    deleted_nodes.insert(node_id);
    stats.nodes_deleted += 1;
    Ok(())
}

/// Deletes one bound relationship for a `DELETE r` clause, routing to the
/// auto-commit or transactional primitive by `txn_id`. `deleted_edges` makes a
/// repeated reference to the same edge in one clause idempotent (counted once,
/// no `EdgeNotFound`).
pub(crate) fn delete_edge_row(
    graph: &mut Graph,
    edge_id: crate::EdgeId,
    txn_id: Option<u64>,
    deleted_edges: &mut std::collections::HashSet<crate::EdgeId>,
    stats: &mut GqlMutationResult,
) -> crate::Result<()> {
    if deleted_edges.contains(&edge_id) {
        return Ok(());
    }
    match txn_id {
        Some(t) => graph.remove_edge_in_txn(t, edge_id)?,
        None => {
            graph.remove_edge(edge_id)?;
        }
    }
    deleted_edges.insert(edge_id);
    stats.edges_deleted += 1;
    Ok(())
}

/// Applies the `CREATE` write of a `MATCH … CREATE`, once per matched row.
///
/// Creates each node/edge pattern and tallies the counters (nodes, edges,
/// labels, and every inline property as a `properties-set`). Split out of
/// [`apply_match_mutation_body`] to keep each write branch small.
fn apply_match_create(
    graph: &mut Graph,
    create: &gql::CreateClause,
    rows: &[gql::MatchRow],
    txn_id: Option<u64>,
) -> crate::Result<GqlMutationResult> {
    use crate::gql::CreatePattern;

    let mut nodes_created: u64 = 0;
    let mut edges_created: u64 = 0;
    let mut labels_added: u64 = 0;
    let mut properties_set: u64 = 0;

    let empty_pm = PatternMatch::empty();
    for row in rows {
        for pattern in &create.patterns {
            match pattern {
                CreatePattern::Node { label, props, .. } => {
                    let properties = gql::resolve_create_props(props, &empty_pm, &*graph, None);
                    properties_set += u64::try_from(properties.len()).unwrap_or(u64::MAX);
                    match txn_id {
                        Some(t) => graph.add_node_in_txn(t, label, properties)?,
                        None => graph.add_node(label, properties)?,
                    };
                    nodes_created += 1;
                    labels_added += count_label(label);
                }
                CreatePattern::Edge {
                    source_var,
                    rel_label,
                    rel_props,
                    target_var,
                } => {
                    let src = row.nodes.get(source_var).ok_or_else(|| {
                        crate::Error::GqlCompileError(format!(
                            "variable '{source_var}' not bound by MATCH clause"
                        ))
                    })?;
                    let tgt = row.nodes.get(target_var).ok_or_else(|| {
                        crate::Error::GqlCompileError(format!(
                            "variable '{target_var}' not bound by MATCH clause"
                        ))
                    })?;
                    let properties = gql::resolve_create_props(rel_props, &empty_pm, &*graph, None);
                    properties_set += u64::try_from(properties.len()).unwrap_or(u64::MAX);
                    match txn_id {
                        Some(t) => {
                            graph.add_edge_in_txn(t, rel_label.as_str(), *src, *tgt, properties)?
                        }
                        None => graph.add_edge(rel_label.as_str(), *src, *tgt, properties)?,
                    };
                    edges_created += 1;
                }
            }
        }
    }

    Ok(GqlMutationResult {
        nodes_created,
        edges_created,
        properties_set,
        labels_added,
        ..GqlMutationResult::default()
    })
}

/// UNWIND phase 1 (read-only): evaluate the UNWIND list expression and compile
/// the optional MATCH bindings over the read `view`.
///
/// Returns the list elements and the MATCH rows (a single synthetic empty row
/// when there is no MATCH clause). Generic over [`GraphAccess`] so the server
/// runs it under a read lock (auto-commit) or over a `TxnView` (inside a txn).
///
/// # Errors
///
/// Returns [`crate::Error`] if compiling the optional MATCH bindings fails.
pub fn eval_unwind_and_match<G: crate::access::GraphAccess + ?Sized>(
    view: &G,
    mutation: &gql::MutationStatement,
    unwind: &gql::UnwindClause,
    deadline: Option<std::time::Instant>,
) -> crate::Result<UnwindReadPhase> {
    let empty = PatternMatch::empty();
    let list_val = gql::execute_expr(&unwind.expr, &empty, view);

    let elems: Vec<GqlValue> = match list_val {
        GqlValue::List(items) => items,
        GqlValue::Null => vec![],
        other => vec![other],
    };

    let match_rows = if let Some(ref mc) = mutation.match_clause {
        gql::compile_match_bindings(view, mc, deadline)?
    } else {
        vec![HashMap::new()]
    };

    Ok((elems, match_rows))
}

/// UNWIND phase 2 (write): cross-join UNWIND elements with MATCH rows and apply
/// the CREATE patterns for each combination. Honours `txn_id` for every write.
///
/// # Errors
///
/// Returns [`crate::Error`] if a create fails (storage/quota), a
/// `CREATE (n $map)` appears inside the pipeline (unsupported), or an edge
/// pattern references a variable bound by neither MATCH nor a prior CREATE.
// `implicit_hasher`: `rows` come from `compile_match_bindings` (default
// `RandomState`) and are consumed as-is; generalizing the hasher would leak an
// implementation detail of the read phase into this signature for no benefit.
#[allow(clippy::implicit_hasher)]
pub fn apply_unwind_create_body(
    graph: &mut Graph,
    unwind: &gql::UnwindClause,
    create: &gql::CreateClause,
    elements: &[GqlValue],
    rows: &[HashMap<String, NodeId>],
    txn_id: Option<u64>,
) -> crate::Result<GqlMutationResult> {
    use crate::gql::{resolve_create_props, CreatePattern};

    let mut nodes_created: u64 = 0;
    let mut edges_created: u64 = 0;
    let mut labels_added: u64 = 0;
    let mut properties_set: u64 = 0;
    let empty_pm = PatternMatch::empty();

    for elem in elements {
        let unwind_var = Some((unwind.var.as_str(), elem));

        for row in rows {
            // Track nodes created in this (elem, row) pair for edge references.
            let mut created_nodes: HashMap<String, NodeId> = HashMap::new();

            for pattern in &create.patterns {
                match pattern {
                    CreatePattern::Node {
                        var,
                        label,
                        props,
                        prop_map,
                    } => {
                        // `CREATE (n:Label $map)` (whole-entity map source) is not
                        // supported inside an UNWIND pipeline. Fail explicitly
                        // rather than silently dropping the map.
                        if prop_map.is_some() {
                            return Err(crate::Error::GqlCompileError(
                                "CREATE (n:Label $map) is not supported inside an \
                                 UNWIND pipeline"
                                    .to_owned(),
                            ));
                        }
                        let properties =
                            resolve_create_props(props, &empty_pm, &*graph, unwind_var);
                        properties_set += u64::try_from(properties.len()).unwrap_or(u64::MAX);
                        let node_id = match txn_id {
                            Some(t) => graph.add_node_in_txn(t, label, properties)?,
                            None => graph.add_node(label, properties)?,
                        };
                        if let Some(v) = var {
                            created_nodes.insert(v.clone(), node_id);
                        }
                        nodes_created += 1;
                        labels_added += count_label(label);
                    }
                    CreatePattern::Edge {
                        source_var,
                        rel_label,
                        rel_props,
                        target_var,
                    } => {
                        let src = created_nodes
                            .get(source_var)
                            .or_else(|| row.get(source_var))
                            .ok_or_else(|| {
                                crate::Error::GqlCompileError(format!(
                                    "variable '{source_var}' not bound by MATCH or CREATE"
                                ))
                            })?;
                        let tgt = created_nodes
                            .get(target_var)
                            .or_else(|| row.get(target_var))
                            .ok_or_else(|| {
                                crate::Error::GqlCompileError(format!(
                                    "variable '{target_var}' not bound by MATCH or CREATE"
                                ))
                            })?;
                        let properties =
                            resolve_create_props(rel_props, &empty_pm, &*graph, unwind_var);
                        properties_set += u64::try_from(properties.len()).unwrap_or(u64::MAX);
                        match txn_id {
                            Some(t) => graph.add_edge_in_txn(
                                t,
                                rel_label.as_str(),
                                *src,
                                *tgt,
                                properties,
                            )?,
                            None => graph.add_edge(rel_label.as_str(), *src, *tgt, properties)?,
                        };
                        edges_created += 1;
                    }
                }
            }
        }
    }

    Ok(GqlMutationResult {
        nodes_created,
        edges_created,
        properties_set,
        labels_added,
        ..GqlMutationResult::default()
    })
}

/// Executes an `UNWIND … CREATE` mutation end-to-end over a single `&mut Graph`.
///
/// Evaluates the UNWIND list and optional MATCH bindings, then cross-joins them
/// and runs the CREATE patterns. With `txn_id = Some(t)` reads and writes go
/// through transaction `t`; with `None` they are auto-committed. The server's
/// auto-commit path runs [`eval_unwind_and_match`] and [`apply_unwind_create_body`]
/// under separate read/write locks; this entry point is for the embedded engine
/// and the transactional path where both phases share one borrow.
///
/// # Errors
///
/// Returns [`crate::Error`] if invoked without an UNWIND clause, the mutation
/// clause is not `CREATE`, or the read/write phases fail.
pub fn execute_unwind_mutation(
    graph: &mut Graph,
    mutation: &gql::MutationStatement,
    deadline: Option<std::time::Instant>,
    txn_id: Option<u64>,
) -> crate::Result<GqlMutationResult> {
    use crate::gql::MutationClause;

    let unwind = mutation.unwind_clause.as_ref().ok_or_else(|| {
        crate::Error::GqlCompileError(
            "execute_unwind_mutation invoked without an UNWIND clause".to_owned(),
        )
    })?;

    // UNWIND supports CREATE (build per element) and DELETE (remove the nodes a
    // per-element MATCH bound). Other clauses are still unsupported here.
    match &mutation.mutation {
        MutationClause::Create(_) | MutationClause::Delete(_) => {}
        other => {
            return Err(crate::Error::GqlCompileError(format!(
                "mutation clause not yet supported with UNWIND: {other:?}"
            )));
        }
    }

    // Phase 1 — read (over the txn snapshot when present). View dropped before write.
    let (elements, rows) = match txn_id {
        Some(t) => {
            let view = crate::gql::txn_view::TxnView::new(graph, t);
            eval_unwind_and_match(&view, mutation, unwind, deadline)?
        }
        None => eval_unwind_and_match(&*graph, mutation, unwind, deadline)?,
    };

    if elements.is_empty() || rows.is_empty() {
        return Ok(GqlMutationResult::default());
    }

    // Phase 2 — write.
    match &mutation.mutation {
        MutationClause::Create(create) => {
            apply_unwind_create_body(graph, unwind, create, &elements, &rows, txn_id)
        }
        MutationClause::Delete(dc) => apply_unwind_delete_body(graph, &rows, dc, txn_id),
        // Guarded by the match above; unreachable in practice.
        other => Err(crate::Error::GqlCompileError(format!(
            "mutation clause not yet supported with UNWIND: {other:?}"
        ))),
    }
}

/// UNWIND … DELETE write phase: deletes each node the per-element MATCH bound,
/// deduplicated across the whole cross-join so a node matched under several
/// UNWIND elements is deleted (and counted) once.
///
/// The MATCH rows already encode the cross-join of UNWIND elements with matched
/// nodes (`eval_unwind_and_match` compiles the MATCH once; the element value is
/// bound into the pattern predicate during that read phase), so this phase only
/// needs to walk the rows. Edge-variable deletion is not reachable here: the
/// UNWIND read phase binds nodes only.
///
/// # Errors
///
/// [`crate::Error::DeleteConnectedNode`] for a non-detach delete of a connected
/// node, or a storage error from the delete primitive.
#[allow(clippy::implicit_hasher)]
pub fn apply_unwind_delete_body(
    graph: &mut Graph,
    rows: &[HashMap<String, NodeId>],
    dc: &gql::DeleteClause,
    txn_id: Option<u64>,
) -> crate::Result<GqlMutationResult> {
    let mut stats = GqlMutationResult::default();
    let mut deleted_nodes: std::collections::HashSet<NodeId> = std::collections::HashSet::new();
    for row in rows {
        for var in &dc.vars {
            let node_id = row.get(var).copied().ok_or_else(|| {
                crate::Error::GqlCompileError(format!("variable '{var}' not bound by MATCH clause"))
            })?;
            delete_node_row(
                graph,
                node_id,
                dc.detach,
                txn_id,
                &mut deleted_nodes,
                &mut stats,
            )?;
        }
    }
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::execute_bare_mutation;
    use crate::gql::{self, GqlStatement, MutationStatement};
    use crate::{Graph, Properties};

    fn parse_mutation(input: &str) -> MutationStatement {
        match gql::parse_statement(input).expect("parse failed") {
            GqlStatement::Mutation(m) => m,
            other => panic!("expected mutation, got {other:?}"),
        }
    }

    #[test]
    fn execute_unwind_delete_dedups_across_cross_join() {
        let mut g = Graph::new();
        // Two Person nodes, no edges.
        g.add_node("Person", Properties::new()).unwrap();
        g.add_node("Person", Properties::new()).unwrap();
        // The cross-join binds each Person once per UNWIND element (2 elements ×
        // 2 nodes = 4 rows). Dedup must delete each real node exactly once.
        let stmt = parse_mutation("UNWIND [1, 2] AS x MATCH (n:Person) DELETE n");
        let stats = super::execute_unwind_mutation(&mut g, &stmt, None, None).unwrap();
        assert_eq!(
            stats.nodes_deleted, 2,
            "each node deleted once despite 4 rows"
        );
        assert_eq!(g.node_count(), 0);
    }

    #[test]
    fn parser_rejects_bare_delete_without_match() {
        // The parser is the first guard: a bare DELETE never produces an AST.
        let err = gql::parse_statement("DELETE n").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("DELETE"), "message: {msg}");
        assert!(msg.contains("MATCH"), "message: {msg}");
    }

    #[test]
    fn execute_bare_delete_without_bindings_errors_clearly() {
        // Defence in depth: even if a caller hands the executor a bare-DELETE
        // statement directly (embedded API, bypassing the parser), it is
        // rejected with a clear message rather than mis-executed.
        let mut g = Graph::new();
        let stmt = MutationStatement {
            unwind_clause: None,
            match_clause: None,
            where_clause: None,
            mutation: crate::gql::MutationClause::Delete(crate::gql::DeleteClause {
                detach: false,
                vars: vec!["n".into()],
            }),
            set_clause: None,
            return_clause: None,
        };
        let err = execute_bare_mutation(&mut g, &stmt, &HashMap::new(), None).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("DELETE"), "message: {msg}");
        assert!(
            msg.contains("MATCH") || msg.contains("bound"),
            "message should explain a binding is required: {msg}"
        );
    }

    #[test]
    fn execute_bare_mutation_autocommit_creates_node() {
        let mut g = Graph::new();
        let stmt = parse_mutation("CREATE (n:Person)");
        let (rows, stats) = execute_bare_mutation(&mut g, &stmt, &HashMap::new(), None).unwrap();
        assert_eq!(stats.nodes_created, 1);
        assert_eq!(stats.edges_created, 0);
        assert_eq!(rows.len(), 0);
        assert_eq!(g.nodes_by_label("Person").len(), 1);
    }

    #[test]
    fn execute_bare_mutation_return_projects_created_node() {
        let mut g = Graph::new();
        let stmt = parse_mutation("CREATE (n:Person {name: 'Ada'}) RETURN n");
        let (rows, stats) = execute_bare_mutation(&mut g, &stmt, &HashMap::new(), None).unwrap();
        assert_eq!(stats.nodes_created, 1);
        assert_eq!(rows.len(), 1);
        assert!(rows[0].contains_key("n"));
    }

    #[test]
    fn execute_bare_mutation_in_txn_writes_pending_not_autocommit() {
        let mut g = Graph::new();
        g.enable_mvcc();
        let txn = g.begin_txn().unwrap();
        let stmt = parse_mutation("CREATE (n:Person) RETURN n");
        let (rows, stats) =
            execute_bare_mutation(&mut g, &stmt, &HashMap::new(), Some(txn)).unwrap();
        assert_eq!(stats.nodes_created, 1);
        assert_eq!(rows.len(), 1);
        // The pending write is invisible to auto-commit readers before COMMIT.
        assert_eq!(g.nodes_by_label("Person").len(), 0);
        g.commit_txn(txn).unwrap();
        assert_eq!(g.nodes_by_label("Person").len(), 1);
    }

    #[test]
    fn execute_bare_mutation_bare_edge_rejected() {
        let mut g = Graph::new();
        let stmt = parse_mutation("CREATE (a:Person)-[:KNOWS]->(b:Person)");
        let result = execute_bare_mutation(&mut g, &stmt, &HashMap::new(), None);
        assert!(result.is_err());
    }

    #[test]
    fn execute_bare_mutation_counts_labels_added_for_labeled_node() {
        let mut g = Graph::new();
        let stmt = parse_mutation("CREATE (n:Person {name: 'Alice'})");
        let (_rows, stats) = execute_bare_mutation(&mut g, &stmt, &HashMap::new(), None).unwrap();
        assert_eq!(stats.nodes_created, 1);
        assert_eq!(stats.labels_added, 1);
    }

    #[test]
    fn execute_bare_mutation_counts_labels_added_for_two_labeled_nodes() {
        let mut g = Graph::new();
        let stmt = parse_mutation("CREATE (:Person), (:City)");
        let (_rows, stats) = execute_bare_mutation(&mut g, &stmt, &HashMap::new(), None).unwrap();
        assert_eq!(stats.nodes_created, 2);
        assert_eq!(stats.labels_added, 2);
    }

    // ── Cycle 14: MATCH … CREATE / MATCH … SET write phase ────────────────────

    #[test]
    fn apply_match_mutation_body_autocommit_creates_edge_per_matched_row() {
        let mut g = Graph::new();
        let a = g.add_node("Base", Properties::new()).unwrap();
        let b = g.add_node("Base", Properties::new()).unwrap();
        // MATCH binds both Base nodes; CREATE adds a Tagged node per row.
        let stmt = parse_mutation("MATCH (n:Base) CREATE (x:Tagged)");
        let mc = stmt.match_clause.as_ref().unwrap();
        let rows = gql::compile_match_rows(&g, mc, None).unwrap();
        let (_rows, stats) =
            super::apply_match_mutation_body(&mut g, &stmt, &rows, &HashMap::new(), None).unwrap();
        assert_eq!(stats.nodes_created, 2, "one Tagged per matched Base");
        assert_eq!(stats.edges_created, 0);
        let _ = (a, b);
        assert_eq!(g.nodes_by_label("Tagged").len(), 2);
    }

    #[test]
    fn apply_match_mutation_body_create_counts_labels_added() {
        let mut g = Graph::new();
        g.add_node("Base", Properties::new()).unwrap();
        g.add_node("Base", Properties::new()).unwrap();
        // MATCH binds both Base nodes; CREATE adds one labelled Tagged node per
        // matched row, so labels-added tracks nodes-created here.
        let stmt = parse_mutation("MATCH (n:Base) CREATE (x:Tagged)");
        let mc = stmt.match_clause.as_ref().unwrap();
        let rows = gql::compile_match_rows(&g, mc, None).unwrap();
        let (_rows, stats) =
            super::apply_match_mutation_body(&mut g, &stmt, &rows, &HashMap::new(), None).unwrap();
        assert_eq!(stats.nodes_created, 2);
        assert_eq!(stats.labels_added, 2, "one Tagged label per matched Base");
    }

    #[test]
    fn apply_match_mutation_body_set_counts_properties_set() {
        let mut g = Graph::new();
        g.add_node("Person", Properties::new()).unwrap();
        g.add_node("Person", Properties::new()).unwrap();
        let stmt = parse_mutation("MATCH (n:Person) SET n.age = 30");
        let mc = stmt.match_clause.as_ref().unwrap();
        let rows = gql::compile_match_rows(&g, mc, None).unwrap();
        let (_rows, stats) =
            super::apply_match_mutation_body(&mut g, &stmt, &rows, &HashMap::new(), None).unwrap();
        assert_eq!(stats.properties_set, 2, "one assignment per matched row");
    }

    #[test]
    fn apply_match_mutation_body_in_txn_writes_pending_not_autocommit() {
        let mut g = Graph::new();
        g.enable_mvcc();
        let base = g.add_node("Base", Properties::new()).unwrap();
        let _ = base;
        let txn = g.begin_txn().unwrap();
        let stmt = parse_mutation("MATCH (n:Base) CREATE (x:Tagged)");
        let mc = stmt.match_clause.as_ref().unwrap();
        // Read phase over the txn view sees committed Base. Scoped so the
        // view's `&mut g` borrow ends before the write phase reborrows it.
        let rows = {
            let view = crate::gql::txn_view::TxnView::new(&mut g, txn);
            gql::compile_match_rows(&view, mc, None).unwrap()
        };
        let (_r, stats) =
            super::apply_match_mutation_body(&mut g, &stmt, &rows, &HashMap::new(), Some(txn))
                .unwrap();
        assert_eq!(stats.nodes_created, 1);
        // Pending: not visible to auto-commit before COMMIT.
        assert_eq!(g.nodes_by_label("Tagged").len(), 0);
        g.commit_txn(txn).unwrap();
        assert_eq!(g.nodes_by_label("Tagged").len(), 1);
    }

    // ── Issue #45: DELETE / DETACH DELETE ─────────────────────────────────────

    #[test]
    fn apply_match_mutation_body_delete_removes_isolated_node() {
        let mut g = Graph::new();
        g.add_node("Person", Properties::new()).unwrap();
        let stmt = parse_mutation("MATCH (n:Person) DELETE n");
        let mc = stmt.match_clause.as_ref().unwrap();
        let rows = gql::compile_match_rows(&g, mc, None).unwrap();
        let (_r, stats) =
            super::apply_match_mutation_body(&mut g, &stmt, &rows, &HashMap::new(), None).unwrap();
        assert_eq!(stats.nodes_deleted, 1);
        assert_eq!(stats.edges_deleted, 0);
        assert_eq!(g.node_count(), 0);
    }

    #[test]
    fn delete_connected_node_without_detach_errors() {
        let mut g = Graph::new();
        let a = g.add_node("Person", Properties::new()).unwrap();
        let b = g.add_node("Person", Properties::new()).unwrap();
        g.add_edge("KNOWS", a, b, Properties::new()).unwrap();
        // DELETE (no DETACH) on `a`, which has an outgoing edge → error.
        let stmt = parse_mutation("MATCH (n:Person) DELETE n");
        let mc = stmt.match_clause.as_ref().unwrap();
        let rows = gql::compile_match_rows(&g, mc, None).unwrap();
        let err = super::apply_match_mutation_body(&mut g, &stmt, &rows, &HashMap::new(), None)
            .unwrap_err();
        assert!(
            matches!(err, crate::Error::DeleteConnectedNode { .. }),
            "expected DeleteConnectedNode, got {err:?}"
        );
        // No partial deletion: both nodes and the edge survive.
        assert_eq!(g.node_count(), 2);
        assert_eq!(g.edge_count(), 1);
    }

    #[test]
    fn detach_delete_removes_node_and_incident_edges() {
        let mut g = Graph::new();
        let a = g.add_node("Solo", Properties::new()).unwrap();
        let b = g.add_node("Other", Properties::new()).unwrap();
        g.add_edge("KNOWS", a, b, Properties::new()).unwrap();
        // DETACH DELETE only the `Solo` node; its incident edge goes with it.
        let stmt = parse_mutation("MATCH (n:Solo) DETACH DELETE n");
        let mc = stmt.match_clause.as_ref().unwrap();
        let rows = gql::compile_match_rows(&g, mc, None).unwrap();
        let (_r, stats) =
            super::apply_match_mutation_body(&mut g, &stmt, &rows, &HashMap::new(), None).unwrap();
        assert_eq!(stats.nodes_deleted, 1);
        assert_eq!(stats.edges_deleted, 1);
        assert_eq!(g.node_count(), 1, "the Other node survives");
        assert_eq!(g.edge_count(), 0);
    }

    #[test]
    fn delete_node_in_txn_isolated_until_commit() {
        // Visibility, not the O(1) meta counter, is the post-commit criterion:
        // under MVCC a delete's category-B baja (counter decrement, exists-set
        // removal, page tombstone) is the vacuum's job, so `node_count()` lags
        // until vacuum. What must be immediate is snapshot visibility — matching
        // the engine's own `remove_node_in_txn` tests.
        let mut g = Graph::new();
        g.enable_mvcc();
        let a = g.add_node("Solo", Properties::new()).unwrap();
        let b = g.add_node("Other", Properties::new()).unwrap();
        let e = g.add_edge("KNOWS", a, b, Properties::new()).unwrap();
        let txn = g.begin_txn().unwrap();
        let stmt = parse_mutation("MATCH (n:Solo) DETACH DELETE n");
        let mc = stmt.match_clause.as_ref().unwrap();
        let rows = {
            let view = crate::gql::txn_view::TxnView::new(&mut g, txn);
            gql::compile_match_rows(&view, mc, None).unwrap()
        };
        let (_r, stats) =
            super::apply_match_mutation_body(&mut g, &stmt, &rows, &HashMap::new(), Some(txn))
                .unwrap();
        assert_eq!(stats.nodes_deleted, 1);
        assert_eq!(stats.edges_deleted, 1);
        // Pending: a fresh auto-commit read still sees the node and edge.
        assert!(g.node(a).is_ok(), "node visible before commit");
        assert!(g.edge(e).is_ok(), "edge visible before commit");
        g.commit_txn(txn).unwrap();
        // Committed: the node and its edge are gone from the visible snapshot;
        // the surviving `Other` node remains.
        assert!(
            matches!(g.node(a), Err(crate::Error::NodeNotFound(_))),
            "deleted node hidden after commit"
        );
        assert!(
            matches!(g.edge(e), Err(crate::Error::EdgeNotFound(_))),
            "cascaded edge hidden after commit"
        );
        assert!(g.node(b).is_ok(), "the Other node survives");
    }

    #[test]
    fn delete_node_in_txn_rollback_restores() {
        let mut g = Graph::new();
        g.enable_mvcc();
        let a = g.add_node("Solo", Properties::new()).unwrap();
        let b = g.add_node("Other", Properties::new()).unwrap();
        let e = g.add_edge("KNOWS", a, b, Properties::new()).unwrap();
        let txn = g.begin_txn().unwrap();
        let stmt = parse_mutation("MATCH (n:Solo) DETACH DELETE n");
        let mc = stmt.match_clause.as_ref().unwrap();
        let rows = {
            let view = crate::gql::txn_view::TxnView::new(&mut g, txn);
            gql::compile_match_rows(&view, mc, None).unwrap()
        };
        super::apply_match_mutation_body(&mut g, &stmt, &rows, &HashMap::new(), Some(txn)).unwrap();
        g.rollback_txn(txn).unwrap();
        assert!(g.node(a).is_ok(), "rollback restores the node");
        assert!(g.edge(e).is_ok(), "rollback restores the edge");
        let _ = b;
    }

    #[test]
    fn delete_edge_by_match_variable() {
        let mut g = Graph::new();
        let a = g.add_node("A", Properties::new()).unwrap();
        let b = g.add_node("B", Properties::new()).unwrap();
        g.add_edge("KNOWS", a, b, Properties::new()).unwrap();
        // Bind the relationship `r` and delete it; both endpoints survive.
        let stmt = parse_mutation("MATCH (a)-[r:KNOWS]->(b) DELETE r");
        let mc = stmt.match_clause.as_ref().unwrap();
        let rows = gql::compile_match_rows(&g, mc, None).unwrap();
        let (_r, stats) =
            super::apply_match_mutation_body(&mut g, &stmt, &rows, &HashMap::new(), None).unwrap();
        assert_eq!(stats.edges_deleted, 1);
        assert_eq!(stats.nodes_deleted, 0);
        assert_eq!(g.node_count(), 2, "both endpoints survive");
        assert_eq!(g.edge_count(), 0);
    }

    #[test]
    fn compile_match_rows_includes_edge_variables() {
        let mut g = Graph::new();
        let a = g.add_node("A", Properties::new()).unwrap();
        let b = g.add_node("B", Properties::new()).unwrap();
        let e = g.add_edge("KNOWS", a, b, Properties::new()).unwrap();
        let stmt = parse_mutation("MATCH (a)-[r:KNOWS]->(b) DELETE r");
        let mc = stmt.match_clause.as_ref().unwrap();
        let rows = gql::compile_match_rows(&g, mc, None).unwrap();
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.nodes.get("a"), Some(&a));
        assert_eq!(row.nodes.get("b"), Some(&b));
        assert_eq!(row.edges.get("r"), Some(&e), "edge variable resolved");
    }

    #[test]
    fn delete_same_node_twice_in_one_query_counts_once() {
        let mut g = Graph::new();
        g.add_node("Person", Properties::new()).unwrap();
        // Both `n` and `m` bind the single Person; DELETE n, m must not error
        // on the second reference and must count the node once.
        let stmt = parse_mutation("MATCH (n:Person), (m:Person) DELETE n, m");
        let mc = stmt.match_clause.as_ref().unwrap();
        let rows = gql::compile_match_rows(&g, mc, None).unwrap();
        let (_r, stats) =
            super::apply_match_mutation_body(&mut g, &stmt, &rows, &HashMap::new(), None).unwrap();
        assert_eq!(stats.nodes_deleted, 1, "one real node deleted once");
        assert_eq!(g.node_count(), 0);
    }

    // ── Cycle 15: MERGE ───────────────────────────────────────────────────────

    fn parse_merge(input: &str) -> crate::gql::MergeClause {
        match parse_mutation(input).mutation {
            crate::gql::MutationClause::Merge(m) => m,
            other => panic!("expected MERGE, got {other:?}"),
        }
    }

    #[test]
    fn execute_bare_merge_autocommit_creates_then_matches() {
        let mut g = Graph::new();
        let m = parse_merge("MERGE (n:Person {id: 1}) ON CREATE SET n.created = true");
        // First run creates.
        let (_r, stats) = super::execute_bare_merge(&mut g, &m, &HashMap::new(), None).unwrap();
        assert_eq!(stats.nodes_created, 1);
        // Second run matches the existing node — nothing created.
        let (_r2, stats2) = super::execute_bare_merge(&mut g, &m, &HashMap::new(), None).unwrap();
        assert_eq!(stats2.nodes_created, 0);
        assert_eq!(g.nodes_by_label("Person").len(), 1);
    }

    #[test]
    fn execute_bare_merge_create_branch_counts_labels_added() {
        let mut g = Graph::new();
        let m = parse_merge("MERGE (n:AssetNode {id: 'x'})");
        let (_r, stats) = super::execute_bare_merge(&mut g, &m, &HashMap::new(), None).unwrap();
        assert_eq!(stats.nodes_created, 1);
        assert_eq!(stats.labels_added, 1);
        assert!(stats.contains_updates());
    }

    #[test]
    fn execute_bare_merge_match_branch_reports_no_updates() {
        let mut g = Graph::new();
        let m = parse_merge("MERGE (n:AssetNode {id: 'x'})");
        // First MERGE creates it.
        super::execute_bare_merge(&mut g, &m, &HashMap::new(), None).unwrap();
        // Second MERGE matches the existing node with no ON MATCH SET.
        let (_r, stats) = super::execute_bare_merge(&mut g, &m, &HashMap::new(), None).unwrap();
        assert_eq!(stats.nodes_created, 0);
        assert_eq!(stats.labels_added, 0);
        assert_eq!(stats.properties_set, 0);
        assert!(!stats.contains_updates());
    }

    #[test]
    fn execute_bare_merge_in_txn_second_merge_finds_own_pending_node() {
        let mut g = Graph::new();
        g.enable_mvcc();
        let txn = g.begin_txn().unwrap();
        let m = parse_merge("MERGE (n:Person {id: 1})");
        // First MERGE in the txn creates a pending node.
        let (_r, stats) =
            super::execute_bare_merge(&mut g, &m, &HashMap::new(), Some(txn)).unwrap();
        assert_eq!(stats.nodes_created, 1);
        // Second MERGE in the SAME txn must find its own pending node by the
        // indexed property lookup — via the txn view's enumeration, not the
        // committed index — so it matches instead of creating a duplicate.
        let (_r2, stats2) =
            super::execute_bare_merge(&mut g, &m, &HashMap::new(), Some(txn)).unwrap();
        assert_eq!(
            stats2.nodes_created, 0,
            "second MERGE must match the txn's own pending node"
        );
        // Still invisible to auto-commit before COMMIT.
        assert_eq!(g.nodes_by_label("Person").len(), 0);
        g.commit_txn(txn).unwrap();
        assert_eq!(g.nodes_by_label("Person").len(), 1);
    }

    // ── Cycle 16: UNWIND … CREATE ─────────────────────────────────────────────

    #[test]
    fn execute_unwind_mutation_autocommit_creates_one_node_per_element() {
        let mut g = Graph::new();
        let stmt = parse_mutation("UNWIND [1, 2, 3] AS x CREATE (n:N {v: x})");
        let stats = super::execute_unwind_mutation(&mut g, &stmt, None, None).unwrap();
        assert_eq!(stats.nodes_created, 3);
        assert_eq!(stats.edges_created, 0);
        assert_eq!(g.nodes_by_label("N").len(), 3);
    }

    #[test]
    fn execute_unwind_mutation_counts_labels_added_per_element() {
        let mut g = Graph::new();
        let stmt = parse_mutation("UNWIND [1, 2, 3] AS x CREATE (:Item {v: x})");
        let stats = super::execute_unwind_mutation(&mut g, &stmt, None, None).unwrap();
        assert_eq!(stats.nodes_created, 3);
        assert_eq!(stats.labels_added, 3);
    }

    #[test]
    fn execute_unwind_mutation_in_txn_pending_visible_by_enumeration() {
        let mut g = Graph::new();
        g.enable_mvcc();
        let txn = g.begin_txn().unwrap();
        let stmt = parse_mutation("UNWIND [1, 2] AS x CREATE (n:N {v: x})");
        let stats = super::execute_unwind_mutation(&mut g, &stmt, None, Some(txn)).unwrap();
        assert_eq!(stats.nodes_created, 2);
        // Pending nodes are enumerable within the txn but invisible to auto-commit.
        assert_eq!(g.nodes_by_label_in_txn(txn, "N").unwrap().len(), 2);
        assert_eq!(g.nodes_by_label("N").len(), 0);
        g.commit_txn(txn).unwrap();
        assert_eq!(g.nodes_by_label("N").len(), 2);
    }
}
