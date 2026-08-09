// SPDX-License-Identifier: MIT

use pyo3::prelude::*;
use pyo3::types::PyDict;

use super::node_id::PyNodeId;
use super::properties;

/// A node (vertex) snapshot from the graph.
#[pyclass(name = "Node", frozen, from_py_object)]
#[derive(Clone)]
pub struct PyNode {
    pub(crate) inner: tessera_graph::Node,
}

#[pymethods]
impl PyNode {
    /// The unique identifier of this node.
    fn id(&self) -> PyNodeId {
        self.inner.id().into()
    }

    /// The label (type) of this node.
    fn label(&self) -> &str {
        self.inner.label()
    }

    /// The properties as a Python dict.
    fn properties<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        properties::to_py_dict(py, self.inner.properties())
    }

    fn __repr__(&self) -> String {
        format!(
            "Node(id={}, label={:?})",
            self.inner.id().as_u64(),
            self.inner.label()
        )
    }

    fn __eq__(&self, other: &Self) -> bool {
        self.inner.id() == other.inner.id()
    }

    fn __hash__(&self) -> u64 {
        self.inner.id().as_u64()
    }
}

impl From<tessera_graph::Node> for PyNode {
    fn from(node: tessera_graph::Node) -> Self {
        Self { inner: node }
    }
}
