// SPDX-License-Identifier: LicenseRef-TesseraGraph-Proprietary

//! `PyNeighborQuery` — Python wrapper for `NeighborQuery`.

use pyo3::prelude::*;

use crate::errors::to_py_err;
use crate::graph::{PyGraph, to_node_id};
use crate::types::direction::{self, PyDirection};
use crate::types::edge::PyEdge;
use crate::types::node_id::PyNodeId;

/// Builder for querying the neighbors of a node.
///
/// Stores configuration and executes the Rust builder in `collect()`.
#[pyclass(name = "NeighborQuery")]
pub struct PyNeighborQuery {
    graph: Py<PyGraph>,
    node: PyNodeId,
    direction: PyDirection,
    label_filter: Option<String>,
}

impl PyNeighborQuery {
    pub fn new(graph: Py<PyGraph>, node: PyNodeId) -> Self {
        Self {
            graph,
            node,
            direction: PyDirection::Both,
            label_filter: None,
        }
    }
}

#[pymethods]
impl PyNeighborQuery {
    /// Sets the traversal direction. Accepts `Direction` enum or string.
    fn direction(mut slf: PyRefMut<'_, Self>, val: &Bound<'_, PyAny>) -> PyResult<Py<Self>> {
        slf.direction = direction::coerce(val)?;
        Ok(slf.into())
    }

    /// Filters by edge label.
    fn label(mut slf: PyRefMut<'_, Self>, label: &str) -> Py<Self> {
        slf.label_filter = Some(label.to_owned());
        slf.into()
    }

    /// Executes the query and returns matching edges.
    fn collect(&self, py: Python<'_>) -> PyResult<Vec<PyEdge>> {
        let g = self.graph.borrow(py);
        let mut builder = g.inner.neighbors(to_node_id(&self.node));
        builder = builder.direction(self.direction.into());
        if let Some(ref l) = self.label_filter {
            builder = builder.label(l.as_str());
        }
        let edges = builder.collect().map_err(to_py_err)?;
        Ok(edges.into_iter().map(PyEdge::from).collect())
    }

    /// Executes the query and returns neighbor node IDs.
    fn node_ids(&self, py: Python<'_>) -> PyResult<Vec<PyNodeId>> {
        let g = self.graph.borrow(py);
        let mut builder = g.inner.neighbors(to_node_id(&self.node));
        builder = builder.direction(self.direction.into());
        if let Some(ref l) = self.label_filter {
            builder = builder.label(l.as_str());
        }
        let ids = builder.node_ids().map_err(to_py_err)?;
        Ok(ids.into_iter().map(PyNodeId::from).collect())
    }
}
