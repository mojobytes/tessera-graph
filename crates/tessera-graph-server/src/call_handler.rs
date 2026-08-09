// SPDX-License-Identifier: BSL-1.1

//! Executes a [`CallStatement`] against a locked [`Graph`], producing a
//! Bolt-shaped result for the caller to surface over `RUN`/`PULL`.
//!
//! Mirrors `ddl_handler`: returns its own [`CallPending`] type (the handler's
//! `PendingResult` and column helpers are private to the `handler` module), and
//! the Bolt handler copies it across. MULTI-TENANT: the caller (handler.rs
//! `dispatch_call`) resolves the session's selected database graph and passes
//! its arc in — the handler does not pick a default.

use std::sync::{Arc, RwLock};

use tessera_graph::Graph;
use tessera_graph::call::{ProcedureKind, resolve_procedure};
use tessera_graph::gql::{CallStatement, Expr, GqlValue, execute_call_result};
use tessera_graph_protocol::packstream::PackStreamValue;

/// A CALL statement result ready to surface over Bolt. Mirrors
/// `ddl_handler::DdlPending`.
#[derive(Debug)]
pub struct CallPending {
    /// Column names, in the order the client wrote them.
    pub fields_psv: Vec<PackStreamValue>,
    /// Result rows, each a positional list matching `fields_psv`.
    pub rows: Vec<Vec<PackStreamValue>>,
}

/// Dispatch a [`CallStatement`] against the given graph arc.
///
/// # Errors
///
/// Returns `Err((bolt_code, message))` when:
/// - the procedure name is not in the registry (`ProcedureNotFound`),
/// - the graph read lock is poisoned (`ExecutionFailed`).
pub fn dispatch_call(
    stmt: &CallStatement,
    graph: &Arc<RwLock<Graph>>,
    max_rows: u64,
) -> Result<CallPending, (String, String)> {
    let ns = stmt.namespace.as_deref();
    let kind = resolve_procedure(ns, &stmt.procedure).ok_or_else(|| {
        (
            "Neo.ClientError.Procedure.ProcedureNotFound".to_owned(),
            format!(
                "There is no procedure with the name `{}.{}` registered",
                ns.unwrap_or(""),
                stmt.procedure
            ),
        )
    })?;

    // One read lock spans the procedure read AND the UNWIND/RETURN evaluation:
    // `execute_call_result` borrows `&G: GraphAccess` (eval_expr needs it even
    // though CALL touches no entities), and one lock avoids a read-read race
    // where labels change between two acquires.
    let g = graph.read().map_err(|_| {
        (
            "Neo.ClientError.Statement.ExecutionFailed".to_owned(),
            "graph read lock poisoned".to_owned(),
        )
    })?;

    let procedure_list: Vec<GqlValue> = match kind {
        ProcedureKind::VertexLabels => g.node_labels().into_iter().map(GqlValue::Str).collect(),
        ProcedureKind::EdgeTypes => g.edge_types().into_iter().map(GqlValue::Str).collect(),
        // Admin backup procedures are async + registry-scoped + admin-gated and
        // are intercepted by the handler BEFORE this sync read dispatcher. If
        // one reaches here, the handler's routing is broken: fail safe rather
        // than silently doing nothing.
        ProcedureKind::Snapshot | ProcedureKind::Restore => {
            return Err((
                "Neo.ClientError.Statement.ExecutionFailed".to_owned(),
                "admin backup procedure routed to the read-only call handler".to_owned(),
            ));
        }
    };

    // `execute_call_result` returns Vec<GqlRow> (= Vec<HashMap<String,GqlValue>>),
    // NOT a Result.
    let gql_rows = execute_call_result(
        &*g,
        GqlValue::List(procedure_list),
        &stmt.yield_col,
        stmt.unwind.as_ref(),
        stmt.return_clause.as_ref(),
    );
    let _ = max_rows; // Cap B enforced at the GraphAccessor boundary, as for pipelines.

    // Column order the client wrote: RETURN aliases/vars, else UNWIND var,
    // else yield col. CALL rows have a single column post-UNWIND, so one
    // ordered list suffices (no union-of-keys needed).
    let columns: Vec<String> = match (&stmt.return_clause, &stmt.unwind) {
        (Some(rc), _) => rc
            .items
            .iter()
            .map(|it| {
                it.alias.clone().unwrap_or_else(|| match &it.expr {
                    Expr::Var(v) => v.clone(),
                    _ => String::new(),
                })
            })
            .collect(),
        (None, Some(u)) => vec![u.var.clone()],
        (None, None) => vec![stmt.yield_col.clone()],
    };

    let fields_psv: Vec<PackStreamValue> = columns
        .iter()
        .cloned()
        .map(PackStreamValue::String)
        .collect();
    let rows: Vec<Vec<PackStreamValue>> = gql_rows
        .iter()
        .map(|row| {
            columns
                .iter()
                .map(|c| gql_value_to_pack(row.get(c)))
                .collect()
        })
        .collect();

    Ok(CallPending { fields_psv, rows })
}

/// Minimal `GqlValue` -> `PackStreamValue` conversion for the values the two
/// built-in procedures produce (String after UNWIND, List for YIELD-only,
/// Null for an absent column). There is no shared public converter in the
/// server crate; `ddl_handler` likewise builds `PackStream` values directly.
fn gql_value_to_pack(v: Option<&GqlValue>) -> PackStreamValue {
    match v {
        Some(GqlValue::Str(s)) => PackStreamValue::String(s.clone()),
        Some(GqlValue::List(items)) => {
            PackStreamValue::List(items.iter().map(|it| gql_value_to_pack(Some(it))).collect())
        }
        Some(GqlValue::Int(n)) => PackStreamValue::Int(*n),
        Some(GqlValue::Bool(b)) => PackStreamValue::Bool(*b),
        // The introspection procedures only ever yield String/List; anything
        // else (incl. Null / absent) maps to Null rather than panicking.
        None | Some(_) => PackStreamValue::Null,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tessera_graph::gql::{CallStatement, ReturnClause, ReturnItem, UnwindClause};
    use tessera_graph::props;

    fn make_graph_with_data() -> Arc<RwLock<Graph>> {
        let mut g = Graph::new();
        g.add_node("Person", props! {}).unwrap();
        g.add_node("Asset", props! {}).unwrap();
        let a = g.add_node("Node", props! {}).unwrap();
        let b = g.add_node("Node", props! {}).unwrap();
        g.add_edge("KNOWS", a, b, props! {}).unwrap();
        g.add_edge("TRUSTS", a, b, props! {}).unwrap();
        Arc::new(RwLock::new(g))
    }

    fn call_full(proc: &str, col: &str, var: &str) -> CallStatement {
        CallStatement {
            namespace: Some("mg".to_owned()),
            procedure: proc.to_owned(),
            args: vec![],
            yield_col: col.to_owned(),
            unwind: Some(UnwindClause {
                expr: Expr::Var(col.to_owned()),
                var: var.to_owned(),
            }),
            return_clause: Some(ReturnClause {
                distinct: false,
                items: vec![ReturnItem {
                    expr: Expr::Var(var.to_owned()),
                    alias: None,
                }],
            }),
        }
    }

    #[test]
    fn vertex_labels_full_pipeline_returns_rows() {
        let graph = make_graph_with_data();
        let pending = dispatch_call(
            &call_full("vertex_labels", "vertex_labels", "vl"),
            &graph,
            1000,
        )
        .unwrap();
        assert_eq!(
            pending.fields_psv,
            vec![PackStreamValue::String("vl".to_owned())]
        );
        // Distinct labels: Asset, Node, Person.
        assert_eq!(pending.rows.len(), 3, "got {:?}", pending.rows);
        // Each row is a single String cell.
        for row in &pending.rows {
            assert_eq!(row.len(), 1);
            assert!(matches!(row[0], PackStreamValue::String(_)));
        }
    }

    #[test]
    fn edge_types_full_pipeline_returns_rows() {
        let graph = make_graph_with_data();
        let pending =
            dispatch_call(&call_full("edge_types", "edge_types", "et"), &graph, 1000).unwrap();
        assert_eq!(pending.rows.len(), 2, "got {:?}", pending.rows);
    }

    #[test]
    fn unknown_procedure_returns_err() {
        let graph = Arc::new(RwLock::new(Graph::new()));
        let stmt = CallStatement {
            namespace: Some("tessera".to_owned()),
            procedure: "nonexistent".to_owned(),
            args: vec![],
            yield_col: "nonexistent".to_owned(),
            unwind: None,
            return_clause: None,
        };
        let (code, _msg) = dispatch_call(&stmt, &graph, 1000).unwrap_err();
        assert!(code.contains("ProcedureNotFound"), "got: {code}");
    }

    #[test]
    fn yield_only_no_unwind_returns_list_row() {
        let mut g = Graph::new();
        g.add_node("Alpha", props! {}).unwrap();
        let graph = Arc::new(RwLock::new(g));
        let stmt = CallStatement {
            namespace: Some("tessera".to_owned()),
            procedure: "vertex_labels".to_owned(),
            args: vec![],
            yield_col: "vertex_labels".to_owned(),
            unwind: None,
            return_clause: None,
        };
        let pending = dispatch_call(&stmt, &graph, 1000).unwrap();
        // No UNWIND: one row whose single cell is the List.
        assert_eq!(pending.rows.len(), 1);
        assert!(
            matches!(&pending.rows[0][0], PackStreamValue::List(_)),
            "expected List cell, got {:?}",
            &pending.rows[0][0]
        );
    }
}
