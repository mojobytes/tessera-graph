// SPDX-License-Identifier: MIT

use pyo3::prelude::*;

/// An edge identifier wrapping a `u64`.
#[pyclass(name = "EdgeId", frozen, eq, hash, from_py_object)]
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct PyEdgeId {
    pub(crate) value: u64,
}

#[pymethods]
impl PyEdgeId {
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
        format!("EdgeId({})", self.value)
    }

    fn __int__(&self) -> u64 {
        self.value
    }
}

impl From<tessera_graph::EdgeId> for PyEdgeId {
    fn from(id: tessera_graph::EdgeId) -> Self {
        Self { value: id.as_u64() }
    }
}
