// SPDX-License-Identifier: LicenseRef-TesseraGraph-Proprietary

use pyo3::prelude::*;

use crate::errors::to_py_err;
use super::edge::PyEdge;
use super::node::PyNode;

/// A single match result from a pattern query, with named node/edge bindings.
#[pyclass(name = "PatternMatch", frozen)]
#[derive(Clone)]
pub struct PyPatternMatch {
    pub(crate) inner: tessera_graph::PatternMatch,
    pub(crate) node_vars: Vec<String>,
    pub(crate) edge_vars: Vec<String>,
}

#[pymethods]
impl PyPatternMatch {
    /// Returns the node bound to the given variable name.
    fn get_node(&self, var: &str) -> PyResult<PyNode> {
        let node = self.inner.get_node(var).map_err(to_py_err)?;
        Ok(PyNode::from(node.clone()))
    }

    /// Returns the edge bound to the given variable name.
    fn get_edge(&self, var: &str) -> PyResult<PyEdge> {
        let edge = self.inner.get_edge(var).map_err(to_py_err)?;
        Ok(PyEdge::from(edge.clone()))
    }

    fn __repr__(&self) -> String {
        let mut vars: Vec<&str> = self.node_vars.iter()
            .chain(self.edge_vars.iter())
            .map(String::as_str)
            .collect();
        vars.sort_unstable();
        format!("PatternMatch(vars={vars:?})")
    }
}
