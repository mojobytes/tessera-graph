// SPDX-License-Identifier: MIT

//! `PySharedGraph` — thread-safe Python wrapper around `SharedGraph`.
//!
//! Uses `Arc<RwLock<Graph>>` internally. The read/write context managers
//! acquire the lock for each method call (no held guards across `PyO3` boundary).

use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};

use ermya_graph::Graph;
use pyo3::prelude::*;
use pyo3::types::PyDict;

use crate::errors::{self, to_py_err};
use crate::graph::{PyGraph, to_edge_id, to_node_id};
use crate::types::edge::PyEdge;
use crate::types::edge_id::PyEdgeId;
use crate::types::node::PyNode;
use crate::types::node_id::PyNodeId;
use crate::types::properties;

// ── Lock helpers ────────────────────────────────────────────────────────────

/// Acquires a read lock, converting poisoning into a Python exception.
fn read_lock(lock: &RwLock<Graph>) -> PyResult<RwLockReadGuard<'_, Graph>> {
    lock.read()
        .map_err(|_| errors::ErmyaError::new_err("shared graph lock is poisoned"))
}

/// Acquires a write lock, converting poisoning into a Python exception.
fn write_lock(lock: &RwLock<Graph>) -> PyResult<RwLockWriteGuard<'_, Graph>> {
    lock.write()
        .map_err(|_| errors::ErmyaError::new_err("shared graph lock is poisoned"))
}

// ── Macro: shared read methods ──────────────────────────────────────────────
//
// Both `ReadGuard` and `WriteGuard` expose the same read-only graph methods.
// Read methods deliberately acquire a *read* lock (not write) even on
// `WriteGuard` to maximise concurrency during read-only portions.

macro_rules! impl_shared_read_methods {
    ($ty:ident) => {
        #[pymethods]
        impl $ty {
            fn node_count(&self) -> PyResult<usize> {
                Ok(read_lock(&self.inner)?.node_count())
            }
            fn edge_count(&self) -> PyResult<usize> {
                Ok(read_lock(&self.inner)?.edge_count())
            }

            fn node_exists(&self, id: &PyNodeId) -> PyResult<bool> {
                Ok(read_lock(&self.inner)?.node_exists(to_node_id(id)))
            }

            fn node(&self, id: &PyNodeId) -> PyResult<PyNode> {
                read_lock(&self.inner)?
                    .node(to_node_id(id))
                    .map(PyNode::from)
                    .map_err(to_py_err)
            }

            fn node_ids(&self) -> PyResult<Vec<PyNodeId>> {
                Ok(read_lock(&self.inner)?
                    .node_ids()
                    .into_iter()
                    .map(PyNodeId::from)
                    .collect())
            }

            fn nodes_by_label(&self, label: &str) -> PyResult<Vec<PyNodeId>> {
                Ok(read_lock(&self.inner)?
                    .nodes_by_label(label)
                    .into_iter()
                    .map(PyNodeId::from)
                    .collect())
            }

            fn edge(&self, id: &PyEdgeId) -> PyResult<PyEdge> {
                read_lock(&self.inner)?
                    .edge(to_edge_id(id))
                    .map(PyEdge::from)
                    .map_err(to_py_err)
            }

            fn edges_by_label(&self, label: &str) -> PyResult<Vec<PyEdgeId>> {
                Ok(read_lock(&self.inner)?
                    .edges_by_label(label)
                    .into_iter()
                    .map(PyEdgeId::from)
                    .collect())
            }

            fn outgoing_edges(&self, node: &PyNodeId) -> PyResult<Vec<PyEdge>> {
                read_lock(&self.inner)?
                    .outgoing_edges(to_node_id(node))
                    .map(|v| v.into_iter().map(PyEdge::from).collect())
                    .map_err(to_py_err)
            }

            fn incoming_edges(&self, node: &PyNodeId) -> PyResult<Vec<PyEdge>> {
                read_lock(&self.inner)?
                    .incoming_edges(to_node_id(node))
                    .map(|v| v.into_iter().map(PyEdge::from).collect())
                    .map_err(to_py_err)
            }
        }
    };
}

// ── SharedGraph ─────────────────────────────────────────────────────────────

/// A thread-safe shared graph. Use `with sg.read() as g:` / `with sg.write() as g:`.
///
/// **Note:** `SharedGraph.new(graph)` moves the data out of the source `Graph`,
/// leaving it empty. The original `Graph` variable should not be reused.
#[pyclass(name = "SharedGraph")]
pub struct PySharedGraph {
    inner: Arc<RwLock<Graph>>,
}

#[pymethods]
impl PySharedGraph {
    /// Wraps a `Graph` in a thread-safe shared wrapper.
    ///
    /// **Warning:** This moves the data out of `graph`. The original `Graph`
    /// object will be empty after this call.
    #[staticmethod]
    #[pyo3(name = "new")]
    fn new_(graph: &mut PyGraph) -> Self {
        let taken = std::mem::replace(&mut graph.inner, Graph::new());
        Self {
            inner: Arc::new(RwLock::new(taken)),
        }
    }

    fn read(&self) -> PyReadGuard {
        PyReadGuard {
            inner: Arc::clone(&self.inner),
        }
    }
    fn write(&self) -> PyWriteGuard {
        PyWriteGuard {
            inner: Arc::clone(&self.inner),
        }
    }

    fn __repr__(&self) -> PyResult<String> {
        let g = read_lock(&self.inner)?;
        Ok(format!(
            "SharedGraph(nodes={}, edges={})",
            g.node_count(),
            g.edge_count()
        ))
    }
}

// ── ReadGuard ───────────────────────────────────────────────────────────────

#[pyclass(name = "ReadGuard")]
pub struct PyReadGuard {
    inner: Arc<RwLock<Graph>>,
}

impl_shared_read_methods!(PyReadGuard);

#[pymethods]
impl PyReadGuard {
    fn __enter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    #[pyo3(signature = (_exc_type=None, _exc_val=None, _exc_tb=None))]
    fn __exit__(
        &self,
        _exc_type: Option<&Bound<'_, PyAny>>,
        _exc_val: Option<&Bound<'_, PyAny>>,
        _exc_tb: Option<&Bound<'_, PyAny>>,
    ) -> bool {
        false
    }
}

// ── WriteGuard ──────────────────────────────────────────────────────────────

#[pyclass(name = "WriteGuard")]
pub struct PyWriteGuard {
    inner: Arc<RwLock<Graph>>,
}

impl_shared_read_methods!(PyWriteGuard);

#[pymethods]
impl PyWriteGuard {
    fn __enter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    #[pyo3(signature = (_exc_type=None, _exc_val=None, _exc_tb=None))]
    fn __exit__(
        &self,
        _exc_type: Option<&Bound<'_, PyAny>>,
        _exc_val: Option<&Bound<'_, PyAny>>,
        _exc_tb: Option<&Bound<'_, PyAny>>,
    ) -> bool {
        false
    }

    fn add_node(&self, label: &str, properties: &Bound<'_, PyDict>) -> PyResult<PyNodeId> {
        let props = properties::from_py_dict(properties)?;
        write_lock(&self.inner)?
            .add_node(label, props)
            .map(PyNodeId::from)
            .map_err(to_py_err)
    }

    fn add_edge(
        &self,
        label: &str,
        source: &PyNodeId,
        target: &PyNodeId,
        properties: &Bound<'_, PyDict>,
    ) -> PyResult<PyEdgeId> {
        let props = properties::from_py_dict(properties)?;
        write_lock(&self.inner)?
            .add_edge(label, to_node_id(source), to_node_id(target), props)
            .map(PyEdgeId::from)
            .map_err(to_py_err)
    }

    fn remove_node(&self, id: &PyNodeId) -> PyResult<PyNode> {
        write_lock(&self.inner)?
            .remove_node(to_node_id(id))
            .map(PyNode::from)
            .map_err(to_py_err)
    }

    fn remove_edge(&self, id: &PyEdgeId) -> PyResult<PyEdge> {
        write_lock(&self.inner)?
            .remove_edge(to_edge_id(id))
            .map(PyEdge::from)
            .map_err(to_py_err)
    }

    #[pyo3(signature = (id, *, label=None, properties=None))]
    fn update_node(
        &self,
        id: &PyNodeId,
        label: Option<&str>,
        properties: Option<&Bound<'_, PyDict>>,
    ) -> PyResult<()> {
        let rust_id = to_node_id(id);
        let mut g = write_lock(&self.inner)?;
        let mut node = g.node(rust_id).map_err(to_py_err)?;
        if let Some(l) = label {
            node.set_label(l);
        }
        if let Some(dict) = properties {
            *node.properties_mut() = properties::from_py_dict(dict)?;
        }
        g.update_node(rust_id, &node).map_err(to_py_err)
    }

    #[pyo3(signature = (id, *, label=None, properties=None))]
    fn update_edge(
        &self,
        id: &PyEdgeId,
        label: Option<&str>,
        properties: Option<&Bound<'_, PyDict>>,
    ) -> PyResult<()> {
        let rust_id = to_edge_id(id);
        let mut g = write_lock(&self.inner)?;
        let mut edge = g.edge(rust_id).map_err(to_py_err)?;
        if let Some(l) = label {
            edge.set_label(l);
        }
        if let Some(dict) = properties {
            *edge.properties_mut() = properties::from_py_dict(dict)?;
        }
        g.update_edge(rust_id, &edge).map_err(to_py_err)
    }

    fn flush(&self) -> PyResult<()> {
        write_lock(&self.inner)?.flush().map_err(to_py_err)
    }
}
