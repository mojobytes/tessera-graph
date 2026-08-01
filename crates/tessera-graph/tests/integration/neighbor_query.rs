// SPDX-License-Identifier: LicenseRef-TesseraGraph-Proprietary

use tessera_graph::{Direction, Graph, Properties, props};

/// Helper: builds a small graph: A --KNOWS--> B --LIKES--> C, B --KNOWS--> A
fn triangle_graph() -> (Graph, tessera_graph::NodeId, tessera_graph::NodeId, tessera_graph::NodeId) {
    let mut g = Graph::new();
    let a = g.add_node("Person", props! { "name" => "Alice" }).unwrap();
    let b = g.add_node("Person", props! { "name" => "Bob" }).unwrap();
    let c = g.add_node("Thing", props! { "name" => "Cats" }).unwrap();

    g.add_edge("KNOWS", a, b, Properties::new()).unwrap();
    g.add_edge("LIKES", b, c, Properties::new()).unwrap();
    g.add_edge("KNOWS", b, a, Properties::new()).unwrap();

    (g, a, b, c)
}

#[test]
fn neighbors_outgoing_returns_correct_edges() {
    let (g, a, b, _c) = triangle_graph();

    let edges = g.neighbors(a).direction(Direction::Outgoing).collect().unwrap();
    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0].label(), "KNOWS");
    assert_eq!(edges[0].target(), b);
}

#[test]
fn neighbors_incoming_returns_correct_edges() {
    let (g, a, b, _c) = triangle_graph();

    // A has one incoming edge: B --KNOWS--> A
    let edges = g.neighbors(a).direction(Direction::Incoming).collect().unwrap();
    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0].label(), "KNOWS");
    assert_eq!(edges[0].source(), b);
}

#[test]
fn neighbors_both_returns_all_edges() {
    let (g, _a, b, _c) = triangle_graph();

    // B has: outgoing LIKES->C, outgoing KNOWS->A, incoming KNOWS from A
    let edges = g.neighbors(b).direction(Direction::Both).collect().unwrap();
    assert_eq!(edges.len(), 3);
}

#[test]
fn neighbors_default_direction_is_both() {
    let (g, _a, b, _c) = triangle_graph();

    // No explicit direction call — should default to Both
    let edges = g.neighbors(b).collect().unwrap();
    assert_eq!(edges.len(), 3);
}

#[test]
fn neighbors_with_label_filter() {
    let (g, _a, b, _c) = triangle_graph();

    // B's outgoing: LIKES->C, KNOWS->A. Filter to LIKES only.
    let edges = g
        .neighbors(b)
        .direction(Direction::Outgoing)
        .label("LIKES")
        .collect()
        .unwrap();
    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0].label(), "LIKES");
}

#[test]
fn neighbors_label_filter_no_match() {
    let (g, a, _b, _c) = triangle_graph();

    let edges = g
        .neighbors(a)
        .direction(Direction::Outgoing)
        .label("NONEXISTENT")
        .collect()
        .unwrap();
    assert!(edges.is_empty());
}

#[test]
fn neighbors_node_ids_returns_other_end() {
    let (g, _a, b, c) = triangle_graph();

    // B outgoing LIKES -> C
    let ids = g
        .neighbors(b)
        .direction(Direction::Outgoing)
        .label("LIKES")
        .node_ids()
        .unwrap();
    assert_eq!(ids, vec![c]);
}

#[test]
fn neighbors_nonexistent_node_returns_error() {
    let mut g = Graph::new();
    let a = g.add_node("Temp", Properties::new()).unwrap();
    g.remove_node(a).unwrap();

    // a no longer exists
    let result = g.neighbors(a).collect();
    assert!(result.is_err());
}

#[test]
fn neighbors_isolated_node_returns_empty() {
    let mut g = Graph::new();
    let a = g.add_node("Lonely", Properties::new()).unwrap();

    let edges = g.neighbors(a).collect().unwrap();
    assert!(edges.is_empty());
}

#[test]
fn neighbors_label_filter_does_not_load_unrelated_edges() {
    let mut g = Graph::new();
    let hub = g.add_node("Person", props! { "name" => "Hub" }).unwrap();

    // 5 outgoing KNOWS edges
    for i in 0..5 {
        let target = g
            .add_node("Person", props! { "name" => format!("K{i}") })
            .unwrap();
        g.add_edge("KNOWS", hub, target, Properties::new()).unwrap();
    }

    // 5 outgoing LIKES edges
    for i in 0..5 {
        let target = g
            .add_node("Thing", props! { "name" => format!("L{i}") })
            .unwrap();
        g.add_edge("LIKES", hub, target, Properties::new()).unwrap();
    }

    let edges = g
        .neighbors(hub)
        .direction(Direction::Outgoing)
        .label("KNOWS")
        .collect()
        .unwrap();

    assert_eq!(edges.len(), 5, "should return exactly 5 KNOWS edges");
    for edge in &edges {
        assert_eq!(edge.label(), "KNOWS", "all edges must have label KNOWS");
    }
}

#[test]
fn neighbor_query_label_accepts_str_ref() {
    let (g, _a, b, _c) = triangle_graph();

    // &str literal must compile and filter correctly — no String clone
    let edges = g
        .neighbors(b)
        .direction(Direction::Outgoing)
        .label("KNOWS")
        .collect()
        .unwrap();
    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0].label(), "KNOWS");
}

#[test]
fn node_exists_returns_correct_values() {
    let mut g = Graph::new();
    let a = g.add_node("A", Properties::new()).unwrap();
    let b = g.add_node("B", Properties::new()).unwrap();
    g.remove_node(b).unwrap();

    assert!(g.node_exists(a));
    assert!(!g.node_exists(b));
}
