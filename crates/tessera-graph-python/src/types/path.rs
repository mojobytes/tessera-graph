// SPDX-License-Identifier: LicenseRef-TesseraGraph-Proprietary

use pyo3::prelude::*;

use super::edge_id::PyEdgeId;
use super::node_id::PyNodeId;

/// An ordered sequence of nodes and edges forming a path in the graph.
#[pyclass(name = "Path", frozen)]
#[derive(Clone)]
pub struct PyPath {
    pub(crate) inner: tessera_graph::Path,
}

#[pymethods]
impl PyPath {
    /// Returns node IDs in path order.
    fn nodes(&self) -> Vec<PyNodeId> {
        self.inner.nodes().iter().copied().map(PyNodeId::from).collect()
    }

    /// Returns edge IDs in path order.
    fn edges(&self) -> Vec<PyEdgeId> {
        self.inner.edges().iter().copied().map(PyEdgeId::from).collect()
    }

    /// Returns the first node, or `None` if empty.
    fn start(&self) -> Option<PyNodeId> {
        self.inner.start().map(PyNodeId::from)
    }

    /// Returns the last node, or `None` if empty.
    #[pyo3(name = "end")]
    fn end_node(&self) -> Option<PyNodeId> {
        self.inner.end().map(PyNodeId::from)
    }

    /// Returns `True` if the path has no edges.
    #[getter]
    fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// Number of edges (hops) in the path.
    fn __len__(&self) -> usize {
        self.inner.len()
    }

    /// A path with edges is truthy; an empty path is falsy.
    fn __bool__(&self) -> bool {
        !self.inner.is_empty()
    }

    /// Iterates over node IDs in the path.
    fn __iter__(&self) -> PyPathIter {
        PyPathIter {
            nodes: self.inner.nodes().iter().copied().map(PyNodeId::from).collect(),
            index: 0,
        }
    }

    fn __repr__(&self) -> String {
        format!("Path(nodes={}, edges={})", self.inner.nodes().len(), self.inner.edges().len())
    }
}

impl From<tessera_graph::Path> for PyPath {
    fn from(path: tessera_graph::Path) -> Self {
        Self { inner: path }
    }
}

/// Iterator over node IDs in a `Path`.
#[pyclass]
pub struct PyPathIter {
    nodes: Vec<PyNodeId>,
    index: usize,
}

#[pymethods]
impl PyPathIter {
    fn __iter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    fn __next__(&mut self) -> Option<PyNodeId> {
        if self.index < self.nodes.len() {
            let val = self.nodes[self.index];
            self.index += 1;
            Some(val)
        } else {
            None
        }
    }
}
