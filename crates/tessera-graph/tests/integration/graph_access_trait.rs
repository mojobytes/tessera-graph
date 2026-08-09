// SPDX-License-Identifier: MIT

use tessera_graph::{
    Direction, Graph, GraphAccess, NeighborQuery, PatternBuilder, Properties, SubgraphQuery,
    TraversalBuilder, props,
};

use crate::helpers::mock_graph::DelegatingGraph;

#[test]
fn graph_access_is_exported() {
    // Compile-time assertion: if `GraphAccess` is not re-exported from
    // `lib.rs`, this file fails to compile — catching the regression
    // before any test runs. The function is instantiated below to make
    // the check explicit and suppress dead_code warnings.
    fn assert_trait_exists<T: GraphAccess>() {}
    assert_trait_exists::<Graph>();
}

#[test]
fn graph_impl_satisfies_trait() {
    fn use_trait<G: GraphAccess>(g: &mut G) -> tessera_graph::NodeId {
        g.add_node("X", Properties::new()).unwrap()
    }

    let mut g = Graph::new();
    let id = use_trait(&mut g);
    assert!(g.node_exists(id));
    assert_eq!(g.node_count(), 1);
}

#[test]
fn graph_access_node_reads() {
    fn read_via_trait<G: GraphAccess>(g: &G) {
        assert_eq!(g.node_count(), 2);
        let ids = g.node_ids();
        assert_eq!(ids.len(), 2);
        let labels = g.nodes_by_label("A");
        assert_eq!(labels.len(), 1);
        for id in ids {
            assert!(g.node_exists(id));
            let _node = g.node(id).unwrap();
        }
    }

    let mut g = Graph::new();
    g.add_node("A", Properties::new()).unwrap();
    g.add_node("B", Properties::new()).unwrap();
    read_via_trait(&g);
}

#[test]
fn graph_access_edge_operations() {
    fn edge_ops<G: GraphAccess>(g: &mut G, src: tessera_graph::NodeId, dst: tessera_graph::NodeId) {
        assert_eq!(g.edge_count(), 0);
        let eid = g.add_edge("KNOWS", src, dst, Properties::new()).unwrap();
        assert_eq!(g.edge_count(), 1);
        let edge = g.edge(eid).unwrap();
        assert_eq!(edge.label(), "KNOWS");
        let out = g.outgoing_edges(src).unwrap();
        assert_eq!(out.len(), 1);
        let inc = g.incoming_edges(dst).unwrap();
        assert_eq!(inc.len(), 1);
    }

    let mut g = Graph::new();
    let a = g.add_node("A", Properties::new()).unwrap();
    let b = g.add_node("B", Properties::new()).unwrap();
    edge_ops(&mut g, a, b);
}

#[test]
fn delegating_graph_implements_graph_access() {
    let mut proxy = DelegatingGraph::new();
    let id = proxy.add_node("Foo", Properties::new()).unwrap();
    assert_eq!(proxy.node_count(), 1);
    assert!(proxy.node_exists(id));

    let node = proxy.node(id).unwrap();
    assert_eq!(node.label(), "Foo");
}

#[test]
fn neighbor_query_works_with_delegating_graph() {
    let mut proxy = DelegatingGraph::new();
    let a = proxy.add_node("A", Properties::new()).unwrap();
    let b = proxy.add_node("B", Properties::new()).unwrap();
    proxy.add_edge("KNOWS", a, b, Properties::new()).unwrap();

    let edges = NeighborQuery::new(&proxy, a)
        .direction(Direction::Outgoing)
        .collect()
        .unwrap();

    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0].label(), "KNOWS");
}

#[test]
fn traversal_builder_works_with_delegating_graph() {
    let mut proxy = DelegatingGraph::new();
    let a = proxy.add_node("A", Properties::new()).unwrap();
    let b = proxy.add_node("B", Properties::new()).unwrap();
    proxy.add_edge("R", a, b, Properties::new()).unwrap();

    let visited = TraversalBuilder::new(&proxy, a)
        .direction(Direction::Outgoing)
        .bfs()
        .collect()
        .unwrap();

    assert_eq!(visited, vec![a, b]);
}

#[test]
fn subgraph_query_works_with_delegating_graph() {
    let mut proxy = DelegatingGraph::new();
    let a = proxy.add_node("A", Properties::new()).unwrap();
    let b = proxy.add_node("B", Properties::new()).unwrap();
    proxy.add_edge("R", a, b, Properties::new()).unwrap();

    let sub = SubgraphQuery::new(&proxy, a)
        .direction(Direction::Outgoing)
        .extract()
        .unwrap();

    assert_eq!(sub.node_count(), 2);
    assert_eq!(sub.edge_count(), 1);
}

#[test]
fn pattern_builder_works_with_delegating_graph() {
    let mut proxy = DelegatingGraph::new();
    let alice = proxy
        .add_node("Person", props! { "name" => "Alice" })
        .unwrap();
    let bob = proxy
        .add_node("Person", props! { "name" => "Bob" })
        .unwrap();
    proxy
        .add_edge("KNOWS", alice, bob, Properties::new())
        .unwrap();

    let results: Vec<_> = PatternBuilder::new(&proxy)
        .node("a")
        .label("Person")
        .edge(Direction::Outgoing)
        .label("KNOWS")
        .node("b")
        .label("Person")
        .execute()
        .unwrap()
        .collect::<tessera_graph::Result<Vec<_>>>()
        .unwrap();

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].get_node("a").unwrap().id(), alice);
    assert_eq!(results[0].get_node("b").unwrap().id(), bob);
}

#[test]
fn delegating_graph_node_projected_via_trait() {
    use tessera_graph::GraphAccess;
    let mut proxy = DelegatingGraph::new();
    let id = proxy
        .add_node("P", props! { "x" => 1_i64, "y" => 2_i64 })
        .unwrap();

    let node = GraphAccess::node_projected(&proxy, id, &["x"]).unwrap();
    assert_eq!(node.properties().len(), 1);
    assert!(node.properties().contains_key("x"));
    assert!(!node.properties().contains_key("y"));
}

#[test]
fn gql_compiler_works_with_delegating_graph() {
    let mut proxy = DelegatingGraph::new();
    proxy
        .add_node("Person", props! { "name" => "Alice" })
        .unwrap();

    let query = tessera_graph::gql::parse("MATCH (a:Person) RETURN a.name").unwrap();
    let result = tessera_graph::gql::execute(&proxy, &query, 0).unwrap();

    assert_eq!(result.len(), 1);
    let row = &result[0];
    assert_eq!(
        row.get("a.name"),
        Some(&tessera_graph::GqlValue::Str("Alice".into()))
    );
}

#[test]
fn dyn_graph_access_works() {
    // Verifies that GraphAccess is object-safe (dyn-compatible).
    let mut g = Graph::new();
    let ga: &mut dyn GraphAccess = &mut g;

    let id = ga.add_node("Label", Properties::new()).unwrap();
    assert_eq!(ga.node_count(), 1);
    assert!(ga.node_exists(id));

    let node = ga.node(id).unwrap();
    assert_eq!(node.label(), "Label");
}

#[test]
fn existing_graph_sugar_api_unchanged() {
    // Verifies that the existing public API on Graph works without any changes.
    let mut g = Graph::new();
    let a = g.add_node("A", Properties::new()).unwrap(); // impl Into<String> sugar
    let b = g.add_node("B", Properties::new()).unwrap();
    g.add_edge("KNOWS", a, b, Properties::new()).unwrap(); // impl Into<String> sugar

    // NeighborQuery via Graph sugar
    let edges = g
        .neighbors(a)
        .direction(Direction::Outgoing)
        .collect()
        .unwrap();
    assert_eq!(edges.len(), 1);

    // TraversalBuilder via Graph sugar
    let visited = g
        .traverse(a)
        .direction(Direction::Outgoing)
        .bfs()
        .collect()
        .unwrap();
    assert_eq!(visited, vec![a, b]);

    // SubgraphQuery via Graph sugar
    let sub = g
        .subgraph(a)
        .direction(Direction::Outgoing)
        .extract()
        .unwrap();
    assert_eq!(sub.node_count(), 2);

    // PatternBuilder via Graph sugar
    let matches: Vec<_> = g
        .pattern()
        .node("x")
        .label("A")
        .edge(Direction::Outgoing)
        .label("KNOWS")
        .node("y")
        .execute()
        .unwrap()
        .collect::<tessera_graph::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(matches.len(), 1);

    // GQL via Graph
    let query = tessera_graph::gql::parse("MATCH (a:A) RETURN a.name").unwrap();
    let _result = tessera_graph::gql::execute(&g, &query, 0).unwrap();
}

#[test]
fn gql_two_anonymous_nodes_do_not_collide() {
    // MATCH (:Person)-[:KNOWS]->(:Person) — both nodes are anonymous.
    // The compiler must generate distinct synthetic variables (_anon_0, _anon_1),
    // not both "_anon", which would cause an InvalidPattern duplicate error.
    let mut g = Graph::new();
    let alice = g.add_node("Person", props! { "name" => "Alice" }).unwrap();
    let bob = g.add_node("Person", props! { "name" => "Bob" }).unwrap();
    g.add_edge("KNOWS", alice, bob, Properties::new()).unwrap();

    let query = tessera_graph::gql::parse("MATCH (:Person)-[:KNOWS]->(:Person) RETURN 1").unwrap();
    let result = tessera_graph::gql::execute(&g, &query, 0).unwrap();
    // Should find exactly one match (alice->bob), not zero or an error.
    assert_eq!(result.len(), 1);
}
