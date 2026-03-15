# TesseraGraph Enterprise — Roadmap

**Product**: Enterprise graph database built on [tessera-graph](https://github.com/MojoBytes/tessera-graph) (MIT)
**License**: Proprietary — Copyright 2026 BelowZero Security OU
**Competitive Reference**: Memgraph (C++ in-memory graph DB)

## Open-Core Model

```
tessera-graph (MIT, embeddable library)
    └── tessera-graph-enterprise (proprietary, server binary)
            ├── tessera-server              TCP/WebSocket server
            ├── tessera-protocol            Wire protocol definitions
            ├── tessera-auth                Authentication, RBAC, LBAC
            ├── tessera-storage-enterprise   MVCC, transactions, snapshots
            ├── tessera-import              CSV, JSON, GQL import/export, SQL migration
            ├── tessera-streaming           Kafka, Pulsar, Redpanda connectors
            ├── tessera-monitor             WebSocket monitoring server
            ├── tessera-audit               Activity logging
            ├── tessera-config              Configuration management
            ├── tessera-replication          HA, leader-follower, Raft
            └── tessera-tenant              Multi-tenant architecture
```

### MIT vs Enterprise Boundary

| MIT (tessera-graph) | Enterprise (tessera-graph-enterprise) |
|---|---|
| In-memory and file-backed storage | MVCC, snapshot isolation, concurrent access |
| WAL crash recovery | Online backup and scheduled snapshots |
| Label index | Property indexes, B-tree on-disk indexes |
| Query primitives (traversal, pathfinding) | Query optimizer, prepared statements, EXPLAIN |
| Pattern builder (Layer 2) | Mutations via GQL, Cypher compat mode |
| GQL parser — read-only queries (Layer 3) | SQL/PGQ bridge |
| | Authentication (native, LDAP, external modules) |
| | RBAC and LBAC |
| | SSL/TLS |
| | Audit logging |
| | Replication (HA) |
| | Multi-tenancy |
| | Streaming connectors |
| | Monitoring server |

---

## Competitive Positioning vs Memgraph

| Dimension | Memgraph | TesseraGraph Enterprise |
|---|---|---|
| Language | C++ | Rust (zero unsafe) |
| Core license | BSL (was Apache 2.0) | MIT |
| Query language | Cypher + proprietary extensions (MAGE) | GQL (ISO 39075) + Cypher compat mode |
| Memory safety | Manual (C++) | Compile-time guaranteed |
| Graph algorithms | MAGE library | Future phase |
| Streaming | Kafka/Pulsar native | Kafka/Pulsar/Redpanda (Phase 3) |

### Differentiators

1. **GQL standard (ISO/IEC 39075:2024)** — Memgraph only supports Cypher, no ISO standard compliance
2. **Rust with `forbid(unsafe_code)`** — Memory safety without garbage collection, no undefined behavior
3. **MIT core** — Truly open source, not BSL with delayed open-source conversion
4. **Cypher compatibility mode** — Migration path from Memgraph/Neo4j while offering GQL

---

## Phase Plan

### PHASE 1: Server Foundation & Storage Enterprise (P0)

#### 1.1 — Cargo Workspace ✅
- 11-crate workspace (Rust 2024, MSRV 1.85)
- Path dependency on tessera-graph
- Workspace-level lints: `forbid(unsafe_code)`, `deny(clippy::all)`

#### 1.2 — Concurrency & Transactions ✅
- Make `Graph` `Send + Sync` with granular locking (RwLock per page/segment)
- Transaction manager: Begin/Commit/Rollback with WAL integration
- Isolation levels:
  - Read Uncommitted (implicit, no lock)
  - Read Committed (reads see only committed data)
  - Snapshot Isolation via MVCC (version chains in slots, visibility map)

#### 1.3 — Backup & Recovery ✅
- Online snapshot: freeze pages → copy files → resume
- WAL tail copy for point-in-time recovery
- Restore: snapshot replay + WAL replay
- Scheduled backups via configuration

### PHASE 1.5: GQL Query Language Engine (P0)

#### 1.5.1 — GQL Parser ✅ (in MIT core)
- Full GQL parser (ISO/IEC 39075:2024)
- Lexer + typed AST
- Core clauses: `MATCH`, `RETURN`, `WHERE`, `ORDER BY`, `LIMIT`, `WITH`, `OPTIONAL MATCH`
- Path patterns: `(a)-[r]->(b)`, variable-length `(a)-[*1..5]->(b)`, quantified paths
- Expressions: arithmetic, boolean, string functions, aggregations (`COUNT`, `SUM`, `AVG`, `MIN`, `MAX`, `COLLECT`)
- Multi-label support
- Subqueries: `CALL { ... }`, `EXISTS { ... }`

#### 1.5.2 — GQL Planner / Compiler ✅ (in MIT core, EXPLAIN pending)
- Compile GQL AST → Layer 2 operations (PatternBuilder)
- Basic query optimizer:
  - Predicate reordering (most selective first)
  - Label index usage when available
  - WHERE filter push-down into pattern matching
- `EXPLAIN` for execution plan inspection

#### 1.5.3 — Mutations via GQL ✅ (enterprise-only)
- `CREATE (n:Label {props})` → `graph.add_node()`
- `CREATE (a)-[:REL {props}]->(b)` → `graph.add_edge()`
- `SET n.prop = value` → `graph.update_node()`
- `DELETE n` / `DETACH DELETE n` → `graph.remove_node()` with edge cleanup
- `MERGE` (create if not exists)

#### 1.5.4 — Cypher Compatibility Mode (P1) ✅
- Accept Cypher syntax where it diverges from GQL
- Configuration flag: `query_language: gql | cypher-compat | strict-gql`
- Migration path from Neo4j/Memgraph
- Implemented: backticks, block comments, STARTS WITH, ENDS WITH, CONTAINS, IN, id(), type(), labels()
- Deferred to 1.5.5+: REMOVE, OPTIONAL MATCH, WITH, UNWIND

#### 1.5.5 — SQL/PGQ Bridge (P3)
- Subset of SQL/PGQ (ISO 9075-16) for SQL-familiar users
- `SELECT ... FROM GRAPH_TABLE ( g MATCH (a)-[r]->(b) COLUMNS (a.name, r.weight) )`
- Enables BI tool integration

### PHASE 2: Security (P0/P1)

#### 2.1 — Native Authentication (P0)
- Internal user store (Argon2id password hashing)
- Configurable password policies:
  - Min/max length
  - Require uppercase, numbers, symbols
  - Password history (no reuse)
  - Expiration and forced rotation
- Session management with temporary tokens

#### 2.2 — LDAP Integration (P2)
- Bind against external LDAP/AD server
- LDAP group → internal role mapping
- Fallback to native auth if LDAP unavailable

#### 2.3 — External Auth Modules (P2)
- Trait/interface for pluggable authentication modules
- OIDC/OAuth2 support as external module
- Plugin API with stable ABI (C FFI or WASM)

#### 2.4 — RBAC (P0)
- Predefined roles: `admin`, `readwrite`, `readonly`, `monitor`
- Custom roles with granular permissions:
  - `node:create`, `node:read`, `node:update`, `node:delete`
  - `edge:create`, `edge:read`, `edge:update`, `edge:delete`
  - `graph:flush`, `graph:backup`, `graph:config`
  - `admin:users`, `admin:roles`, `admin:audit`

#### 2.5 — LBAC (P2)
- Security labels on nodes and edges
- User clearance levels
- Invisible query-level filtering (user only sees data at or below their clearance)
- Integrated into query engine

#### 2.6 — SSL/TLS (P0)
- TLS 1.3 for all network connections
- Configurable certificates (PEM)
- Optional mTLS for client certificate auth

#### 2.7 — Activity Auditing (P1)
- Immutable log of all operations:
  - Who (user), when (timestamp), what (operation), target (IDs), result (success/denied)
- Separate from WAL (not truncated on checkpoint)
- Exportable, rotatable, configurable retention

### PHASE 3: Import/Export & Migration (P2/P3)

#### 3.1 — CSV Import (P2)
- Streaming parser (not full in-memory load)
- Configurable column mapping → labels, properties
- Relationship support (source/target columns)
- Per-row validation and error reporting

#### 3.2 — JSON Import/Export (P2)
- Full graph or subgraph import/export
- Custom documented format + GraphSON compatibility
- Streaming for large datasets

#### 3.3 — GQL Import/Export (P2)
- Export: dump graph as GQL `CREATE` statement sequence
- Import: streaming parser of GQL scripts (CREATE/MERGE)
- Cypher compat: accept Neo4j/Memgraph dumps in `cypher-compat` mode
- `.gql` file extension

#### 3.4 — SQL Migration Tool (P3)
- Connectors: PostgreSQL, MySQL, SQLite
- Configurable mapping: tables → nodes, foreign keys → edges
- Automatic schema inference with manual override
- Optional incremental migration (change data capture)

### PHASE 4: Streaming & Connectivity (P1/P2/P3)

#### 4.1 — Connection Protocol (P1)
- Custom binary protocol over TCP (framing + commands)
- GQL queries as text + parameters
- Prepared statements: parse once, execute many
- Result streaming (column-oriented records)
- Rust client driver (publishable as crate)

#### 4.2 — Kafka/Pulsar/Redpanda Integration (P3)
- Consumer: ingest messages as nodes/edges (configurable mapping)
- Producer: emit change events (CDC) to stream
- Exactly-once semantics where broker supports it
- Per-tenant configuration in multi-tenant mode

#### 4.3 — WebSocket Monitoring Server (P2)
- Real-time metrics: ops/sec, latency, buffer pool hit rate, WAL size
- Replication status
- Active queries
- Optional embedded web dashboard (feature flag)

### PHASE 5: High Availability & Multi-Tenancy (P3)

#### 5.1 — Replication
- Leader-follower with WAL shipping
- Automatic failover (Raft consensus for leader election)
- Read replicas for read scaling
- Configuration: sync vs async replication
- Leverages existing `_wal_reserved: u16` field for LSN

#### 5.2 — Multi-Tenant Architecture
- Tenant isolation at storage level (separate databases per tenant)
- Connection routing by tenant ID
- Per-tenant quotas: memory, storage, ops/sec
- Admin API for tenant management (CRUD, suspend, migrate)
- Per-tenant user/role/label namespace

---

## Priority Summary

| Priority | Components | Justification |
|----------|-----------|---------------|
| **P0** | Workspace, concurrency, transactions, GQL (parser+compiler+mutations), native auth, RBAC, SSL/TLS | Minimum viable enterprise product |
| **P1** | MVCC/snapshot isolation, backup/snapshots, Cypher compat, auditing, TCP protocol | Production readiness |
| **P2** | CSV/JSON/GQL import/export, LDAP, external auth, LBAC, monitoring WebSocket | Enterprise sales enablement |
| **P3** | SQL/PGQ bridge, SQL migration, streaming, replication (HA), multi-tenancy | Scale and ecosystem integration |

---

## Benchmarking Strategy

### Competitive Reference: Memgraph

All benchmarks must produce results directly comparable to Memgraph's published performance numbers.

### Benchmark Dimensions

| Dimension | Metric | Target |
|-----------|--------|--------|
| Write throughput | nodes/s, edges/s (bulk ingestion) | Match or exceed Memgraph |
| Read latency | p50, p95, p99 (point lookups, traversals) | Sub-millisecond target |
| Traversal throughput | queries/s (BFS/DFS at varying depths) | Comparable to Memgraph |
| Pathfinding | shortest path latency (1K–10M nodes) | Comparable to Memgraph |
| Streaming ingestion | events/s from Kafka | Comparable to Memgraph |
| Concurrent workload | throughput under N clients (mixed R/W) | Linear scaling |

### Standard Datasets

- **LDBC Social Network Benchmark (SNB)** — Industry standard for graph database comparison. Primary benchmark.
- **Synthetic scale-free graphs** — Barabási–Albert model for controlled scalability testing
- **Real-world datasets** — Pokec social network, LiveJournal, Twitter follows

### Phased Benchmark Development

| Phase | Benchmark Scope | Tools |
|-------|----------------|-------|
| Phase 1 (storage) | Write/read/traversal microbenchmarks | Criterion (extends existing tessera-graph suite) |
| Phase 1.5 (GQL) | Query latency: GQL vs direct API overhead | Criterion |
| Phase 4 (protocol) | End-to-end client-server benchmarks | Custom harness, comparable to Memgraph published numbers |
| Phase 5 (HA) | Concurrent multi-client benchmarks | Custom harness with configurable client count |

### LDBC SNB Workloads

The LDBC Social Network Benchmark defines standardized workloads:
- **Interactive Short** — Simple lookups (person by ID, friends of person)
- **Interactive Complex** — Multi-hop traversals with filters and aggregations
- **Business Intelligence** — Analytical queries on the full graph
- **Interactive Update** — Mixed read/write under concurrent load

Implementing LDBC SNB compliance allows direct, auditable comparison with Memgraph and other graph databases (Neo4j, Amazon Neptune, TigerGraph).
