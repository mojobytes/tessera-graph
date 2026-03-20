// Copyright 2026 BelowZero Security OU. All rights reserved.

use tessera_graph::{Graph, NodeId, Property};

/// Find a node by label and a single property match.
/// Returns the first matching node ID, or `None`.
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
