// SPDX-License-Identifier: LicenseRef-TesseraGraph-Proprietary

use tessera_graph::{Graph, GraphConfig, Properties};

#[test]
fn node_and_edge_take_shared_ref() {
    let mut g = Graph::new();
    let n = g.add_node("Person", Properties::new()).unwrap();
    let e = g.add_edge("KNOWS", n, n, Properties::new()).unwrap();

    // Read methods accept &self (shared reference)
    let g_ref: &Graph = &g;
    let node = g_ref.node(n).unwrap();
    let edge = g_ref.edge(e).unwrap();
    assert_eq!(node.id(), n);
    assert_eq!(edge.id(), e);
}

#[test]
fn outgoing_and_incoming_take_shared_ref() {
    let mut g = Graph::new();
    let a = g.add_node("A", Properties::new()).unwrap();
    let b = g.add_node("B", Properties::new()).unwrap();
    g.add_edge("R", a, b, Properties::new()).unwrap();

    let g_ref: &Graph = &g;
    let out = g_ref.outgoing_edges(a).unwrap();
    let inc = g_ref.incoming_edges(b).unwrap();
    assert_eq!(out.len(), 1);
    assert_eq!(inc.len(), 1);
}

#[test]
fn file_backed_reads_take_shared_ref() {
    let tmp = tempfile::TempDir::new().unwrap();
    let config = GraphConfig {
        memory_limit_bytes: 64 * 1024,
        create_if_missing: true,
        adj_cache_capacity: 1024,
        wal_enabled: true,
        ..GraphConfig::new()
    };

    let n;
    let e;
    {
        let mut g = Graph::open(tmp.path(), &config).unwrap();
        n = g.add_node("Person", Properties::new()).unwrap();
        e = g.add_edge("KNOWS", n, n, Properties::new()).unwrap();
        g.flush().unwrap();
    }

    {
        let g = Graph::open(tmp.path(), &config).unwrap();
        let g_ref: &Graph = &g;
        let node = g_ref.node(n).unwrap();
        let edge = g_ref.edge(e).unwrap();
        assert_eq!(node.label(), "Person");
        assert_eq!(edge.label(), "KNOWS");
    }
}
