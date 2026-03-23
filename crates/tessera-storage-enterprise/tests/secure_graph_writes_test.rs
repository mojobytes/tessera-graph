// Copyright 2026 BelowZero Security OU. All rights reserved.

use std::collections::BTreeSet;
use tessera_auth::lbac::{Clearance, SecurityLabel, SecurityPolicy};
use tessera_graph::{Graph, GraphAccess, props};
use tessera_storage_enterprise::lbac::SecureGraph;

fn comps(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|s| (*s).to_string()).collect()
}

fn clearance(level: u16, compartments: &[&str]) -> Clearance {
    Clearance::new(level, comps(compartments))
}

fn labeled_props(level: u16, compartments: &[&str]) -> tessera_graph::Properties {
    let label = SecurityLabel::new(level, comps(compartments));
    let mut p = props! {};
    SecurityPolicy::inject_label(&mut p, &label);
    p
}

// --- add_node via GraphAccess trait ---

#[test]
fn add_node_user_cannot_inject_security_level_directly() {
    let mut g = Graph::new();
    let mut sg = SecureGraph::new(&mut g, clearance(5, &[]));
    let mut p = props! { "name" => "Alice" };
    p.insert(
        SecurityPolicy::LEVEL_KEY.to_string(),
        tessera_graph::Property::I64(99),
    );
    let id = sg.add_node("Person", p).unwrap();
    drop(sg);
    let raw = g.node(id).unwrap();
    let stored_level = raw
        .properties()
        .get(SecurityPolicy::LEVEL_KEY)
        .and_then(tessera_graph::Property::as_i64)
        .unwrap_or(0);
    assert_eq!(stored_level, 5, "level must match caller clearance, not user-injected value");
}

#[test]
fn add_node_with_public_clearance_creates_public_node() {
    let mut g = Graph::new();
    let mut sg = SecureGraph::new(&mut g, clearance(0, &[]));
    let id = sg.add_node("Person", props! { "name" => "Bob" }).unwrap();
    drop(sg);
    let raw = g.node(id).unwrap();
    let stored_level = raw
        .properties()
        .get(SecurityPolicy::LEVEL_KEY)
        .and_then(tessera_graph::Property::as_i64)
        .unwrap_or(-1);
    assert_eq!(stored_level, 0);
}

// --- add_node_with_label ---

#[test]
fn add_node_with_label_stores_security_label() {
    let mut g = Graph::new();
    let mut sg = SecureGraph::new(&mut g, clearance(5, &["FINANCE"]));
    let label = SecurityLabel::new(3, comps(&["FINANCE"]));
    let id = sg
        .add_node_with_label("Secret", props! { "data" => "classified" }, &label)
        .unwrap();
    drop(sg);
    let raw = g.node(id).unwrap();
    let extracted = SecurityPolicy::extract_label(raw.properties());
    assert_eq!(extracted.level, 3);
    assert_eq!(extracted.compartments, comps(&["FINANCE"]));
}

#[test]
fn add_node_with_label_denied_if_clearance_insufficient() {
    let mut g = Graph::new();
    let mut sg = SecureGraph::new(&mut g, clearance(2, &[]));
    let label = SecurityLabel::new(3, BTreeSet::new());
    let result = sg.add_node_with_label("Secret", props! {}, &label);
    assert!(result.is_err());
}

#[test]
fn add_node_with_label_denied_if_compartment_missing() {
    let mut g = Graph::new();
    let mut sg = SecureGraph::new(&mut g, clearance(5, &["FINANCE"]));
    let label = SecurityLabel::new(1, comps(&["LEGAL"]));
    let result = sg.add_node_with_label("Secret", props! {}, &label);
    assert!(result.is_err());
}

// --- update_node ---

#[test]
fn update_node_preserves_existing_security_label() {
    let mut g = Graph::new();
    // Create a node with level 2, FINANCE
    let label = SecurityLabel::new(2, comps(&["FINANCE"]));
    let mut p = props! { "name" => "original" };
    SecurityPolicy::inject_label(&mut p, &label);
    let id = g.add_node("N", p).unwrap();

    let mut sg = SecureGraph::new(&mut g, clearance(3, &["FINANCE"]));
    let mut node = sg.node(id).unwrap();
    // Try to update with a new name and sneak in a security property
    node.properties_mut()
        .insert("name".to_string(), tessera_graph::Property::String("updated".to_string()));
    node.properties_mut()
        .insert(SecurityPolicy::LEVEL_KEY.to_string(), tessera_graph::Property::I64(99));
    sg.update_node(id, &node).unwrap();
    drop(sg);

    let raw = g.node(id).unwrap();
    let stored = SecurityPolicy::extract_label(raw.properties());
    assert_eq!(stored.level, 2, "security level must be preserved, not overwritten");
    assert_eq!(stored.compartments, comps(&["FINANCE"]));
    assert_eq!(raw.properties().get("name").and_then(|p| p.as_str()), Some("updated"));
}

#[test]
fn update_node_denied_if_not_visible() {
    let mut g = Graph::new();
    let label = SecurityLabel::new(5, BTreeSet::new());
    let mut p = props! {};
    SecurityPolicy::inject_label(&mut p, &label);
    let id = g.add_node("N", p).unwrap();
    let node = g.node(id).unwrap();

    let mut sg = SecureGraph::new(&mut g, clearance(0, &[]));
    assert!(sg.update_node(id, &node).is_err());
}

// --- remove_node ---

#[test]
fn remove_node_denied_if_not_visible() {
    let mut g = Graph::new();
    let label = SecurityLabel::new(5, comps(&["TOP_SECRET"]));
    let mut p = props! {};
    SecurityPolicy::inject_label(&mut p, &label);
    let id = g.add_node("N", p).unwrap();

    let mut sg = SecureGraph::new(&mut g, clearance(0, &[]));
    assert!(sg.remove_node(id).is_err());
}

#[test]
fn remove_node_succeeds_if_visible() {
    let mut g = Graph::new();
    let label = SecurityLabel::default();
    let mut p = props! { "x" => 1_i64 };
    SecurityPolicy::inject_label(&mut p, &label);
    let id = g.add_node("N", p).unwrap();

    let mut sg = SecureGraph::new(&mut g, clearance(0, &[]));
    let removed = sg.remove_node(id).unwrap();
    assert!(!removed.properties().contains_key(SecurityPolicy::LEVEL_KEY));
}

// --- add_edge via GraphAccess trait ---

#[test]
fn add_edge_creates_public_edge() {
    let mut g = Graph::new();
    let a = g.add_node("N", labeled_props(0, &[])).unwrap();
    let b = g.add_node("N", labeled_props(0, &[])).unwrap();
    let mut sg = SecureGraph::new(&mut g, clearance(0, &[]));
    let eid = sg.add_edge("REL", a, b, props! {}).unwrap();
    drop(sg);
    let raw = g.edge(eid).unwrap();
    let stored = SecurityPolicy::extract_label(raw.properties());
    assert_eq!(stored.level, 0);
    assert!(stored.compartments.is_empty());
}

#[test]
fn add_edge_denied_if_target_not_visible() {
    let mut g = Graph::new();
    let a = g.add_node("N", labeled_props(0, &[])).unwrap();
    let b = g.add_node("N", labeled_props(5, &["SECRET"])).unwrap();
    let mut sg = SecureGraph::new(&mut g, clearance(0, &[]));
    assert!(sg.add_edge("REL", a, b, props! {}).is_err());
}

// --- add_edge_with_label ---

#[test]
fn add_edge_with_label_denied_if_clearance_insufficient() {
    let mut g = Graph::new();
    let a = g.add_node("N", labeled_props(0, &[])).unwrap();
    let b = g.add_node("N", labeled_props(0, &[])).unwrap();
    let mut sg = SecureGraph::new(&mut g, clearance(1, &[]));
    let label = SecurityLabel::new(5, BTreeSet::new());
    assert!(sg.add_edge_with_label("REL", a, b, props! {}, &label).is_err());
}

// --- remove_edge ---

#[test]
fn remove_edge_denied_if_not_visible() {
    let mut g = Graph::new();
    let a = g.add_node("N", labeled_props(5, &[])).unwrap();
    let b = g.add_node("N", labeled_props(5, &[])).unwrap();
    let eid = g.add_edge("E", a, b, labeled_props(5, &[])).unwrap();
    let mut sg = SecureGraph::new(&mut g, clearance(0, &[]));
    assert!(sg.remove_edge(eid).is_err());
}

// --- write-dominance regression guards (#1) ---

#[test]
fn update_node_denied_when_label_has_compartment_caller_lacks() {
    let mut g = Graph::new();
    let label = SecurityLabel::new(3, comps(&["FINANCE", "LEGAL"]));
    let mut p = props! { "data" => "sensitive" };
    SecurityPolicy::inject_label(&mut p, &label);
    let id = g.add_node("N", p).unwrap();
    let node = g.node(id).unwrap();

    // Clearance has FINANCE but not LEGAL — cannot dominate the label
    let mut sg = SecureGraph::new(&mut g, clearance(5, &["FINANCE"]));
    let result = sg.update_node(id, &node);
    assert!(
        result.is_err(),
        "update_node must be denied when clearance does not dominate existing label"
    );
}

#[test]
fn update_edge_denied_when_label_has_compartment_caller_lacks() {
    let mut g = Graph::new();
    let label = SecurityLabel::new(2, comps(&["FINANCE", "LEGAL"]));
    let mut np = props! {};
    SecurityPolicy::inject_label(&mut np, &label);
    let src = g.add_node("N", np.clone()).unwrap();
    let tgt = g.add_node("N", np).unwrap();
    let mut ep = props! {};
    SecurityPolicy::inject_label(&mut ep, &label);
    let eid = g.add_edge("E", src, tgt, ep).unwrap();
    let edge = g.edge(eid).unwrap();

    // Clearance has FINANCE but not LEGAL
    let mut sg = SecureGraph::new(&mut g, clearance(5, &["FINANCE"]));
    assert!(sg.update_edge(eid, &edge).is_err());
}

// --- Bell-LaPadula no write-down: add_node/add_edge inherit caller clearance (#3) ---

#[test]
fn add_node_inherits_caller_clearance_level() {
    let mut g = Graph::new();
    let mut sg = SecureGraph::new(&mut g, clearance(3, &["FINANCE"]));
    let id = sg
        .add_node("Person", props! { "name" => "Charlie" })
        .unwrap();
    drop(sg);
    let raw = g.node(id).unwrap();
    let stored = SecurityPolicy::extract_label(raw.properties());
    assert_eq!(stored.level, 3, "node must inherit caller clearance level");
    assert_eq!(
        stored.compartments,
        comps(&["FINANCE"]),
        "node must inherit caller compartments"
    );
}

#[test]
fn add_edge_inherits_caller_clearance_level() {
    let mut g = Graph::new();
    let label = SecurityLabel::new(3, comps(&["FINANCE"]));
    let mut np = props! {};
    SecurityPolicy::inject_label(&mut np, &label);
    let a = g.add_node("N", np.clone()).unwrap();
    let b = g.add_node("N", np).unwrap();

    let mut sg = SecureGraph::new(&mut g, clearance(3, &["FINANCE"]));
    let eid = sg.add_edge("REL", a, b, props! {}).unwrap();
    drop(sg);
    let raw = g.edge(eid).unwrap();
    let stored = SecurityPolicy::extract_label(raw.properties());
    assert_eq!(stored.level, 3);
    assert_eq!(stored.compartments, comps(&["FINANCE"]));
}
