// SPDX-License-Identifier: MIT

use pyo3::prelude::*;

/// A node identifier wrapping a `u64`.
#[pyclass(name = "NodeId", frozen, eq, hash, from_py_object)]
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct PyNodeId {
    pub(crate) value: u64,
}

#[pymethods]
impl PyNodeId {
    #[new]
    fn new(value: u64) -> Self {
        Self { value }
    }

    /// The underlying integer value.
    #[getter]
    const fn value(&self) -> u64 {
        self.value
    }

    fn __repr__(&self) -> String {
        format!("NodeId({})", self.value)
    }

    fn __int__(&self) -> u64 {
        self.value
    }
}

impl From<ermya_graph::NodeId> for PyNodeId {
    fn from(id: ermya_graph::NodeId) -> Self {
        Self { value: id.as_u64() }
    }
}
