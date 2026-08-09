// SPDX-License-Identifier: MIT

use tessera_graph::{Direction, Edge, Graph, Properties, Property, props};

fn cost(edge: &Edge) -> f64 {
    match edge.properties().get("cost") {
        Some(Property::F64(v)) => *v,
        _ => 1.0,
    }
}

#[test]
fn dijkstra_direct_edge() {
    let mut graph = Graph::new();
    let n0 = graph.add_node("N", Properties::new()).unwrap();
    let n1 = graph.add_node("N", Properties::new()).unwrap();
    graph
        .add_edge("R", n0, n1, props! { "cost" => 5.0 })
        .unwrap();

    let (total, path) = graph
        .weighted_shortest_path(n0, n1)
        .direction(Direction::Outgoing)
        .weight(cost)
        .find()
        .unwrap()
        .unwrap();

    assert!((total - 5.0).abs() < f64::EPSILON);
    assert_eq!(path.nodes(), &[n0, n1]);
}

#[test]
fn dijkstra_picks_cheaper_two_hop_over_expensive_direct() {
    let mut graph = Graph::new();
    let n0 = graph.add_node("N", Properties::new()).unwrap();
    let n1 = graph.add_node("N", Properties::new()).unwrap();
    let n2 = graph.add_node("N", Properties::new()).unwrap();

    // Expensive direct: cost 10
    graph
        .add_edge("R", n0, n2, props! { "cost" => 10.0 })
        .unwrap();
    // Cheap via n1: cost 1 + 2 = 3
    graph
        .add_edge("R", n0, n1, props! { "cost" => 1.0 })
        .unwrap();
    graph
        .add_edge("R", n1, n2, props! { "cost" => 2.0 })
        .unwrap();

    let (total, path) = graph
        .weighted_shortest_path(n0, n2)
        .direction(Direction::Outgoing)
        .weight(cost)
        .find()
        .unwrap()
        .unwrap();

    assert!((total - 3.0).abs() < f64::EPSILON);
    assert_eq!(path.nodes(), &[n0, n1, n2]);
}

#[test]
fn dijkstra_same_node_returns_zero_cost() {
    let mut graph = Graph::new();
    let node = graph.add_node("N", Properties::new()).unwrap();

    let (total, path) = graph
        .weighted_shortest_path(node, node)
        .find()
        .unwrap()
        .unwrap();

    assert!((total - 0.0).abs() < f64::EPSILON);
    assert_eq!(path.len(), 0);
}

#[test]
fn dijkstra_unreachable_returns_none() {
    let mut graph = Graph::new();
    let n0 = graph.add_node("N", Properties::new()).unwrap();
    let n1 = graph.add_node("N", Properties::new()).unwrap();

    let result = graph
        .weighted_shortest_path(n0, n1)
        .direction(Direction::Outgoing)
        .find()
        .unwrap();
    assert!(result.is_none());
}

#[test]
fn dijkstra_nonexistent_node_returns_error() {
    let mut graph = Graph::new();
    let n0 = graph.add_node("N", Properties::new()).unwrap();
    let n1 = graph.add_node("N", Properties::new()).unwrap();
    graph.remove_node(n1).unwrap();

    assert!(graph.weighted_shortest_path(n0, n1).find().is_err());
}

#[test]
fn dijkstra_with_unit_weight_acts_like_bfs() {
    let mut graph = Graph::new();
    let n0 = graph.add_node("N", Properties::new()).unwrap();
    let n1 = graph.add_node("N", Properties::new()).unwrap();
    let n2 = graph.add_node("N", Properties::new()).unwrap();

    graph.add_edge("R", n0, n1, Properties::new()).unwrap();
    graph.add_edge("R", n1, n2, Properties::new()).unwrap();

    // Default weight is 1.0 per edge
    let (total, path) = graph
        .weighted_shortest_path(n0, n2)
        .direction(Direction::Outgoing)
        .find()
        .unwrap()
        .unwrap();

    assert!((total - 2.0).abs() < f64::EPSILON);
    assert_eq!(path.len(), 2);
}

#[test]
fn dijkstra_with_label_filter() {
    let mut graph = Graph::new();
    let n0 = graph.add_node("N", Properties::new()).unwrap();
    let n1 = graph.add_node("N", Properties::new()).unwrap();
    let n2 = graph.add_node("N", Properties::new()).unwrap();

    graph
        .add_edge("ROAD", n0, n1, props! { "cost" => 1.0 })
        .unwrap();
    graph
        .add_edge("RAIL", n1, n2, props! { "cost" => 1.0 })
        .unwrap();

    // Only ROAD — can't reach n2
    let result = graph
        .weighted_shortest_path(n0, n2)
        .direction(Direction::Outgoing)
        .label("ROAD")
        .weight(cost)
        .find()
        .unwrap();
    assert!(result.is_none());
}

#[test]
fn dijkstra_cycle_does_not_loop() {
    let mut graph = Graph::new();
    let n0 = graph.add_node("N", Properties::new()).unwrap();
    let n1 = graph.add_node("N", Properties::new()).unwrap();
    let n2 = graph.add_node("N", Properties::new()).unwrap();

    graph
        .add_edge("R", n0, n1, props! { "cost" => 1.0 })
        .unwrap();
    graph
        .add_edge("R", n1, n2, props! { "cost" => 1.0 })
        .unwrap();
    graph
        .add_edge("R", n2, n0, props! { "cost" => 1.0 })
        .unwrap();

    let (total, path) = graph
        .weighted_shortest_path(n0, n2)
        .direction(Direction::Outgoing)
        .weight(cost)
        .find()
        .unwrap()
        .unwrap();

    assert!((total - 2.0).abs() < f64::EPSILON);
    assert_eq!(path.len(), 2);
}
