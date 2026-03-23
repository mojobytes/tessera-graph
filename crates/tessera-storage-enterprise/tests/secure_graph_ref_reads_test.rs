// Copyright 2026 BelowZero Security OU. All rights reserved.

use std::collections::BTreeSet;
use tessera_auth::lbac::{Clearance, SecurityLabel, SecurityPolicy};
use tessera_graph::{props, Graph, GraphAccess};
use tessera_storage_enterprise::lbac::SecureGraphRef;

fn comps(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|s| (*s).to_string()).collect()
}

fn clearance(level: u16, compartments: &[&str]) -> Clearance {
    Clearance::new(level, comps(compartments))
}

fn make_graph_with_node(level: u16, compartments: &[&str]) -> (Graph, tessera_graph::NodeId) {
    let mut g = Graph::new();
    let security_label = SecurityLabel::new(level, comps(compartments));
    let mut p = props! { "name" => "Alice" };
    SecurityPolicy::inject_label(&mut p, &security_label);
    let id = g.add_node("Person", p).unwrap();
    (g, id)
}

// --- node() read filtering ---

#[test]
fn ref_node_returns_node_when_clearance_dominates() {
    let (g, id) = make_graph_with_node(2, &["FINANCE"]);
    let sg = SecureGraphRef::new(&g, clearance(3, &["FINANCE", "HR"]));
    assert!(sg.node(id).is_ok());
}

#[test]
fn ref_node_strips_security_properties() {
    let (g, id) = make_graph_with_node(2, &["FINANCE"]);
    let sg = SecureGraphRef::new(&g, clearance(3, &["FINANCE"]));
    let node = sg.node(id).unwrap();
    assert!(!node.properties().contains_key(SecurityPolicy::LEVEL_KEY));
    assert!(
        !node
            .properties()
            .contains_key(SecurityPolicy::COMPARTMENTS_KEY)
    );
}

#[test]
fn ref_node_denied_when_level_insufficient() {
    let (g, id) = make_graph_with_node(5, &[]);
    let sg = SecureGraphRef::new(&g, clearance(4, &[]));
    assert!(sg.node(id).is_err());
}

#[test]
fn ref_node_denied_when_compartment_missing() {
    let (g, id) = make_graph_with_node(1, &["LEGAL"]);
    let sg = SecureGraphRef::new(&g, clearance(5, &["FINANCE"]));
    assert!(sg.node(id).is_err());
}

#[test]
fn ref_public_resource_visible_to_zero_clearance() {
    let (g, id) = make_graph_with_node(0, &[]);
    let sg = SecureGraphRef::new(&g, clearance(0, &[]));
    assert!(sg.node(id).is_ok());
}

// --- node_ids() ---

#[test]
fn ref_node_ids_filters_inaccessible_nodes() {
    let mut g = Graph::new();
    let pub_label = SecurityLabel::default();
    let fin_label = SecurityLabel::new(1, comps(&["FINANCE"]));
    let mut p1 = props! {};
    SecurityPolicy::inject_label(&mut p1, &pub_label);
    let mut p2 = props! {};
    SecurityPolicy::inject_label(&mut p2, &fin_label);
    g.add_node("N", p1).unwrap();
    g.add_node("N", p2).unwrap();
    let sg = SecureGraphRef::new(&g, clearance(0, &[]));
    assert_eq!(sg.node_ids().len(), 1);
}

// --- nodes_by_label() ---

#[test]
fn ref_nodes_by_label_filters_inaccessible() {
    let mut g = Graph::new();
    let pub_label = SecurityLabel::default();
    let fin_label = SecurityLabel::new(0, comps(&["FINANCE"]));
    let mut p1 = props! {};
    SecurityPolicy::inject_label(&mut p1, &pub_label);
    let mut p2 = props! {};
    SecurityPolicy::inject_label(&mut p2, &fin_label);
    g.add_node("Person", p1).unwrap();
    g.add_node("Person", p2).unwrap();
    let sg = SecureGraphRef::new(&g, clearance(0, &[]));
    assert_eq!(sg.nodes_by_label("Person").len(), 1);
}

// --- node_count() ---

#[test]
fn ref_node_count_reflects_only_accessible() {
    let mut g = Graph::new();
    let pub_label = SecurityLabel::default();
    let class_label = SecurityLabel::new(3, comps(&["CLASSIFIED"]));
    let mut p1 = props! {};
    SecurityPolicy::inject_label(&mut p1, &pub_label);
    let mut p2 = props! {};
    SecurityPolicy::inject_label(&mut p2, &class_label);
    g.add_node("N", p1).unwrap();
    g.add_node("N", p2).unwrap();
    let sg = SecureGraphRef::new(&g, clearance(0, &[]));
    assert_eq!(sg.node_count(), 1);
}

// --- node_exists() ---

#[test]
fn ref_node_exists_returns_false_for_inaccessible() {
    let (g, id) = make_graph_with_node(5, &["TOP_SECRET"]);
    let sg = SecureGraphRef::new(&g, clearance(0, &[]));
    assert!(!sg.node_exists(id));
}

// --- edge() ---

#[test]
fn ref_edge_visible_when_clearance_dominates_all_three() {
    let mut g = Graph::new();
    let fin_label = SecurityLabel::new(1, comps(&["FINANCE"]));
    let mut np = props! {};
    SecurityPolicy::inject_label(&mut np, &fin_label);
    let src = g.add_node("N", np.clone()).unwrap();
    let tgt = g.add_node("N", np).unwrap();
    let mut ep = props! {};
    SecurityPolicy::inject_label(&mut ep, &fin_label);
    let eid = g.add_edge("E", src, tgt, ep).unwrap();
    let sg = SecureGraphRef::new(&g, clearance(2, &["FINANCE"]));
    assert!(sg.edge(eid).is_ok());
}

#[test]
fn ref_edge_strips_security_properties() {
    let mut g = Graph::new();
    let fin_label = SecurityLabel::new(1, comps(&["FINANCE"]));
    let mut np = props! {};
    SecurityPolicy::inject_label(&mut np, &fin_label);
    let src = g.add_node("N", np.clone()).unwrap();
    let tgt = g.add_node("N", np).unwrap();
    let mut ep = props! {};
    SecurityPolicy::inject_label(&mut ep, &fin_label);
    let eid = g.add_edge("E", src, tgt, ep).unwrap();
    let sg = SecureGraphRef::new(&g, clearance(2, &["FINANCE"]));
    let edge = sg.edge(eid).unwrap();
    assert!(!edge.properties().contains_key(SecurityPolicy::LEVEL_KEY));
}

#[test]
fn ref_edge_not_visible_when_endpoint_inaccessible() {
    let mut g = Graph::new();
    let secret_label = SecurityLabel::new(0, comps(&["SECRET"]));
    let mut np = props! {};
    SecurityPolicy::inject_label(&mut np, &secret_label);
    let src = g.add_node("N", np.clone()).unwrap();
    let tgt = g.add_node("N", np).unwrap();
    let pub_label = SecurityLabel::default();
    let mut ep = props! {};
    SecurityPolicy::inject_label(&mut ep, &pub_label);
    let eid = g.add_edge("E", src, tgt, ep).unwrap();
    let sg = SecureGraphRef::new(&g, clearance(0, &[]));
    assert!(sg.edge(eid).is_err());
}

// --- outgoing_edges() ---

#[test]
fn ref_outgoing_edges_filters_inaccessible() {
    let mut g = Graph::new();
    let pub_label = SecurityLabel::default();
    let mut np = props! {};
    SecurityPolicy::inject_label(&mut np, &pub_label);
    let src = g.add_node("N", np.clone()).unwrap();
    let tgt = g.add_node("N", np).unwrap();
    let mut ep_pub = props! {};
    SecurityPolicy::inject_label(&mut ep_pub, &pub_label);
    g.add_edge("E", src, tgt, ep_pub).unwrap();
    let fin_label = SecurityLabel::new(0, comps(&["FINANCE"]));
    let mut ep_fin = props! {};
    SecurityPolicy::inject_label(&mut ep_fin, &fin_label);
    g.add_edge("E", src, tgt, ep_fin).unwrap();
    let sg = SecureGraphRef::new(&g, clearance(0, &[]));
    let edges = sg.outgoing_edges(src).unwrap();
    assert_eq!(edges.len(), 1);
}

// --- edge_count() ---

#[test]
fn ref_edge_count_counts_only_accessible() {
    let mut g = Graph::new();
    let pub_label = SecurityLabel::default();
    let fin_label = SecurityLabel::new(0, comps(&["FINANCE"]));
    let mut np = props! {};
    SecurityPolicy::inject_label(&mut np, &pub_label);
    let src = g.add_node("N", np.clone()).unwrap();
    let tgt = g.add_node("N", np).unwrap();
    let mut ep_pub = props! {};
    SecurityPolicy::inject_label(&mut ep_pub, &pub_label);
    let mut ep_fin = props! {};
    SecurityPolicy::inject_label(&mut ep_fin, &fin_label);
    g.add_edge("E", src, tgt, ep_pub).unwrap();
    g.add_edge("E", src, tgt, ep_fin).unwrap();
    let sg = SecureGraphRef::new(&g, clearance(0, &[]));
    assert_eq!(sg.edge_count(), 1);
}

// --- Mutation methods return typed errors (not panic) ---

#[test]
fn ref_add_node_returns_error() {
    let g = Graph::new();
    let mut sg = SecureGraphRef::new(&g, clearance(99, &[]));
    let result = sg.add_node("X", props! {});
    assert!(result.is_err(), "add_node on SecureGraphRef must return Err");
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("read-only"),
        "error message must mention 'read-only', got: {err_msg}"
    );
}

#[test]
fn ref_add_edge_returns_error() {
    let (g, id) = make_graph_with_node(0, &[]);
    let mut sg = SecureGraphRef::new(&g, clearance(99, &[]));
    let result = sg.add_edge("E", id, id, props! {});
    assert!(result.is_err());
    let err_msg = result.unwrap_err().to_string();
    assert!(err_msg.contains("read-only"), "got: {err_msg}");
}

#[test]
fn ref_remove_node_returns_error() {
    let (g, id) = make_graph_with_node(0, &[]);
    let mut sg = SecureGraphRef::new(&g, clearance(99, &[]));
    let result = sg.remove_node(id);
    assert!(result.is_err());
    let err_msg = result.unwrap_err().to_string();
    assert!(err_msg.contains("read-only"), "got: {err_msg}");
}

#[test]
fn ref_update_node_returns_error() {
    let (g, id) = make_graph_with_node(0, &[]);
    let node = g.node(id).unwrap();
    let mut sg = SecureGraphRef::new(&g, clearance(99, &[]));
    let result = sg.update_node(id, &node);
    assert!(result.is_err());
}

#[test]
fn ref_update_edge_returns_error() {
    let mut g = Graph::new();
    let pub_label = SecurityLabel::default();
    let mut np = props! {};
    SecurityPolicy::inject_label(&mut np, &pub_label);
    let src = g.add_node("N", np.clone()).unwrap();
    let tgt = g.add_node("N", np).unwrap();
    let mut ep = props! {};
    SecurityPolicy::inject_label(&mut ep, &pub_label);
    let eid = g.add_edge("E", src, tgt, ep).unwrap();
    let edge = g.edge(eid).unwrap();
    let mut sg = SecureGraphRef::new(&g, clearance(99, &[]));
    let result = sg.update_edge(eid, &edge);
    assert!(result.is_err());
}

#[test]
fn ref_remove_edge_returns_error() {
    let mut g = Graph::new();
    let pub_label = SecurityLabel::default();
    let mut np = props! {};
    SecurityPolicy::inject_label(&mut np, &pub_label);
    let src = g.add_node("N", np.clone()).unwrap();
    let tgt = g.add_node("N", np).unwrap();
    let mut ep = props! {};
    SecurityPolicy::inject_label(&mut ep, &pub_label);
    let eid = g.add_edge("E", src, tgt, ep).unwrap();
    let mut sg = SecureGraphRef::new(&g, clearance(99, &[]));
    let result = sg.remove_edge(eid);
    assert!(result.is_err());
    let err_msg = result.unwrap_err().to_string();
    assert!(err_msg.contains("read-only"), "got: {err_msg}");
}
