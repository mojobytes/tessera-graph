// SPDX-License-Identifier: LicenseRef-TesseraGraph-Proprietary

//! Batch context manager for `PyGraph`.

use pyo3::prelude::*;

use crate::errors::to_py_err;
use crate::graph::PyGraph;

/// Context manager that wraps `begin_batch` / `end_batch`.
///
/// Usage:
/// ```python
/// with graph.batch():
///     graph.add_node("N", {})
/// ```
#[pyclass(name = "BatchContext")]
pub struct PyBatchContext {
    graph: Py<PyGraph>,
}

impl PyBatchContext {
    pub fn new(graph: Py<PyGraph>) -> Self {
        Self { graph }
    }
}

#[pymethods]
impl PyBatchContext {
    fn __enter__(&self, py: Python<'_>) -> Option<PyObject> {
        self.graph.borrow_mut(py).inner.begin_batch();
        None
    }

    #[pyo3(signature = (_exc_type=None, _exc_val=None, _exc_tb=None))]
    fn __exit__(
        &self,
        py: Python<'_>,
        _exc_type: Option<&Bound<'_, PyAny>>,
        _exc_val: Option<&Bound<'_, PyAny>>,
        _exc_tb: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<bool> {
        self.graph
            .borrow_mut(py)
            .inner
            .end_batch()
            .map_err(to_py_err)?;
        // Do not suppress exceptions
        Ok(false)
    }
}
