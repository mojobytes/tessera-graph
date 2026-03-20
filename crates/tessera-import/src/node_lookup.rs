// Copyright 2026 BelowZero Security OU. All rights reserved.

use std::collections::HashMap;

use tessera_graph::{Graph, NodeId, Property};

use crate::error::{ImportError, ImportResult};

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

/// Composite string key for the lookup index.
///
/// Uses null-byte `\0` as a separator because validated label and property-key
/// strings cannot contain null bytes (enforced by `is_valid_property_key`).
/// This reduces each lookup from 3 heap allocations (tuple of three `String`s)
/// to 1 (a single formatted `String`).
///
/// # TODO(perf)
/// A zero-allocation path is possible by implementing a custom `Hash + Eq`
/// wrapper over `(&str, &str, &str)` and using `HashMap::get` with it via the
/// `Borrow` trait. Left as a future optimisation — the single-allocation path
/// is already a 2× improvement over the original tuple.
#[inline]
fn lookup_key(label: &str, prop_key: &str, prop_value: &str) -> String {
    format!("{label}\0{prop_key}\0{prop_value}")
}

/// Key: composite `"{label}\0{prop_key}\0{prop_value}"` string.
pub type NodeLookupIndex = HashMap<String, NodeId>;

/// Build a full lookup index from all nodes currently in the graph.
///
/// After building, edge import uses O(1) lookups instead of O(N) scans per
/// edge. Build once before the edge import loop; pass `&index` into
/// [`find_node_in_index`] for each edge.
///
/// # Errors
///
/// Returns [`ImportError::GraphRead`] if any node cannot be read from the
/// graph. Previously such errors were silently swallowed, producing an
/// incomplete index and ghost `NodeNotFoundForEdge` errors downstream.
pub fn build_lookup_index(graph: &Graph) -> ImportResult<NodeLookupIndex> {
    let mut index = NodeLookupIndex::new();
    for id in graph.node_ids() {
        let node = graph
            .node(id)
            .map_err(|e| ImportError::GraphRead(e.to_string()))?;
        let label = node.label().to_owned();
        for (prop_key, prop_val) in node.properties() {
            let value_str = match prop_val {
                Property::String(s) => s.clone(),
                other => other.to_string(),
            };
            index.insert(lookup_key(&label, prop_key, &value_str), id);
        }
    }
    Ok(index)
}

/// O(1) lookup using a pre-built index.
pub fn find_node_in_index(
    index: &NodeLookupIndex,
    label: &str,
    prop_key: &str,
    prop_value: &str,
) -> Option<NodeId> {
    index.get(&lookup_key(label, prop_key, prop_value)).copied()
}
