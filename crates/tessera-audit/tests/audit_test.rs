// Copyright 2026 BelowZero Security OU. All rights reserved.

use tessera_audit::{AuditEntry, AuditLog, AuditResult};

#[test]
fn audit_log_records_successful_operation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("audit.ndjson");
    let log = AuditLog::open(&path).unwrap();

    log.record_success(Some(1), "CREATE_NODE", Some("Person"))
        .unwrap();

    let contents = std::fs::read_to_string(&path).unwrap();
    assert!(contents.contains("CREATE_NODE"));
    assert!(contents.contains("Person"));
}

#[test]
fn audit_log_records_denied_operation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("audit.ndjson");
    let log = AuditLog::open(&path).unwrap();

    log.record_denied(Some(2), "DELETE_NODE", Some("Person"), "permission denied")
        .unwrap();

    let contents = std::fs::read_to_string(&path).unwrap();
    assert!(contents.contains("Denied"));
    assert!(contents.contains("permission denied"));
}

#[test]
fn audit_log_preserves_user_timestamp_operation_and_result() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("audit.ndjson");
    let log = AuditLog::open(&path).unwrap();

    log.record_success(Some(42), "QUERY", Some("MATCH (n) RETURN n"))
        .unwrap();

    let contents = std::fs::read_to_string(&path).unwrap();
    let entry: AuditEntry = serde_json::from_str(contents.trim()).unwrap();
    assert_eq!(entry.user_id, Some(42));
    assert_eq!(entry.operation, "QUERY");
    assert_eq!(entry.target.as_deref(), Some("MATCH (n) RETURN n"));
    assert!(matches!(entry.result, AuditResult::Success));
    assert!(entry.timestamp_unix > 0);
}

#[test]
fn audit_log_is_append_only() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("audit.ndjson");

    // Write first entry
    {
        let log = AuditLog::open(&path).unwrap();
        log.record_success(Some(1), "OP_A", None).unwrap();
    }

    // Reopen and write second entry
    {
        let log = AuditLog::open(&path).unwrap();
        log.record_success(Some(2), "OP_B", None).unwrap();
    }

    let contents = std::fs::read_to_string(&path).unwrap();
    let lines: Vec<&str> = contents.trim().lines().collect();
    assert_eq!(lines.len(), 2);
    assert!(lines[0].contains("OP_A"));
    assert!(lines[1].contains("OP_B"));
}

#[test]
fn audit_log_survives_roundtrip_to_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("audit.ndjson");
    let log = AuditLog::open(&path).unwrap();

    log.record_success(Some(10), "BACKUP", Some("/tmp/snap"))
        .unwrap();

    let contents = std::fs::read_to_string(&path).unwrap();
    let entry: AuditEntry = serde_json::from_str(contents.trim()).unwrap();
    assert_eq!(entry.user_id, Some(10));
    assert_eq!(entry.operation, "BACKUP");
}

#[test]
fn audit_log_records_error_operation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("audit.ndjson");
    let log = AuditLog::open(&path).unwrap();

    log.record_error(Some(3), "LOAD_SNAPSHOT", Some("/tmp/snap"), "lock poisoned")
        .unwrap();

    let contents = std::fs::read_to_string(&path).unwrap();
    let entry: AuditEntry = serde_json::from_str(contents.trim()).unwrap();
    assert_eq!(entry.user_id, Some(3));
    assert_eq!(entry.operation, "LOAD_SNAPSHOT");
    assert_eq!(entry.target.as_deref(), Some("/tmp/snap"));
    assert!(
        matches!(entry.result, AuditResult::Error { ref message } if message == "lock poisoned")
    );
    assert!(entry.timestamp_unix > 0);
}

#[test]
fn audit_entry_serializes_to_json_lines_format() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("audit.ndjson");
    let log = AuditLog::open(&path).unwrap();

    log.record_success(Some(1), "A", None).unwrap();
    log.record_denied(Some(2), "B", None, "forbidden").unwrap();

    let contents = std::fs::read_to_string(&path).unwrap();
    // Each entry is a single line of valid JSON (NDJSON format)
    for line in contents.trim().lines() {
        let _: serde_json::Value = serde_json::from_str(line)
            .unwrap_or_else(|_| panic!("not valid JSON: {line}"));
    }
}
