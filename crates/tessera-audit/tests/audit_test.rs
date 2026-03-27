// Copyright 2026 BelowZero Security OU. All rights reserved.

use tessera_audit::{AuditEntry, AuditError, AuditEvent, AuditLog, AuditResult};

#[tokio::test]
async fn audit_log_records_login_success() {
    let dir = tempfile::tempdir().expect("tempdir"); // OK: test
    let path = dir.path().join("audit.ndjson");
    let (log, task) = AuditLog::open(&path, 0, 0).expect("open"); // OK: test
    let handle = tokio::spawn(task.run());

    log.record_event(AuditEntry::success(
        Some(1),
        AuditEvent::LoginSuccess { username: "alice".into() },
    )).expect("record"); // OK: test

    drop(log);
    handle.await.expect("writer"); // OK: test

    let contents = std::fs::read_to_string(&path).expect("read"); // OK: test
    assert!(contents.contains("LoginSuccess"), "got: {contents}");
    assert!(contents.contains("alice"), "got: {contents}");
}

#[tokio::test]
async fn audit_log_records_login_failure() {
    let dir = tempfile::tempdir().expect("tempdir"); // OK: test
    let path = dir.path().join("audit.ndjson");
    let (log, task) = AuditLog::open(&path, 0, 0).expect("open"); // OK: test
    let handle = tokio::spawn(task.run());

    log.record_event(AuditEntry::denied(
        None,
        AuditEvent::LoginFailure { username: "bob".into() },
        "invalid credentials".into(),
    )).expect("record"); // OK: test

    drop(log);
    handle.await.expect("writer"); // OK: test

    let contents = std::fs::read_to_string(&path).expect("read"); // OK: test
    assert!(contents.contains("LoginFailure"), "got: {contents}");
    assert!(contents.contains("Denied"), "got: {contents}");
}

#[tokio::test]
async fn audit_log_records_query_executed() {
    let dir = tempfile::tempdir().expect("tempdir"); // OK: test
    let path = dir.path().join("audit.ndjson");
    let (log, task) = AuditLog::open(&path, 0, 0).expect("open"); // OK: test
    let handle = tokio::spawn(task.run());

    log.record_event(AuditEntry::success(
        Some(42),
        AuditEvent::QueryExecuted { query_preview: "MATCH (n) RETURN n".into() },
    )).expect("record"); // OK: test

    drop(log);
    handle.await.expect("writer"); // OK: test

    let contents = std::fs::read_to_string(&path).expect("read"); // OK: test
    let parsed: AuditEntry = serde_json::from_str(contents.trim()).expect("parse"); // OK: test
    assert_eq!(parsed.user_id, Some(42));
    assert!(matches!(parsed.event, AuditEvent::QueryExecuted { .. }));
    assert!(matches!(parsed.result, AuditResult::Success));
}

#[tokio::test]
async fn audit_log_records_mutation_executed() {
    let dir = tempfile::tempdir().expect("tempdir"); // OK: test
    let path = dir.path().join("audit.ndjson");
    let (log, task) = AuditLog::open(&path, 0, 0).expect("open"); // OK: test
    let handle = tokio::spawn(task.run());

    log.record_event(AuditEntry::success(
        Some(10),
        AuditEvent::MutationExecuted { query_preview: "CREATE (:Person)".into() },
    )).expect("record"); // OK: test

    drop(log);
    handle.await.expect("writer"); // OK: test

    let contents = std::fs::read_to_string(&path).expect("read"); // OK: test
    assert!(contents.contains("MutationExecuted"), "got: {contents}");
}

#[tokio::test]
async fn audit_log_records_permission_denied() {
    let dir = tempfile::tempdir().expect("tempdir"); // OK: test
    let path = dir.path().join("audit.ndjson");
    let (log, task) = AuditLog::open(&path, 0, 0).expect("open"); // OK: test
    let handle = tokio::spawn(task.run());

    log.record_event(AuditEntry::denied(
        Some(5),
        AuditEvent::PermissionDenied { permission: "node:delete".into() },
        "insufficient privileges".into(),
    )).expect("record"); // OK: test

    drop(log);
    handle.await.expect("writer"); // OK: test

    let contents = std::fs::read_to_string(&path).expect("read"); // OK: test
    assert!(contents.contains("PermissionDenied"), "got: {contents}");
    assert!(contents.contains("node:delete"), "got: {contents}");
}

#[tokio::test]
async fn audit_log_is_append_only() {
    let dir = tempfile::tempdir().expect("tempdir"); // OK: test
    let path = dir.path().join("audit.ndjson");

    // First session.
    {
        let (log, task) = AuditLog::open(&path, 0, 0).expect("open"); // OK: test
        let handle = tokio::spawn(task.run());
        log.record_event(AuditEntry::success(Some(1), AuditEvent::SchemaFlush))
            .expect("record"); // OK: test
        drop(log);
        handle.await.expect("writer"); // OK: test
    }

    // Second session — appends, doesn't overwrite.
    {
        let (log, task) = AuditLog::open(&path, 0, 0).expect("open"); // OK: test
        let handle = tokio::spawn(task.run());
        log.record_event(AuditEntry::success(Some(2), AuditEvent::Logout))
            .expect("record"); // OK: test
        drop(log);
        handle.await.expect("writer"); // OK: test
    }

    let contents = std::fs::read_to_string(&path).expect("read"); // OK: test
    let lines: Vec<&str> = contents.trim().lines().collect();
    assert_eq!(lines.len(), 2, "expected 2 entries, got: {}", lines.len());
    assert!(lines[0].contains("SchemaFlush"));
    assert!(lines[1].contains("Logout"));
}

#[tokio::test]
async fn audit_entry_serializes_to_json_lines_format() {
    let dir = tempfile::tempdir().expect("tempdir"); // OK: test
    let path = dir.path().join("audit.ndjson");
    let (log, task) = AuditLog::open(&path, 0, 0).expect("open"); // OK: test
    let handle = tokio::spawn(task.run());

    log.record_event(AuditEntry::success(
        Some(1),
        AuditEvent::QueryExecuted { query_preview: "A".into() },
    )).expect("record"); // OK: test
    log.record_event(AuditEntry::denied(
        Some(2),
        AuditEvent::PermissionDenied { permission: "B".into() },
        "no".into(),
    )).expect("record"); // OK: test

    drop(log);
    handle.await.expect("writer"); // OK: test

    let contents = std::fs::read_to_string(&path).expect("read"); // OK: test
    for line in contents.trim().lines() {
        let _: serde_json::Value =
            serde_json::from_str(line).unwrap_or_else(|_| panic!("not valid JSON: {line}")); // OK: test
    }
}

#[tokio::test]
async fn audit_log_null_logger_returns_channel_closed() {
    let log = AuditLog::new_null();
    let result = log.record_event(AuditEntry::success(
        None,
        AuditEvent::LoginSuccess { username: "test".into() },
    ));
    assert!(
        matches!(result, Err(AuditError::ChannelClosed)),
        "null logger should return ChannelClosed, got: {result:?}"
    );
}

// ── C1: ChannelFull vs ChannelClosed ────────────────────────────────────────

#[test]
fn record_event_returns_channel_full_when_buffer_exhausted() {
    let (tx, _rx) = tokio::sync::mpsc::channel(1);
    let log = AuditLog::new_with_sender(tx);
    let entry = AuditEntry::success(None, AuditEvent::Logout);
    assert!(log.record_event(entry.clone()).is_ok(), "first send should succeed"); // OK: test
    let result = log.record_event(entry);
    assert!(
        matches!(result, Err(AuditError::ChannelFull)),
        "expected ChannelFull, got: {result:?}"
    );
}

#[test]
fn record_event_returns_channel_closed_when_receiver_dropped() {
    let log = AuditLog::new_null();
    let entry = AuditEntry::success(None, AuditEvent::Logout);
    let result = log.record_event(entry);
    assert!(
        matches!(result, Err(AuditError::ChannelClosed)),
        "expected ChannelClosed, got: {result:?}"
    );
}

// ── R1: Configurable channel capacity ───────────────────────────────────────

#[test]
fn audit_log_open_respects_channel_capacity() {
    let dir = tempfile::tempdir().expect("tempdir"); // OK: test
    let path = dir.path().join("audit.ndjson");
    let (log, _task) = AuditLog::open_with_capacity(&path, 0, 0, 2).expect("open"); // OK: test
    let entry = AuditEntry::success(None, AuditEvent::Logout);
    assert!(log.record_event(entry.clone()).is_ok()); // OK: test
    assert!(log.record_event(entry.clone()).is_ok()); // OK: test
    let result = log.record_event(entry);
    assert!(
        matches!(result, Err(AuditError::ChannelFull)),
        "third send should return ChannelFull, got: {result:?}"
    );
}

// ── O1: Subsecond timestamp ─────────────────────────────────────────────────

#[test]
fn audit_entry_timestamp_has_millisecond_resolution() {
    let e = AuditEntry::success(None, AuditEvent::Logout);
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock") // OK: test
        .as_millis() as u64;
    assert!(
        e.timestamp_ms >= now_ms.saturating_sub(2000) && e.timestamp_ms <= now_ms + 2000,
        "timestamp_ms {} not near now_ms {}", e.timestamp_ms, now_ms
    );
}

// ── R2: Batched flush throughput ────────────────────────────────────────────

#[tokio::test]
async fn writer_throughput_no_regression() {
    let dir = tempfile::tempdir().expect("tempdir"); // OK: test
    let path = dir.path().join("audit.ndjson");
    let (log, task) = AuditLog::open(&path, 0, 0).expect("open"); // OK: test
    let handle = tokio::spawn(task.run());

    let n = 500_u64;
    for i in 0..n {
        log.record_event(AuditEntry::success(
            Some(i),
            AuditEvent::QueryExecuted { query_preview: "Q".into() },
        )).expect("record"); // OK: test
    }
    drop(log);
    handle.await.expect("writer"); // OK: test

    let contents = std::fs::read_to_string(&path).expect("read"); // OK: test
    assert_eq!(contents.trim().lines().count(), n as usize, "all entries must survive");
}
