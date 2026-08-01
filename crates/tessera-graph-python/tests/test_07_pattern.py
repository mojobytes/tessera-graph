"""Phase 7: PatternBuilder and PatternMatch."""

import pytest
from tessera_graph import (
    Graph,
    Node,
    Edge,
    Direction,
    PatternMatch,
    TesseraError,
)


@pytest.fixture
def social():
    """Alice -KNOWS-> Bob, Alice -KNOWS-> Carol."""
    g = Graph.new()
    alice = g.add_node("Person", {"name": "Alice"})
    bob = g.add_node("Person", {"name": "Bob"})
    carol = g.add_node("Person", {"name": "Carol"})
    g.add_edge("KNOWS", alice, bob, {"strength": 10})
    g.add_edge("KNOWS", alice, carol, {"strength": 5})
    return g, alice, bob, carol


# ── Basic pattern matching ───────────────────────────────────────────────────


def test_pattern_simple_node_match(social):
    g, _, _, _ = social
    results = g.pattern().node("a").label("Person").execute()
    assert len(results) == 3
    assert all(isinstance(m, PatternMatch) for m in results)


def test_pattern_match_get_node(social):
    g, _, _, _ = social
    results = g.pattern().node("a").label("Person").execute()
    node = results[0].get_node("a")
    assert isinstance(node, Node)
    assert node.label() == "Person"


def test_pattern_match_get_node_not_found(social):
    g, _, _, _ = social
    results = g.pattern().node("a").label("Person").execute()
    with pytest.raises(TesseraError):
        results[0].get_node("nonexistent")


def test_pattern_node_edge_node(social):
    g, alice, bob, carol = social
    results = (
        g.pattern()
        .node("a")
        .edge(Direction.OUTGOING)
        .label("KNOWS")
        .node("b")
        .execute()
    )
    assert len(results) == 2  # alice->bob, alice->carol
    a_ids = {m.get_node("a").id() for m in results}
    assert a_ids == {alice}
    b_ids = {m.get_node("b").id() for m in results}
    assert bob in b_ids
    assert carol in b_ids


def test_pattern_with_edge_var(social):
    g, _, _, _ = social
    results = (
        g.pattern()
        .node("a")
        .edge_var("r", Direction.OUTGOING)
        .label("KNOWS")
        .node("b")
        .execute()
    )
    assert len(results) == 2
    edge = results[0].get_edge("r")
    assert isinstance(edge, Edge)
    assert edge.label() == "KNOWS"


def test_pattern_where_prop_filter(social):
    g, _, _, _ = social
    results = (
        g.pattern()
        .node("a")
        .label("Person")
        .where_prop("name", "Alice")
        .execute()
    )
    assert len(results) == 1
    assert results[0].get_node("a").properties()["name"] == "Alice"


def test_pattern_direction_string(social):
    g, _, _, _ = social
    results = (
        g.pattern()
        .node("a")
        .edge("outgoing")
        .label("KNOWS")
        .node("b")
        .execute()
    )
    assert len(results) == 2


def test_pattern_no_match_returns_empty(social):
    g, _, _, _ = social
    results = g.pattern().node("x").label("NonExistent").execute()
    assert results == []


def test_pattern_match_repr(social):
    g, _, _, _ = social
    results = g.pattern().node("a").label("Person").execute()
    r = repr(results[0])
    assert "PatternMatch" in r
