// SPDX-License-Identifier: MIT

use std::collections::HashSet;

use crate::access::GraphAccess;
use crate::edge::Edge;
use crate::error::{EdgeId, Error, NodeId, Result};
use crate::node::Node;
use crate::query::direction::Direction;
use crate::query::traversal::TraversalBuilder;

/// A subgraph extracted from a parent graph: a set of nodes and edges.
#[derive(Debug)]
pub struct Subgraph {
    nodes: Vec<Node>,
    edges: Vec<Edge>,
}

impl Subgraph {
    /// Returns the nodes in this subgraph.
    #[must_use]
    pub fn nodes(&self) -> &[Node] {
        &self.nodes
    }

    /// Returns the edges in this subgraph.
    #[must_use]
    pub fn edges(&self) -> &[Edge] {
        &self.edges
    }

    /// Returns the number of nodes.
    #[must_use]
    pub const fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Returns the number of edges.
    #[must_use]
    pub const fn edge_count(&self) -> usize {
        self.edges.len()
    }

    /// Returns `true` if the subgraph contains no nodes.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }
}

/// Builder for extracting a subgraph by BFS/DFS traversal from a start node.
///
/// The subgraph includes all reachable nodes (within constraints) and the edges
/// that connect them.
///
/// # Example
///
/// ```
/// use tessera_graph::{Graph, Properties, Direction, props};
///
/// let mut g = Graph::new();
/// let n0 = g.add_node("A", Properties::new()).unwrap();
/// let n1 = g.add_node("B", Properties::new()).unwrap();
/// let n2 = g.add_node("C", Properties::new()).unwrap();
/// g.add_edge("R", n0, n1, Properties::new()).unwrap();
/// g.add_edge("R", n1, n2, Properties::new()).unwrap();
///
/// let sub = g.subgraph(n0)
///     .direction(Direction::Outgoing)
///     .extract()
///     .unwrap();
///
/// assert_eq!(sub.node_count(), 3);
/// assert_eq!(sub.edge_count(), 2);
/// ```
pub struct SubgraphQuery<'g, G: GraphAccess + ?Sized> {
    graph: &'g G,
    start: NodeId,
    direction: Direction,
    label_filter: Option<String>,
    max_depth: Option<usize>,
}

impl<'g, G: GraphAccess + ?Sized> SubgraphQuery<'g, G> {
    /// Creates a new subgraph extraction builder.
    pub const fn new(graph: &'g G, start: NodeId) -> Self {
        Self {
            graph,
            start,
            direction: Direction::Both,
            label_filter: None,
            max_depth: None,
        }
    }

    /// Sets the traversal direction.
    #[must_use]
    pub const fn direction(mut self, direction: Direction) -> Self {
        self.direction = direction;
        self
    }

    /// Restricts traversal to edges with the given label.
    #[must_use]
    pub fn label(mut self, label: impl Into<String>) -> Self {
        self.label_filter = Some(label.into());
        self
    }

    /// Sets the maximum traversal depth.
    #[must_use]
    pub const fn max_depth(mut self, depth: usize) -> Self {
        self.max_depth = Some(depth);
        self
    }

    /// Extracts the subgraph by traversing from the start node.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NodeNotFound`] if the start node does not exist.
    pub fn extract(self) -> Result<Subgraph> {
        if !self.graph.node_exists(self.start) {
            return Err(Error::NodeNotFound(self.start));
        }

        // Build a traversal with the same filters.
        let mut traversal = TraversalBuilder::new(self.graph, self.start).direction(self.direction);

        if let Some(ref label) = self.label_filter {
            traversal = traversal.label(label.clone());
        }
        if let Some(depth) = self.max_depth {
            traversal = traversal.max_depth(depth);
        }

        let node_ids = traversal.collect()?;
        let node_set: HashSet<NodeId> = node_ids.iter().copied().collect();

        // Collect full node objects.
        let mut nodes = Vec::with_capacity(node_ids.len());
        for &nid in &node_ids {
            nodes.push(self.graph.node(nid)?);
        }

        // Collect edges between nodes in the subgraph.
        let mut edge_set: HashSet<EdgeId> = HashSet::new();
        let mut edges = Vec::new();

        for &nid in &node_ids {
            let outgoing = self.graph.outgoing_edges(nid)?;
            for edge in outgoing {
                if node_set.contains(&edge.target())
                    && self.matches_label(&edge)
                    && edge_set.insert(edge.id())
                {
                    edges.push(edge);
                }
            }

            let incoming = self.graph.incoming_edges(nid)?;
            for edge in incoming {
                if node_set.contains(&edge.source())
                    && self.matches_label(&edge)
                    && edge_set.insert(edge.id())
                {
                    edges.push(edge);
                }
            }
        }

        Ok(Subgraph { nodes, edges })
    }

    fn matches_label(&self, edge: &Edge) -> bool {
        self.label_filter
            .as_ref()
            .is_none_or(|label| edge.label() == label)
    }
}
