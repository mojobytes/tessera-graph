// SPDX-License-Identifier: Apache-2.0

use crate::access::GraphAccess;
use crate::edge::Edge;
use crate::error::{Error, NodeId, Result};
use crate::query::direction::Direction;

/// Builder for querying the neighbors of a node.
///
/// Allows filtering by [`Direction`] and edge label before collecting results.
///
/// # Example
///
/// ```
/// use tessera_graph::{Graph, Properties, Direction, props};
///
/// let mut g = Graph::new();
/// let a = g.add_node("A", Properties::new()).unwrap();
/// let b = g.add_node("B", Properties::new()).unwrap();
/// g.add_edge("KNOWS", a, b, Properties::new()).unwrap();
///
/// let neighbors: Vec<_> = g.neighbors(a)
///     .direction(Direction::Outgoing)
///     .label("KNOWS")
///     .collect()
///     .unwrap();
///
/// assert_eq!(neighbors.len(), 1);
/// ```
pub struct NeighborQuery<'g, G: GraphAccess + ?Sized> {
    graph: &'g G,
    node: NodeId,
    direction: Direction,
    label_filter: Option<&'g str>,
}

impl<'g, G: GraphAccess + ?Sized> NeighborQuery<'g, G> {
    /// Creates a new neighbor query for the given node.
    ///
    /// Defaults to [`Direction::Both`] with no label filter.
    pub const fn new(graph: &'g G, node: NodeId) -> Self {
        Self {
            graph,
            node,
            direction: Direction::Both,
            label_filter: None,
        }
    }

    /// Restricts the query to the given direction.
    #[must_use]
    pub const fn direction(mut self, direction: Direction) -> Self {
        self.direction = direction;
        self
    }

    /// Restricts the query to edges with the given label.
    #[must_use]
    pub const fn label(mut self, label: &'g str) -> Self {
        self.label_filter = Some(label);
        self
    }

    /// Executes the query and returns matching edges.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NodeNotFound`] if the source node does not exist.
    pub fn collect(self) -> Result<Vec<Edge>> {
        if !self.graph.node_exists(self.node) {
            return Err(Error::NodeNotFound(self.node));
        }

        let mut edges = Vec::new();

        if let Some(label) = self.label_filter {
            match self.direction {
                Direction::Outgoing => {
                    edges.extend(self.graph.outgoing_edges_by_label(self.node, label)?);
                }
                Direction::Incoming => {
                    edges.extend(self.graph.incoming_edges_by_label(self.node, label)?);
                }
                Direction::Both => {
                    edges.extend(self.graph.outgoing_edges_by_label(self.node, label)?);
                    edges.extend(self.graph.incoming_edges_by_label(self.node, label)?);
                }
            }
        } else {
            match self.direction {
                Direction::Outgoing => {
                    edges.extend(self.graph.outgoing_edges(self.node)?);
                }
                Direction::Incoming => {
                    edges.extend(self.graph.incoming_edges(self.node)?);
                }
                Direction::Both => {
                    edges.extend(self.graph.outgoing_edges(self.node)?);
                    edges.extend(self.graph.incoming_edges(self.node)?);
                }
            }
        }

        Ok(edges)
    }

    /// Convenience: collects and returns only the neighbor node IDs.
    ///
    /// For each matching edge, the "other" node (not the query node) is returned.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NodeNotFound`] if the source node does not exist.
    pub fn node_ids(self) -> Result<Vec<NodeId>> {
        let source = self.node;
        let edges = self.collect()?;
        Ok(edges
            .into_iter()
            .map(|e| {
                if e.source() == source {
                    e.target()
                } else {
                    e.source()
                }
            })
            .collect())
    }
}
