// SPDX-License-Identifier: MIT

//! Performance regression guard for Block 4 MVCC (Phase 3, Cycle 17).
//!
//! MVCC adds `Option`-gated branches to the node write and read paths. This
//! guard verifies that a graph with MVCC **disabled** (the legacy default, how
//! the overwhelming majority of the engine runs today) does not pay a
//! measurable penalty for those branches.
//!
//! The check is ratio-based, not an absolute ops/s threshold: the machine is
//! never guaranteed idle during development, and an absolute guard falsely
//! fails under unrelated CPU load (see the project's throughput-guard lesson).
//! We compare the non-MVCC `add_node` path against a `HashSet::insert` baseline
//! measured in the same process, so both scale together with whatever load the
//! machine is under. Only a genuine per-op regression in the write path (not
//! ambient load) moves the ratio.

use std::collections::HashSet;
use std::time::Instant;

use ermya_graph::{Graph, Properties};

const N: usize = 50_000;

#[test]
fn add_node_without_mvcc_not_regressed_vs_baseline() {
    // Baseline: N HashSet inserts — a trivial in-memory operation that tracks
    // the machine's current speed without touching the engine.
    let mut set: HashSet<u64> = HashSet::new();
    let baseline_start = Instant::now();
    for i in 0..N as u64 {
        set.insert(i);
    }
    let baseline = baseline_start.elapsed().as_secs_f64().max(f64::EPSILON);
    // Read the set so the inserts are not optimized away and the baseline is real.
    assert_eq!(set.len(), N);

    // Subject: N add_node calls on a graph that NEVER enables MVCC.
    let mut g = Graph::new();
    assert!(!g.mvcc_enabled(), "guard must measure the legacy path");
    let subject_start = Instant::now();
    for _ in 0..N {
        g.add_node("N", Properties::new()).unwrap();
    }
    let subject = subject_start.elapsed().as_secs_f64();

    // add_node does real work (id bump, slot encode, index updates), so it is
    // legitimately slower than a HashSet insert. The generous ceiling catches a
    // gross regression (e.g. the MVCC branch accidentally running for every
    // legacy write) without flaking on the constant-factor difference or on
    // ambient CPU load.
    let ratio = subject / baseline;
    assert!(
        ratio < 400.0,
        "non-MVCC add_node is {ratio:.0}x a HashSet insert (ceiling 400x): \
         possible legacy write-path regression"
    );
    assert_eq!(g.node_count(), N);
}
