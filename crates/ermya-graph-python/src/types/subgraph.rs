// SPDX-License-Identifier: MIT

use pyo3::prelude::*;

use super::edge::PyEdge;
use super::edge_id::PyEdgeId;
use super::node::PyNode;
use super::node_id::PyNodeId;

/// A subgraph extracted from a parent graph: a set of nodes and edges.
#[pyclass(name = "Subgraph", frozen, from_py_object)]
#[derive(Clone)]
pub struct PySubgraph {
    nodes: Vec<PyNode>,
    edges: Vec<PyEdge>,
}

impl PySubgraph {
    pub fn from_rust(sg: ermya_graph::Subgraph) -> Self {
        let nodes = sg.nodes().iter().cloned().map(PyNode::from).collect();
        let edges = sg.edges().iter().cloned().map(PyEdge::from).collect();
        Self { nodes, edges }
    }
}

#[pymethods]
impl PySubgraph {
    /// The nodes in this subgraph.
    #[getter]
    fn nodes(&self) -> Vec<PyNode> {
        self.nodes.clone()
    }

    /// The edges in this subgraph.
    #[getter]
    fn edges(&self) -> Vec<PyEdge> {
        self.edges.clone()
    }

    /// Number of nodes.
    #[getter]
    fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Number of edges.
    #[getter]
    fn edge_count(&self) -> usize {
        self.edges.len()
    }

    /// Returns `True` if the subgraph contains no nodes.
    #[getter]
    fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    fn __len__(&self) -> usize {
        self.nodes.len()
    }

    fn __bool__(&self) -> bool {
        !self.nodes.is_empty()
    }

    /// Checks if a node (by `NodeId` or `Node`) or edge (by `EdgeId` or `Edge`)
    /// is in this subgraph.
    fn __contains__(&self, item: &Bound<'_, PyAny>) -> bool {
        if let Ok(nid) = item.extract::<PyNodeId>() {
            return self
                .nodes
                .iter()
                .any(|n| n.inner.id().as_u64() == nid.value);
        }
        if let Ok(node) = item.extract::<PyNode>() {
            return self.nodes.iter().any(|n| n.inner.id() == node.inner.id());
        }
        if let Ok(eid) = item.extract::<PyEdgeId>() {
            return self
                .edges
                .iter()
                .any(|e| e.inner.id().as_u64() == eid.value);
        }
        if let Ok(edge) = item.extract::<PyEdge>() {
            return self.edges.iter().any(|e| e.inner.id() == edge.inner.id());
        }
        false
    }

    fn __repr__(&self) -> String {
        format!(
            "Subgraph(nodes={}, edges={})",
            self.nodes.len(),
            self.edges.len()
        )
    }
}
