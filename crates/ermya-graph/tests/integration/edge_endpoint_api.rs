// SPDX-License-Identifier: MIT

use ermya_graph::{Graph, props};

#[test]
fn read_edge_endpoints_returns_source_and_target() {
    let mut g = Graph::new();
    let a = g.add_node("N", props! {}).unwrap();
    let b = g.add_node("N", props! {}).unwrap();
    let eid = g.add_edge("E", a, b, props! {}).unwrap();
    let (src, tgt) = g.read_edge_endpoints(eid.as_u64()).unwrap();
    assert_eq!(src, a.as_u64());
    assert_eq!(tgt, b.as_u64());
}

#[test]
fn read_edge_label_hash_is_consistent() {
    let mut g = Graph::new();
    let a = g.add_node("N", props! {}).unwrap();
    let b = g.add_node("N", props! {}).unwrap();
    let e1 = g.add_edge("KNOWS", a, b, props! {}).unwrap();
    let e2 = g.add_edge("KNOWS", b, a, props! {}).unwrap();
    let h1 = g.read_edge_label_hash(e1.as_u64()).unwrap();
    let h2 = g.read_edge_label_hash(e2.as_u64()).unwrap();
    assert_eq!(h1, h2, "same label must produce same hash");
}

#[test]
fn read_edge_label_hash_differs_for_different_labels() {
    let mut g = Graph::new();
    let a = g.add_node("N", props! {}).unwrap();
    let b = g.add_node("N", props! {}).unwrap();
    let e1 = g.add_edge("KNOWS", a, b, props! {}).unwrap();
    let e2 = g.add_edge("LIKES", a, b, props! {}).unwrap();
    let h1 = g.read_edge_label_hash(e1.as_u64()).unwrap();
    let h2 = g.read_edge_label_hash(e2.as_u64()).unwrap();
    assert_ne!(h1, h2, "different labels should produce different hashes");
}

#[test]
fn read_edge_endpoints_invalid_id_returns_error() {
    let g = Graph::new();
    assert!(g.read_edge_endpoints(9999).is_err());
}

#[test]
fn read_edge_label_hash_invalid_id_returns_error() {
    let g = Graph::new();
    assert!(g.read_edge_label_hash(9999).is_err());
}
