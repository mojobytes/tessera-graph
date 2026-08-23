// SPDX-License-Identifier: BSL-1.1

//! `AuditSink` + event + rotation tests.

use std::net::SocketAddr;
use std::time::Duration;

use tokio::sync::watch;

use ermya_graph_server::audit::{
    AdminAction, AuditOutcome, AuditSink, AuthFailureReason, CloseReason, DatabaseOptionsAudit,
    GrantChangeAction, QueryOutcome,
};

fn parse_line(line: &str) -> serde_json::Value {
    serde_json::from_str(line).expect("valid JSON per line")
}

#[tokio::test]
async fn off_sink_accepts_all_calls_without_side_effects() {
    let sink = AuditSink::off();
    let peer: SocketAddr = "127.0.0.1:1234".parse().unwrap();
    sink.connection_open(1, peer, true);
    sink.auth_success(1, "alice", "alice");
    sink.query_exec(1, "alice", "abc", 0, 0, QueryOutcome::Success);
    sink.connection_close(1, Some("alice"), CloseReason::Goodbye, 0);
    // No panic, no output — nothing to assert beyond "did not crash".
}

#[tokio::test]
async fn file_sink_emits_one_json_per_line() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("audit.log");
    let (_tx, rx) = watch::channel(false);
    let sink = AuditSink::file(path.clone(), 1_000_000, 3, 0, rx).unwrap();

    let peer: SocketAddr = "127.0.0.1:1234".parse().unwrap();
    sink.connection_open(1, peer, true);
    sink.auth_success(1, "alice", "alice");
    sink.auth_failure(2, "root", AuthFailureReason::UnknownUser);
    sink.query_exec(1, "alice", "abc123", 5, 10, QueryOutcome::Success);
    sink.admin_action(
        1,
        "admin",
        AdminAction::CreateUser {
            target: "bob".to_owned(),
        },
    );
    sink.connection_close(1, Some("alice"), CloseReason::Goodbye, 7);

    drop(sink);
    tokio::time::sleep(Duration::from_millis(200)).await;

    let contents = std::fs::read_to_string(&path).unwrap();
    let lines: Vec<_> = contents.lines().collect();
    assert_eq!(lines.len(), 6);

    for line in &lines {
        let v = parse_line(line);
        assert!(
            v.get("timestamp")
                .and_then(serde_json::Value::as_str)
                .is_some(),
            "timestamp present in {line}"
        );
        assert!(
            v.get("event_type")
                .and_then(serde_json::Value::as_str)
                .is_some(),
            "event_type present in {line}"
        );
        assert!(
            v.get("connection_id")
                .and_then(serde_json::Value::as_u64)
                .is_some(),
            "connection_id present in {line}"
        );
        assert!(v.get("details").is_some(), "details present in {line}");
    }

    let types: Vec<String> = lines
        .iter()
        .map(|l| parse_line(l)["event_type"].as_str().unwrap().to_owned())
        .collect();
    assert!(types.contains(&"connection_open".to_owned()));
    assert!(types.contains(&"auth_success".to_owned()));
    assert!(types.contains(&"auth_failure".to_owned()));
    assert!(types.contains(&"query_exec".to_owned()));
    assert!(types.contains(&"admin_action".to_owned()));
    assert!(types.contains(&"connection_close".to_owned()));
}

#[tokio::test]
async fn principal_truncated_to_256_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("audit.log");
    let (_tx, rx) = watch::channel(false);
    let sink = AuditSink::file(path.clone(), 1_000_000, 3, 0, rx).unwrap();

    let long = "x".repeat(1024);
    sink.auth_failure(1, &long, AuthFailureReason::InvalidCredentials);
    drop(sink);
    tokio::time::sleep(Duration::from_millis(200)).await;

    let contents = std::fs::read_to_string(&path).unwrap();
    let v = parse_line(contents.trim());
    let attempted = v["details"]["principal_attempted"].as_str().unwrap();
    assert!(attempted.len() <= 256);
}

// Task 10 ciclo 8: query_exec carries database (spec section 6.3).
//
// Spec section 6.3: every post-HELLO event gains a `database` field
// with the value of the session's `DbHandle`. `auth_success` already
// does it (ciclo 1). `query_exec` is next: every RUN dispatched in
// multi-database mode must record which tenant database it ran
// against; otherwise audit log analysis cannot attribute traffic to
// tenants. The legacy single-database path keeps emitting events
// without the field (`Option::None` + `skip_serializing_if`) so that
// existing audit consumers do not see a schema break.

#[tokio::test]
async fn query_exec_with_database_serialises_database_field() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("audit.log");
    let (_tx, rx) = watch::channel(false);
    let sink = AuditSink::file(path.clone(), 1_000_000, 3, 0, rx).unwrap();

    sink.query_exec_with_database(
        1,
        "alice",
        Some("plantA"),
        "abc123",
        5,
        10,
        QueryOutcome::Success,
    );
    drop(sink);
    tokio::time::sleep(Duration::from_millis(200)).await;

    let contents = std::fs::read_to_string(&path).unwrap();
    let v = parse_line(contents.trim());
    assert_eq!(
        v["event_type"].as_str(),
        Some("query_exec"),
        "expected query_exec event, got: {v}"
    );
    assert_eq!(
        v["details"]["database"].as_str(),
        Some("plantA"),
        "expected database=plantA in details, got: {}",
        v["details"]
    );
}

#[tokio::test]
async fn query_exec_legacy_call_does_not_emit_database_field() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("audit.log");
    let (_tx, rx) = watch::channel(false);
    let sink = AuditSink::file(path.clone(), 1_000_000, 3, 0, rx).unwrap();

    // Legacy (Fase 1a) call site, no database context.
    sink.query_exec(1, "alice", "abc123", 5, 10, QueryOutcome::Success);
    drop(sink);
    tokio::time::sleep(Duration::from_millis(200)).await;

    let contents = std::fs::read_to_string(&path).unwrap();
    let v = parse_line(contents.trim());
    assert!(
        v["details"].get("database").is_none(),
        "legacy query_exec must not emit `database` (skip_if_none), got: {}",
        v["details"]
    );
}

// Task 14: spec §6.3 introduces three top-level audit events for
// database/grant catalog operations: `database_created`,
// `database_dropped`, `grant_changed`. Each carries an `outcome`
// (Success | Failed { reason }) so operators can attribute both
// successful catalog mutations and rejected attempts.

#[tokio::test]
async fn database_created_event_serialises_success_with_options() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("audit.log");
    let (_tx, rx) = watch::channel(false);
    let sink = AuditSink::file(path.clone(), 1_000_000, 3, 0, rx).unwrap();

    sink.database_created(
        42,
        "admin",
        "plantA",
        DatabaseOptionsAudit {
            max_size_bytes: Some(1_048_576),
            max_connections: Some(8),
        },
        AuditOutcome::Success,
    );
    drop(sink);
    tokio::time::sleep(Duration::from_millis(200)).await;

    let contents = std::fs::read_to_string(&path).unwrap();
    let v = parse_line(contents.trim());
    assert_eq!(
        v["event_type"].as_str(),
        Some("database_created"),
        "expected database_created event, got: {v}"
    );
    assert_eq!(v["user"].as_str(), Some("admin"));
    assert_eq!(v["connection_id"].as_u64(), Some(42));
    assert_eq!(v["details"]["name"].as_str(), Some("plantA"));
    assert_eq!(
        v["details"]["options"]["max_size_bytes"].as_u64(),
        Some(1_048_576)
    );
    assert_eq!(v["details"]["options"]["max_connections"].as_u64(), Some(8));
    assert_eq!(v["details"]["outcome"].as_str(), Some("success"));
}

#[tokio::test]
async fn database_created_event_skips_unset_options() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("audit.log");
    let (_tx, rx) = watch::channel(false);
    let sink = AuditSink::file(path.clone(), 1_000_000, 3, 0, rx).unwrap();

    sink.database_created(
        1,
        "admin",
        "plantA",
        DatabaseOptionsAudit {
            max_size_bytes: None,
            max_connections: None,
        },
        AuditOutcome::Success,
    );
    drop(sink);
    tokio::time::sleep(Duration::from_millis(200)).await;

    let contents = std::fs::read_to_string(&path).unwrap();
    let v = parse_line(contents.trim());
    assert!(
        v["details"]["options"].get("max_size_bytes").is_none(),
        "unset max_size_bytes must be skipped on the wire, got: {}",
        v["details"]["options"]
    );
    assert!(
        v["details"]["options"].get("max_connections").is_none(),
        "unset max_connections must be skipped on the wire, got: {}",
        v["details"]["options"]
    );
}

#[tokio::test]
async fn database_created_event_carries_failed_outcome_with_reason() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("audit.log");
    let (_tx, rx) = watch::channel(false);
    let sink = AuditSink::file(path.clone(), 1_000_000, 3, 0, rx).unwrap();

    sink.database_created(
        1,
        "admin",
        "plantA",
        DatabaseOptionsAudit {
            max_size_bytes: None,
            max_connections: None,
        },
        AuditOutcome::Failed {
            reason: "duplicate_name".to_owned(),
        },
    );
    drop(sink);
    tokio::time::sleep(Duration::from_millis(200)).await;

    let contents = std::fs::read_to_string(&path).unwrap();
    let v = parse_line(contents.trim());
    assert_eq!(v["details"]["outcome"].as_str(), Some("failed"));
    assert_eq!(v["details"]["reason"].as_str(), Some("duplicate_name"));
}

#[tokio::test]
async fn database_dropped_event_serialises_success() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("audit.log");
    let (_tx, rx) = watch::channel(false);
    let sink = AuditSink::file(path.clone(), 1_000_000, 3, 0, rx).unwrap();

    sink.database_dropped(7, "admin", "plantA", AuditOutcome::Success);
    drop(sink);
    tokio::time::sleep(Duration::from_millis(200)).await;

    let contents = std::fs::read_to_string(&path).unwrap();
    let v = parse_line(contents.trim());
    assert_eq!(v["event_type"].as_str(), Some("database_dropped"));
    assert_eq!(v["user"].as_str(), Some("admin"));
    assert_eq!(v["connection_id"].as_u64(), Some(7));
    assert_eq!(v["details"]["name"].as_str(), Some("plantA"));
    assert_eq!(v["details"]["outcome"].as_str(), Some("success"));
}

#[tokio::test]
async fn database_dropped_event_carries_failed_outcome() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("audit.log");
    let (_tx, rx) = watch::channel(false);
    let sink = AuditSink::file(path.clone(), 1_000_000, 3, 0, rx).unwrap();

    sink.database_dropped(
        7,
        "admin",
        "plantA",
        AuditOutcome::Failed {
            reason: "database_in_use".to_owned(),
        },
    );
    drop(sink);
    tokio::time::sleep(Duration::from_millis(200)).await;

    let contents = std::fs::read_to_string(&path).unwrap();
    let v = parse_line(contents.trim());
    assert_eq!(v["details"]["outcome"].as_str(), Some("failed"));
    assert_eq!(v["details"]["reason"].as_str(), Some("database_in_use"));
}

#[tokio::test]
async fn grant_changed_grant_event_serialises_success() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("audit.log");
    let (_tx, rx) = watch::channel(false);
    let sink = AuditSink::file(path.clone(), 1_000_000, 3, 0, rx).unwrap();

    sink.grant_changed(
        3,
        "admin",
        "alice",
        "plantA",
        "READ_WRITE",
        GrantChangeAction::Grant,
        AuditOutcome::Success,
    );
    drop(sink);
    tokio::time::sleep(Duration::from_millis(200)).await;

    let contents = std::fs::read_to_string(&path).unwrap();
    let v = parse_line(contents.trim());
    assert_eq!(v["event_type"].as_str(), Some("grant_changed"));
    assert_eq!(v["user"].as_str(), Some("admin"));
    assert_eq!(v["details"]["user_target"].as_str(), Some("alice"));
    assert_eq!(v["details"]["database"].as_str(), Some("plantA"));
    assert_eq!(v["details"]["access_level"].as_str(), Some("READ_WRITE"));
    assert_eq!(v["details"]["action"].as_str(), Some("grant"));
    assert_eq!(v["details"]["outcome"].as_str(), Some("success"));
}

#[tokio::test]
async fn grant_changed_revoke_event_serialises_success() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("audit.log");
    let (_tx, rx) = watch::channel(false);
    let sink = AuditSink::file(path.clone(), 1_000_000, 3, 0, rx).unwrap();

    sink.grant_changed(
        3,
        "admin",
        "alice",
        "plantA",
        "",
        GrantChangeAction::Revoke,
        AuditOutcome::Success,
    );
    drop(sink);
    tokio::time::sleep(Duration::from_millis(200)).await;

    let contents = std::fs::read_to_string(&path).unwrap();
    let v = parse_line(contents.trim());
    assert_eq!(v["details"]["action"].as_str(), Some("revoke"));
    assert_eq!(v["details"]["access_level"].as_str(), Some(""));
}

#[tokio::test]
async fn grant_changed_event_carries_failed_outcome() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("audit.log");
    let (_tx, rx) = watch::channel(false);
    let sink = AuditSink::file(path.clone(), 1_000_000, 3, 0, rx).unwrap();

    sink.grant_changed(
        3,
        "admin",
        "alice",
        "plantA",
        "READ_WRITE",
        GrantChangeAction::Grant,
        AuditOutcome::Failed {
            reason: "unknown_user".to_owned(),
        },
    );
    drop(sink);
    tokio::time::sleep(Duration::from_millis(200)).await;

    let contents = std::fs::read_to_string(&path).unwrap();
    let v = parse_line(contents.trim());
    assert_eq!(v["details"]["outcome"].as_str(), Some("failed"));
    assert_eq!(v["details"]["reason"].as_str(), Some("unknown_user"));
}

// Task 14 QR-#8: truncation bounds for the new sink methods. The
// truncate() helper is shared across all sink methods, but a future
// refactor could silently drop the bound for one event type. Each new
// truncation surface gets a dedicated test so the contract is pinned.

#[tokio::test]
async fn database_created_truncates_user_to_principal_bound() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("audit.log");
    let (_tx, rx) = watch::channel(false);
    let sink = AuditSink::file(path.clone(), 1_000_000, 3, 0, rx).unwrap();

    let huge_user = "u".repeat(1024);
    sink.database_created(
        1,
        &huge_user,
        "plantA",
        DatabaseOptionsAudit {
            max_size_bytes: None,
            max_connections: None,
        },
        AuditOutcome::Success,
    );
    drop(sink);
    tokio::time::sleep(Duration::from_millis(200)).await;

    let contents = std::fs::read_to_string(&path).unwrap();
    let v = parse_line(contents.trim());
    let user = v["user"].as_str().unwrap();
    assert!(
        user.len() <= 256,
        "user must be truncated to PRINCIPAL_MAX_BYTES (256), got {} bytes",
        user.len()
    );
}

#[tokio::test]
async fn database_dropped_truncates_name_to_64_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("audit.log");
    let (_tx, rx) = watch::channel(false);
    let sink = AuditSink::file(path.clone(), 1_000_000, 3, 0, rx).unwrap();

    let huge_name = "n".repeat(128);
    sink.database_dropped(1, "admin", &huge_name, AuditOutcome::Success);
    drop(sink);
    tokio::time::sleep(Duration::from_millis(200)).await;

    let contents = std::fs::read_to_string(&path).unwrap();
    let v = parse_line(contents.trim());
    let name = v["details"]["name"].as_str().unwrap();
    assert!(
        name.len() <= 64,
        "details.name must be truncated to 64 bytes, got {} bytes",
        name.len()
    );
}

#[tokio::test]
async fn grant_changed_truncates_user_target_and_access_level() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("audit.log");
    let (_tx, rx) = watch::channel(false);
    let sink = AuditSink::file(path.clone(), 1_000_000, 3, 0, rx).unwrap();

    let huge_target = "t".repeat(1024);
    let huge_level = "L".repeat(128);
    sink.grant_changed(
        1,
        "admin",
        &huge_target,
        "plantA",
        &huge_level,
        GrantChangeAction::Grant,
        AuditOutcome::Success,
    );
    drop(sink);
    tokio::time::sleep(Duration::from_millis(200)).await;

    let contents = std::fs::read_to_string(&path).unwrap();
    let v = parse_line(contents.trim());
    let user_target = v["details"]["user_target"].as_str().unwrap();
    let access_level = v["details"]["access_level"].as_str().unwrap();
    assert!(
        user_target.len() <= 256,
        "details.user_target must be truncated to PRINCIPAL_MAX_BYTES, got {} bytes",
        user_target.len()
    );
    assert!(
        access_level.len() <= 32,
        "details.access_level must be truncated to 32 bytes, got {} bytes",
        access_level.len()
    );
}

// Task 14 QR-#3: `reason` in AuditOutcome::Failed is bounded to 512
// bytes so a Backend(String) wrapping a long I/O message (filesystem
// path chains, etc.) cannot bloat the audit line.

#[tokio::test]
async fn failed_outcome_reason_truncated_to_512_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("audit.log");
    let (_tx, rx) = watch::channel(false);
    let sink = AuditSink::file(path.clone(), 1_000_000, 3, 0, rx).unwrap();

    let huge_reason = "r".repeat(2048);
    sink.database_created(
        1,
        "admin",
        "plantA",
        DatabaseOptionsAudit {
            max_size_bytes: None,
            max_connections: None,
        },
        AuditOutcome::Failed {
            reason: huge_reason,
        },
    );
    drop(sink);
    tokio::time::sleep(Duration::from_millis(200)).await;

    let contents = std::fs::read_to_string(&path).unwrap();
    let v = parse_line(contents.trim());
    let reason = v["details"]["reason"].as_str().unwrap();
    assert!(
        reason.len() <= 512,
        "Failed.reason must be truncated to REASON_MAX_BYTES (512), got {} bytes",
        reason.len()
    );
}

// Task 14 QR-#9: the MPSC channel between the sink and the writer task
// drops events when full and increments a counter that surfaces as an
// `audit_backpressure` event. Verify the new sink methods participate
// in this contract (rather than panicking under pressure).

#[tokio::test]
async fn database_created_events_drop_silently_when_channel_is_full() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("audit.log");
    let (_tx, rx) = watch::channel(false);
    let sink = AuditSink::file(path.clone(), 10_000_000, 3, 0, rx).unwrap();

    // CHANNEL_CAPACITY = 10_000. Push 15_000 events synchronously
    // before the writer task can drain — at least some will fill the
    // channel and trip `try_send` failure. The sink must not panic.
    for i in 0..15_000_u64 {
        sink.database_created(
            i,
            "admin",
            "plantA",
            DatabaseOptionsAudit {
                max_size_bytes: None,
                max_connections: None,
            },
            AuditOutcome::Success,
        );
    }
    // Sink dropped: writer task finishes, audit_backpressure (if
    // present) is emitted, then file is flushed.
    drop(sink);
    tokio::time::sleep(Duration::from_millis(500)).await;

    let contents = std::fs::read_to_string(&path).unwrap();
    let types: Vec<String> = contents
        .lines()
        .map(|l| parse_line(l)["event_type"].as_str().unwrap().to_owned())
        .collect();
    // The exact ratio of drops vs writes depends on scheduling, but
    // we MUST see at least one database_created (the writer drained
    // something) and the test must not panic — both already proved
    // by reaching this point. The `audit_backpressure` event is
    // emitted opportunistically only when the writer has room AFTER
    // a drop, so its presence is not guaranteed in this synchronous
    // pump pattern — assert what we can rely on.
    assert!(
        types.iter().any(|t| t == "database_created"),
        "at least one database_created must reach disk, got types: {types:?}"
    );
}

// Task 14 ciclo 4: synchronous sink for CLI offline emission. The
// asynchronous AuditSink::file() spawns a tokio task to drain an MPSC
// channel — a model designed for the server hot path. The CLI emits
// 1-2 events per invocation with no concurrent producers, so it uses
// a dedicated `AuditSink::file_sync_oneshot` that opens the file in
// append mode, writes a JSON line, fsyncs, and closes. No tokio task,
// no MPSC, no backpressure handling.

#[test]
fn file_sync_oneshot_writes_event_immediately() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cli-audit.log");
    let sink = AuditSink::file_sync_oneshot(&path).expect("sync sink");

    sink.database_created(
        1,
        "cli:501@host",
        "plantA",
        DatabaseOptionsAudit {
            max_size_bytes: Some(1_048_576),
            max_connections: None,
        },
        AuditOutcome::Success,
    );

    // No drop+sleep needed: file_sync_oneshot writes synchronously and
    // returns only after fsync completes.
    let contents = std::fs::read_to_string(&path).expect("audit log");
    let v: serde_json::Value = serde_json::from_str(contents.trim()).expect("JSON");
    assert_eq!(v["event_type"].as_str(), Some("database_created"));
    assert_eq!(v["user"].as_str(), Some("cli:501@host"));
    assert_eq!(v["details"]["name"].as_str(), Some("plantA"));
    assert_eq!(v["details"]["outcome"].as_str(), Some("success"));
}

#[test]
fn file_sync_oneshot_appends_multiple_events() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cli-audit.log");
    let sink = AuditSink::file_sync_oneshot(&path).expect("sync sink");

    sink.database_created(
        1,
        "cli:501@host",
        "plantA",
        DatabaseOptionsAudit {
            max_size_bytes: None,
            max_connections: None,
        },
        AuditOutcome::Success,
    );
    sink.database_dropped(2, "cli:501@host", "plantA", AuditOutcome::Success);

    let contents = std::fs::read_to_string(&path).expect("audit log");
    let lines: Vec<&str> = contents.lines().collect();
    assert_eq!(lines.len(), 2, "two events must produce two lines");
    let types: Vec<String> = lines
        .iter()
        .map(|l| {
            serde_json::from_str::<serde_json::Value>(l).unwrap()["event_type"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect();
    assert!(types.iter().any(|t| t == "database_created"));
    assert!(types.iter().any(|t| t == "database_dropped"));
}

#[test]
fn file_sync_oneshot_returns_error_when_parent_dir_missing_and_uncreatable() {
    // A path whose parent cannot be created (we use a file as the
    // parent component) surfaces the I/O error rather than panicking.
    let tmp = tempfile::tempdir().unwrap();
    let blocker = tmp.path().join("not-a-dir");
    std::fs::write(&blocker, b"plain file").unwrap();
    let path = blocker.join("audit.log");
    let result = AuditSink::file_sync_oneshot(&path);
    assert!(
        result.is_err(),
        "sync sink must surface I/O error when parent path is not a directory"
    );
}

#[tokio::test]
async fn file_sink_rotates_at_max_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("audit.log");
    let (_tx, rx) = watch::channel(false);
    let sink = AuditSink::file(path.clone(), 256, 3, 0, rx).unwrap();

    for i in 0..40 {
        sink.query_exec(i, "alice", "deadbeef", 1, 0, QueryOutcome::Success);
    }
    drop(sink);
    tokio::time::sleep(Duration::from_millis(400)).await;

    let rotated = dir.path().join("audit.log.1");
    assert!(rotated.exists(), "rotation produced audit.log.1");
}
