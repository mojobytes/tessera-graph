"""Phase 2: Full CRUD for nodes and edges, update via kwargs, convenience iterators."""

import pytest
from tessera_graph import (
    Graph,
    NodeId,
    EdgeId,
    Node,
    Edge,
    NodeNotFoundError,
    EdgeNotFoundError,
    TesseraError,
)


# ── Fixtures ─────────────────────────────────────────────────────────────────


@pytest.fixture
def g():
    return Graph.new()


@pytest.fixture
def populated(g):
    """Graph with 2 Person nodes and 1 KNOWS edge."""
    a = g.add_node("Person", {"name": "Alice", "age": 30})
    b = g.add_node("Person", {"name": "Bob", "age": 25})
    e = g.add_edge("KNOWS", a, b, {"since": 2020})
    return g, a, b, e


# ── Node CRUD ────────────────────────────────────────────────────────────────


def test_add_node_returns_node_id(g):
    nid = g.add_node("Person", {"name": "Alice"})
    assert isinstance(nid, NodeId)
    assert isinstance(nid.value, int)


def test_node_count_increments(g):
    g.add_node("A", {})
    g.add_node("B", {})
    g.add_node("C", {})
    assert g.node_count() == 3


def test_get_node_by_id(populated):
    g, a, b, _ = populated
    node = g.node(a)
    assert node.id() == a
    assert node.label() == "Person"
    assert node.properties()["name"] == "Alice"


def test_node_not_found_raises(g):
    with pytest.raises(NodeNotFoundError):
        g.node(NodeId(99999))


def test_node_exists(populated):
    g, a, _, _ = populated
    assert g.node_exists(a) is True
    assert g.node_exists(NodeId(99999)) is False


def test_node_ids_returns_list_of_node_ids(populated):
    g, _, _, _ = populated
    ids = g.node_ids()
    assert isinstance(ids, list)
    assert len(ids) == g.node_count()
    assert all(isinstance(nid, NodeId) for nid in ids)


def test_nodes_by_label(g):
    g.add_node("Person", {"name": "Alice"})
    g.add_node("Person", {"name": "Bob"})
    g.add_node("Company", {"name": "Acme"})
    persons = g.nodes_by_label("Person")
    assert len(persons) == 2
    assert all(isinstance(nid, NodeId) for nid in persons)
    assert g.nodes_by_label("Nonexistent") == []


def test_remove_node_returns_node(populated):
    g, a, _, _ = populated
    removed = g.remove_node(a)
    assert isinstance(removed, Node)
    assert removed.label() == "Person"
    assert removed.properties()["name"] == "Alice"
    assert g.node_exists(a) is False


def test_remove_node_decrements_count(g):
    nid = g.add_node("X", {})
    assert g.node_count() == 1
    g.remove_node(nid)
    assert g.node_count() == 0


def test_remove_nonexistent_node_raises(g):
    with pytest.raises(NodeNotFoundError):
        g.remove_node(NodeId(99999))


def test_update_node_label(populated):
    g, a, _, _ = populated
    g.update_node(a, label="Employee")
    assert g.node(a).label() == "Employee"


def test_update_node_properties(populated):
    g, a, _, _ = populated
    g.update_node(a, properties={"role": "engineer"})
    props = g.node(a).properties()
    assert props["role"] == "engineer"


def test_update_node_label_and_properties(populated):
    g, a, _, _ = populated
    g.update_node(a, label="Staff", properties={"dept": "eng"})
    node = g.node(a)
    assert node.label() == "Staff"
    assert node.properties()["dept"] == "eng"


def test_update_nonexistent_node_raises(g):
    with pytest.raises(NodeNotFoundError):
        g.update_node(NodeId(99999), label="X")


# ── Node dunder methods ─────────────────────────────────────────────────────


def test_node_repr(populated):
    g, a, _, _ = populated
    node = g.node(a)
    r = repr(node)
    assert "Node" in r
    assert "Person" in r


def test_node_eq(populated):
    g, a, _, _ = populated
    n1 = g.node(a)
    n2 = g.node(a)
    assert n1 == n2


def test_node_hash(populated):
    g, a, b, _ = populated
    s = {g.node(a), g.node(b), g.node(a)}
    assert len(s) == 2


# ── Edge CRUD ────────────────────────────────────────────────────────────────


def test_add_edge_returns_edge_id(populated):
    _, _, _, e = populated
    assert isinstance(e, EdgeId)


def test_edge_count_increments(g):
    a = g.add_node("A", {})
    b = g.add_node("B", {})
    g.add_edge("R", a, b, {})
    g.add_edge("S", a, b, {})
    assert g.edge_count() == 2


def test_get_edge_by_id(populated):
    g, a, b, e = populated
    edge = g.edge(e)
    assert edge.id() == e
    assert edge.label() == "KNOWS"
    assert edge.source() == a
    assert edge.target() == b
    assert edge.properties()["since"] == 2020


def test_edge_not_found_raises(g):
    with pytest.raises(EdgeNotFoundError):
        g.edge(EdgeId(99999))


def test_edges_by_label(g):
    a = g.add_node("A", {})
    b = g.add_node("B", {})
    g.add_edge("KNOWS", a, b, {})
    g.add_edge("KNOWS", b, a, {})
    g.add_edge("LIKES", a, b, {})
    assert len(g.edges_by_label("KNOWS")) == 2
    assert len(g.edges_by_label("LIKES")) == 1
    assert g.edges_by_label("NONE") == []


def test_remove_edge_returns_edge(populated):
    g, _, _, e = populated
    removed = g.remove_edge(e)
    assert isinstance(removed, Edge)
    assert removed.label() == "KNOWS"
    assert g.edge_count() == 0


def test_remove_nonexistent_edge_raises(g):
    with pytest.raises(EdgeNotFoundError):
        g.remove_edge(EdgeId(99999))


def test_outgoing_edges(populated):
    g, a, _, _ = populated
    edges = g.outgoing_edges(a)
    assert isinstance(edges, list)
    assert len(edges) == 1
    assert all(isinstance(e, Edge) for e in edges)
    assert edges[0].source() == a


def test_incoming_edges(populated):
    g, _, b, _ = populated
    edges = g.incoming_edges(b)
    assert isinstance(edges, list)
    assert len(edges) == 1
    assert edges[0].target() == b


def test_outgoing_edges_node_not_found(g):
    with pytest.raises(NodeNotFoundError):
        g.outgoing_edges(NodeId(99999))


def test_update_edge_label(populated):
    g, _, _, e = populated
    g.update_edge(e, label="FRIENDS")
    assert g.edge(e).label() == "FRIENDS"


def test_update_edge_properties(populated):
    g, _, _, e = populated
    g.update_edge(e, properties={"weight": 5.0})
    assert g.edge(e).properties()["weight"] == 5.0


def test_update_nonexistent_edge_raises(g):
    with pytest.raises(EdgeNotFoundError):
        g.update_edge(EdgeId(99999), label="X")


# ── Edge dunder methods ──────────────────────────────────────────────────────


def test_edge_repr(populated):
    g, _, _, e = populated
    r = repr(g.edge(e))
    assert "Edge" in r
    assert "KNOWS" in r


def test_edge_eq(populated):
    g, _, _, e = populated
    e1 = g.edge(e)
    e2 = g.edge(e)
    assert e1 == e2


def test_edge_hash(populated):
    g, a, b, e = populated
    e2 = g.add_edge("LIKES", a, b, {})
    s = {g.edge(e), g.edge(e2), g.edge(e)}
    assert len(s) == 2


# ── Convenience iterators ───────────────────────────────────────────────────


def test_nodes_iterator(populated):
    g, _, _, _ = populated
    nodes = g.nodes()
    assert isinstance(nodes, list)
    assert len(nodes) == g.node_count()
    assert all(isinstance(n, Node) for n in nodes)


def test_edges_iterator(populated):
    g, _, _, _ = populated
    edges = g.edges()
    assert isinstance(edges, list)
    assert len(edges) == g.edge_count()
    assert all(isinstance(e, Edge) for e in edges)


def test_nodes_empty_graph(g):
    assert g.nodes() == []


def test_edges_empty_graph(g):
    assert g.edges() == []
