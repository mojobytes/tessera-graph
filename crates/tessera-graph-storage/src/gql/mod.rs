//! GQL execution — enterprise-only.
//!
//! Provides:
//! - `execute_mut` for executing GQL mutation statements (CREATE, DELETE,
//!   SET, MERGE) against any mutable `GraphAccess` implementation.
//! - `execute_query` for read-only queries, with an optimized enterprise
//!   traversal engine for variable-hop patterns and shortestPath.
//! - `needs_optimized_execution` to classify queries that benefit from the
//!   enterprise optimized traversal engine.
//!
//! Read-only query execution for non-optimized queries delegates to the MIT
//! core (`tessera_graph::gql::execute`).

use std::collections::{HashMap, HashSet, VecDeque};

// `eval_set_value` is deprecated upstream (MIT core 0.5.0). The enterprise
// SET execution path will be migrated to `apply_pipeline_set` (which
// evaluates full Expr trees against a Binding context) in Phase 2 of the
// 0.5.0 sync — see `.private/migration-plan-mit-core-0.5.0.md` §4. Until
// then, the literal-only helper still produces correct results for every
// SET statement the enterprise pipeline emits.
#[allow(deprecated)]
use tessera_graph::gql::eval_set_value;
use tessera_graph::gql::{
    CreatePattern, DeleteClause, EdgeLength, Expr, GqlQuery, GqlResult, GqlRow, GqlValue,
    Literal, MergeClause, MutationClause, MutationStatement, SetClause, compile_match_for_mutation,
    literal_to_property, resolve_create_props,
};
use tessera_graph::{Direction, Error, GqlMutationResult, GraphAccess, NodeId, Property};

use crate::cache::NeighborCache;
use crate::shared_cache::{ClearanceKey, SharedNeighborCache};

// ── Query Classifier ────────────────────────────────────────────────────────

/// Returns `true` if the query contains patterns that benefit from the
/// enterprise optimized execution path:
///
/// - Variable-length edge patterns (`-[*1..3]->`)
/// - `shortestPath(a, b)` function calls in RETURN
///
/// Queries that return `false` are delegated to the MIT core engine.
///
/// # Limitations
///
/// This classifier only inspects:
/// - Edge patterns in MATCH for variable-length hops
/// - RETURN items for `shortestPath` function calls
///
/// It does NOT inspect WHERE clauses or nested expressions. Queries with
/// WHERE are always delegated to MIT core by `execute_query`. If
/// `shortestPath` appears outside of RETURN items, it will not be detected.
#[must_use]
pub fn needs_optimized_execution(query: &GqlQuery) -> bool {
    for pattern in &query.match_clause.patterns {
        for (edge, _node) in &pattern.hops {
            if matches!(edge.length, EdgeLength::Variable { .. }) {
                return true;
            }
        }
    }

    for item in &query.return_clause.items {
        if has_shortest_path_call(&item.expr) {
            return true;
        }
    }

    false
}

/// Returns `true` if any RETURN item is a bare variable reference (`Expr::Var`).
/// Bare variables cannot be projected correctly by the optimized engine (no
/// property keys), so these queries must be delegated to the MIT core.
fn has_bare_var_return(query: &GqlQuery) -> bool {
    query
        .return_clause
        .items
        .iter()
        .any(|item| matches!(&item.expr, Expr::Var(_)))
}

fn has_shortest_path_call(expr: &Expr) -> bool {
    matches!(expr, Expr::FunctionCall { name, .. } if name == "shortestpath")
}

// ── Optimized Query Execution ────────────────────────────────────────────────

/// Executes a GQL read-only query, using the enterprise optimized engine for
/// variable-hop traversals and shortestPath, delegating everything else to the
/// MIT core engine.
///
/// # Errors
///
/// Returns errors from the MIT core engine or from the optimized traversal.
pub fn execute_query<G: GraphAccess + ?Sized>(
    graph: &G,
    query: &GqlQuery,
) -> tessera_graph::Result<GqlResult> {
    // Delegate to MIT core for non-optimized queries, queries with WHERE
    // (WHERE requires expression evaluation not available in this crate),
    // or queries with bare Var in RETURN (Expr::Var produces incomplete
    // results in the optimized path — no property keys).
    if !needs_optimized_execution(query)
        || query.where_clause.is_some()
        || has_bare_var_return(query)
    {
        return tessera_graph::gql::execute(graph, query);
    }

    // Check if this is a shortestPath query
    if let Some(result) = try_execute_shortest_path(graph, query)? {
        return Ok(result);
    }

    execute_variable_hop_query(graph, query, &|n, d| neighbor_ids_uncached(graph, n, d))
}

/// Executes a GQL read-only query using the enterprise `NeighborCache`.
///
/// Same semantics as `execute_query` but BFS/DFS neighbor resolution uses
/// the cache instead of full `Edge` deserialization.
///
/// # Errors
///
/// Returns errors from the MIT core engine or from the optimized traversal.
pub fn execute_query_cached<G: GraphAccess>(
    graph: &G,
    cache: &NeighborCache<G>,
    query: &GqlQuery,
) -> tessera_graph::Result<GqlResult> {
    if !needs_optimized_execution(query)
        || query.where_clause.is_some()
        || has_bare_var_return(query)
    {
        return tessera_graph::gql::execute(graph, query);
    }

    if let Some(result) = try_execute_shortest_path_cached(graph, cache, query)? {
        return Ok(result);
    }

    execute_variable_hop_query(
        graph,
        query,
        &|n, d| neighbor_ids_for_direction(graph, Some(cache), n, d),
    )
}

/// Executes a GQL read-only query using the thread-safe `SharedNeighborCache`.
///
/// LBAC-safe: the `clearance` is used to build a `ClearanceKey` that scopes
/// cache entries. The `graph` parameter must be a `SecureGraphRef` (or
/// equivalent LBAC-filtered `GraphAccess` implementation) so that neighbor
/// lists are populated with LBAC-filtered results.
///
/// # Errors
///
/// Returns errors from the MIT core engine or from the optimized traversal.
pub fn execute_query_with_shared_cache<G: GraphAccess + ?Sized>(
    graph: &G,
    cache: &SharedNeighborCache,
    clearance: &tessera_graph_auth::lbac::Clearance,
    query: &GqlQuery,
) -> tessera_graph::Result<GqlResult> {
    if !needs_optimized_execution(query)
        || query.where_clause.is_some()
        || has_bare_var_return(query)
    {
        return tessera_graph::gql::execute(graph, query);
    }

    let ck = ClearanceKey::from(clearance);

    if let Some(result) = try_execute_shortest_path_shared(graph, cache, &ck, query)? {
        return Ok(result);
    }

    execute_variable_hop_query(
        graph,
        query,
        &|n, d| shared_neighbor_ids_for_direction(graph, cache, &ck, n, d),
    )
}

// ── Bidirectional BFS shortestPath ───────────────────────────────────────────

/// Attempts to execute a shortestPath query using bidirectional BFS.
///
/// Returns `Ok(Some(result))` if the query contains a shortestPath call,
/// `Ok(None)` if it doesn't (caller should try other execution paths).
#[allow(clippy::unnecessary_wraps)] // Result needed for consistency with execute_query call site
fn try_execute_shortest_path<G: GraphAccess + ?Sized>(
    graph: &G,
    query: &GqlQuery,
) -> tessera_graph::Result<Option<GqlResult>> {
    // Find shortestPath(a, b) in RETURN items
    let sp_item = query.return_clause.items.iter().find(|item| {
        matches!(&item.expr, Expr::FunctionCall { name, .. } if name == "shortestpath")
    });

    let Some(sp_item) = sp_item else {
        return Ok(None);
    };

    let Expr::FunctionCall { args, .. } = &sp_item.expr else {
        return Ok(None);
    };

    // Extract the two variable names from shortestPath(a, b)
    let (Some(Expr::Var(from_var)), Some(Expr::Var(to_var))) = (args.first(), args.get(1)) else {
        return Ok(None);
    };

    // Resolve from/to node IDs by scanning MATCH clause patterns for variable bindings.
    let from_ids = resolve_var_ids(graph, from_var, query);
    let to_ids = resolve_var_ids(graph, to_var, query);

    let col_name = sp_item
        .alias
        .as_deref()
        .map_or_else(|| expr_surface_name(&sp_item.expr), String::from);

    if from_ids.is_empty() || to_ids.is_empty() {
        // No matching nodes — produce a single row with Null
        let mut row = HashMap::with_capacity(1);
        row.insert(col_name, GqlValue::Null);
        return Ok(Some(vec![row]));
    }

    // Run bidirectional BFS for each (from, to) pair
    let mut results: GqlResult = Vec::new();

    for &from_id in &from_ids {
        for &to_id in &to_ids {
            let path = bidirectional_bfs(
                from_id,
                to_id,
                &|n| Ok(graph.outgoing_edges(n)?.iter().map(tessera_graph::Edge::target).collect()),
                &|n| Ok(graph.incoming_edges(n)?.iter().map(tessera_graph::Edge::source).collect()),
            );

            #[allow(clippy::cast_possible_wrap)]
            let value = path.map_or(GqlValue::Null, |ids| {
                GqlValue::List(
                    ids.into_iter()
                        .map(|nid| GqlValue::Int(nid.as_u64() as i64))
                        .collect(),
                )
            });

            let mut row = HashMap::with_capacity(1);
            row.insert(col_name.clone(), value);
            results.push(row);
        }
    }

    Ok(Some(results))
}

/// Cached variant of `try_execute_shortest_path` — uses `NeighborCache` for
/// BFS neighbor resolution instead of full `Edge` deserialization.
#[allow(clippy::unnecessary_wraps)]
fn try_execute_shortest_path_cached<G: GraphAccess>(
    graph: &G,
    cache: &NeighborCache<G>,
    query: &GqlQuery,
) -> tessera_graph::Result<Option<GqlResult>> {
    let sp_item = query.return_clause.items.iter().find(|item| {
        matches!(&item.expr, Expr::FunctionCall { name, .. } if name == "shortestpath")
    });

    let Some(sp_item) = sp_item else {
        return Ok(None);
    };

    let Expr::FunctionCall { args, .. } = &sp_item.expr else {
        return Ok(None);
    };

    let (Some(Expr::Var(from_var)), Some(Expr::Var(to_var))) = (args.first(), args.get(1)) else {
        return Ok(None);
    };

    let from_ids = resolve_var_ids(graph, from_var, query);
    let to_ids = resolve_var_ids(graph, to_var, query);

    let col_name = sp_item
        .alias
        .as_deref()
        .map_or_else(|| expr_surface_name(&sp_item.expr), String::from);

    if from_ids.is_empty() || to_ids.is_empty() {
        let mut row = HashMap::with_capacity(1);
        row.insert(col_name, GqlValue::Null);
        return Ok(Some(vec![row]));
    }

    let mut results: GqlResult = Vec::new();

    for &from_id in &from_ids {
        for &to_id in &to_ids {
            let path = bidirectional_bfs(
                from_id,
                to_id,
                &|n| cache.outgoing_neighbor_ids(n),
                &|n| cache.incoming_neighbor_ids(n),
            );

            #[allow(clippy::cast_possible_wrap)]
            let value = path.map_or(GqlValue::Null, |ids| {
                GqlValue::List(
                    ids.into_iter()
                        .map(|nid| GqlValue::Int(nid.as_u64() as i64))
                        .collect(),
                )
            });

            let mut row = HashMap::with_capacity(1);
            row.insert(col_name.clone(), value);
            results.push(row);
        }
    }

    Ok(Some(results))
}

/// `SharedNeighborCache` variant of shortest path execution.
#[allow(clippy::unnecessary_wraps)]
fn try_execute_shortest_path_shared<G: GraphAccess + ?Sized>(
    graph: &G,
    cache: &SharedNeighborCache,
    ck: &ClearanceKey,
    query: &GqlQuery,
) -> tessera_graph::Result<Option<GqlResult>> {
    let sp_item = query.return_clause.items.iter().find(|item| {
        matches!(&item.expr, Expr::FunctionCall { name, .. } if name == "shortestpath")
    });

    let Some(sp_item) = sp_item else {
        return Ok(None);
    };

    let Expr::FunctionCall { args, .. } = &sp_item.expr else {
        return Ok(None);
    };

    let (Some(Expr::Var(from_var)), Some(Expr::Var(to_var))) = (args.first(), args.get(1)) else {
        return Ok(None);
    };

    let from_ids = resolve_var_ids(graph, from_var, query);
    let to_ids = resolve_var_ids(graph, to_var, query);

    let col_name = sp_item
        .alias
        .as_deref()
        .map_or_else(|| expr_surface_name(&sp_item.expr), String::from);

    if from_ids.is_empty() || to_ids.is_empty() {
        let mut row = HashMap::with_capacity(1);
        row.insert(col_name, GqlValue::Null);
        return Ok(Some(vec![row]));
    }

    let mut results: GqlResult = Vec::new();

    for &from_id in &from_ids {
        for &to_id in &to_ids {
            let path = bidirectional_bfs(
                from_id,
                to_id,
                &|n| cache.outgoing_neighbor_ids(graph, n, ck),
                &|n| cache.incoming_neighbor_ids(graph, n, ck),
            );

            #[allow(clippy::cast_possible_wrap)]
            let value = path.map_or(GqlValue::Null, |ids| {
                GqlValue::List(
                    ids.into_iter()
                        .map(|nid| GqlValue::Int(nid.as_u64() as i64))
                        .collect(),
                )
            });

            let mut row = HashMap::with_capacity(1);
            row.insert(col_name.clone(), value);
            results.push(row);
        }
    }

    Ok(Some(results))
}

/// Resolves a variable name to matching node IDs by scanning MATCH clause
/// patterns for node bindings with that variable name.
fn resolve_var_ids<G: GraphAccess + ?Sized>(
    graph: &G,
    var_name: &str,
    query: &GqlQuery,
) -> Vec<NodeId> {
    for pattern in &query.match_clause.patterns {
        // Check start node
        if pattern.start.var.as_deref() == Some(var_name) {
            let labels = &pattern.start.labels;
            let props = &pattern.start.props;
            return resolve_by_label_props(graph, labels, props);
        }
        // Check hop end nodes
        for (_ep, np) in &pattern.hops {
            if np.var.as_deref() == Some(var_name) {
                return resolve_by_label_props(graph, &np.labels, &np.props);
            }
        }
    }
    Vec::new()
}

/// Finds node IDs matching a label + inline property constraints.
fn resolve_by_label_props<G: GraphAccess + ?Sized>(
    graph: &G,
    labels: &[String],
    props: &[(String, Literal)],
) -> Vec<NodeId> {
    let Some(label) = labels.first() else {
        return Vec::new();
    };
    let candidates = graph.nodes_by_label(label);
    if props.is_empty() {
        return candidates;
    }
    let prop_filters: Vec<(String, Property)> = props
        .iter()
        .filter_map(|(k, v)| literal_to_property(v).map(|p| (k.clone(), p)))
        .collect();
    candidates
        .into_iter()
        .filter(|&id| {
            let Ok(node) = graph.node(id) else { return false };
            prop_filters.iter().all(|(key, expected)| {
                node.properties().get(key).is_some_and(|actual| actual == expected)
            })
        })
        .collect()
}

/// Bidirectional BFS: expands forward from `from` and backward from `to`,
/// alternating the smaller frontier. Returns the shortest path as a list
/// of node IDs (including endpoints), or None if unreachable.
///
/// Neighbor resolution is parameterized via closures so callers can supply
/// either a `NeighborCache` (fast, cached `Vec<NodeId>`) or fall back to
/// full `Edge` deserialization via `GraphAccess`.
fn bidirectional_bfs(
    from: NodeId,
    to: NodeId,
    outgoing_fn: &dyn Fn(NodeId) -> tessera_graph::Result<Vec<NodeId>>,
    incoming_fn: &dyn Fn(NodeId) -> tessera_graph::Result<Vec<NodeId>>,
) -> Option<Vec<NodeId>> {
    if from == to {
        return Some(vec![from]);
    }

    // Forward: from → to (outgoing edges)
    let mut fwd_visited: HashMap<NodeId, Option<NodeId>> = HashMap::new(); // node → parent
    let mut fwd_frontier: Vec<NodeId> = vec![from];
    fwd_visited.insert(from, None);

    // Backward: to → from (incoming edges)
    let mut bwd_visited: HashMap<NodeId, Option<NodeId>> = HashMap::new();
    let mut bwd_frontier: Vec<NodeId> = vec![to];
    bwd_visited.insert(to, None);

    loop {
        if fwd_frontier.is_empty() && bwd_frontier.is_empty() {
            return None; // Unreachable
        }

        // Expand the smaller frontier, accumulating all meeting candidates
        // from the complete layer before selecting the optimal one.
        let mut meeting_candidate: Option<NodeId> = None;

        #[allow(clippy::branches_sharing_code)] // forward/backward are distinct despite shared init
        if !fwd_frontier.is_empty()
            && (bwd_frontier.is_empty() || fwd_frontier.len() <= bwd_frontier.len())
        {
            // Expand forward — complete the entire layer
            let mut next_frontier = Vec::new();
            for &node in &fwd_frontier {
                let Ok(neighbors) = outgoing_fn(node) else {
                    continue;
                };
                for neighbor in neighbors {
                    if fwd_visited.contains_key(&neighbor) {
                        continue;
                    }
                    fwd_visited.insert(neighbor, Some(node));

                    if bwd_visited.contains_key(&neighbor) {
                        // Accumulate candidate — don't return yet
                        meeting_candidate = Some(neighbor);
                    } else {
                        next_frontier.push(neighbor);
                    }
                }
            }
            fwd_frontier = next_frontier;
        } else {
            // Expand backward — complete the entire layer
            let mut next_frontier = Vec::new();
            for &node in &bwd_frontier {
                let Ok(neighbors) = incoming_fn(node) else {
                    continue;
                };
                for neighbor in neighbors {
                    if bwd_visited.contains_key(&neighbor) {
                        continue;
                    }
                    bwd_visited.insert(neighbor, Some(node));

                    if fwd_visited.contains_key(&neighbor) {
                        meeting_candidate = Some(neighbor);
                    } else {
                        next_frontier.push(neighbor);
                    }
                }
            }
            bwd_frontier = next_frontier;
        }

        // After completing the layer, if any meeting point was found, reconstruct.
        // All candidates in the same BFS layer have the same total depth, so any
        // candidate produces an optimal path.
        if let Some(meeting) = meeting_candidate {
            return Some(reconstruct_path(&fwd_visited, &bwd_visited, meeting));
        }
    }
}

/// Reconstructs the shortest path from two parent maps meeting at `meeting`.
fn reconstruct_path(
    fwd_parents: &HashMap<NodeId, Option<NodeId>>,
    bwd_parents: &HashMap<NodeId, Option<NodeId>>,
    meeting: NodeId,
) -> Vec<NodeId> {
    // Build forward part: from → meeting
    let mut fwd_path = Vec::new();
    let mut current = meeting;
    loop {
        fwd_path.push(current);
        match fwd_parents.get(&current) {
            Some(Some(parent)) => current = *parent,
            _ => break,
        }
    }
    fwd_path.reverse();

    // Build backward part: meeting → to
    current = meeting;
    while let Some(Some(child)) = bwd_parents.get(&current) {
        current = *child;
        fwd_path.push(current);
    }

    fwd_path
}

// ── Variable-hop Optimized Execution ────────────────────────────────────────

/// Optimized variable-hop BFS execution.
///
/// Instead of cloning full `Node` / `HashMap` per visited node (MIT core),
/// this implementation:
/// - Tracks `NodeId` only during BFS
/// - Builds a `HashSet<NodeId>` for O(1) end-label membership check
/// - Only fetches `graph.node(id)` when emitting a result that passes filters
/// - Projects RETURN items directly into `GqlRow`
#[allow(clippy::too_many_lines)]
fn execute_variable_hop_query<G: GraphAccess + ?Sized>(
    graph: &G,
    query: &GqlQuery,
    resolve_neighbors: &dyn Fn(NodeId, Direction) -> tessera_graph::Result<Vec<NodeId>>,
) -> tessera_graph::Result<GqlResult> {
    let pattern = &query.match_clause.patterns[0];

    if pattern.hops.is_empty() {
        return tessera_graph::gql::execute(graph, query);
    }

    // Find the variable-hop edge. The caller (execute_query) guarantees at
    // least one exists via needs_optimized_execution, but we delegate
    // gracefully instead of panicking if the invariant is somehow violated.
    let Some(hop_idx) = pattern
        .hops
        .iter()
        .position(|(ep, _)| matches!(ep.length, EdgeLength::Variable { .. }))
    else {
        return tessera_graph::gql::execute(graph, query);
    };

    // If there are hops before the variable-hop, delegate to MIT core
    // (complex multi-hop + variable-hop not yet supported in optimized path).
    if hop_idx > 0 {
        return tessera_graph::gql::execute(graph, query);
    }

    let (ep, end_np) = &pattern.hops[hop_idx];
    let start_np = &pattern.start;

    let (min, max) = match ep.length {
        EdgeLength::Variable { min, max } => (min.unwrap_or(0), max.unwrap_or(u32::MAX)),
        EdgeLength::Fixed => unreachable!(),
    };

    // Resolve start nodes: label filter, then property filter.
    #[allow(clippy::option_if_let_else)] // complex logic is clearer as if-let
    let start_ids: Vec<NodeId> = if let Some(label) = start_np.labels.first() {
        let candidates = graph.nodes_by_label(label);
        if start_np.props.is_empty() {
            candidates
        } else {
            let prop_filters: Vec<(String, Property)> = start_np
                .props
                .iter()
                .filter_map(|(k, v)| literal_to_property(v).map(|p| (k.clone(), p)))
                .collect();
            candidates
                .into_iter()
                .filter(|&id| {
                    let Ok(node) = graph.node(id) else { return false };
                    prop_filters.iter().all(|(key, expected)| {
                        node.properties().get(key).is_some_and(|actual| actual == expected)
                    })
                })
                .collect()
        }
    } else {
        // No label — match all nodes (unusual but valid).
        // KNOWN LIMITATION: assumes IDs are in range 0..node_count(). If the
        // graph has deleted nodes (sparse IDs), node_exists() guards against
        // invalid IDs but nodes with ID >= node_count() will be missed.
        // GraphAccess does not expose an all-node-IDs iterator.
        (0..graph.node_count())
            .filter_map(|i| {
                #[allow(clippy::cast_possible_truncation)]
                let id = NodeId::from_raw(i as u64);
                if graph.node_exists(id) { Some(id) } else { None }
            })
            .collect()
    };

    // Build label filter set for O(1) end-node label check.
    let label_filter: Option<HashSet<NodeId>> = end_np
        .labels
        .first()
        .map(|label| graph.nodes_by_label(label).into_iter().collect());

    // Build property filter from end NodePattern inline props.
    let end_props: Vec<(String, Property)> = end_np
        .props
        .iter()
        .filter_map(|(k, v)| literal_to_property(v).map(|p| (k.clone(), p)))
        .collect();

    // Build a variable map: variable name → role (start or end) so we can
    // resolve RETURN expressions.
    let start_var = start_np.var.as_deref();
    let end_var = end_np.var.as_deref();

    // Convert AstDirection → tessera_graph::Direction for BFS traversal.
    // AstDirection is not re-exported from MIT core but has the same variants
    // as Direction (Outgoing, Incoming, Both). We convert via Debug formatting.
    let direction = ast_direction_to_direction(&ep.direction);

    let mut results: GqlResult = Vec::new();

    for &start_id in &start_ids {
        // BFS tracking NodeId + depth only
        let mut queue: VecDeque<(NodeId, u32)> = VecDeque::new();
        let mut visited = HashSet::new();
        visited.insert(start_id);

        // Emit start node at depth 0 when min == 0
        if min == 0 && node_passes_filter(start_id, label_filter.as_ref(), &end_props, graph) {
            let row = project_bfs_row(graph, start_id, start_id, start_var, end_var, query)?;
            results.push(row);
        }

        // Seed BFS
        for next_id in resolve_neighbors(start_id, direction)? {
            if visited.insert(next_id) {
                queue.push_back((next_id, 1));
            }
        }

        while let Some((node_id, depth)) = queue.pop_front() {
            if depth > max {
                continue;
            }

            // Emit if within [min..=max] and passes end-node filters
            if depth >= min && node_passes_filter(node_id, label_filter.as_ref(), &end_props, graph) {
                let row =
                    project_bfs_row(graph, start_id, node_id, start_var, end_var, query)?;
                results.push(row);
            }

            // Continue BFS if not at max depth
            if depth < max {
                if let Ok(next_ids) = resolve_neighbors(node_id, direction) {
                    for next_id in next_ids {
                        if visited.insert(next_id) {
                            queue.push_back((next_id, depth + 1));
                        }
                    }
                }
            }
        }
    }

    Ok(results)
}

// ── Optimized traversal helpers ─────────────────────────────────────────────

/// Converts an `AstDirection` (not re-exported from MIT core) into the public
/// `tessera_graph::Direction` enum by matching its Debug representation.
///
/// Both enums have identical variants (Outgoing, Incoming, Both).
fn ast_direction_to_direction(ast_dir: &impl std::fmt::Debug) -> Direction {
    let s = format!("{ast_dir:?}");
    if s.contains("Incoming") {
        Direction::Incoming
    } else if s.contains("Both") {
        Direction::Both
    } else {
        debug_assert!(
            s.contains("Outgoing"),
            "ast_direction_to_direction: unknown AstDirection variant '{s}'. \
             Update this function if MIT core adds new Direction variants."
        );
        Direction::Outgoing
    }
}

/// Returns edges from a node following the given direction.
fn edges_for_direction<G: GraphAccess + ?Sized>(
    graph: &G,
    node_id: NodeId,
    direction: Direction,
) -> tessera_graph::Result<Vec<tessera_graph::Edge>> {
    match direction {
        Direction::Outgoing => graph.outgoing_edges(node_id),
        Direction::Incoming => graph.incoming_edges(node_id),
        Direction::Both => {
            let mut edges = graph.outgoing_edges(node_id)?;
            let incoming = graph.incoming_edges(node_id)?;
            // Self-loops appear in both outgoing and incoming; skip duplicates.
            for edge in incoming {
                if edge.source() != edge.target() {
                    edges.push(edge);
                }
            }
            Ok(edges)
        }
    }
}

/// Returns neighbor node IDs from a node following the given direction.
///
/// Lightweight alternative to `edges_for_direction` — returns `Vec<NodeId>` instead of
/// `Vec<Edge>`, avoiding full edge deserialization. When a `NeighborCache` is available,
/// this resolves from the cache with zero page reads and zero heap allocations.
/// Returns neighbor node IDs from a node following the given direction.
///
/// Lightweight alternative to `edges_for_direction` — returns `Vec<NodeId>` instead of
/// `Vec<Edge>`, avoiding full edge deserialization. When a `NeighborCache` is available,
/// this resolves from the cache with zero page reads and zero heap allocations.
fn neighbor_ids_for_direction<G: GraphAccess>(
    graph: &G,
    cache: Option<&NeighborCache<G>>,
    node_id: NodeId,
    direction: Direction,
) -> tessera_graph::Result<Vec<NodeId>> {
    match (direction, cache) {
        (Direction::Outgoing, Some(c)) => c.outgoing_neighbor_ids(node_id),
        (Direction::Incoming, Some(c)) => c.incoming_neighbor_ids(node_id),
        (Direction::Both, Some(c)) => {
            let mut ids = c.outgoing_neighbor_ids(node_id)?;
            let incoming = c.incoming_neighbor_ids(node_id)?;
            // Skip self-loops (they appear in both outgoing and incoming)
            for id in incoming {
                if id != node_id {
                    ids.push(id);
                }
            }
            Ok(ids)
        }
        // Fallback: full edge deserialization
        (Direction::Outgoing, None) => {
            Ok(graph.outgoing_edges(node_id)?.iter().map(tessera_graph::Edge::target).collect())
        }
        (Direction::Incoming, None) => {
            Ok(graph.incoming_edges(node_id)?.iter().map(tessera_graph::Edge::source).collect())
        }
        (Direction::Both, None) => {
            let edges = edges_for_direction(graph, node_id, direction)?;
            Ok(edges.iter().map(|e| {
                if e.source() == node_id { e.target() } else { e.source() }
            }).collect())
        }
    }
}

/// Uncached variant of `neighbor_ids_for_direction` for `?Sized` graph types.
fn neighbor_ids_uncached<G: GraphAccess + ?Sized>(
    graph: &G,
    node_id: NodeId,
    direction: Direction,
) -> tessera_graph::Result<Vec<NodeId>> {
    match direction {
        Direction::Outgoing => {
            Ok(graph.outgoing_edges(node_id)?.iter().map(tessera_graph::Edge::target).collect())
        }
        Direction::Incoming => {
            Ok(graph.incoming_edges(node_id)?.iter().map(tessera_graph::Edge::source).collect())
        }
        Direction::Both => {
            let edges = edges_for_direction(graph, node_id, direction)?;
            Ok(edges.iter().map(|e| {
                if e.source() == node_id { e.target() } else { e.source() }
            }).collect())
        }
    }
}

/// Resolves neighbor IDs via `SharedNeighborCache` (thread-safe, LBAC-scoped).
fn shared_neighbor_ids_for_direction<G: GraphAccess + ?Sized>(
    graph: &G,
    cache: &SharedNeighborCache,
    ck: &ClearanceKey,
    node_id: NodeId,
    direction: Direction,
) -> tessera_graph::Result<Vec<NodeId>> {
    match direction {
        Direction::Outgoing => cache.outgoing_neighbor_ids(graph, node_id, ck),
        Direction::Incoming => cache.incoming_neighbor_ids(graph, node_id, ck),
        Direction::Both => {
            let mut ids = cache.outgoing_neighbor_ids(graph, node_id, ck)?;
            let incoming = cache.incoming_neighbor_ids(graph, node_id, ck)?;
            for id in incoming {
                if id != node_id {
                    ids.push(id);
                }
            }
            Ok(ids)
        }
    }
}

/// Converts a graph `Property` into a runtime `GqlValue`.
fn gql_value_from_property(p: &Property) -> GqlValue {
    match p {
        Property::String(s) => GqlValue::Str(s.clone()),
        Property::I64(v) => GqlValue::Int(*v),
        Property::F64(v) => GqlValue::Float(*v),
        Property::Bool(b) => GqlValue::Bool(*b),
        Property::Bytes(_) => GqlValue::Null,
    }
}

/// Checks if a node passes the end-node label and property filters without
/// fetching the full Node (uses the precomputed label `HashSet` for O(1) check).
fn node_passes_filter<G: GraphAccess + ?Sized>(
    node_id: NodeId,
    label_filter: Option<&HashSet<NodeId>>,
    end_props: &[(String, Property)],
    graph: &G,
) -> bool {
    // Label check via precomputed set
    if let Some(allowed) = label_filter {
        if !allowed.contains(&node_id) {
            return false;
        }
    }

    // Property check — only fetch node if we have props to check.
    // graph.node() errors are treated as "does not pass filter" (fail-safe:
    // exclude the node from results rather than propagating the error).
    if !end_props.is_empty() {
        let Ok(node) = graph.node(node_id) else {
            return false;
        };
        for (key, expected) in end_props {
            match node.properties().get(key) {
                Some(actual) if actual == expected => {}
                _ => return false,
            }
        }
    }

    true
}

/// Projects a RETURN row from BFS result, resolving variable references
/// to the start or end node.
fn project_bfs_row<G: GraphAccess + ?Sized>(
    graph: &G,
    start_id: NodeId,
    end_id: NodeId,
    start_var: Option<&str>,
    end_var: Option<&str>,
    query: &GqlQuery,
) -> tessera_graph::Result<GqlRow> {
    let mut row = HashMap::with_capacity(query.return_clause.items.len());

    for item in &query.return_clause.items {
        let col_name = item
            .alias
            .as_deref()
            .map_or_else(|| expr_surface_name(&item.expr), String::from);

        let value = eval_bfs_expr(&item.expr, graph, start_id, end_id, start_var, end_var)?;
        row.insert(col_name, value);
    }

    Ok(row)
}

/// Evaluates a RETURN expression against BFS-resolved nodes.
///
/// Supports `PropAccess`, `Var`, and `Literal`. For anything else, returns Null.
fn eval_bfs_expr<G: GraphAccess + ?Sized>(
    expr: &Expr,
    graph: &G,
    start_id: NodeId,
    end_id: NodeId,
    start_var: Option<&str>,
    end_var: Option<&str>,
) -> tessera_graph::Result<GqlValue> {
    match expr {
        Expr::PropAccess { var, prop } => {
            let node_id = resolve_var_to_node_id(var, start_var, start_id, end_var, end_id);
            match node_id {
                Some(id) => {
                    let node = graph.node(id)?;
                    Ok(node
                        .properties()
                        .get(prop)
                        .map_or(GqlValue::Null, gql_value_from_property))
                }
                None => Ok(GqlValue::Null),
            }
        }
        Expr::Var(var) => {
            // Return the variable name as a map of all properties
            let node_id = resolve_var_to_node_id(var, start_var, start_id, end_var, end_id);
            match node_id {
                Some(id) => {
                    let node = graph.node(id)?;
                    let props: Vec<GqlValue> = node
                        .properties()
                        .values()
                        .map(gql_value_from_property)
                        .collect();
                    // For a bare variable, return all properties as a list
                    // (MIT core returns the full node as a map — we approximate)
                    Ok(GqlValue::List(props))
                }
                None => Ok(GqlValue::Null),
            }
        }
        Expr::Literal(lit) => Ok(compile_literal(lit)),
        _ => Ok(GqlValue::Null),
    }
}

/// Resolves a variable name to a `NodeId` based on the start/end variable bindings.
fn resolve_var_to_node_id(
    var: &str,
    start_var: Option<&str>,
    start_id: NodeId,
    end_var: Option<&str>,
    end_id: NodeId,
) -> Option<NodeId> {
    if start_var == Some(var) {
        Some(start_id)
    } else if end_var == Some(var) {
        Some(end_id)
    } else {
        None
    }
}

/// Converts an AST `Literal` into a `GqlValue`.
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

/// Produces a display name for an expression (column name when no AS alias).
fn expr_surface_name(expr: &Expr) -> String {
    match expr {
        Expr::Literal(Literal::Int(v)) => v.to_string(),
        Expr::Literal(Literal::Str(s)) => format!("'{s}'"),
        Expr::Var(v) => v.clone(),
        Expr::PropAccess { var, prop } => format!("{var}.{prop}"),
        Expr::FunctionCall { name, args } => {
            let arg_strs: Vec<String> = args.iter().map(expr_surface_name).collect();
            format!("{}({})", name, arg_strs.join(", "))
        }
        _ => "?".to_string(),
    }
}

// ── Mutation Execution ──────────────────────────────────────────────────────

/// Executes a GQL mutation statement against a mutable graph.
///
/// The function first resolves any MATCH clause (using a short-lived immutable
/// borrow), then performs the write operation with a mutable borrow.
///
/// # Errors
///
/// Returns [`Error::GqlMutationError`] on semantic errors (e.g., DELETE without
/// DETACH when a node has edges, referencing an unbound variable).
/// Returns [`Error::GqlCompileError`] if variable resolution fails.
/// May also return storage errors from the underlying `Graph` API.
pub fn execute_mut<G: GraphAccess>(
    graph: &mut G,
    stmt: &MutationStatement,
) -> tessera_graph::Result<GqlMutationResult> {
    // Phase 1: immutable MATCH — collect (variable_name, NodeId) pairs.
    let all_matches: Vec<(String, NodeId)> = {
        if let Some(ref mc) = stmt.match_clause {
            compile_match_for_mutation(graph, mc)?
        } else {
            Vec::new()
        }
    };

    // Build a multi-value variable map: var_name → Vec<NodeId>.
    let mut node_var_multi: HashMap<String, Vec<NodeId>> = HashMap::new();
    for (var, id) in &all_matches {
        node_var_multi.entry(var.clone()).or_default().push(*id);
    }

    // Post-mutation SET clause is not yet implemented.
    if stmt.set_clause.is_some() {
        return Err(Error::GqlMutationError(
            "set_clause combined with CREATE/MERGE is not yet implemented; \
             use a standalone MATCH ... SET ... statement"
                .into(),
        ));
    }

    // Phase 2: mutable writes.
    let mut result = GqlMutationResult::default();

    // Shared variable map: first ID per variable for CREATE/MERGE arms.
    let mut node_vars: HashMap<String, NodeId> = node_var_multi
        .iter()
        .filter_map(|(k, v)| v.first().copied().map(|id| (k.clone(), id)))
        .collect();

    match &stmt.mutation {
        MutationClause::Create(c) => {
            execute_create(graph, c, &mut node_vars, &mut result)?;
        }
        MutationClause::Set(s) => {
            execute_set(graph, s, &node_var_multi, &mut result)?;
        }
        MutationClause::Delete(d) => {
            execute_delete(graph, d, &node_var_multi, &mut result)?;
        }
        MutationClause::Merge(m) => {
            execute_merge(graph, m, &mut node_vars, &mut result)?;
        }
    }

    Ok(result)
}

// ── CREATE ───────────────────────────────────────────────────────────────────

fn execute_create<G: GraphAccess>(
    graph: &mut G,
    clause: &tessera_graph::gql::CreateClause,
    node_vars: &mut HashMap<String, NodeId>,
    result: &mut GqlMutationResult,
) -> tessera_graph::Result<()> {
    for pattern in &clause.patterns {
        match pattern {
            CreatePattern::Node { var, label, props } => {
                // Top-level CREATE has no MATCH context — evaluate prop
                // expressions with an empty PatternMatch. Literal expressions
                // resolve fine; variable references resolve to Null and are
                // silently skipped by `resolve_create_props`.
                let properties = resolve_create_props(
                    props,
                    &tessera_graph::PatternMatch::empty(),
                    graph,
                    None,
                );
                let id = graph.add_node(label, properties)?;
                result.nodes_created += 1;
                if let Some(v) = var {
                    node_vars.insert(v.clone(), id);
                }
            }
            CreatePattern::Edge {
                source_var,
                rel_label,
                rel_props,
                target_var,
            } => {
                let source = *node_vars.get(source_var.as_str()).ok_or_else(|| {
                    Error::GqlMutationError(format!(
                        "unbound variable '{source_var}' in CREATE edge"
                    ))
                })?;
                let target = *node_vars.get(target_var.as_str()).ok_or_else(|| {
                    Error::GqlMutationError(format!(
                        "unbound variable '{target_var}' in CREATE edge"
                    ))
                })?;
                let properties = resolve_create_props(
                    rel_props,
                    &tessera_graph::PatternMatch::empty(),
                    graph,
                    None,
                );
                graph.add_edge(rel_label, source, target, properties)?;
                result.edges_created += 1;
            }
        }
    }
    Ok(())
}

// ── DELETE ───────────────────────────────────────────────────────────────────

fn execute_delete<G: GraphAccess>(
    graph: &mut G,
    clause: &DeleteClause,
    node_var_multi: &HashMap<String, Vec<NodeId>>,
    result: &mut GqlMutationResult,
) -> tessera_graph::Result<()> {
    let mut ids_to_delete: Vec<NodeId> = Vec::new();
    for var in &clause.vars {
        let ids = node_var_multi.get(var.as_str()).ok_or_else(|| {
            Error::GqlMutationError(format!("unbound variable '{var}' in DELETE"))
        })?;
        ids_to_delete.extend_from_slice(ids);
    }

    ids_to_delete.sort_unstable_by_key(|id| id.as_u64());
    ids_to_delete.dedup_by_key(|id| id.as_u64());

    for id in ids_to_delete {
        if !graph.node_exists(id) {
            continue;
        }

        if !clause.detach {
            let outgoing = graph.outgoing_edges(id)?;
            let incoming = graph.incoming_edges(id)?;
            if !outgoing.is_empty() || !incoming.is_empty() {
                return Err(Error::GqlMutationError(format!(
                    "Cannot DELETE node {id} which has {} outgoing and {} incoming relationships. \
                     Use DETACH DELETE to delete the node and its relationships.",
                    outgoing.len(),
                    incoming.len(),
                )));
            }
        }

        let edges_before = graph.edge_count();
        graph.remove_node(id)?;
        let edges_removed = edges_before.saturating_sub(graph.edge_count());
        result.nodes_deleted += 1;
        #[allow(clippy::cast_possible_truncation)] // usize→u64: lossless on all supported platforms
        {
            result.edges_deleted += edges_removed as u64;
        }
    }

    Ok(())
}

// ── SET ──────────────────────────────────────────────────────────────────────

fn execute_set<G: GraphAccess>(
    graph: &mut G,
    clause: &SetClause,
    node_var_multi: &HashMap<String, Vec<NodeId>>,
    result: &mut GqlMutationResult,
) -> tessera_graph::Result<()> {
    // Group all (prop_key, prop_value) by NodeId.
    let mut per_node: HashMap<NodeId, Vec<(String, Property)>> = HashMap::new();

    for assignment in &clause.assignments {
        let ids = node_var_multi.get(assignment.var.as_str()).ok_or_else(|| {
            Error::GqlMutationError(format!("unbound variable '{}' in SET", assignment.var))
        })?;
        // See import comment for rationale of using a deprecated helper here.
        #[allow(deprecated)]
        let value = eval_set_value(&assignment.value)?;
        for &id in ids {
            per_node
                .entry(id)
                .or_default()
                .push((assignment.prop.clone(), value.clone()));
        }
    }

    // Apply all properties per node in a single read+write.
    for (id, assignments) in per_node {
        let mut node = graph.node(id)?;
        let count = assignments.len();
        for (prop_key, prop_value) in assignments {
            node.properties_mut().insert(prop_key, prop_value);
        }
        graph.update_node(id, &node)?;
        #[allow(clippy::cast_possible_truncation)]
        {
            result.properties_set += count as u64;
        }
    }

    Ok(())
}

// ── MERGE ────────────────────────────────────────────────────────────────────

fn execute_merge<G: GraphAccess>(
    graph: &mut G,
    clause: &MergeClause,
    node_vars: &mut HashMap<String, NodeId>,
    result: &mut GqlMutationResult,
) -> tessera_graph::Result<()> {
    let candidate_ids = graph.nodes_by_label(&clause.label);

    let match_props: Vec<(String, Property)> = clause
        .props
        .iter()
        .filter_map(|(k, v)| literal_to_property(v).map(|p| (k.clone(), p)))
        .collect();

    let mut found_id: Option<NodeId> = None;

    'search: for id in candidate_ids {
        let node = graph.node(id)?;
        for (key, expected) in &match_props {
            match node.properties().get(key) {
                Some(actual) if actual == expected => {}
                _ => continue 'search,
            }
        }
        found_id = Some(id);
        break;
    }

    let id = if let Some(existing) = found_id {
        existing
    } else {
        let properties: tessera_graph::Properties = match_props.into_iter().collect();
        let new_id = graph.add_node(&clause.label, properties)?;
        result.nodes_created += 1;
        new_id
    };

    if let Some(ref v) = clause.var {
        node_vars.insert(v.clone(), id);
    }

    Ok(())
}
