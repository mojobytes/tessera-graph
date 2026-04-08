//! [`BenchmarkTarget`] implementation backed by the in-process `tessera-graph` engine
//! with the enterprise `NeighborCache` for traversal optimization.

use std::collections::{HashMap, HashSet, VecDeque};

use tessera_graph::{Direction, EdgeId, Graph, GraphAccess, NodeId, Properties};
use tessera_graph_storage::cache::NeighborCache;

use crate::error::Result;
use crate::target::{BenchmarkTarget, EdgeData, EdgeHandle, NodeData, NodeHandle};

/// In-process target that exercises `tessera-graph` with the enterprise
/// `NeighborCache`, eliminating full `Edge` deserialization during traversals.
pub struct TesseraTarget {
    graph: NeighborCache<Graph>,
}

impl Default for TesseraTarget {
    fn default() -> Self {
        Self::new()
    }
}

impl TesseraTarget {
    /// Creates a new target backed by a fresh in-memory graph with
    /// enterprise `NeighborCache`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            graph: NeighborCache::new(Graph::new()),
        }
    }

    /// Returns a shared reference to the underlying graph (useful for
    /// external benchmark harnesses that need direct access).
    #[must_use]
    pub const fn graph(&self) -> &Graph {
        self.graph.inner()
    }

    // -- private helpers --

    const fn to_node_id(h: NodeHandle) -> NodeId {
        NodeId::from_raw(h.0)
    }

    const fn to_edge_id(h: EdgeHandle) -> EdgeId {
        EdgeId::from_raw(h.0)
    }

    const fn to_node_handle(id: NodeId) -> NodeHandle {
        NodeHandle(id.as_u64())
    }

    fn collect_node_handles(ids: &[NodeId]) -> Vec<NodeHandle> {
        ids.iter().copied().map(Self::to_node_handle).collect()
    }

    /// BFS traversal using `NeighborCache::outgoing_neighbor_ids` to avoid
    /// full `Edge` deserialization on every hop.
    fn cached_bfs(&self, start: NodeId, max_depth: u32, direction: Direction) -> tessera_graph::Result<Vec<NodeId>> {
        let mut visited = HashSet::new();
        let mut order = Vec::new();
        let mut queue: VecDeque<(NodeId, u32)> = VecDeque::new();

        visited.insert(start);
        order.push(start);
        queue.push_back((start, 0));

        while let Some((current, depth)) = queue.pop_front() {
            if depth >= max_depth {
                continue;
            }
            for neighbor in self.cached_neighbors(current, direction)? {
                if visited.insert(neighbor) {
                    order.push(neighbor);
                    queue.push_back((neighbor, depth + 1));
                }
            }
        }
        Ok(order)
    }

    /// DFS traversal using `NeighborCache::outgoing_neighbor_ids`.
    fn cached_dfs(&self, start: NodeId, max_depth: u32, direction: Direction) -> tessera_graph::Result<Vec<NodeId>> {
        let mut visited = HashSet::new();
        let mut order = Vec::new();
        let mut stack: Vec<(NodeId, u32)> = vec![(start, 0)];

        while let Some((current, depth)) = stack.pop() {
            if !visited.insert(current) {
                continue;
            }
            order.push(current);
            if depth >= max_depth {
                continue;
            }
            let neighbors = self.cached_neighbors(current, direction)?;
            for neighbor in neighbors.into_iter().rev() {
                if !visited.contains(&neighbor) {
                    stack.push((neighbor, depth + 1));
                }
            }
        }
        Ok(order)
    }

    /// Shortest path via BFS using `NeighborCache`.
    fn cached_shortest_path(&self, from: NodeId, to: NodeId, direction: Direction) -> tessera_graph::Result<Option<Vec<NodeId>>> {
        if from == to {
            return Ok(Some(vec![from]));
        }
        let mut visited = HashSet::new();
        let mut parent: HashMap<NodeId, NodeId> = HashMap::new();
        let mut queue: VecDeque<NodeId> = VecDeque::new();

        visited.insert(from);
        queue.push_back(from);

        while let Some(current) = queue.pop_front() {
            for neighbor in self.cached_neighbors(current, direction)? {
                if visited.insert(neighbor) {
                    parent.insert(neighbor, current);
                    if neighbor == to {
                        // Reconstruct path
                        let mut path = Vec::new();
                        let mut node = to;
                        while node != from {
                            path.push(node);
                            node = *parent.get(&node).expect("parent map incomplete");
                        }
                        path.push(from);
                        path.reverse();
                        return Ok(Some(path));
                    }
                    queue.push_back(neighbor);
                }
            }
        }
        Ok(None)
    }

    /// Returns neighbor IDs for a node using the cache, respecting direction.
    fn cached_neighbors(&self, node: NodeId, direction: Direction) -> tessera_graph::Result<Vec<NodeId>> {
        match direction {
            Direction::Outgoing => self.graph.outgoing_neighbor_ids(node),
            Direction::Incoming => self.graph.incoming_neighbor_ids(node),
            Direction::Both => {
                let mut out = self.graph.outgoing_neighbor_ids(node)?;
                let inc = self.graph.incoming_neighbor_ids(node)?;
                for id in inc {
                    if !out.contains(&id) {
                        out.push(id);
                    }
                }
                Ok(out)
            }
        }
    }
}

impl BenchmarkTarget for TesseraTarget {
    fn name(&self) -> &'static str {
        "tessera"
    }

    fn create_node(&mut self, label: &str, props: Properties) -> Result<NodeHandle> {
        let id = self.graph.add_node(label, props)?;
        Ok(Self::to_node_handle(id))
    }

    fn create_edge(
        &mut self,
        label: &str,
        from: NodeHandle,
        to: NodeHandle,
        props: Properties,
    ) -> Result<EdgeHandle> {
        let id = self
            .graph
            .add_edge(label, Self::to_node_id(from), Self::to_node_id(to), props)?;
        Ok(EdgeHandle(id.as_u64()))
    }

    fn get_node(&self, handle: NodeHandle) -> Result<NodeData> {
        let node = self.graph.node(Self::to_node_id(handle))?;
        Ok(NodeData {
            label: node.label().to_string(),
            props: node.properties().clone(),
        })
    }

    fn get_edge(&self, handle: EdgeHandle) -> Result<EdgeData> {
        let edge = self.graph.edge(Self::to_edge_id(handle))?;
        Ok(EdgeData {
            label: edge.label().to_string(),
            props: edge.properties().clone(),
        })
    }

    fn traverse_bfs(&self, start: NodeHandle, max_depth: u32) -> Result<Vec<NodeHandle>> {
        let ids = self.cached_bfs(Self::to_node_id(start), max_depth, Direction::Outgoing)?;
        Ok(Self::collect_node_handles(&ids))
    }

    fn traverse_dfs(&self, start: NodeHandle, max_depth: u32) -> Result<Vec<NodeHandle>> {
        let ids = self.cached_dfs(Self::to_node_id(start), max_depth, Direction::Outgoing)?;
        Ok(Self::collect_node_handles(&ids))
    }

    fn shortest_path(&self, from: NodeHandle, to: NodeHandle) -> Result<Option<Vec<NodeHandle>>> {
        let result = self.cached_shortest_path(
            Self::to_node_id(from),
            Self::to_node_id(to),
            Direction::Outgoing,
        )?;
        Ok(result.map(|ids| Self::collect_node_handles(&ids)))
    }

    fn clear(&mut self) {
        self.graph = NeighborCache::new(Graph::new());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_and_get_node_round_trips() {
        let mut t = TesseraTarget::new();
        let h = t.create_node("Person", Properties::new()).unwrap();
        let data = t.get_node(h).unwrap();
        assert_eq!(data.label, "Person");
    }

    #[test]
    fn create_and_get_edge_round_trips() {
        let mut t = TesseraTarget::new();
        let a = t.create_node("A", Properties::new()).unwrap();
        let b = t.create_node("B", Properties::new()).unwrap();
        let eh = t.create_edge("KNOWS", a, b, Properties::new()).unwrap();
        let data = t.get_edge(eh).unwrap();
        assert_eq!(data.label, "KNOWS");
    }

    #[test]
    fn bfs_visits_all_nodes_in_chain_of_five() {
        let mut t = TesseraTarget::new();
        let start = t.create_node("N", Properties::new()).unwrap();
        let mut prev = start;
        for _ in 0..4 {
            let next = t.create_node("N", Properties::new()).unwrap();
            t.create_edge("NEXT", prev, next, Properties::new())
                .unwrap();
            prev = next;
        }
        let visited = t.traverse_bfs(start, 10).unwrap();
        assert_eq!(visited.len(), 5);
    }

    #[test]
    fn dfs_visits_all_nodes_in_chain_of_five() {
        let mut t = TesseraTarget::new();
        let start = t.create_node("N", Properties::new()).unwrap();
        let mut prev = start;
        for _ in 0..4 {
            let next = t.create_node("N", Properties::new()).unwrap();
            t.create_edge("NEXT", prev, next, Properties::new())
                .unwrap();
            prev = next;
        }
        let visited = t.traverse_dfs(start, 10).unwrap();
        assert_eq!(visited.len(), 5);
    }

    #[test]
    fn shortest_path_in_chain_of_three() {
        let mut t = TesseraTarget::new();
        let a = t.create_node("N", Properties::new()).unwrap();
        let b = t.create_node("N", Properties::new()).unwrap();
        let c = t.create_node("N", Properties::new()).unwrap();
        t.create_edge("E", a, b, Properties::new()).unwrap();
        t.create_edge("E", b, c, Properties::new()).unwrap();
        let path = t.shortest_path(a, c).unwrap();
        assert!(path.is_some());
        assert_eq!(path.unwrap().len(), 3); // a, b, c
    }

    #[test]
    fn shortest_path_unreachable_returns_none() {
        let mut t = TesseraTarget::new();
        let a = t.create_node("N", Properties::new()).unwrap();
        let b = t.create_node("N", Properties::new()).unwrap();
        let path = t.shortest_path(a, b).unwrap();
        assert!(path.is_none());
    }

    #[test]
    fn clear_resets_state() {
        let mut t = TesseraTarget::new();
        t.create_node("N", Properties::new()).unwrap();
        t.create_node("N", Properties::new()).unwrap();
        t.clear();
        let h = t.create_node("N", Properties::new()).unwrap();
        let data = t.get_node(h).unwrap();
        assert_eq!(data.label, "N");
    }

    #[test]
    fn name_returns_tessera() {
        let t = TesseraTarget::new();
        assert_eq!(t.name(), "tessera");
    }

    #[test]
    fn insert_throughput_regression_guard() {
        use std::time::Instant;
        let mut t = TesseraTarget::new();
        let n = 10_000usize;
        let start = Instant::now();
        for _ in 0..n {
            t.create_node("N", Properties::new()).unwrap();
        }
        let elapsed = start.elapsed();
        #[allow(
            clippy::cast_precision_loss,
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss
        )]
        let ops_per_sec = (n as f64 / elapsed.as_secs_f64()) as u64;
        let threshold: u64 = if cfg!(debug_assertions) {
            10_000
        } else {
            50_000
        };
        assert!(
            ops_per_sec >= threshold,
            "Insert throughput regression: {ops_per_sec} ops/s < {threshold} ops/s minimum"
        );
    }

    #[test]
    fn concurrent_insert_throughput_regression_guard() {
        use crate::scenario::ConcurrentScenario;
        let s = ConcurrentScenario {
            thread_count: 4,
            ops_per_thread: 2_500,
            write_ratio: 1.0,
        };
        let r = s
            .run_with_factory(|| Box::new(TesseraTarget::new()))
            .unwrap();
        let threshold: u64 = if cfg!(debug_assertions) {
            5_000
        } else {
            20_000
        };
        assert!(
            r.throughput_ops_per_sec >= threshold,
            "Concurrent insert throughput regression: {} ops/s < {} ops/s minimum",
            r.throughput_ops_per_sec,
            threshold
        );
    }

    #[test]
    fn bfs_throughput_regression_guard() {
        use std::time::Instant;
        let mut t = TesseraTarget::new();
        let start_h = t.create_node("N", Properties::new()).unwrap();
        let mut prev = start_h;
        for _ in 0..999 {
            let next = t.create_node("N", Properties::new()).unwrap();
            t.create_edge("E", prev, next, Properties::new()).unwrap();
            prev = next;
        }
        let t0 = Instant::now();
        for _ in 0..100 {
            let visited = t.traverse_bfs(start_h, 1001).unwrap();
            assert_eq!(visited.len(), 1000);
        }
        let elapsed = t0.elapsed();
        let max_ms: u128 = if cfg!(debug_assertions) { 2_000 } else { 500 };
        assert!(
            elapsed.as_millis() < max_ms,
            "BFS traversal regression: 100 x 1000-node BFS took {}ms >= {max_ms}ms",
            elapsed.as_millis()
        );
    }
}
