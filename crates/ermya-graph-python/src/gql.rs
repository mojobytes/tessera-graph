// SPDX-License-Identifier: MIT

//! GQL query execution and validation for the Python bridge.

use std::collections::HashMap;

use pyo3::exceptions::PyKeyError;
use pyo3::prelude::*;

use crate::errors::to_py_err;
use crate::graph::PyGraph;

/// Executes a GQL read-only query against the graph and returns results.
///
/// Parses the query string, compiles and executes it.
#[pyfunction]
pub fn execute(py: Python<'_>, graph: &PyGraph, query: &str) -> PyResult<PyGqlResult> {
    let parsed = ermya_graph::gql::parse(query).map_err(to_py_err)?;
    // max_rows = 0 (cap disabled): the result-row cap is a Bolt-server
    // policy for the multi-tenant service. Embedded use controls its own
    // memory, so the PyO3 binding opts out.
    let rows = ermya_graph::gql::execute(&graph.inner, &parsed, 0).map_err(to_py_err)?;

    let py_rows: Vec<PyGqlRow> = rows
        .into_iter()
        .map(|row| {
            let inner: HashMap<String, Py<PyAny>> = row
                .into_iter()
                .map(|(k, v)| (k, gql_value_to_py(py, &v)))
                .collect();
            PyGqlRow { inner }
        })
        .collect();

    Ok(PyGqlResult {
        rows: py_rows,
        mutations: None,
    })
}

/// Validates a GQL query or mutation string without executing.
///
/// Returns `True` if the syntax is valid. Raises `GqlSyntaxError` otherwise.
#[pyfunction]
pub fn validate(query: &str) -> PyResult<bool> {
    ermya_graph::gql::parse_statement(query).map_err(to_py_err)?;
    Ok(true)
}

// ── Result types ────────────────────────────────────────────────────────────

/// The complete result of a GQL query execution.
#[pyclass(name = "GqlResult", frozen)]
pub struct PyGqlResult {
    rows: Vec<PyGqlRow>,
    mutations: Option<PyGqlMutationResult>,
}

#[pymethods]
impl PyGqlResult {
    /// The result rows. Note: creates a new list on each access.
    /// For indexed access use `result[i]` instead.
    #[getter]
    fn rows(&self) -> Vec<PyGqlRow> {
        self.rows.clone()
    }

    /// The mutation summary, or `None` for read-only queries.
    ///
    /// Currently always `None` — mutation execution (CREATE, DELETE, SET)
    /// is not yet implemented in the open-source GQL compiler.
    // TODO: populate when mutation executor is added to ermya-graph
    #[getter]
    fn mutations(&self) -> Option<PyGqlMutationResult> {
        self.mutations.clone()
    }

    /// Access a row by index without cloning the entire list.
    #[allow(clippy::cast_possible_wrap, clippy::cast_sign_loss)]
    fn __getitem__(&self, index: isize) -> PyResult<PyGqlRow> {
        let len = self.rows.len() as isize;
        let idx = if index < 0 { len + index } else { index };
        if idx < 0 || idx >= len {
            return Err(pyo3::exceptions::PyIndexError::new_err(
                "row index out of range",
            ));
        }
        Ok(self.rows[idx as usize].clone())
    }

    fn __len__(&self) -> usize {
        self.rows.len()
    }

    fn __bool__(&self) -> bool {
        !self.rows.is_empty()
    }

    fn __iter__(&self) -> PyGqlResultIter {
        PyGqlResultIter {
            rows: self.rows.clone(),
            index: 0,
        }
    }

    fn __repr__(&self) -> String {
        format!("GqlResult(rows={})", self.rows.len())
    }
}

#[pyclass]
pub struct PyGqlResultIter {
    rows: Vec<PyGqlRow>,
    index: usize,
}

#[pymethods]
impl PyGqlResultIter {
    fn __iter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    fn __next__(&mut self) -> Option<PyGqlRow> {
        if self.index < self.rows.len() {
            let row = self.rows[self.index].clone();
            self.index += 1;
            Some(row)
        } else {
            None
        }
    }
}

/// A single result row: column name → Python value.
#[pyclass(name = "GqlRow", frozen, from_py_object)]
pub struct PyGqlRow {
    inner: HashMap<String, Py<PyAny>>,
}

impl Clone for PyGqlRow {
    fn clone(&self) -> Self {
        Python::attach(|py| Self {
            inner: self
                .inner
                .iter()
                .map(|(k, v)| (k.clone(), v.clone_ref(py)))
                .collect(),
        })
    }
}

#[pymethods]
impl PyGqlRow {
    fn __getitem__(&self, py: Python<'_>, key: &str) -> PyResult<Py<PyAny>> {
        self.inner
            .get(key)
            .map(|v| v.clone_ref(py))
            .ok_or_else(|| PyKeyError::new_err(key.to_owned()))
    }

    /// Returns all column keys.
    fn keys(&self) -> Vec<String> {
        self.inner.keys().cloned().collect()
    }

    fn __repr__(&self) -> String {
        let keys: Vec<&str> = self.inner.keys().map(String::as_str).collect();
        format!("GqlRow(columns={keys:?})")
    }
}

/// Summary of mutations applied (currently unused — reserved for future mutation support).
#[pyclass(name = "GqlMutationResult", frozen, from_py_object)]
#[derive(Clone)]
pub struct PyGqlMutationResult {
    #[pyo3(get)]
    nodes_created: u64,
    #[pyo3(get)]
    edges_created: u64,
    #[pyo3(get)]
    nodes_deleted: u64,
    #[pyo3(get)]
    edges_deleted: u64,
    #[pyo3(get)]
    properties_set: u64,
}

// ── Value conversion ────────────────────────────────────────────────────────

/// Converts a `GqlValue` to a native Python object.
fn gql_value_to_py(py: Python<'_>, val: &ermya_graph::GqlValue) -> Py<PyAny> {
    use ermya_graph::GqlValue;
    match val {
        GqlValue::Null => py.None(),
        GqlValue::Bool(b) => b
            .into_pyobject(py)
            .expect("bool to py")
            .to_owned()
            .into_any()
            .unbind(),
        GqlValue::Int(i) => i.into_pyobject(py).expect("int to py").into_any().unbind(),
        GqlValue::Float(f) => f
            .into_pyobject(py)
            .expect("float to py")
            .into_any()
            .unbind(),
        GqlValue::Str(s) => s.into_pyobject(py).expect("str to py").into_any().unbind(),
        GqlValue::List(items) => {
            let py_items: Vec<Py<PyAny>> = items.iter().map(|v| gql_value_to_py(py, v)).collect();
            pyo3::types::PyList::new(py, &py_items)
                .expect("list to py")
                .into_any()
                .unbind()
        }
        GqlValue::Map(entries) => {
            // A property map (e.g. `MERGE (...) RETURN n`) surfaces as a Python
            // dict, recursing on each value just like the list case.
            let dict = pyo3::types::PyDict::new(py);
            for (k, v) in entries {
                dict.set_item(k, gql_value_to_py(py, v))
                    .expect("dict set_item");
            }
            dict.into_any().unbind()
        }
        // Graph-entity values (Fase B). The embedded Python binding has no
        // driver-style Node/Relationship/Path types, so they surface as plain
        // dicts — the same representation a user would build by hand, and
        // consistent with the Map → dict case above.
        GqlValue::Node(n) => gql_node_to_py(py, n),
        GqlValue::Relationship(r) => gql_relationship_to_py(py, r),
        GqlValue::Path(p) => gql_path_to_py(py, p),
    }
}

/// Converts a `GqlNode` to a Python dict `{id, labels, properties}`.
fn gql_node_to_py(py: Python<'_>, n: &ermya_graph::gql::GqlNode) -> Py<PyAny> {
    let dict = pyo3::types::PyDict::new(py);
    dict.set_item("id", n.id).expect("node id");
    dict.set_item("labels", n.labels.clone())
        .expect("node labels");
    dict.set_item("properties", props_to_py(py, &n.props))
        .expect("node properties");
    dict.into_any().unbind()
}

/// Converts a `GqlRelationship` to a dict `{id, type, start, end, properties}`.
fn gql_relationship_to_py(py: Python<'_>, r: &ermya_graph::gql::GqlRelationship) -> Py<PyAny> {
    let dict = pyo3::types::PyDict::new(py);
    dict.set_item("id", r.id).expect("rel id");
    dict.set_item("type", r.rel_type.clone()).expect("rel type");
    dict.set_item("start", r.start_id).expect("rel start");
    dict.set_item("end", r.end_id).expect("rel end");
    dict.set_item("properties", props_to_py(py, &r.props))
        .expect("rel properties");
    dict.into_any().unbind()
}

/// Converts a `GqlPath` to a dict `{nodes, relationships}` (lists of dicts).
fn gql_path_to_py(py: Python<'_>, p: &ermya_graph::gql::GqlPath) -> Py<PyAny> {
    let dict = pyo3::types::PyDict::new(py);
    let nodes: Vec<Py<PyAny>> = p.nodes.iter().map(|n| gql_node_to_py(py, n)).collect();
    let rels: Vec<Py<PyAny>> = p
        .rels
        .iter()
        .map(|r| gql_relationship_to_py(py, r))
        .collect();
    dict.set_item(
        "nodes",
        pyo3::types::PyList::new(py, &nodes).expect("path nodes"),
    )
    .expect("path nodes set");
    dict.set_item(
        "relationships",
        pyo3::types::PyList::new(py, &rels).expect("path rels"),
    )
    .expect("path rels set");
    dict.into_any().unbind()
}

/// Converts an entity's property map to a Python dict, recursing on values.
fn props_to_py(
    py: Python<'_>,
    props: &std::collections::HashMap<String, ermya_graph::GqlValue>,
) -> Py<PyAny> {
    let dict = pyo3::types::PyDict::new(py);
    for (k, v) in props {
        dict.set_item(k, gql_value_to_py(py, v))
            .expect("prop set_item");
    }
    dict.into_any().unbind()
}
