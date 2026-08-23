// SPDX-License-Identifier: MIT

//! `PyGraph` — Python wrapper around `ermya_graph::Graph`.

use ermya_graph::{Graph, GraphConfig};
use pyo3::prelude::*;
use pyo3::types::PyDict;

use crate::errors::to_py_err;
use crate::types::edge::PyEdge;
use crate::types::edge_id::PyEdgeId;
use crate::types::node::PyNode;
use crate::types::node_id::PyNodeId;
use crate::types::properties;

/// An embeddable property graph. No server, no schema, no infrastructure.
#[pyclass(name = "Graph")]
pub struct PyGraph {
    pub(crate) inner: Graph,
}

#[pymethods]
impl PyGraph {
    /// Creates a new in-memory graph.
    #[staticmethod]
    #[pyo3(name = "new")]
    fn new_() -> Self {
        Self {
            inner: Graph::new(),
        }
    }

    /// Opens a file-backed graph at the given path.
    #[staticmethod]
    #[pyo3(
        name = "open",
        signature = (path, *, create_if_missing=true, memory_limit_bytes=67_108_864, wal_enabled=true, adj_cache_capacity=65_536)
    )]
    fn open(
        path: &str,
        create_if_missing: bool,
        memory_limit_bytes: usize,
        wal_enabled: bool,
        adj_cache_capacity: usize,
    ) -> PyResult<Self> {
        let config = GraphConfig {
            memory_limit_bytes,
            create_if_missing,
            adj_cache_capacity,
            wal_enabled,
            ..GraphConfig::new()
        };
        let g = Graph::open(path, &config).map_err(to_py_err)?;
        Ok(Self { inner: g })
    }

    /// Persists all data and flushes the WAL.
    fn flush(&mut self) -> PyResult<()> {
        self.inner.flush().map_err(to_py_err)
    }

    /// Returns the total number of nodes.
    fn node_count(&self) -> usize {
        self.inner.node_count()
    }

    /// Returns the total number of edges.
    fn edge_count(&self) -> usize {
        self.inner.edge_count()
    }

    /// Adds a new node with the given label and properties dict.
    fn add_node(&mut self, label: &str, properties: &Bound<'_, PyDict>) -> PyResult<PyNodeId> {
        let props = properties::from_py_dict(properties)?;
        let id = self.inner.add_node(label, props).map_err(to_py_err)?;
        Ok(id.into())
    }

    /// Retrieves a node by its ID.
    fn node(&self, id: &PyNodeId) -> PyResult<PyNode> {
        let node = self.inner.node(to_node_id(id)).map_err(to_py_err)?;
        Ok(node.into())
    }

    /// Adds a directed edge between two nodes.
    fn add_edge(
        &mut self,
        label: &str,
        source: &PyNodeId,
        target: &PyNodeId,
        properties: &Bound<'_, PyDict>,
    ) -> PyResult<PyEdgeId> {
        let props = properties::from_py_dict(properties)?;
        let id = self
            .inner
            .add_edge(label, to_node_id(source), to_node_id(target), props)
            .map_err(to_py_err)?;
        Ok(id.into())
    }

    /// Retrieves an edge by its ID.
    fn edge(&self, id: &PyEdgeId) -> PyResult<PyEdge> {
        let edge = self.inner.edge(to_edge_id(id)).map_err(to_py_err)?;
        Ok(edge.into())
    }

    // ── Node reads ────────────────────────────────────────────────────────

    /// Returns `True` if a node with the given ID exists.
    fn node_exists(&self, id: &PyNodeId) -> bool {
        self.inner.node_exists(to_node_id(id))
    }

    /// Returns all node IDs in the graph.
    fn node_ids(&self) -> Vec<PyNodeId> {
        self.inner
            .node_ids()
            .into_iter()
            .map(PyNodeId::from)
            .collect()
    }

    /// Returns node IDs whose label matches exactly.
    fn nodes_by_label(&self, label: &str) -> Vec<PyNodeId> {
        self.inner
            .nodes_by_label(label)
            .into_iter()
            .map(PyNodeId::from)
            .collect()
    }

    // ── Node mutations ──────────────────────────────────────────────────

    /// Removes a node and all its connected edges. Returns the removed node.
    fn remove_node(&mut self, id: &PyNodeId) -> PyResult<PyNode> {
        let node = self.inner.remove_node(to_node_id(id)).map_err(to_py_err)?;
        Ok(node.into())
    }

    /// Updates a node's label and/or properties.
    #[pyo3(signature = (id, *, label=None, properties=None))]
    fn update_node(
        &mut self,
        id: &PyNodeId,
        label: Option<&str>,
        properties: Option<&Bound<'_, PyDict>>,
    ) -> PyResult<()> {
        let rust_id = to_node_id(id);
        let mut node = self.inner.node(rust_id).map_err(to_py_err)?;
        if let Some(l) = label {
            node.set_label(l);
        }
        if let Some(dict) = properties {
            let props = properties::from_py_dict(dict)?;
            *node.properties_mut() = props;
        }
        self.inner.update_node(rust_id, &node).map_err(to_py_err)
    }

    // ── Edge reads ──────────────────────────────────────────────────────

    /// Returns edge IDs whose label matches exactly.
    fn edges_by_label(&self, label: &str) -> Vec<PyEdgeId> {
        self.inner
            .edges_by_label(label)
            .into_iter()
            .map(PyEdgeId::from)
            .collect()
    }

    /// Returns outgoing edges from the given node.
    fn outgoing_edges(&self, node: &PyNodeId) -> PyResult<Vec<PyEdge>> {
        let edges = self
            .inner
            .outgoing_edges(to_node_id(node))
            .map_err(to_py_err)?;
        Ok(edges.into_iter().map(PyEdge::from).collect())
    }

    /// Returns incoming edges to the given node.
    fn incoming_edges(&self, node: &PyNodeId) -> PyResult<Vec<PyEdge>> {
        let edges = self
            .inner
            .incoming_edges(to_node_id(node))
            .map_err(to_py_err)?;
        Ok(edges.into_iter().map(PyEdge::from).collect())
    }

    // ── Edge mutations ──────────────────────────────────────────────────

    /// Removes an edge. Returns the removed edge.
    fn remove_edge(&mut self, id: &PyEdgeId) -> PyResult<PyEdge> {
        let edge = self.inner.remove_edge(to_edge_id(id)).map_err(to_py_err)?;
        Ok(edge.into())
    }

    /// Updates an edge's label and/or properties.
    #[pyo3(signature = (id, *, label=None, properties=None))]
    fn update_edge(
        &mut self,
        id: &PyEdgeId,
        label: Option<&str>,
        properties: Option<&Bound<'_, PyDict>>,
    ) -> PyResult<()> {
        let rust_id = to_edge_id(id);
        let mut edge = self.inner.edge(rust_id).map_err(to_py_err)?;
        if let Some(l) = label {
            edge.set_label(l);
        }
        if let Some(dict) = properties {
            let props = properties::from_py_dict(dict)?;
            *edge.properties_mut() = props;
        }
        self.inner.update_edge(rust_id, &edge).map_err(to_py_err)
    }

    // ── Convenience iterators ───────────────────────────────────────────

    /// Returns all nodes as a list of Node objects.
    fn nodes(&self) -> PyResult<Vec<PyNode>> {
        let ids = self.inner.node_ids();
        let mut result = Vec::with_capacity(ids.len());
        for id in ids {
            result.push(PyNode::from(self.inner.node(id).map_err(to_py_err)?));
        }
        Ok(result)
    }

    /// Returns all edges as a list of Edge objects.
    fn edges(&self) -> PyResult<Vec<PyEdge>> {
        let mut result = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for nid in self.inner.node_ids() {
            for e in self.inner.outgoing_edges(nid).map_err(to_py_err)? {
                if seen.insert(e.id().as_u64()) {
                    result.push(PyEdge::from(e));
                }
            }
        }
        Ok(result)
    }

    // ── Query builders ───────────────────────────────────────────────────

    /// Creates a neighbor query builder for the given node.
    fn neighbors(slf: Py<Self>, node: &PyNodeId) -> crate::query::neighbor::PyNeighborQuery {
        crate::query::neighbor::PyNeighborQuery::new(slf, *node)
    }

    /// Creates a traversal builder starting from the given node.
    fn traverse(slf: Py<Self>, start: &PyNodeId) -> crate::query::traversal::PyTraversalBuilder {
        crate::query::traversal::PyTraversalBuilder::new(slf, *start)
    }

    /// Creates a shortest-path query (unweighted BFS).
    fn shortest_path(
        slf: Py<Self>,
        from: &PyNodeId,
        to: &PyNodeId,
    ) -> crate::query::shortest_path::PyShortestPathQuery {
        crate::query::shortest_path::PyShortestPathQuery::new(slf, *from, *to)
    }

    /// Creates a weighted shortest-path query (Dijkstra).
    fn weighted_shortest_path(
        slf: Py<Self>,
        from: &PyNodeId,
        to: &PyNodeId,
    ) -> crate::query::weighted_path::PyWeightedPathQuery {
        crate::query::weighted_path::PyWeightedPathQuery::new(slf, *from, *to)
    }

    /// Creates a pattern matching builder.
    fn pattern(slf: Py<Self>) -> crate::query::pattern::PyPatternBuilder {
        crate::query::pattern::PyPatternBuilder::new(slf)
    }

    /// Creates a subgraph extraction query.
    fn subgraph(slf: Py<Self>, start: &PyNodeId) -> crate::query::subgraph::PySubgraphQuery {
        crate::query::subgraph::PySubgraphQuery::new(slf, *start)
    }

    // ── Batch operations ─────────────────────────────────────────────────

    /// Marks the beginning of a batch transaction (defers fsync).
    fn begin_batch(&mut self) {
        self.inner.begin_batch();
    }

    /// Ends a batch transaction and issues a single fsync.
    fn end_batch(&mut self) -> PyResult<()> {
        self.inner.end_batch().map_err(to_py_err)
    }

    /// Returns a context manager for batch operations.
    fn batch(slf: Py<Self>) -> crate::batch::PyBatchContext {
        crate::batch::PyBatchContext::new(slf)
    }

    fn __repr__(&self) -> String {
        format!(
            "Graph(nodes={}, edges={})",
            self.inner.node_count(),
            self.inner.edge_count()
        )
    }
}

/// Converts `PyNodeId` → `ermya_graph::NodeId`.
pub(crate) fn to_node_id(py_id: &PyNodeId) -> ermya_graph::NodeId {
    ermya_graph::NodeId::from_raw(py_id.value)
}

/// Converts `PyEdgeId` → `ermya_graph::EdgeId`.
pub(crate) fn to_edge_id(py_id: &PyEdgeId) -> ermya_graph::EdgeId {
    ermya_graph::EdgeId::from_raw(py_id.value)
}
