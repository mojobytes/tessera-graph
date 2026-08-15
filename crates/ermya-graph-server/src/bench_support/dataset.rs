// SPDX-License-Identifier: BSL-1.1

//! Deterministic dataset builder for the lock-contention benchmark.
//!
//! Every point in the matrix starts from a graph built here: `n` nodes labelled
//! [`BENCH_NODE_LABEL`], each carrying a sequential `idx` property, wired into a
//! ring by `:NEXT` edges. No randomness — two independent builds of the same
//! size produce identical content, which is what makes the benchmark repeatable.

use ermya_graph::{Graph, Properties, Property};

/// Label applied to every node the builder creates. The MATCH scenarios bind
/// against this label, so it is shared between the builder and the queries.
pub const BENCH_NODE_LABEL: &str = "BenchNode";

/// Relationship type used for the ring edges.
pub const BENCH_EDGE_LABEL: &str = "NEXT";

/// Builds a deterministic ring graph of `n` nodes into `graph`.
///
/// Each node `i` (0-based) is labelled [`BENCH_NODE_LABEL`] with an `idx`
/// property equal to `i`. Nodes are wired `i -> (i + 1) % n` via
/// [`BENCH_EDGE_LABEL`] edges, so there are exactly `n` edges for `n >= 2`.
/// `n == 0` produces an empty graph; `n == 1` produces one node and one
/// self-loop edge (`0 -> 0`), keeping the "edge count == node count" invariant
/// uniform.
///
/// # Errors
/// Propagates any engine error from `add_node`/`add_edge` as a `String`.
pub fn build_dataset(graph: &mut Graph, n: u32) -> Result<(), String> {
    if n == 0 {
        return Ok(());
    }
    let mut ids = Vec::with_capacity(n as usize);
    for i in 0..n {
        let mut props: Properties = Properties::new();
        props.insert("idx".to_owned(), Property::I64(i64::from(i)));
        let id = graph
            .add_node(BENCH_NODE_LABEL, props)
            .map_err(|e| e.to_string())?;
        ids.push(id);
    }
    for i in 0..n as usize {
        let src = ids[i];
        let tgt = ids[(i + 1) % n as usize];
        graph
            .add_edge(BENCH_EDGE_LABEL, src, tgt, Properties::new())
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ermya_graph::Graph;

    #[test]
    fn build_dataset_creates_exact_node_count() {
        let mut g = Graph::new();
        build_dataset(&mut g, 500).unwrap();
        assert_eq!(g.node_count(), 500);
    }

    #[test]
    fn build_dataset_creates_ring_edges_count_equals_node_count() {
        let mut g = Graph::new();
        build_dataset(&mut g, 500).unwrap();
        assert_eq!(g.edge_count(), 500);
    }

    #[test]
    fn build_dataset_is_deterministic_across_two_independent_builds() {
        let mut a = Graph::new();
        let mut b = Graph::new();
        build_dataset(&mut a, 200).unwrap();
        build_dataset(&mut b, 200).unwrap();
        assert_eq!(a.node_count(), b.node_count());
        assert_eq!(a.edge_count(), b.edge_count());
        // Same label population in both.
        assert_eq!(
            a.nodes_by_label(BENCH_NODE_LABEL).len(),
            b.nodes_by_label(BENCH_NODE_LABEL).len(),
        );
    }

    #[test]
    fn build_dataset_zero_size_produces_empty_graph() {
        let mut g = Graph::new();
        build_dataset(&mut g, 0).unwrap();
        assert_eq!(g.node_count(), 0);
        assert_eq!(g.edge_count(), 0);
    }

    #[test]
    fn build_dataset_assigns_label_used_by_match_scenarios() {
        let mut g = Graph::new();
        build_dataset(&mut g, 300).unwrap();
        assert_eq!(g.nodes_by_label(BENCH_NODE_LABEL).len(), 300);
    }
}
