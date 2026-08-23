"""Phase 8: SharedGraph with read/write context managers and threading."""

import threading
import pytest
from ermya_graph import Graph, SharedGraph, NodeId


def test_shared_graph_new():
    g = Graph.new()
    sg = SharedGraph.new(g)
    assert isinstance(sg, SharedGraph)


def test_shared_graph_write_context_manager():
    sg = SharedGraph.new(Graph.new())
    with sg.write() as g:
        g.add_node("N", {})
    with sg.read() as g:
        assert g.node_count() == 1


def test_shared_graph_read_context_manager():
    sg = SharedGraph.new(Graph.new())
    with sg.write() as g:
        g.add_node("N", {})
    with sg.read() as g:
        assert g.node_count() == 1
        ids = g.node_ids()
        assert len(ids) == 1


def test_shared_graph_read_guard_exposes_read_api():
    sg = SharedGraph.new(Graph.new())
    with sg.write() as g:
        nid = g.add_node("Person", {"name": "Alice"})
    with sg.read() as g:
        assert g.node_count() == 1
        assert g.edge_count() == 0
        assert g.node_exists(nid)
        node = g.node(nid)
        assert node.label() == "Person"
        ids = g.node_ids()
        assert len(ids) == 1
        by_label = g.nodes_by_label("Person")
        assert len(by_label) == 1


def test_shared_graph_write_guard_exposes_mutating_api():
    sg = SharedGraph.new(Graph.new())
    with sg.write() as g:
        a = g.add_node("A", {})
        b = g.add_node("B", {})
        eid = g.add_edge("R", a, b, {})
        g.remove_edge(eid)
        g.remove_node(b)
    with sg.read() as g:
        assert g.node_count() == 1
        assert g.edge_count() == 0


def test_shared_graph_concurrent_reads():
    sg = SharedGraph.new(Graph.new())
    with sg.write() as g:
        for i in range(10):
            g.add_node("N", {"i": i})

    results = []
    errors = []

    def reader():
        try:
            with sg.read() as g:
                results.append(g.node_count())
        except Exception as e:
            errors.append(e)

    threads = [threading.Thread(target=reader) for _ in range(4)]
    for t in threads:
        t.start()
    for t in threads:
        t.join(timeout=5.0)

    assert not errors, f"Errors in reader threads: {errors}"
    assert all(r == 10 for r in results)


def test_shared_graph_write_is_exclusive():
    """Writes from different threads see consistent state."""
    sg = SharedGraph.new(Graph.new())
    errors = []

    def writer(n):
        try:
            for _ in range(10):
                with sg.write() as g:
                    g.add_node("N", {})
        except Exception as e:
            errors.append(e)

    threads = [threading.Thread(target=writer, args=(i,)) for i in range(3)]
    for t in threads:
        t.start()
    for t in threads:
        t.join(timeout=10.0)

    assert not errors, f"Errors in writer threads: {errors}"
    with sg.read() as g:
        assert g.node_count() == 30  # 3 threads * 10 nodes each


def test_shared_graph_repr():
    sg = SharedGraph.new(Graph.new())
    r = repr(sg)
    assert "SharedGraph" in r
