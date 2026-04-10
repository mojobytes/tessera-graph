// Copyright 2026 BelowZero Security OU. All rights reserved.

use tessera_graph_audit::{AuditEntry, AuditEvent, AuditLog};

#[tokio::test]
async fn rotation_creates_new_file_when_size_exceeded() {
    let dir = tempfile::tempdir().expect("tempdir"); // OK: test
    let path = dir.path().join("audit.ndjson");

    // Very small rotation threshold to trigger quickly.
    let (log, task) = AuditLog::open(&path, 200, 0).expect("open"); // OK: test
    let handle = tokio::spawn(task.run());

    // Write enough entries to exceed 200 bytes.
    for i in 0..20 {
        log.record_event(AuditEntry::success(
            Some(i),
            AuditEvent::QueryExecuted {
                query_preview: format!("MATCH (n{i}) RETURN n{i}"),
            },
        ))
        .expect("record"); // OK: test
    }

    // Close the channel and wait for the writer to flush all entries.
    drop(log);
    handle.await.expect("writer task"); // OK: test

    // Should have at least 2 files (current + rotated).
    let files: Vec<_> = std::fs::read_dir(dir.path())
        .expect("readdir") // OK: test
        .filter_map(Result::ok)
        .filter(|e| {
            e.path()
                .extension()
                .is_some_and(|ext| ext == "ndjson")
        })
        .collect();
    assert!(
        files.len() >= 2,
        "expected at least 2 files after rotation, got {}",
        files.len()
    );
}

#[tokio::test]
async fn rotation_preserves_all_entries_across_files() {
    let dir = tempfile::tempdir().expect("tempdir"); // OK: test
    let path = dir.path().join("audit.ndjson");

    let (log, task) = AuditLog::open(&path, 200, 0).expect("open"); // OK: test
    let handle = tokio::spawn(task.run());

    let total = 30;
    for i in 0..total {
        log.record_event(AuditEntry::success(
            Some(i),
            AuditEvent::QueryExecuted {
                query_preview: format!("Q{i}"),
            },
        ))
        .expect("record"); // OK: test
    }

    // Drop the sender to signal the writer task to finish, then wait for it.
    drop(log);
    handle.await.expect("writer task"); // OK: test

    // Count total lines across all .ndjson files.
    let mut total_lines = 0_usize;
    for entry in std::fs::read_dir(dir.path()).expect("readdir").flatten() {
        // OK: test
        let p = entry.path();
        if p.extension().is_some_and(|ext| ext == "ndjson") {
            let contents = std::fs::read_to_string(&p).expect("read"); // OK: test
            total_lines += contents.trim().lines().count();
        }
    }
    assert_eq!(
        total_lines,
        usize::try_from(total).expect("test constant fits usize"),
        "all {total} entries must survive rotation"
    );
}

#[tokio::test]
async fn rotation_disabled_when_max_size_zero() {
    let dir = tempfile::tempdir().expect("tempdir"); // OK: test
    let path = dir.path().join("audit.ndjson");

    let (log, task) = AuditLog::open(&path, 0, 0).expect("open"); // OK: test
    let handle = tokio::spawn(task.run());

    for i in 0..50 {
        log.record_event(AuditEntry::success(
            Some(i),
            AuditEvent::QueryExecuted {
                query_preview: format!("MATCH (n{i}) RETURN n{i}"),
            },
        ))
        .expect("record"); // OK: test
    }

    // Close the channel and wait for the writer to flush all entries.
    drop(log);
    handle.await.expect("writer task"); // OK: test

    let files: Vec<_> = std::fs::read_dir(dir.path())
        .expect("readdir") // OK: test
        .filter_map(Result::ok)
        .filter(|e| {
            e.path()
                .extension()
                .is_some_and(|ext| ext == "ndjson")
        })
        .collect();
    assert_eq!(files.len(), 1, "no rotation when max_size is 0");
}

#[tokio::test]
async fn rotation_prunes_old_files_when_max_rotated_files_set() {
    let dir = tempfile::tempdir().expect("tempdir"); // OK: test
    let path = dir.path().join("audit.ndjson");

    // Tiny rotation (100 bytes), keep max 2 rotated files.
    let (log, task) = AuditLog::open(&path, 100, 2).expect("open"); // OK: test
    let handle = tokio::spawn(task.run());

    // Write many entries to force multiple rotations.
    for i in 0..100 {
        log.record_event(AuditEntry::success(
            Some(i),
            AuditEvent::QueryExecuted {
                query_preview: format!("MATCH (node_{i}) RETURN node_{i}"),
            },
        ))
        .expect("record"); // OK: test
    }

    // Close the channel and wait for the writer to flush all entries.
    drop(log);
    handle.await.expect("writer task"); // OK: test

    let files: Vec<_> = std::fs::read_dir(dir.path())
        .expect("readdir") // OK: test
        .filter_map(Result::ok)
        .filter(|e| {
            e.path()
                .extension()
                .is_some_and(|ext| ext == "ndjson")
        })
        .collect();

    // max_rotated_files=2 + current file = at most 3.
    assert!(
        files.len() <= 3,
        "expected at most 3 files (2 rotated + current), got {}",
        files.len()
    );
}
