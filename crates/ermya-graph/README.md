# ermya-graph

An embeddable graph database for Rust — no server, no schema, no external
infrastructure. `ermya-graph` is a library you link into your process: it
stores a property graph, persists it to disk, and answers Cypher queries, all
in-process.

## What you get

- **Property graph storage** — labelled nodes and relationships with typed
  properties, backed by a page-based store with a buffer pool and a
  write-ahead log.
- **Embedded Cypher** — parse and execute read *and* write Cypher
  (`MATCH`, `CREATE`, `SET`, `MERGE`, `DELETE`, `UNWIND`, transactions) directly
  against the in-process graph. No query server to run.
- **Transactions** — MVCC-based isolation for concurrent readers and writers.
- **No schema step** — create nodes and relationships as you go.

## Quick start

```toml
[dependencies]
ermya-graph = "0.13"
```

```rust
use ermya_graph::{Graph, GraphConfig};

// Open (or create) a file-backed graph.
let graph = Graph::open("./my-graph", &GraphConfig::default())?;

// Match nodes with the pattern builder.
let people = graph
    .pattern()
    .node("p")
    .label("Person")
    .execute()?
    .collect::<ermya_graph::Result<Vec<_>>>()?;
```

For Cypher, the `gql` module houses the parser and executor: `gql::parse`
turns a query string into an AST and `gql::execute` runs it against anything
implementing `GraphAccess` (which `Graph` does). See the API docs on
[docs.rs](https://docs.rs/ermya-graph) for the exact signatures.

## When to reach for it

Use `ermya-graph` when you want graph storage and Cypher *inside* your
application — a desktop app, a CLI, a service that needs local graph state —
without standing up and operating a separate graph database.

If you need a networked server (Bolt protocol, multi-database, authentication,
authorization), those live in the ErmyaGraph server editions, not in this
crate.

## License

Licensed under the [MIT License](LICENSE).

Part of [ErmyaGraph](https://ermya-vector.io).
