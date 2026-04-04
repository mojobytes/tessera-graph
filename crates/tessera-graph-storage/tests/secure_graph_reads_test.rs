// Copyright 2026 BelowZero Security OU. All rights reserved.

use std::collections::BTreeSet;
use tessera_graph_auth::lbac::{Clearance, SecurityLabel, SecurityPolicy};
use tessera_graph::{Graph, GraphAccess, props};
use tessera_graph_storage::lbac::SecureGraph;

fn comps(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|s| (*s).to_string()).collect()
}

fn clearance(level: u16, compartments: &[&str]) -> Clearance {
    Clearance::new(level, comps(compartments))
}

fn make_graph_with_node(level: u16, compartments: &[&str]) -> (Graph, tessera_graph::NodeId) {
    let mut g = Graph::new();
    let label = SecurityLabel::new(level, comps(compartments));
    let mut p = props! { "name" => "Alice" };
    SecurityPolicy::inject_label(&mut p, &label);
    let id = g.add_node("Person", p).unwrap();
    (g, id)
}

// --- node() ---

#[test]
fn node_returns_node_when_clearance_dominates() {
    let (mut g, id) = make_graph_with_node(2, &["FINANCE"]);
    let sg = SecureGraph::new(&mut g, clearance(3, &["FINANCE", "HR"]));
    let node = sg.node(id).unwrap();
    assert_eq!(node.label(), "Person");
}

#[test]
fn node_strips_security_properties_from_result() {
    let (mut g, id) = make_graph_with_node(2, &["FINANCE"]);
    let sg = SecureGraph::new(&mut g, clearance(3, &["FINANCE"]));
    let node = sg.node(id).unwrap();
    assert!(!node.properties().contains_key(SecurityPolicy::LEVEL_KEY));
    assert!(
        !node
            .properties()
            .contains_key(SecurityPolicy::COMPARTMENTS_KEY)
    );
}

#[test]
fn node_returns_not_found_when_level_insufficient() {
    let (mut g, id) = make_graph_with_node(5, &[]);
    let sg = SecureGraph::new(&mut g, clearance(4, &[]));
    assert!(sg.node(id).is_err());
}

#[test]
fn node_returns_not_found_when_compartment_missing() {
    let (mut g, id) = make_graph_with_node(1, &["LEGAL"]);
    let sg = SecureGraph::new(&mut g, clearance(5, &["FINANCE"]));
    assert!(sg.node(id).is_err());
}

#[test]
fn node_public_resource_visible_to_zero_clearance() {
    let (mut g, id) = make_graph_with_node(0, &[]);
    let sg = SecureGraph::new(&mut g, clearance(0, &[]));
    assert!(sg.node(id).is_ok());
}

// --- node_ids() ---

#[test]
fn node_ids_filters_inaccessible_nodes() {
    let mut g = Graph::new();
    let label_pub = SecurityLabel::default();
    let label_fin = SecurityLabel::new(1, comps(&["FINANCE"]));
    let mut p1 = props! { "x" => 1_i64 };
    SecurityPolicy::inject_label(&mut p1, &label_pub);
    let mut p2 = props! { "x" => 2_i64 };
    SecurityPolicy::inject_label(&mut p2, &label_fin);
    g.add_node("N", p1).unwrap();
    g.add_node("N", p2).unwrap();
    let sg = SecureGraph::new(&mut g, clearance(0, &[]));
    assert_eq!(sg.node_ids().len(), 1);
}

#[test]
fn nodes_by_label_filters_inaccessible() {
    let mut g = Graph::new();
    let label_pub = SecurityLabel::default();
    let label_fin = SecurityLabel::new(0, comps(&["FINANCE"]));
    let mut p1 = props! {};
    SecurityPolicy::inject_label(&mut p1, &label_pub);
    let mut p2 = props! {};
    SecurityPolicy::inject_label(&mut p2, &label_fin);
    g.add_node("Person", p1).unwrap();
    g.add_node("Person", p2).unwrap();
    let sg = SecureGraph::new(&mut g, clearance(0, &[]));
    assert_eq!(sg.nodes_by_label("Person").len(), 1);
}

// --- node_count() ---

#[test]
fn node_count_reflects_only_accessible_nodes() {
    let mut g = Graph::new();
    let label_pub = SecurityLabel::default();
    let label_class = SecurityLabel::new(3, comps(&["CLASSIFIED"]));
    let mut p1 = props! {};
    SecurityPolicy::inject_label(&mut p1, &label_pub);
    let mut p2 = props! {};
    SecurityPolicy::inject_label(&mut p2, &label_class);
    g.add_node("N", p1).unwrap();
    g.add_node("N", p2).unwrap();
    let sg = SecureGraph::new(&mut g, clearance(0, &[]));
    assert_eq!(sg.node_count(), 1);
}

// --- node_exists() ---

#[test]
fn node_exists_returns_false_for_inaccessible_node() {
    let (mut g, id) = make_graph_with_node(5, &["TOP_SECRET"]);
    let sg = SecureGraph::new(&mut g, clearance(0, &[]));
    assert!(!sg.node_exists(id));
}

#[test]
fn node_exists_returns_true_for_accessible_node() {
    let (mut g, id) = make_graph_with_node(0, &[]);
    let sg = SecureGraph::new(&mut g, clearance(0, &[]));
    assert!(sg.node_exists(id));
}

// --- edge() and outgoing/incoming ---

fn make_graph_with_edge(
    node_level: u16,
    edge_level: u16,
    compartments: &[&str],
) -> (Graph, tessera_graph::EdgeId) {
    let mut g = Graph::new();
    let c = comps(compartments);
    let node_label = SecurityLabel::new(node_level, c.clone());
    let edge_label = SecurityLabel::new(edge_level, c);
    let mut np = props! {};
    SecurityPolicy::inject_label(&mut np, &node_label);
    let src = g.add_node("N", np.clone()).unwrap();
    let tgt = g.add_node("N", np).unwrap();
    let mut ep = props! {};
    SecurityPolicy::inject_label(&mut ep, &edge_label);
    let eid = g.add_edge("E", src, tgt, ep).unwrap();
    (g, eid)
}

#[test]
fn edge_returns_edge_when_clearance_dominates_all_three() {
    let (mut g, eid) = make_graph_with_edge(1, 1, &["FINANCE"]);
    let sg = SecureGraph::new(&mut g, clearance(2, &["FINANCE"]));
    assert!(sg.edge(eid).is_ok());
}

#[test]
fn edge_strips_security_properties_from_result() {
    let (mut g, eid) = make_graph_with_edge(1, 1, &["FINANCE"]);
    let sg = SecureGraph::new(&mut g, clearance(2, &["FINANCE"]));
    let edge = sg.edge(eid).unwrap();
    assert!(!edge.properties().contains_key(SecurityPolicy::LEVEL_KEY));
    assert!(
        !edge
            .properties()
            .contains_key(SecurityPolicy::COMPARTMENTS_KEY)
    );
}

#[test]
fn edge_not_visible_when_endpoint_node_inaccessible() {
    let mut g = Graph::new();
    let node_label = SecurityLabel::new(0, comps(&["SECRET"]));
    let mut np = props! {};
    SecurityPolicy::inject_label(&mut np, &node_label);
    let src = g.add_node("N", np.clone()).unwrap();
    let tgt = g.add_node("N", np).unwrap();
    let edge_label = SecurityLabel::default();
    let mut ep = props! {};
    SecurityPolicy::inject_label(&mut ep, &edge_label);
    g.add_edge("E", src, tgt, ep).unwrap();
    let sg = SecureGraph::new(&mut g, clearance(0, &[]));
    // edge_count should be 0 because endpoints are not visible
    assert_eq!(sg.edge_count(), 0);
}

#[test]
fn outgoing_edges_filters_inaccessible_edges() {
    let mut g = Graph::new();
    let label_pub = SecurityLabel::default();
    let mut np = props! {};
    SecurityPolicy::inject_label(&mut np, &label_pub);
    let src = g.add_node("N", np.clone()).unwrap();
    let tgt = g.add_node("N", np).unwrap();
    // Public edge
    let mut ep_pub = props! {};
    SecurityPolicy::inject_label(&mut ep_pub, &label_pub);
    g.add_edge("E", src, tgt, ep_pub).unwrap();
    // Classified edge
    let label_class = SecurityLabel::new(0, comps(&["FINANCE"]));
    let mut ep_class = props! {};
    SecurityPolicy::inject_label(&mut ep_class, &label_class);
    g.add_edge("E", src, tgt, ep_class).unwrap();
    let sg = SecureGraph::new(&mut g, clearance(0, &[]));
    let edges = sg.outgoing_edges(src).unwrap();
    assert_eq!(edges.len(), 1);
}

#[test]
fn edge_count_counts_only_accessible_edges() {
    let mut g = Graph::new();
    let label_pub = SecurityLabel::default();
    let label_fin = SecurityLabel::new(0, comps(&["FINANCE"]));
    let mut np = props! {};
    SecurityPolicy::inject_label(&mut np, &label_pub);
    let src = g.add_node("N", np.clone()).unwrap();
    let tgt = g.add_node("N", np).unwrap();
    let mut ep_pub = props! {};
    SecurityPolicy::inject_label(&mut ep_pub, &label_pub);
    let mut ep_fin = props! {};
    SecurityPolicy::inject_label(&mut ep_fin, &label_fin);
    g.add_edge("E", src, tgt, ep_pub).unwrap();
    g.add_edge("E", src, tgt, ep_fin).unwrap();
    let sg = SecureGraph::new(&mut g, clearance(0, &[]));
    assert_eq!(sg.edge_count(), 1);
}

// --- edges_by_label (coverage gap #11) ---

#[test]
fn edges_by_label_filters_inaccessible_edges() {
    let mut g = Graph::new();
    let label_pub = SecurityLabel::default();
    let label_fin = SecurityLabel::new(0, comps(&["FINANCE"]));
    let mut np = props! {};
    SecurityPolicy::inject_label(&mut np, &label_pub);
    let src = g.add_node("N", np.clone()).unwrap();
    let tgt = g.add_node("N", np).unwrap();
    let mut ep_pub = props! {};
    SecurityPolicy::inject_label(&mut ep_pub, &label_pub);
    g.add_edge("REL", src, tgt, ep_pub).unwrap();
    let mut ep_fin = props! {};
    SecurityPolicy::inject_label(&mut ep_fin, &label_fin);
    g.add_edge("REL", src, tgt, ep_fin).unwrap();
    let sg = SecureGraph::new(&mut g, clearance(0, &[]));
    assert_eq!(sg.edges_by_label("REL").len(), 1);
}

// --- incoming_edges (coverage gap #11) ---

#[test]
fn incoming_edges_filters_inaccessible_edges() {
    let mut g = Graph::new();
    let label_pub = SecurityLabel::default();
    let mut np = props! {};
    SecurityPolicy::inject_label(&mut np, &label_pub);
    let src = g.add_node("N", np.clone()).unwrap();
    let tgt = g.add_node("N", np).unwrap();
    let mut ep_pub = props! {};
    SecurityPolicy::inject_label(&mut ep_pub, &label_pub);
    g.add_edge("E", src, tgt, ep_pub).unwrap();
    let label_fin = SecurityLabel::new(0, comps(&["FINANCE"]));
    let mut ep_fin = props! {};
    SecurityPolicy::inject_label(&mut ep_fin, &label_fin);
    g.add_edge("E", src, tgt, ep_fin).unwrap();
    let sg = SecureGraph::new(&mut g, clearance(0, &[]));
    let incoming = sg.incoming_edges(tgt).unwrap();
    assert_eq!(
        incoming.len(),
        1,
        "incoming_edges must filter compartmented edge"
    );
}

// --- node_projected() ---

#[test]
fn node_projected_returns_only_requested_keys() {
    let mut g = Graph::new();
    let label = SecurityLabel::new(1, comps(&["FINANCE"]));
    let mut p = props! { "name" => "Alice", "age" => 30_i64, "city" => "Berlin" };
    SecurityPolicy::inject_label(&mut p, &label);
    let id = g.add_node("Person", p).unwrap(); // OK: test

    let sg = SecureGraph::new(&mut g, clearance(2, &["FINANCE"]));
    let node = sg.node_projected(id, &["name", "age"]).unwrap(); // OK: test
    assert_eq!(node.properties().len(), 2);
    assert!(node.properties().contains_key("name"));
    assert!(node.properties().contains_key("age"));
    assert!(!node.properties().contains_key("city"));
}

#[test]
fn node_projected_strips_security_properties() {
    let (mut g, id) = make_graph_with_node(2, &["FINANCE"]);
    let sg = SecureGraph::new(&mut g, clearance(3, &["FINANCE"]));
    // Request all keys including security ones — they must be stripped
    let node = sg
        .node_projected(
            id,
            &["name", SecurityPolicy::LEVEL_KEY, SecurityPolicy::COMPARTMENTS_KEY],
        )
        .unwrap(); // OK: test
    assert!(!node.properties().contains_key(SecurityPolicy::LEVEL_KEY));
    assert!(
        !node
            .properties()
            .contains_key(SecurityPolicy::COMPARTMENTS_KEY)
    );
    assert!(node.properties().contains_key("name"));
}

#[test]
fn node_projected_denied_when_clearance_insufficient() {
    let (mut g, id) = make_graph_with_node(5, &[]);
    let sg = SecureGraph::new(&mut g, clearance(4, &[]));
    assert!(sg.node_projected(id, &["name"]).is_err());
}

#[test]
fn node_projected_empty_keys_returns_no_properties() {
    let (mut g, id) = make_graph_with_node(1, &[]);
    let sg = SecureGraph::new(&mut g, clearance(2, &[]));
    let node = sg.node_projected(id, &[]).unwrap(); // OK: test
    assert!(node.properties().is_empty());
    assert_eq!(node.label(), "Person");
}
