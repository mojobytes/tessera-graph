"""Phase 4: Direction and Strategy enums with string coercion."""

import pytest
from tessera_graph import Direction, Strategy, TesseraError


# ── Direction ────────────────────────────────────────────────────────────────


def test_direction_enum_values():
    assert Direction.OUTGOING is not None
    assert Direction.INCOMING is not None
    assert Direction.BOTH is not None


def test_direction_from_string_case_insensitive():
    assert Direction.from_str("outgoing") == Direction.OUTGOING
    assert Direction.from_str("INCOMING") == Direction.INCOMING
    assert Direction.from_str("Both") == Direction.BOTH


def test_direction_from_string_invalid():
    with pytest.raises(TesseraError):
        Direction.from_str("invalid")


def test_direction_repr():
    assert repr(Direction.OUTGOING) == "Direction.OUTGOING"
    assert repr(Direction.INCOMING) == "Direction.INCOMING"
    assert repr(Direction.BOTH) == "Direction.BOTH"


def test_direction_eq():
    assert Direction.OUTGOING == Direction.OUTGOING
    assert Direction.OUTGOING != Direction.INCOMING


def test_direction_hash():
    s = {Direction.OUTGOING, Direction.INCOMING, Direction.OUTGOING}
    assert len(s) == 2


# ── Strategy ─────────────────────────────────────────────────────────────────


def test_strategy_enum_values():
    assert Strategy.BFS is not None
    assert Strategy.DFS is not None


def test_strategy_from_string_case_insensitive():
    assert Strategy.from_str("bfs") == Strategy.BFS
    assert Strategy.from_str("DFS") == Strategy.DFS


def test_strategy_from_string_invalid():
    with pytest.raises(TesseraError):
        Strategy.from_str("invalid")


def test_strategy_repr():
    assert repr(Strategy.BFS) == "Strategy.BFS"
    assert repr(Strategy.DFS) == "Strategy.DFS"


def test_strategy_eq():
    assert Strategy.BFS == Strategy.BFS
    assert Strategy.BFS != Strategy.DFS


def test_strategy_hash():
    s = {Strategy.BFS, Strategy.DFS, Strategy.BFS}
    assert len(s) == 2
