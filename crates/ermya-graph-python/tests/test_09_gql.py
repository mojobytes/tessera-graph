"""Phase 9: GQL execute, validate, GqlResult/GqlRow."""

import pytest
from ermya_graph import (
    Graph,
    GqlResult,
    GqlRow,
    ErmyaError,
    GqlSyntaxError,
    execute,
    validate,
)


@pytest.fixture
def g():
    g = Graph.new()
    g.add_node("Person", {"name": "Alice", "age": 30})
    g.add_node("Person", {"name": "Bob", "age": 25})
    return g


# ── execute ──────────────────────────────────────────────────────────────────


def test_gql_execute_simple_match(g):
    result = execute(g, "MATCH (a:Person) RETURN a.name")
    assert isinstance(result, GqlResult)
    assert len(result) == 2


def test_gql_result_is_iterable(g):
    result = execute(g, "MATCH (a:Person) RETURN a.name")
    rows = list(result)
    assert len(rows) == 2
    assert all(isinstance(r, GqlRow) for r in rows)


def test_gql_row_getitem(g):
    result = execute(g, "MATCH (a:Person) RETURN a.name")
    names = {result.rows[i]["a.name"] for i in range(len(result))}
    assert names == {"Alice", "Bob"}


def test_gql_row_getitem_missing_key_raises(g):
    result = execute(g, "MATCH (a:Person) RETURN a.name")
    with pytest.raises(KeyError):
        result.rows[0]["nonexistent"]


def test_gql_result_len(g):
    result = execute(g, "MATCH (a:Person) RETURN a.name")
    assert len(result) == 2


def test_gql_result_bool_nonempty(g):
    result = execute(g, "MATCH (a:Person) RETURN a.name")
    assert bool(result) is True


def test_gql_result_bool_empty(g):
    result = execute(g, "MATCH (a:Person {name: 'Nobody'}) RETURN a.name")
    assert bool(result) is False
    assert len(result) == 0


def test_gql_value_types(g):
    result = execute(g, "MATCH (a:Person) WHERE a.name = 'Alice' RETURN a.name, a.age")
    row = result.rows[0]
    assert isinstance(row["a.name"], str)
    assert isinstance(row["a.age"], int)


def test_gql_value_null():
    g = Graph.new()
    g.add_node("X", {})
    result = execute(g, "MATCH (a:X) RETURN a.missing_prop")
    row = result.rows[0]
    assert row["a.missing_prop"] is None


def test_gql_syntax_error_raises():
    g = Graph.new()
    with pytest.raises(GqlSyntaxError):
        execute(g, "THIS IS NOT GQL")


def test_gql_syntax_error_is_ermya_error():
    g = Graph.new()
    with pytest.raises(ErmyaError):
        execute(g, "NOT VALID")


def test_gql_result_rows_property(g):
    result = execute(g, "MATCH (a:Person) RETURN a.name")
    assert isinstance(result.rows, list)
    assert all(isinstance(r, GqlRow) for r in result.rows)


def test_gql_row_keys(g):
    result = execute(g, "MATCH (a:Person) RETURN a.name, a.age")
    row = result.rows[0]
    keys = row.keys()
    assert "a.name" in keys
    assert "a.age" in keys


def test_gql_row_repr(g):
    result = execute(g, "MATCH (a:Person) RETURN a.name")
    r = repr(result.rows[0])
    assert "GqlRow" in r


def test_gql_result_repr(g):
    result = execute(g, "MATCH (a:Person) RETURN a.name")
    r = repr(result)
    assert "GqlResult" in r


def test_gql_where_filter(g):
    result = execute(g, "MATCH (a:Person) WHERE a.age > 27 RETURN a.name")
    assert len(result) == 1
    assert result.rows[0]["a.name"] == "Alice"


def test_gql_order_by(g):
    result = execute(g, "MATCH (a:Person) RETURN a.name ORDER BY a.name ASC")
    names = [r["a.name"] for r in result]
    assert names == ["Alice", "Bob"]


def test_gql_limit(g):
    result = execute(g, "MATCH (a:Person) RETURN a.name LIMIT 1")
    assert len(result) == 1


# ── validate ─────────────────────────────────────────────────────────────────


def test_validate_valid_query():
    assert validate("MATCH (a:Person) RETURN a.name") is True


def test_validate_valid_mutation():
    assert validate("CREATE (n:City {name: 'Madrid'})") is True


def test_validate_invalid_raises():
    with pytest.raises(GqlSyntaxError):
        validate("NOT VALID GQL")
