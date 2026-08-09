// SPDX-License-Identifier: MIT

use pyo3::prelude::*;

use crate::errors;

/// Traversal strategy.
#[pyclass(name = "Strategy", frozen, eq, hash, from_py_object)]
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub enum PyStrategy {
    #[pyo3(name = "BFS")]
    Bfs,
    #[pyo3(name = "DFS")]
    Dfs,
}

#[pymethods]
impl PyStrategy {
    /// Parses a strategy from a case-insensitive string.
    #[staticmethod]
    fn from_str(s: &str) -> PyResult<Self> {
        match s.to_ascii_lowercase().as_str() {
            "bfs" => Ok(Self::Bfs),
            "dfs" => Ok(Self::Dfs),
            _ => Err(errors::TesseraError::new_err(format!(
                "invalid strategy: {s:?} (expected bfs or dfs)"
            ))),
        }
    }

    fn __repr__(&self) -> &'static str {
        match self {
            Self::Bfs => "Strategy.BFS",
            Self::Dfs => "Strategy.DFS",
        }
    }
}

impl From<PyStrategy> for tessera_graph::Strategy {
    fn from(s: PyStrategy) -> Self {
        match s {
            PyStrategy::Bfs => Self::Bfs,
            PyStrategy::Dfs => Self::Dfs,
        }
    }
}
