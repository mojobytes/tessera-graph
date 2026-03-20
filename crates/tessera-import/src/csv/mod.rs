// Copyright 2026 BelowZero Security OU. All rights reserved.

//! CSV import and export for `TesseraGraph`.
//!
//! Uses a manual CSV parser — no external crate required.
//!
//! ## Edge CSV Format Note
//!
//! Edge export and import use different formats by design:
//! - **Export** (`export_edges_csv`): `source_id,target_id,rel_label,...` — uses internal IDs
//!   for compactness; suitable for inspection and backup.
//! - **Import** (`import_edges_csv`): `source_label,source_prop,source_value,...` — uses
//!   property-based node matching; suitable for loading data from external sources.
//!
//! Round-trip (export then re-import) is not supported for edges. If you need round-trip
//! capability, use JSON format which uses the same property-match convention for both.

use std::collections::HashMap;

use tessera_graph::{Graph, Property};

use crate::error::{ExportResult, ImportError, ImportResult};
use crate::node_lookup::{build_lookup_index, find_node_in_index};
use crate::property_coerce::{coerce_str_value, property_to_json};

// ── CSV parsing helpers ──────────────────────────────────────────────────────

/// Parse a single CSV line into fields, respecting double-quoted fields.
fn parse_csv_line(line: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut chars = line.chars().peekable();

    while let Some(ch) = chars.next() {
        match ch {
            '"' if in_quotes => {
                // Peek to see if this is an escaped quote `""`
                if chars.peek() == Some(&'"') {
                    chars.next();
                    current.push('"');
                } else {
                    in_quotes = false;
                }
            }
            '"' => {
                in_quotes = true;
            }
            ',' if !in_quotes => {
                fields.push(current.clone());
                current.clear();
            }
            other => {
                current.push(other);
            }
        }
    }
    fields.push(current);
    fields
}

/// Quote a CSV field value if it contains commas, double-quotes, or newlines.
fn quote_csv_field(value: &str) -> String {
    if value.contains(',') || value.contains('"') || value.contains('\n') {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_owned()
    }
}

// ── Node import ──────────────────────────────────────────────────────────────

/// Import nodes from a CSV string into the graph.
///
/// The first line must be the header row starting with `label`. Each subsequent
/// non-blank line is a data row where the first column is the node label and
/// remaining columns are property key-value pairs.
///
/// Returns the number of nodes imported.
///
/// # Errors
///
/// Returns [`ImportError::CsvParse`] if the header is missing, the `label`
/// column is absent, a data row has fewer columns than the header, or the label
/// column is empty or whitespace-only.
/// Returns [`ImportError::CsvParse`] (with row context) if inserting a node
/// into the graph fails.
pub fn import_nodes_csv(graph: &mut Graph, csv: &str) -> ImportResult<usize> {
    let mut lines = csv.lines();

    // Parse header.
    let header_line = lines.next().ok_or_else(|| ImportError::CsvParse {
        row: 0,
        reason: "empty CSV input".to_owned(),
    })?;
    let headers = parse_csv_line(header_line);
    if headers.is_empty() || headers[0].trim() != "label" {
        return Err(ImportError::CsvParse {
            row: 0,
            reason: "first column of header must be 'label'".to_owned(),
        });
    }

    let prop_keys: Vec<&str> = headers[1..].iter().map(|s| s.trim()).collect();
    let mut count = 0_usize;

    for (row_idx, line) in lines.enumerate() {
        let row_num = row_idx + 2; // 1-based, header is row 1
        if line.trim().is_empty() {
            continue;
        }

        let fields = parse_csv_line(line);
        if fields.is_empty() {
            continue;
        }

        let label = fields[0].trim().to_owned();
        if label.is_empty() {
            return Err(ImportError::CsvParse {
                row: row_num,
                reason: "label column must not be empty".to_owned(),
            });
        }

        let mut properties: tessera_graph::Properties = HashMap::new();

        for (i, key) in prop_keys.iter().enumerate() {
            let value = fields.get(i + 1).map_or("", |s| s.trim());
            if !value.is_empty() {
                properties.insert((*key).to_owned(), coerce_str_value(value));
            }
        }

        graph
            .add_node(label, properties)
            .map_err(|e| ImportError::CsvParse {
                row: row_num,
                reason: format!("graph write failed: {e}"),
            })?;
        count += 1;
    }

    Ok(count)
}

// ── Edge import ──────────────────────────────────────────────────────────────

/// Import edges from a CSV string into the graph.
///
/// Header format:
/// `source_label,source_prop,source_value,target_label,target_prop,target_value,rel_label[,prop1,...]`
///
/// The first 7 columns are required. Additional columns are edge properties.
///
/// Returns the number of edges imported.
///
/// # Errors
///
/// Returns [`ImportError::CsvParse`] if the header has fewer than 7 columns or
/// a data row cannot be parsed. Returns [`ImportError::NodeNotFoundForEdge`] if
/// an endpoint node cannot be located. Returns [`ImportError::CsvParse`] (with
/// row context) if inserting an edge fails.
pub fn import_edges_csv(graph: &mut Graph, csv: &str) -> ImportResult<usize> {
    let mut lines = csv.lines();

    let header_line = lines.next().ok_or_else(|| ImportError::CsvParse {
        row: 0,
        reason: "empty CSV input".to_owned(),
    })?;
    let headers = parse_csv_line(header_line);
    if headers.len() < 7 {
        return Err(ImportError::CsvParse {
            row: 0,
            reason: format!(
                "edge CSV header must have at least 7 columns, got {}",
                headers.len()
            ),
        });
    }

    // Property keys start at column index 7.
    let edge_prop_keys: Vec<&str> = headers[7..].iter().map(|s| s.trim()).collect();
    let mut count = 0_usize;

    // Build lookup index once before the loop — O(N) build, O(1) lookups per edge.
    let index = build_lookup_index(graph);

    for (row_idx, line) in lines.enumerate() {
        let row_num = row_idx + 2;
        if line.trim().is_empty() {
            continue;
        }

        let fields = parse_csv_line(line);
        if fields.len() < 7 {
            return Err(ImportError::CsvParse {
                row: row_num,
                reason: format!("expected at least 7 fields, got {}", fields.len()),
            });
        }

        let source_label = fields[0].trim();
        let source_prop = fields[1].trim();
        let source_value = fields[2].trim();
        let target_label = fields[3].trim();
        let target_prop = fields[4].trim();
        let target_value = fields[5].trim();
        let rel_label = fields[6].trim().to_owned();

        let source_id = find_node_in_index(&index, source_label, source_prop, source_value)
            .ok_or_else(|| ImportError::NodeNotFoundForEdge {
                label: source_label.to_owned(),
                prop: source_prop.to_owned(),
                value: source_value.to_owned(),
            })?;

        let target_id = find_node_in_index(&index, target_label, target_prop, target_value)
            .ok_or_else(|| ImportError::NodeNotFoundForEdge {
                label: target_label.to_owned(),
                prop: target_prop.to_owned(),
                value: target_value.to_owned(),
            })?;

        let mut edge_props: tessera_graph::Properties = HashMap::new();
        for (i, key) in edge_prop_keys.iter().enumerate() {
            let value = fields.get(i + 7).map_or("", |s| s.trim());
            if !value.is_empty() {
                edge_props.insert((*key).to_owned(), coerce_str_value(value));
            }
        }

        graph
            .add_edge(rel_label, source_id, target_id, edge_props)
            .map_err(|e| ImportError::CsvParse {
                row: row_num,
                reason: format!("graph write: {e}"),
            })?;
        count += 1;
    }

    Ok(count)
}

// ── Node export ──────────────────────────────────────────────────────────────

/// Export all nodes in the graph to a CSV string.
///
/// The header row is `label,key1,key2,...` where property keys are the sorted
/// union of all property keys across all nodes. Values containing commas or
/// double-quotes are quoted.
///
/// # Errors
///
/// Returns [`crate::error::ExportError::GraphRead`] if a node cannot be read.
/// Returns [`crate::error::ExportError::UnsupportedType`] if a node property
/// has type `Bytes`.
pub fn export_nodes_csv(graph: &Graph) -> ExportResult<String> {
    use crate::error::ExportError;

    // Collect all property keys — union across all nodes, sorted.
    let all_keys: Vec<String> = {
        let mut key_set = std::collections::BTreeSet::new();
        for id in graph.node_ids() {
            let node = graph
                .node(id)
                .map_err(|e| ExportError::GraphRead(e.to_string()))?;
            for k in node.properties().keys() {
                key_set.insert(k.clone());
            }
        }
        key_set.into_iter().collect()
    };

    let node_count = graph.node_ids().len();
    let mut out = String::with_capacity(30 * (node_count + 1));

    // Header row.
    out.push_str("label");
    for k in &all_keys {
        out.push(',');
        out.push_str(&quote_csv_field(k));
    }
    out.push('\n');

    // Data rows — deterministic order via sorted node IDs.
    let mut node_ids = graph.node_ids();
    node_ids.sort_unstable_by_key(|id| id.as_u64());

    for id in node_ids {
        let node = graph
            .node(id)
            .map_err(|e| ExportError::GraphRead(e.to_string()))?;
        out.push_str(&quote_csv_field(node.label()));
        for k in &all_keys {
            out.push(',');
            if let Some(prop) = node.properties().get(k) {
                // Bytes is not supported in CSV export.
                if matches!(prop, Property::Bytes(_)) {
                    return Err(ExportError::UnsupportedType {
                        context: "csv export".to_owned(),
                        type_name: "Bytes".to_owned(),
                    });
                }
                let v = match prop {
                    Property::String(s) => quote_csv_field(s),
                    other => {
                        let jv = property_to_json(other)?;
                        quote_csv_field(&jv.to_string())
                    }
                };
                out.push_str(&v);
            }
        }
        out.push('\n');
    }

    Ok(out)
}

// ── Edge export ──────────────────────────────────────────────────────────────

/// Export all edges in the graph to a CSV string.
///
/// Header: `source_id,target_id,rel_label,key1,...` where property keys are
/// the sorted union of all edge property keys.
///
/// # Errors
///
/// Returns [`crate::error::ExportError::GraphRead`] if an edge cannot be read.
/// Returns [`crate::error::ExportError::UnsupportedType`] if an edge property
/// has type `Bytes`.
pub fn export_edges_csv(graph: &Graph) -> ExportResult<String> {
    use crate::error::ExportError;

    // Collect all edge property keys.
    let mut node_ids = graph.node_ids();
    node_ids.sort_unstable_by_key(|id| id.as_u64());

    let mut all_edges = Vec::new();
    for id in &node_ids {
        let edges = graph
            .outgoing_edges(*id)
            .map_err(|e| ExportError::GraphRead(e.to_string()))?;
        all_edges.extend(edges);
    }

    let all_keys: Vec<String> = {
        let mut key_set = std::collections::BTreeSet::new();
        for edge in &all_edges {
            for k in edge.properties().keys() {
                key_set.insert(k.clone());
            }
        }
        key_set.into_iter().collect()
    };

    let mut out = String::with_capacity(40 * (all_edges.len() + 1));

    // Header row.
    out.push_str("source_id,target_id,rel_label");
    for k in &all_keys {
        out.push(',');
        out.push_str(&quote_csv_field(k));
    }
    out.push('\n');

    for edge in &all_edges {
        out.push_str(&edge.source().as_u64().to_string());
        out.push(',');
        out.push_str(&edge.target().as_u64().to_string());
        out.push(',');
        out.push_str(&quote_csv_field(edge.label()));
        for k in &all_keys {
            out.push(',');
            if let Some(prop) = edge.properties().get(k) {
                // Bytes is not supported in CSV export.
                if matches!(prop, Property::Bytes(_)) {
                    return Err(ExportError::UnsupportedType {
                        context: "csv export".to_owned(),
                        type_name: "Bytes".to_owned(),
                    });
                }
                let v = match prop {
                    Property::String(s) => quote_csv_field(s),
                    other => {
                        let jv = property_to_json(other)?;
                        quote_csv_field(&jv.to_string())
                    }
                };
                out.push_str(&v);
            }
        }
        out.push('\n');
    }

    Ok(out)
}
