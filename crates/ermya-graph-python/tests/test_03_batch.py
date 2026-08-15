"""Phase 3: Batch context manager and manual begin/end_batch."""

import pytest
from ermya_graph import Graph


@pytest.fixture
def g():
    return Graph.new()


def test_begin_end_batch_manual(g):
    g.begin_batch()
    g.add_node("N", {})
    g.end_batch()
    assert g.node_count() == 1


def test_batch_context_manager_commits_on_exit(g):
    with g.batch():
        g.add_node("N", {})
    assert g.node_count() == 1


def test_batch_context_manager_calls_end_batch_on_exception(g):
    """end_batch is called even if the body raises; graph remains usable."""
    with pytest.raises(ValueError):
        with g.batch():
            g.add_node("N", {})
            raise ValueError("boom")
    # Graph is still usable after the failed batch
    assert g.node_count() == 1
    with g.batch():
        g.add_node("M", {})
    assert g.node_count() == 2


def test_nested_batch_context_managers(g):
    with g.batch():
        with g.batch():
            g.add_node("N", {})
    assert g.node_count() == 1


def test_batch_multiple_operations(g):
    with g.batch():
        a = g.add_node("A", {})
        b = g.add_node("B", {})
        g.add_edge("R", a, b, {})
    assert g.node_count() == 2
    assert g.edge_count() == 1


def test_batch_context_manager_returns_none(g):
    """The context manager __enter__ returns None (not the graph)."""
    with g.batch() as ctx:
        # ctx should be None — the graph is used directly
        assert ctx is None
        g.add_node("N", {})
    assert g.node_count() == 1
