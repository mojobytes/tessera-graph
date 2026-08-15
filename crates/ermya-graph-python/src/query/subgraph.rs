// SPDX-License-Identifier: MIT

//! `PySubgraphQuery` — Python wrapper for `SubgraphQuery`.

use pyo3::prelude::*;

use crate::errors::to_py_err;
use crate::graph::{PyGraph, to_node_id};
use crate::types::direction::{self, PyDirection};
use crate::types::node_id::PyNodeId;
use crate::types::subgraph::PySubgraph;

/// Builder for extracting a subgraph by traversal from a start node.
#[pyclass(name = "SubgraphQuery")]
pub struct PySubgraphQuery {
    graph: Py<PyGraph>,
    start: PyNodeId,
    direction: PyDirection,
    label_filter: Option<String>,
    max_depth: Option<usize>,
}

impl PySubgraphQuery {
    pub fn new(graph: Py<PyGraph>, start: PyNodeId) -> Self {
        Self {
            graph,
            start,
            direction: PyDirection::Both,
            label_filter: None,
            max_depth: None,
        }
    }
}

#[pymethods]
impl PySubgraphQuery {
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

    /// Sets the maximum traversal depth.
    fn max_depth(mut slf: PyRefMut<'_, Self>, depth: usize) -> Py<Self> {
        slf.max_depth = Some(depth);
        slf.into()
    }

    /// Executes the extraction and returns a `Subgraph`.
    fn extract(&self, py: Python<'_>) -> PyResult<PySubgraph> {
        let g = self.graph.borrow(py);
        let mut b = g.inner.subgraph(to_node_id(&self.start));
        b = b.direction(self.direction.into());
        if let Some(ref l) = self.label_filter {
            b = b.label(l.as_str());
        }
        if let Some(d) = self.max_depth {
            b = b.max_depth(d);
        }
        let sg = b.extract().map_err(to_py_err)?;
        Ok(PySubgraph::from_rust(sg))
    }
}
