// SPDX-License-Identifier: MIT

#![allow(clippy::significant_drop_tightening)]

use std::thread;
use std::time::{Duration, Instant};

use ermya_graph::{Graph, SharedGraph, props};

#[test]
fn two_writers_do_not_corrupt_graph() {
    let graph = SharedGraph::new(Graph::new());
    let n_threads = 4;
    let ops_per_thread = 100;

    let handles: Vec<_> = (0..n_threads)
        .map(|_| {
            let g = graph.clone();
            thread::spawn(move || {
                for i in 0..ops_per_thread {
                    g.write()
                        .add_node("Worker", props! { "i" => i64::try_from(i).unwrap() })
                        .unwrap();
                }
            })
        })
        .collect();

    for h in handles {
        h.join().unwrap();
    }

    assert_eq!(graph.read().node_count(), n_threads * ops_per_thread);
}

#[test]
fn concurrent_readers_do_not_block_each_other() {
    let graph = SharedGraph::new(Graph::new());
    {
        let mut g = graph.write();
        for i in 0..1000_i64 {
            g.add_node("N", props! { "v" => i }).unwrap();
        }
    }

    let start = Instant::now();
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let g = graph.clone();
            thread::spawn(move || {
                let _count = g.read().node_count();
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }

    // 8 concurrent readers should complete well under 100ms
    assert!(
        start.elapsed() < Duration::from_millis(100),
        "concurrent readers took too long: {:?}",
        start.elapsed()
    );
}

#[test]
fn shared_graph_clone_shares_state() {
    let g1 = SharedGraph::new(Graph::new());
    let g2 = g1.clone();

    let id = g1.write().add_node("A", props! {}).unwrap();
    let node = g2.read().node(id).unwrap();
    assert_eq!(node.label(), "A");
}

#[test]
fn writer_blocks_readers_correctly() {
    // Ensure that data written under a write lock is visible to a subsequent reader.
    let graph = SharedGraph::new(Graph::new());
    let graph2 = graph.clone();

    let writer = thread::spawn(move || {
        let mut g = graph2.write();
        for i in 0..50_i64 {
            g.add_node("N", props! { "i" => i }).unwrap();
        }
    });

    writer.join().unwrap();
    assert_eq!(graph.read().node_count(), 50);
}
