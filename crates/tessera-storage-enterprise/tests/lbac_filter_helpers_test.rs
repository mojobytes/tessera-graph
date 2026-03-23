// Copyright 2026 BelowZero Security OU. All rights reserved.

use tessera_auth::lbac::{Clearance, SecurityLabel, SecurityPolicy};
use tessera_graph::{props, Graph};
use tessera_storage_enterprise::lbac::filter;

fn clearance(level: u16, comps: &[&str]) -> Clearance {
    Clearance::new(level, comps.iter().map(|s| (*s).to_string()).collect())
}

fn label(level: u16, comps: &[&str]) -> SecurityLabel {
    SecurityLabel::new(level, comps.iter().map(|s| (*s).to_string()).collect())
}

#[test]
fn can_read_props_returns_true_when_clearance_dominates() {
    let mut p = props! {};
    SecurityPolicy::inject_label(&mut p, &label(2, &["FINANCE"]));
    assert!(filter::can_read_props(&clearance(3, &["FINANCE"]), &p));
}

#[test]
fn can_read_props_returns_false_when_level_insufficient() {
    let mut p = props! {};
    SecurityPolicy::inject_label(&mut p, &label(5, &[]));
    assert!(!filter::can_read_props(&clearance(4, &[]), &p));
}

#[test]
fn can_read_props_returns_false_when_compartment_missing() {
    let mut p = props! {};
    SecurityPolicy::inject_label(&mut p, &label(0, &["SECRET"]));
    assert!(!filter::can_read_props(&clearance(10, &[]), &p));
}

#[test]
fn strip_node_removes_security_keys() {
    let mut g = Graph::new();
    let mut p = props! { "name" => "Alice" };
    SecurityPolicy::inject_label(&mut p, &label(1, &["HR"]));
    let id = g.add_node("P", p).unwrap();
    let node = g.node(id).unwrap();
    let stripped = filter::strip_node(node);
    assert!(!stripped.properties().contains_key(SecurityPolicy::LEVEL_KEY));
    assert!(
        !stripped
            .properties()
            .contains_key(SecurityPolicy::COMPARTMENTS_KEY)
    );
    assert_eq!(
        stripped.properties().get("name").and_then(|v| v.as_str()),
        Some("Alice")
    );
}

#[test]
fn strip_edge_removes_security_keys() {
    let mut g = Graph::new();
    let mut np = props! {};
    SecurityPolicy::inject_label(&mut np, &label(0, &[]));
    let src = g.add_node("N", np.clone()).unwrap();
    let tgt = g.add_node("N", np).unwrap();
    let mut ep = props! { "weight" => 42_i64 };
    SecurityPolicy::inject_label(&mut ep, &label(1, &["HR"]));
    let eid = g.add_edge("E", src, tgt, ep).unwrap();
    let edge = g.edge(eid).unwrap();
    let stripped = filter::strip_edge(edge);
    assert!(!stripped.properties().contains_key(SecurityPolicy::LEVEL_KEY));
    assert!(
        !stripped
            .properties()
            .contains_key(SecurityPolicy::COMPARTMENTS_KEY)
    );
    assert_eq!(
        stripped
            .properties()
            .get("weight")
            .and_then(tessera_graph::Property::as_i64),
        Some(42)
    );
}

#[test]
fn edge_visible_for_returns_true_when_all_three_dominated() {
    let mut g = Graph::new();
    let lbl = label(1, &["FINANCE"]);
    let mut np = props! {};
    SecurityPolicy::inject_label(&mut np, &lbl);
    let src = g.add_node("N", np.clone()).unwrap();
    let tgt = g.add_node("N", np).unwrap();
    let mut ep = props! {};
    SecurityPolicy::inject_label(&mut ep, &lbl);
    let eid = g.add_edge("E", src, tgt, ep).unwrap();
    let edge = g.edge(eid).unwrap();
    assert!(filter::edge_visible_for(
        &g,
        &clearance(2, &["FINANCE"]),
        &edge
    ));
}

#[test]
fn edge_visible_for_returns_false_when_endpoint_not_accessible() {
    let mut g = Graph::new();
    let secret = label(5, &["SECRET"]);
    let mut np = props! {};
    SecurityPolicy::inject_label(&mut np, &secret);
    let src = g.add_node("N", np.clone()).unwrap();
    let tgt = g.add_node("N", np).unwrap();
    let mut ep = props! {};
    SecurityPolicy::inject_label(&mut ep, &label(0, &[]));
    let eid = g.add_edge("E", src, tgt, ep).unwrap();
    let edge = g.edge(eid).unwrap();
    assert!(!filter::edge_visible_for(&g, &clearance(0, &[]), &edge));
}

#[test]
fn edge_visible_for_returns_false_when_edge_itself_not_dominated() {
    let mut g = Graph::new();
    let pub_label = label(0, &[]);
    let mut np = props! {};
    SecurityPolicy::inject_label(&mut np, &pub_label);
    let src = g.add_node("N", np.clone()).unwrap();
    let tgt = g.add_node("N", np).unwrap();
    let mut ep = props! {};
    SecurityPolicy::inject_label(&mut ep, &label(5, &[]));
    let eid = g.add_edge("E", src, tgt, ep).unwrap();
    let edge = g.edge(eid).unwrap();
    assert!(!filter::edge_visible_for(&g, &clearance(0, &[]), &edge));
}
