// SPDX-License-Identifier: Apache-2.0

use std::collections::{HashSet, VecDeque};

use crate::access::GraphAccess;
use crate::error::{Error, NodeId, Result};
use crate::query::direction::Direction;
use crate::query::neighbor::NeighborQuery;
use crate::query::path::Path;

/// Traversal strategy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Strategy {
    /// Breadth-first search (level-order).
    Bfs,
    /// Depth-first search (pre-order).
    Dfs,
}

/// Builder for graph traversals starting from a given node.
///
/// Configure direction, label filter, max depth, and strategy, then call
/// `.collect()` to get visited node IDs or `.collect_paths()` for full paths.
///
/// # Example
///
/// ```
/// use tessera_graph::{Graph, Properties, Direction, Strategy, props};
///
/// let mut g = Graph::new();
/// let a = g.add_node("A", Properties::new()).unwrap();
/// let b = g.add_node("B", Properties::new()).unwrap();
/// let c = g.add_node("C", Properties::new()).unwrap();
/// g.add_edge("R", a, b, Properties::new()).unwrap();
/// g.add_edge("R", b, c, Properties::new()).unwrap();
///
/// let visited = g.traverse(a)
///     .direction(Direction::Outgoing)
///     .bfs()
///     .collect()
///     .unwrap();
///
/// assert_eq!(visited, vec![a, b, c]);
/// ```
pub struct TraversalBuilder<'g, G: GraphAccess + ?Sized> {
    graph: &'g G,
    start: NodeId,
    direction: Direction,
    label_filter: Option<String>,
    max_depth: Option<usize>,
    strategy: Strategy,
}

impl<'g, G: GraphAccess + ?Sized> TraversalBuilder<'g, G> {
    /// Creates a new traversal builder starting from the given node.
    pub const fn new(graph: &'g G, start: NodeId) -> Self {
        Self {
            graph,
            start,
            direction: Direction::Both,
            label_filter: None,
            max_depth: None,
            strategy: Strategy::Bfs,
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

    /// Sets the maximum traversal depth (number of hops from the start node).
    ///
    /// A depth of 0 returns only the start node. `None` means unlimited.
    #[must_use]
    pub const fn max_depth(mut self, depth: usize) -> Self {
        self.max_depth = Some(depth);
        self
    }

    /// Uses breadth-first search strategy (the default).
    #[must_use]
    pub const fn bfs(mut self) -> Self {
        self.strategy = Strategy::Bfs;
        self
    }

    /// Uses depth-first search strategy.
    #[must_use]
    pub const fn dfs(mut self) -> Self {
        self.strategy = Strategy::Dfs;
        self
    }

    /// Executes the traversal and returns visited node IDs in traversal order.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NodeNotFound`] if the start node does not exist.
    pub fn collect(self) -> Result<Vec<NodeId>> {
        if !self.graph.node_exists(self.start) {
            return Err(Error::NodeNotFound(self.start));
        }

        match self.strategy {
            Strategy::Bfs => self.bfs_collect(),
            Strategy::Dfs => self.dfs_collect(),
        }
    }

    /// Executes the traversal and returns full paths from the start node
    /// to each visited node.
    ///
    /// The first path is always the trivial path containing only the start node.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NodeNotFound`] if the start node does not exist.
    pub fn collect_paths(self) -> Result<Vec<Path>> {
        if !self.graph.node_exists(self.start) {
            return Err(Error::NodeNotFound(self.start));
        }

        match self.strategy {
            Strategy::Bfs => self.bfs_collect_paths(),
            Strategy::Dfs => self.dfs_collect_paths(),
        }
    }

    // ------------------------------------------------------------------
    // BFS
    // ------------------------------------------------------------------

    fn bfs_collect(&self) -> Result<Vec<NodeId>> {
        let mut visited = HashSet::new();
        let mut order = Vec::new();
        let mut queue: VecDeque<(NodeId, usize)> = VecDeque::new();

        visited.insert(self.start);
        order.push(self.start);
        queue.push_back((self.start, 0));

        while let Some((current, depth)) = queue.pop_front() {
            if self.max_depth.is_some_and(|max| depth >= max) {
                continue;
            }

            for (neighbor, _edge_id) in self.filtered_neighbors(current)? {
                if visited.insert(neighbor) {
                    order.push(neighbor);
                    queue.push_back((neighbor, depth + 1));
                }
            }
        }

        Ok(order)
    }

    fn bfs_collect_paths(&self) -> Result<Vec<Path>> {
        let mut visited = HashSet::new();
        let mut paths = Vec::new();
        let mut queue: VecDeque<(NodeId, usize, Path)> = VecDeque::new();

        let start_path = Path::single(self.start);
        visited.insert(self.start);
        paths.push(start_path.clone());
        queue.push_back((self.start, 0, start_path));

        while let Some((current, depth, current_path)) = queue.pop_front() {
            if self.max_depth.is_some_and(|max| depth >= max) {
                continue;
            }

            for (neighbor, edge_id) in self.filtered_neighbors(current)? {
                if visited.insert(neighbor) {
                    let mut path = current_path.clone();
                    path.push(neighbor, edge_id);
                    paths.push(path.clone());
                    queue.push_back((neighbor, depth + 1, path));
                }
            }
        }

        Ok(paths)
    }

    // ------------------------------------------------------------------
    // DFS
    // ------------------------------------------------------------------

    fn dfs_collect(&self) -> Result<Vec<NodeId>> {
        let mut visited = HashSet::new();
        let mut order = Vec::new();
        // Stack entries: (node, depth)
        let mut stack: Vec<(NodeId, usize)> = vec![(self.start, 0)];

        while let Some((current, depth)) = stack.pop() {
            if !visited.insert(current) {
                continue;
            }
            order.push(current);

            if self.max_depth.is_some_and(|max| depth >= max) {
                continue;
            }

            // Push neighbors in reverse so the first neighbor is popped first
            let neighbors: Vec<_> = self.filtered_neighbors(current)?;
            for (neighbor, _edge_id) in neighbors.into_iter().rev() {
                if !visited.contains(&neighbor) {
                    stack.push((neighbor, depth + 1));
                }
            }
        }

        Ok(order)
    }

    fn dfs_collect_paths(&self) -> Result<Vec<Path>> {
        let mut visited = HashSet::new();
        let mut paths = Vec::new();
        // Stack entries: (node, depth, current_path)
        let mut stack: Vec<(NodeId, usize, Path)> = vec![(self.start, 0, Path::single(self.start))];

        while let Some((current, depth, path)) = stack.pop() {
            if !visited.insert(current) {
                continue;
            }
            paths.push(path.clone());

            if self.max_depth.is_some_and(|max| depth >= max) {
                continue;
            }

            let neighbors: Vec<_> = self.filtered_neighbors(current)?;
            for (neighbor, edge_id) in neighbors.into_iter().rev() {
                if !visited.contains(&neighbor) {
                    let mut new_path = path.clone();
                    new_path.push(neighbor, edge_id);
                    stack.push((neighbor, depth + 1, new_path));
                }
            }
        }

        Ok(paths)
    }

    // ------------------------------------------------------------------
    // Helpers
    // ------------------------------------------------------------------

    /// Returns neighbors of the given node filtered by direction and label,
    /// as (`neighbor_id`, `edge_id`) pairs.
    fn filtered_neighbors(&self, node: NodeId) -> Result<Vec<(NodeId, crate::error::EdgeId)>> {
        let edges = NeighborQuery::new(self.graph, node)
            .direction(self.direction)
            .collect()?;

        let mut result = Vec::with_capacity(edges.len());
        for edge in edges {
            if let Some(ref label) = self.label_filter {
                if edge.label() != label {
                    continue;
                }
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
