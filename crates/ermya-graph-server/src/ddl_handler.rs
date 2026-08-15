// SPDX-License-Identifier: BSL-1.1

//! Executes a [`DdlStatement`] against a locked [`Graph`], producing a
//! Bolt-shaped result for the caller to surface over RUN/PULL.
//!
//! Mutation DDL statements (CREATE/DROP INDEX, CREATE/DROP CONSTRAINT) return
//! empty `fields_psv` and `rows`. SHOW statements return tabular rows with
//! a minimal column subset that satisfies the .NET pilot's schema introspection.

use std::sync::{Arc, RwLock};

use ermya_graph::Graph;
use ermya_graph::gql::DdlStatement;
use ermya_graph_protocol::packstream::PackStreamValue;

/// Result of a DDL statement ready to be surfaced via Bolt.
/// For mutation-style DDL (CREATE/DROP) both fields are empty.
/// For SHOW statements they carry the schema and rows.
#[derive(Debug)]
pub struct DdlPending {
    pub fields_psv: Vec<PackStreamValue>,
    pub rows: Vec<Vec<PackStreamValue>>,
}

// DRIVER-DECISION (user, 2026-06-15): the pilot migrates from MEMGRAPH and its
// schema-validation code parses Memgraph's SHOW output shape, NOT Neo4j's. Start
// from Memgraph columns; the Docker gate (Task 10) is the final arbiter and may
// adjust these against the real driver.
//
//   Memgraph `SHOW INDEX INFO`      → columns: index type | label | property | count
//   Memgraph `SHOW CONSTRAINT INFO` → columns: constraint type | label | properties

/// Column schema for `SHOW INDEX INFO` (Memgraph shape).
const INDEX_INFO_FIELDS: &[&str] = &["index type", "label", "property", "count"];

/// Column schema for `SHOW CONSTRAINT INFO` (Memgraph shape).
const CONSTRAINT_INFO_FIELDS: &[&str] = &["constraint type", "label", "properties"];

/// Column schema for `SHOW APPEND ONLY INFO` (issue #61). No Memgraph shape to
/// match — the mode is this engine's own — so it carries the one column the
/// declaration has.
const APPEND_ONLY_INFO_FIELDS: &[&str] = &["label"];

/// Dispatch a [`DdlStatement`] against the given graph.
///
/// # Errors
///
/// Returns `Err((bolt_code, message))` when the write lock cannot be acquired
/// (lock poisoned).
pub fn dispatch_ddl(
    stmt: DdlStatement,
    graph: &Arc<RwLock<Graph>>,
) -> Result<DdlPending, (String, String)> {
    match stmt {
        DdlStatement::CreateIndexLegacy { label, prop }
        | DdlStatement::CreateIndexFor { label, prop } => {
            let mut g = graph.write().map_err(|_| lock_error())?;
            g.schema_catalog_mut().add_index(&label, &prop);
            g.persist_schema().map_err(|e| persist_error(&e))?;
            Ok(empty_result())
        }
        DdlStatement::DropIndex { label, prop } => {
            let mut g = graph.write().map_err(|_| lock_error())?;
            g.schema_catalog_mut().remove_index(&label, &prop);
            g.persist_schema().map_err(|e| persist_error(&e))?;
            Ok(empty_result())
        }
        DdlStatement::CreateUniqueConstraint { label, prop } => {
            let mut g = graph.write().map_err(|_| lock_error())?;
            g.schema_catalog_mut().add_unique_constraint(&label, &prop);
            g.persist_schema().map_err(|e| persist_error(&e))?;
            Ok(empty_result())
        }
        DdlStatement::DropConstraint { label, prop } => {
            let mut g = graph.write().map_err(|_| lock_error())?;
            g.schema_catalog_mut()
                .remove_unique_constraint(&label, &prop);
            g.persist_schema().map_err(|e| persist_error(&e))?;
            Ok(empty_result())
        }
        DdlStatement::ShowIndexInfo => {
            let g = graph.read().map_err(|_| lock_error())?;
            let fields_psv: Vec<PackStreamValue> = INDEX_INFO_FIELDS
                .iter()
                .map(|&f| PackStreamValue::String(f.to_owned()))
                .collect();
            let rows: Vec<Vec<PackStreamValue>> = g
                .schema_catalog()
                .indexes()
                .into_iter()
                .map(|idx| {
                    // Memgraph shape: index type | label | property | count.
                    // `count` is the cardinality; we surface 0 (not tracked yet)
                    // — the Docker gate confirms the client tolerates it.
                    vec![
                        PackStreamValue::String("label+property".to_owned()),
                        PackStreamValue::String(idx.label.clone()),
                        PackStreamValue::String(idx.prop.clone()),
                        PackStreamValue::Int(0),
                    ]
                })
                .collect();
            Ok(DdlPending { fields_psv, rows })
        }
        DdlStatement::ShowConstraintInfo => {
            let g = graph.read().map_err(|_| lock_error())?;
            let fields_psv: Vec<PackStreamValue> = CONSTRAINT_INFO_FIELDS
                .iter()
                .map(|&f| PackStreamValue::String(f.to_owned()))
                .collect();
            let rows: Vec<Vec<PackStreamValue>> = g
                .schema_catalog()
                .constraints()
                .into_iter()
                .map(|c| {
                    // Memgraph shape: constraint type | label | properties.
                    // `properties` is a LIST in Memgraph even for a single prop.
                    vec![
                        PackStreamValue::String("unique".to_owned()),
                        PackStreamValue::String(c.label.clone()),
                        PackStreamValue::List(vec![PackStreamValue::String(c.prop.clone())]),
                    ]
                })
                .collect();
            Ok(DdlPending { fields_psv, rows })
        }
        DdlStatement::SetLabelAppendOnly { label, on } => {
            let mut g = graph.write().map_err(|_| lock_error())?;
            // Goes through the Graph rather than the catalog directly: on
            // withdrawal it also frees the label's existing nodes, which the
            // catalog call alone would defer to the next restart (issue #61).
            g.set_label_append_only(&label, on);
            g.persist_schema().map_err(|e| persist_error(&e))?;
            Ok(empty_result())
        }
        DdlStatement::ShowAppendOnlyInfo => {
            let g = graph.read().map_err(|_| lock_error())?;
            let fields_psv: Vec<PackStreamValue> = APPEND_ONLY_INFO_FIELDS
                .iter()
                .map(|&f| PackStreamValue::String(f.to_owned()))
                .collect();
            let rows: Vec<Vec<PackStreamValue>> = g
                .schema_catalog()
                .append_only_labels()
                .into_iter()
                .map(|d| vec![PackStreamValue::String(d.label.clone())])
                .collect();
            Ok(DdlPending { fields_psv, rows })
        }
    }
}

fn empty_result() -> DdlPending {
    DdlPending {
        fields_psv: Vec::new(),
        rows: Vec::new(),
    }
}

fn lock_error() -> (String, String) {
    (
        "Neo.ClientError.Statement.ExecutionFailed".to_owned(),
        "graph lock poisoned".to_owned(),
    )
}

/// Maps a schema-persistence failure to a Bolt error pair. A DDL mutation that
/// updated the in-memory catalog but failed to write `schema.bin` is surfaced
/// as an execution failure so the client knows the change is not durable.
fn persist_error(e: &ermya_graph::Error) -> (String, String) {
    (
        "Neo.ClientError.Statement.ExecutionFailed".to_owned(),
        format!("failed to persist schema catalog: {e}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, RwLock};
    use ermya_graph::Graph;

    fn make_graph() -> Arc<RwLock<Graph>> {
        Arc::new(RwLock::new(Graph::new()))
    }

    // ── ALTER LABEL … APPEND ONLY (issue #61) ──────────────────────────────

    #[test]
    fn set_label_append_only_declares_in_catalog() {
        let g = make_graph();
        let stmt = ermya_graph::gql::DdlStatement::SetLabelAppendOnly {
            label: "Event".to_owned(),
            on: true,
        };
        let result = dispatch_ddl(stmt, &g).unwrap();
        assert!(result.fields_psv.is_empty(), "mutation DDL returns no rows");
        assert!(result.rows.is_empty());
        assert!(
            g.read()
                .unwrap()
                .schema_catalog()
                .is_label_append_only("Event")
        );
    }

    #[test]
    fn remove_label_append_only_withdraws_and_frees_existing_nodes() {
        let g = make_graph();
        dispatch_ddl(
            ermya_graph::gql::DdlStatement::SetLabelAppendOnly {
                label: "Event".to_owned(),
                on: true,
            },
            &g,
        )
        .unwrap();

        // A node created under the declaration takes the fast path.
        let id = {
            let mut guard = g.write().unwrap();
            guard
                .add_node("Event", ermya_graph::Properties::new())
                .unwrap()
        };

        dispatch_ddl(
            ermya_graph::gql::DdlStatement::SetLabelAppendOnly {
                label: "Event".to_owned(),
                on: false,
            },
            &g,
        )
        .unwrap();

        let mut guard = g.write().unwrap();
        assert!(!guard.schema_catalog().is_label_append_only("Event"));
        // The point of routing through `Graph::set_label_append_only`: the node
        // is freed now, not at the next restart. Proven by mutating it inside a
        // transaction — still-exempt nodes are refused there — rather than by
        // reading it back, which succeeds either way.
        guard.enable_mvcc();
        let txn = guard.begin_txn().unwrap();
        guard
            .remove_node_in_txn(txn, id)
            .expect("a freed node must be transactionally mutable at once");
        guard.commit_txn(txn).unwrap();
    }

    /// Declaring from a query must not capture nodes that already exist: they
    /// may carry a delta chain, and the fast path skips resolving it (issue
    /// #61). The engine records the boundary; this pins that the DDL path
    /// inherits it rather than declaring retroactively.
    #[test]
    fn declaring_from_a_query_does_not_capture_pre_existing_nodes() {
        let g = make_graph();
        let old = {
            let mut guard = g.write().unwrap();
            guard
                .add_node("Event", ermya_graph::Properties::new())
                .unwrap()
        };

        dispatch_ddl(
            ermya_graph::gql::DdlStatement::SetLabelAppendOnly {
                label: "Event".to_owned(),
                on: true,
            },
            &g,
        )
        .unwrap();

        let mut guard = g.write().unwrap();
        assert!(
            guard.schema_catalog().is_label_append_only("Event"),
            "the label is declared from now on"
        );

        // The pre-existing node stays ordinary: a transactional write on it is
        // accepted, which an exempt node would refuse.
        guard.enable_mvcc();
        let txn = guard.begin_txn().unwrap();
        guard
            .remove_node_in_txn(txn, old)
            .expect("a node predating the declaration must stay mutable");
        guard.commit_txn(txn).unwrap();
    }

    #[test]
    fn show_append_only_info_lists_declared_labels() {
        let g = make_graph();
        for label in ["Event", "AuditTrail"] {
            dispatch_ddl(
                ermya_graph::gql::DdlStatement::SetLabelAppendOnly {
                    label: label.to_owned(),
                    on: true,
                },
                &g,
            )
            .unwrap();
        }

        let result =
            dispatch_ddl(ermya_graph::gql::DdlStatement::ShowAppendOnlyInfo, &g).unwrap();

        assert_eq!(
            result.fields_psv,
            vec![PackStreamValue::String("label".to_owned())]
        );
        let mut listed: Vec<String> = result
            .rows
            .iter()
            .map(|r| match &r[0] {
                PackStreamValue::String(s) => s.clone(),
                other => panic!("expected a string label, got {other:?}"),
            })
            .collect();
        listed.sort();
        assert_eq!(listed, vec!["AuditTrail".to_owned(), "Event".to_owned()]);
    }

    #[test]
    fn show_append_only_info_is_empty_when_nothing_is_declared() {
        let g = make_graph();
        let result =
            dispatch_ddl(ermya_graph::gql::DdlStatement::ShowAppendOnlyInfo, &g).unwrap();
        assert!(result.rows.is_empty());
        // The column schema is still reported, so a client can bind to it.
        assert_eq!(result.fields_psv.len(), 1);
    }

    #[test]
    fn create_index_legacy_adds_to_catalog() {
        let g = make_graph();
        let stmt = ermya_graph::gql::DdlStatement::CreateIndexLegacy {
            label: "Person".to_owned(),
            prop: "id".to_owned(),
        };
        let result = dispatch_ddl(stmt, &g).unwrap();
        // Mutation DDL returns empty fields/rows.
        assert!(result.fields_psv.is_empty());
        assert!(result.rows.is_empty());
        // Catalog must reflect the declaration.
        let guard = g.read().unwrap();
        assert!(guard.schema_catalog().has_index("Person", "id"));
    }

    #[test]
    fn create_index_for_adds_to_catalog() {
        let g = make_graph();
        let stmt = ermya_graph::gql::DdlStatement::CreateIndexFor {
            label: "Person".to_owned(),
            prop: "email".to_owned(),
        };
        dispatch_ddl(stmt, &g).unwrap();
        assert!(
            g.read()
                .unwrap()
                .schema_catalog()
                .has_index("Person", "email")
        );
    }

    #[test]
    fn drop_index_removes_from_catalog() {
        let g = make_graph();
        g.write()
            .unwrap()
            .schema_catalog_mut()
            .add_index("Person", "id");
        let stmt = ermya_graph::gql::DdlStatement::DropIndex {
            label: "Person".to_owned(),
            prop: "id".to_owned(),
        };
        dispatch_ddl(stmt, &g).unwrap();
        assert!(!g.read().unwrap().schema_catalog().has_index("Person", "id"));
    }

    #[test]
    fn create_unique_constraint_adds_to_catalog() {
        let g = make_graph();
        let stmt = ermya_graph::gql::DdlStatement::CreateUniqueConstraint {
            label: "Asset".to_owned(),
            prop: "id".to_owned(),
        };
        dispatch_ddl(stmt, &g).unwrap();
        assert!(
            g.read()
                .unwrap()
                .schema_catalog()
                .has_unique_constraint("Asset", "id")
        );
    }

    #[test]
    fn drop_constraint_removes_from_catalog() {
        let g = make_graph();
        g.write()
            .unwrap()
            .schema_catalog_mut()
            .add_unique_constraint("Asset", "id");
        let stmt = ermya_graph::gql::DdlStatement::DropConstraint {
            label: "Asset".to_owned(),
            prop: "id".to_owned(),
        };
        dispatch_ddl(stmt, &g).unwrap();
        assert!(
            !g.read()
                .unwrap()
                .schema_catalog()
                .has_unique_constraint("Asset", "id")
        );
    }

    #[test]
    fn show_index_info_returns_tabular_rows() {
        let g = make_graph();
        {
            let mut guard = g.write().unwrap();
            guard.schema_catalog_mut().add_index("Person", "id");
            guard.schema_catalog_mut().add_index("Asset", "status");
        }
        let result = dispatch_ddl(ermya_graph::gql::DdlStatement::ShowIndexInfo, &g).unwrap();
        // Must have column headers.
        assert!(
            !result.fields_psv.is_empty(),
            "SHOW INDEX INFO must return column headers"
        );
        // Must have one row per declared index.
        assert_eq!(
            result.rows.len(),
            2,
            "expected 2 index rows, got {}",
            result.rows.len()
        );
    }

    #[test]
    fn show_constraint_info_returns_tabular_rows() {
        let g = make_graph();
        g.write()
            .unwrap()
            .schema_catalog_mut()
            .add_unique_constraint("Asset", "id");
        let result =
            dispatch_ddl(ermya_graph::gql::DdlStatement::ShowConstraintInfo, &g).unwrap();
        assert!(!result.fields_psv.is_empty());
        assert_eq!(result.rows.len(), 1);
    }

    #[test]
    fn show_index_info_empty_returns_zero_rows() {
        let g = make_graph();
        let result = dispatch_ddl(ermya_graph::gql::DdlStatement::ShowIndexInfo, &g).unwrap();
        assert!(result.rows.is_empty());
    }

    #[test]
    fn create_index_persists_schema_bin() {
        use ermya_graph::GraphConfig;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db");
        let cfg = GraphConfig::default();
        {
            let graph = ermya_graph::Graph::open(&path, &cfg).unwrap();
            let arc = Arc::new(RwLock::new(graph));
            let stmt = ermya_graph::gql::DdlStatement::CreateIndexLegacy {
                label: "Asset".to_owned(),
                prop: "id".to_owned(),
            };
            // No explicit flush — dispatch_ddl must persist schema.bin immediately.
            dispatch_ddl(stmt, &arc).unwrap();
        }
        {
            let g = ermya_graph::Graph::open(&path, &cfg).unwrap();
            assert!(
                g.schema_catalog().has_index("Asset", "id"),
                "index must survive reopen without explicit flush"
            );
        }
    }

    #[test]
    fn create_constraint_persists_schema_bin() {
        use ermya_graph::GraphConfig;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db");
        let cfg = GraphConfig::default();
        {
            let graph = ermya_graph::Graph::open(&path, &cfg).unwrap();
            let arc = Arc::new(RwLock::new(graph));
            let stmt = ermya_graph::gql::DdlStatement::CreateUniqueConstraint {
                label: "Asset".to_owned(),
                prop: "id".to_owned(),
            };
            dispatch_ddl(stmt, &arc).unwrap();
        }
        {
            let g = ermya_graph::Graph::open(&path, &cfg).unwrap();
            assert!(
                g.schema_catalog().has_unique_constraint("Asset", "id"),
                "constraint must survive reopen without explicit flush"
            );
        }
    }
}
