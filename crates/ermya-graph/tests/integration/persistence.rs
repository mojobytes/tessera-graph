// SPDX-License-Identifier: MIT

use ermya_graph::{Graph, GraphConfig, Property, props};
use tempfile::TempDir;

const fn test_config() -> GraphConfig {
    GraphConfig {
        memory_limit_bytes: 64 * 1024,
        create_if_missing: true,
        adj_cache_capacity: 1024,
        wal_enabled: true,
        ..GraphConfig::new()
    }
}

fn assert_str_prop(props: &ermya_graph::Properties, key: &str, expected: &str) {
    match props.get(key) {
        Some(Property::String(s)) => assert_eq!(s, expected),
        other => panic!("expected String(\"{expected}\") for key \"{key}\", got {other:?}"),
    }
}

fn assert_i64_prop(props: &ermya_graph::Properties, key: &str, expected: i64) {
    match props.get(key) {
        Some(Property::I64(v)) => assert_eq!(*v, expected),
        other => panic!("expected I64({expected}) for key \"{key}\", got {other:?}"),
    }
}

#[test]
fn open_creates_new_store() {
    let tmp = TempDir::new().unwrap();
    let mut g = Graph::open(tmp.path(), &test_config()).unwrap();
    assert_eq!(g.node_count(), 0);
    assert_eq!(g.edge_count(), 0);
    g.flush().unwrap();
}

#[test]
fn nodes_persist_across_reopen() {
    let tmp = TempDir::new().unwrap();

    let n1;
    let n2;
    {
        let mut g = Graph::open(tmp.path(), &test_config()).unwrap();
        n1 = g.add_node("Person", props! { "name" => "Alice" }).unwrap();
        n2 = g.add_node("Person", props! { "name" => "Bob" }).unwrap();
        g.flush().unwrap();
    }

    {
        let g = Graph::open(tmp.path(), &test_config()).unwrap();
        assert_eq!(g.node_count(), 2);

        let alice = g.node(n1).unwrap();
        assert_eq!(alice.label(), "Person");
        assert_str_prop(alice.properties(), "name", "Alice");

        let bob = g.node(n2).unwrap();
        assert_eq!(bob.label(), "Person");
        assert_str_prop(bob.properties(), "name", "Bob");
    }
}

#[test]
fn edges_persist_across_reopen() {
    let tmp = TempDir::new().unwrap();

    let n1;
    let n2;
    let e1;
    {
        let mut g = Graph::open(tmp.path(), &test_config()).unwrap();
        n1 = g.add_node("A", props! {}).unwrap();
        n2 = g.add_node("B", props! {}).unwrap();
        e1 = g
            .add_edge("KNOWS", n1, n2, props! { "since" => 2020_i64 })
            .unwrap();
        g.flush().unwrap();
    }

    {
        let g = Graph::open(tmp.path(), &test_config()).unwrap();
        assert_eq!(g.edge_count(), 1);

        let edge = g.edge(e1).unwrap();
        assert_eq!(edge.label(), "KNOWS");
        assert_eq!(edge.source(), n1);
        assert_eq!(edge.target(), n2);
        assert_i64_prop(edge.properties(), "since", 2020);
    }
}

#[test]
fn adjacency_persists_across_reopen() {
    let tmp = TempDir::new().unwrap();

    let center;
    {
        let mut g = Graph::open(tmp.path(), &test_config()).unwrap();
        center = g.add_node("Hub", props! {}).unwrap();
        for i in 0_i64..5 {
            let leaf = g.add_node("Leaf", props! { "idx" => i }).unwrap();
            g.add_edge("LINK", center, leaf, props! {}).unwrap();
        }
        g.flush().unwrap();
    }

    {
        let g = Graph::open(tmp.path(), &test_config()).unwrap();
        assert_eq!(g.node_count(), 6);
        assert_eq!(g.edge_count(), 5);

        let outgoing = g.outgoing_edges(center).unwrap();
        assert_eq!(outgoing.len(), 5);
    }
}

#[test]
fn remove_then_reopen() {
    let tmp = TempDir::new().unwrap();

    let n2;
    {
        let mut g = Graph::open(tmp.path(), &test_config()).unwrap();
        let n1 = g.add_node("A", props! {}).unwrap();
        n2 = g.add_node("B", props! {}).unwrap();
        let n3 = g.add_node("C", props! {}).unwrap();
        g.add_edge("AB", n1, n2, props! {}).unwrap();
        g.add_edge("BC", n2, n3, props! {}).unwrap();

        g.remove_node(n2).unwrap();
        g.flush().unwrap();
    }

    {
        let g = Graph::open(tmp.path(), &test_config()).unwrap();
        assert_eq!(g.node_count(), 2);
        assert_eq!(g.edge_count(), 0);
        assert!(g.node(n2).is_err());
    }
}

#[test]
fn incremental_writes_across_sessions() {
    let tmp = TempDir::new().unwrap();

    {
        let mut g = Graph::open(tmp.path(), &test_config()).unwrap();
        for i in 0_i64..10 {
            g.add_node("N", props! { "i" => i }).unwrap();
        }
        g.flush().unwrap();
    }

    {
        let mut g = Graph::open(tmp.path(), &test_config()).unwrap();
        assert_eq!(g.node_count(), 10);

        for i in 10_i64..20 {
            g.add_node("N", props! { "i" => i }).unwrap();
        }
        g.flush().unwrap();
    }

    {
        let g = Graph::open(tmp.path(), &test_config()).unwrap();
        assert_eq!(g.node_count(), 20);
    }
}

#[test]
fn overflow_label_persists() {
    let tmp = TempDir::new().unwrap();
    let long_label = "X".repeat(100);

    let nid;
    {
        let mut g = Graph::open(tmp.path(), &test_config()).unwrap();
        nid = g.add_node(&long_label, props! {}).unwrap();
        g.flush().unwrap();
    }

    {
        let g = Graph::open(tmp.path(), &test_config()).unwrap();
        let node = g.node(nid).unwrap();
        assert_eq!(node.label(), long_label);
    }
}

#[test]
fn overflow_props_persist() {
    let tmp = TempDir::new().unwrap();
    let big_value = "V".repeat(200);

    let nid;
    {
        let mut g = Graph::open(tmp.path(), &test_config()).unwrap();
        nid = g
            .add_node("Node", props! { "data" => big_value.as_str() })
            .unwrap();
        g.flush().unwrap();
    }

    {
        let g = Graph::open(tmp.path(), &test_config()).unwrap();
        let node = g.node(nid).unwrap();
        assert_str_prop(node.properties(), "data", &big_value);
    }
}

/// The exact scenario from issues #62/#75: a node whose `full_text` property
/// measures 283,718 bytes (the AI Act's `Preamble` in the reporting
/// consumer). Under the old u16 length prefix this was first silently
/// truncated (pre-v0.11.1, corrupting reads intermittently) and then
/// rejected with `RecordTooLarge` (v0.11.1–v0.12.x). Since #75 it must
/// round-trip through the full public path — encode, overflow across ~70
/// pages, flush, reopen, decode — alongside a second property that would
/// desynchronise if the recorded length were wrong.
#[test]
fn property_value_over_64kib_persists_across_reopen() {
    let tmp = TempDir::new().unwrap();
    let full_text = "x".repeat(283_718);

    let nid;
    {
        let mut g = Graph::open(tmp.path(), &test_config()).unwrap();
        nid = g
            .add_node(
                "Preamble",
                props! { "full_text" => full_text.as_str(), "order" => 1_i64 },
            )
            .unwrap();
        g.flush().unwrap();
    }

    {
        let g = Graph::open(tmp.path(), &test_config()).unwrap();
        let node = g.node(nid).unwrap();
        assert_eq!(node.label(), "Preamble");
        assert_str_prop(node.properties(), "full_text", &full_text);
        // The property after the big one is the canary: a wrong recorded
        // length desynchronises everything that follows it in the stream.
        assert_i64_prop(node.properties(), "order", 1);
    }
}

#[test]
fn many_nodes_across_pages() {
    let tmp = TempDir::new().unwrap();
    let count: usize = 100;

    {
        let mut g = Graph::open(tmp.path(), &test_config()).unwrap();
        for i in 0_i64..100 {
            g.add_node("N", props! { "i" => i }).unwrap();
        }
        g.flush().unwrap();
    }

    {
        let g = Graph::open(tmp.path(), &test_config()).unwrap();
        assert_eq!(g.node_count(), count);
    }
}

#[test]
fn update_then_reopen() {
    let tmp = TempDir::new().unwrap();

    let nid;
    {
        let mut g = Graph::open(tmp.path(), &test_config()).unwrap();
        nid = g
            .add_node("Original", props! { "version" => 1_i64 })
            .unwrap();
        g.flush().unwrap();
    }

    {
        let mut g = Graph::open(tmp.path(), &test_config()).unwrap();
        let mut node = g.node(nid).unwrap();
        node.properties_mut()
            .insert("version".to_string(), 2_i64.into());
        g.update_node(nid, &node).unwrap();
        g.flush().unwrap();
    }

    {
        let g = Graph::open(tmp.path(), &test_config()).unwrap();
        let node = g.node(nid).unwrap();
        assert_i64_prop(node.properties(), "version", 2);
    }
}
