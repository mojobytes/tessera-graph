# TDD Plan: LBAC Server Wiring — SecureGraph in tessera-server

**Created**: 2026-03-23
**Status**: Pending — ready for implementation
**Depends on**: LBAC implementation (complete), GraphAccess trait (complete)

---

## Contexto

LBAC (Bell-LaPadula with compartments) is fully implemented in `tessera-auth::lbac` and `tessera-storage-enterprise::lbac` but has zero presence in the server query path. `handle_query` currently passes raw `Graph` pointers directly to `gql::execute` (reads) and `execute_mut` (mutations), bypassing all clearance filtering.

The critical structural problem is that the existing `SecureGraph<'g, G>` holds `&'g mut G`, so it cannot wrap the `RwLockReadGuard<Graph>` produced by the read path (which gives only `&Graph`). Solution: a `SecureGraphRef<'g, G>` holding `&'g G` for reads.

**Stack**: Rust 1.85, Edition 2024, `thiserror` v2, `clippy all=deny/pedantic=warn/nursery=warn`, `unsafe_code=forbid`
**Conventions**: Copyright `// Copyright 2026 BelowZero Security OU. All rights reserved.`, tests in `crates/<crate>/tests/`, dual thresholds with `cfg!(debug_assertions)`
**Affects hot path**: YES — `handle_query` is called on every client request

## Decisiones Previas Necesarias

Ninguna — la arquitectura está cerrada.

---

## Plan de Ejecución

### Ciclo 1: Extraer helpers compartidos a `lbac::filter`

**Motivación**: Tanto `SecureGraph` como el futuro `SecureGraphRef` necesitan idéntica lógica de `can_read`, `strip_node`, `strip_edge`, `edge_visible_for`. Extraer antes de crear `SecureGraphRef` elimina la única fuente de duplicación.

**Ciclo 1 — RED: Tests para los helpers como funciones libres**

1. [ ] Crear archivo de test (15 min)
   - Archivo: `crates/tessera-storage-enterprise/tests/lbac_filter_helpers_test.rs`

```rust
// Copyright 2026 BelowZero Security OU. All rights reserved.

use std::collections::BTreeSet;
use tessera_auth::lbac::{Clearance, SecurityLabel, SecurityPolicy};
use tessera_graph::{Graph, GraphAccess, props};
use tessera_storage_enterprise::lbac::filter;

fn clearance(level: u16, comps: &[&str]) -> Clearance {
    Clearance::new(level, comps.iter().map(|s| s.to_string()).collect())
}

fn label(level: u16, comps: &[&str]) -> SecurityLabel {
    SecurityLabel::new(level, comps.iter().map(|s| s.to_string()).collect())
}

#[test]
fn can_read_props_returns_true_when_clearance_dominates() {
    let mut p = props! {};
    SecurityPolicy::inject_label(&mut p, &label(2, &["FINANCE"]));
    assert!(filter::can_read_props(&clearance(3, &["FINANCE"]), &p));
}

#[test]
fn can_read_props_returns_false_when_level_insufficient() {
    let mut p = props! {};
    SecurityPolicy::inject_label(&mut p, &label(5, &[]));
    assert!(!filter::can_read_props(&clearance(4, &[]), &p));
}

#[test]
fn strip_node_removes_security_keys() {
    let mut g = Graph::new();
    let mut p = props! { "name" => "Alice" };
    SecurityPolicy::inject_label(&mut p, &label(1, &["HR"]));
    let id = g.add_node("P", p).unwrap();
    let node = g.node(id).unwrap();
    let stripped = filter::strip_node(node);
    assert!(!stripped.properties().contains_key(SecurityPolicy::LEVEL_KEY));
    assert!(!stripped.properties().contains_key(SecurityPolicy::COMPARTMENTS_KEY));
    assert_eq!(stripped.properties().get("name").and_then(|v| v.as_str()), Some("Alice"));
}

#[test]
fn edge_visible_for_returns_false_when_endpoint_not_accessible() {
    let mut g = Graph::new();
    let secret = label(5, &["SECRET"]);
    let mut np = props! {};
    SecurityPolicy::inject_label(&mut np, &secret);
    let src = g.add_node("N", np.clone()).unwrap();
    let tgt = g.add_node("N", np).unwrap();
    let mut ep = props! {};
    SecurityPolicy::inject_label(&mut ep, &label(0, &[]));
    let eid = g.add_edge("E", src, tgt, ep).unwrap();
    let edge = g.edge(eid).unwrap();
    assert!(!filter::edge_visible_for(&g, &clearance(0, &[]), &edge));
}
```

**Ciclo 1 — GREEN: Crear `pub mod filter` en `lbac.rs` y migrar `SecureGraph`**

2. [ ] Crear módulo `filter` y refactorizar `SecureGraph` (25 min)
   - Archivo: `crates/tessera-storage-enterprise/src/lbac.rs`
   - Acción: Añadir `pub mod filter` con funciones libres:

```rust
/// Shared pure filtering helpers used by both `SecureGraph` and `SecureGraphRef`.
pub mod filter {
    use tessera_auth::lbac::{Clearance, SecurityPolicy};
    use tessera_graph::{Edge, GraphAccess, Node, Properties};

    /// Returns `true` iff `clearance` dominates the security label encoded in `props`.
    #[must_use]
    pub fn can_read_props(clearance: &Clearance, props: &Properties) -> bool {
        let label = SecurityPolicy::extract_label(props);
        clearance.dominates(&label)
    }

    /// Return a copy of `node` with all reserved security properties removed.
    #[must_use]
    pub fn strip_node(mut node: Node) -> Node {
        SecurityPolicy::strip_security_properties(node.properties_mut());
        node
    }

    /// Return a copy of `edge` with all reserved security properties removed.
    #[must_use]
    pub fn strip_edge(mut edge: Edge) -> Edge {
        SecurityPolicy::strip_security_properties(edge.properties_mut());
        edge
    }

    /// Returns `true` iff edge and both endpoint nodes are visible to `clearance`.
    #[must_use]
    pub fn edge_visible_for<G: GraphAccess>(graph: &G, clearance: &Clearance, edge: &Edge) -> bool {
        if !can_read_props(clearance, edge.properties()) {
            return false;
        }
        let src_ok = graph
            .node(edge.source())
            .map(|n| can_read_props(clearance, n.properties()))
            .unwrap_or(false);
        let tgt_ok = graph
            .node(edge.target())
            .map(|n| can_read_props(clearance, n.properties()))
            .unwrap_or(false);
        src_ok && tgt_ok
    }
}
```

   - Reescribir `SecureGraph` para delegar a `filter::`:
     - `self.can_read(props)` → `filter::can_read_props(&self.clearance, props)`
     - `Self::strip_node(node)` → `filter::strip_node(node)`
     - `Self::strip_edge(edge)` → `filter::strip_edge(edge)`
     - `self.edge_visible_for(&e)` → `filter::edge_visible_for(self.inner, &self.clearance, &e)`
   - Eliminar los métodos privados `can_read`, `strip_node`, `strip_edge`, `edge_visible_for` de `SecureGraph`

**Ciclo 1 — REFACTOR**: Verificar que no queda duplicación:
```bash
grep -n "fn can_read\|fn strip_node\|fn strip_edge\|fn edge_visible_for" \
  crates/tessera-storage-enterprise/src/lbac.rs
# Solo debe aparecer dentro de `mod filter`
```
Todos los tests existentes deben seguir pasando.

---

### Ciclo 2: `SecureGraphRef` — wrapper de solo lectura

**Ciclo 2 — RED: Tests de lectura filtrada y errores en mutations**

3. [ ] Crear archivo de test (20 min)
   - Archivo: `crates/tessera-storage-enterprise/tests/secure_graph_ref_reads_test.rs`

```rust
// Copyright 2026 BelowZero Security OU. All rights reserved.

use std::collections::BTreeSet;
use tessera_auth::lbac::{Clearance, SecurityLabel, SecurityPolicy};
use tessera_graph::{Graph, GraphAccess, props};
use tessera_storage_enterprise::lbac::SecureGraphRef;

fn comps(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|s| (*s).to_string()).collect()
}

fn clearance(level: u16, compartments: &[&str]) -> Clearance {
    Clearance::new(level, comps(compartments))
}

fn make_graph_with_node(level: u16, compartments: &[&str]) -> (Graph, tessera_graph::NodeId) {
    let mut g = Graph::new();
    let security_label = SecurityLabel::new(level, comps(compartments));
    let mut p = props! { "name" => "Alice" };
    SecurityPolicy::inject_label(&mut p, &security_label);
    let id = g.add_node("Person", p).unwrap();
    (g, id)
}

#[test]
fn ref_node_returns_node_when_clearance_dominates() {
    let (g, id) = make_graph_with_node(2, &["FINANCE"]);
    let sg = SecureGraphRef::new(&g, clearance(3, &["FINANCE", "HR"]));
    assert!(sg.node(id).is_ok());
}

#[test]
fn ref_node_strips_security_properties() {
    let (g, id) = make_graph_with_node(2, &["FINANCE"]);
    let sg = SecureGraphRef::new(&g, clearance(3, &["FINANCE"]));
    let node = sg.node(id).unwrap();
    assert!(!node.properties().contains_key(SecurityPolicy::LEVEL_KEY));
    assert!(!node.properties().contains_key(SecurityPolicy::COMPARTMENTS_KEY));
}

#[test]
fn ref_node_denied_when_level_insufficient() {
    let (g, id) = make_graph_with_node(5, &[]);
    let sg = SecureGraphRef::new(&g, clearance(4, &[]));
    assert!(sg.node(id).is_err());
}

#[test]
fn ref_node_denied_when_compartment_missing() {
    let (g, id) = make_graph_with_node(1, &["LEGAL"]);
    let sg = SecureGraphRef::new(&g, clearance(5, &["FINANCE"]));
    assert!(sg.node(id).is_err());
}

#[test]
fn ref_node_ids_filters_inaccessible_nodes() {
    let mut g = Graph::new();
    let pub_label = SecurityLabel::default();
    let fin_label = SecurityLabel::new(1, comps(&["FINANCE"]));
    let mut p1 = props! {};
    SecurityPolicy::inject_label(&mut p1, &pub_label);
    let mut p2 = props! {};
    SecurityPolicy::inject_label(&mut p2, &fin_label);
    g.add_node("N", p1).unwrap();
    g.add_node("N", p2).unwrap();
    let sg = SecureGraphRef::new(&g, clearance(0, &[]));
    assert_eq!(sg.node_ids().len(), 1);
}

#[test]
fn ref_edge_visible_when_clearance_dominates_all_three() {
    let mut g = Graph::new();
    let fin_label = SecurityLabel::new(1, comps(&["FINANCE"]));
    let mut np = props! {};
    SecurityPolicy::inject_label(&mut np, &fin_label);
    let src = g.add_node("N", np.clone()).unwrap();
    let tgt = g.add_node("N", np).unwrap();
    let mut ep = props! {};
    SecurityPolicy::inject_label(&mut ep, &fin_label);
    let eid = g.add_edge("E", src, tgt, ep).unwrap();
    let sg = SecureGraphRef::new(&g, clearance(2, &["FINANCE"]));
    assert!(sg.edge(eid).is_ok());
}

#[test]
fn ref_edge_strips_security_properties() {
    let mut g = Graph::new();
    let fin_label = SecurityLabel::new(1, comps(&["FINANCE"]));
    let mut np = props! {};
    SecurityPolicy::inject_label(&mut np, &fin_label);
    let src = g.add_node("N", np.clone()).unwrap();
    let tgt = g.add_node("N", np).unwrap();
    let mut ep = props! {};
    SecurityPolicy::inject_label(&mut ep, &fin_label);
    let eid = g.add_edge("E", src, tgt, ep).unwrap();
    let sg = SecureGraphRef::new(&g, clearance(2, &["FINANCE"]));
    let edge = sg.edge(eid).unwrap();
    assert!(!edge.properties().contains_key(SecurityPolicy::LEVEL_KEY));
}

// --- Mutation methods return typed errors (not panic) ---

#[test]
fn ref_add_node_returns_error() {
    let g = Graph::new();
    let mut sg = SecureGraphRef::new(&g, clearance(99, &[]));
    let result = sg.add_node("X", props! {});
    assert!(result.is_err(), "add_node on SecureGraphRef must return Err");
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("read-only"),
        "error message must mention 'read-only', got: {err_msg}"
    );
}

#[test]
fn ref_add_edge_returns_error() {
    let g = Graph::new();
    let mut sg = SecureGraphRef::new(&g, clearance(99, &[]));
    let fake_id = tessera_graph::NodeId::from_raw(0);
    let result = sg.add_edge("E", fake_id, fake_id, props! {});
    assert!(result.is_err());
}

#[test]
fn ref_remove_node_returns_error() {
    let g = Graph::new();
    let mut sg = SecureGraphRef::new(&g, clearance(99, &[]));
    let fake_id = tessera_graph::NodeId::from_raw(0);
    let result = sg.remove_node(fake_id);
    assert!(result.is_err());
}
```

**Ciclo 2 — GREEN: Implementar `SecureGraphRef`**

4. [ ] Añadir `SecureGraphRef` a `lbac.rs` (25 min)
   - Archivo: `crates/tessera-storage-enterprise/src/lbac.rs`
   - Acción: Append tras `SecureGraph` impl. Estructura:

```rust
pub struct SecureGraphRef<'g, G: GraphAccess> {
    inner: &'g G,
    clearance: Clearance,
}

impl<'g, G: GraphAccess> SecureGraphRef<'g, G> {
    pub const fn new(inner: &'g G, clearance: Clearance) -> Self { .. }
}

impl<G: GraphAccess> GraphAccess for SecureGraphRef<'_, G> {
    // Reads: delegan a filter:: (idéntico a SecureGraph pero con &self.inner)
    // Mutations: retornan Err(Error::GqlMutationError("read-only secure graph: mutations are not permitted"))
}
```

**Ciclo 2 — REFACTOR: Throughput guards para `SecureGraphRef`**

5. [ ] Añadir 2 tests de throughput (15 min)
   - Archivo: `crates/tessera-storage-enterprise/tests/lbac_throughput_test.rs`
   - Acción: Append tests `secure_graph_ref_node_read_throughput_regression_guard` y `secure_graph_ref_node_ids_throughput_regression_guard` con mismos thresholds que `SecureGraph`.

---

### Ciclo 3: `resolve_clearance` en `ServerContext`

**Ciclo 3 — RED: Tests de resolución de clearance**

6. [ ] Crear archivo de test (20 min)
   - Archivo: `crates/tessera-server/tests/resolve_clearance_test.rs`

```rust
// Copyright 2026 BelowZero Security OU. All rights reserved.

mod common;

use tessera_auth::lbac::Clearance;
use tessera_auth::session::SessionToken;

#[test]
fn resolve_clearance_returns_clearance_for_valid_session() {
    let ctx = common::test_context();
    let user_id = ctx
        .user_store()
        .authenticate("admin", &tessera_auth::credentials::Password::new("Admin@Init1!").unwrap())
        .unwrap();
    let token = ctx.sessions().create_session(user_id).unwrap();
    let clearance = ctx.resolve_clearance(&token).unwrap();
    assert_eq!(clearance, Clearance::default());
}

#[test]
fn resolve_clearance_fails_for_invalid_token() {
    let ctx = common::test_context();
    let bad_token = SessionToken::from_raw("totally-invalid-token".to_string());
    let result = ctx.resolve_clearance(&bad_token);
    assert!(result.is_err());
}
```

**Ciclo 3 — GREEN: Implementar `resolve_clearance`**

7. [ ] Añadir método a `ServerContext` (15 min)
   - Archivo: `crates/tessera-server/src/context.rs`
   - Acción: Añadir import `use tessera_auth::lbac::Clearance;` y método:

```rust
/// Resolve the LBAC `Clearance` for the session identified by `token`.
///
/// Steps: (1) validate token → `UserId`, (2) look up clearance.
///
/// # Errors
///
/// Returns `AuthError` if token is invalid/expired or user not found.
/// **Fail-safe**: any error results in denial.
pub fn resolve_clearance(&self, token: &SessionToken) -> tessera_auth::Result<Clearance> {
    let user_id = self.sessions.validate(token)?;
    self.user_store.get_clearance(user_id)
}
```

**Ciclo 3 — REFACTOR**: `cargo clippy -p tessera-server --tests` limpio.

---

### Ciclo 4: Wire LBAC en el read path

**Ciclo 4 — RED: Tests de integración de lectura LBAC**

8. [ ] Crear archivo de test (25 min)
   - Archivo: `crates/tessera-server/tests/lbac_query_integration_test.rs`

```rust
// Copyright 2026 BelowZero Security OU. All rights reserved.

mod common;

use std::collections::BTreeSet;
use std::sync::{Arc, RwLock};

use tessera_auth::credentials::Password;
use tessera_auth::lbac::{Clearance, SecurityLabel, SecurityPolicy};
use tessera_graph::{Graph, props};
use tessera_protocol::message::{ClientMessage, ServerMessage};

use common::{send_recv, spawn_handler, test_context};

fn test_context_with_clearance(level: u16, compartments: &[&str]) -> Arc<tessera_server::context::ServerContext> {
    let ctx = test_context();
    let clearance = Clearance::new(
        level,
        compartments.iter().map(|s| s.to_string()).collect::<BTreeSet<_>>(),
    );
    ctx.user_store().set_clearance("admin", clearance).unwrap();
    ctx
}

fn graph_with_classified_node(level: u16, compartments: &[&str]) -> Arc<RwLock<Graph>> {
    let mut g = Graph::new();
    let label = SecurityLabel::new(
        level,
        compartments.iter().map(|s| s.to_string()).collect::<BTreeSet<_>>(),
    );
    let mut p = props! { "name" => "Secret" };
    SecurityPolicy::inject_label(&mut p, &label);
    g.add_node("Thing", p).unwrap();
    Arc::new(RwLock::new(g))
}

async fn login(
    writer: &mut tessera_protocol::frame::FramedWriter<tokio::io::WriteHalf<tokio::io::DuplexStream>>,
    reader: &mut tessera_protocol::frame::FramedReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
) {
    let response = send_recv(
        writer,
        reader,
        &ClientMessage::Login {
            username: "admin".into(),
            password: "Admin@Init1!".into(),
        },
    )
    .await;
    assert!(matches!(response, ServerMessage::AuthOk { .. }), "login failed: {response:?}");
}

#[tokio::test]
async fn query_read_hides_classified_node_from_under_cleared_user() {
    let ctx = test_context_with_clearance(0, &[]);
    let graph = graph_with_classified_node(5, &[]);
    let (mut writer, mut reader, _shutdown) = spawn_handler(ctx, graph);
    login(&mut writer, &mut reader).await;
    let response = send_recv(
        &mut writer,
        &mut reader,
        &ClientMessage::Query {
            query: "MATCH (n:Thing) RETURN n.name".into(),
            language: "gql".into(),
        },
    )
    .await;
    match response {
        ServerMessage::QueryResult { rows, .. } => {
            assert!(rows.is_empty(), "under-cleared user must see 0 rows, got {rows:?}");
        }
        other => panic!("expected QueryResult, got {other:?}"),
    }
}

#[tokio::test]
async fn query_read_shows_node_to_fully_cleared_user() {
    let ctx = test_context_with_clearance(10, &[]);
    let graph = graph_with_classified_node(5, &[]);
    let (mut writer, mut reader, _shutdown) = spawn_handler(ctx, graph);
    login(&mut writer, &mut reader).await;
    let response = send_recv(
        &mut writer,
        &mut reader,
        &ClientMessage::Query {
            query: "MATCH (n:Thing) RETURN n.name".into(),
            language: "gql".into(),
        },
    )
    .await;
    match response {
        ServerMessage::QueryResult { rows, .. } => {
            assert!(!rows.is_empty(), "fully-cleared user must see the node");
        }
        other => panic!("expected QueryResult, got {other:?}"),
    }
}

#[tokio::test]
async fn query_read_hides_compartmented_node_when_compartment_missing() {
    let ctx = test_context_with_clearance(5, &["FINANCE"]);
    let graph = graph_with_classified_node(1, &["SECRET_LEGAL"]);
    let (mut writer, mut reader, _shutdown) = spawn_handler(ctx, graph);
    login(&mut writer, &mut reader).await;
    let response = send_recv(
        &mut writer,
        &mut reader,
        &ClientMessage::Query {
            query: "MATCH (n:Thing) RETURN n.name".into(),
            language: "gql".into(),
        },
    )
    .await;
    match response {
        ServerMessage::QueryResult { rows, .. } => {
            assert!(rows.is_empty(), "compartment mismatch must hide node");
        }
        other => panic!("expected QueryResult, got {other:?}"),
    }
}

#[tokio::test]
async fn query_result_never_contains_security_properties() {
    let ctx = test_context_with_clearance(10, &[]);
    let graph = graph_with_classified_node(5, &[]);
    let (mut writer, mut reader, _shutdown) = spawn_handler(ctx, graph);
    login(&mut writer, &mut reader).await;
    let response = send_recv(
        &mut writer,
        &mut reader,
        &ClientMessage::Query {
            query: "MATCH (n:Thing) RETURN n".into(),
            language: "gql".into(),
        },
    )
    .await;
    if let ServerMessage::QueryResult { rows, .. } = response {
        for row in &rows {
            for val in row {
                let serialized = serde_json::to_string(val).unwrap();
                assert!(
                    !serialized.contains(SecurityPolicy::LEVEL_KEY),
                    "security level key must not appear in query result: {serialized}"
                );
                assert!(
                    !serialized.contains(SecurityPolicy::COMPARTMENTS_KEY),
                    "security compartments key must not appear in query result: {serialized}"
                );
            }
        }
    }
}
```

**Ciclo 4 — GREEN: Wire `SecureGraphRef` en el read path**

9. [ ] Modificar `handle_query` — arm `GqlStatement::Query` (20 min)
   - Archivo: `crates/tessera-server/src/connection.rs`
   - Acción: Añadir import `use tessera_storage_enterprise::lbac::SecureGraphRef;`
   - Reemplazar el arm de Query para:
     1. Extraer clearance del session token via `ctx.resolve_clearance()`
     2. Envolver `&*graph` (read guard) en `SecureGraphRef::new(&*graph, clearance)`
     3. Pasar a `gql::execute(&secure, q)`
   - On clearance error → `ServerMessage::AuthError` + audit denied

**Ciclo 4 — REFACTOR**: `cargo clippy -p tessera-server --tests` limpio.

---

### Ciclo 5: Wire LBAC en el mutation path + refactor helper

**Ciclo 5 — RED: Tests de integración de mutación LBAC**

10. [ ] Añadir tests de mutación al archivo `lbac_query_integration_test.rs` (20 min)

```rust
#[tokio::test]
async fn mutation_creates_public_node_for_level_zero_user() {
    let ctx = test_context_with_clearance(0, &[]);
    let graph = Arc::new(RwLock::new(Graph::new()));
    let (mut writer, mut reader, _shutdown) = spawn_handler(ctx, graph);
    login(&mut writer, &mut reader).await;
    let response = send_recv(
        &mut writer,
        &mut reader,
        &ClientMessage::Query {
            query: "CREATE (n:TestNode {name: 'public'})".into(),
            language: "gql".into(),
        },
    )
    .await;
    assert!(
        matches!(response, ServerMessage::QueryResult { .. }),
        "public CREATE must succeed: {response:?}"
    );
}

#[tokio::test]
async fn mutation_through_secure_graph_is_visible_on_subsequent_read() {
    let ctx = test_context_with_clearance(5, &[]);
    let graph = Arc::new(RwLock::new(Graph::new()));
    let (mut writer, mut reader, _shutdown) = spawn_handler(ctx, Arc::clone(&graph));
    login(&mut writer, &mut reader).await;

    let create_resp = send_recv(
        &mut writer,
        &mut reader,
        &ClientMessage::Query {
            query: "CREATE (n:Marker {tag: 'wired'})".into(),
            language: "gql".into(),
        },
    )
    .await;
    assert!(matches!(create_resp, ServerMessage::QueryResult { .. }));

    let read_resp = send_recv(
        &mut writer,
        &mut reader,
        &ClientMessage::Query {
            query: "MATCH (n:Marker) RETURN n.tag".into(),
            language: "gql".into(),
        },
    )
    .await;
    match read_resp {
        ServerMessage::QueryResult { rows, .. } => {
            assert!(!rows.is_empty(), "level-5 user must see created node");
        }
        other => panic!("expected QueryResult, got {other:?}"),
    }
}
```

**Ciclo 5 — GREEN: Wire `SecureGraph` en el mutation path**

11. [ ] Modificar `handle_query` — arm `GqlStatement::Mutation` (20 min)
    - Archivo: `crates/tessera-server/src/connection.rs`
    - Acción: Añadir import `use tessera_storage_enterprise::lbac::SecureGraph;`
    - Reemplazar el arm de Mutation para:
      1. Extraer clearance del session token
      2. Envolver `&mut *graph` (write guard) en `SecureGraph::new(&mut *graph, clearance)`
      3. Pasar a `execute_mut(&mut secure, m)`

**Ciclo 5 — REFACTOR: Extraer helper `resolve_clearance_or_deny`**

12. [ ] Refactorizar duplicación de clearance resolution (15 min)
    - Archivo: `crates/tessera-server/src/connection.rs`
    - Acción: Crear método privado:

```rust
async fn resolve_clearance_or_deny(
    &mut self,
    operation: &'static str,
) -> Result<Option<tessera_auth::lbac::Clearance>> {
    let token = self
        .session_token
        .as_ref()
        .expect("session_token always set before handle_query");
    match self.ctx.resolve_clearance(token) {
        Ok(c) => Ok(Some(c)),
        Err(e) => {
            let _ = self.ctx.audit().record_denied(
                None,
                operation,
                None,
                &format!("clearance resolution failed: {e}"),
            );
            self.send_message(&ServerMessage::AuthError {
                reason: "access denied".into(),
            })
            .await?;
            Ok(None)
        }
    }
}
```

    - Usar en ambos arms:
```rust
let Some(clearance) = self.resolve_clearance_or_deny("gql_read").await? else {
    return Ok(());
};
```

---

### Ciclo 6: Verificación de wiring — sin exports muertos

**Solo verificación — 0 código nuevo si todo está correcto.**

13. [ ] Verificar call sites de todos los exports nuevos (10 min)

```bash
# SecureGraphRef usado en connection.rs
grep -rn "SecureGraphRef" crates/tessera-server/src/

# SecureGraph::new en mutation arm
grep -rn "SecureGraph::new" crates/tessera-server/src/

# resolve_clearance en connection.rs (via helper)
grep -rn "resolve_clearance" crates/tessera-server/src/

# filter:: usado por ambos wrappers
grep -rn "filter::" crates/tessera-storage-enterprise/src/lbac.rs

# resolve_clearance_or_deny en ambos arms
grep -rn "resolve_clearance_or_deny" crates/tessera-server/src/connection.rs
```

Cada grep debe retornar ≥2 resultados (definición + uso). Si alguno retorna solo 1, wiring incompleto.

14. [ ] Verificar que los métodos privados migrados ya no existen fuera de `filter::` (5 min)

```bash
# Debe retornar solo matches dentro de `pub mod filter`
grep -n "fn can_read\|fn strip_node\|fn strip_edge\|fn edge_visible_for" \
  crates/tessera-storage-enterprise/src/lbac.rs
```

15. [ ] Suite completa y clippy (15 min)

```bash
nice cargo clippy --workspace --tests -- -D warnings
nice cargo test --workspace
```

---

## Estimación Total

| Ciclo | Implementación | Testing |
|---|---|---|
| 1 — filter helpers | 25 min | 15 min |
| 2 — SecureGraphRef | 25 min | 35 min |
| 3 — resolve_clearance | 15 min | 20 min |
| 4 — wire read path | 20 min | 25 min |
| 5 — wire mutation + refactor | 35 min | 20 min |
| 6 — wiring verification | 0 min | 15 min |
| **Total** | **~2h** | **~2h** |

## Criterios de Éxito

- [ ] `cargo test --workspace` pasa (all existing + new tests)
- [ ] `cargo clippy --workspace --tests -- -D warnings` sale 0
- [ ] `SecureGraphRef node()` throughput >= 50k ops/sec (debug) / 500k ops/sec (release)
- [ ] No duplicación: `can_read`, `strip_node`, `strip_edge`, `edge_visible_for` SOLO en `filter::`
- [ ] Todos los greps del Ciclo 6.1 retornan ≥2 resultados
- [ ] Nodo nivel 5 invisible para usuario clearance 0 a través del stack completo del servidor
- [ ] `__security_level` y `__security_compartments` nunca aparecen en `QueryResult`
- [ ] Fallo de `resolve_clearance` → `AuthError` (no `QueryError`, no panic)

## Archivos Creados/Modificados

| Archivo | Acción |
|---|---|
| `crates/tessera-storage-enterprise/src/lbac.rs` | Modificado (add `filter` module, add `SecureGraphRef`, remove private methods from `SecureGraph`) |
| `crates/tessera-server/src/context.rs` | Modificado (add `resolve_clearance`) |
| `crates/tessera-server/src/connection.rs` | Modificado (wire both paths, add `resolve_clearance_or_deny`) |
| `crates/tessera-storage-enterprise/tests/lbac_filter_helpers_test.rs` | Creado |
| `crates/tessera-storage-enterprise/tests/secure_graph_ref_reads_test.rs` | Creado |
| `crates/tessera-storage-enterprise/tests/lbac_throughput_test.rs` | Modificado (2 new guards) |
| `crates/tessera-server/tests/resolve_clearance_test.rs` | Creado |
| `crates/tessera-server/tests/lbac_query_integration_test.rs` | Creado |
