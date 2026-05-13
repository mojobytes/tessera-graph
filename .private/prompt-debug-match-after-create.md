# Debug: MATCH returns 0 rows after CREATE on same Bolt session

**Date**: 2026-04-05
**Repo**: `tessera-graph-enterprise`
**Severity**: Bloqueante para benchmark comparison y TesseraBoltTarget integration tests

---

## Symptom

On the same Bolt 4.4 connection:
1. `RUN "CREATE (:N)"` + `PULL` → server returns SUCCESS with `nodes_created: 1`
2. `RUN "MATCH (n) RETURN id(n) AS nid ORDER BY id(n) ASC"` + `PULL` → returns **0 rows**

This happens consistently, not intermittently. The `BoltClient::run_query`
sends `RUN` + `PULL` directly (auto-commit mode, NO `BEGIN`).

## What we ruled out

- **`BEGIN` causing FAILED state**: `BoltClient::run_query` does NOT send
  `BEGIN`. Verified in `tessera-graph-protocol/src/bolt_client.rs:158-195`.
- **Different graph instances**: `self.graph` is set once during HELLO
  (line 294 of bolt_handler.rs) as `Arc<RwLock<Graph>>`. Both mutation
  (write lock, line 499) and query (read lock, line 487) use the same Arc.
- **Lock poisoning**: Would produce an explicit error, not 0 rows.
- **Deferred flush**: Irrelevant — the data is in memory (in the Graph
  struct), not on disk. The read lock sees the in-memory state.

## Where to investigate

### 1. Verify the mutation actually executes

Add `tracing::info!` in `bolt_handler.rs` at line 503 (after `execute_mut`):
```rust
let r = tessera_graph_storage::gql::execute_mut(&mut secure, m);
tracing::info!("mutation result: {r:?}");
```

### 2. Verify the query sees nodes

Add `tracing::info!` in `bolt_handler.rs` at line 491 (after `execute`):
```rust
let result = tessera_graph::gql::execute(&secure, q);
tracing::info!("query result rows: {}", result.as_ref().map(|r| r.len()).unwrap_or(0));
```

### 3. Check if `id()` function works at all

The query uses `id(n)` which requires `extended-gql`. The workspace enables
this feature. But verify:
- Does `MATCH (n) RETURN n.label` also return 0 rows? (no `id()` needed)
- Does `MATCH (n:N) RETURN n` return 0 rows? (label filter)

### 4. Check LBAC clearance

`SecureGraph` wraps the mutation (line 502). `SecureGraphRef` wraps the
query (line 490). Both use `clearance` derived from the session. If the
CREATE injects a security label that the subsequent read can't see, nodes
would be invisible.

Check `SecureGraph::add_node` in `lbac.rs` — it calls
`SecurityPolicy::inject_label(&mut properties, &caller_label)`. The node
gets the caller's clearance level. Then `SecureGraphRef::node_ids` filters
via `can_read_props(clearance, node.properties())`. If the injected label
doesn't match the reader's clearance, the node is filtered out.

**Key question**: is the clearance the same for both the write and the read?
Both use `self.clearance()` (line 445 of bolt_handler.rs). If the session
clearance changes between queries (it shouldn't), that would explain it.

### 5. Check if `node_ids()` returns the created node

In `SecureGraphRef::node_ids` (lbac.rs), add a debug trace:
```rust
fn node_ids(&self) -> Vec<NodeId> {
    let all = self.inner.node_ids();
    let filtered = filter::secure_node_ids(self.inner, &self.clearance);
    tracing::debug!("node_ids: all={}, filtered={}", all.len(), filtered.len());
    filtered
}
```

If `all > 0` but `filtered == 0`, the LBAC filter is hiding the nodes.

### 6. Simplest reproduction

Write a Bolt handler integration test in `crates/tessera-graph-server/tests/`:
```rust
#[tokio::test]
async fn create_then_match_returns_created_node() {
    let (mut writer, mut reader, _shutdown, _dir) = setup_authenticated().await;
    
    // CREATE
    bolt_send(&mut writer, &BoltRequest::Run {
        query: "CREATE (:TestNode {name: 'hello'})".to_owned(),
        params: vec![], extra: vec![],
    }).await;
    let create_resp = bolt_recv(&mut reader).await;
    assert!(matches!(create_resp, BoltResponse::Success { .. }));
    bolt_send(&mut writer, &BoltRequest::Pull { extra: vec![] }).await;
    drain_pull(&mut reader).await; // consume summary row
    
    // MATCH
    bolt_send(&mut writer, &BoltRequest::Run {
        query: "MATCH (n:TestNode) RETURN n.name".to_owned(),
        params: vec![], extra: vec![],
    }).await;
    let match_resp = bolt_recv(&mut reader).await;
    assert!(matches!(match_resp, BoltResponse::Success { .. }));
    bolt_send(&mut writer, &BoltRequest::Pull { extra: vec![] }).await;
    
    // Should get at least 1 RECORD
    let first = bolt_recv(&mut reader).await;
    assert!(
        matches!(first, BoltResponse::Record { .. }),
        "MATCH after CREATE must return at least one record, got: {first:?}"
    );
}
```

Use the existing test helpers in `tests/common/mod.rs` (`spawn_bolt_handler`,
`bolt_send`, `bolt_recv`). These bypass Docker and test the handler directly.

## Files to read

- `crates/tessera-graph-server/src/bolt_handler.rs` — lines 419-530 (`handle_run`)
- `crates/tessera-graph-server/tests/common/mod.rs` — test helpers
- `crates/tessera-graph-server/tests/bolt_handler_test.rs` — existing tests for reference
- `crates/tessera-graph-storage/src/lbac.rs` — `SecureGraph`, `SecureGraphRef`, `filter::secure_node_ids`
- `crates/tessera-graph-auth/src/lbac.rs` — `Clearance`, `SecurityPolicy::inject_label`

## Expected outcome

Either:
1. Find the root cause and fix it (likely LBAC clearance mismatch or `id()` function bug)
2. Or confirm it works with the direct handler test and the bug is in `BoltClient` or the Docker setup
