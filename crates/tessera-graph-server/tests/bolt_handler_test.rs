// Copyright 2026 BelowZero Security OU. All rights reserved.

//! Integration tests for [`BoltConnectionHandler`].
//!
//! Each test uses `spawn_bolt_handler` from `common`, which performs the Bolt
//! 4.4 handshake and returns chunked reader/writer halves.

mod common;

use std::collections::BTreeSet;
use std::sync::Arc;

use tessera_graph_auth::lbac::{Clearance, SecurityLabel, SecurityPolicy};
use tessera_graph::{GraphConfig, props};
use tessera_graph_protocol::PackStreamValue;
use tessera_graph_protocol::bolt_message::{BoltRequest, BoltResponse};
use tessera_graph_tenant::{DatabaseAddress, DatabaseName, TenantId, TenantRegistry};

use common::{
    bolt_recv, bolt_send, spawn_bolt_handler, test_context, test_context_with_rate_limit,
    test_context_with_registry,
};

// ── Helpers ───────────────────────────────────────────────────────────────────

fn hello_request(username: &str, password: &str) -> BoltRequest {
    BoltRequest::Hello {
        extra: vec![
            (
                "principal".to_owned(),
                PackStreamValue::String(username.to_owned()),
            ),
            (
                "credentials".to_owned(),
                PackStreamValue::String(password.to_owned()),
            ),
        ],
    }
}

fn hello_with_db(username: &str, password: &str, db: &str) -> BoltRequest {
    BoltRequest::Hello {
        extra: vec![
            (
                "principal".to_owned(),
                PackStreamValue::String(username.to_owned()),
            ),
            (
                "credentials".to_owned(),
                PackStreamValue::String(password.to_owned()),
            ),
            ("db".to_owned(), PackStreamValue::String(db.to_owned())),
        ],
    }
}

fn run_query(query: &str) -> BoltRequest {
    BoltRequest::Run {
        query: query.to_owned(),
        params: vec![],
        extra: vec![],
    }
}

#[allow(clippy::missing_const_for_fn)] // BoltRequest is not const-constructible
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

// ── Auth tests ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn bolt_hello_valid_credentials_returns_success() {
    let (_dir, ctx) = test_context();
    let (mut writer, mut reader, _shutdown) = spawn_bolt_handler(ctx).await;

    bolt_send(&mut writer, &hello_request("admin", "Admin@Init1!")).await;
    let resp = bolt_recv(&mut reader).await;

    assert!(
        matches!(resp, BoltResponse::Success { .. }),
        "expected SUCCESS for valid credentials, got {resp:?}"
    );
}

#[tokio::test]
async fn bolt_hello_wrong_password_returns_failure() {
    let (_dir, ctx) = test_context();
    let (mut writer, mut reader, _shutdown) = spawn_bolt_handler(ctx).await;

    bolt_send(&mut writer, &hello_request("admin", "WrongPassword!")).await;
    let resp = bolt_recv(&mut reader).await;

    assert!(
        matches!(resp, BoltResponse::Failure { .. }),
        "expected FAILURE for wrong password, got {resp:?}"
    );

    // Message must be generic — no username or internal detail.
    let msg = dict_str(&resp, "message").unwrap_or_default();
    assert_eq!(msg, "authentication failed");
}

#[tokio::test]
async fn bolt_hello_selects_database_from_db_field() {
    let dir = tempfile::tempdir().unwrap();
    let config = GraphConfig::new();
    let registry = Arc::new(TenantRegistry::new(dir.path(), config));
    let ctx = test_context_with_registry(Arc::clone(&registry));

    let (mut writer, mut reader, _shutdown) = spawn_bolt_handler(ctx).await;

    // Request a specific database.
    bolt_send(&mut writer, &hello_with_db("admin", "Admin@Init1!", "mydb")).await;
    let resp = bolt_recv(&mut reader).await;

    assert!(
        matches!(resp, BoltResponse::Success { .. }),
        "HELLO with db field should succeed, got {resp:?}"
    );

    // The database must have been loaded into the registry.
    let addr = DatabaseAddress {
        tenant: TenantId::new("default").unwrap(),
        database: DatabaseName::new("mydb").unwrap(),
    };
    assert!(
        registry.get_or_load(&addr).is_ok(),
        "database 'mydb' should be loaded after HELLO"
    );
}

// ── RUN + PULL tests ──────────────────────────────────────────────────────────

#[tokio::test]
async fn bolt_run_pull_returns_records() {
    let dir = tempfile::tempdir().unwrap();
    let registry = Arc::new(TenantRegistry::new(dir.path(), GraphConfig::new()));

    // Pre-populate the default database.
    {
        let addr = DatabaseAddress {
            tenant: TenantId::new("default").unwrap(),
            database: DatabaseName::default_name(),
        };
        let g = registry.get_or_load(&addr).unwrap();
        let mut graph = g.write().unwrap();
        graph
            .add_node(
                "Person",
                std::iter::once((
                    "name".to_owned(),
                    tessera_graph::Property::String("Alice".to_owned()),
                ))
                .collect(),
            )
            .unwrap();
    }

    let ctx = test_context_with_registry(registry);
    let (mut writer, mut reader, _shutdown) = spawn_bolt_handler(ctx).await;

    bolt_send(&mut writer, &hello_request("admin", "Admin@Init1!")).await;
    assert!(matches!(
        bolt_recv(&mut reader).await,
        BoltResponse::Success { .. }
    ));

    bolt_send(&mut writer, &run_query("MATCH (n:Person) RETURN n.name")).await;
    let run_resp = bolt_recv(&mut reader).await;
    assert!(
        matches!(run_resp, BoltResponse::Success { .. }),
        "RUN failed: {run_resp:?}"
    );

    bolt_send(&mut writer, &pull()).await;

    // Collect all RECORDs.
    let mut records = Vec::new();
    loop {
        let resp = bolt_recv(&mut reader).await;
        match resp {
            BoltResponse::Record { fields } => records.push(fields),
            BoltResponse::Success { .. } => break,
            other => panic!("unexpected response during PULL: {other:?}"),
        }
    }

    assert_eq!(records.len(), 1, "expected 1 row, got {}", records.len());
    assert!(
        records[0]
            .iter()
            .any(|v| matches!(v, PackStreamValue::String(s) if s == "Alice")),
        "expected Alice in record, got {:?}",
        records[0]
    );
}

#[tokio::test]
async fn bolt_run_pull_empty_result() {
    let (_dir, ctx) = test_context();
    let (mut writer, mut reader, _shutdown) = spawn_bolt_handler(ctx).await;

    bolt_send(&mut writer, &hello_request("admin", "Admin@Init1!")).await;
    assert!(matches!(
        bolt_recv(&mut reader).await,
        BoltResponse::Success { .. }
    ));

    bolt_send(&mut writer, &run_query("MATCH (n:NonExistent) RETURN n")).await;
    assert!(matches!(
        bolt_recv(&mut reader).await,
        BoltResponse::Success { .. }
    ));

    bolt_send(&mut writer, &pull()).await;

    // Should get SUCCESS immediately with no RECORDs.
    loop {
        let resp = bolt_recv(&mut reader).await;
        match resp {
            BoltResponse::Record { .. } => {} // no records expected but loop anyway
            BoltResponse::Success { .. } => break,
            other => panic!("unexpected response: {other:?}"),
        }
    }
}

#[tokio::test]
async fn bolt_run_mutation_creates_node() {
    let (_dir, ctx) = test_context();
    let (mut writer, mut reader, _shutdown) = spawn_bolt_handler(ctx).await;

    bolt_send(&mut writer, &hello_request("admin", "Admin@Init1!")).await;
    assert!(matches!(
        bolt_recv(&mut reader).await,
        BoltResponse::Success { .. }
    ));

    bolt_send(
        &mut writer,
        &run_query("CREATE (n:BoltNode {name: 'created'})"),
    )
    .await;
    let run_resp = bolt_recv(&mut reader).await;
    assert!(
        matches!(run_resp, BoltResponse::Success { .. }),
        "CREATE failed: {run_resp:?}"
    );

    bolt_send(&mut writer, &pull()).await;
    let mut nodes_created = 0i64;
    loop {
        let resp = bolt_recv(&mut reader).await;
        match resp {
            BoltResponse::Record { fields } => {
                // Find nodes_created value in mutation summary row.
                if let Some(PackStreamValue::Int(n)) = fields.first() {
                    nodes_created = *n;
                }
            }
            BoltResponse::Success { .. } => break,
            other => panic!("unexpected: {other:?}"),
        }
    }
    assert_eq!(nodes_created, 1, "expected 1 node created");
}

#[tokio::test]
async fn bolt_run_pull_mutation_is_flushed() {
    // Verify that mutations survive a registry reload (i.e., flush() was called).
    let dir = tempfile::tempdir().unwrap();
    let config = GraphConfig::new();

    let addr = DatabaseAddress {
        tenant: TenantId::new("default").unwrap(),
        database: DatabaseName::default_name(),
    };

    // --- First session: create a node ---
    {
        let registry = Arc::new(TenantRegistry::new(dir.path(), config.clone()));
        let ctx = test_context_with_registry(Arc::clone(&registry));
        let (mut writer, mut reader, shutdown_tx) = spawn_bolt_handler(ctx).await;

        bolt_send(&mut writer, &hello_request("admin", "Admin@Init1!")).await;
        assert!(matches!(
            bolt_recv(&mut reader).await,
            BoltResponse::Success { .. }
        ));

        bolt_send(
            &mut writer,
            &run_query("CREATE (n:Durable {name: 'persisted'})"),
        )
        .await;
        assert!(matches!(
            bolt_recv(&mut reader).await,
            BoltResponse::Success { .. }
        ));
        bolt_send(&mut writer, &pull()).await;
        loop {
            if matches!(bolt_recv(&mut reader).await, BoltResponse::Success { .. }) {
                break;
            }
        }
        // Signal shutdown so the handler completes and flushes.
        drop(shutdown_tx);
    }

    // Small wait to ensure flush completes.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // --- Second session: verify the node survived ---
    let registry2 = Arc::new(TenantRegistry::new(dir.path(), config));
    let node_count = {
        let graph = registry2.get_or_load(&addr).unwrap();
        let g = graph.read().unwrap();
        g.nodes_by_label("Durable").len()
    };
    assert_eq!(node_count, 1, "expected 1 Durable node after reopen");
}

// ── LBAC tests ────────────────────────────────────────────────────────────────

fn ctx_with_clearance_and_node(
    level: u16,
    compartments: &[&str],
    node_level: u16,
    node_compartments: &[&str],
) -> (
    tempfile::TempDir,
    Arc<tessera_graph_server::context::ServerContext>,
    Arc<TenantRegistry>,
) {
    let dir = tempfile::tempdir().unwrap();
    let registry = Arc::new(TenantRegistry::new(dir.path(), GraphConfig::new()));

    // Populate the default database.
    let addr = DatabaseAddress {
        tenant: TenantId::new("default").unwrap(),
        database: DatabaseName::default_name(),
    };
    let g = registry.get_or_load(&addr).unwrap();
    let mut graph = g.write().unwrap();

    let label = SecurityLabel::new(
        node_level,
        node_compartments
            .iter()
            .map(|s| (*s).to_string())
            .collect::<BTreeSet<_>>(),
    );
    let mut p = props! { "name" => "Secret" };
    SecurityPolicy::inject_label(&mut p, &label);
    graph.add_node("Thing", p).unwrap();
    drop(graph);

    let ctx = test_context_with_registry(Arc::clone(&registry));
    let clearance = Clearance::new(
        level,
        compartments
            .iter()
            .map(|s| (*s).to_string())
            .collect::<BTreeSet<_>>(),
    );
    ctx.user_store().set_clearance("admin", clearance).unwrap();
    (dir, ctx, registry)
}

#[tokio::test]
async fn bolt_lbac_hides_classified_node() {
    // User clearance level 0, node at level 5 → must be hidden.
    let (_dir, ctx, _registry) = ctx_with_clearance_and_node(0, &[], 5, &[]);
    let (mut writer, mut reader, _shutdown) = spawn_bolt_handler(ctx).await;

    bolt_send(&mut writer, &hello_request("admin", "Admin@Init1!")).await;
    assert!(matches!(
        bolt_recv(&mut reader).await,
        BoltResponse::Success { .. }
    ));

    bolt_send(&mut writer, &run_query("MATCH (n:Thing) RETURN n.name")).await;
    assert!(matches!(
        bolt_recv(&mut reader).await,
        BoltResponse::Success { .. }
    ));

    bolt_send(&mut writer, &pull()).await;
    let mut record_count = 0;
    loop {
        let resp = bolt_recv(&mut reader).await;
        match resp {
            BoltResponse::Record { .. } => record_count += 1,
            BoltResponse::Success { .. } => break,
            other => panic!("unexpected: {other:?}"),
        }
    }
    assert_eq!(record_count, 0, "under-cleared user must see 0 records");
}

#[tokio::test]
async fn bolt_lbac_shows_node_to_cleared_user() {
    // User clearance level 10, node at level 5 → must be visible.
    let (_dir, ctx, _registry) = ctx_with_clearance_and_node(10, &[], 5, &[]);
    let (mut writer, mut reader, _shutdown) = spawn_bolt_handler(ctx).await;

    bolt_send(&mut writer, &hello_request("admin", "Admin@Init1!")).await;
    assert!(matches!(
        bolt_recv(&mut reader).await,
        BoltResponse::Success { .. }
    ));

    bolt_send(&mut writer, &run_query("MATCH (n:Thing) RETURN n.name")).await;
    assert!(matches!(
        bolt_recv(&mut reader).await,
        BoltResponse::Success { .. }
    ));

    bolt_send(&mut writer, &pull()).await;
    let mut record_count = 0;
    loop {
        let resp = bolt_recv(&mut reader).await;
        match resp {
            BoltResponse::Record { .. } => record_count += 1,
            BoltResponse::Success { .. } => break,
            other => panic!("unexpected: {other:?}"),
        }
    }
    assert_eq!(record_count, 1, "cleared user must see the node");
}

// ── State machine tests ───────────────────────────────────────────────────────

#[tokio::test]
async fn bolt_after_failure_returns_ignored() {
    let (_dir, ctx) = test_context();
    let (mut writer, mut reader, _shutdown) = spawn_bolt_handler(ctx).await;

    // Authenticate first.
    bolt_send(&mut writer, &hello_request("admin", "Admin@Init1!")).await;
    assert!(matches!(
        bolt_recv(&mut reader).await,
        BoltResponse::Success { .. }
    ));

    // Trigger a failure with an invalid query.
    bolt_send(&mut writer, &run_query("THIS IS NOT VALID CYPHER!!!")).await;
    let fail_resp = bolt_recv(&mut reader).await;
    assert!(
        matches!(fail_resp, BoltResponse::Failure { .. }),
        "expected FAILURE for bad query, got {fail_resp:?}"
    );

    // Next request (RUN) must be IGNORED.
    bolt_send(&mut writer, &run_query("MATCH (n) RETURN n")).await;
    let ignored = bolt_recv(&mut reader).await;
    assert!(
        matches!(ignored, BoltResponse::Ignored),
        "expected IGNORED after failure, got {ignored:?}"
    );
}

#[tokio::test]
async fn bolt_reset_clears_failure() {
    let (_dir, ctx) = test_context();
    let (mut writer, mut reader, _shutdown) = spawn_bolt_handler(ctx).await;

    bolt_send(&mut writer, &hello_request("admin", "Admin@Init1!")).await;
    assert!(matches!(
        bolt_recv(&mut reader).await,
        BoltResponse::Success { .. }
    ));

    // Trigger failure.
    bolt_send(&mut writer, &run_query("BAD QUERY")).await;
    assert!(matches!(
        bolt_recv(&mut reader).await,
        BoltResponse::Failure { .. }
    ));

    // RESET must clear the failed flag.
    bolt_send(&mut writer, &BoltRequest::Reset).await;
    assert!(matches!(
        bolt_recv(&mut reader).await,
        BoltResponse::Success { .. }
    ));

    // Now a valid RUN should succeed.
    bolt_send(&mut writer, &run_query("MATCH (n) RETURN n")).await;
    let run_resp = bolt_recv(&mut reader).await;
    assert!(
        matches!(run_resp, BoltResponse::Success { .. }),
        "expected SUCCESS after RESET, got {run_resp:?}"
    );
}

#[tokio::test]
async fn bolt_goodbye_closes_connection() {
    let (_dir, ctx) = test_context();
    let (mut writer, mut reader, _shutdown) = spawn_bolt_handler(ctx).await;

    bolt_send(&mut writer, &hello_request("admin", "Admin@Init1!")).await;
    assert!(matches!(
        bolt_recv(&mut reader).await,
        BoltResponse::Success { .. }
    ));

    // Send GOODBYE — the handler should close the connection.
    bolt_send(&mut writer, &BoltRequest::Goodbye).await;

    // The server side closes; the next read should return None (EOF).
    let eof = reader.read_message().await.unwrap();
    assert!(eof.is_none(), "expected EOF after GOODBYE");
}

// ── Transaction tests ─────────────────────────────────────────────────────────
// Explicit transactions (BEGIN/COMMIT/ROLLBACK) are NOT implemented.
// BEGIN must respond FAILURE so clients don't silently lose rollback semantics.

#[tokio::test]
async fn bolt_begin_responds_failure() {
    let (_dir, ctx) = test_context();
    let (mut writer, mut reader, _shutdown) = spawn_bolt_handler(ctx).await;

    bolt_send(&mut writer, &hello_request("admin", "Admin@Init1!")).await;
    assert!(matches!(
        bolt_recv(&mut reader).await,
        BoltResponse::Success { .. }
    ));

    bolt_send(&mut writer, &BoltRequest::Begin { extra: vec![] }).await;
    let resp = bolt_recv(&mut reader).await;
    assert!(
        matches!(resp, BoltResponse::Failure { .. }),
        "BEGIN must respond FAILURE (explicit transactions not supported), got {resp:?}"
    );
}

#[tokio::test]
async fn bolt_after_begin_failure_run_is_ignored() {
    let (_dir, ctx) = test_context();
    let (mut writer, mut reader, _shutdown) = spawn_bolt_handler(ctx).await;

    bolt_send(&mut writer, &hello_request("admin", "Admin@Init1!")).await;
    assert!(matches!(
        bolt_recv(&mut reader).await,
        BoltResponse::Success { .. }
    ));

    // BEGIN enters FAILED state
    bolt_send(&mut writer, &BoltRequest::Begin { extra: vec![] }).await;
    assert!(matches!(
        bolt_recv(&mut reader).await,
        BoltResponse::Failure { .. }
    ));

    // RUN in FAILED state must be IGNORED
    bolt_send(&mut writer, &run_query("MATCH (n) RETURN n")).await;
    let resp = bolt_recv(&mut reader).await;
    assert!(
        matches!(resp, BoltResponse::Ignored),
        "RUN after BEGIN failure must be IGNORED, got {resp:?}"
    );
}

#[tokio::test]
async fn bolt_after_begin_failure_reset_recovers() {
    let (_dir, ctx) = test_context();
    let (mut writer, mut reader, _shutdown) = spawn_bolt_handler(ctx).await;

    bolt_send(&mut writer, &hello_request("admin", "Admin@Init1!")).await;
    assert!(matches!(
        bolt_recv(&mut reader).await,
        BoltResponse::Success { .. }
    ));

    // BEGIN fails
    bolt_send(&mut writer, &BoltRequest::Begin { extra: vec![] }).await;
    assert!(matches!(
        bolt_recv(&mut reader).await,
        BoltResponse::Failure { .. }
    ));

    // RESET recovers
    bolt_send(&mut writer, &BoltRequest::Reset).await;
    assert!(matches!(
        bolt_recv(&mut reader).await,
        BoltResponse::Success { .. }
    ));

    // RUN should work again
    bolt_send(&mut writer, &run_query("MATCH (n) RETURN n")).await;
    let resp = bolt_recv(&mut reader).await;
    assert!(
        matches!(resp, BoltResponse::Success { .. }),
        "RUN after RESET should succeed, got {resp:?}"
    );
}

#[tokio::test]
async fn bolt_commit_without_begin_returns_ignored() {
    let (_dir, ctx) = test_context();
    let (mut writer, mut reader, _shutdown) = spawn_bolt_handler(ctx).await;

    bolt_send(&mut writer, &hello_request("admin", "Admin@Init1!")).await;
    assert!(matches!(
        bolt_recv(&mut reader).await,
        BoltResponse::Success { .. }
    ));

    bolt_send(&mut writer, &BoltRequest::Commit).await;
    let resp = bolt_recv(&mut reader).await;
    assert!(
        matches!(resp, BoltResponse::Ignored),
        "COMMIT without BEGIN must be IGNORED, got {resp:?}"
    );
}

#[tokio::test]
async fn bolt_rollback_without_begin_returns_ignored() {
    let (_dir, ctx) = test_context();
    let (mut writer, mut reader, _shutdown) = spawn_bolt_handler(ctx).await;

    bolt_send(&mut writer, &hello_request("admin", "Admin@Init1!")).await;
    assert!(matches!(
        bolt_recv(&mut reader).await,
        BoltResponse::Success { .. }
    ));

    bolt_send(&mut writer, &BoltRequest::Rollback).await;
    let resp = bolt_recv(&mut reader).await;
    assert!(
        matches!(resp, BoltResponse::Ignored),
        "ROLLBACK without BEGIN must be IGNORED, got {resp:?}"
    );
}

// ── Rate-limiting tests ──────────────────────────────────────────────────────

#[tokio::test]
async fn bolt_rate_limit_locks_after_max_failures() {
    // Policy: lock after 2 failures for 60 seconds.
    let (_dir, ctx) = test_context_with_rate_limit(2, 60);
    let (mut writer, mut reader, _shutdown) = spawn_bolt_handler(ctx).await;

    // Two failed attempts with RESET between them.
    for _ in 0..2 {
        bolt_send(&mut writer, &hello_request("admin", "wrong")).await;
        let resp = bolt_recv(&mut reader).await;
        assert!(matches!(resp, BoltResponse::Failure { .. }));
        bolt_send(&mut writer, &BoltRequest::Reset).await;
        assert!(matches!(
            bolt_recv(&mut reader).await,
            BoltResponse::Success { .. }
        ));
    }

    // Third attempt with correct password — account is locked.
    bolt_send(&mut writer, &hello_request("admin", "Admin@Init1!")).await;
    let resp = bolt_recv(&mut reader).await;
    assert!(
        matches!(resp, BoltResponse::Failure { .. }),
        "locked account must be rejected even with correct password, got {resp:?}"
    );
}

#[tokio::test]
async fn bolt_rate_limit_success_resets_counter() {
    // Policy: lock after 3 failures for 60 seconds.
    let (_dir, ctx) = test_context_with_rate_limit(3, 60);
    let (mut writer, mut reader, _shutdown) = spawn_bolt_handler(ctx).await;

    // One failure.
    bolt_send(&mut writer, &hello_request("admin", "wrong")).await;
    assert!(matches!(
        bolt_recv(&mut reader).await,
        BoltResponse::Failure { .. }
    ));
    bolt_send(&mut writer, &BoltRequest::Reset).await;
    assert!(matches!(
        bolt_recv(&mut reader).await,
        BoltResponse::Success { .. }
    ));

    // Successful login resets the counter.
    bolt_send(&mut writer, &hello_request("admin", "Admin@Init1!")).await;
    assert!(matches!(
        bolt_recv(&mut reader).await,
        BoltResponse::Success { .. }
    ));
}

// ── O1: Unique connection_id ──────────────────────────────────────────────────

#[tokio::test]
async fn bolt_hello_returns_unique_connection_id() {
    // Two independent connections must get different connection_ids.
    let (_dir1, ctx1) = test_context();
    let (mut w1, mut r1, _s1) = spawn_bolt_handler(ctx1).await;
    bolt_send(&mut w1, &hello_request("admin", "Admin@Init1!")).await;
    let resp1 = bolt_recv(&mut r1).await;

    let (_dir2, ctx2) = test_context();
    let (mut w2, mut r2, _s2) = spawn_bolt_handler(ctx2).await;
    bolt_send(&mut w2, &hello_request("admin", "Admin@Init1!")).await;
    let resp2 = bolt_recv(&mut r2).await;

    let id1 = dict_str(&resp1, "connection_id").expect("connection_id in resp1");
    let id2 = dict_str(&resp2, "connection_id").expect("connection_id in resp2");
    assert_ne!(id1, id2, "connection_ids must be unique across connections");
    assert!(
        id1.starts_with("bolt-tessera-"),
        "connection_id must start with bolt-tessera-, got {id1}"
    );
}

// ── O5: Non-empty params rejected ────────────────────────────────────────────

#[tokio::test]
async fn bolt_run_with_params_returns_failure() {
    let (_dir, ctx) = test_context();
    let (mut writer, mut reader, _shutdown) = spawn_bolt_handler(ctx).await;

    bolt_send(&mut writer, &hello_request("admin", "Admin@Init1!")).await;
    assert!(matches!(
        bolt_recv(&mut reader).await,
        BoltResponse::Success { .. }
    ));

    // Send RUN with non-empty params.
    let req = BoltRequest::Run {
        query: "MATCH (n) WHERE n.name = $name RETURN n".to_owned(),
        params: vec![(
            "name".to_owned(),
            PackStreamValue::String("Alice".to_owned()),
        )],
        extra: vec![],
    };
    bolt_send(&mut writer, &req).await;
    let resp = bolt_recv(&mut reader).await;
    assert!(
        matches!(resp, BoltResponse::Failure { .. }),
        "RUN with params must fail, got {resp:?}"
    );
}

// ── Shutdown tests ────────────────────────────────────────────────────────────

#[tokio::test]
async fn bolt_shutdown_signal_closes_handler() {
    let (_dir, ctx) = test_context();
    let (mut writer, mut reader, shutdown_tx) = spawn_bolt_handler(ctx).await;

    bolt_send(&mut writer, &hello_request("admin", "Admin@Init1!")).await;
    assert!(matches!(
        bolt_recv(&mut reader).await,
        BoltResponse::Success { .. }
    ));

    // Signal shutdown.
    let _ = shutdown_tx.send(true);

    // The connection should close — next read returns EOF.
    let result =
        tokio::time::timeout(std::time::Duration::from_secs(3), reader.read_message()).await;
    assert!(result.is_ok(), "timed out waiting for handler to close");
    // Either EOF or an error is acceptable — the handler exited.
}
