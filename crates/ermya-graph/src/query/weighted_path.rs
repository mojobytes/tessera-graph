// SPDX-License-Identifier: MIT

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap, HashSet};

use crate::edge::Edge;
use crate::error::{EdgeId, Error, NodeId, Result};
use crate::graph::Graph;
use crate::query::direction::Direction;
use crate::query::path::Path;

/// Default weight function: every edge has weight 1.0.
const fn unit_weight(_edge: &Edge) -> f64 {
    1.0
}

/// Entry in the priority queue for Dijkstra.
struct DijkstraEntry {
    node: NodeId,
    cost: f64,
}

impl PartialEq for DijkstraEntry {
    fn eq(&self, other: &Self) -> bool {
        self.cost == other.cost
    }
}

impl Eq for DijkstraEntry {}

impl PartialOrd for DijkstraEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for DijkstraEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        // Reverse ordering for min-heap behavior.
        other
            .cost
            .partial_cmp(&self.cost)
            .unwrap_or(Ordering::Equal)
    }
}

/// Builder for finding the shortest weighted path between two nodes using Dijkstra.
///
/// The weight function `W` can be any `Fn(&Edge) -> f64` — including closures
/// that capture state (e.g., a property key name to look up).
///
/// # Example
///
/// ```
/// use ermya_graph::{Graph, Properties, Direction, Property, props};
///
/// let mut g = Graph::new();
/// let n0 = g.add_node("N", Properties::new()).unwrap();
/// let n1 = g.add_node("N", Properties::new()).unwrap();
/// let n2 = g.add_node("N", Properties::new()).unwrap();
///
/// g.add_edge("R", n0, n1, props! { "cost" => 1.0 }).unwrap();
/// g.add_edge("R", n1, n2, props! { "cost" => 2.0 }).unwrap();
/// g.add_edge("R", n0, n2, props! { "cost" => 10.0 }).unwrap();
///
/// // Using a closure that captures a local variable:
/// let key = "cost";
/// let (total_cost, path) = g.weighted_shortest_path(n0, n2)
///     .direction(Direction::Outgoing)
///     .weight(|edge| {
///         match edge.properties().get(key) {
///             Some(Property::F64(v)) => *v,
///             _ => 1.0,
///         }
///     })
///     .find()
///     .unwrap()
///     .unwrap();
///
/// assert!((total_cost - 3.0).abs() < f64::EPSILON);
/// assert_eq!(path.nodes(), &[n0, n1, n2]);
/// ```
pub struct WeightedPathQuery<'g, W = fn(&Edge) -> f64> {
    graph: &'g Graph,
    from: NodeId,
    to: NodeId,
    direction: Direction,
    label_filter: Option<String>,
    weight_fn: W,
}

impl<'g> WeightedPathQuery<'g> {
    /// Creates a new weighted shortest-path query with unit weights.
    pub(crate) const fn new(graph: &'g Graph, from: NodeId, to: NodeId) -> Self {
        Self {
            graph,
            from,
            to,
            direction: Direction::Both,
            label_filter: None,
            weight_fn: unit_weight as fn(&Edge) -> f64,
        }
    }
}

impl<'g, W: Fn(&Edge) -> f64> WeightedPathQuery<'g, W> {
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

    /// Sets the weight function for edge costs.
    ///
    /// The function must return a non-negative value for each edge.
    /// Accepts closures, function pointers, or any `Fn(&Edge) -> f64`.
    #[must_use]
    pub fn weight<W2: Fn(&Edge) -> f64>(self, weight_fn: W2) -> WeightedPathQuery<'g, W2> {
        WeightedPathQuery {
            graph: self.graph,
            from: self.from,
            to: self.to,
            direction: self.direction,
            label_filter: self.label_filter,
            weight_fn,
        }
    }

    /// Executes Dijkstra and returns `(total_cost, path)`, or `None` if unreachable.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NodeNotFound`] if either endpoint does not exist.
    pub fn find(self) -> Result<Option<(f64, Path)>> {
        if !self.graph.node_exists(self.from) {
            return Err(Error::NodeNotFound(self.from));
        }
        if !self.graph.node_exists(self.to) {
            return Err(Error::NodeNotFound(self.to));
        }

        if self.from == self.to {
            return Ok(Some((0.0, Path::single(self.from))));
        }

        let mut dist: HashMap<NodeId, f64> = HashMap::new();
        let mut parent: HashMap<NodeId, (NodeId, EdgeId)> = HashMap::new();
        let mut visited = HashSet::new();
        let mut heap = BinaryHeap::new();

        dist.insert(self.from, 0.0);
        heap.push(DijkstraEntry {
            node: self.from,
            cost: 0.0,
        });

        while let Some(DijkstraEntry { node, cost }) = heap.pop() {
            if !visited.insert(node) {
                continue;
            }
            if node == self.to {
                let path = Self::reconstruct_path(self.from, self.to, &parent);
                return Ok(Some((cost, path)));
            }

            for (neighbor, edge_id, edge_cost) in self.filtered_neighbors_weighted(node)? {
                let new_cost = cost + edge_cost;
                if new_cost < *dist.get(&neighbor).unwrap_or(&f64::INFINITY) {
                    dist.insert(neighbor, new_cost);
                    parent.insert(neighbor, (node, edge_id));
                    heap.push(DijkstraEntry {
                        node: neighbor,
                        cost: new_cost,
                    });
                }
            }
        }

        Ok(None)
    }

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

    /// Returns filtered neighbors with their edge weights.
    fn filtered_neighbors_weighted(&self, node: NodeId) -> Result<Vec<(NodeId, EdgeId, f64)>> {
        let edges = self
            .graph
            .neighbors(node)
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
            let cost = (self.weight_fn)(&edge);
            result.push((neighbor, edge.id(), cost));
        }
        Ok(result)
    }
}
