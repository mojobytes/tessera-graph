// SPDX-License-Identifier: MIT

use std::collections::{HashMap, HashSet, VecDeque};

use crate::error::{EdgeId, Error, NodeId, Result};
use crate::graph::Graph;
use crate::query::direction::Direction;
use crate::query::path::Path;

/// Builder for finding the shortest (unweighted) path between two nodes.
///
/// Uses BFS to find the path with the fewest hops. Configure direction and
/// label filter before calling `.find()`.
///
/// # Example
///
/// ```
/// use ermya_graph::{Graph, Properties, Direction, props};
///
/// let mut g = Graph::new();
/// let a = g.add_node("A", Properties::new()).unwrap();
/// let b = g.add_node("B", Properties::new()).unwrap();
/// let c = g.add_node("C", Properties::new()).unwrap();
/// g.add_edge("R", a, b, Properties::new()).unwrap();
/// g.add_edge("R", b, c, Properties::new()).unwrap();
///
/// let path = g.shortest_path(a, c)
///     .direction(Direction::Outgoing)
///     .find()
///     .unwrap();
///
/// assert!(path.is_some());
/// let path = path.unwrap();
/// assert_eq!(path.len(), 2); // 2 hops: a->b->c
/// assert_eq!(path.nodes(), &[a, b, c]);
/// ```
pub struct ShortestPathQuery<'g> {
    graph: &'g Graph,
    from: NodeId,
    to: NodeId,
    direction: Direction,
    label_filter: Option<String>,
}

impl<'g> ShortestPathQuery<'g> {
    /// Creates a new shortest-path query.
    pub(crate) const fn new(graph: &'g Graph, from: NodeId, to: NodeId) -> Self {
        Self {
            graph,
            from,
            to,
            direction: Direction::Both,
            label_filter: None,
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

    /// Executes the BFS and returns the shortest path, or `None` if unreachable.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NodeNotFound`] if either endpoint does not exist.
    pub fn find(self) -> Result<Option<Path>> {
        if !self.graph.node_exists(self.from) {
            return Err(Error::NodeNotFound(self.from));
        }
        if !self.graph.node_exists(self.to) {
            return Err(Error::NodeNotFound(self.to));
        }

        // Same node — trivial path.
        if self.from == self.to {
            return Ok(Some(Path::single(self.from)));
        }

        // BFS with parent tracking.
        let mut visited = HashSet::new();
        let mut parent: HashMap<NodeId, (NodeId, EdgeId)> = HashMap::new();
        let mut queue: VecDeque<NodeId> = VecDeque::new();

        visited.insert(self.from);
        queue.push_back(self.from);

        while let Some(current) = queue.pop_front() {
            for (neighbor, edge_id) in self.filtered_neighbors(current)? {
                if visited.insert(neighbor) {
                    parent.insert(neighbor, (current, edge_id));
                    if neighbor == self.to {
                        return Ok(Some(Self::reconstruct_path(self.from, self.to, &parent)));
                    }
                    queue.push_back(neighbor);
                }
            }
        }

        Ok(None)
    }

    /// Reconstructs the path from `from` to `to` using the parent map.
    fn reconstruct_path(
        from: NodeId,
        to: NodeId,
        parent: &HashMap<NodeId, (NodeId, EdgeId)>,
    ) -> Path {
        let mut segments = Vec::new();
        let mut current = to;
        while current != from {
            let &(prev, edge_id) = parent.get(&current).expect("parent map incomplete");
            segments.push((current, edge_id));
            current = prev;
        }
        segments.reverse();

        let mut path = Path::single(from);
        for (node, edge_id) in segments {
            path.push(node, edge_id);
        }
        path
    }

    /// Returns filtered neighbors as (`neighbor_id`, `edge_id`) pairs.
    fn filtered_neighbors(&self, node: NodeId) -> Result<Vec<(NodeId, EdgeId)>> {
        let edges = self
            .graph
            .neighbors(node)
            .direction(self.direction)
            .collect()?;

        let mut result = Vec::with_capacity(edges.len());
        for edge in edges {
            if let Some(ref label) = self.label_filter
                && edge.label() != label
            {
                continue;
            }
            let neighbor = if edge.source() == node {
                edge.target()
            } else {
                edge.source()
            };
            result.push((neighbor, edge.id()));
        }
        Ok(result)
    }
}
