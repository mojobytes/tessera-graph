// SPDX-License-Identifier: MIT

use pyo3::prelude::*;
use pyo3::types::PyDict;

use super::edge_id::PyEdgeId;
use super::node_id::PyNodeId;
use super::properties;

/// A directed edge (relationship) snapshot from the graph.
#[pyclass(name = "Edge", frozen, from_py_object)]
#[derive(Clone)]
pub struct PyEdge {
    pub(crate) inner: ermya_graph::Edge,
}

#[pymethods]
impl PyEdge {
    /// The unique identifier of this edge.
    fn id(&self) -> PyEdgeId {
        self.inner.id().into()
    }

    /// The label (relationship type) of this edge.
    fn label(&self) -> &str {
        self.inner.label()
    }

    /// The source (origin) node identifier.
    fn source(&self) -> PyNodeId {
        self.inner.source().into()
    }

    /// The target (destination) node identifier.
    fn target(&self) -> PyNodeId {
        self.inner.target().into()
    }

    /// The properties as a Python dict.
    fn properties<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        properties::to_py_dict(py, self.inner.properties())
    }

    fn __repr__(&self) -> String {
        format!(
            "Edge(id={}, label={:?}, source={}, target={})",
            self.inner.id().as_u64(),
            self.inner.label(),
            self.inner.source().as_u64(),
            self.inner.target().as_u64(),
        )
    }

    fn __eq__(&self, other: &Self) -> bool {
        self.inner.id() == other.inner.id()
    }

    fn __hash__(&self) -> u64 {
        self.inner.id().as_u64()
    }
}

impl From<ermya_graph::Edge> for PyEdge {
    fn from(edge: ermya_graph::Edge) -> Self {
        Self { inner: edge }
    }
}
