// SPDX-License-Identifier: Apache-2.0

use crate::error::{EdgeId, NodeId};

/// An ordered sequence of nodes and edges forming a path in the graph.
///
/// Invariant: `edges.len() == nodes.len().saturating_sub(1)`.
#[derive(Debug, Clone)]
pub struct Path {
    nodes: Vec<NodeId>,
    edges: Vec<EdgeId>,
}

impl Path {
    /// Creates an empty path.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            nodes: Vec::new(),
            edges: Vec::new(),
        }
    }

    /// Creates a trivial path containing a single node.
    #[must_use]
    pub fn single(node: NodeId) -> Self {
        Self {
            nodes: vec![node],
            edges: Vec::new(),
        }
    }

    /// Extends the path with an edge leading to the given node.
    ///
    /// If the path is empty, only the node is added (the edge is ignored).
    pub fn push(&mut self, node: NodeId, edge: EdgeId) {
        if !self.nodes.is_empty() {
            self.edges.push(edge);
        }
        self.nodes.push(node);
        debug_assert_eq!(
            self.edges.len(),
            self.nodes.len().saturating_sub(1),
            "path invariant violated"
        );
    }

    /// Returns the nodes in this path, in order.
    #[must_use]
    pub fn nodes(&self) -> &[NodeId] {
        &self.nodes
    }

    /// Returns the edges in this path, in order.
    #[must_use]
    pub fn edges(&self) -> &[EdgeId] {
        &self.edges
    }

    /// Returns the first node in the path, or `None` if the path is empty.
    #[must_use]
    pub fn start(&self) -> Option<NodeId> {
        self.nodes.first().copied()
    }

    /// Returns the last node in the path, or `None` if the path is empty.
    #[must_use]
    pub fn end(&self) -> Option<NodeId> {
        self.nodes.last().copied()
    }

    /// Returns the number of edges in the path (hops).
    #[must_use]
    pub fn len(&self) -> usize {
        self.edges.len()
    }

    /// Returns `true` if the path has no edges.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.edges.is_empty()
    }
}

impl Default for Path {
    fn default() -> Self {
        Self::new()
    }
}
