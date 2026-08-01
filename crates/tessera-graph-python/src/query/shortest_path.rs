// SPDX-License-Identifier: LicenseRef-TesseraGraph-Proprietary

//! `PyShortestPathQuery` — Python wrapper for `ShortestPathQuery`.

use pyo3::prelude::*;

use crate::errors::to_py_err;
use crate::graph::{PyGraph, to_node_id};
use crate::types::direction::{self, PyDirection};
use crate::types::node_id::PyNodeId;
use crate::types::path::PyPath;

/// Builder for finding the shortest unweighted path between two nodes (BFS).
#[pyclass(name = "ShortestPathQuery")]
pub struct PyShortestPathQuery {
    graph: Py<PyGraph>,
    from: PyNodeId,
    to: PyNodeId,
    direction: PyDirection,
    label_filter: Option<String>,
}

impl PyShortestPathQuery {
    pub fn new(graph: Py<PyGraph>, from: PyNodeId, to: PyNodeId) -> Self {
        Self {
            graph,
            from,
            to,
            direction: PyDirection::Both,
            label_filter: None,
        }
    }
}

#[pymethods]
impl PyShortestPathQuery {
    /// Sets the traversal direction. Accepts `Direction` enum or string.
    fn direction(mut slf: PyRefMut<'_, Self>, val: &Bound<'_, PyAny>) -> PyResult<Py<Self>> {
        slf.direction = direction::coerce(val)?;
        Ok(slf.into())
    }

    /// Filters to edges with the given label.
    fn label(mut slf: PyRefMut<'_, Self>, label: &str) -> Py<Self> {
        slf.label_filter = Some(label.to_owned());
        slf.into()
    }

    /// Executes BFS and returns the shortest `Path`, or `None` if unreachable.
    fn find(&self, py: Python<'_>) -> PyResult<Option<PyPath>> {
        let g = self.graph.borrow(py);
        let mut b = g.inner.shortest_path(to_node_id(&self.from), to_node_id(&self.to));
        b = b.direction(self.direction.into());
        if let Some(ref l) = self.label_filter {
            b = b.label(l.as_str());
        }
        let result = b.find().map_err(to_py_err)?;
        Ok(result.map(PyPath::from))
    }
}
