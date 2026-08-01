// SPDX-License-Identifier: LicenseRef-TesseraGraph-Proprietary

//! `PyWeightedPathQuery` — Python wrapper for `WeightedPathQuery`.
//!
//! The `weight()` method accepts a Python callable `Callable[[Edge], float]`.
//! During Dijkstra execution, each candidate edge is converted to a `PyEdge`
//! and passed to the callable across the `PyO3` boundary.

use pyo3::prelude::*;

use crate::errors::to_py_err;
use crate::graph::{PyGraph, to_node_id};
use crate::types::direction::{self, PyDirection};
use crate::types::edge::PyEdge;
use crate::types::node_id::PyNodeId;
use crate::types::path::PyPath;

/// Builder for finding the shortest weighted path using Dijkstra.
#[pyclass(name = "WeightedPathQuery")]
pub struct PyWeightedPathQuery {
    graph: Py<PyGraph>,
    from: PyNodeId,
    to: PyNodeId,
    direction: PyDirection,
    label_filter: Option<String>,
    weight_fn: Option<PyObject>,
}

impl PyWeightedPathQuery {
    pub fn new(graph: Py<PyGraph>, from: PyNodeId, to: PyNodeId) -> Self {
        Self {
            graph,
            from,
            to,
            direction: PyDirection::Both,
            label_filter: None,
            weight_fn: None,
        }
    }
}

#[pymethods]
impl PyWeightedPathQuery {
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

    /// Sets the weight function. Accepts any `Callable[[Edge], float]`.
    fn weight(mut slf: PyRefMut<'_, Self>, callable: PyObject) -> Py<Self> {
        slf.weight_fn = Some(callable);
        slf.into()
    }

    /// Executes Dijkstra and returns `(cost, Path)` or `None` if unreachable.
    fn find(&self, py: Python<'_>) -> PyResult<Option<(f64, PyPath)>> {
        let g = self.graph.borrow(py);
        let from = to_node_id(&self.from);
        let to = to_node_id(&self.to);

        if let Some(ref callable) = self.weight_fn {
            // Build with Python callable as weight function.
            // The GIL is already held via `py` — no need for `Python::with_gil`.
            let callable = callable.clone_ref(py);
            let weight_fn = |edge: &tessera_graph::Edge| -> f64 {
                let py_edge = PyEdge::from(edge.clone());
                callable
                    .call1(py, (py_edge,))
                    .and_then(|result| result.extract::<f64>(py))
                    .unwrap_or(1.0)
            };

            let mut b = g.inner.weighted_shortest_path(from, to);
            b = b.direction(self.direction.into());
            if let Some(ref l) = self.label_filter {
                b = b.label(l.as_str());
            }
            let b = b.weight(weight_fn);
            let result = b.find().map_err(to_py_err)?;
            Ok(result.map(|(cost, path)| (cost, PyPath::from(path))))
        } else {
            // Default unit weights.
            let mut b = g.inner.weighted_shortest_path(from, to);
            b = b.direction(self.direction.into());
            if let Some(ref l) = self.label_filter {
                b = b.label(l.as_str());
            }
            let result = b.find().map_err(to_py_err)?;
            Ok(result.map(|(cost, path)| (cost, PyPath::from(path))))
        }
    }
}
