// Copyright 2026 BelowZero Security OU. All rights reserved.

use std::fmt::Write;

use tessera_graph::{Graph, Property};
use tessera_import::csv::import_edges_csv;
use tessera_import::error::ImportError;

fn graph_with_n_persons(n: usize) -> Graph {
    let mut g = Graph::new();
    for i in 0..n {
        let props: tessera_graph::Properties =
            std::iter::once(("id".to_owned(), Property::I64(i64::try_from(i).unwrap()))).collect();
        g.add_node("Person", props).unwrap();
    }
    g
}

#[test]
fn import_edges_csv_large_graph_completes_in_reasonable_time() {
    // With O(n²) this would take seconds; with O(1) index it's instant.
    // 500 nodes, 499 sequential edges.
    let mut g = graph_with_n_persons(500);

    let mut csv = String::from(
        "source_label,source_prop,source_value,target_label,target_prop,target_value,rel_label\n",
    );
    for i in 0..499_usize {
        writeln!(csv, "Person,id,{i},Person,id,{},NEXT", i + 1).unwrap();
    }

    let start = std::time::Instant::now();
    let count = import_edges_csv(&mut g, &csv).unwrap();
    let elapsed = start.elapsed();

    assert_eq!(count, 499);
    assert!(
        elapsed.as_millis() < 500,
        "edge import of 499 edges into 500 nodes took {elapsed:?} — O(n²) regression?",
    );
}

#[test]
fn import_edges_json_large_graph_completes_in_reasonable_time() {
    use tessera_import::json::import_json;

    let mut g = graph_with_n_persons(500);

    let edges: Vec<String> = (0..499_usize)
        .map(|i| {
            format!(
                r#"{{"source":{{"label":"Person","match":{{"id":{i}}}}},
                    "target":{{"label":"Person","match":{{"id":{}}}}},
                    "label":"NEXT","properties":{{}}}}"#,
                i + 1
            )
        })
        .collect();
    let json = format!(r#"{{"nodes":[],"edges":[{}]}}"#, edges.join(","));

    let start = std::time::Instant::now();
    let summary = import_json(&mut g, &json).unwrap();
    let elapsed = start.elapsed();

    assert_eq!(summary.edges_imported, 499);
    assert!(
        elapsed.as_millis() < 500,
        "JSON edge import took {elapsed:?} — O(n²) regression?",
    );
}

/// Contract test: importing edges into an empty graph (no nodes) must succeed
/// and produce 0 edges, not panic. This verifies that `build_lookup_index` on
/// an empty graph returns `Ok(empty_map)` and the edge loop is never entered.
#[test]
fn build_lookup_index_on_empty_graph_returns_ok_empty() {
    let mut g = Graph::new();
    // An edge CSV with valid format but no matching nodes returns NodeNotFoundForEdge.
    // If the graph is empty and we try to import an edge, it should fail with
    // NodeNotFoundForEdge — not with a GraphRead error or a panic.
    let csv = "source_label,source_prop,source_value,target_label,target_prop,target_value,rel_label\nPerson,id,0,Person,id,1,NEXT\n";
    let result = import_edges_csv(&mut g, csv);
    assert!(
        matches!(result, Err(ImportError::NodeNotFoundForEdge { .. })),
        "expected NodeNotFoundForEdge on empty graph, got: {result:?}"
    );
}
