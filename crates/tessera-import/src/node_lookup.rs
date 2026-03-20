// Copyright 2026 BelowZero Security OU. All rights reserved.

use std::collections::HashMap;

use tessera_graph::{Graph, NodeId, Property};

/// Find a node by label and a single property match.
/// Returns the first matching node ID, or `None`.
///
/// Kept for callers that do not benefit from a pre-built index.
#[allow(dead_code)]
pub fn find_node_by_label_and_prop(
    graph: &Graph,
    label: &str,
    prop_key: &str,
    prop_value: &str,
) -> Option<NodeId> {
    let candidates = graph.nodes_by_label(label);
    for id in candidates {
        if let Ok(node) = graph.node(id) {
            if let Some(p) = node.properties().get(prop_key) {
                let matches = match p {
                    Property::String(s) => s == prop_value,
                    other => other.to_string() == prop_value,
                };
                if matches {
                    return Some(id);
                }
            }
        }
    }
    None
}

/// Key: `(label, prop_key, prop_value_as_string)`
pub type NodeLookupIndex = HashMap<(String, String, String), NodeId>;

/// Build a full lookup index from all nodes currently in the graph.
///
/// After building, edge import uses O(1) lookups instead of O(N) scans per
/// edge. Build once before the edge import loop; pass `&index` into
/// [`find_node_in_index`] for each edge.
pub fn build_lookup_index(graph: &Graph) -> NodeLookupIndex {
    let mut index = NodeLookupIndex::new();
    for id in graph.node_ids() {
        if let Ok(node) = graph.node(id) {
            let label = node.label().to_owned();
            for (prop_key, prop_val) in node.properties() {
                let value_str = match prop_val {
                    Property::String(s) => s.clone(),
                    other => other.to_string(),
                };
                index.insert((label.clone(), prop_key.clone(), value_str), id);
            }
        }
    }
    index
}

/// O(1) lookup using a pre-built index.
pub fn find_node_in_index(
    index: &NodeLookupIndex,
    label: &str,
    prop_key: &str,
    prop_value: &str,
) -> Option<NodeId> {
    index
        .get(&(label.to_owned(), prop_key.to_owned(), prop_value.to_owned()))
        .copied()
}
