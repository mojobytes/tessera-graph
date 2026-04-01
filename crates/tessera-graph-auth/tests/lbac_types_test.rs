// Copyright 2026 BelowZero Security OU. All rights reserved.

use std::collections::BTreeSet;
use tessera_graph_auth::lbac::{Clearance, SecurityLabel};

fn comps(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|s| (*s).to_string()).collect()
}

// --- SecurityLabel ---

#[test]
fn security_label_default_is_public() {
    let label = SecurityLabel::default();
    assert_eq!(label.level, 0);
    assert!(label.compartments.is_empty());
}

#[test]
fn security_label_new_stores_level_and_compartments() {
    let c = comps(&["FINANCE", "HR"]);
    let label = SecurityLabel::new(3, c.clone());
    assert_eq!(label.level, 3);
    assert_eq!(label.compartments, c);
}

#[test]
fn security_label_serializes_and_deserializes() {
    let c = comps(&["LEGAL"]);
    let label = SecurityLabel::new(2, c);
    let json = serde_json::to_string(&label).unwrap();
    let back: SecurityLabel = serde_json::from_str(&json).unwrap();
    assert_eq!(back.level, label.level);
    assert_eq!(back.compartments, label.compartments);
}

// --- Clearance ---

#[test]
fn clearance_default_is_level_zero_no_compartments() {
    let c = Clearance::default();
    assert_eq!(c.level, 0);
    assert!(c.compartments.is_empty());
}

#[test]
fn clearance_new_stores_fields() {
    let c = comps(&["FINANCE"]);
    let cl = Clearance::new(5, c.clone());
    assert_eq!(cl.level, 5);
    assert_eq!(cl.compartments, c);
}

#[test]
fn clearance_serializes_and_deserializes() {
    let c = comps(&["HR", "LEGAL"]);
    let cl = Clearance::new(4, c);
    let json = serde_json::to_string(&cl).unwrap();
    let back: Clearance = serde_json::from_str(&json).unwrap();
    assert_eq!(back.level, cl.level);
    assert_eq!(back.compartments, cl.compartments);
}

// --- Dominance ---

#[test]
fn dominates_level_and_superset_compartments() {
    let label = SecurityLabel::new(2, comps(&["FINANCE"]));
    let clearance = Clearance::new(3, comps(&["FINANCE", "HR"]));
    assert!(clearance.dominates(&label));
}

#[test]
fn dominates_exact_level_and_exact_compartments() {
    let c = comps(&["FINANCE"]);
    let label = SecurityLabel::new(2, c.clone());
    let clearance = Clearance::new(2, c);
    assert!(clearance.dominates(&label));
}

#[test]
fn does_not_dominate_insufficient_level() {
    let label = SecurityLabel::new(5, BTreeSet::new());
    let clearance = Clearance::new(4, BTreeSet::new());
    assert!(!clearance.dominates(&label));
}

#[test]
fn does_not_dominate_missing_compartment() {
    let label = SecurityLabel::new(1, comps(&["FINANCE", "LEGAL"]));
    let clearance = Clearance::new(10, comps(&["FINANCE"]));
    assert!(!clearance.dominates(&label));
}

#[test]
fn public_resource_dominated_by_any_clearance() {
    let label = SecurityLabel::default();
    let clearance = Clearance::new(0, BTreeSet::new());
    assert!(clearance.dominates(&label));
}

#[test]
fn user_with_no_compartments_cannot_access_compartmented_resource() {
    let label = SecurityLabel::new(0, comps(&["SECRET"]));
    let clearance = Clearance::new(100, BTreeSet::new());
    assert!(!clearance.dominates(&label));
}

#[test]
fn empty_compartment_label_accessible_to_clearance_with_compartments() {
    let label = SecurityLabel::new(1, BTreeSet::new());
    let clearance = Clearance::new(1, comps(&["FINANCE"]));
    assert!(clearance.dominates(&label));
}
