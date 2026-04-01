//! [`BenchmarkTarget`] implementation backed by the in-process `tessera-graph` engine.

use tessera_graph::{Direction, EdgeId, Graph, NodeId, Properties};

use crate::error::Result;
use crate::target::{BenchmarkTarget, EdgeData, EdgeHandle, NodeData, NodeHandle};

/// In-process target that exercises `tessera-graph` directly through its Rust API.
pub struct TesseraTarget {
    graph: Graph,
}

impl Default for TesseraTarget {
    fn default() -> Self {
        Self::new()
    }
}

impl TesseraTarget {
    /// Creates a new target backed by a fresh in-memory graph.
    #[must_use]
    pub fn new() -> Self {
        Self {
            graph: Graph::new(),
        }
    }

    /// Returns a shared reference to the underlying graph (useful for
    /// external benchmark harnesses that need direct access).
    #[must_use]
    pub const fn graph(&self) -> &Graph {
        &self.graph
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
        let ids = self
            .graph
            .traverse(Self::to_node_id(start))
            .direction(Direction::Outgoing)
            .max_depth(max_depth as usize)
            .bfs()
            .collect()?;
        Ok(Self::collect_node_handles(&ids))
    }

    fn traverse_dfs(&self, start: NodeHandle, max_depth: u32) -> Result<Vec<NodeHandle>> {
        let ids = self
            .graph
            .traverse(Self::to_node_id(start))
            .direction(Direction::Outgoing)
            .max_depth(max_depth as usize)
            .dfs()
            .collect()?;
        Ok(Self::collect_node_handles(&ids))
    }

    fn shortest_path(&self, from: NodeHandle, to: NodeHandle) -> Result<Option<Vec<NodeHandle>>> {
        let result = self
            .graph
            .shortest_path(Self::to_node_id(from), Self::to_node_id(to))
            .direction(Direction::Outgoing)
            .find()?;
        Ok(result.map(|path| Self::collect_node_handles(path.nodes())))
    }

    fn clear(&mut self) {
        self.graph = Graph::new();
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
