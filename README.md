# TesseraGraph Community

An embeddable graph database with a Bolt server.

This repository contains the **Community** edition: the graph engine, the full
query interpreter (reads, writes, and transactions), local username and
password authentication, a TLS-enabled Bolt server, cold backups, and basic
auditing.

## Status

Functional. The server and administration tool build and run, and the test
suite passes.

| Package | Description |
| --- | --- |
| `tessera-graph` | Graph engine: storage, indexes, and transactions |
| `tessera-graph-cypher` | Query language interpreter |
| `tessera-graph-protocol` | Bolt protocol encoding |
| `tessera-graph-server` | Bolt server (`tessera-graph-server` binary) |
| `tessera-graph-cli` | Administration tool (`tessera-graph-cli` binary) |
| `tessera-graph-config` | Configuration loader |
| `tessera-graph-python` | Python bindings |

Requirements: Rust 1.88 or later, edition 2024.

## Quick start

The server **requires TLS** and will not start without a certificate. This is
intentional: a database server that accepts plaintext connections by default
is a security flaw, not a convenience.

Generate a self-signed certificate for local testing:

```bash
openssl req -x509 -newkey rsa:4096 -nodes -days 365 \
  -keyout server.key -out server.crt \
  -subj "/CN=localhost"
```

Start the server:

```bash
export TESSERA_TLS_CERT=$PWD/server.crt
export TESSERA_TLS_KEY=$PWD/server.key
export TESSERA_DATA_DIR=$PWD/data
export TESSERA_BIND=127.0.0.1:7687
export TESSERA_PASSWORD='a-long-password'

cargo run --release --bin tessera-graph-server
```

Connect with any Bolt 4.4 client, such as the official Neo4j Python driver:

```python
from neo4j import GraphDatabase

driver = GraphDatabase.driver(
    "bolt+ssc://127.0.0.1:7687",  # +ssc accepts a self-signed certificate
    auth=("tessera", "a-long-password"),
)

with driver.session(database="neo4j") as session:
    session.run("CREATE (:Person {name: 'Ada'})")
    for record in session.run("MATCH (p:Person) RETURN p.name AS name"):
        print(record["name"])
```

## Differences from Neo4j

The server speaks Bolt 4.4 and works with the official Neo4j driver without
adapters, but the query interpreter is not a clone. It follows the GQL standard
more closely and requires some constructs that Neo4j infers to be explicit.
The behavior below has been verified against a running server rather than
derived from documentation.

### The database must be named when opening a session

Neo4j selects a default database; TesseraGraph rejects the first query if no
database is specified. The Community edition serves one database named
`neo4j`.

```python
driver.session(database="neo4j")  # required
```

### One write clause per query

Chaining multiple write clauses in one statement is a syntax error. Send them
as separate queries, or create relationships after matching the nodes.

```cypher
-- Not supported
CREATE (:Person {name: 'Ada'}) CREATE (:Person {name: 'Alan'})

-- Supported equivalent: two queries, with MATCH before relationship creation
CREATE (:Person {name: 'Ada'})
CREATE (:Person {name: 'Alan'})
MATCH (p:Person), (f:Field) CREATE (p)-[:WORKED_IN]->(f)
```

### Grouping requires an explicit `GROUP BY`

When a grouping value is mixed with an aggregate function, Neo4j infers the
grouping. TesseraGraph requires it to be written explicitly.

```cypher
-- Not supported
MATCH (p:Person)-[:WORKED_IN]->(f:Field) RETURN f.name, count(p)

-- Supported
MATCH (p:Person)-[:WORKED_IN]->(f:Field)
RETURN f.name AS field, count(p) AS total GROUP BY f.name
```

A standalone aggregate needs no additional syntax:
`MATCH (n) RETURN count(*)` works as written.

### Other interpreter restrictions

| Case | Behavior |
| --- | --- |
| `CREATE (:A:B)` | Multiple labels per node are not supported; use one label per node |
| `CREATE (a)<-[:R]-(b)` | `CREATE` only supports outgoing relationships (`-[:R]->`); reverse the pattern |
| `MATCH … CREATE … RETURN` | Not supported: nodes created this way are unavailable for projection. A standalone `CREATE (n) RETURN n` does work |
| `UNWIND` without a preceding scope | Requires a preceding construct: `UNWIND range(1,3) AS i CREATE (…)` works; a standalone `UNWIND [1,2,3] AS x RETURN x` does not |
| `SKIP` | Not recognized; `ORDER BY` and `LIMIT` are supported |
| `SHOW INDEXES`, `EXPLAIN` | Not recognized by the query interpreter |
| `CALL proc()` without `YIELD` | Procedure calls require `YIELD <column>` |

For parameter names, avoid reserved words. `$min` fails to parse because `min`
is an aggregate function; `$threshold` with the same value works. Both named
parameters (`$threshold`) and positional parameters (`$1`, numbered from one)
are supported in equality and inequality expressions.

### Behavior compatible with Neo4j

Variable-length traversals (`-[:R*1..2]->`), `OPTIONAL MATCH`, `WITH` pipelines,
`MERGE`, `SET`, `DETACH DELETE`, `ORDER BY … LIMIT`, `STARTS WITH`, list
predicates (`ALL`, `ANY`, `NONE`, and `SINGLE`), functions such as `toLower`
and `coalesce`, index creation, and explicit commit and rollback transactions
behave as they do in Neo4j.

Schema definition, procedure calls, and administration statements are not
allowed inside an explicit transaction. Commit or roll back the transaction
first.

## Configuration

Settings are read from `/etc/tessera/tessera.toml` by default. Environment
variables take precedence. The most commonly used variables are:

| Variable | Purpose |
| --- | --- |
| `TESSERA_BIND` | Listen address and port |
| `TESSERA_DATA_DIR` | Data directory |
| `TESSERA_TLS_CERT` / `TESSERA_TLS_KEY` | TLS certificate and key (required) |
| `TESSERA_PASSWORD` | Initial administrator password |
| `TESSERA_METRICS_ADDR` | Metrics listen address |
| `TESSERA_QUERY_TIMEOUT_MS` | Per-query timeout |
| `TESSERA_MAX_CONNECTIONS` | Maximum concurrent connections |
| `TESSERA_AUDIT_ENABLED` | Enables audit logging |

Around forty additional settings control resource limits, rate limiting, audit
log rotation, and background maintenance.

## Development

```bash
cargo check --workspace --all-targets
cargo test --workspace --exclude tessera-graph-python --features plain-tcp
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

The `plain-tcp` feature enables an unencrypted channel **reserved for
integration tests**. Without it, test binaries that require a direct connection
compile without those tests, allowing the suite to pass without exercising
them. Published binaries never enable this feature.

The Python bindings are tested on Python 3.9 and 3.12. To build and test them in
a virtual environment:

```bash
python3 -m venv .venv
.venv/bin/pip install 'maturin>=1.7,<2.0' pytest
VIRTUAL_ENV=$PWD/.venv .venv/bin/maturin develop --locked \
  --manifest-path crates/tessera-graph-python/Cargo.toml
.venv/bin/python -m pytest crates/tessera-graph-python/tests -q
```

A reproducible, isolated build is also available through Docker:

```bash
docker build --target test \
  -f crates/tessera-graph-python/Dockerfile .
```

Before a release, the pipeline runs `scripts/check-release.sh`. A `vX.Y.Z` tag
matching the workspace version triggers Community binary builds, Python wheel
builds, and packaging of the MIT-licensed crate. The pipeline publishes the
crate to crates.io through OIDC trusted publishing and attaches all artifacts
to a GitHub release. Changes are documented in
[CHANGELOG.md](CHANGELOG.md).

## Licenses

The licensing split between components is part of the product design:

- **Engine** (`tessera-graph`) and Python bindings: MIT, published as an
  independent embeddable graph package.
- **Community server and network components**: BSL 1.1. Internal production use
  is permitted, but DBaaS, redistribution/OEM, and competing products require a
  commercial agreement. Each version converts to Apache-2.0 four years after
  its release.

Enterprise edition features (fine-grained authorization, multiple databases
and tenants, compliance auditing, enterprise identity providers, and hot
backups) live in a separate repository and are not part of this one.

---

[tesseradb.io](https://tesseradb.io)
