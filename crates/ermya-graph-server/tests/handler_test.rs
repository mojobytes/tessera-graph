// SPDX-License-Identifier: BSL-1.1

//! Integration tests for [`BoltHandler`] over `DuplexStream`.
//!
//! Each test uses `spawn_bolt_handler` from `common`, which performs the Bolt
//! 4.4 handshake and returns chunked reader/writer halves.
//!
//! # Alcance
//!
//! Sólo lo que se comporta igual en las dos ediciones: saludo y metadatos del
//! protocolo, consultas y mutaciones, transacciones explícitas, parámetros,
//! paginación de resultados, y los topes de tráfico, tiempo y filas.
//!
//! Lo que sólo tiene sentido con varias bases —elegir base, cambiarse de base y
//! los rechazos por catálogo y por permiso— vive en `handler_multidb_test.rs`,
//! que **no viaja al árbol público**. El corte va por tema, no por montaje:
//! apartado 5.14 del inventario.

mod common;

use std::sync::Arc;

use ermya_graph_protocol::PackStreamValue;
use ermya_graph_protocol::bolt_message::{BoltRequest, BoltResponse};
use ermya_graph_server::auth::NoAuthProvider;

use common::{bolt_recv, bolt_send};

// ── Helpers ─────────────────────────────────────────────────────────────────

// Used again in Task 8 when SystemGraphAuthProvider-backed handler tests
// land; the interim between Task 4 and Task 8 leaves it unused.
fn hello_request(principal: &str, password: &str) -> BoltRequest {
    BoltRequest::Hello {
        extra: vec![
            (
                "principal".to_owned(),
                PackStreamValue::String(principal.to_owned()),
            ),
            (
                "credentials".to_owned(),
                PackStreamValue::String(password.to_owned()),
            ),
        ],
    }
}
fn hello_no_auth() -> BoltRequest {
    BoltRequest::Hello { extra: vec![] }
}
/// Build a RUN bound to the default test database. Post-Plan-B every RUN must
/// carry `extra["db"]` (the single-graph fallback was removed), so the legacy
/// query tests route through [`common::DEFAULT_TEST_DB`]. Protocol tests that
/// must run *without* a bind (e.g. RUN-before-HELLO) use [`run_query_no_db`].
fn run_query(query: &str) -> BoltRequest {
    BoltRequest::Run {
        query: query.to_owned(),
        params: vec![],
        extra: vec![(
            "db".to_owned(),
            PackStreamValue::String(common::DEFAULT_TEST_DB.to_owned()),
        )],
    }
}
/// Build a RUN with no `db` — for protocol tests that exercise the
/// pre-bind path (the RUN is rejected before any database routing).
fn run_query_no_db(query: &str) -> BoltRequest {
    BoltRequest::Run {
        query: query.to_owned(),
        params: vec![],
        extra: vec![],
    }
}
#[allow(clippy::missing_const_for_fn)]
fn pull() -> BoltRequest {
    BoltRequest::Pull { extra: vec![] }
}
fn dict_str(resp: &BoltResponse, key: &str) -> Option<String> {
    if let BoltResponse::Success { metadata } | BoltResponse::Failure { metadata } = resp {
        metadata.iter().find_map(|(k, v)| {
            if k == key {
                if let PackStreamValue::String(s) = v {
                    Some(s.clone())
                } else {
                    None
                }
            } else {
                None
            }
        })
    } else {
        None
    }
}
fn dict_bool(resp: &BoltResponse, key: &str) -> Option<bool> {
    if let BoltResponse::Success { metadata } = resp {
        metadata.iter().find_map(|(k, v)| {
            if k == key {
                if let PackStreamValue::Bool(b) = v {
                    Some(*b)
                } else {
                    None
                }
            } else {
                None
            }
        })
    } else {
        None
    }
}
fn dict_list(resp: &BoltResponse, key: &str) -> Option<Vec<PackStreamValue>> {
    if let BoltResponse::Success { metadata } = resp {
        metadata.iter().find_map(|(k, v)| {
            if k == key {
                if let PackStreamValue::List(l) = v {
                    Some(l.clone())
                } else {
                    None
                }
            } else {
                None
            }
        })
    } else {
        None
    }
}
/// Extract a nested Bolt `Dict` (map) value from a SUCCESS metadata by key —
/// used to read the `stats` block emitted after a mutation's final PULL.
fn dict_dict(resp: &BoltResponse, key: &str) -> Option<Vec<(String, PackStreamValue)>> {
    if let BoltResponse::Success { metadata } = resp {
        metadata
            .iter()
            .find_map(|(k, v)| match (k.as_str() == key, v) {
                (true, PackStreamValue::Dict(d)) => Some(d.clone()),
                _ => None,
            })
    } else {
        None
    }
}
/// Look up an integer entry inside a `stats` dict (returns `None` if absent —
/// the wire contract omits zero-valued numeric keys).
fn stats_int(stats: &[(String, PackStreamValue)], key: &str) -> Option<i64> {
    stats.iter().find_map(|(k, v)| match (k == key, v) {
        (true, PackStreamValue::Int(n)) => Some(*n),
        _ => None,
    })
}
/// Look up the `contains-updates` bool inside a `stats` dict.
fn stats_bool(stats: &[(String, PackStreamValue)], key: &str) -> Option<bool> {
    stats.iter().find_map(|(k, v)| match (k == key, v) {
        (true, PackStreamValue::Bool(b)) => Some(*b),
        _ => None,
    })
}
#[tokio::test]
async fn hello_no_auth_returns_success_with_server_and_connection_id() {
    let auth = Arc::new(NoAuthProvider);
    let (registry, _tmp) = common::single_db_registry().await;
    let (mut writer, mut reader, _shutdown) = common::spawn_single_db_handler(auth, registry).await;

    bolt_send(&mut writer, &hello_no_auth()).await;
    let resp = bolt_recv(&mut reader).await;

    assert!(
        matches!(resp, BoltResponse::Success { .. }),
        "expected SUCCESS, got {resp:?}"
    );
    let server = dict_str(&resp, "server").expect("missing 'server' in metadata");
    // The agent string contract (default Neo4j/<semver>, .NET semver shape) is
    // pinned by `hello_server_agent_is_neo4j_by_default` and
    // `hello_server_metadata_is_semver_compatible_for_dotnet_driver`; here we
    // only assert the field is present and non-empty.
    assert!(!server.is_empty(), "server metadata must not be empty");
    assert!(
        dict_str(&resp, "connection_id").is_some()
            || matches!(
                resp,
                BoltResponse::Success { ref metadata }
                if metadata.iter().any(|(k, _)| k == "connection_id")
            ),
        "expected connection_id in metadata"
    );
}
/// v0.7.0 Block 1: the default agent string is `Neo4j/<semver>` so the official
/// Neo4j Python driver connects without patching `check_supported_server_product`
/// (which rejects any product not starting with `Neo4j/`). This pins the default
/// flowing config → handler → HELLO `server` metadata.
#[tokio::test]
async fn hello_server_agent_is_neo4j_by_default() {
    let auth = Arc::new(NoAuthProvider);
    let (registry, _tmp) = common::single_db_registry().await;
    let (mut writer, mut reader, _shutdown) = common::spawn_single_db_handler(auth, registry).await;

    bolt_send(&mut writer, &hello_no_auth()).await;
    let resp = bolt_recv(&mut reader).await;
    let server = dict_str(&resp, "server").expect("missing 'server' in metadata");
    assert!(
        server.starts_with("Neo4j/"),
        "default server agent must start with Neo4j/ for Python driver compat, got {server:?}"
    );
}
/// The Neo4j .NET driver v5.x parses the `server` metadata string
/// assuming the `<name>/<semver>` shape (e.g. `Neo4j/5.28.0`) and throws
/// `ArgumentOutOfRangeException: Unexpected server version format` when
/// the token after the slash is not a valid semver. Before v0.4.1 the
/// handler sent `ErmyaGraph/Community`, which tripped this check and
/// made the reference .NET consumer unable to connect even after a
/// successful authentication. This test pins the shape so the regression
/// cannot silently come back through a refactor.
#[tokio::test]
async fn hello_server_metadata_is_semver_compatible_for_dotnet_driver() {
    let auth = Arc::new(NoAuthProvider);
    let (registry, _tmp) = common::single_db_registry().await;
    let (mut writer, mut reader, _shutdown) = common::spawn_single_db_handler(auth, registry).await;

    bolt_send(&mut writer, &hello_no_auth()).await;
    let resp = bolt_recv(&mut reader).await;
    let server = dict_str(&resp, "server").expect("missing 'server' in metadata");

    let (product, version) = server
        .split_once('/')
        .unwrap_or_else(|| panic!("`server` must be 'name/version', got {server:?}"));
    // v0.7.0 Block 1: the default product token is now "Neo4j" (configurable
    // via ERMYA_SERVER_AGENT). What the .NET driver needs is a semver-valid
    // version after the slash — that is what this test still guards below.
    assert!(
        !product.is_empty(),
        "product token must not be empty, got {server:?}"
    );

    // Minimum semver check: three dot-separated numeric components.
    // Full semver allows pre-release / build metadata but the driver
    // only needs the numeric core to parse.
    let numeric: Vec<&str> = version.split('.').collect();
    assert!(
        numeric.len() >= 3,
        "version {version:?} must have at least three dot-separated components"
    );
    for component in &numeric[..3] {
        assert!(
            component.chars().all(|c| c.is_ascii_digit()),
            "version component {component:?} in {version:?} must be numeric"
        );
    }
}
/// `connection_id` must be a Bolt String, not an Int. The .NET driver
/// throws `ProtocolException: Expected 'connection_id' metadata to be
/// of type 'String', but got 'Int64'` otherwise. The Python driver is
/// more lenient but also prefers String per the Neo4j spec. Before
/// v0.4.1 Ermya sent an Int.
#[tokio::test]
async fn hello_connection_id_is_bolt_string_not_int() {
    let auth = Arc::new(NoAuthProvider);
    let (registry, _tmp) = common::single_db_registry().await;
    let (mut writer, mut reader, _shutdown) = common::spawn_single_db_handler(auth, registry).await;

    bolt_send(&mut writer, &hello_no_auth()).await;
    let resp = bolt_recv(&mut reader).await;

    let BoltResponse::Success { ref metadata } = resp else {
        panic!("expected Success, got {resp:?}");
    };
    let entry = metadata
        .iter()
        .find(|(k, _)| k == "connection_id")
        .expect("connection_id missing from HELLO success metadata");
    match &entry.1 {
        PackStreamValue::String(_) => {}
        other => {
            panic!("connection_id must be a Bolt String (driver compatibility); got {other:?}")
        }
    }
}
#[tokio::test]
async fn run_before_hello_returns_failure() {
    let auth = Arc::new(NoAuthProvider);
    let (registry, _tmp) = common::single_db_registry().await;
    let (mut writer, mut reader, _shutdown) = common::spawn_single_db_handler(auth, registry).await;

    // Skip HELLO, go straight to RUN (no db — rejected before any bind).
    bolt_send(&mut writer, &run_query_no_db("MATCH (n) RETURN n")).await;
    let resp = bolt_recv(&mut reader).await;

    assert!(
        matches!(resp, BoltResponse::Failure { .. }),
        "expected FAILURE for RUN without HELLO, got {resp:?}"
    );
    // Spec §8 contract: a missing-HELLO is a protocol-violation
    // (`Request.Invalid`), not a credentials issue (`Unauthorized`).
    // Pinned across both legacy single-DB and registry-mode paths.
    let code = dict_str(&resp, "code").unwrap_or_default();
    assert!(
        code.contains("Request.Invalid"),
        "expected Request.Invalid for RUN without HELLO, got: {code} (resp: {resp:?})"
    );
    let msg = dict_str(&resp, "message").unwrap_or_default();
    assert!(
        msg.contains("not authenticated") || msg.contains("HELLO"),
        "expected auth-related message, got: {msg}"
    );
}
#[tokio::test]
async fn run_match_on_empty_graph_returns_success_with_fields() {
    let auth = Arc::new(NoAuthProvider);
    let (registry, _tmp) = common::single_db_registry().await;
    let (mut writer, mut reader, _shutdown) = common::spawn_single_db_handler(auth, registry).await;

    // Authenticate first.
    bolt_send(&mut writer, &hello_no_auth()).await;
    let _ = bolt_recv(&mut reader).await;

    // RUN a MATCH query.
    bolt_send(&mut writer, &run_query("MATCH (n) RETURN n")).await;
    let resp = bolt_recv(&mut reader).await;

    assert!(
        matches!(resp, BoltResponse::Success { .. }),
        "expected SUCCESS for RUN, got {resp:?}"
    );
    // Should have "fields" in metadata.
    assert!(
        dict_list(&resp, "fields").is_some(),
        "expected 'fields' in RUN SUCCESS metadata"
    );
}
#[tokio::test]
async fn pull_after_empty_match_returns_success_with_no_records() {
    let auth = Arc::new(NoAuthProvider);
    let (registry, _tmp) = common::single_db_registry().await;
    let (mut writer, mut reader, _shutdown) = common::spawn_single_db_handler(auth, registry).await;

    bolt_send(&mut writer, &hello_no_auth()).await;
    let _ = bolt_recv(&mut reader).await;

    bolt_send(&mut writer, &run_query("MATCH (n) RETURN n")).await;
    let _ = bolt_recv(&mut reader).await;

    // PULL — no records expected on empty graph.
    bolt_send(&mut writer, &pull()).await;
    let resp = bolt_recv(&mut reader).await;

    assert!(
        matches!(resp, BoltResponse::Success { .. }),
        "expected SUCCESS for PULL (no records), got {resp:?}"
    );
    assert_eq!(
        dict_bool(&resp, "has_more"),
        Some(false),
        "expected has_more=false"
    );
}
#[tokio::test]
async fn create_then_match_returns_records() {
    let auth = Arc::new(NoAuthProvider);
    let (registry, _tmp) = common::single_db_registry().await;
    let (mut writer, mut reader, _shutdown) =
        common::spawn_single_db_handler(Arc::clone(&auth), Arc::clone(&registry)).await;

    // Authenticate.
    bolt_send(&mut writer, &hello_no_auth()).await;
    let _ = bolt_recv(&mut reader).await;

    // CREATE a node.
    bolt_send(&mut writer, &run_query("CREATE (:Person {name: 'Alice'})")).await;
    let create_resp = bolt_recv(&mut reader).await;
    assert!(
        matches!(create_resp, BoltResponse::Success { .. }),
        "expected SUCCESS for CREATE, got {create_resp:?}"
    );

    // PULL the CREATE result.
    bolt_send(&mut writer, &pull()).await;
    let pull_resp = bolt_recv(&mut reader).await;
    // Approach A: a non-returning CREATE yields NO data row; the counters
    // travel in the final SUCCESS `stats` dict instead.
    let mut records = vec![];
    let final_resp = collect_records(pull_resp, &mut records, &mut reader).await;
    assert!(
        records.is_empty(),
        "CREATE must produce no data row, got {records:?}"
    );
    assert!(
        matches!(final_resp, BoltResponse::Success { .. }),
        "expected trailing SUCCESS after PULL"
    );

    // Now MATCH to verify the node exists.
    bolt_send(&mut writer, &run_query("MATCH (n:Person) RETURN n.name")).await;
    let run_resp = bolt_recv(&mut reader).await;
    assert!(matches!(run_resp, BoltResponse::Success { .. }));

    bolt_send(&mut writer, &pull()).await;
    let first = bolt_recv(&mut reader).await;
    let mut match_records = vec![];
    let final_match = collect_records(first, &mut match_records, &mut reader).await;

    assert!(
        !match_records.is_empty(),
        "expected at least 1 RECORD from MATCH"
    );
    assert!(matches!(final_match, BoltResponse::Success { .. }));
}
/// A non-returning CREATE emits no data row over Bolt; the counters ride the
/// final PULL SUCCESS `stats` dict instead (Neo4j-compatible).
#[tokio::test]
async fn create_mutation_pull_success_carries_no_data_row() {
    let auth = Arc::new(NoAuthProvider);
    let (registry, _tmp) = common::single_db_registry().await;
    let (mut writer, mut reader, _shutdown) = common::spawn_single_db_handler(auth, registry).await;

    bolt_send(&mut writer, &hello_no_auth()).await;
    let _ = bolt_recv(&mut reader).await;

    bolt_send(&mut writer, &run_query("CREATE (:Person)")).await;
    assert!(matches!(
        bolt_recv(&mut reader).await,
        BoltResponse::Success { .. }
    ));

    bolt_send(&mut writer, &pull()).await;
    let first = bolt_recv(&mut reader).await;
    let mut records = vec![];
    let _final = collect_records(first, &mut records, &mut reader).await;
    assert!(
        records.is_empty(),
        "CREATE must produce no data row, got {records:?}"
    );
}
/// The final PULL SUCCESS after a CREATE carries a `stats` dict with the
/// Neo4j-style hyphenated keys, listing only the non-zero counters plus
/// `contains-updates`.
#[tokio::test]
async fn pull_final_success_carries_stats_dict_for_create() {
    let auth = Arc::new(NoAuthProvider);
    let (registry, _tmp) = common::single_db_registry().await;
    let (mut writer, mut reader, _shutdown) = common::spawn_single_db_handler(auth, registry).await;

    bolt_send(&mut writer, &hello_no_auth()).await;
    let _ = bolt_recv(&mut reader).await;

    bolt_send(&mut writer, &run_query("CREATE (:Person {name: 'Alice'})")).await;
    assert!(matches!(
        bolt_recv(&mut reader).await,
        BoltResponse::Success { .. }
    ));

    bolt_send(&mut writer, &pull()).await;
    let first = bolt_recv(&mut reader).await;
    let mut records = vec![];
    let final_resp = collect_records(first, &mut records, &mut reader).await;

    let stats = dict_dict(&final_resp, "stats").expect("final SUCCESS must carry a stats dict");
    assert_eq!(stats_int(&stats, "nodes-created"), Some(1));
    assert_eq!(stats_int(&stats, "labels-added"), Some(1));
    assert_eq!(stats_bool(&stats, "contains-updates"), Some(true));
    // Zero-valued numeric keys are omitted from the wire dict.
    assert_eq!(
        stats_int(&stats, "relationships-created"),
        None,
        "zero-valued keys must be omitted"
    );
}
/// Issue #45: a DETACH DELETE over the Bolt flow reports `nodes-deleted` and
/// `relationships-deleted` in the final PULL SUCCESS `stats` dict — the driver's
/// `summary.Counters` reads these directly.
#[tokio::test]
async fn handler_detach_delete_reports_counters() {
    let auth = Arc::new(NoAuthProvider);
    let (registry, _tmp) = common::single_db_registry().await;
    let (mut writer, mut reader, _shutdown) = common::spawn_single_db_handler(auth, registry).await;

    bolt_send(&mut writer, &hello_no_auth()).await;
    let _ = bolt_recv(&mut reader).await;

    // Seed two nodes and one edge between them via supported forms (bare CREATE
    // for nodes, then MATCH-bound CREATE for the edge).
    for q in ["CREATE (:Src)", "CREATE (:Dst)"] {
        bolt_send(&mut writer, &run_query(q)).await;
        let _ = bolt_recv(&mut reader).await;
        bolt_send(&mut writer, &pull()).await;
        let mut sink = vec![];
        let _ = collect_records(bolt_recv(&mut reader).await, &mut sink, &mut reader).await;
    }
    bolt_send(
        &mut writer,
        &run_query("MATCH (a:Src), (b:Dst) CREATE (a)-[:KNOWS]->(b)"),
    )
    .await;
    let _ = bolt_recv(&mut reader).await;
    bolt_send(&mut writer, &pull()).await;
    let mut sink = vec![];
    let _ = collect_records(bolt_recv(&mut reader).await, &mut sink, &mut reader).await;

    // DETACH DELETE the source node: the node and its incident edge are removed.
    bolt_send(&mut writer, &run_query("MATCH (a:Src) DETACH DELETE a")).await;
    assert!(
        matches!(bolt_recv(&mut reader).await, BoltResponse::Success { .. }),
        "expected SUCCESS for DETACH DELETE"
    );
    bolt_send(&mut writer, &pull()).await;
    let mut records = vec![];
    let final_resp = collect_records(bolt_recv(&mut reader).await, &mut records, &mut reader).await;
    assert!(records.is_empty(), "DETACH DELETE produces no data row");

    let stats = dict_dict(&final_resp, "stats").expect("DELETE must carry a stats dict");
    assert_eq!(stats_int(&stats, "nodes-deleted"), Some(1));
    assert_eq!(stats_int(&stats, "relationships-deleted"), Some(1));
    assert_eq!(stats_bool(&stats, "contains-updates"), Some(true));
}
/// Issue #45: deleting a still-connected node WITHOUT DETACH returns a Bolt
/// FAILURE (not a crash), carrying the connected-node error.
///
/// Issue #43 cycle A10 additionally pins the wire code. Asserting only that
/// *some* failure came back is what let the dedicated code promised by the
/// engine error's docstring go un-wired: the client was being handed the
/// generic execution failure, which reads as "maybe retry" for what is really
/// a graph-integrity violation.
#[tokio::test]
async fn handler_delete_connected_node_without_detach_fails() {
    let auth = Arc::new(NoAuthProvider);
    let (registry, _tmp) = common::single_db_registry().await;
    let (mut writer, mut reader, _shutdown) = common::spawn_single_db_handler(auth, registry).await;

    bolt_send(&mut writer, &hello_no_auth()).await;
    let _ = bolt_recv(&mut reader).await;

    for q in ["CREATE (:Src)", "CREATE (:Dst)"] {
        bolt_send(&mut writer, &run_query(q)).await;
        let _ = bolt_recv(&mut reader).await;
        bolt_send(&mut writer, &pull()).await;
        let mut sink = vec![];
        let _ = collect_records(bolt_recv(&mut reader).await, &mut sink, &mut reader).await;
    }
    bolt_send(
        &mut writer,
        &run_query("MATCH (a:Src), (b:Dst) CREATE (a)-[:KNOWS]->(b)"),
    )
    .await;
    let _ = bolt_recv(&mut reader).await;
    bolt_send(&mut writer, &pull()).await;
    let mut sink = vec![];
    let _ = collect_records(bolt_recv(&mut reader).await, &mut sink, &mut reader).await;

    // DELETE (no DETACH) of the connected node → FAILURE, server stays up.
    bolt_send(&mut writer, &run_query("MATCH (a:Src) DELETE a")).await;
    let resp = bolt_recv(&mut reader).await;
    let BoltResponse::Failure { metadata } = &resp else {
        panic!("expected FAILURE for DELETE of a connected node, got {resp:?}");
    };
    let code = metadata
        .iter()
        .find_map(|(k, v)| match (k.as_str(), v) {
            ("code", PackStreamValue::String(s)) => Some(s.clone()),
            _ => None,
        })
        .expect("code in Failure");
    assert_eq!(
        code, "Neo.ClientError.Schema.ConstraintValidationFailed",
        "a connected-node delete is an integrity violation, not a generic execution failure"
    );
}
/// A pipeline SET terminal (no RETURN) likewise emits no data row.
#[tokio::test]
async fn pipeline_set_terminal_pull_success_carries_no_data_row() {
    let auth = Arc::new(NoAuthProvider);
    let (registry, _tmp) = common::single_db_registry().await;
    let (mut writer, mut reader, _shutdown) = common::spawn_single_db_handler(auth, registry).await;

    bolt_send(&mut writer, &hello_no_auth()).await;
    let _ = bolt_recv(&mut reader).await;

    bolt_send(&mut writer, &run_query("CREATE (:Person {age: 0})")).await;
    let _ = bolt_recv(&mut reader).await;
    bolt_send(&mut writer, &pull()).await;
    let mut sink = vec![];
    let _ = collect_records(bolt_recv(&mut reader).await, &mut sink, &mut reader).await;

    bolt_send(
        &mut writer,
        &run_query("MATCH (n:Person) WITH n SET n.age = 1"),
    )
    .await;
    assert!(matches!(
        bolt_recv(&mut reader).await,
        BoltResponse::Success { .. }
    ));
    bolt_send(&mut writer, &pull()).await;
    let mut records = vec![];
    let final_resp = collect_records(bolt_recv(&mut reader).await, &mut records, &mut reader).await;
    assert!(
        records.is_empty(),
        "pipeline SET must produce no data row, got {records:?}"
    );
    let stats = dict_dict(&final_resp, "stats").expect("pipeline SET must carry a stats dict");
    assert_eq!(stats_int(&stats, "properties-set"), Some(1));
    assert_eq!(stats_bool(&stats, "contains-updates"), Some(true));
}
/// An intermediate PULL (`has_more == true`) never carries a `stats` dict —
/// Neo4j sends stats only in the terminal SUCCESS. Exercised with a partial
/// fetch (`n = 1`) over a two-row result.
#[tokio::test]
async fn pull_intermediate_success_has_no_stats_key() {
    let auth = Arc::new(NoAuthProvider);
    let (registry, _tmp) = common::single_db_registry().await;
    let (mut writer, mut reader, _shutdown) = common::spawn_single_db_handler(auth, registry).await;

    bolt_send(&mut writer, &hello_no_auth()).await;
    let _ = bolt_recv(&mut reader).await;

    // Seed two nodes so a MATCH returns two rows.
    for q in ["CREATE (:Item)", "CREATE (:Item)"] {
        bolt_send(&mut writer, &run_query(q)).await;
        let _ = bolt_recv(&mut reader).await;
        bolt_send(&mut writer, &pull()).await;
        let mut sink = vec![];
        let _ = collect_records(bolt_recv(&mut reader).await, &mut sink, &mut reader).await;
    }

    bolt_send(&mut writer, &run_query("MATCH (n:Item) RETURN n")).await;
    assert!(matches!(
        bolt_recv(&mut reader).await,
        BoltResponse::Success { .. }
    ));

    // Partial fetch: pull a single row, leaving one behind → has_more = true.
    bolt_send(
        &mut writer,
        &BoltRequest::Pull {
            extra: vec![("n".to_owned(), PackStreamValue::Int(1))],
        },
    )
    .await;
    let mut records = vec![];
    let intermediate =
        collect_records(bolt_recv(&mut reader).await, &mut records, &mut reader).await;
    assert_eq!(
        dict_bool(&intermediate, "has_more"),
        Some(true),
        "expected more rows pending"
    );
    assert!(
        dict_dict(&intermediate, "stats").is_none(),
        "intermediate SUCCESS must not carry stats"
    );
}
/// A driver that consumes without pulling rows uses DISCARD; the `stats` dict
/// must ride the DISCARD SUCCESS just like PULL.
#[tokio::test]
async fn discard_final_success_carries_stats_dict_for_create() {
    let auth = Arc::new(NoAuthProvider);
    let (registry, _tmp) = common::single_db_registry().await;
    let (mut writer, mut reader, _shutdown) = common::spawn_single_db_handler(auth, registry).await;

    bolt_send(&mut writer, &hello_no_auth()).await;
    let _ = bolt_recv(&mut reader).await;

    bolt_send(&mut writer, &run_query("CREATE (:Person)")).await;
    assert!(matches!(
        bolt_recv(&mut reader).await,
        BoltResponse::Success { .. }
    ));

    bolt_send(&mut writer, &BoltRequest::Discard { extra: vec![] }).await;
    let resp = bolt_recv(&mut reader).await;
    let stats = dict_dict(&resp, "stats").expect("DISCARD SUCCESS must carry a stats dict");
    assert_eq!(stats_int(&stats, "nodes-created"), Some(1));
    assert_eq!(stats_bool(&stats, "contains-updates"), Some(true));
}
#[tokio::test]
async fn run_return_literal_one_returns_single_row_via_handler() {
    // End-to-end keep-alive: client sends `RETURN 1` with no MATCH, the
    // handler routes through GqlStatement::ConstReturn and emits exactly
    // one RECORD containing `Int(1)`.
    let auth = Arc::new(NoAuthProvider);
    let (registry, _tmp) = common::single_db_registry().await;
    let (mut writer, mut reader, _shutdown) = common::spawn_single_db_handler(auth, registry).await;

    bolt_send(&mut writer, &hello_no_auth()).await;
    let _ = bolt_recv(&mut reader).await;

    bolt_send(&mut writer, &run_query("RETURN 1")).await;
    let run_resp = bolt_recv(&mut reader).await;
    assert!(
        matches!(run_resp, BoltResponse::Success { .. }),
        "expected SUCCESS for RUN, got {run_resp:?}"
    );

    bolt_send(&mut writer, &pull()).await;
    let first = bolt_recv(&mut reader).await;
    let mut records = vec![];
    let final_resp = collect_records(first, &mut records, &mut reader).await;
    assert_eq!(records.len(), 1, "expected exactly one RECORD for RETURN 1");
    assert!(
        matches!(final_resp, BoltResponse::Success { .. }),
        "expected trailing SUCCESS, got {final_resp:?}"
    );
}
#[tokio::test]
async fn reset_after_failure_clears_state() {
    let auth = Arc::new(NoAuthProvider);
    let (registry, _tmp) = common::single_db_registry().await;
    let (mut writer, mut reader, _shutdown) = common::spawn_single_db_handler(auth, registry).await;

    // Authenticate.
    bolt_send(&mut writer, &hello_no_auth()).await;
    let _ = bolt_recv(&mut reader).await;

    // Send an invalid query to trigger FAILURE.
    bolt_send(&mut writer, &run_query("THIS IS NOT VALID GQL")).await;
    let fail_resp = bolt_recv(&mut reader).await;
    assert!(
        matches!(fail_resp, BoltResponse::Failure { .. }),
        "expected FAILURE for invalid query"
    );

    // In FAILED state, subsequent messages should be IGNORED.
    bolt_send(&mut writer, &run_query("MATCH (n) RETURN n")).await;
    let ignored = bolt_recv(&mut reader).await;
    assert!(
        matches!(ignored, BoltResponse::Ignored),
        "expected IGNORED in failed state, got {ignored:?}"
    );

    // RESET clears the failed state.
    bolt_send(&mut writer, &BoltRequest::Reset).await;
    let reset_resp = bolt_recv(&mut reader).await;
    assert!(
        matches!(reset_resp, BoltResponse::Success { .. }),
        "expected SUCCESS for RESET"
    );

    // Now queries should work again.
    bolt_send(&mut writer, &run_query("MATCH (n) RETURN n")).await;
    let resp = bolt_recv(&mut reader).await;
    assert!(
        matches!(resp, BoltResponse::Success { .. }),
        "expected SUCCESS after RESET, got {resp:?}"
    );
}
#[tokio::test]
async fn goodbye_closes_connection() {
    let auth = Arc::new(NoAuthProvider);
    let (registry, _tmp) = common::single_db_registry().await;
    let (mut writer, mut reader, _shutdown) = common::spawn_single_db_handler(auth, registry).await;

    bolt_send(&mut writer, &hello_no_auth()).await;
    let _ = bolt_recv(&mut reader).await;

    bolt_send(&mut writer, &BoltRequest::Goodbye).await;

    // After GOODBYE, the server should close the stream.
    // Attempting to read should return None (EOF).
    let data = reader.read_message().await.expect("read should not error");
    assert!(data.is_none(), "expected EOF after GOODBYE");
}
#[tokio::test]
async fn discard_clears_pending_result() {
    let auth = Arc::new(NoAuthProvider);
    let (registry, _tmp) = common::single_db_registry().await;
    let (mut writer, mut reader, _shutdown) = common::spawn_single_db_handler(auth, registry).await;

    bolt_send(&mut writer, &hello_no_auth()).await;
    let _ = bolt_recv(&mut reader).await;

    bolt_send(&mut writer, &run_query("MATCH (n) RETURN n")).await;
    let _ = bolt_recv(&mut reader).await;

    // DISCARD instead of PULL.
    bolt_send(&mut writer, &BoltRequest::Discard { extra: vec![] }).await;
    let resp = bolt_recv(&mut reader).await;
    assert!(
        matches!(resp, BoltResponse::Success { .. }),
        "expected SUCCESS for DISCARD"
    );
    assert_eq!(dict_bool(&resp, "has_more"), Some(false));
}
#[tokio::test]
async fn begin_before_database_bind_fails_request_invalid() {
    // A BEGIN with no prior RUN has no bound database to open a transaction on.
    let auth = Arc::new(NoAuthProvider);
    let (registry, _tmp) = common::single_db_registry().await;
    let (mut writer, mut reader, _shutdown) = common::spawn_single_db_handler(auth, registry).await;

    bolt_send(&mut writer, &hello_no_auth()).await;
    let _ = bolt_recv(&mut reader).await;

    bolt_send(&mut writer, &BoltRequest::Begin { extra: vec![] }).await;
    let resp = bolt_recv(&mut reader).await;
    assert!(
        matches!(resp, BoltResponse::Failure { .. }),
        "expected FAILURE for BEGIN before any database bind"
    );
    let msg = dict_str(&resp, "message").unwrap_or_default();
    assert!(
        msg.contains("no database selected"),
        "expected 'no database selected' message, got: {msg}"
    );
}
#[tokio::test]
async fn begin_commit_after_bind_succeeds() {
    let auth = Arc::new(NoAuthProvider);
    let (registry, _tmp) = common::single_db_registry().await;
    let (mut writer, mut reader, _shutdown) = common::spawn_single_db_handler(auth, registry).await;

    bolt_send(&mut writer, &hello_no_auth()).await;
    let _ = bolt_recv(&mut reader).await;
    // Bind a database with a first RUN.
    bolt_send(&mut writer, &run_query("MATCH (n) RETURN n")).await;
    let _ = bolt_recv(&mut reader).await;

    bolt_send(&mut writer, &BoltRequest::Begin { extra: vec![] }).await;
    let begin = bolt_recv(&mut reader).await;
    assert!(
        matches!(begin, BoltResponse::Success { .. }),
        "BEGIN should succeed"
    );

    bolt_send(&mut writer, &BoltRequest::Commit).await;
    let commit = bolt_recv(&mut reader).await;
    assert!(
        matches!(commit, BoltResponse::Success { .. }),
        "COMMIT should succeed"
    );
}
#[tokio::test]
async fn begin_rollback_after_bind_succeeds() {
    let auth = Arc::new(NoAuthProvider);
    let (registry, _tmp) = common::single_db_registry().await;
    let (mut writer, mut reader, _shutdown) = common::spawn_single_db_handler(auth, registry).await;

    bolt_send(&mut writer, &hello_no_auth()).await;
    let _ = bolt_recv(&mut reader).await;
    bolt_send(&mut writer, &run_query("MATCH (n) RETURN n")).await;
    let _ = bolt_recv(&mut reader).await;

    bolt_send(&mut writer, &BoltRequest::Begin { extra: vec![] }).await;
    let _ = bolt_recv(&mut reader).await;
    bolt_send(&mut writer, &BoltRequest::Rollback).await;
    let resp = bolt_recv(&mut reader).await;
    assert!(
        matches!(resp, BoltResponse::Success { .. }),
        "ROLLBACK should succeed"
    );
}
#[tokio::test]
async fn commit_without_begin_fails_request_invalid() {
    let auth = Arc::new(NoAuthProvider);
    let (registry, _tmp) = common::single_db_registry().await;
    let (mut writer, mut reader, _shutdown) = common::spawn_single_db_handler(auth, registry).await;

    bolt_send(&mut writer, &hello_no_auth()).await;
    let _ = bolt_recv(&mut reader).await;
    bolt_send(&mut writer, &run_query("MATCH (n) RETURN n")).await;
    let _ = bolt_recv(&mut reader).await;

    bolt_send(&mut writer, &BoltRequest::Commit).await;
    let resp = bolt_recv(&mut reader).await;
    assert!(
        matches!(resp, BoltResponse::Failure { .. }),
        "COMMIT without BEGIN must fail"
    );
    let msg = dict_str(&resp, "message").unwrap_or_default();
    assert!(msg.contains("no open transaction"), "got: {msg}");
}
#[tokio::test]
async fn run_inside_open_txn_executes_in_txn_snapshot() {
    // Phase 5: a RUN inside an open explicit transaction now executes against
    // the transaction's MVCC snapshot instead of being rejected. The write is
    // pending until COMMIT (isolation preserved by the snapshot, not by
    // refusing the statement).
    let auth = Arc::new(NoAuthProvider);
    let (registry, _tmp) = common::single_db_registry().await;
    let (mut writer, mut reader, _shutdown) = common::spawn_single_db_handler(auth, registry).await;

    bolt_send(&mut writer, &hello_no_auth()).await;
    let _ = bolt_recv(&mut reader).await;
    bolt_send(&mut writer, &run_query("MATCH (n) RETURN n")).await;
    let _ = bolt_recv(&mut reader).await;

    bolt_send(&mut writer, &BoltRequest::Begin { extra: vec![] }).await;
    let _ = bolt_recv(&mut reader).await;

    bolt_send(&mut writer, &run_query("CREATE (n:N)")).await;
    let resp = bolt_recv(&mut reader).await;
    assert!(
        matches!(resp, BoltResponse::Success { .. }),
        "RUN inside an open explicit transaction must now succeed, got {resp:?}"
    );
}
#[tokio::test]
async fn run_inside_txn_sees_own_uncommitted_write() {
    // Cycle 26: read-your-writes across two RUNs in the same open transaction.
    // BEGIN; RUN CREATE; RUN MATCH — the second RUN sees the node the first
    // created, before any COMMIT.
    let auth = Arc::new(NoAuthProvider);
    let (registry, _tmp) = common::single_db_registry().await;
    let (mut writer, mut reader, _shutdown) = common::spawn_single_db_handler(auth, registry).await;

    bolt_send(&mut writer, &hello_no_auth()).await;
    let _ = bolt_recv(&mut reader).await;

    // Bind the session's database before BEGIN (BEGIN carries no `db`; the
    // per-session accessor is selected by the first RUN's `extra["db"]`).
    bolt_send(&mut writer, &run_query("MATCH (n) RETURN n")).await;
    let _ = bolt_recv(&mut reader).await;

    bolt_send(&mut writer, &BoltRequest::Begin { extra: vec![] }).await;
    let _ = bolt_recv(&mut reader).await;

    bolt_send(&mut writer, &run_query("CREATE (n:Persona)")).await;
    assert!(matches!(
        bolt_recv(&mut reader).await,
        BoltResponse::Success { .. }
    ));
    bolt_send(&mut writer, &pull()).await;
    let mut sink = vec![];
    let _ = collect_records(bolt_recv(&mut reader).await, &mut sink, &mut reader).await;

    // Second RUN in the same txn: the MATCH must see the pending node.
    bolt_send(&mut writer, &run_query("MATCH (n:Persona) RETURN n")).await;
    assert!(matches!(
        bolt_recv(&mut reader).await,
        BoltResponse::Success { .. }
    ));
    bolt_send(&mut writer, &pull()).await;
    let mut records = vec![];
    let _ = collect_records(bolt_recv(&mut reader).await, &mut records, &mut reader).await;
    assert_eq!(
        records.len(),
        1,
        "second RUN must see the txn's own pending node"
    );
}
#[tokio::test]
async fn run_inside_txn_not_visible_to_other_autocommit_session_until_commit() {
    // Cycle 27: isolation across connections. A pending write in one connection's
    // open transaction is invisible to a second auto-commit connection until the
    // first COMMITs.
    let auth = Arc::new(NoAuthProvider);
    let (registry, _tmp) = common::single_db_registry().await;

    let (mut w1, mut r1, _s1) =
        common::spawn_single_db_handler(Arc::clone(&auth), Arc::clone(&registry)).await;
    let (mut w2, mut r2, _s2) =
        common::spawn_single_db_handler(Arc::clone(&auth), Arc::clone(&registry)).await;

    bolt_send(&mut w1, &hello_no_auth()).await;
    let _ = bolt_recv(&mut r1).await;
    bolt_send(&mut w2, &hello_no_auth()).await;
    let _ = bolt_recv(&mut r2).await;

    // Connection 1: bind DB (first RUN), then BEGIN + CREATE (pending).
    bolt_send(&mut w1, &run_query("MATCH (n) RETURN n")).await;
    let _ = bolt_recv(&mut r1).await;
    bolt_send(&mut w1, &BoltRequest::Begin { extra: vec![] }).await;
    let _ = bolt_recv(&mut r1).await;
    bolt_send(&mut w1, &run_query("CREATE (n:Persona)")).await;
    assert!(matches!(
        bolt_recv(&mut r1).await,
        BoltResponse::Success { .. }
    ));
    bolt_send(&mut w1, &pull()).await;
    let mut s = vec![];
    let _ = collect_records(bolt_recv(&mut r1).await, &mut s, &mut r1).await;

    // Connection 2 (auto-commit): must NOT see the pending node.
    bolt_send(&mut w2, &run_query("MATCH (n:Persona) RETURN n")).await;
    assert!(matches!(
        bolt_recv(&mut r2).await,
        BoltResponse::Success { .. }
    ));
    bolt_send(&mut w2, &pull()).await;
    let mut before = vec![];
    let _ = collect_records(bolt_recv(&mut r2).await, &mut before, &mut r2).await;
    assert_eq!(
        before.len(),
        0,
        "pending write invisible to other session before COMMIT"
    );

    // Connection 1: COMMIT.
    bolt_send(&mut w1, &BoltRequest::Commit).await;
    assert!(matches!(
        bolt_recv(&mut r1).await,
        BoltResponse::Success { .. }
    ));

    // Connection 2: now sees it.
    bolt_send(&mut w2, &run_query("MATCH (n:Persona) RETURN n")).await;
    assert!(matches!(
        bolt_recv(&mut r2).await,
        BoltResponse::Success { .. }
    ));
    bolt_send(&mut w2, &pull()).await;
    let mut after = vec![];
    let _ = collect_records(bolt_recv(&mut r2).await, &mut after, &mut r2).await;
    assert_eq!(after.len(), 1, "committed write visible to other session");
}
#[tokio::test]
async fn begin_create_rollback_discards_pending_write() {
    // Cycle 27: ROLLBACK discards the transaction's pending writes.
    let auth = Arc::new(NoAuthProvider);
    let (registry, _tmp) = common::single_db_registry().await;
    let (mut writer, mut reader, _shutdown) = common::spawn_single_db_handler(auth, registry).await;

    bolt_send(&mut writer, &hello_no_auth()).await;
    let _ = bolt_recv(&mut reader).await;

    // Bind DB before BEGIN.
    bolt_send(&mut writer, &run_query("MATCH (n) RETURN n")).await;
    let _ = bolt_recv(&mut reader).await;

    bolt_send(&mut writer, &BoltRequest::Begin { extra: vec![] }).await;
    let _ = bolt_recv(&mut reader).await;
    bolt_send(&mut writer, &run_query("CREATE (n:Persona)")).await;
    assert!(matches!(
        bolt_recv(&mut reader).await,
        BoltResponse::Success { .. }
    ));
    bolt_send(&mut writer, &pull()).await;
    let mut s = vec![];
    let _ = collect_records(bolt_recv(&mut reader).await, &mut s, &mut reader).await;

    bolt_send(&mut writer, &BoltRequest::Rollback).await;
    assert!(matches!(
        bolt_recv(&mut reader).await,
        BoltResponse::Success { .. }
    ));

    // Auto-commit read after rollback: nothing persisted.
    bolt_send(&mut writer, &run_query("MATCH (n:Persona) RETURN n")).await;
    assert!(matches!(
        bolt_recv(&mut reader).await,
        BoltResponse::Success { .. }
    ));
    bolt_send(&mut writer, &pull()).await;
    let mut records = vec![];
    let _ = collect_records(bolt_recv(&mut reader).await, &mut records, &mut reader).await;
    assert_eq!(records.len(), 0, "ROLLBACK must discard the pending write");
}
#[tokio::test]
async fn run_error_inside_txn_then_reset_abandons_txn_cleanly() {
    // Cycle 28: an invalid RUN inside a txn fails, and — per Bolt — every
    // subsequent message except RESET/GOODBYE is IGNORED until the client
    // sends RESET. RESET rolls back the open transaction (Bolt semantics), so
    // after RESET the connection is clean and back in auto-commit: a fresh RUN
    // succeeds and nothing from the aborted transaction persisted. This is the
    // real observable Bolt flow; a RUN cannot "continue" a txn after an error
    // without a RESET, and RESET ends the txn.
    let auth = Arc::new(NoAuthProvider);
    let (registry, _tmp) = common::single_db_registry().await;
    let (mut writer, mut reader, _shutdown) = common::spawn_single_db_handler(auth, registry).await;

    bolt_send(&mut writer, &hello_no_auth()).await;
    let _ = bolt_recv(&mut reader).await;

    // Bind DB before BEGIN.
    bolt_send(&mut writer, &run_query("MATCH (n) RETURN n")).await;
    let _ = bolt_recv(&mut reader).await;

    bolt_send(&mut writer, &BoltRequest::Begin { extra: vec![] }).await;
    let _ = bolt_recv(&mut reader).await;

    // A valid write in the txn (pending), then an invalid statement fails.
    bolt_send(&mut writer, &run_query("CREATE (n:Persona)")).await;
    assert!(matches!(
        bolt_recv(&mut reader).await,
        BoltResponse::Success { .. }
    ));
    bolt_send(&mut writer, &pull()).await;
    let mut s = vec![];
    let _ = collect_records(bolt_recv(&mut reader).await, &mut s, &mut reader).await;

    bolt_send(&mut writer, &run_query("THIS IS NOT CYPHER")).await;
    let bad = bolt_recv(&mut reader).await;
    assert!(
        matches!(bad, BoltResponse::Failure { .. }),
        "invalid query must fail"
    );

    // In FAILED state, a further RUN is IGNORED (not executed).
    bolt_send(&mut writer, &run_query("CREATE (m:Otro)")).await;
    let ignored = bolt_recv(&mut reader).await;
    assert!(
        matches!(ignored, BoltResponse::Ignored),
        "after a failure, statements are ignored until RESET, got {ignored:?}"
    );

    // RESET clears the failure and rolls back the open txn.
    bolt_send(&mut writer, &BoltRequest::Reset).await;
    assert!(matches!(
        bolt_recv(&mut reader).await,
        BoltResponse::Success { .. }
    ));

    // Back in auto-commit: a fresh read shows the aborted txn persisted nothing.
    bolt_send(&mut writer, &run_query("MATCH (n:Persona) RETURN n")).await;
    assert!(matches!(
        bolt_recv(&mut reader).await,
        BoltResponse::Success { .. }
    ));
    bolt_send(&mut writer, &pull()).await;
    let mut records = vec![];
    let _ = collect_records(bolt_recv(&mut reader).await, &mut records, &mut reader).await;
    assert_eq!(
        records.len(),
        0,
        "RESET rolled back the txn; nothing persisted"
    );
}
#[tokio::test]
async fn txn_mutation_summary_counters_match_autocommit_shape() {
    // A mutation inside a txn reports its counters the same way as auto-commit:
    // no data row, counts in the final PULL SUCCESS `stats` dict (approach A).
    let auth = Arc::new(NoAuthProvider);
    let (registry, _tmp) = common::single_db_registry().await;
    let (mut writer, mut reader, _shutdown) = common::spawn_single_db_handler(auth, registry).await;

    bolt_send(&mut writer, &hello_no_auth()).await;
    let _ = bolt_recv(&mut reader).await;

    // Bind DB before BEGIN.
    bolt_send(&mut writer, &run_query("MATCH (n) RETURN n")).await;
    let _ = bolt_recv(&mut reader).await;

    bolt_send(&mut writer, &BoltRequest::Begin { extra: vec![] }).await;
    let _ = bolt_recv(&mut reader).await;

    bolt_send(&mut writer, &run_query("CREATE (n:A), (m:B)")).await;
    assert!(matches!(
        bolt_recv(&mut reader).await,
        BoltResponse::Success { .. }
    ));
    bolt_send(&mut writer, &pull()).await;
    let mut records = vec![];
    let final_resp = collect_records(bolt_recv(&mut reader).await, &mut records, &mut reader).await;
    assert!(
        records.is_empty(),
        "no data row inside the txn, got {records:?}"
    );
    let stats = dict_dict(&final_resp, "stats").expect("txn mutation must carry a stats dict");
    assert_eq!(
        stats_int(&stats, "nodes-created"),
        Some(2),
        "two nodes created inside the txn"
    );
    assert_eq!(stats_int(&stats, "labels-added"), Some(2), "labels A and B");

    bolt_send(&mut writer, &BoltRequest::Rollback).await;
    let _ = bolt_recv(&mut reader).await;
}
#[tokio::test]
async fn ddl_and_call_inside_txn_are_rejected() {
    // Cycle 25 boundary: DDL/CALL/Admin are not part of the transactional
    // read/write path, so inside an open transaction they are rejected with a
    // clear error instead of silently running in auto-commit.
    let auth = Arc::new(NoAuthProvider);
    let (registry, _tmp) = common::single_db_registry().await;
    let (mut writer, mut reader, _shutdown) = common::spawn_single_db_handler(auth, registry).await;

    bolt_send(&mut writer, &hello_no_auth()).await;
    let _ = bolt_recv(&mut reader).await;

    // Bind DB before BEGIN.
    bolt_send(&mut writer, &run_query("MATCH (n) RETURN n")).await;
    let _ = bolt_recv(&mut reader).await;

    bolt_send(&mut writer, &BoltRequest::Begin { extra: vec![] }).await;
    let _ = bolt_recv(&mut reader).await;

    // A CALL inside the txn is rejected (uses a procedure the parser accepts so
    // the statement reaches the txn guard, not a parse error).
    bolt_send(
        &mut writer,
        &run_query(
            "CALL mg.vertex_labels() YIELD vertex_labels \
             UNWIND vertex_labels AS vl RETURN vl",
        ),
    )
    .await;
    let call_resp = bolt_recv(&mut reader).await;
    assert!(
        matches!(call_resp, BoltResponse::Failure { .. }),
        "CALL inside a txn must be rejected, got {call_resp:?}"
    );
    let msg = dict_str(&call_resp, "message").unwrap_or_default();
    assert!(
        msg.contains("not supported inside an explicit transaction"),
        "expected the txn-guard message, got: {msg}"
    );

    // RESET to clear the failure, then confirm a DDL inside a fresh txn is also
    // rejected.
    bolt_send(&mut writer, &BoltRequest::Reset).await;
    let _ = bolt_recv(&mut reader).await;
    bolt_send(&mut writer, &run_query("MATCH (n) RETURN n")).await;
    let _ = bolt_recv(&mut reader).await;
    bolt_send(&mut writer, &BoltRequest::Begin { extra: vec![] }).await;
    let _ = bolt_recv(&mut reader).await;

    bolt_send(
        &mut writer,
        &run_query("CREATE INDEX FOR (n:Person) ON (n.name)"),
    )
    .await;
    let ddl_resp = bolt_recv(&mut reader).await;
    assert!(
        matches!(ddl_resp, BoltResponse::Failure { .. }),
        "DDL inside a txn must be rejected, got {ddl_resp:?}"
    );

    bolt_send(&mut writer, &BoltRequest::Reset).await;
    let _ = bolt_recv(&mut reader).await;
}
#[tokio::test]
async fn pull_without_run_returns_success_no_records() {
    let auth = Arc::new(NoAuthProvider);
    let (registry, _tmp) = common::single_db_registry().await;
    let (mut writer, mut reader, _shutdown) = common::spawn_single_db_handler(auth, registry).await;

    bolt_send(&mut writer, &hello_no_auth()).await;
    let _ = bolt_recv(&mut reader).await;

    // PULL without a prior RUN should succeed with has_more=false.
    bolt_send(&mut writer, &pull()).await;
    let resp = bolt_recv(&mut reader).await;
    assert!(
        matches!(resp, BoltResponse::Success { .. }),
        "expected SUCCESS for orphan PULL"
    );
    assert_eq!(dict_bool(&resp, "has_more"), Some(false));
}
async fn collect_records(
    first: BoltResponse,
    records: &mut Vec<BoltResponse>,
    reader: &mut ermya_graph_protocol::bolt_frame::BoltChunkedReader<
        tokio::io::ReadHalf<tokio::io::DuplexStream>,
    >,
) -> BoltResponse {
    let mut current = first;
    loop {
        match current {
            BoltResponse::Record { .. } => {
                records.push(current);
                current = bolt_recv(reader).await;
            }
            _ => return current,
        }
    }
}
async fn fresh_system_provider_with_user(
    username: &str,
    password: &str,
) -> std::sync::Arc<ermya_graph_server::auth::SystemGraphAuthProvider> {
    use ermya_graph_server::auth::{
        SecretString, SystemGraphAuthProvider, SystemGraphAuthStore, UserStore,
    };
    let graph = std::sync::Arc::new(std::sync::RwLock::new(ermya_graph::Graph::new()));
    let store = std::sync::Arc::new(SystemGraphAuthStore::new(graph).unwrap());
    let pw = SecretString::new(password.to_owned());
    store.create_user(username, &pw, false).await.unwrap();
    std::sync::Arc::new(SystemGraphAuthProvider::from_store(store))
}
#[tokio::test]
async fn hello_with_system_graph_valid_credentials_succeeds() {
    let auth = fresh_system_provider_with_user("alice", "hunter22!x").await;
    let (registry, _tmp) = common::single_db_registry().await;
    let (mut writer, mut reader, _shutdown) = common::spawn_single_db_handler(auth, registry).await;

    bolt_send(&mut writer, &hello_request("alice", "hunter22!x")).await;
    let resp = bolt_recv(&mut reader).await;
    assert!(
        matches!(resp, BoltResponse::Success { .. }),
        "expected SUCCESS, got {resp:?}"
    );
}
#[tokio::test]
async fn hello_with_system_graph_wrong_password_fails() {
    let auth = fresh_system_provider_with_user("alice", "hunter22!x").await;
    let (registry, _tmp) = common::single_db_registry().await;
    let (mut writer, mut reader, _shutdown) = common::spawn_single_db_handler(auth, registry).await;

    bolt_send(&mut writer, &hello_request("alice", "wrong-password")).await;
    let resp = bolt_recv(&mut reader).await;
    assert!(
        matches!(resp, BoltResponse::Failure { .. }),
        "expected FAILURE, got {resp:?}"
    );
    let code = dict_str(&resp, "code").expect("code");
    assert!(
        code.contains("Unauthorized"),
        "expected Unauthorized, got {code}"
    );
}
#[tokio::test]
async fn hello_with_system_graph_unknown_user_fails() {
    let auth = fresh_system_provider_with_user("alice", "hunter22!x").await;
    let (registry, _tmp) = common::single_db_registry().await;
    let (mut writer, mut reader, _shutdown) = common::spawn_single_db_handler(auth, registry).await;

    bolt_send(&mut writer, &hello_request("ghost", "anything")).await;
    let resp = bolt_recv(&mut reader).await;
    assert!(
        matches!(resp, BoltResponse::Failure { .. }),
        "expected FAILURE, got {resp:?}"
    );
}
#[tokio::test]
async fn parser_error_message_preserves_position_for_user() {
    // Montaje de una sola base: esta prueba es del ANALIZADOR —que el error
    // señale la posición—, no del catálogo ni de los permisos.
    let (mut writer, mut reader, _shutdown, _audit_tx, _tmp, _audit) =
        common::fresh_community_handler_with_audit_file("alice").await;
    common::send_ok_hello(&mut writer, &mut reader, "alice", "plantA").await;

    common::bolt_send(&mut writer, &common::run_message("THIS IS NOT VALID GQL")).await;
    let resp = common::bolt_recv(&mut reader).await;
    let msg = dict_str(&resp, "message").unwrap_or_default();
    assert!(
        msg.contains("line") && msg.contains("col"),
        "parser errors must carry position info for the user; got: {msg}"
    );
}
fn find_access_denied(events: &[serde_json::Value]) -> Option<&serde_json::Value> {
    events
        .iter()
        .find(|e| e["event_type"].as_str() == Some("access_denied"))
}
#[tokio::test]
async fn run_before_hello_emits_access_denied_not_authenticated() {
    let (mut writer, mut reader, _shutdown, audit_tx, _tmp, audit_path) =
        common::fresh_community_handler_with_audit_file("alice").await;
    common::bolt_send(&mut writer, &common::run_message("MATCH (n) RETURN n")).await;
    let _ = common::bolt_recv(&mut reader).await;

    let events = common::read_audit_events(&audit_tx, &audit_path).await;
    let event = find_access_denied(&events)
        .unwrap_or_else(|| panic!("expected access_denied event, got events: {events:#?}"));
    assert_eq!(
        event["details"]["reason"].as_str(),
        Some("not_authenticated"),
        "wrong reason in {event:#}"
    );
    // No DbHandle yet → database field is omitted (skip_if_none).
    assert!(
        event["details"].get("database").is_none(),
        "expected database absent before HELLO, got: {event:#}"
    );
}
/// Build a RUN with named/positional parameters as a Bolt dict, bound to the
/// default test database (every RUN must carry `db` post-Plan-B).
fn run_with_params(query: &str, params: Vec<(&str, PackStreamValue)>) -> BoltRequest {
    BoltRequest::Run {
        query: query.to_owned(),
        params: params.into_iter().map(|(k, v)| (k.to_owned(), v)).collect(),
        extra: vec![(
            "db".to_owned(),
            PackStreamValue::String(common::DEFAULT_TEST_DB.to_owned()),
        )],
    }
}
#[tokio::test]
async fn handler_run_with_named_param_substitutes_value() {
    // RUN "RETURN $x" with params {"x": 99} must emit one RECORD with Int(99).
    let auth = Arc::new(NoAuthProvider);
    let (registry, _tmp) = common::single_db_registry().await;
    let (mut writer, mut reader, _shutdown) = common::spawn_single_db_handler(auth, registry).await;

    bolt_send(&mut writer, &hello_no_auth()).await;
    let _ = bolt_recv(&mut reader).await;

    bolt_send(
        &mut writer,
        &run_with_params("RETURN $x", vec![("x", PackStreamValue::Int(99))]),
    )
    .await;
    let run_resp = bolt_recv(&mut reader).await;
    assert!(
        matches!(run_resp, BoltResponse::Success { .. }),
        "expected SUCCESS for parametrised RUN, got {run_resp:?}"
    );

    bolt_send(&mut writer, &pull()).await;
    let first = bolt_recv(&mut reader).await;
    let mut records = vec![];
    let final_resp = collect_records(first, &mut records, &mut reader).await;
    assert!(
        matches!(final_resp, BoltResponse::Success { .. }),
        "expected trailing SUCCESS, got {final_resp:?}"
    );
    assert_eq!(
        records.len(),
        1,
        "expected exactly one RECORD for RETURN $x"
    );
    let BoltResponse::Record { fields } = &records[0] else {
        panic!("expected Record, got {:?}", records[0]);
    };
    assert_eq!(
        fields.as_slice(),
        &[PackStreamValue::Int(99)],
        "expected the substituted Int(99) value"
    );
}
#[tokio::test]
async fn handler_run_with_positional_param_substitutes_value() {
    // RUN "RETURN $1" with params {"1": "hello"} must emit one RECORD with Str("hello").
    let auth = Arc::new(NoAuthProvider);
    let (registry, _tmp) = common::single_db_registry().await;
    let (mut writer, mut reader, _shutdown) = common::spawn_single_db_handler(auth, registry).await;

    bolt_send(&mut writer, &hello_no_auth()).await;
    let _ = bolt_recv(&mut reader).await;

    bolt_send(
        &mut writer,
        &run_with_params(
            "RETURN $1",
            vec![("1", PackStreamValue::String("hello".to_owned()))],
        ),
    )
    .await;
    let run_resp = bolt_recv(&mut reader).await;
    assert!(
        matches!(run_resp, BoltResponse::Success { .. }),
        "expected SUCCESS for positional parametrised RUN, got {run_resp:?}"
    );

    bolt_send(&mut writer, &pull()).await;
    let first = bolt_recv(&mut reader).await;
    let mut records = vec![];
    let final_resp = collect_records(first, &mut records, &mut reader).await;
    assert!(
        matches!(final_resp, BoltResponse::Success { .. }),
        "expected trailing SUCCESS, got {final_resp:?}"
    );
    assert_eq!(
        records.len(),
        1,
        "expected exactly one RECORD for RETURN $1"
    );
    let BoltResponse::Record { fields } = &records[0] else {
        panic!("expected Record, got {:?}", records[0]);
    };
    assert_eq!(
        fields.as_slice(),
        &[PackStreamValue::String("hello".to_owned())],
        "expected the substituted Str(\"hello\") value"
    );
}
#[tokio::test]
async fn handler_run_missing_param_returns_parameter_missing_wire_code() {
    // RUN referencing $id with empty params must FAIL with
    // `Neo.ClientError.Statement.ParameterMissing`.
    let auth = Arc::new(NoAuthProvider);
    let (registry, _tmp) = common::single_db_registry().await;
    let (mut writer, mut reader, _shutdown) = common::spawn_single_db_handler(auth, registry).await;

    bolt_send(&mut writer, &hello_no_auth()).await;
    let _ = bolt_recv(&mut reader).await;

    bolt_send(
        &mut writer,
        &run_with_params("MATCH (n) WHERE n.id = $id RETURN n", vec![]),
    )
    .await;
    let resp = bolt_recv(&mut reader).await;
    assert!(
        matches!(resp, BoltResponse::Failure { .. }),
        "expected FAILURE for missing param, got {resp:?}"
    );
    let code = dict_str(&resp, "code").expect("code");
    assert_eq!(
        code, "Neo.ClientError.Statement.ParameterMissing",
        "expected ParameterMissing wire code, got {code}"
    );
    let msg = dict_str(&resp, "message").unwrap_or_default();
    assert!(
        msg.contains("id"),
        "expected the missing parameter name in the failure message, got {msg}"
    );
}
#[tokio::test]
async fn handler_run_const_return_no_params() {
    // RUN "RETURN 1" with empty params (standard driver keep-alive) must
    // succeed and emit one RECORD with Int(1). Confirms the params path
    // does not regress the no-param ConstReturn case.
    let auth = Arc::new(NoAuthProvider);
    let (registry, _tmp) = common::single_db_registry().await;
    let (mut writer, mut reader, _shutdown) = common::spawn_single_db_handler(auth, registry).await;

    bolt_send(&mut writer, &hello_no_auth()).await;
    let _ = bolt_recv(&mut reader).await;

    bolt_send(&mut writer, &run_with_params("RETURN 1", vec![])).await;
    let run_resp = bolt_recv(&mut reader).await;
    assert!(
        matches!(run_resp, BoltResponse::Success { .. }),
        "expected SUCCESS for empty-params RETURN 1, got {run_resp:?}"
    );

    bolt_send(&mut writer, &pull()).await;
    let first = bolt_recv(&mut reader).await;
    let mut records = vec![];
    let final_resp = collect_records(first, &mut records, &mut reader).await;
    assert!(
        matches!(final_resp, BoltResponse::Success { .. }),
        "expected trailing SUCCESS, got {final_resp:?}"
    );
    assert_eq!(records.len(), 1, "expected exactly one RECORD for RETURN 1");
    let BoltResponse::Record { fields } = &records[0] else {
        panic!("expected Record, got {:?}", records[0]);
    };
    assert_eq!(
        fields.as_slice(),
        &[PackStreamValue::Int(1)],
        "expected Int(1) for RETURN 1"
    );
}
#[tokio::test]
async fn run_exceeding_result_cap_returns_result_exhausted() {
    let auth = Arc::new(NoAuthProvider);
    let (registry, _tmp) = common::single_db_registry().await;
    let (mut writer, mut reader, _shutdown) =
        common::spawn_bolt_handler_with_cap(Arc::clone(&auth), Arc::clone(&registry), 3).await;

    bolt_send(&mut writer, &hello_no_auth()).await;
    let _ = bolt_recv(&mut reader).await;

    // Seed 10 nodes — above the cap of 3. A literal list is used (not
    // `range()`) because `UNWIND range(...) CREATE` does not persist via
    // the mutation path — see error-log 2026-05-27.
    bolt_send(
        &mut writer,
        &run_query("UNWIND [1,2,3,4,5,6,7,8,9,10] AS i CREATE (:N {i:i})"),
    )
    .await;
    assert!(
        matches!(bolt_recv(&mut reader).await, BoltResponse::Success { .. }),
        "expected SUCCESS for the seeding CREATE"
    );
    bolt_send(&mut writer, &pull()).await;
    let _ = collect_records(bolt_recv(&mut reader).await, &mut vec![], &mut reader).await;

    // MATCH all 10 — output exceeds cap 3 → ResultExhausted at RUN time.
    bolt_send(&mut writer, &run_query("MATCH (n:N) RETURN n")).await;
    let resp = bolt_recv(&mut reader).await;
    assert!(
        matches!(resp, BoltResponse::Failure { .. }),
        "over-cap query must FAIL at RUN, got {resp:?}"
    );
    assert_eq!(
        dict_str(&resp, "code").as_deref(),
        Some("Neo.ClientError.General.ResultExhausted"),
        "over-cap query must surface ResultExhausted, got {resp:?}"
    );
}
#[tokio::test]
async fn run_under_result_cap_succeeds() {
    let auth = Arc::new(NoAuthProvider);
    let (registry, _tmp) = common::single_db_registry().await;
    let (mut writer, mut reader, _shutdown) =
        common::spawn_bolt_handler_with_cap(Arc::clone(&auth), Arc::clone(&registry), 100).await;

    bolt_send(&mut writer, &hello_no_auth()).await;
    let _ = bolt_recv(&mut reader).await;

    // Literal list (not `range()`) — see error-log 2026-05-27.
    bolt_send(
        &mut writer,
        &run_query("UNWIND [1,2,3] AS i CREATE (:N {i:i})"),
    )
    .await;
    let create_resp = bolt_recv(&mut reader).await;
    assert!(
        matches!(create_resp, BoltResponse::Success { .. }),
        "expected SUCCESS for the seeding CREATE, got {create_resp:?}"
    );
    bolt_send(&mut writer, &pull()).await;
    let _ = collect_records(bolt_recv(&mut reader).await, &mut vec![], &mut reader).await;

    bolt_send(&mut writer, &run_query("MATCH (n:N) RETURN n")).await;
    let run_resp = bolt_recv(&mut reader).await;
    assert!(
        matches!(run_resp, BoltResponse::Success { .. }),
        "under-cap query must succeed at RUN, got {run_resp:?}"
    );

    bolt_send(&mut writer, &pull()).await;
    let mut records = vec![];
    let final_resp = collect_records(bolt_recv(&mut reader).await, &mut records, &mut reader).await;
    assert!(
        matches!(final_resp, BoltResponse::Success { .. }),
        "expected trailing SUCCESS after PULL, got {final_resp:?}"
    );
    assert_eq!(
        records.len(),
        3,
        "under-cap MATCH returns all 3 rows, got {} (records={records:?})",
        records.len()
    );
}
/// Build a `PULL {n: <n>}` request — drives one incremental fetch batch.
fn pull_n(n: i64) -> BoltRequest {
    BoltRequest::Pull {
        extra: vec![("n".to_owned(), PackStreamValue::Int(n))],
    }
}
/// Drive one PULL `req`, collecting Records until the trailing Success.
/// Returns `(record_count, has_more)`. `has_more` is read from the
/// Success metadata.
async fn pull_batch(
    writer: &mut ermya_graph_protocol::bolt_frame::BoltChunkedWriter<
        tokio::io::WriteHalf<tokio::io::DuplexStream>,
    >,
    reader: &mut ermya_graph_protocol::bolt_frame::BoltChunkedReader<
        tokio::io::ReadHalf<tokio::io::DuplexStream>,
    >,
    req: &BoltRequest,
) -> (usize, bool) {
    bolt_send(writer, req).await;
    let mut records = vec![];
    let final_resp = collect_records(bolt_recv(reader).await, &mut records, reader).await;
    let has_more =
        dict_bool(&final_resp, "has_more").expect("PULL Success must carry has_more metadata");
    (records.len(), has_more)
}
/// Seed `n` :N nodes over an authenticated session via a literal-list
/// UNWIND CREATE (not `range()` — see error-log 2026-05-27), draining
/// the CREATE result. Leaves the session ready for a MATCH.
async fn seed_n_nodes(
    writer: &mut ermya_graph_protocol::bolt_frame::BoltChunkedWriter<
        tokio::io::WriteHalf<tokio::io::DuplexStream>,
    >,
    reader: &mut ermya_graph_protocol::bolt_frame::BoltChunkedReader<
        tokio::io::ReadHalf<tokio::io::DuplexStream>,
    >,
    n: usize,
) {
    let list = (1..=n).map(|i| i.to_string()).collect::<Vec<_>>().join(",");
    bolt_send(
        writer,
        &run_query(&format!("UNWIND [{list}] AS i CREATE (:N {{i:i}})")),
    )
    .await;
    assert!(
        matches!(bolt_recv(reader).await, BoltResponse::Success { .. }),
        "seeding CREATE must succeed"
    );
    bolt_send(writer, &pull()).await;
    let _ = collect_records(bolt_recv(reader).await, &mut vec![], reader).await;
}
#[tokio::test]
async fn pull_respects_n_and_signals_has_more() {
    let auth = Arc::new(NoAuthProvider);
    let (registry, _tmp) = common::single_db_registry().await;
    let (mut writer, mut reader, _shutdown) =
        common::spawn_single_db_handler(Arc::clone(&auth), Arc::clone(&registry)).await;
    bolt_send(&mut writer, &hello_no_auth()).await;
    let _ = bolt_recv(&mut reader).await;

    seed_n_nodes(&mut writer, &mut reader, 5).await;
    bolt_send(&mut writer, &run_query("MATCH (n:N) RETURN n.i AS i")).await;
    let run_resp = bolt_recv(&mut reader).await;
    assert!(
        matches!(run_resp, BoltResponse::Success { .. }),
        "RUN MATCH must succeed, got {run_resp:?}"
    );

    // 5 rows fetched 2 at a time: (2, more), (2, more), (1, drained).
    assert_eq!(
        pull_batch(&mut writer, &mut reader, &pull_n(2)).await,
        (2, true),
        "batch 1/3: PULL n=2 of 5 → 2 rows + has_more=true"
    );
    assert_eq!(
        pull_batch(&mut writer, &mut reader, &pull_n(2)).await,
        (2, true),
        "batch 2/3: PULL n=2 of remaining 3 → 2 rows + has_more=true"
    );
    assert_eq!(
        pull_batch(&mut writer, &mut reader, &pull_n(2)).await,
        (1, false),
        "batch 3/3: PULL n=2 of remaining 1 → 1 row + has_more=false (drained)"
    );

    // Stream drained — a further PULL yields nothing and has_more=false.
    assert_eq!(
        pull_batch(&mut writer, &mut reader, &pull_n(2)).await,
        (0, false),
        "post-drain PULL n=2 → 0 rows + has_more=false"
    );
}
#[tokio::test]
async fn pull_without_n_returns_all_for_legacy_clients() {
    let auth = Arc::new(NoAuthProvider);
    let (registry, _tmp) = common::single_db_registry().await;
    let (mut writer, mut reader, _shutdown) =
        common::spawn_single_db_handler(Arc::clone(&auth), Arc::clone(&registry)).await;
    bolt_send(&mut writer, &hello_no_auth()).await;
    let _ = bolt_recv(&mut reader).await;

    seed_n_nodes(&mut writer, &mut reader, 5).await;
    bolt_send(&mut writer, &run_query("MATCH (n:N) RETURN n.i AS i")).await;
    let run_resp = bolt_recv(&mut reader).await;
    assert!(
        matches!(run_resp, BoltResponse::Success { .. }),
        "RUN MATCH must succeed, got {run_resp:?}"
    );

    // PULL {} (n absent → -1 → all) returns all 5, has_more=false.
    assert_eq!(
        pull_batch(&mut writer, &mut reader, &pull()).await,
        (5, false),
        "legacy PULL {{}} (n absent) must return all 5 rows + has_more=false"
    );
}
#[tokio::test]
async fn pull_n_larger_than_remaining_returns_all_and_drains() {
    let auth = Arc::new(NoAuthProvider);
    let (registry, _tmp) = common::single_db_registry().await;
    let (mut writer, mut reader, _shutdown) =
        common::spawn_single_db_handler(Arc::clone(&auth), Arc::clone(&registry)).await;
    bolt_send(&mut writer, &hello_no_auth()).await;
    let _ = bolt_recv(&mut reader).await;

    seed_n_nodes(&mut writer, &mut reader, 3).await;
    bolt_send(&mut writer, &run_query("MATCH (n:N) RETURN n.i AS i")).await;
    let run_resp = bolt_recv(&mut reader).await;
    assert!(
        matches!(run_resp, BoltResponse::Success { .. }),
        "RUN MATCH must succeed, got {run_resp:?}"
    );

    // n=100 > 3 remaining → all 3, drained.
    assert_eq!(
        pull_batch(&mut writer, &mut reader, &pull_n(100)).await,
        (3, false),
        "PULL n=100 with 3 remaining → all 3 rows + has_more=false"
    );
}
/// Build a `DISCARD {n: <n>}` request.
fn discard_n(n: i64) -> BoltRequest {
    BoltRequest::Discard {
        extra: vec![("n".to_owned(), PackStreamValue::Int(n))],
    }
}
/// Build a `DISCARD {}` request (discard all remaining).
fn discard_all() -> BoltRequest {
    BoltRequest::Discard { extra: vec![] }
}
/// Drive one DISCARD `req` (which produces no Records, only a Success).
/// Returns `has_more` from the Success metadata.
async fn discard_meta(
    writer: &mut ermya_graph_protocol::bolt_frame::BoltChunkedWriter<
        tokio::io::WriteHalf<tokio::io::DuplexStream>,
    >,
    reader: &mut ermya_graph_protocol::bolt_frame::BoltChunkedReader<
        tokio::io::ReadHalf<tokio::io::DuplexStream>,
    >,
    req: &BoltRequest,
) -> bool {
    bolt_send(writer, req).await;
    let resp = bolt_recv(reader).await;
    dict_bool(&resp, "has_more").expect("DISCARD Success must carry has_more metadata")
}
#[tokio::test]
async fn discard_n_drops_partial_and_signals_has_more() {
    let auth = Arc::new(NoAuthProvider);
    let (registry, _tmp) = common::single_db_registry().await;
    let (mut writer, mut reader, _shutdown) =
        common::spawn_single_db_handler(Arc::clone(&auth), Arc::clone(&registry)).await;
    bolt_send(&mut writer, &hello_no_auth()).await;
    let _ = bolt_recv(&mut reader).await;

    seed_n_nodes(&mut writer, &mut reader, 5).await;
    bolt_send(&mut writer, &run_query("MATCH (n:N) RETURN n.i AS i")).await;
    let run_resp = bolt_recv(&mut reader).await;
    assert!(
        matches!(run_resp, BoltResponse::Success { .. }),
        "RUN MATCH must succeed, got {run_resp:?}"
    );

    // DISCARD n=2 drops 2 of 5 → has_more=true. The remaining 3 are then
    // observable via PULL.
    assert!(
        discard_meta(&mut writer, &mut reader, &discard_n(2)).await,
        "3 rows remain after discarding 2 of 5"
    );
    assert_eq!(
        pull_batch(&mut writer, &mut reader, &pull()).await,
        (3, false),
        "post-DISCARD PULL {{}} must return remaining 3 rows + has_more=false"
    );
}
#[tokio::test]
async fn discard_all_clears_stream() {
    let auth = Arc::new(NoAuthProvider);
    let (registry, _tmp) = common::single_db_registry().await;
    let (mut writer, mut reader, _shutdown) =
        common::spawn_single_db_handler(Arc::clone(&auth), Arc::clone(&registry)).await;
    bolt_send(&mut writer, &hello_no_auth()).await;
    let _ = bolt_recv(&mut reader).await;

    seed_n_nodes(&mut writer, &mut reader, 5).await;
    bolt_send(&mut writer, &run_query("MATCH (n:N) RETURN n.i AS i")).await;
    let run_resp = bolt_recv(&mut reader).await;
    assert!(
        matches!(run_resp, BoltResponse::Success { .. }),
        "RUN MATCH must succeed, got {run_resp:?}"
    );

    // DISCARD {} drops everything → has_more=false, stream cleared.
    assert!(
        !discard_meta(&mut writer, &mut reader, &discard_all()).await,
        "DISCARD {{}} must drain the stream"
    );
    // A subsequent PULL finds no pending result.
    assert_eq!(
        pull_batch(&mut writer, &mut reader, &pull()).await,
        (0, false),
        "post-DISCARD-all PULL {{}} must return 0 rows + has_more=false"
    );
}
#[tokio::test]
async fn discard_n_then_discard_rest() {
    let auth = Arc::new(NoAuthProvider);
    let (registry, _tmp) = common::single_db_registry().await;
    let (mut writer, mut reader, _shutdown) =
        common::spawn_single_db_handler(Arc::clone(&auth), Arc::clone(&registry)).await;
    bolt_send(&mut writer, &hello_no_auth()).await;
    let _ = bolt_recv(&mut reader).await;

    seed_n_nodes(&mut writer, &mut reader, 4).await;
    bolt_send(&mut writer, &run_query("MATCH (n:N) RETURN n.i AS i")).await;
    let run_resp = bolt_recv(&mut reader).await;
    assert!(
        matches!(run_resp, BoltResponse::Success { .. }),
        "RUN MATCH must succeed, got {run_resp:?}"
    );

    // DISCARD n=1 (more), DISCARD n=3 drains the rest (no more).
    assert!(
        discard_meta(&mut writer, &mut reader, &discard_n(1)).await,
        "DISCARD n=1 of 4 → 3 rows remain, has_more=true"
    );
    assert!(
        !discard_meta(&mut writer, &mut reader, &discard_n(3)).await,
        "DISCARD n=3 of remaining 3 → drained, has_more=false"
    );
}
#[tokio::test]
async fn hello_throttle_after_n_failures_returns_auth_expired() {
    // RateLimiter cap=3 (auth_max_failures_per_minute). The 4th HELLO from
    // the same peer IP must be refused with AuthorizationExpired BEFORE
    // credentials are evaluated.
    // Each HELLO attempt uses a fresh connection from the same peer IP.
    // Cumulative state lives in the shared RateLimiter inside the fixture.
    // Production parallel: an attacker opening conn → HELLO → fail → close,
    // repeated. The single-conn case is blocked by handler.failed=true
    // (a connection that fails HELLO once is dead).
    let fixture = common::AuthRateLimitFixture::new(3).await;

    // 3 HELLOs with bad creds, one per connection. Each returns
    // generic Unauthorized; the limiter records each failure.
    for i in 0..3 {
        let (mut writer, mut reader, _shutdown) = fixture.spawn_extra_handler().await;
        bolt_send(
            &mut writer,
            &common::hello_with_extras(&[("principal", "alice"), ("credentials", "wrongpass")]),
        )
        .await;
        let resp = bolt_recv(&mut reader).await;
        let code = dict_str(&resp, "code").unwrap_or_default();
        assert!(
            code.contains("Unauthorized"),
            "HELLO #{i} with bad creds must FAILURE Unauthorized, got code: {code} (resp: {resp:?})"
        );
    }

    // 4th HELLO on a new connection from the same IP → AuthorizationExpired.
    let (mut writer, mut reader, _shutdown) = fixture.spawn_extra_handler().await;
    bolt_send(
        &mut writer,
        &common::hello_with_extras(&[("principal", "alice"), ("credentials", "wrongpass")]),
    )
    .await;
    let resp = bolt_recv(&mut reader).await;
    let code = dict_str(&resp, "code").unwrap_or_default();
    assert!(
        code.contains("AuthorizationExpired"),
        "4th HELLO from the same IP must return AuthorizationExpired, got code: {code} (resp: {resp:?})"
    );

    // Drain audit log and confirm the AuthThrottled event was emitted.
    let events = fixture.drain_audit().await;
    let throttle_events: Vec<_> = events
        .iter()
        .filter(|e| {
            e.get("event_type").and_then(serde_json::Value::as_str) == Some("auth_throttled")
        })
        .collect();
    assert!(
        !throttle_events.is_empty(),
        "expected at least one auth_throttled audit event, got events: {events:#?}"
    );
}
#[tokio::test]
async fn run_throttle_after_burst_returns_too_many_requests() {
    // queries_max_per_second = 10 → bucket capacity = 20, refill = 10/sec.
    // RUN and PULL each consume one token (spec §4.1), so a tight
    // RUN-then-drain loop exhausts the bucket within a few iterations and
    // the next RUN must fail-fast with TooManyRequests. We do NOT assume an
    // exact token count (RUN+PULL dual consumption makes it implementation
    // detail); instead we assert that throttling DOES occur within a small
    // ceiling, carries the right wire code, and emits the audit event.
    let (mut writer, mut reader, _shutdown, audit_tx, _tmp, audit_path) =
        common::spawn_bolt_handler_with_query_cap(10).await;

    bolt_send(&mut writer, &hello_no_auth()).await;
    let hello_resp = bolt_recv(&mut reader).await;
    assert!(
        matches!(hello_resp, BoltResponse::Success { .. }),
        "HELLO must SUCCESS, got {hello_resp:?}"
    );

    // Within `capacity` (=20) RUNs the bucket must run dry and throttle.
    // 30 is a safe ceiling: even if PULL did not consume tokens, 20 RUNs
    // would throttle by the 21st; with PULL consuming too, it is sooner.
    let mut throttled_at = None;
    for i in 0..30 {
        bolt_send(&mut writer, &run_query("RETURN 1")).await;
        let resp = bolt_recv(&mut reader).await;
        if let BoltResponse::Failure { .. } = resp {
            let code = dict_str(&resp, "code").unwrap_or_default();
            assert!(
                code.contains("TooManyRequests"),
                "throttled RUN #{i} must carry TooManyRequests, got code: {code} (resp: {resp:?})"
            );
            throttled_at = Some(i);
            break;
        }
        // SUCCESS → drain the single `RETURN 1` record via PULL.
        assert!(
            matches!(resp, BoltResponse::Success { .. }),
            "RUN #{i} must be SUCCESS or throttled Failure, got {resp:?}"
        );
        bolt_send(&mut writer, &pull()).await;
        let first = bolt_recv(&mut reader).await;
        let _summary = collect_records(first, &mut vec![], &mut reader).await;
    }

    let throttled_at = throttled_at
        .expect("query bucket (cap 10, capacity 20) must throttle within 30 RUNs but never did");
    assert!(
        throttled_at <= 20,
        "throttle must fire within bucket capacity (≤20 RUNs), fired at {throttled_at}"
    );

    // The throttle must have emitted a `query_throttled` audit event with
    // tokens_available = 0 (bucket was empty at the rejection).
    let events = common::read_audit_events(&audit_tx, &audit_path).await;
    let throttle = events
        .iter()
        .find(|e| {
            e.get("event_type").and_then(serde_json::Value::as_str) == Some("query_throttled")
        })
        .unwrap_or_else(|| panic!("expected a query_throttled audit event, got: {events:#?}"));
    assert_eq!(
        throttle
            .get("tokens_available")
            .and_then(serde_json::Value::as_u64),
        Some(0),
        "query_throttled.tokens_available must be 0 at the rejection: {throttle:#?}"
    );
    assert_eq!(
        throttle
            .get("statement_sha256")
            .and_then(serde_json::Value::as_str)
            .map(str::len),
        Some(64),
        "query_throttled.statement_sha256 must be a 64-hex SHA-256: {throttle:#?}"
    );
}
#[tokio::test]
async fn bandwidth_cap_throttles_large_pull() {
    // Bandwidth cap = 2048 B/s → bucket capacity = 4096 B. The PULL streams
    // 50 nodes, each carrying a ~400-char string property, so every Bolt
    // RECORD frame is ~430 B → ~21.5 KB total. Past the 4 KB burst budget
    // that is ~(21500-4096)/2048 ≈ 8.5s of cooperative write-path sleeps.
    //
    // Sizing note (empirical, see error-log 2026-05-30): a RECORD carrying
    // a single small integer is only ~20 B, so a small-value dataset never
    // drains the bucket — the throttle is real but a 30×int PULL stayed
    // under capacity. We inflate each row with a fixed-width string so the
    // transferred byte count is predictable and comfortably past capacity.
    // Budget is deliberately wide for loaded CI: ≥4s proves the throttle
    // bit; ≤30s is the catastrophe ceiling.
    use std::time::{Duration, Instant};

    let (mut writer, mut reader, _shutdown, audit_tx, _tmp, audit_path) =
        common::spawn_bolt_handler_with_bytes_cap(2048).await;

    bolt_send(&mut writer, &hello_no_auth()).await;
    let hello_resp = bolt_recv(&mut reader).await;
    assert!(
        matches!(hello_resp, BoltResponse::Success { .. }),
        "HELLO must SUCCESS, got {hello_resp:?}"
    );

    // Seed 50 nodes, each with a 400-char payload string. Uses a LITERAL
    // list (not `range()`) because `UNWIND range(..) CREATE` creates 0 rows
    // via Bolt — a preexisting engine bug (see error-log 2026-05-27).
    let payload = "x".repeat(400);
    let list = (1..=50)
        .map(|i| i.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let seed = format!("UNWIND [{list}] AS i CREATE (:N {{i: i, payload: '{payload}'}})");
    bolt_send(&mut writer, &run_query(&seed)).await;
    let seed_resp = bolt_recv(&mut reader).await;
    assert!(
        matches!(seed_resp, BoltResponse::Success { .. }),
        "seed CREATE must SUCCESS, got {seed_resp:?}"
    );
    bolt_send(&mut writer, &pull()).await;
    let _ = collect_records(bolt_recv(&mut reader).await, &mut vec![], &mut reader).await;

    bolt_send(&mut writer, &run_query("MATCH (n:N) RETURN n.payload AS p")).await;
    let run_resp = bolt_recv(&mut reader).await;
    assert!(
        matches!(run_resp, BoltResponse::Success { .. }),
        "MATCH RUN must SUCCESS, got {run_resp:?}"
    );

    let start = Instant::now();
    bolt_send(&mut writer, &pull()).await;
    let mut records = vec![];
    let _summary = collect_records(bolt_recv(&mut reader).await, &mut records, &mut reader).await;
    let elapsed = start.elapsed();

    assert_eq!(
        records.len(),
        50,
        "PULL must stream all 50 seeded rows, got {}",
        records.len()
    );
    // Closing the connection must emit one aggregate bandwidth_throttled
    // audit event with a non-zero sleep count and duration.
    drop(writer);
    drop(reader);
    let events = common::read_audit_events(&audit_tx, &audit_path).await;
    assert!(
        elapsed >= Duration::from_secs(4),
        "50-row PULL (~21.5 KB) at a 2048 B/s cap must take ≥4s (throttled), took {elapsed:?}"
    );
    assert!(
        elapsed <= Duration::from_secs(30),
        "throttle catastrophe ceiling 30s exceeded, took {elapsed:?}"
    );

    let bw = events
        .iter()
        .find(|e| {
            e.get("event_type").and_then(serde_json::Value::as_str) == Some("bandwidth_throttled")
        })
        .unwrap_or_else(|| panic!("expected a bandwidth_throttled audit event, got: {events:#?}"));
    let total_sleeps = bw
        .get("total_sleeps")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    assert!(
        total_sleeps > 0,
        "bandwidth_throttled.total_sleeps must be > 0: {bw:#?}"
    );
    assert!(
        bw.get("total_sleep_duration_ms")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0)
            > 0,
        "bandwidth_throttled.total_sleep_duration_ms must be > 0: {bw:#?}"
    );
}
#[tokio::test]
async fn query_timeout_surfaces_execution_failed_and_audit_event() {
    // `query_timeout_ms = 50` configures the handler's deadline machinery so the
    // audit event echoes it; the abort itself comes from the injected
    // `TimeoutAccessor` double, deterministically and without the real clock.
    let (mut writer, mut reader, _shutdown, audit_tx, _tmp, audit_path) =
        common::spawn_bolt_handler_no_auth_with_timeout_double(50).await;

    bolt_send(&mut writer, &hello_no_auth()).await;
    let hello_resp = bolt_recv(&mut reader).await;
    assert!(
        matches!(hello_resp, BoltResponse::Success { .. }),
        "HELLO must SUCCESS, got {hello_resp:?}"
    );

    // A RUN whose execution "times out" (the double aborts) must FAILURE with
    // the non-retryable ClientError wire code — never a retryable TransientError.
    bolt_send(&mut writer, &run_query("MATCH (a:A), (b:B) RETURN a, b")).await;
    let resp = bolt_recv(&mut reader).await;
    assert!(
        matches!(resp, BoltResponse::Failure { .. }),
        "timed-out RUN must FAILURE, got {resp:?}"
    );
    let code = dict_str(&resp, "code").unwrap_or_default();
    assert_eq!(
        code, "Neo.ClientError.Statement.ExecutionFailed",
        "timeout must surface the non-retryable ClientError code, got: {code} (resp: {resp:?})"
    );
    assert!(
        !code.contains("TransientError"),
        "timeout must NOT be a retryable TransientError (would re-run the query): {code}"
    );

    // The abort must have emitted a `query_timed_out` audit event carrying the
    // configured timeout and a 64-hex statement fingerprint.
    let events = common::read_audit_events(&audit_tx, &audit_path).await;
    let timed_out = events
        .iter()
        .find(|e| {
            e.get("event_type").and_then(serde_json::Value::as_str) == Some("query_timed_out")
        })
        .unwrap_or_else(|| panic!("expected a query_timed_out audit event, got: {events:#?}"));
    assert_eq!(
        timed_out
            .get("timeout_ms")
            .and_then(serde_json::Value::as_u64),
        Some(50),
        "query_timed_out.timeout_ms must echo the configured timeout: {timed_out:#?}"
    );
    assert_eq!(
        timed_out
            .get("statement_sha256")
            .and_then(serde_json::Value::as_str)
            .map(str::len),
        Some(64),
        "query_timed_out.statement_sha256 must be a 64-hex SHA-256: {timed_out:#?}"
    );
}
// 3d T6: the Bolt handler routes GqlStatement::Call to dispatch_call (not to
// the defence-in-depth unreachable!() arm), end-to-end over the wire. Seeds
// labels by issuing CREATE over Bolt, then runs the pilot's verbatim CALL query
// and asserts one RECORD per distinct label. This proves the early-return guard
// + dispatch_call wiring without needing Docker (the full driver gate is T8).
#[tokio::test]
async fn call_vertex_labels_routes_over_bolt_and_returns_label_rows() {
    let auth = Arc::new(NoAuthProvider);
    let (registry, _tmp) = common::single_db_registry().await;
    let (mut writer, mut reader, _shutdown) =
        common::spawn_single_db_handler(Arc::clone(&auth), Arc::clone(&registry)).await;

    bolt_send(&mut writer, &hello_no_auth()).await;
    let _ = bolt_recv(&mut reader).await;

    // Seed two distinct labels over the wire.
    for q in ["CREATE (:Person {id: 1})", "CREATE (:Asset {id: 2})"] {
        bolt_send(&mut writer, &run_query(q)).await;
        assert!(
            matches!(bolt_recv(&mut reader).await, BoltResponse::Success { .. }),
            "CREATE failed for {q}"
        );
        bolt_send(&mut writer, &pull()).await;
        let mut drain = vec![];
        let _ = collect_records(bolt_recv(&mut reader).await, &mut drain, &mut reader).await;
    }

    // The pilot's verbatim introspection query.
    bolt_send(
        &mut writer,
        &run_query(
            "CALL mg.vertex_labels() YIELD vertex_labels UNWIND vertex_labels AS vl RETURN vl",
        ),
    )
    .await;
    let run_resp = bolt_recv(&mut reader).await;
    assert!(
        matches!(run_resp, BoltResponse::Success { .. }),
        "CALL must route to dispatch_call and return SUCCESS, got {run_resp:?}"
    );

    bolt_send(&mut writer, &pull()).await;
    let mut records = vec![];
    let final_resp = collect_records(bolt_recv(&mut reader).await, &mut records, &mut reader).await;
    assert!(
        matches!(final_resp, BoltResponse::Success { .. }),
        "expected trailing SUCCESS after CALL PULL, got {final_resp:?}"
    );
    // One row per distinct label: Asset, Person.
    assert_eq!(
        records.len(),
        2,
        "expected 2 label rows from vertex_labels(), got {records:?}"
    );
}
/// Community rechaza las sentencias administrativas DE PAGO, no todas.
///
/// El catálogo multi-base y los permisos son Enterprise: un servidor Community
/// no tiene ni lo uno ni lo otro, así que no puede responder sobre ellos.
/// Devolver una lista de concesiones vacía como si fuera la verdad sería peor
/// que fallar — el operador creería que no hay concesiones cuando lo que no hay
/// es la función.
///
/// La contrapartida está en `community_manages_its_own_users`: las seis
/// sentencias de gestión de usuarios SÍ funcionan. Bloquear el despachador
/// entero fue una regresión contra el reparto decidido.
#[tokio::test]
async fn community_rejects_paid_admin_statements_instead_of_answering_them() {
    let (mut writer, mut reader, _shutdown, audit_tx, _tmp, _audit_path) =
        common::fresh_community_handler_with_audit_file("alice").await;

    common::send_ok_hello(&mut writer, &mut reader, "alice", "neo4j").await;

    common::bolt_send(&mut writer, &common::run_message("SHOW DATABASES")).await;
    let resp = common::bolt_recv(&mut reader).await;

    assert!(
        matches!(resp, BoltResponse::Failure { .. }),
        "una sentencia de catálogo en Community debe fallar, no responder: {resp:?}"
    );
    let msg = dict_str(&resp, "message").unwrap_or_default();
    assert!(
        msg.contains("not available"),
        "el fallo debe decir que no está disponible en esta edición, no dar un \
         error genérico: {msg:?}"
    );

    drop(audit_tx);
}
/// Community gestiona sus propios usuarios. La autenticación local NO es de pago.
///
/// Contrapartida del test de rechazo: bloquear el despachador administrativo
/// entero porque falta el gestor multi-base deja a un servidor Community sin
/// poder crear ni listar usuarios. El plan maestro lo rechaza expresamente —
/// "un servidor que no puede pedir usuario y contraseña sin licencia no es
/// utilizable, y convierte la edición gratuita en un señuelo".
///
/// De las doce sentencias administrativas, seis son gestión de usuarios
/// (Community) y seis son catálogo y permisos (Enterprise). El corte va por
/// sentencia, no por despachador.
#[tokio::test]
async fn community_manages_its_own_users() {
    let (mut writer, mut reader, _shutdown, audit_tx, _tmp, _audit_path) =
        common::fresh_community_handler_with_audit_file_as("admin", true).await;

    common::send_ok_hello(&mut writer, &mut reader, "admin", "neo4j").await;

    common::bolt_send(&mut writer, &common::run_message("SHOW USERS")).await;
    let resp = common::bolt_recv(&mut reader).await;
    assert!(
        matches!(resp, BoltResponse::Success { .. }),
        "listar usuarios es gestión de identidad local, que es Community: {resp:?}"
    );

    drop(audit_tx);
}
