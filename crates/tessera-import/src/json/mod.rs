// Copyright 2026 BelowZero Security OU. All rights reserved.

//! JSON import and export for `TesseraGraph`.

use std::collections::HashMap;

use tessera_graph::GraphAccess;

use crate::error::{ExportResult, ImportError, ImportResult};
use crate::node_lookup::{NodeLookupIndex, build_lookup_index, find_node_in_index};
use crate::property_coerce::{is_valid_property_key, json_value_to_property, property_to_json};

/// Summary returned after a successful JSON import.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportJsonSummary {
    /// Number of nodes inserted.
    pub nodes_imported: usize,
    /// Number of edges inserted.
    pub edges_imported: usize,
}

// ── Import ───────────────────────────────────────────────────────────────────

/// Import nodes and edges from a JSON string.
///
/// Expected format:
/// ```json
/// {
///   "nodes": [
///     { "label": "Person", "properties": { "name": "Alice", "age": 30 } }
///   ],
///   "edges": [
///     {
///       "source": { "label": "Person", "match": { "name": "Alice" } },
///       "target": { "label": "Person", "match": { "name": "Bob" } },
///       "label": "KNOWS",
///       "properties": {}
///     }
///   ]
/// }
/// ```
///
/// The `match` object in each edge endpoint must contain exactly one key.
///
/// # Errors
///
/// Returns [`ImportError::JsonInvalid`] if the JSON cannot be parsed or if a
/// `match` object has more than one key.
/// Returns [`ImportError::JsonMissingField`] if required fields are absent.
/// Returns [`ImportError::NodeNotFoundForEdge`] if an endpoint cannot be found.
/// Returns [`ImportError::GraphWrite`] if a graph insertion fails.
///
/// # LBAC Note
///
/// When `graph` is a `SecureGraph`, the lookup index only contains nodes
/// visible at the writer's clearance level. Nodes imported at a higher
/// clearance will be invisible to the index, causing
/// [`ImportError::NodeNotFoundForEdge`] — indistinguishable from a truly
/// absent node. Callers must import nodes and edges at the same clearance
/// level to avoid this.
pub fn import_json<G: GraphAccess>(graph: &mut G, json_text: &str) -> ImportResult<ImportJsonSummary> {
    let root: serde_json::Value =
        serde_json::from_str(json_text).map_err(|e| ImportError::JsonInvalid(e.to_string()))?;

    let obj = root
        .as_object()
        .ok_or_else(|| ImportError::JsonInvalid("root JSON value must be an object".to_owned()))?;

    // Import nodes.
    let nodes_arr = obj
        .get("nodes")
        .and_then(|v| v.as_array())
        .ok_or_else(|| ImportError::JsonMissingField("nodes".to_owned()))?;

    let mut nodes_imported = 0_usize;
    for node_val in nodes_arr {
        let node_obj = node_val.as_object().ok_or_else(|| {
            ImportError::JsonInvalid("each node entry must be a JSON object".to_owned())
        })?;

        let label = node_obj
            .get("label")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ImportError::JsonMissingField("nodes[].label".to_owned()))?
            .to_owned();

        let mut properties: tessera_graph::Properties = HashMap::new();
        if let Some(props_val) = node_obj.get("properties") {
            if let Some(props_obj) = props_val.as_object() {
                for (k, v) in props_obj {
                    if !is_valid_property_key(k) {
                        return Err(ImportError::InvalidPropertyKey(k.clone()));
                    }
                    properties.insert(k.clone(), json_value_to_property(v));
                }
            }
        }

        graph
            .add_node(&label, properties)
            .map_err(|e| ImportError::GraphWrite(e.to_string()))?;
        nodes_imported += 1;
    }

    // Import edges — build index once after all nodes are inserted.
    let edges_arr = obj
        .get("edges")
        .and_then(|v| v.as_array())
        .ok_or_else(|| ImportError::JsonMissingField("edges".to_owned()))?;

    let mut edges_imported = 0_usize;

    if !edges_arr.is_empty() {
        let index = build_lookup_index(graph)?;

        for edge_val in edges_arr {
            let edge_obj = edge_val.as_object().ok_or_else(|| {
                ImportError::JsonInvalid("each edge entry must be a JSON object".to_owned())
            })?;

            let rel_label = edge_obj
                .get("label")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ImportError::JsonMissingField("edges[].label".to_owned()))?
                .to_owned();

            let source_id = resolve_endpoint(&index, edge_obj, "source")?;
            let target_id = resolve_endpoint(&index, edge_obj, "target")?;

            let mut edge_props: tessera_graph::Properties = HashMap::new();
            if let Some(props_val) = edge_obj.get("properties") {
                if let Some(props_obj) = props_val.as_object() {
                    for (k, v) in props_obj {
                        if !is_valid_property_key(k) {
                            return Err(ImportError::InvalidPropertyKey(k.clone()));
                        }
                        edge_props.insert(k.clone(), json_value_to_property(v));
                    }
                }
            }

            graph
                .add_edge(&rel_label, source_id, target_id, edge_props)
                .map_err(|e| ImportError::GraphWrite(e.to_string()))?;
            edges_imported += 1;
        }
    }

    Ok(ImportJsonSummary {
        nodes_imported,
        edges_imported,
    })
}

/// Resolve a node endpoint (source or target) from an edge JSON object.
fn resolve_endpoint(
    index: &NodeLookupIndex,
    edge_obj: &serde_json::Map<String, serde_json::Value>,
    endpoint_key: &str,
) -> ImportResult<tessera_graph::NodeId> {
    let ep = edge_obj
        .get(endpoint_key)
        .and_then(|v| v.as_object())
        .ok_or_else(|| ImportError::JsonMissingField(format!("edges[].{endpoint_key}")))?;

    let label = ep
        .get("label")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ImportError::JsonMissingField(format!("edges[].{endpoint_key}.label")))?;

    let match_obj = ep
        .get("match")
        .and_then(|v| v.as_object())
        .ok_or_else(|| ImportError::JsonMissingField(format!("edges[].{endpoint_key}.match")))?;

    if match_obj.len() > 1 {
        return Err(ImportError::JsonInvalid(format!(
            "edges[].{endpoint_key}.match must have exactly 1 key, found {}",
            match_obj.len()
        )));
    }

    // Use the first (and only) key in the match object.
    let (prop_key, prop_val) = match_obj.iter().next().ok_or_else(|| {
        ImportError::JsonMissingField(format!("edges[].{endpoint_key}.match (empty)"))
    })?;

    let prop_value_str = prop_val.to_string();
    // JSON strings are serialized with quotes; strip them for string values.
    let prop_value_clean = prop_val.as_str().map_or(prop_value_str, str::to_owned);

    find_node_in_index(index, label, prop_key, &prop_value_clean).ok_or_else(|| {
        ImportError::NodeNotFoundForEdge {
            label: label.to_owned(),
            prop: prop_key.clone(),
            value: prop_value_clean,
        }
    })
}

// ── Export ───────────────────────────────────────────────────────────────────

/// Export all nodes and edges in the graph to a pretty-printed JSON string.
///
/// Output format:
/// ```json
/// {
///   "nodes": [
///     { "label": "...", "properties": { ... } }
///   ],
///   "edges": [
///     { "source_id": 1, "target_id": 2, "label": "...", "properties": { ... } }
///   ]
/// }
/// ```
///
/// # Errors
///
/// Returns [`crate::error::ExportError::GraphRead`] if graph data cannot be read.
/// Returns [`crate::error::ExportError::UnsupportedType`] if a property has
/// type `Bytes`.
/// Returns [`crate::error::ExportError::Serialize`] if JSON serialization fails.
pub fn export_json<G: GraphAccess>(graph: &G) -> ExportResult<String> {
    use crate::error::ExportError;

    // Build nodes array.
    let mut node_ids = graph.node_ids();
    node_ids.sort_unstable_by_key(|id| id.as_u64());

    let mut nodes_arr = Vec::new();
    for id in &node_ids {
        let node = graph
            .node(*id)
            .map_err(|e| ExportError::GraphRead(e.to_string()))?;

        let mut props_obj = serde_json::Map::new();
        let mut sorted_props: Vec<(&String, &tessera_graph::Property)> =
            node.properties().iter().collect();
        sorted_props.sort_by_key(|(k, _)| k.as_str());
        for (k, v) in sorted_props {
            props_obj.insert(k.clone(), property_to_json(v)?);
        }

        nodes_arr.push(serde_json::json!({
            "label": node.label(),
            "properties": serde_json::Value::Object(props_obj),
        }));
    }

    // Build edges array.
    let mut edges_arr = Vec::new();
    for id in &node_ids {
        let edges = graph
            .outgoing_edges(*id)
            .map_err(|e| ExportError::GraphRead(e.to_string()))?;
        let mut sorted_edges = edges;
        sorted_edges.sort_unstable_by_key(|e| e.id().as_u64());
        for edge in sorted_edges {
            let mut props_obj = serde_json::Map::new();
            let mut sorted_props: Vec<(&String, &tessera_graph::Property)> =
                edge.properties().iter().collect();
            sorted_props.sort_by_key(|(k, _)| k.as_str());
            for (k, v) in sorted_props {
                props_obj.insert(k.clone(), property_to_json(v)?);
            }
            edges_arr.push(serde_json::json!({
                "source_id": edge.source().as_u64(),
                "target_id": edge.target().as_u64(),
                "label": edge.label(),
                "properties": serde_json::Value::Object(props_obj),
            }));
        }
    }

    let root = serde_json::json!({
        "nodes": nodes_arr,
        "edges": edges_arr,
    });

    serde_json::to_string_pretty(&root).map_err(|e| ExportError::Serialize(e.to_string()))
}
