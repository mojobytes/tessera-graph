// SPDX-License-Identifier: MIT

use tessera_graph::{Graph, NodeId, Properties, Property};

/// Creates a properties map with a short string and an i64 (fits inline).
pub fn small_props() -> Properties {
    let mut p = Properties::new();
    p.insert("name".into(), Property::String("Alice".into()));
    p.insert("age".into(), Property::I64(30));
    p
}

/// Creates a properties map that exceeds inline limits and triggers overflow.
pub fn large_props() -> Properties {
    let mut p = Properties::new();
    p.insert("data".into(), Property::Bytes(vec![0xAB; 50]));
    p.insert("name".into(), Property::String("x".repeat(100)));
    p
}

/// Creates a graph pre-populated with `n` nodes, each with small props.
pub fn graph_with_nodes(n: usize) -> Graph {
    let mut g = Graph::new();
    for _ in 0..n {
        g.add_node("Person", small_props()).unwrap();
    }
    g
}

/// Creates a graph with a star topology: one center node with `degree` outgoing edges.
pub fn star_graph(degree: usize) -> (Graph, NodeId) {
    let mut g = Graph::new();
    let center = g.add_node("Center", Properties::new()).unwrap();
    for _ in 0..degree {
        let leaf = g.add_node("Leaf", Properties::new()).unwrap();
        g.add_edge("CONNECTS", center, leaf, Properties::new())
            .unwrap();
    }
    (g, center)
}

/// Creates a graph with a reverse star topology: one center node with `degree` incoming edges.
pub fn reverse_star_graph(degree: usize) -> (Graph, NodeId) {
    let mut g = Graph::new();
    let center = g.add_node("Center", Properties::new()).unwrap();
    for _ in 0..degree {
        let leaf = g.add_node("Leaf", Properties::new()).unwrap();
        g.add_edge("CONNECTS", leaf, center, Properties::new())
            .unwrap();
    }
    (g, center)
}

/// Creates a graph with `n` nodes forming a linear chain of edges.
/// Returns the graph and the vector of node IDs.
pub fn chain_graph(n: usize) -> Graph {
    let (g, _ids) = chain_graph_with_ids(n);
    g
}

/// Creates a chain graph and also returns node IDs.
pub fn chain_graph_with_ids(n: usize) -> (Graph, Vec<NodeId>) {
    let mut g = Graph::new();
    let mut ids = Vec::with_capacity(n);
    for _ in 0..n {
        ids.push(g.add_node("N", Properties::new()).unwrap());
    }
    for pair in ids.windows(2) {
        g.add_edge("NEXT", pair[0], pair[1], Properties::new())
            .unwrap();
    }
    (g, ids)
}

/// Creates a binary tree of depth `depth` (2^depth - 1 nodes).
pub fn binary_tree_graph(depth: usize) -> (Graph, NodeId) {
    let mut g = Graph::new();
    let root = g.add_node("N", Properties::new()).unwrap();
    let mut level = vec![root];

    for _ in 1..depth {
        let mut next_level = Vec::with_capacity(level.len() * 2);
        for &parent in &level {
            let left = g.add_node("N", Properties::new()).unwrap();
            let right = g.add_node("N", Properties::new()).unwrap();
            g.add_edge("CHILD", parent, left, Properties::new())
                .unwrap();
            g.add_edge("CHILD", parent, right, Properties::new())
                .unwrap();
            next_level.push(left);
            next_level.push(right);
        }
        level = next_level;
    }
    (g, root)
}

/// Creates a grid graph of `rows x cols` with edges to right and down neighbors.
/// Returns the graph and the node ID matrix (row-major).
pub fn grid_graph(rows: usize, cols: usize) -> (Graph, Vec<Vec<NodeId>>) {
    let mut g = Graph::new();
    let mut matrix = Vec::with_capacity(rows);

    for _ in 0..rows {
        let mut row = Vec::with_capacity(cols);
        for _ in 0..cols {
            row.push(g.add_node("N", Properties::new()).unwrap());
        }
        matrix.push(row);
    }

    for r in 0..rows {
        for c in 0..cols {
            if c + 1 < cols {
                g.add_edge("RIGHT", matrix[r][c], matrix[r][c + 1], Properties::new())
                    .unwrap();
            }
            if r + 1 < rows {
                g.add_edge("DOWN", matrix[r][c], matrix[r + 1][c], Properties::new())
                    .unwrap();
            }
        }
    }

    (g, matrix)
}
