"""Phase 1: Graph lifecycle, exceptions, NodeId/EdgeId, and properties round-trip."""

import os
import pytest


# ── Import tests ─────────────────────────────────────────────────────────────


def test_graph_new_returns_graph_object():
    from ermya_graph import Graph

    g = Graph.new()
    assert g.node_count() == 0
    assert g.edge_count() == 0


def test_graph_open_creates_dir_when_create_if_missing_true(tmp_path):
    from ermya_graph import Graph

    path = str(tmp_path / "test_store")
    g = Graph.open(path, create_if_missing=True)
    assert g.node_count() == 0


def test_graph_open_raises_ermya_error_when_dir_missing_and_flag_false():
    from ermya_graph import Graph, ErmyaError

    with pytest.raises(ErmyaError):
        Graph.open("/nonexistent/path/that/does/not/exist", create_if_missing=False)


def test_graph_flush_does_not_raise_on_memory_graph():
    from ermya_graph import Graph

    g = Graph.new()
    g.flush()  # Should not raise


def test_graph_open_keyword_args_defaults(tmp_path):
    from ermya_graph import Graph

    path = str(tmp_path / "defaults")
    # All defaults
    g = Graph.open(path)
    assert g.node_count() == 0

    # Explicit WAL disabled
    path2 = str(tmp_path / "no_wal")
    g2 = Graph.open(path2, wal_enabled=False)
    assert g2.node_count() == 0

    # Explicit memory limit
    path3 = str(tmp_path / "small_mem")
    g3 = Graph.open(path3, memory_limit_bytes=1_048_576)
    assert g3.node_count() == 0


# ── Exception hierarchy ──────────────────────────────────────────────────────


def test_exception_hierarchy():
    from ermya_graph import (
        ErmyaError,
        NodeNotFoundError,
        EdgeNotFoundError,
        GqlSyntaxError,
    )

    assert issubclass(NodeNotFoundError, ErmyaError)
    assert issubclass(EdgeNotFoundError, ErmyaError)
    assert issubclass(GqlSyntaxError, ErmyaError)
    assert issubclass(ErmyaError, Exception)


def test_node_not_found_is_catchable_as_ermya_error():
    from ermya_graph import Graph, NodeId, ErmyaError, NodeNotFoundError

    g = Graph.new()
    with pytest.raises(ErmyaError):
        g.node(NodeId(99999))
    with pytest.raises(NodeNotFoundError):
        g.node(NodeId(99999))


# ── NodeId ───────────────────────────────────────────────────────────────────


def test_node_id_wraps_u64():
    from ermya_graph import NodeId

    nid = NodeId(42)
    assert nid.value == 42
    assert isinstance(nid.value, int)


def test_node_id_repr():
    from ermya_graph import NodeId

    assert repr(NodeId(7)) == "NodeId(7)"


def test_node_id_eq():
    from ermya_graph import NodeId

    assert NodeId(1) == NodeId(1)
    assert NodeId(1) != NodeId(2)


def test_node_id_hash():
    from ermya_graph import NodeId

    s = {NodeId(1), NodeId(2), NodeId(1)}
    assert len(s) == 2


def test_node_id_int_conversion():
    from ermya_graph import NodeId

    assert int(NodeId(42)) == 42


# ── EdgeId ───────────────────────────────────────────────────────────────────


def test_edge_id_wraps_u64():
    from ermya_graph import EdgeId

    eid = EdgeId(10)
    assert eid.value == 10
    assert isinstance(eid.value, int)


def test_edge_id_repr():
    from ermya_graph import EdgeId

    assert repr(EdgeId(3)) == "EdgeId(3)"


def test_edge_id_eq():
    from ermya_graph import EdgeId

    assert EdgeId(1) == EdgeId(1)
    assert EdgeId(1) != EdgeId(2)


def test_edge_id_hash():
    from ermya_graph import EdgeId

    s = {EdgeId(1), EdgeId(2), EdgeId(1)}
    assert len(s) == 2


def test_edge_id_int_conversion():
    from ermya_graph import EdgeId

    assert int(EdgeId(99)) == 99


# ── Properties round-trip (basic — full CRUD tested in Phase 2) ──────────────


def test_add_node_and_retrieve_properties():
    from ermya_graph import Graph

    g = Graph.new()
    nid = g.add_node("Person", {"name": "Alice", "age": 30})
    node = g.node(nid)
    assert node.label() == "Person"
    props = node.properties()
    assert props["name"] == "Alice"
    assert props["age"] == 30


def test_property_types_roundtrip():
    from ermya_graph import Graph

    g = Graph.new()
    props = {
        "s": "hello",
        "i": 42,
        "f": 3.14,
        "b": True,
        "raw": b"\x00\x01\x02",
    }
    nid = g.add_node("Test", props)
    got = g.node(nid).properties()
    assert got["s"] == "hello"
    assert got["i"] == 42
    assert got["f"] == 3.14
    assert got["b"] is True
    assert got["raw"] == b"\x00\x01\x02"


def test_wrong_property_type_raises_type_error():
    from ermya_graph import Graph

    g = Graph.new()
    with pytest.raises(TypeError):
        g.add_node("Bad", {"obj": object()})
