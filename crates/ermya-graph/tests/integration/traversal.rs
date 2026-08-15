// SPDX-License-Identifier: MIT

use ermya_graph::{Direction, Graph, NodeId, Properties};

/// Builds a linear chain: n0 -> n1 -> n2 -> n3
fn chain_graph() -> (Graph, Vec<NodeId>) {
    let mut graph = Graph::new();
    let n0 = graph.add_node("N", Properties::new()).unwrap();
    let n1 = graph.add_node("N", Properties::new()).unwrap();
    let n2 = graph.add_node("N", Properties::new()).unwrap();
    let n3 = graph.add_node("N", Properties::new()).unwrap();

    graph.add_edge("NEXT", n0, n1, Properties::new()).unwrap();
    graph.add_edge("NEXT", n1, n2, Properties::new()).unwrap();
    graph.add_edge("NEXT", n2, n3, Properties::new()).unwrap();

    (graph, vec![n0, n1, n2, n3])
}

/// Builds a diamond: n0 -> n1, n0 -> n2, n1 -> n3, n2 -> n3
fn diamond_graph() -> (Graph, NodeId, NodeId, NodeId, NodeId) {
    let mut graph = Graph::new();
    let n0 = graph.add_node("N", Properties::new()).unwrap();
    let n1 = graph.add_node("N", Properties::new()).unwrap();
    let n2 = graph.add_node("N", Properties::new()).unwrap();
    let n3 = graph.add_node("N", Properties::new()).unwrap();

    graph.add_edge("R", n0, n1, Properties::new()).unwrap();
    graph.add_edge("R", n0, n2, Properties::new()).unwrap();
    graph.add_edge("R", n1, n3, Properties::new()).unwrap();
    graph.add_edge("R", n2, n3, Properties::new()).unwrap();

    (graph, n0, n1, n2, n3)
}

// ---------------------------------------------------------------
// BFS tests
// ---------------------------------------------------------------

#[test]
fn bfs_chain_visits_all_in_order() {
    let (g, nodes) = chain_graph();
    let visited = g
        .traverse(nodes[0])
        .direction(Direction::Outgoing)
        .bfs()
        .collect()
        .unwrap();
    assert_eq!(visited, nodes);
}

#[test]
fn bfs_diamond_visits_all_nodes() {
    let (graph, n0, n1, n2, n3) = diamond_graph();
    let visited = graph
        .traverse(n0)
        .direction(Direction::Outgoing)
        .bfs()
        .collect()
        .unwrap();

    assert_eq!(visited[0], n0);
    // n1 and n2 at depth 1 (order depends on adjacency list)
    assert!(visited.contains(&n1));
    assert!(visited.contains(&n2));
    assert_eq!(visited[3], n3);
    assert_eq!(visited.len(), 4);
}

#[test]
fn bfs_max_depth_zero_returns_only_start() {
    let (g, nodes) = chain_graph();
    let visited = g
        .traverse(nodes[0])
        .direction(Direction::Outgoing)
        .max_depth(0)
        .collect()
        .unwrap();
    assert_eq!(visited, vec![nodes[0]]);
}

#[test]
fn bfs_max_depth_one_returns_start_and_neighbors() {
    let (g, nodes) = chain_graph();
    let visited = g
        .traverse(nodes[0])
        .direction(Direction::Outgoing)
        .max_depth(1)
        .collect()
        .unwrap();
    assert_eq!(visited, vec![nodes[0], nodes[1]]);
}

#[test]
fn bfs_max_depth_two_returns_three_nodes() {
    let (g, nodes) = chain_graph();
    let visited = g
        .traverse(nodes[0])
        .direction(Direction::Outgoing)
        .max_depth(2)
        .collect()
        .unwrap();
    assert_eq!(visited, vec![nodes[0], nodes[1], nodes[2]]);
}

#[test]
fn bfs_with_label_filter() {
    let mut g = Graph::new();
    let a = g.add_node("N", Properties::new()).unwrap();
    let b = g.add_node("N", Properties::new()).unwrap();
    let c = g.add_node("N", Properties::new()).unwrap();

    g.add_edge("KNOWS", a, b, Properties::new()).unwrap();
    g.add_edge("LIKES", a, c, Properties::new()).unwrap();

    let visited = g
        .traverse(a)
        .direction(Direction::Outgoing)
        .label("KNOWS")
        .collect()
        .unwrap();
    assert_eq!(visited, vec![a, b]);
}

#[test]
fn bfs_nonexistent_start_returns_error() {
    let mut g = Graph::new();
    let a = g.add_node("N", Properties::new()).unwrap();
    g.remove_node(a).unwrap();

    assert!(g.traverse(a).collect().is_err());
}

#[test]
fn bfs_isolated_node_returns_only_start() {
    let mut g = Graph::new();
    let a = g.add_node("Lonely", Properties::new()).unwrap();

    let visited = g.traverse(a).collect().unwrap();
    assert_eq!(visited, vec![a]);
}

#[test]
fn bfs_cycle_does_not_loop() {
    let mut g = Graph::new();
    let a = g.add_node("N", Properties::new()).unwrap();
    let b = g.add_node("N", Properties::new()).unwrap();
    g.add_edge("R", a, b, Properties::new()).unwrap();
    g.add_edge("R", b, a, Properties::new()).unwrap();

    let visited = g
        .traverse(a)
        .direction(Direction::Outgoing)
        .collect()
        .unwrap();
    assert_eq!(visited.len(), 2);
    assert!(visited.contains(&a));
    assert!(visited.contains(&b));
}

// ---------------------------------------------------------------
// DFS tests
// ---------------------------------------------------------------

#[test]
fn dfs_chain_visits_all() {
    let (g, nodes) = chain_graph();
    let visited = g
        .traverse(nodes[0])
        .direction(Direction::Outgoing)
        .dfs()
        .collect()
        .unwrap();
    assert_eq!(visited, nodes);
}

#[test]
fn dfs_max_depth_limits_depth() {
    let (g, nodes) = chain_graph();
    let visited = g
        .traverse(nodes[0])
        .direction(Direction::Outgoing)
        .dfs()
        .max_depth(1)
        .collect()
        .unwrap();
    assert_eq!(visited, vec![nodes[0], nodes[1]]);
}

#[test]
fn dfs_cycle_does_not_loop() {
    let mut g = Graph::new();
    let a = g.add_node("N", Properties::new()).unwrap();
    let b = g.add_node("N", Properties::new()).unwrap();
    let c = g.add_node("N", Properties::new()).unwrap();

    g.add_edge("R", a, b, Properties::new()).unwrap();
    g.add_edge("R", b, c, Properties::new()).unwrap();
    g.add_edge("R", c, a, Properties::new()).unwrap();

    let visited = g
        .traverse(a)
        .direction(Direction::Outgoing)
        .dfs()
        .collect()
        .unwrap();
    assert_eq!(visited.len(), 3);
}

// ---------------------------------------------------------------
// collect_paths tests
// ---------------------------------------------------------------

#[test]
fn bfs_collect_paths_chain() {
    let (g, nodes) = chain_graph();
    let paths = g
        .traverse(nodes[0])
        .direction(Direction::Outgoing)
        .bfs()
        .collect_paths()
        .unwrap();

    // 4 nodes → 4 paths (start, start→1, start→1→2, start→1→2→3)
    assert_eq!(paths.len(), 4);
    assert_eq!(paths[0].len(), 0); // trivial path (just start node)
    assert_eq!(paths[1].len(), 1);
    assert_eq!(paths[2].len(), 2);
    assert_eq!(paths[3].len(), 3);

    // Verify path endpoints
    assert_eq!(paths[0].nodes(), &[nodes[0]]);
    assert_eq!(*paths[3].nodes().last().unwrap(), nodes[3]);
}

#[test]
fn dfs_collect_paths_chain() {
    let (g, nodes) = chain_graph();
    let paths = g
        .traverse(nodes[0])
        .direction(Direction::Outgoing)
        .dfs()
        .collect_paths()
        .unwrap();

    assert_eq!(paths.len(), 4);
    assert_eq!(paths[0].nodes(), &[nodes[0]]);
}

// ---------------------------------------------------------------
// Direction::Incoming traversal
// ---------------------------------------------------------------

#[test]
fn bfs_incoming_traverses_reverse() {
    let (g, nodes) = chain_graph();
    // Start from D, traverse incoming
    let visited = g
        .traverse(nodes[3])
        .direction(Direction::Incoming)
        .collect()
        .unwrap();
    assert_eq!(visited, vec![nodes[3], nodes[2], nodes[1], nodes[0]]);
}

// ---------------------------------------------------------------
// Direction::Both traversal
// ---------------------------------------------------------------

#[test]
fn bfs_both_direction_from_middle() {
    let (g, nodes) = chain_graph();
    // Start from B (index 1), direction Both
    let visited = g
        .traverse(nodes[1])
        .direction(Direction::Both)
        .collect()
        .unwrap();
    assert_eq!(visited.len(), 4);
}
