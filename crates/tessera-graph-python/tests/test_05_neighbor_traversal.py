"""Phase 5: NeighborQuery, TraversalBuilder, and Path."""

import pytest
from tessera_graph import (
    Graph,
    Node,
    Edge,
    NodeId,
    EdgeId,
    Direction,
    Path,
    NodeNotFoundError,
)


@pytest.fixture
def triangle():
    """a -KNOWS-> b -KNOWS-> c, a -LIKES-> c."""
    g = Graph.new()
    a = g.add_node("Person", {"name": "Alice"})
    b = g.add_node("Person", {"name": "Bob"})
    c = g.add_node("Person", {"name": "Carol"})
    g.add_edge("KNOWS", a, b, {})
    g.add_edge("KNOWS", b, c, {})
    g.add_edge("LIKES", a, c, {})
    return g, a, b, c


@pytest.fixture
def linear():
    """a -> b -> c -> d (all NEXT edges)."""
    g = Graph.new()
    a = g.add_node("N", {})
    b = g.add_node("N", {})
    c = g.add_node("N", {})
    d = g.add_node("N", {})
    g.add_edge("NEXT", a, b, {})
    g.add_edge("NEXT", b, c, {})
    g.add_edge("NEXT", c, d, {})
    return g, a, b, c, d


# ── NeighborQuery ────────────────────────────────────────────────────────────


def test_neighbors_collect_returns_edges(triangle):
    g, a, _, _ = triangle
    edges = g.neighbors(a).collect()
    assert isinstance(edges, list)
    assert len(edges) == 2  # KNOWS->b, LIKES->c
    assert all(isinstance(e, Edge) for e in edges)


def test_neighbors_direction_outgoing(triangle):
    g, a, b, _ = triangle
    out_a = g.neighbors(a).direction(Direction.OUTGOING).collect()
    assert len(out_a) == 2
    out_b = g.neighbors(b).direction(Direction.OUTGOING).collect()
    assert len(out_b) == 1


def test_neighbors_direction_incoming(triangle):
    g, _, b, _ = triangle
    inc = g.neighbors(b).direction(Direction.INCOMING).collect()
    assert len(inc) == 1
    assert inc[0].target() == b


def test_neighbors_direction_string(triangle):
    g, a, _, _ = triangle
    edges = g.neighbors(a).direction("outgoing").collect()
    assert len(edges) == 2


def test_neighbors_label_filter(triangle):
    g, a, _, _ = triangle
    knows = g.neighbors(a).label("KNOWS").collect()
    assert len(knows) == 1
    assert knows[0].label() == "KNOWS"
    none = g.neighbors(a).label("NONEXISTENT").collect()
    assert len(none) == 0


def test_neighbors_node_ids(triangle):
    g, a, b, c = triangle
    ids = g.neighbors(a).direction(Direction.OUTGOING).node_ids()
    assert isinstance(ids, list)
    assert len(ids) == 2
    assert all(isinstance(nid, NodeId) for nid in ids)
    assert set(nid.value for nid in ids) == {b.value, c.value}


def test_neighbors_chain(triangle):
    g, a, _, _ = triangle
    result = g.neighbors(a).direction(Direction.OUTGOING).label("KNOWS").collect()
    assert len(result) == 1


def test_neighbors_node_not_found(triangle):
    g, _, _, _ = triangle
    with pytest.raises(NodeNotFoundError):
        g.neighbors(NodeId(99999)).collect()


# ── TraversalBuilder ─────────────────────────────────────────────────────────


def test_traverse_bfs_collect(linear):
    g, a, b, c, d = linear
    visited = g.traverse(a).direction(Direction.OUTGOING).bfs().collect()
    assert isinstance(visited, list)
    assert all(isinstance(nid, NodeId) for nid in visited)
    assert len(visited) == 4
    assert visited[0] == a


def test_traverse_dfs_collect(linear):
    g, a, _, _, _ = linear
    visited = g.traverse(a).direction(Direction.OUTGOING).dfs().collect()
    assert len(visited) == 4
    assert visited[0] == a


def test_traverse_max_depth(linear):
    g, a, b, c, _ = linear
    visited = g.traverse(a).direction(Direction.OUTGOING).max_depth(2).collect()
    assert len(visited) == 3  # a (depth 0), b (1), c (2)
    values = [nid.value for nid in visited]
    assert a.value in values
    assert b.value in values
    assert c.value in values


def test_traverse_label_filter(triangle):
    g, a, b, _ = triangle
    visited = g.traverse(a).direction(Direction.OUTGOING).label("KNOWS").collect()
    # a -> b (KNOWS), b -> c (KNOWS); LIKES edge skipped
    values = {nid.value for nid in visited}
    assert a.value in values
    assert b.value in values


def test_traverse_direction_string(linear):
    g, a, _, _, _ = linear
    visited = g.traverse(a).direction("outgoing").collect()
    assert len(visited) == 4


def test_traverse_collect_paths(linear):
    g, a, _, _, _ = linear
    paths = g.traverse(a).direction(Direction.OUTGOING).collect_paths()
    assert isinstance(paths, list)
    assert len(paths) == 4  # one path per visited node
    assert all(isinstance(p, Path) for p in paths)


def test_traverse_chain(linear):
    g, a, _, _, _ = linear
    result = (
        g.traverse(a)
        .direction(Direction.OUTGOING)
        .dfs()
        .max_depth(5)
        .label("NEXT")
        .collect()
    )
    assert isinstance(result, list)
    assert all(isinstance(nid, NodeId) for nid in result)


# ── Path type ────────────────────────────────────────────────────────────────


def test_path_nodes_and_edges(linear):
    g, a, _, _, _ = linear
    paths = g.traverse(a).direction(Direction.OUTGOING).collect_paths()
    # The path to the last node has 3 edges
    longest = max(paths, key=len)
    assert len(longest) == 3
    nodes = longest.nodes()
    edges = longest.edges()
    assert len(nodes) == 4
    assert len(edges) == 3
    assert all(isinstance(nid, NodeId) for nid in nodes)
    assert all(isinstance(eid, EdgeId) for eid in edges)


def test_path_start_end(linear):
    g, a, _, _, d = linear
    paths = g.traverse(a).direction(Direction.OUTGOING).collect_paths()
    longest = max(paths, key=len)
    assert longest.start() == a
    assert longest.end() == d


def test_path_len(linear):
    g, a, _, _, _ = linear
    paths = g.traverse(a).direction(Direction.OUTGOING).collect_paths()
    single = [p for p in paths if len(p) == 0]
    assert len(single) == 1  # the start node path has 0 edges


def test_path_bool():
    """Empty path is falsy, non-empty is truthy."""
    g = Graph.new()
    a = g.add_node("N", {})
    paths = g.traverse(a).direction(Direction.OUTGOING).collect_paths()
    assert len(paths) == 1
    assert not paths[0]  # single node, 0 edges -> falsy


def test_path_is_empty(linear):
    g, a, _, _, _ = linear
    paths = g.traverse(a).direction(Direction.OUTGOING).collect_paths()
    empty_paths = [p for p in paths if p.is_empty]
    non_empty = [p for p in paths if not p.is_empty]
    assert len(empty_paths) == 1  # start node
    assert len(non_empty) == 3


def test_path_repr(linear):
    g, a, _, _, _ = linear
    paths = g.traverse(a).direction(Direction.OUTGOING).collect_paths()
    r = repr(paths[0])
    assert "Path" in r


def test_path_iter(linear):
    g, a, _, _, _ = linear
    paths = g.traverse(a).direction(Direction.OUTGOING).collect_paths()
    longest = max(paths, key=len)
    node_ids = list(longest)
    assert len(node_ids) == 4
    assert all(isinstance(nid, NodeId) for nid in node_ids)
