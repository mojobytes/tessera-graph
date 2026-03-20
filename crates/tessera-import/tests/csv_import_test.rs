// Copyright 2026 BelowZero Security OU. All rights reserved.

use tessera_graph::{Graph, Property};
use tessera_import::csv::{import_edges_csv, import_nodes_csv};
use tessera_import::error::ImportError;

fn empty_graph() -> Graph {
    Graph::new()
}

// ── Node import ──────────────────────────────────────────────────────────────

#[test]
fn import_single_node_no_props() {
    let mut g = empty_graph();
    let csv = "label\nPerson\n";
    let count = import_nodes_csv(&mut g, csv).unwrap();
    assert_eq!(count, 1);
    assert_eq!(g.node_count(), 1);
    let ids = g.nodes_by_label("Person");
    assert_eq!(ids.len(), 1);
}

#[test]
fn import_nodes_with_string_prop() {
    let mut g = empty_graph();
    let csv = "label,name\nPerson,Alice\nPerson,Bob\n";
    let count = import_nodes_csv(&mut g, csv).unwrap();
    assert_eq!(count, 2);
    assert_eq!(g.nodes_by_label("Person").len(), 2);
}

#[test]
fn import_nodes_coerces_integer_prop() {
    let mut g = empty_graph();
    let csv = "label,age\nPerson,30\n";
    import_nodes_csv(&mut g, csv).unwrap();
    let id = g.nodes_by_label("Person")[0];
    let node = g.node(id).unwrap();
    assert_eq!(node.properties().get("age"), Some(&Property::I64(30)));
}

#[test]
fn import_nodes_coerces_float_prop() {
    let mut g = empty_graph();
    let csv = "label,score\nPlayer,9.5\n";
    import_nodes_csv(&mut g, csv).unwrap();
    let id = g.nodes_by_label("Player")[0];
    let node = g.node(id).unwrap();
    assert_eq!(node.properties().get("score"), Some(&Property::F64(9.5)));
}

#[test]
fn import_nodes_coerces_bool_prop() {
    let mut g = empty_graph();
    let csv = "label,active\nUser,true\n";
    import_nodes_csv(&mut g, csv).unwrap();
    let id = g.nodes_by_label("User")[0];
    let node = g.node(id).unwrap();
    assert_eq!(node.properties().get("active"), Some(&Property::Bool(true)));
}

#[test]
fn import_nodes_skips_blank_lines() {
    let mut g = empty_graph();
    let csv = "label,name\n\nPerson,Alice\n\nPerson,Bob\n";
    let count = import_nodes_csv(&mut g, csv).unwrap();
    assert_eq!(count, 2);
}

#[test]
fn import_nodes_error_missing_label_column() {
    let mut g = empty_graph();
    let csv = "name,age\nAlice,30\n";
    let result = import_nodes_csv(&mut g, csv);
    assert!(matches!(result, Err(ImportError::CsvParse { .. })));
}

#[test]
fn import_nodes_error_empty_csv() {
    let mut g = empty_graph();
    let result = import_nodes_csv(&mut g, "");
    assert!(matches!(result, Err(ImportError::CsvParse { .. })));
}

#[test]
fn import_nodes_missing_optional_prop_column_is_skipped() {
    let mut g = empty_graph();
    // Row has fewer fields than header — extra props default to empty (skipped).
    let csv = "label,name,age\nPerson,Alice\n";
    let count = import_nodes_csv(&mut g, csv).unwrap();
    assert_eq!(count, 1);
    let id = g.nodes_by_label("Person")[0];
    let node = g.node(id).unwrap();
    assert!(node.properties().get("age").is_none());
    assert_eq!(
        node.properties().get("name"),
        Some(&Property::String("Alice".to_owned()))
    );
}

// ── Edge import ──────────────────────────────────────────────────────────────

fn graph_with_two_persons() -> Graph {
    let mut g = Graph::new();
    let props_alice: tessera_graph::Properties =
        std::iter::once(("name".to_owned(), Property::String("Alice".to_owned()))).collect();
    let props_bob: tessera_graph::Properties =
        std::iter::once(("name".to_owned(), Property::String("Bob".to_owned()))).collect();
    g.add_node("Person", props_alice).unwrap();
    g.add_node("Person", props_bob).unwrap();
    g
}

#[test]
fn import_edge_basic() {
    let mut g = graph_with_two_persons();
    let csv = "source_label,source_prop,source_value,target_label,target_prop,target_value,rel_label\n\
         Person,name,Alice,Person,name,Bob,KNOWS\n";
    let count = import_edges_csv(&mut g, csv).unwrap();
    assert_eq!(count, 1);
    assert_eq!(g.edge_count(), 1);
}

#[test]
fn import_edge_with_properties() {
    let mut g = graph_with_two_persons();
    let csv = "source_label,source_prop,source_value,target_label,target_prop,target_value,rel_label,since\n\
         Person,name,Alice,Person,name,Bob,KNOWS,2020\n";
    import_edges_csv(&mut g, csv).unwrap();
    let ids = g.edges_by_label("KNOWS");
    assert_eq!(ids.len(), 1);
    let edge = g.edge(ids[0]).unwrap();
    assert_eq!(edge.properties().get("since"), Some(&Property::I64(2020)));
}

#[test]
fn import_edge_error_node_not_found() {
    let mut g = empty_graph();
    let csv = "source_label,source_prop,source_value,target_label,target_prop,target_value,rel_label\n\
         Person,name,Alice,Person,name,Bob,KNOWS\n";
    let result = import_edges_csv(&mut g, csv);
    assert!(matches!(
        result,
        Err(ImportError::NodeNotFoundForEdge { .. })
    ));
}

#[test]
fn import_edge_error_too_few_header_columns() {
    let mut g = empty_graph();
    let csv = "source_label,source_prop\n";
    let result = import_edges_csv(&mut g, csv);
    assert!(matches!(result, Err(ImportError::CsvParse { .. })));
}

#[test]
fn import_nodes_csv_empty_label_returns_error() {
    let mut g = empty_graph();
    let csv = "label,name\n,Alice\n";
    let result = import_nodes_csv(&mut g, csv);
    assert!(
        matches!(result, Err(ImportError::CsvParse { row: 2, .. })),
        "expected CsvParse error for empty label, got: {result:?}"
    );
}

#[test]
fn import_nodes_csv_whitespace_only_label_returns_error() {
    let mut g = empty_graph();
    let csv = "label,name\n   ,Bob\n";
    let result = import_nodes_csv(&mut g, csv);
    assert!(
        matches!(result, Err(ImportError::CsvParse { row: 2, .. })),
        "expected CsvParse error for whitespace-only label, got: {result:?}"
    );
}

#[test]
fn coerce_str_value_nan_is_string_not_float() {
    // "NaN" parses as f64::NAN via str::parse — we must NOT silently store it
    // as Property::F64(NaN). After the fix it should become Property::String("NaN").
    let g = {
        let mut g = tessera_graph::Graph::new();
        tessera_import::csv::import_nodes_csv(&mut g, "label,val\nThing,NaN\n").unwrap();
        g
    };
    let id = g.nodes_by_label("Thing")[0];
    let node = g.node(id).unwrap();
    assert_eq!(
        node.properties().get("val"),
        Some(&Property::String("NaN".to_owned())),
        "NaN must not be silently stored as f64"
    );
}

#[test]
fn coerce_str_value_inf_is_string_not_float() {
    let mut g = tessera_graph::Graph::new();
    tessera_import::csv::import_nodes_csv(&mut g, "label,val\nThing,inf\n").unwrap();
    let id = g.nodes_by_label("Thing")[0];
    let node = g.node(id).unwrap();
    assert_eq!(
        node.properties().get("val"),
        Some(&Property::String("inf".to_owned())),
    );
}

#[test]
fn import_nodes_csv_invalid_property_key_in_header_returns_error() {
    let mut g = empty_graph();
    // Header key "has space" is invalid — must be rejected before any graph write.
    let csv = "label,has space\nPerson,Alice\n";
    let result = import_nodes_csv(&mut g, csv);
    assert!(
        matches!(result, Err(ImportError::InvalidPropertyKey(_))),
        "expected InvalidPropertyKey for header with space, got: {result:?}"
    );
    // No partial writes should have occurred.
    assert_eq!(g.node_count(), 0);
}

#[test]
fn import_nodes_csv_invalid_property_key_digit_prefix_returns_error() {
    let mut g = empty_graph();
    let csv = "label,1bad_key\nPerson,Alice\n";
    let result = import_nodes_csv(&mut g, csv);
    assert!(
        matches!(result, Err(ImportError::InvalidPropertyKey(_))),
        "expected InvalidPropertyKey for key starting with digit, got: {result:?}"
    );
}

#[test]
fn import_nodes_csv_unclosed_quote_returns_error() {
    let mut g = empty_graph();
    // The name field opens a quote that is never closed.
    let csv = "label,name\nPerson,\"Alice\n";
    let result = import_nodes_csv(&mut g, csv);
    assert!(
        matches!(result, Err(ImportError::CsvParse { row: 2, .. })),
        "expected CsvParse(row=2) for unclosed quote, got: {result:?}"
    );
    if let Err(ImportError::CsvParse { reason, .. }) = &result {
        assert!(
            reason.contains("unclosed"),
            "reason should mention 'unclosed', got: {reason}"
        );
    }
}
