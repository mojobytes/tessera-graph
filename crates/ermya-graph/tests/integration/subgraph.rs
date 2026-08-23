// SPDX-License-Identifier: MIT

use ermya_graph::{Direction, Graph, Properties};

#[test]
fn subgraph_extracts_all_reachable() {
    let mut graph = Graph::new();
    let n0 = graph.add_node("N", Properties::new()).unwrap();
    let n1 = graph.add_node("N", Properties::new()).unwrap();
    let n2 = graph.add_node("N", Properties::new()).unwrap();
    let _isolated = graph.add_node("Isolated", Properties::new()).unwrap();

    graph.add_edge("R", n0, n1, Properties::new()).unwrap();
    graph.add_edge("R", n1, n2, Properties::new()).unwrap();

    let sub = graph
        .subgraph(n0)
        .direction(Direction::Outgoing)
        .extract()
        .unwrap();

    assert_eq!(sub.node_count(), 3);
    assert_eq!(sub.edge_count(), 2);
}

#[test]
fn subgraph_max_depth_limits_extraction() {
    let mut graph = Graph::new();
    let n0 = graph.add_node("N", Properties::new()).unwrap();
    let n1 = graph.add_node("N", Properties::new()).unwrap();
    let n2 = graph.add_node("N", Properties::new()).unwrap();

    graph.add_edge("R", n0, n1, Properties::new()).unwrap();
    graph.add_edge("R", n1, n2, Properties::new()).unwrap();

    let sub = graph
        .subgraph(n0)
        .direction(Direction::Outgoing)
        .max_depth(1)
        .extract()
        .unwrap();

    assert_eq!(sub.node_count(), 2); // n0 and n1
    assert_eq!(sub.edge_count(), 1); // n0->n1
}

#[test]
fn subgraph_with_label_filter() {
    let mut graph = Graph::new();
    let n0 = graph.add_node("N", Properties::new()).unwrap();
    let n1 = graph.add_node("N", Properties::new()).unwrap();
    let n2 = graph.add_node("N", Properties::new()).unwrap();

    graph.add_edge("KNOWS", n0, n1, Properties::new()).unwrap();
    graph.add_edge("LIKES", n1, n2, Properties::new()).unwrap();

    let sub = graph
        .subgraph(n0)
        .direction(Direction::Outgoing)
        .label("KNOWS")
        .extract()
        .unwrap();

    // Traversal stops at n1 because no KNOWS edges from n1
    assert_eq!(sub.node_count(), 2);
    assert_eq!(sub.edge_count(), 1);
}

#[test]
fn subgraph_isolated_node() {
    let mut graph = Graph::new();
    let n0 = graph.add_node("Lonely", Properties::new()).unwrap();

    let sub = graph.subgraph(n0).extract().unwrap();
    assert_eq!(sub.node_count(), 1);
    assert_eq!(sub.edge_count(), 0);
}

#[test]
fn subgraph_nonexistent_node_returns_error() {
    let mut graph = Graph::new();
    let n0 = graph.add_node("N", Properties::new()).unwrap();
    graph.remove_node(n0).unwrap();

    assert!(graph.subgraph(n0).extract().is_err());
}

#[test]
fn subgraph_no_duplicate_edges() {
    let mut graph = Graph::new();
    let n0 = graph.add_node("N", Properties::new()).unwrap();
    let n1 = graph.add_node("N", Properties::new()).unwrap();
    graph.add_edge("R", n0, n1, Properties::new()).unwrap();

    // Direction::Both will visit the edge from both sides
    let sub = graph
        .subgraph(n0)
        .direction(Direction::Both)
        .extract()
        .unwrap();

    assert_eq!(sub.edge_count(), 1); // should not duplicate
}

#[test]
fn subgraph_includes_full_node_data() {
    let mut graph = Graph::new();
    let n0 = graph
        .add_node("Person", ermya_graph::props! { "name" => "Alice" })
        .unwrap();

    let sub = graph.subgraph(n0).extract().unwrap();
    assert_eq!(sub.nodes()[0].label(), "Person");
    assert_eq!(
        sub.nodes()[0].properties().get("name"),
        Some(&ermya_graph::Property::String("Alice".into()))
    );
}
