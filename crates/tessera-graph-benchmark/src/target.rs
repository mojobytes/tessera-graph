//! Abstract benchmark target trait and associated types.

use tessera_graph::Properties;

use crate::error::Result;

/// Opaque handle to a node within a [`BenchmarkTarget`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NodeHandle(pub u64);

/// Opaque handle to an edge within a [`BenchmarkTarget`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EdgeHandle(pub u64);

/// Data returned by a node lookup.
#[derive(Debug, Clone)]
pub struct NodeData {
    /// The node label.
    pub label: String,
    /// The node properties.
    pub props: Properties,
}

/// Data returned by an edge lookup.
#[derive(Debug, Clone)]
pub struct EdgeData {
    /// The edge label.
    pub label: String,
    /// The edge properties.
    pub props: Properties,
}

/// Abstraction over a graph database target so that the same benchmark
/// scenarios can run against different backends (`TesseraGraph`, Memgraph, …).
#[allow(clippy::missing_errors_doc)]
pub trait BenchmarkTarget {
    /// Human-readable name of this target (e.g. `"tessera"`, `"memgraph"`).
    fn name(&self) -> &str;

    /// Creates a node with the given label and properties.
    fn create_node(&mut self, label: &str, props: Properties) -> Result<NodeHandle>;

    /// Creates an edge between two nodes.
    fn create_edge(
        &mut self,
        label: &str,
        from: NodeHandle,
        to: NodeHandle,
        props: Properties,
    ) -> Result<EdgeHandle>;

    /// Retrieves node data by handle.
    fn get_node(&self, handle: NodeHandle) -> Result<NodeData>;

    /// Retrieves edge data by handle.
    fn get_edge(&self, handle: EdgeHandle) -> Result<EdgeData>;

    /// BFS traversal from `start` up to `max_depth`, returning visited node handles.
    fn traverse_bfs(&self, start: NodeHandle, max_depth: u32) -> Result<Vec<NodeHandle>>;

    /// DFS traversal from `start` up to `max_depth`, returning visited node handles.
    fn traverse_dfs(&self, start: NodeHandle, max_depth: u32) -> Result<Vec<NodeHandle>>;

    /// Finds the shortest path between two nodes.
    /// Returns `None` if the nodes are not connected.
    fn shortest_path(&self, from: NodeHandle, to: NodeHandle) -> Result<Option<Vec<NodeHandle>>>;

    /// Resets all state so the next benchmark run starts fresh.
    fn clear(&mut self);
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Mock;

    impl BenchmarkTarget for Mock {
        fn name(&self) -> &'static str {
            "mock"
        }
        fn create_node(&mut self, _l: &str, _p: Properties) -> Result<NodeHandle> {
            Ok(NodeHandle(0))
        }
        fn create_edge(
            &mut self,
            _l: &str,
            _f: NodeHandle,
            _t: NodeHandle,
            _p: Properties,
        ) -> Result<EdgeHandle> {
            Ok(EdgeHandle(0))
        }
        fn get_node(&self, _h: NodeHandle) -> Result<NodeData> {
            Ok(NodeData {
                label: "X".into(),
                props: Properties::new(),
            })
        }
        fn get_edge(&self, _h: EdgeHandle) -> Result<EdgeData> {
            Ok(EdgeData {
                label: "R".into(),
                props: Properties::new(),
            })
        }
        fn traverse_bfs(&self, _s: NodeHandle, _d: u32) -> Result<Vec<NodeHandle>> {
            Ok(vec![])
        }
        fn traverse_dfs(&self, _s: NodeHandle, _d: u32) -> Result<Vec<NodeHandle>> {
            Ok(vec![])
        }
        fn shortest_path(&self, _f: NodeHandle, _t: NodeHandle) -> Result<Option<Vec<NodeHandle>>> {
            Ok(None)
        }
        fn clear(&mut self) {}
    }

    #[test]
    fn mock_target_name_is_returned() {
        assert_eq!(Mock.name(), "mock");
    }

    #[test]
    fn trait_is_object_safe() {
        fn accepts_dyn(_t: &dyn BenchmarkTarget) {}
        accepts_dyn(&Mock);
    }
}
