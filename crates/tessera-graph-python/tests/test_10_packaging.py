"""Phase 10: Module exports, naming, and packaging."""

import tessera_graph


def test_module_imports_all_types():
    from tessera_graph import (
        Graph,
        NodeId,
        EdgeId,
        Node,
        Edge,
        Direction,
        Strategy,
        Path,
        Subgraph,
        SharedGraph,
        PatternMatch,
        GqlResult,
        GqlRow,
        GqlMutationResult,
        TesseraError,
        NodeNotFoundError,
        EdgeNotFoundError,
        GqlSyntaxError,
        execute,
        validate,
    )
    # All should be importable without error
    assert Graph is not None
    assert execute is not None
    assert validate is not None


def test_graph_class_named_graph():
    assert type(tessera_graph.Graph.new()).__name__ == "Graph"


def test_node_class_named_node():
    g = tessera_graph.Graph.new()
    nid = g.add_node("N", {})
    assert type(g.node(nid)).__name__ == "Node"


def test_edge_class_named_edge():
    g = tessera_graph.Graph.new()
    a = g.add_node("A", {})
    b = g.add_node("B", {})
    eid = g.add_edge("R", a, b, {})
    assert type(g.edge(eid)).__name__ == "Edge"


def test_direction_class_named_direction():
    assert type(tessera_graph.Direction.OUTGOING).__name__ == "Direction"


def test_strategy_class_named_strategy():
    assert type(tessera_graph.Strategy.BFS).__name__ == "Strategy"


def test_node_id_class_named():
    assert type(tessera_graph.NodeId(1)).__name__ == "NodeId"


def test_edge_id_class_named():
    assert type(tessera_graph.EdgeId(1)).__name__ == "EdgeId"


def test_shared_graph_class_named():
    sg = tessera_graph.SharedGraph.new(tessera_graph.Graph.new())
    assert type(sg).__name__ == "SharedGraph"


def test_end_to_end_workflow():
    """Full workflow: create graph, add data, query with GQL, use pattern matching."""
    g = tessera_graph.Graph.new()

    # Add data
    alice = g.add_node("Person", {"name": "Alice", "age": 30})
    bob = g.add_node("Person", {"name": "Bob", "age": 25})
    g.add_edge("KNOWS", alice, bob, {"since": 2020})

    # GQL query
    result = tessera_graph.execute(g, "MATCH (a:Person) RETURN a.name ORDER BY a.name ASC")
    assert [r["a.name"] for r in result] == ["Alice", "Bob"]

    # Pattern matching
    matches = (
        g.pattern()
        .node("a")
        .label("Person")
        .where_prop("name", "Alice")
        .edge(tessera_graph.Direction.OUTGOING)
        .label("KNOWS")
        .node("b")
        .execute()
    )
    assert len(matches) == 1
    assert matches[0].get_node("b").properties()["name"] == "Bob"

    # Traversal
    visited = g.traverse(alice).direction("outgoing").collect()
    assert len(visited) == 2

    # Shortest path
    path = g.shortest_path(alice, bob).direction("outgoing").find()
    assert path is not None
    assert len(path) == 1
