// Copyright 2026 BelowZero Security OU. All rights reserved.

use tessera_graph::{Graph, Property};
use tessera_import::csv::{export_nodes_csv, import_nodes_csv};
use tessera_import::gql_export::export_gql;
use tessera_import::gql_import::import_gql;
use tessera_import::json::{export_json, import_json};

fn alice_bob_graph() -> Graph {
    let mut g = Graph::new();
    let props_alice: tessera_graph::Properties = [
        ("name".to_owned(), Property::String("Alice".to_owned())),
        ("age".to_owned(), Property::I64(30)),
        ("active".to_owned(), Property::Bool(true)),
    ]
    .into_iter()
    .collect();
    let props_bob: tessera_graph::Properties = [
        ("name".to_owned(), Property::String("Bob".to_owned())),
        ("age".to_owned(), Property::I64(25)),
        ("score".to_owned(), Property::F64(9.5)),
    ]
    .into_iter()
    .collect();
    g.add_node("Person", props_alice).unwrap();
    g.add_node("Person", props_bob).unwrap();
    g
}

#[test]
fn csv_node_round_trip_preserves_count_and_labels() {
    let original = alice_bob_graph();
    let csv = export_nodes_csv(&original).unwrap();

    let mut restored = Graph::new();
    let count = import_nodes_csv(&mut restored, &csv).unwrap();

    assert_eq!(count, 2);
    assert_eq!(restored.node_count(), 2);
    assert_eq!(restored.nodes_by_label("Person").len(), 2);
}

#[test]
fn csv_node_round_trip_preserves_integer_property() {
    let original = alice_bob_graph();
    let csv = export_nodes_csv(&original).unwrap();

    let mut restored = Graph::new();
    import_nodes_csv(&mut restored, &csv).unwrap();

    let alice_id = restored.nodes_by_label("Person").into_iter().find(|&id| {
        restored
            .node(id)
            .ok()
            .and_then(|n| n.properties().get("name").cloned())
            == Some(Property::String("Alice".to_owned()))
    });
    let alice_id = alice_id.expect("Alice not found after round-trip");
    let node = restored.node(alice_id).unwrap();
    assert_eq!(node.properties().get("age"), Some(&Property::I64(30)));
    assert_eq!(node.properties().get("active"), Some(&Property::Bool(true)));
}

#[test]
fn csv_node_round_trip_string_with_comma() {
    let mut original = Graph::new();
    let props: tessera_graph::Properties = std::iter::once((
        "desc".to_owned(),
        Property::String("hello, world".to_owned()),
    ))
    .collect();
    original.add_node("Thing", props).unwrap();

    let csv = export_nodes_csv(&original).unwrap();
    let mut restored = Graph::new();
    import_nodes_csv(&mut restored, &csv).unwrap();

    let id = restored.nodes_by_label("Thing")[0];
    let node = restored.node(id).unwrap();
    assert_eq!(
        node.properties().get("desc"),
        Some(&Property::String("hello, world".to_owned()))
    );
}

#[test]
fn gql_node_round_trip_preserves_string_with_apostrophe() {
    let mut original = Graph::new();
    let props: tessera_graph::Properties =
        std::iter::once(("name".to_owned(), Property::String("O'Brien".to_owned()))).collect();
    original.add_node("Person", props).unwrap();

    let gql = export_gql(&original).unwrap();
    // The exported GQL must contain the backslash-escaped apostrophe
    assert!(
        gql.contains("\\'"),
        "export must escape apostrophe as \\'; got: {gql}"
    );

    let mut restored = Graph::new();
    import_gql(&mut restored, &gql).unwrap();

    assert_eq!(restored.node_count(), 1);
    let id = restored.nodes_by_label("Person")[0];
    let node = restored.node(id).unwrap();
    assert_eq!(
        node.properties().get("name"),
        Some(&Property::String("O'Brien".to_owned())),
        "apostrophe must survive GQL export + import round-trip"
    );
}

#[test]
fn gql_node_round_trip_preserves_integer_and_bool() {
    let mut original = Graph::new();
    let props: tessera_graph::Properties = [
        ("age".to_owned(), Property::I64(42)),
        ("active".to_owned(), Property::Bool(false)),
    ]
    .into_iter()
    .collect();
    original.add_node("User", props).unwrap();

    let gql = export_gql(&original).unwrap();
    let mut restored = Graph::new();
    import_gql(&mut restored, &gql).unwrap();

    let id = restored.nodes_by_label("User")[0];
    let node = restored.node(id).unwrap();
    assert_eq!(node.properties().get("age"), Some(&Property::I64(42)));
    assert_eq!(
        node.properties().get("active"),
        Some(&Property::Bool(false))
    );
}

#[test]
fn json_node_round_trip_preserves_properties() {
    let original = alice_bob_graph();
    let json_str = export_json(&original).unwrap();

    // JSON export uses source_id/target_id for edges (no nodes-only import).
    // Re-import using the nodes array only (manually extracted).
    let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();
    let nodes_only = serde_json::json!({
        "nodes": parsed["nodes"].clone(),
        "edges": []
    });

    let mut restored = Graph::new();
    let summary = import_json(&mut restored, &nodes_only.to_string()).unwrap();

    assert_eq!(summary.nodes_imported, 2);
    assert_eq!(restored.nodes_by_label("Person").len(), 2);
}
