//! Deterministic dataset generators for benchmark scenarios.

use tessera_graph::Properties;

use crate::error::Result;
use crate::target::{BenchmarkTarget, EdgeHandle, NodeHandle};

/// The result of building a dataset: all created node and edge handles.
#[derive(Debug, Clone)]
pub struct DatasetResult {
    /// Node handles in creation order.
    pub nodes: Vec<NodeHandle>,
    /// Edge handles in creation order.
    pub edges: Vec<EdgeHandle>,
}

/// Trait for deterministic dataset generators.
#[allow(clippy::missing_errors_doc)]
pub trait Dataset {
    /// Builds the dataset inside the given `target`, returning all created handles.
    fn build(&self, target: &mut dyn BenchmarkTarget) -> Result<DatasetResult>;
}

// ---------------------------------------------------------------------------
// Chain: N nodes linked linearly (node_0 -> node_1 -> ... -> node_{n-1})
// ---------------------------------------------------------------------------

/// A linear chain of `length` nodes connected by `length - 1` edges.
pub struct ChainDataset {
    /// Number of nodes in the chain.
    pub length: usize,
}

impl Dataset for ChainDataset {
    fn build(&self, target: &mut dyn BenchmarkTarget) -> Result<DatasetResult> {
        let mut nodes = Vec::with_capacity(self.length);
        let mut edges = Vec::with_capacity(self.length.saturating_sub(1));

        for _ in 0..self.length {
            nodes.push(target.create_node("N", Properties::new())?);
        }
        for pair in nodes.windows(2) {
            edges.push(target.create_edge("NEXT", pair[0], pair[1], Properties::new())?);
        }

        Ok(DatasetResult { nodes, edges })
    }
}

// ---------------------------------------------------------------------------
// Star: hub node with `spokes` outgoing edges to leaf nodes
// ---------------------------------------------------------------------------

/// A star topology with one hub and `spokes` leaf nodes.
pub struct StarDataset {
    /// Number of leaf nodes (edges from hub).
    pub spokes: usize,
}

impl Dataset for StarDataset {
    fn build(&self, target: &mut dyn BenchmarkTarget) -> Result<DatasetResult> {
        let mut nodes = Vec::with_capacity(1 + self.spokes);
        let mut edges = Vec::with_capacity(self.spokes);

        let hub = target.create_node("Hub", Properties::new())?;
        nodes.push(hub);

        for _ in 0..self.spokes {
            let leaf = target.create_node("Leaf", Properties::new())?;
            nodes.push(leaf);
            edges.push(target.create_edge("CONNECTS", hub, leaf, Properties::new())?);
        }

        Ok(DatasetResult { nodes, edges })
    }
}

// ---------------------------------------------------------------------------
// Tree: binary tree of given depth
// ---------------------------------------------------------------------------

/// A complete binary tree of `depth` levels (2^depth - 1 nodes).
pub struct TreeDataset {
    /// Depth of the tree (root is depth 1).
    pub depth: u32,
}

impl Dataset for TreeDataset {
    fn build(&self, target: &mut dyn BenchmarkTarget) -> Result<DatasetResult> {
        let total_nodes = (1_usize << self.depth) - 1;
        let mut nodes = Vec::with_capacity(total_nodes);
        let mut edges = Vec::with_capacity(total_nodes.saturating_sub(1));

        let root = target.create_node("N", Properties::new())?;
        nodes.push(root);

        let mut level = vec![root];
        for _ in 1..self.depth {
            let mut next_level = Vec::with_capacity(level.len() * 2);
            for &parent in &level {
                let left = target.create_node("N", Properties::new())?;
                let right = target.create_node("N", Properties::new())?;
                edges.push(target.create_edge("CHILD", parent, left, Properties::new())?);
                edges.push(target.create_edge("CHILD", parent, right, Properties::new())?);
                nodes.push(left);
                nodes.push(right);
                next_level.push(left);
                next_level.push(right);
            }
            level = next_level;
        }

        Ok(DatasetResult { nodes, edges })
    }
}

// ---------------------------------------------------------------------------
// Grid: rows x cols grid with RIGHT and DOWN edges
// ---------------------------------------------------------------------------

/// A grid graph of `rows` x `cols` nodes with edges going right and down.
pub struct GridDataset {
    /// Number of rows.
    pub rows: usize,
    /// Number of columns.
    pub cols: usize,
}

impl Dataset for GridDataset {
    fn build(&self, target: &mut dyn BenchmarkTarget) -> Result<DatasetResult> {
        let total_nodes = self.rows * self.cols;
        let total_edges = self.rows * (self.cols - 1) + (self.rows - 1) * self.cols;
        let mut nodes = Vec::with_capacity(total_nodes);
        let mut edges = Vec::with_capacity(total_edges);

        // Create all nodes row-major
        let mut matrix: Vec<Vec<NodeHandle>> = Vec::with_capacity(self.rows);
        for _ in 0..self.rows {
            let mut row = Vec::with_capacity(self.cols);
            for _ in 0..self.cols {
                let h = target.create_node("N", Properties::new())?;
                nodes.push(h);
                row.push(h);
            }
            matrix.push(row);
        }

        // Wire right and down edges
        for r in 0..self.rows {
            for c in 0..self.cols {
                if c + 1 < self.cols {
                    edges.push(target.create_edge(
                        "RIGHT",
                        matrix[r][c],
                        matrix[r][c + 1],
                        Properties::new(),
                    )?);
                }
                if r + 1 < self.rows {
                    edges.push(target.create_edge(
                        "DOWN",
                        matrix[r][c],
                        matrix[r + 1][c],
                        Properties::new(),
                    )?);
                }
            }
        }

        Ok(DatasetResult { nodes, edges })
    }
}

// ---------------------------------------------------------------------------
// Social: LDBC-lite deterministic social network
// ---------------------------------------------------------------------------

/// A deterministic social-network-like graph with persons and posts.
///
/// Each person KNOWS `knows_degree` other persons (modular index wrapping)
/// and AUTHORS `posts_per_person` posts.
pub struct SocialDataset {
    /// Number of person nodes.
    pub persons: usize,
    /// Number of posts per person.
    pub posts_per_person: usize,
    /// Number of KNOWS edges per person.
    pub knows_degree: usize,
}

impl Dataset for SocialDataset {
    fn build(&self, target: &mut dyn BenchmarkTarget) -> Result<DatasetResult> {
        let total_nodes = self.persons + self.persons * self.posts_per_person;
        let total_edges = self.persons * self.knows_degree + self.persons * self.posts_per_person;
        let mut nodes = Vec::with_capacity(total_nodes);
        let mut edges = Vec::with_capacity(total_edges);

        // Create person nodes
        let mut person_handles = Vec::with_capacity(self.persons);
        for _ in 0..self.persons {
            let h = target.create_node("Person", Properties::new())?;
            nodes.push(h);
            person_handles.push(h);
        }

        // KNOWS edges (deterministic: person i knows person (i + k + 1) % persons)
        for (i, &person) in person_handles.iter().enumerate() {
            for k in 0..self.knows_degree {
                let j = (i + k + 1) % self.persons;
                edges.push(target.create_edge(
                    "KNOWS",
                    person,
                    person_handles[j],
                    Properties::new(),
                )?);
            }
        }

        // Create posts and AUTHORED edges
        for &person in &person_handles {
            for _ in 0..self.posts_per_person {
                let post = target.create_node("Post", Properties::new())?;
                nodes.push(post);
                edges.push(target.create_edge("AUTHORED", person, post, Properties::new())?);
            }
        }

        Ok(DatasetResult { nodes, edges })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tessera_target::TesseraTarget;

    #[test]
    fn chain_dataset_node_and_edge_counts() {
        let mut t = TesseraTarget::new();
        let ds = ChainDataset { length: 5 };
        let result = ds.build(&mut t).unwrap();
        assert_eq!(result.nodes.len(), 5);
        assert_eq!(result.edges.len(), 4);
    }

    #[test]
    fn star_dataset_node_and_edge_counts() {
        let mut t = TesseraTarget::new();
        let ds = StarDataset { spokes: 8 };
        let result = ds.build(&mut t).unwrap();
        assert_eq!(result.nodes.len(), 9);
        assert_eq!(result.edges.len(), 8);
    }

    #[test]
    fn tree_dataset_depth_3_counts() {
        let mut t = TesseraTarget::new();
        let ds = TreeDataset { depth: 3 };
        let result = ds.build(&mut t).unwrap();
        // depth 3: 1 + 2 + 4 = 7 nodes, 6 edges
        assert_eq!(result.nodes.len(), 7);
        assert_eq!(result.edges.len(), 6);
    }

    #[test]
    fn grid_dataset_3x3_counts() {
        let mut t = TesseraTarget::new();
        let ds = GridDataset { rows: 3, cols: 3 };
        let result = ds.build(&mut t).unwrap();
        // 9 nodes; edges: row-right (3*2=6) + col-down (2*3=6) = 12
        assert_eq!(result.nodes.len(), 9);
        assert_eq!(result.edges.len(), 12);
    }

    #[test]
    fn social_dataset_10_persons_counts() {
        let mut t = TesseraTarget::new();
        let ds = SocialDataset {
            persons: 10,
            posts_per_person: 2,
            knows_degree: 3,
        };
        let result = ds.build(&mut t).unwrap();
        // persons: 10, posts: 20 → 30 nodes
        // KNOWS edges: 10 * 3 = 30, AUTHORED: 10 * 2 = 20 → 50 edges
        assert_eq!(result.nodes.len(), 30);
        assert_eq!(result.edges.len(), 50);
    }

    #[test]
    fn chain_dataset_single_node() {
        let mut t = TesseraTarget::new();
        let ds = ChainDataset { length: 1 };
        let result = ds.build(&mut t).unwrap();
        assert_eq!(result.nodes.len(), 1);
        assert_eq!(result.edges.len(), 0);
    }
}
