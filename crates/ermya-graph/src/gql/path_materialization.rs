// SPDX-License-Identifier: MIT

//! Builds [`GqlPath`] values from matched fixed-segment patterns.
//!
//! [`materialise_fixed_path`] turns already-bound node/edge variables from a
//! fixed-segment `MATCH p = (a)-[r1]->(b)-[r2]->(c)` into a path, upholding the
//! Neo4j invariant `nodes.len() == rels.len() + 1`.
//!
//! Variable-length path materialisation (`MATCH p = (a)-[*1..N]->(b)`) does NOT
//! live here: it needs the constraint-matching helpers (`edge_matches_pattern`,
//! `get_edges_for_direction`, …) that are private to `compiler.rs`, so it is
//! implemented there as `materialise_varlen_path` to avoid duplicating that
//! logic. See `compiler.rs::materialise_path_for_match`.

use super::compiler::{GqlPath, gql_node_from_entity, gql_relationship_from_entity};
use crate::query::pattern::PatternMatch;

/// Builds a [`GqlPath`] from already-bound node/edge variables, in traversal
/// order. Returns `None` if the `nodes.len() == edges.len() + 1` invariant is
/// violated or any named variable is unbound.
pub fn materialise_fixed_path(
    pm: &PatternMatch,
    node_vars: &[&str],
    edge_vars: &[&str],
) -> Option<GqlPath> {
    if node_vars.len() != edge_vars.len() + 1 {
        return None;
    }
    let nodes = node_vars
        .iter()
        .map(|v| pm.get_node(v).ok().map(gql_node_from_entity))
        .collect::<Option<_>>()?;
    let rels = edge_vars
        .iter()
        .map(|v| pm.get_edge(v).ok().map(gql_relationship_from_entity))
        .collect::<Option<_>>()?;
    Some(GqlPath { nodes, rels })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::pattern::PatternMatch;
    use crate::{Graph, gql::compiler::GqlValue, props};
    use std::collections::HashMap;

    /// Graph `(a)-[:R {w:1}]->(b)-[:R {w:2}]->(c)`, returned as a
    /// `PatternMatch` binding `a,b,c` and `r1,r2`, exactly as the executor
    /// would bind a fixed-segment `MATCH p = (a)-[r1]->(b)-[r2]->(c)`.
    fn build_three_node_pattern_match() -> PatternMatch {
        let mut g = Graph::new();
        let a = g.add_node("N", props! { "name" => "a" }).unwrap();
        let b = g.add_node("N", props! { "name" => "b" }).unwrap();
        let c = g.add_node("N", props! { "name" => "c" }).unwrap();
        let r1 = g.add_edge("R", a, b, props! { "w" => 1_i64 }).unwrap();
        let r2 = g.add_edge("R", b, c, props! { "w" => 2_i64 }).unwrap();

        let mut nodes = HashMap::new();
        nodes.insert("a".to_owned(), g.node(a).unwrap());
        nodes.insert("b".to_owned(), g.node(b).unwrap());
        nodes.insert("c".to_owned(), g.node(c).unwrap());
        let mut edges = HashMap::new();
        edges.insert("r1".to_owned(), g.edge(r1).unwrap());
        edges.insert("r2".to_owned(), g.edge(r2).unwrap());
        PatternMatch::new(nodes, edges)
    }

    #[test]
    fn fixed_two_segment_path_materialises_nodes_and_edges() {
        let pm = build_three_node_pattern_match();
        let path = materialise_fixed_path(&pm, &["a", "b", "c"], &["r1", "r2"])
            .expect("a,b,c,r1,r2 are all bound with a valid 3-node/2-edge shape");
        assert_eq!(path.nodes.len(), 3, "three nodes a,b,c");
        assert_eq!(path.rels.len(), 2, "two edges r1,r2");
        assert_eq!(
            path.nodes.len(),
            path.rels.len() + 1,
            "Neo4j path invariant"
        );
        assert_eq!(
            path.rels[1].props.get("w"),
            Some(&GqlValue::Int(2)),
            "second edge (r2) preserves its w=2 property",
        );
    }

    #[test]
    fn unbound_variable_yields_none() {
        let pm = build_three_node_pattern_match();
        assert!(materialise_fixed_path(&pm, &["a", "missing", "c"], &["r1", "r2"]).is_none());
    }

    #[test]
    fn invariant_violation_yields_none() {
        let pm = build_three_node_pattern_match();
        // 3 nodes, 1 edge → 3 != 1 + 1 → rejected.
        assert!(materialise_fixed_path(&pm, &["a", "b", "c"], &["r1"]).is_none());
    }
}
