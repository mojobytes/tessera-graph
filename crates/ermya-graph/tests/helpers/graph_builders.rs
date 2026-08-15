// SPDX-License-Identifier: MIT

//! Shared graph construction helpers for integration tests.

use ermya_graph::{Graph, NodeId, props};

/// Builds a linear chain of `count` nodes with the given labels.
/// Returns the [`NodeId`] of the first node in the chain.
pub fn build_chain(graph: &mut Graph, label: &str, edge_label: &str, count: usize) -> NodeId {
    let first = graph.add_node(label, props! {}).unwrap();
    let mut prev = first;
    for _ in 1..count {
        let next = graph.add_node(label, props! {}).unwrap();
        graph.add_edge(edge_label, prev, next, props! {}).unwrap();
        prev = next;
    }
    first
}
