//! GQL mutation execution — enterprise-only.
//!
//! Provides `execute_mut` for executing GQL mutation statements (CREATE, DELETE,
//! SET, MERGE) against any mutable `GraphAccess` implementation. Read-only query
//! execution remains in the MIT core (`tessera_graph::gql::execute`).

use std::collections::HashMap;

use tessera_graph::gql::{
    CreatePattern, DeleteClause, MergeClause, MutationClause, MutationStatement, SetClause,
    compile_match_for_mutation, eval_set_value, literal_to_property, literal_vec_to_properties,
};
use tessera_graph::{Error, GqlMutationResult, GraphAccess, NodeId, Property};

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
                let properties = literal_vec_to_properties(props);
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
                let properties = literal_vec_to_properties(rel_props);
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
