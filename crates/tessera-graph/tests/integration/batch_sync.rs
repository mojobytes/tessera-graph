// SPDX-License-Identifier: MIT

use std::time::Instant;
use tempfile::TempDir;
use tessera_graph::{Graph, GraphConfig, Properties, props};

const fn wal_config() -> GraphConfig {
    GraphConfig {
        memory_limit_bytes: 64 * 1024,
        create_if_missing: true,
        adj_cache_capacity: 1024,
        wal_enabled: true,
        ..GraphConfig::new()
    }
}

#[test]
fn begin_end_batch_basic_lifecycle() {
    let mut g = Graph::new();
    g.begin_batch();
    g.add_node("N", Properties::new()).unwrap();
    g.end_batch().unwrap();
}

#[test]
fn end_batch_without_begin_is_noop() {
    let mut g = Graph::new();
    // Should not panic or error.
    g.end_batch().unwrap();
}

#[test]
fn batch_nodes_readable_before_end_batch() {
    let tmp = TempDir::new().unwrap();
    let mut g = Graph::open(tmp.path(), &wal_config()).unwrap();

    g.begin_batch();
    let id = g.add_node("Person", props! { "name" => "Alice" }).unwrap();

    // Data is in memory — should be readable even before end_batch.
    let node = g.node(id).unwrap();
    assert_eq!(node.label(), "Person");

    g.end_batch().unwrap();
}

#[test]
fn batch_nodes_persist_after_end_batch_and_flush() {
    let tmp = TempDir::new().unwrap();
    let config = wal_config();

    {
        let mut g = Graph::open(tmp.path(), &config).unwrap();
        g.begin_batch();
        for _ in 0..100 {
            g.add_node("N", Properties::new()).unwrap();
        }
        g.end_batch().unwrap();
        g.flush().unwrap();
    }

    // Reopen and verify.
    {
        let g = Graph::open(tmp.path(), &config).unwrap();
        assert_eq!(g.node_count(), 100);
    }
}

#[test]
fn nested_batch_only_syncs_on_outermost_end() {
    let tmp = TempDir::new().unwrap();
    let config = wal_config();

    {
        let mut g = Graph::open(tmp.path(), &config).unwrap();
        g.begin_batch();
        g.add_node("A", Properties::new()).unwrap();

        g.begin_batch(); // nested
        g.add_node("B", Properties::new()).unwrap();
        g.end_batch().unwrap(); // inner — no fsync yet

        g.add_node("C", Properties::new()).unwrap();
        g.end_batch().unwrap(); // outer — fsync here

        g.flush().unwrap();
    }

    {
        let g = Graph::open(tmp.path(), &config).unwrap();
        assert_eq!(g.node_count(), 3);
    }
}

#[test]
fn without_batch_behavior_unchanged() {
    let tmp = TempDir::new().unwrap();
    let config = wal_config();

    {
        let mut g = Graph::open(tmp.path(), &config).unwrap();
        g.add_node("Solo", Properties::new()).unwrap();
        g.flush().unwrap();
    }

    {
        let g = Graph::open(tmp.path(), &config).unwrap();
        assert_eq!(g.node_count(), 1);
    }
}

#[test]
fn flush_inside_batch_still_works() {
    let tmp = TempDir::new().unwrap();
    let config = wal_config();

    {
        let mut g = Graph::open(tmp.path(), &config).unwrap();
        g.begin_batch();
        for _ in 0..10 {
            g.add_node("N", Properties::new()).unwrap();
        }
        g.flush().unwrap(); // should checkpoint WAL even inside batch
        g.end_batch().unwrap();
    }

    {
        let g = Graph::open(tmp.path(), &config).unwrap();
        assert_eq!(g.node_count(), 10);
    }
}

#[test]
fn batch_with_mixed_mutations() {
    let tmp = TempDir::new().unwrap();
    let config = wal_config();

    {
        let mut g = Graph::open(tmp.path(), &config).unwrap();
        g.begin_batch();

        let a = g.add_node("A", props! { "x" => 1 }).unwrap();
        let b = g.add_node("B", Properties::new()).unwrap();
        let e = g.add_edge("KNOWS", a, b, Properties::new()).unwrap();

        // Update node
        let mut node_a = g.node(a).unwrap();
        node_a
            .properties_mut()
            .insert("x".into(), tessera_graph::Property::I64(42));
        g.update_node(a, &node_a).unwrap();

        // Remove edge
        g.remove_edge(e).unwrap();

        g.end_batch().unwrap();
        g.flush().unwrap();
    }

    {
        let g = Graph::open(tmp.path(), &config).unwrap();
        assert_eq!(g.node_count(), 2);
        assert_eq!(g.edge_count(), 0);
    }
}

#[test]
#[ignore = "I/O-bound: flaky under parallel test contention; run with --ignored"]
fn batch_throughput_regression_guard() {
    let n = 200;

    // With batch.
    let tmp_batch = TempDir::new().unwrap();
    let config = wal_config();
    let batch_time = {
        let mut g = Graph::open(tmp_batch.path(), &config).unwrap();
        let start = Instant::now();
        g.begin_batch();
        for _ in 0..n {
            g.add_node("N", Properties::new()).unwrap();
        }
        g.end_batch().unwrap();
        start.elapsed()
    };

    // Without batch.
    let tmp_no_batch = TempDir::new().unwrap();
    let no_batch_time = {
        let mut g = Graph::open(tmp_no_batch.path(), &config).unwrap();
        let start = Instant::now();
        for _ in 0..n {
            g.add_node("N", Properties::new()).unwrap();
        }
        start.elapsed()
    };

    // Batch should be significantly faster. We use a generous 3x threshold
    // to avoid flakiness under I/O contention (e.g. parallel test runs).
    assert!(
        batch_time < no_batch_time / 3,
        "batch mode ({batch_time:?}) should be at least 3x faster than no-batch ({no_batch_time:?})"
    );
}

// ── Issue #43 Part B: fsync cause reaches the observer by identity ────────
//
// End to end through the real file-backed WAL path: a write outside a batch
// must reach the observer as an Individual fsync, and closing a batch of N
// operations must reach it as a single BatchClose carrying op_count == N. This
// lets a consumer measure coalescence by reading the cause off the fsync,
// instead of inferring the batch boundary by counting how many fsyncs fired.
#[test]
fn wal_observer_distinguishes_individual_and_batch_close_causes() {
    use std::sync::{Arc, Mutex};
    use tessera_graph::{FsyncCause, WalObserver};

    let tmp = TempDir::new().unwrap();
    let causes: Arc<Mutex<Vec<FsyncCause>>> = Arc::new(Mutex::new(vec![]));
    let causes_clone = Arc::clone(&causes);
    let obs: WalObserver = Box::new(move |cause: FsyncCause, _d| {
        causes_clone.lock().unwrap().push(cause);
    });

    let mut g = Graph::open_with_wal_observer(tmp.path(), &wal_config(), obs).unwrap();

    // One write outside any batch → one Individual fsync.
    g.add_node("Event", Properties::new()).unwrap();

    // A batch of two writes → one BatchClose fsync coalescing both.
    g.begin_batch();
    g.add_node("Event", Properties::new()).unwrap();
    g.add_node("Event", Properties::new()).unwrap();
    g.end_batch().unwrap();

    assert_eq!(
        *causes.lock().unwrap(),
        vec![
            FsyncCause::Individual,
            FsyncCause::BatchClose { op_count: 2 },
        ],
        "the observer sees the individual write, then one batch-close coalescing 2 ops",
    );
}
