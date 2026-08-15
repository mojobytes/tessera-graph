// SPDX-License-Identifier: MIT

use ermya_graph::{Graph, props};

#[test]
fn add_nodes_and_edges() {
    let mut g = Graph::new();

    let plant = g
        .add_node(
            "Plant",
            props! { "name" => "Solar Plant A", "country" => "ES" },
        )
        .unwrap();
    let system = g
        .add_node("System", props! { "name" => "Inverter Bank 1" })
        .unwrap();
    let doc = g
        .add_node(
            "Document",
            props! { "type" => "warranty", "expires" => 2027_i64 },
        )
        .unwrap();

    let e1 = g.add_edge("HAS_SYSTEM", plant, system, props! {}).unwrap();
    let e2 = g
        .add_edge("HAS_DOCUMENT", system, doc, props! { "critical" => true })
        .unwrap();

    assert_eq!(g.node_count(), 3);
    assert_eq!(g.edge_count(), 2);

    assert_eq!(g.node(plant).unwrap().label(), "Plant");
    assert_eq!(g.edge(e1).unwrap().label(), "HAS_SYSTEM");
    assert_eq!(g.edge(e2).unwrap().source(), system);
    assert_eq!(g.edge(e2).unwrap().target(), doc);
}

#[test]
fn remove_node_cascades_edges() {
    let mut g = Graph::new();

    let a = g.add_node("A", props! {}).unwrap();
    let b = g.add_node("B", props! {}).unwrap();
    let c = g.add_node("C", props! {}).unwrap();

    g.add_edge("AB", a, b, props! {}).unwrap();
    g.add_edge("BC", b, c, props! {}).unwrap();

    g.remove_node(b).unwrap();

    assert_eq!(g.node_count(), 2);
    assert_eq!(g.edge_count(), 0);
}

#[test]
fn remove_edge_keeps_nodes() {
    let mut g = Graph::new();

    let a = g.add_node("A", props! {}).unwrap();
    let b = g.add_node("B", props! {}).unwrap();
    let eid = g.add_edge("REL", a, b, props! {}).unwrap();

    g.remove_edge(eid).unwrap();

    assert_eq!(g.node_count(), 2);
    assert_eq!(g.edge_count(), 0);
}

#[test]
fn error_on_missing_node() {
    let mut g = Graph::new();
    let temp = g.add_node("Temp", props! {}).unwrap();
    g.remove_node(temp).unwrap();

    assert!(g.node(temp).is_err());
}

#[test]
fn error_on_edge_with_missing_nodes() {
    let mut g = Graph::new();
    let a = g.add_node("A", props! {}).unwrap();
    let temp = g.add_node("Temp", props! {}).unwrap();
    g.remove_node(temp).unwrap();

    assert!(g.add_edge("REL", a, temp, props! {}).is_err());
    assert!(g.add_edge("REL", temp, a, props! {}).is_err());
}

#[test]
fn outgoing_and_incoming_edges() {
    let mut g = Graph::new();

    let a = g.add_node("A", props! {}).unwrap();
    let b = g.add_node("B", props! {}).unwrap();
    let c = g.add_node("C", props! {}).unwrap();

    g.add_edge("AB", a, b, props! {}).unwrap();
    g.add_edge("AC", a, c, props! {}).unwrap();
    g.add_edge("BC", b, c, props! {}).unwrap();

    assert_eq!(g.outgoing_edges(a).unwrap().len(), 2);
    assert_eq!(g.incoming_edges(c).unwrap().len(), 2);
    assert_eq!(g.incoming_edges(a).unwrap().len(), 0);
}

#[test]
fn node_projected_returns_only_requested_properties() {
    let mut g = Graph::new();
    let id = g
        .add_node(
            "Person",
            props! { "name" => "Alice", "age" => 30_i64, "city" => "Madrid" },
        )
        .unwrap();

    let projected = g.node_projected(id, &["name", "age"]).unwrap();
    assert_eq!(projected.id(), id);
    assert_eq!(projected.label(), "Person");
    assert_eq!(projected.properties().len(), 2);
    assert!(projected.properties().contains_key("name"));
    assert!(projected.properties().contains_key("age"));
    assert!(!projected.properties().contains_key("city"));
}

#[test]
fn node_projected_empty_keys_returns_no_properties() {
    let mut g = Graph::new();
    let id = g
        .add_node("Person", props! { "name" => "Alice", "age" => 30_i64 })
        .unwrap();

    let projected = g.node_projected(id, &[]).unwrap();
    assert!(projected.properties().is_empty());
    assert_eq!(projected.label(), "Person");
}

#[test]
fn node_projected_not_found() {
    use ermya_graph::{Error, NodeId};
    let g = Graph::new();
    let result = g.node_projected(NodeId::from_raw(999), &["name"]);
    assert!(matches!(result, Err(Error::NodeNotFound(_))));
}

#[test]
fn node_projected_with_overflow_properties_returns_only_requested_keys() {
    use ermya_graph::Property;
    let mut g = Graph::new();
    let id = g
        .add_node(
            "Big",
            props! { "name" => "test", "payload" => Property::Bytes(vec![0u8; 50]) },
        )
        .unwrap();

    let projected = g.node_projected(id, &["name"]).unwrap();
    assert_eq!(projected.id(), id);
    assert_eq!(projected.label(), "Big");
    assert_eq!(projected.properties().len(), 1);
    assert!(projected.properties().contains_key("name"));
    assert!(!projected.properties().contains_key("payload"));
}
