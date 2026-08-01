"""Phase 6: ShortestPath, WeightedPath (Python callable), Subgraph."""

import pytest
from tessera_graph import (
    Graph,
    Node,
    Edge,
    NodeId,
    EdgeId,
    Direction,
    Path,
    Subgraph,
    NodeNotFoundError,
)


# ── Fixtures ─────────────────────────────────────────────────────────────────


@pytest.fixture
def linear():
    """a -> b -> c (NEXT edges)."""
    g = Graph.new()
    a = g.add_node("N", {})
    b = g.add_node("N", {})
    c = g.add_node("N", {})
    g.add_edge("NEXT", a, b, {})
    g.add_edge("NEXT", b, c, {})
    return g, a, b, c


@pytest.fixture
def weighted_diamond():
    """
    a -R(cost=10)-> b -R(cost=10)-> c
    a -S(cost=1)-> d -S(cost=1)-> c
    Cheap path is a->d->c (cost 2), expensive is a->b->c (cost 20).
    """
    g = Graph.new()
    a = g.add_node("N", {})
    b = g.add_node("N", {})
    c = g.add_node("N", {})
    d = g.add_node("N", {})
    g.add_edge("R", a, b, {"cost": 10.0})
    g.add_edge("R", b, c, {"cost": 10.0})
    g.add_edge("S", a, d, {"cost": 1.0})
    g.add_edge("S", d, c, {"cost": 1.0})
    return g, a, b, c, d


# ── ShortestPathQuery ────────────────────────────────────────────────────────


def test_shortest_path_direct(linear):
    g, a, b, _ = linear
    path = g.shortest_path(a, b).direction(Direction.OUTGOING).find()
    assert path is not None
    assert isinstance(path, Path)
    assert len(path) == 1


def test_shortest_path_multi_hop(linear):
    g, a, b, c = linear
    path = g.shortest_path(a, c).direction(Direction.OUTGOING).find()
    assert path is not None
    assert len(path) == 2
    assert path.nodes() == [a, b, c]


def test_shortest_path_returns_none_when_no_path():
    g = Graph.new()
    a = g.add_node("A", {})
    b = g.add_node("B", {})
    path = g.shortest_path(a, b).direction(Direction.OUTGOING).find()
    assert path is None


def test_shortest_path_label_filter(linear):
    g, a, _, c = linear
    # NEXT edges exist; filtering by non-existent label gives no path
    path = g.shortest_path(a, c).direction(Direction.OUTGOING).label("NONEXISTENT").find()
    assert path is None
    # With correct label, path found
    path = g.shortest_path(a, c).direction(Direction.OUTGOING).label("NEXT").find()
    assert path is not None
    assert len(path) == 2


def test_shortest_path_direction_string(linear):
    g, a, _, c = linear
    path = g.shortest_path(a, c).direction("outgoing").find()
    assert path is not None


# ── WeightedPathQuery ────────────────────────────────────────────────────────


def test_weighted_path_default_weight(linear):
    g, a, _, c = linear
    result = g.weighted_shortest_path(a, c).direction(Direction.OUTGOING).find()
    assert result is not None
    cost, path = result
    assert isinstance(cost, float)
    assert cost == 2.0  # unit weight per edge, 2 edges
    assert isinstance(path, Path)


def test_weighted_path_python_callable(weighted_diamond):
    g, a, _, c, _ = weighted_diamond
    weight_fn = lambda edge: edge.properties().get("cost", 1.0)
    result = (
        g.weighted_shortest_path(a, c)
        .direction(Direction.OUTGOING)
        .weight(weight_fn)
        .find()
    )
    assert result is not None
    cost, path = result
    assert cost == 2.0  # cheap path a->d->c


def test_weighted_path_callable_chooses_optimal(weighted_diamond):
    g, a, b, c, d = weighted_diamond
    weight_fn = lambda edge: edge.properties().get("cost", 1.0)
    cost, path = (
        g.weighted_shortest_path(a, c)
        .direction(Direction.OUTGOING)
        .weight(weight_fn)
        .find()
    )
    # Cheap path goes through d, not b
    node_values = {nid.value for nid in path.nodes()}
    assert d.value in node_values
    assert cost == 2.0


def test_weighted_path_returns_none_when_unreachable():
    g = Graph.new()
    a = g.add_node("A", {})
    b = g.add_node("B", {})
    result = g.weighted_shortest_path(a, b).direction(Direction.OUTGOING).find()
    assert result is None


def test_weighted_path_direction_string(linear):
    g, a, _, c = linear
    result = g.weighted_shortest_path(a, c).direction("outgoing").find()
    assert result is not None


# ── SubgraphQuery ────────────────────────────────────────────────────────────


def test_subgraph_extract(linear):
    g, a, _, _ = linear
    sg = g.subgraph(a).direction(Direction.OUTGOING).extract()
    assert isinstance(sg, Subgraph)
    assert sg.node_count == 3
    assert sg.edge_count == 2
    assert bool(sg) is True


def test_subgraph_nodes_and_edges_types(linear):
    g, a, _, _ = linear
    sg = g.subgraph(a).direction(Direction.OUTGOING).extract()
    assert all(isinstance(n, Node) for n in sg.nodes)
    assert all(isinstance(e, Edge) for e in sg.edges)


def test_subgraph_isolated_node():
    g = Graph.new()
    a = g.add_node("Lone", {})
    sg = g.subgraph(a).direction(Direction.OUTGOING).extract()
    assert sg.node_count == 1
    assert sg.edge_count == 0
    assert bool(sg) is True  # has nodes, so not empty


def test_subgraph_max_depth(linear):
    g, a, _, _ = linear
    sg = g.subgraph(a).direction(Direction.OUTGOING).max_depth(1).extract()
    assert sg.node_count == 2  # a (depth 0), b (depth 1)


def test_subgraph_label_filter():
    g = Graph.new()
    a = g.add_node("N", {})
    b = g.add_node("N", {})
    c = g.add_node("N", {})
    g.add_edge("R", a, b, {})
    g.add_edge("S", a, c, {})
    sg = g.subgraph(a).direction(Direction.OUTGOING).label("R").extract()
    assert sg.node_count == 2  # a and b only


def test_subgraph_direction_string(linear):
    g, a, _, _ = linear
    sg = g.subgraph(a).direction("outgoing").extract()
    assert sg.node_count == 3


def test_subgraph_contains_node(linear):
    g, a, _, c = linear
    sg = g.subgraph(a).direction(Direction.OUTGOING).extract()
    # Check by NodeId
    assert a in sg
    assert c in sg
    assert NodeId(99999) not in sg


def test_subgraph_contains_edge(linear):
    g, a, _, _ = linear
    sg = g.subgraph(a).direction(Direction.OUTGOING).extract()
    edges = sg.edges
    assert edges[0] in sg


def test_subgraph_len(linear):
    g, a, _, _ = linear
    sg = g.subgraph(a).direction(Direction.OUTGOING).extract()
    assert len(sg) == 3  # node_count


def test_subgraph_repr(linear):
    g, a, _, _ = linear
    sg = g.subgraph(a).direction(Direction.OUTGOING).extract()
    r = repr(sg)
    assert "Subgraph" in r
