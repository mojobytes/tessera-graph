// SPDX-License-Identifier: MIT

use tessera_graph::{Direction, Graph, Properties};

#[test]
fn shortest_path_direct_edge() {
    let mut graph = Graph::new();
    let src = graph.add_node("N", Properties::new()).unwrap();
    let dst = graph.add_node("N", Properties::new()).unwrap();
    graph.add_edge("R", src, dst, Properties::new()).unwrap();

    let path = graph
        .shortest_path(src, dst)
        .direction(Direction::Outgoing)
        .find()
        .unwrap()
        .unwrap();

    assert_eq!(path.len(), 1);
    assert_eq!(path.nodes(), &[src, dst]);
}

#[test]
fn shortest_path_two_hops() {
    let mut graph = Graph::new();
    let n0 = graph.add_node("N", Properties::new()).unwrap();
    let n1 = graph.add_node("N", Properties::new()).unwrap();
    let n2 = graph.add_node("N", Properties::new()).unwrap();
    graph.add_edge("R", n0, n1, Properties::new()).unwrap();
    graph.add_edge("R", n1, n2, Properties::new()).unwrap();

    let path = graph
        .shortest_path(n0, n2)
        .direction(Direction::Outgoing)
        .find()
        .unwrap()
        .unwrap();

    assert_eq!(path.len(), 2);
    assert_eq!(path.nodes(), &[n0, n1, n2]);
}

#[test]
fn shortest_path_picks_shortest_in_diamond() {
    let mut graph = Graph::new();
    let n0 = graph.add_node("N", Properties::new()).unwrap();
    let n1 = graph.add_node("N", Properties::new()).unwrap();
    let n2 = graph.add_node("N", Properties::new()).unwrap();
    let n3 = graph.add_node("N", Properties::new()).unwrap();

    // Direct path: n0 -> n3 (1 hop)
    graph.add_edge("R", n0, n3, Properties::new()).unwrap();
    // Longer path: n0 -> n1 -> n2 -> n3 (3 hops)
    graph.add_edge("R", n0, n1, Properties::new()).unwrap();
    graph.add_edge("R", n1, n2, Properties::new()).unwrap();
    graph.add_edge("R", n2, n3, Properties::new()).unwrap();

    let path = graph
        .shortest_path(n0, n3)
        .direction(Direction::Outgoing)
        .find()
        .unwrap()
        .unwrap();

    assert_eq!(path.len(), 1); // direct hop
}

#[test]
fn shortest_path_same_node_returns_trivial() {
    let mut graph = Graph::new();
    let node = graph.add_node("N", Properties::new()).unwrap();

    let path = graph.shortest_path(node, node).find().unwrap().unwrap();
    assert_eq!(path.len(), 0);
    assert_eq!(path.nodes(), &[node]);
}

#[test]
fn shortest_path_unreachable_returns_none() {
    let mut graph = Graph::new();
    let n0 = graph.add_node("N", Properties::new()).unwrap();
    let n1 = graph.add_node("N", Properties::new()).unwrap();
    // No edges

    let result = graph
        .shortest_path(n0, n1)
        .direction(Direction::Outgoing)
        .find()
        .unwrap();
    assert!(result.is_none());
}

#[test]
fn shortest_path_wrong_direction_returns_none() {
    let mut graph = Graph::new();
    let n0 = graph.add_node("N", Properties::new()).unwrap();
    let n1 = graph.add_node("N", Properties::new()).unwrap();
    graph.add_edge("R", n0, n1, Properties::new()).unwrap();

    // Try to go from n1 to n0 using Outgoing — impossible
    let result = graph
        .shortest_path(n1, n0)
        .direction(Direction::Outgoing)
        .find()
        .unwrap();
    assert!(result.is_none());

    // But reachable with Incoming
    let path = graph
        .shortest_path(n1, n0)
        .direction(Direction::Incoming)
        .find()
        .unwrap()
        .unwrap();
    assert_eq!(path.len(), 1);
}

#[test]
fn shortest_path_with_label_filter() {
    let mut graph = Graph::new();
    let n0 = graph.add_node("N", Properties::new()).unwrap();
    let n1 = graph.add_node("N", Properties::new()).unwrap();
    let n2 = graph.add_node("N", Properties::new()).unwrap();

    graph.add_edge("KNOWS", n0, n1, Properties::new()).unwrap();
    graph.add_edge("LIKES", n1, n2, Properties::new()).unwrap();

    // Only KNOWS edges — can't reach n2
    let result = graph
        .shortest_path(n0, n2)
        .direction(Direction::Outgoing)
        .label("KNOWS")
        .find()
        .unwrap();
    assert!(result.is_none());
}

#[test]
fn shortest_path_nonexistent_source_returns_error() {
    let mut graph = Graph::new();
    let n0 = graph.add_node("N", Properties::new()).unwrap();
    let n1 = graph.add_node("N", Properties::new()).unwrap();
    graph.remove_node(n0).unwrap();

    assert!(graph.shortest_path(n0, n1).find().is_err());
}

#[test]
fn shortest_path_nonexistent_target_returns_error() {
    let mut graph = Graph::new();
    let n0 = graph.add_node("N", Properties::new()).unwrap();
    let n1 = graph.add_node("N", Properties::new()).unwrap();
    graph.remove_node(n1).unwrap();

    assert!(graph.shortest_path(n0, n1).find().is_err());
}

#[test]
fn shortest_path_with_cycle_finds_shortest() {
    let mut graph = Graph::new();
    let n0 = graph.add_node("N", Properties::new()).unwrap();
    let n1 = graph.add_node("N", Properties::new()).unwrap();
    let n2 = graph.add_node("N", Properties::new()).unwrap();

    graph.add_edge("R", n0, n1, Properties::new()).unwrap();
    graph.add_edge("R", n1, n2, Properties::new()).unwrap();
    graph.add_edge("R", n2, n0, Properties::new()).unwrap(); // cycle

    let path = graph
        .shortest_path(n0, n2)
        .direction(Direction::Outgoing)
        .find()
        .unwrap()
        .unwrap();

    assert_eq!(path.len(), 2); // n0 -> n1 -> n2
}

#[test]
fn shortest_path_edges_are_populated() {
    let mut graph = Graph::new();
    let n0 = graph.add_node("N", Properties::new()).unwrap();
    let n1 = graph.add_node("N", Properties::new()).unwrap();
    let eid = graph.add_edge("R", n0, n1, Properties::new()).unwrap();

    let path = graph
        .shortest_path(n0, n1)
        .direction(Direction::Outgoing)
        .find()
        .unwrap()
        .unwrap();

    assert_eq!(path.edges().len(), 1);
    assert_eq!(path.edges()[0], eid);
}
