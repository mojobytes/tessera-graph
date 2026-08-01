// SPDX-License-Identifier: LicenseRef-TesseraGraph-Proprietary

//! `PyTraversalBuilder` — Python wrapper for `TraversalBuilder`.

use pyo3::prelude::*;

use crate::errors::to_py_err;
use crate::graph::{PyGraph, to_node_id};
use crate::types::direction::{self, PyDirection};
use crate::types::node_id::PyNodeId;
use crate::types::path::PyPath;
use crate::types::strategy::PyStrategy;

/// Builder for graph traversals (BFS/DFS).
///
/// Stores configuration and executes the Rust builder in terminal methods.
#[pyclass(name = "TraversalBuilder")]
pub struct PyTraversalBuilder {
    graph: Py<PyGraph>,
    start: PyNodeId,
    direction: PyDirection,
    label_filter: Option<String>,
    max_depth: Option<usize>,
    strategy: PyStrategy,
}

impl PyTraversalBuilder {
    pub fn new(graph: Py<PyGraph>, start: PyNodeId) -> Self {
        Self {
            graph,
            start,
            direction: PyDirection::Both,
            label_filter: None,
            max_depth: None,
            strategy: PyStrategy::Bfs,
        }
    }
}

#[pymethods]
impl PyTraversalBuilder {
    /// Sets the traversal direction. Accepts `Direction` enum or string.
    fn direction(mut slf: PyRefMut<'_, Self>, val: &Bound<'_, PyAny>) -> PyResult<Py<Self>> {
        slf.direction = direction::coerce(val)?;
        Ok(slf.into())
    }

    /// Filters traversal to edges with the given label.
    fn label(mut slf: PyRefMut<'_, Self>, label: &str) -> Py<Self> {
        slf.label_filter = Some(label.to_owned());
        slf.into()
    }

    /// Sets the maximum traversal depth.
    fn max_depth(mut slf: PyRefMut<'_, Self>, depth: usize) -> Py<Self> {
        slf.max_depth = Some(depth);
        slf.into()
    }

    /// Uses breadth-first search strategy.
    fn bfs(mut slf: PyRefMut<'_, Self>) -> Py<Self> {
        slf.strategy = PyStrategy::Bfs;
        slf.into()
    }

    /// Uses depth-first search strategy.
    fn dfs(mut slf: PyRefMut<'_, Self>) -> Py<Self> {
        slf.strategy = PyStrategy::Dfs;
        slf.into()
    }

    /// Executes the traversal and returns visited node IDs in order.
    fn collect(&self, py: Python<'_>) -> PyResult<Vec<PyNodeId>> {
        let g = self.graph.borrow(py);
        let builder = self.build_rust(&g.inner);
        let ids = builder.collect().map_err(to_py_err)?;
        Ok(ids.into_iter().map(PyNodeId::from).collect())
    }

    /// Executes the traversal and returns full paths to each visited node.
    fn collect_paths(&self, py: Python<'_>) -> PyResult<Vec<PyPath>> {
        let g = self.graph.borrow(py);
        let builder = self.build_rust(&g.inner);
        let paths = builder.collect_paths().map_err(to_py_err)?;
        Ok(paths.into_iter().map(PyPath::from).collect())
    }
}

impl PyTraversalBuilder {
    /// Reconstructs the Rust `TraversalBuilder` from stored configuration.
    fn build_rust<'g>(
        &self,
        graph: &'g tessera_graph::Graph,
    ) -> tessera_graph::TraversalBuilder<'g, tessera_graph::Graph> {
        let mut b = graph.traverse(to_node_id(&self.start));
        b = b.direction(self.direction.into());
        if let Some(ref l) = self.label_filter {
            b = b.label(l.as_str());
        }
        if let Some(d) = self.max_depth {
            b = b.max_depth(d);
        }
        b = match self.strategy {
            PyStrategy::Bfs => b.bfs(),
            PyStrategy::Dfs => b.dfs(),
        };
        b
    }
}
