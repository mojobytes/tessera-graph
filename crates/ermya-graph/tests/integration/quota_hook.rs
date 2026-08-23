// SPDX-License-Identifier: MIT

//! Task 15 ciclo 1 — Pre-WAL quota hook API tests.
//!
//! Decision Q1 (Option C'): the hook fires at the entry of every write
//! operation BEFORE any in-memory mutation or WAL append. When it returns
//! `Err`, nothing has been written. The hook is installed via
//! `Graph::open_with_hook` (Decision Q2: builder, not setter) and produces
//! a `ermya_graph::Error::QuotaExceeded` variant (Decision Q3).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use ermya_graph::{Error, Graph, GraphConfig, props};
use tempfile::TempDir;

const fn test_config() -> GraphConfig {
    GraphConfig {
        memory_limit_bytes: 64 * 1024,
        create_if_missing: true,
        adj_cache_capacity: 1024,
        wal_enabled: true,
        ..GraphConfig::new()
    }
}

fn always_ok_hook() -> Box<dyn Fn() -> ermya_graph::Result<()> + Send + Sync> {
    Box::new(|| Ok(()))
}

fn always_quota_exceeded_hook(
    path: String,
) -> Box<dyn Fn() -> ermya_graph::Result<()> + Send + Sync> {
    Box::new(move || {
        Err(Error::QuotaExceeded {
            path: path.clone(),
            limit_bytes: 1024,
            current_bytes: 2048,
        })
    })
}

#[test]
fn open_without_hook_is_unaffected() {
    let tmp = TempDir::new().unwrap();
    let mut g = Graph::open(tmp.path(), &test_config()).unwrap();
    let n = g
        .add_node("Person", props! { "name" => "Alice" })
        .expect("add_node succeeds on a graph without a quota hook");
    assert_eq!(g.node_count(), 1);
    let _ = n;
}

#[test]
fn open_with_hook_returning_ok_does_not_block_writes() {
    let tmp = TempDir::new().unwrap();
    let mut g = Graph::open_with_hook(tmp.path(), &test_config(), always_ok_hook()).unwrap();
    g.add_node("Person", props! { "name" => "Alice" })
        .expect("hook returning Ok must not block writes");
    g.add_node("Person", props! { "name" => "Bob" })
        .expect("hook returning Ok must not block writes (second call)");
    assert_eq!(g.node_count(), 2);
}

#[test]
fn open_with_hook_returning_err_rejects_writes_cleanly() {
    let tmp = TempDir::new().unwrap();
    let path_for_hook = tmp.path().to_string_lossy().into_owned();
    let mut g = Graph::open_with_hook(
        tmp.path(),
        &test_config(),
        always_quota_exceeded_hook(path_for_hook),
    )
    .unwrap();

    let err = g
        .add_node("Person", props! { "name" => "Alice" })
        .expect_err("hook returning Err must propagate as Error::QuotaExceeded");
    match err {
        Error::QuotaExceeded {
            limit_bytes,
            current_bytes,
            ..
        } => {
            assert_eq!(limit_bytes, 1024);
            assert_eq!(current_bytes, 2048);
        }
        other => panic!("expected Error::QuotaExceeded, got {other:?}"),
    }

    // C': the rejection is BEFORE any state mutation. node_count is still 0.
    assert_eq!(
        g.node_count(),
        0,
        "C' contract: nothing is written when the hook rejects"
    );
}

#[test]
fn rejected_write_does_not_persist_across_reopen() {
    // Reopen with no hook (so reads are not blocked). The rejected write
    // must not appear in the WAL — opening the same path with a plain
    // Graph::open must see node_count == 0.
    let tmp = TempDir::new().unwrap();
    let path_for_hook = tmp.path().to_string_lossy().into_owned();
    {
        let mut g = Graph::open_with_hook(
            tmp.path(),
            &test_config(),
            always_quota_exceeded_hook(path_for_hook),
        )
        .unwrap();
        let _ = g.add_node("Person", props! { "name" => "Alice" });
        // graph drops here, releasing the WAL writer
    }
    let g = Graph::open(tmp.path(), &test_config()).unwrap();
    assert_eq!(
        g.node_count(),
        0,
        "C' contract: rejected write must not be in the WAL"
    );
}

// Task 15 ciclo 7: regression guard. The hook adds a recursive
// `read_dir` per write (in real registry use; here the hook is a
// closure that does a real stat). A future refactor that turns the
// per-op overhead into O(N²) or blocks indefinitely on a slow
// filesystem would surface here as a timeout. The 30s bound is
// generous: the per-op stat itself is sub-millisecond on a local
// tempdir, so an unmodified hook completes the 256 KiB target in
// well under 5 seconds.
#[test]
fn write_throughput_with_real_stat_hook_stays_under_30_seconds() {
    use std::time::Instant;

    let tmp = TempDir::new().unwrap();
    let limit: u64 = 256 * 1024;
    // Closure that mirrors what the registry hook does in production:
    // recursive stat + threshold check. The engine test does not
    // depend on the server crate, so we re-implement the stat
    // inline using `std::fs`.
    let hook_dir = tmp.path().to_path_buf();
    let hook: Box<dyn Fn() -> ermya_graph::Result<()> + Send + Sync> = Box::new(move || {
        let mut total: u64 = 0;
        let mut stack: Vec<std::path::PathBuf> = vec![hook_dir.clone()];
        while let Some(p) = stack.pop() {
            let Ok(rd) = std::fs::read_dir(&p) else {
                continue;
            };
            for entry in rd.flatten() {
                if let Ok(meta) = entry.metadata() {
                    if meta.is_dir() {
                        stack.push(entry.path());
                    } else {
                        total = total.saturating_add(meta.len());
                    }
                }
            }
        }
        if total >= limit {
            Err(Error::QuotaExceeded {
                path: hook_dir.to_string_lossy().into_owned(),
                limit_bytes: limit,
                current_bytes: total,
            })
        } else {
            Ok(())
        }
    });

    let mut g = Graph::open_with_hook(tmp.path(), &test_config(), hook).unwrap();
    let start = Instant::now();
    let mut hit_quota = false;
    for i in 0..10_000_u64 {
        match g.add_node("T", props! { "n" => i.to_string(), "p" => "x".repeat(300) }) {
            Ok(_) => {}
            Err(Error::QuotaExceeded { .. }) => {
                hit_quota = true;
                break;
            }
            Err(other) => panic!("unexpected: {other:?}"),
        }
    }
    let elapsed = start.elapsed();
    assert!(
        hit_quota,
        "256 KiB quota must trigger within 10k iterations on 300-byte payloads"
    );
    assert!(
        elapsed.as_secs() < 30,
        "regression guard: hook + writes must complete in under 30s, got {elapsed:?}"
    );
}

#[test]
fn hook_fires_once_per_write_op() {
    // A counter hook proves the hook fires for every write entry point,
    // not just once at construction.
    let tmp = TempDir::new().unwrap();
    let calls = Arc::new(AtomicU64::new(0));
    let calls_inner = Arc::clone(&calls);
    let hook: Box<dyn Fn() -> ermya_graph::Result<()> + Send + Sync> = Box::new(move || {
        calls_inner.fetch_add(1, Ordering::Relaxed);
        Ok(())
    });
    let mut g = Graph::open_with_hook(tmp.path(), &test_config(), hook).unwrap();
    let a = g.add_node("Person", props! { "name" => "Alice" }).unwrap();
    let b = g.add_node("Person", props! { "name" => "Bob" }).unwrap();
    g.add_edge("KNOWS", a, b, props! {}).unwrap();
    // 3 mutating calls → at least 3 hook fires. The exact count may be
    // higher if a write op internally calls the hook from multiple seams
    // (e.g. add_edge could check both endpoints) but it must be >= 3.
    let n = calls.load(Ordering::Relaxed);
    assert!(
        n >= 3,
        "hook must fire at least 3 times for 2 add_node + 1 add_edge, got {n}"
    );
}
