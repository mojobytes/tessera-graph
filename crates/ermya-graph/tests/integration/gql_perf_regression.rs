// SPDX-License-Identifier: MIT

//! Performance regression guards for GQL query paths.
//!
//! These tests verify that:
//! 1. Fixed-path queries do not regress after variable-length path support.
//! 2. Variable-length bounded queries complete within reasonable time.
//! 3. Cyclic graphs with variable-length paths terminate promptly.
//!
//! # What makes a wall-clock guard real (issue #69)
//!
//! A ceiling only guards something if disabling the optimization it watches
//! pushes the measurement across it. That is a property of the SCENARIO, not of
//! the number: a scenario that never exercises what the optimization avoids
//! produces a small gap no ceiling can straddle, because separating it from
//! machine noise would need less headroom than the noise itself.
//!
//! Every ceiling below is stated with the signal measured behind it — the
//! with-optimization and without-optimization timings that justify it. Measured
//! on an idle machine, release build, three runs per cell, by disabling each
//! optimization at its source. The probe that produced these numbers is
//! `examples/issue69_guard_scenario_probe.rs`; re-run it before re-tuning any
//! ceiling here, and if a scenario changes, re-measure its signal rather than
//! adjusting the ceiling to whatever the new timing happens to be.
//!
//! Two construction details turned out to decide whether a guard works at all,
//! and both are easy to get wrong:
//!
//! - **Nodes need properties.** Several optimizations here save on decoding
//!   node properties. Building the scenario with `Properties::new()` removes
//!   exactly what they avoid, shrinking a 5.2x signal to 2.3x.
//! - **Predicate order decides which index work happens.** `narrow_candidates`
//!   intersects property indices left to right. Putting the selective predicate
//!   first collapses the candidate set before intersection runs, so the
//!   intersection it is meant to guard never does any real work.
//!
//! Guards that compare two queries timed in the same run (a ratio, not a
//! ceiling) are immune to host load by construction and need no such headroom;
//! `fixed_path_throughput_not_regressed` is the model. Prefer a ratio whenever
//! the signal is below ~2.5x: a ceiling that tight is indistinguishable from
//! contention.
//!
//! # Not every optimization deserves a guard
//!
//! Two of these were found to be unguardable, and are documented as such rather
//! than left looking healthy:
//!
//! - `multi_hop_throughput_guard` — reusing one buffer across hops saves a few
//!   allocations per QUERY, while the work per query scales with the match
//!   count. No scenario makes that visible (15.363s vs 15.345s at 2 000 nodes
//!   and 6 hops). Neither a clock nor a counter would help.
//! - `expand_hop_clone_throughput_guard` — worth a flat 1.2-1.3x, inside the
//!   noise band of a shared machine.
//!
//! Both keep a ratio assertion that catches a catastrophic regression, and say
//! plainly what they cannot detect. A guard that silently detects nothing is
//! worse than none, because it reads as coverage.

use std::time::Instant;
use tempfile::TempDir;
use ermya_graph::{Direction, Graph, GraphConfig, Properties, props};

use crate::helpers::graph_builders::build_chain;

fn execute_query(graph: &Graph, query_str: &str) -> Vec<ermya_graph::gql::GqlRow> {
    let query = ermya_graph::gql::parse(query_str).unwrap();
    ermya_graph::gql::execute(graph, &query, 0).unwrap()
}

#[test]
fn fixed_path_throughput_not_regressed() {
    let mut g = Graph::new();
    build_chain(&mut g, "N", "R", 1000);

    let iterations = 100;

    // Baseline: single-node scan (no edge traversal).
    let baseline_query = "MATCH (a:N) RETURN id(a)";
    let baseline_start = Instant::now();
    for _ in 0..iterations {
        let _ = execute_query(&g, baseline_query);
    }
    let baseline_elapsed = baseline_start.elapsed();

    // Subject: fixed-hop edge traversal.
    let subject_query = "MATCH (a)-[r]->(b) RETURN id(b)";
    let subject_start = Instant::now();
    for _ in 0..iterations {
        let _ = execute_query(&g, subject_query);
    }
    let subject_elapsed = subject_start.elapsed();

    // Ratio-based: fixed-path query must not be more than 10x slower than
    // single-node scan. This avoids flaky absolute thresholds in CI.
    let ratio = subject_elapsed.as_secs_f64() / baseline_elapsed.as_secs_f64().max(f64::EPSILON);
    assert!(
        ratio < 10.0,
        "fixed-path query is {ratio:.1}x slower than baseline (threshold: 10x)"
    );
}

#[test]
fn var_len_path_1000_node_chain_within_safety_limit() {
    let mut g = Graph::new();
    // Create start node with a distinct label, then chain 999 more.
    let first = g.add_node("Start", props! {}).unwrap();
    let mut prev = first;
    for _ in 1..1000 {
        let next = g.add_node("N", props! {}).unwrap();
        g.add_edge("R", prev, next, props! {}).unwrap();
        prev = next;
    }

    let query_str = "MATCH (a:Start)-[*1..5]->(b) RETURN DISTINCT id(b)";
    let iterations = 100;

    let start = Instant::now();
    for _ in 0..iterations {
        let rows = execute_query(&g, query_str);
        assert_eq!(rows.len(), 5, "should find exactly 5 nodes within 5 hops");
    }
    let elapsed = start.elapsed();

    // Sanity: 100 iterations of bounded BFS on 1000 nodes must complete in < 5 seconds.
    assert!(
        elapsed.as_secs_f64() < 5.0,
        "var-len path too slow: {:.2}s (expected <5.0s)",
        elapsed.as_secs_f64()
    );
}

/// Guards the bindings handling in `expand_hop` and in the materialized
/// iterator (commit f34bc51).
///
/// What that commit actually optimized is worth stating precisely, because the
/// name "clone" points at the wrong half and issue #69 was reopened on that
/// misreading:
///
/// - **In `expand_hop`, it saves nothing.** Sharing the map and then inserting
///   through `Arc::make_mut` still deep-copies whenever another reference is
///   alive — and one always is, since the source bindings outlive the neighbour
///   loop. Verified directly: over 20 neighbours the copy fires 20/20 times,
///   and the entries copied are identical either way (100 vs 100 for 5 bound
///   variables). The old `map.clone()` and the new `Arc::clone` +
///   `make_mut` do the same work here.
/// - **The saving is in the iterator.** `Arc::try_unwrap` hands the map over
///   without copying when the pattern's own vector is the last owner, replacing
///   a full copy per emitted match. That is one avoided copy per RESULT ROW,
///   which is where the measured difference comes from.
///
/// Measured by restoring the pre-f34bc51 code: a flat **1.2-1.3x**, and flat is
/// the finding. Across a 400-node chain — 1 hop 0.394s -> 0.505s, 2 hops 0.482s
/// -> 0.583s, 4 hops 1.025s -> 1.279s, 6 hops 1.756s -> 2.136s. It does not
/// grow with the number of bound variables, which the "per-match map copy"
/// reading would predict; it tracks the row count, which the iterator reading
/// does.
///
/// 20-25% sits inside the noise band of a shared machine, so no absolute
/// ceiling can separate it from host contention. A deterministic counter was
/// considered and rejected: the only quantity that differs between the two
/// versions is maps moved vs copied in the iterator, and instrumenting that
/// means adding a counter to the hot path of every pattern query to guard a
/// 25% effect. The ratio below is the proportionate answer.
///
/// It compares the hop expansion against a bare scan of the same nodes, timed
/// in the same run: immune to host load, and it still catches a catastrophic
/// regression (a per-match O(N) copy, which multiplies rather than adds 25%).
#[test]
fn expand_hop_clone_throughput_guard() {
    // 100 source nodes, each with 20 KNOWS edges → 2000 PatternMatch results.
    let mut g = Graph::new();
    for _ in 0..100 {
        let src = g.add_node("S", props! {}).unwrap();
        for _ in 0..20 {
            let tgt = g.add_node("T", props! {}).unwrap();
            g.add_edge("KNOWS", src, tgt, props! {}).unwrap();
        }
    }

    let iterations = 50;

    // Baseline: scan the same 2000 target nodes with no hop expansion.
    let baseline_query = "MATCH (b:T) RETURN id(b)";
    let baseline_start = Instant::now();
    for _ in 0..iterations {
        let rows = execute_query(&g, baseline_query);
        assert_eq!(rows.len(), 2000);
    }
    let baseline_elapsed = baseline_start.elapsed();

    // Subject: reach the same 2000 nodes through a hop, producing one bindings
    // map per match — the path the Arc sharing lives on.
    let subject_query = "MATCH (a:S)-[:KNOWS]->(b:T) RETURN id(b)";
    let subject_start = Instant::now();
    for _ in 0..iterations {
        let rows = execute_query(&g, subject_query);
        assert_eq!(rows.len(), 2000);
    }
    let subject_elapsed = subject_start.elapsed();

    let ratio = subject_elapsed.as_secs_f64() / baseline_elapsed.as_secs_f64().max(f64::EPSILON);
    assert!(
        ratio < 10.0,
        "expand_hop is {ratio:.1}x slower than a bare scan of the same nodes \
         (threshold: 10x) — per-match bindings copying may have returned"
    );
}

#[test]
fn var_len_path_cycle_terminates() {
    let mut g = Graph::new();
    let a = g.add_node("P", props! {}).unwrap();
    let b = g.add_node("P", props! {}).unwrap();
    let c = g.add_node("P", props! {}).unwrap();
    g.add_edge("R", a, b, props! {}).unwrap();
    g.add_edge("R", b, c, props! {}).unwrap();
    g.add_edge("R", c, a, props! {}).unwrap();

    let start = Instant::now();
    let rows = execute_query(&g, "MATCH (a:P)-[*1..100]->(b) RETURN DISTINCT id(b)");
    let elapsed = start.elapsed();

    assert!(!rows.is_empty(), "should find reachable nodes");
    assert!(
        elapsed.as_secs_f64() < 1.0,
        "cycle traversal too slow: {:.2}s (expected <1.0s)",
        elapsed.as_secs_f64()
    );
}

#[test]
fn label_filter_hop_throughput_guard() {
    // 1 hub connected to 1000 nodes; only 1 matches label "Target".
    // The label-only fast-path should skip property deserialization for the
    // 999 non-matching "Filler" nodes.
    let mut g = Graph::new();
    let hub = g.add_node("Hub", Properties::new()).unwrap();

    g.begin_batch();
    for i in 0..999_i64 {
        // Realistic filler payload: the fast path's saving IS the property
        // decoding it skips for these 999 non-matching nodes, so a thin filler
        // understates it. Measured: thin fillers 0.27s -> 0.46s without the
        // fast path (1.7x); with the payload below, 0.26s -> 0.82s (3.1x).
        let t = g
            .add_node(
                "Filler",
                props! {
                    "idx" => i,
                    "data" => "padding",
                    "name" => format!("filler-{i}").as_str(),
                    "notes" => "lorem ipsum dolor sit amet consectetur adipiscing elit sed do",
                },
            )
            .unwrap();
        g.add_edge("LINK", hub, t, Properties::new()).unwrap();
    }
    let target = g
        .add_node("Target", props! { "idx" => 999_i64, "data" => "special" })
        .unwrap();
    g.add_edge("LINK", hub, target, Properties::new()).unwrap();
    g.end_batch().unwrap();

    let query_str = "MATCH (h:Hub)-[:LINK]->(t:Target) RETURN id(t)";
    let baseline_query = "MATCH (h:Hub)-[:LINK]->(t) RETURN id(t)";

    let iterations = 200;
    let baseline_start = Instant::now();
    for _ in 0..iterations {
        let rows = execute_query(&g, baseline_query);
        assert_eq!(rows.len(), 1_000);
    }
    let baseline_elapsed = baseline_start.elapsed();

    let start = Instant::now();
    for _ in 0..iterations {
        let rows = execute_query(&g, query_str);
        assert_eq!(rows.len(), 1);
    }
    let elapsed = start.elapsed();

    // Compare against work performed on the same graph and process instead of
    // an absolute wall-clock ceiling. External CPU contention affects both
    // measurements; losing the label-only fast path makes the filtered query
    // converge on the full materialisation baseline.
    let ratio = elapsed.as_secs_f64() / baseline_elapsed.as_secs_f64().max(f64::EPSILON);
    assert!(
        ratio < 0.8,
        "label-filtered traversal took {ratio:.2}x the unfiltered baseline \
         (threshold: 0.8x) — label-only fast path may be lost"
    );
}

/// Guards the label-hash check in `outgoing_edges_by_label` (commit ffc0838),
/// which skips string-heap and property-overflow resolution for edges whose
/// label does not match, instead of loading every edge in full and dropping the
/// non-matching ones afterwards.
///
/// Ratio-based, and measured rather than assumed. Restoring the pre-ffc0838
/// load-then-retain path costs 0.95s -> 1.46s with the edge properties below
/// (1.5x), and 1.20s -> 1.98s with far heavier ones (1.66x). Making the payload
/// heavier barely widens it, so no absolute ceiling can hold: 1.5x is inside
/// the noise band of a loaded machine, and a ceiling loose enough not to be
/// flaky would clear the regressed timing too.
///
/// Note the edges carry the properties, not the nodes. What the label check
/// avoids is resolving the EDGE's payload; with the bare edges this guard used
/// to build, disabling the optimization changed nothing at all (0.376s vs
/// 0.337s — it measured no difference whatsoever).
#[test]
fn neighbor_label_filter_throughput_guard() {
    // Hub node with 500 KNOWS + 500 LIKES edges; filter for KNOWS only.
    let mut g = Graph::new();
    let hub = g.add_node("Hub", Properties::new()).unwrap();

    g.begin_batch();
    for i in 0..500_i64 {
        let t = g.add_node("K", Properties::new()).unwrap();
        g.add_edge(
            "KNOWS",
            hub,
            t,
            props! { "since" => i, "note" => "lorem ipsum dolor sit amet consectetur adipiscing" },
        )
        .unwrap();
    }
    for i in 0..500_i64 {
        let t = g.add_node("L", Properties::new()).unwrap();
        g.add_edge(
            "LIKES",
            hub,
            t,
            props! { "since" => i, "note" => "lorem ipsum dolor sit amet consectetur adipiscing" },
        )
        .unwrap();
    }
    g.end_batch().unwrap();

    let iterations = 200;

    // Baseline: same traversal, no label filter — loads all 1 000 edges.
    let baseline_start = Instant::now();
    for _ in 0..iterations {
        let edges = g
            .neighbors(hub)
            .direction(Direction::Outgoing)
            .collect()
            .unwrap();
        assert_eq!(edges.len(), 1_000);
    }
    let baseline_elapsed = baseline_start.elapsed();

    // Subject: half the edges match, so the label check should skip resolving
    // the payload of the other 500.
    let subject_start = Instant::now();
    for _ in 0..iterations {
        let edges = g
            .neighbors(hub)
            .direction(Direction::Outgoing)
            .label("KNOWS")
            .collect()
            .unwrap();
        assert_eq!(edges.len(), 500);
    }
    let subject_elapsed = subject_start.elapsed();

    // Filtering to half the edges must not cost MORE than loading them all.
    // With the label check it is cheaper; with a post-hoc retain it converges
    // on the baseline, and a per-edge regression pushes it past.
    let ratio = subject_elapsed.as_secs_f64() / baseline_elapsed.as_secs_f64().max(f64::EPSILON);
    assert!(
        ratio < 1.5,
        "label-filtered traversal is {ratio:.2}x the cost of loading every edge \
         (threshold: 1.5x) — the edge label-hash check may be lost"
    );
}

#[test]
fn count_one_hop_throughput_guard() {
    // 1000 Container nodes, each with 5 Items = 5000 edges.
    //
    // The Items carry properties on purpose. COUNT pushdown counts edges
    // without materialising the neighbour nodes, so what it saves is precisely
    // the property decoding; with `Properties::new()` items there is almost
    // nothing to skip and the guard measures a difference that is not there.
    // Measured: empty items 0.38s -> 0.87s without pushdown (2.3x); with the
    // properties below, 0.37s -> 1.94s (5.2x). See the module header.
    let mut g = Graph::new();
    let mut item_idx = 0_i64;
    for _ in 0..1_000 {
        let container = g.add_node("Container", Properties::new()).unwrap();
        for _ in 0..5 {
            let item = g
                .add_node(
                    "Item",
                    props! {
                        "name" => format!("item-{item_idx}").as_str(),
                        "sku" => format!("SKU-{item_idx:08}").as_str(),
                        "qty" => item_idx,
                        "notes" => "lorem ipsum dolor sit amet consectetur adipiscing elit sed do",
                    },
                )
                .unwrap();
            g.add_edge("CONTAINS", container, item, Properties::new())
                .unwrap();
            item_idx += 1;
        }
    }

    let iterations = 50;
    let baseline_start = Instant::now();
    for _ in 0..iterations {
        let rows = execute_query(&g, "MATCH (p:Container)-[:CONTAINS]->(c:Item) RETURN c");
        assert_eq!(rows.len(), 5_000);
    }
    let baseline_elapsed = baseline_start.elapsed();

    let start = Instant::now();
    for _ in 0..iterations {
        let rows = execute_query(
            &g,
            "MATCH (p:Container)-[:CONTAINS]->(c:Item) RETURN count(c)",
        );
        assert_eq!(rows.len(), 1);
    }
    let elapsed = start.elapsed();

    let ratio = elapsed.as_secs_f64() / baseline_elapsed.as_secs_f64().max(f64::EPSILON);
    assert!(
        ratio < 0.8,
        "COUNT 1-hop pushdown ratio {ratio:.2} (count {elapsed:?}, materialized baseline \
         {baseline_elapsed:?}); aggregate pushdown may be lost"
    );
}

/// Guards the sort-key pre-computation in `apply_order_by` (commit 3a12381):
/// keys are evaluated once per row before sorting, instead of calling
/// `eval_expr` inside the comparator on every comparison.
#[test]
fn order_by_throughput_guard() {
    // 500 nodes with a numeric `score` property.
    let mut g = ermya_graph::Graph::new();
    for i in 0_i64..500 {
        g.add_node("Item", props! { "score" => i }).unwrap();
    }

    let query = "MATCH (n:Item) RETURN n.score ORDER BY n.score DESC";
    let baseline_query = "MATCH (n:Item) RETURN n.score";
    let iterations = 100;
    let baseline_start = Instant::now();
    for _ in 0..iterations {
        let rows = execute_query(&g, baseline_query);
        assert_eq!(rows.len(), 500, "baseline must return all rows");
    }
    let baseline_elapsed = baseline_start.elapsed();

    let start = Instant::now();
    for _ in 0..iterations {
        let rows = execute_query(&g, query);
        assert_eq!(rows.len(), 500, "ORDER BY must return all rows");
    }
    let elapsed = start.elapsed();

    // A same-process unsorted baseline makes this robust against machine load.
    // Sort-key pre-computation adds bounded sorting work; evaluating expressions
    // inside every comparator call makes this ratio grow with O(N log N).
    let ratio = elapsed.as_secs_f64() / baseline_elapsed.as_secs_f64().max(f64::EPSILON);
    assert!(
        ratio < 2.5,
        "ORDER BY took {ratio:.2}x the unsorted baseline (threshold: 2.5x) \
         — sort-key pre-computation may be lost"
    );
}

/// Correctness + scaling anchor for multi-hop chain traversal.
///
/// This was a throughput guard for the double-buffered hop expansion (commit
/// d60d964: reuse one `Vec` across hops instead of allocating a fresh one per
/// hop). It no longer claims to guard that, because it cannot: restoring the
/// per-hop allocation changes nothing measurable. Measured unoptimized, three
/// runs per cell — 100-node chain / 3 hops: 0.310s vs 0.309s. Stretched to a
/// 2 000-node chain and 6 hops, where the allocation happens six times over
/// ~2 000 rows: 15.363s vs 15.345s. Ratio 1.00 in both.
///
/// That is the expected outcome once stated plainly: the optimization saves a
/// handful of allocations per QUERY, while the work per query is proportional
/// to the number of matches. There is no scenario where a few allocations show
/// up against that, so no guard — wall-clock or deterministic — is worth
/// building for it. The optimization is still correct and worth keeping; it
/// simply does not need watching.
///
/// What remains here is a ratio anchor: multi-hop traversal must stay
/// proportionate to a single hop over the same chain, which catches a genuine
/// blow-up (a per-hop O(N) scan, an accidental cross product) without pretending
/// to detect the allocation change.
#[test]
fn multi_hop_throughput_guard() {
    // 100-node chain: n0 -R-> n1 -R-> ... -R-> n99
    let mut g = Graph::new();
    build_chain(&mut g, "N", "R", 100);

    let iterations = 50;

    // Baseline: one hop over the same chain.
    let baseline_query = "MATCH (a:N)-[:R]->(b:N) RETURN id(b)";
    let baseline_start = Instant::now();
    for _ in 0..iterations {
        let rows = execute_query(&g, baseline_query);
        assert!(!rows.is_empty(), "1-hop chain must produce results");
    }
    let baseline_elapsed = baseline_start.elapsed();

    // Subject: three hops. On a chain each hop yields at most one continuation,
    // so the match count barely changes and the cost should stay proportionate.
    let subject_query = "MATCH (a:N)-[:R]->(b:N)-[:R]->(c:N)-[:R]->(d:N) RETURN id(d)";
    let subject_start = Instant::now();
    for _ in 0..iterations {
        let rows = execute_query(&g, subject_query);
        assert!(!rows.is_empty(), "3-hop chain must produce results");
    }
    let subject_elapsed = subject_start.elapsed();

    let ratio = subject_elapsed.as_secs_f64() / baseline_elapsed.as_secs_f64().max(f64::EPSILON);
    assert!(
        ratio < 6.0,
        "3-hop chain traversal is {ratio:.1}x a 1-hop traversal of the same chain \
         (threshold: 6x) — hop expansion may have gone super-linear"
    );
}

// ── Move-based projection throughput ─────────────────────────────────────────

/// Throughput guard for the terminal projection path (no ORDER BY), which
/// moves string properties out of the loaded node instead of cloning them.
///
/// Ratio-based: projecting a 100-char string is compared against projecting a
/// small integer over the same scan, so the measurement isolates the string
/// handling from the scan cost and is immune to host load. The absolute
/// ceiling this replaces (1.0 s against a ~0.4 s typical) failed on a merely
/// busy machine, and the signal behind it was never measured.
#[test]
fn projection_string_throughput_guard() {
    let data: String = "x".repeat(100);
    let mut g = Graph::new();
    for i in 0..1_000_i64 {
        g.add_node("N", props! { "data" => data.as_str(), "n" => i })
            .unwrap();
    }

    let iterations = 50;

    // Baseline: same scan, same row count, projecting a scalar instead.
    let baseline_start = Instant::now();
    for _ in 0..iterations {
        let rows = execute_query(&g, "MATCH (n:N) RETURN n.n");
        assert_eq!(rows.len(), 1_000, "must return all 1 000 nodes");
    }
    let baseline_elapsed = baseline_start.elapsed();

    let subject_start = Instant::now();
    for _ in 0..iterations {
        let rows = execute_query(&g, "MATCH (n:N) RETURN n.data");
        assert_eq!(rows.len(), 1_000, "must return all 1 000 nodes");
    }
    let subject_elapsed = subject_start.elapsed();

    let ratio = subject_elapsed.as_secs_f64() / baseline_elapsed.as_secs_f64().max(f64::EPSILON);
    assert!(
        ratio < 5.0,
        "string projection is {ratio:.1}x slower than scalar projection over the \
         same scan (threshold: 5x) — per-value string cloning may have returned"
    );
}

/// Throughput guard for multi-property index intersection in `narrow_candidates`.
///
/// 10 000 nodes with two indexed properties plus a realistic payload; a
/// 2-property filter whose BROAD predicate comes first (5 000 candidates) and
/// whose selective one comes second (1 match). Intersection must collapse the
/// candidate set to 1 before any node is deserialized.
///
/// The predicate order is the whole point. `narrow_candidates` intersects the
/// per-property index sets left to right, so with the selective predicate
/// FIRST the candidate set is already 1 by the time intersection runs — and
/// intersecting then only ADDS a second index lookup materialising 5 000 ids.
/// Measured in that order, disabling intersection made the query 17x FASTER
/// (0.034s -> 0.002s): the guard was penalising the optimization it claims to
/// protect, and no ceiling could ever have failed for the right reason.
///
/// With the broad predicate first, the same switch costs 0.067s -> 3.17s.
#[test]
fn multi_property_index_throughput_guard() {
    let mut g = Graph::new();
    g.begin_batch();
    for i in 0_i64..10_000 {
        let status = if i % 2 == 0 { "Active" } else { "Inactive" };
        // The payload beyond the two indexed keys is what pruning avoids
        // decoding for the 4 999 non-matching 'Active' nodes.
        g.add_node(
            "Person",
            props! {
                "id" => i,
                "status" => status,
                "bio" => "lorem ipsum dolor sit amet consectetur adipiscing elit sed do eiusmod",
                "email" => format!("person{i}@example.com").as_str(),
                "score" => i * 7,
            },
        )
        .unwrap();
    }
    g.end_batch().unwrap();

    // Broad predicate first (5 000 'Active'), selective second (id=42 → 1).
    let query_str = "MATCH (p:Person {status: 'Active', id: 42}) RETURN id(p)";

    let iterations = 200;
    let baseline_start = Instant::now();
    for _ in 0..iterations {
        let rows = execute_query(&g, "MATCH (p:Person {status: 'Active'}) RETURN id(p)");
        assert_eq!(rows.len(), 5_000);
    }
    let baseline_elapsed = baseline_start.elapsed();

    let start = Instant::now();
    for _ in 0..iterations {
        let rows = execute_query(&g, query_str);
        assert_eq!(rows.len(), 1, "expected exactly 1 match");
    }
    let elapsed = start.elapsed();

    let ratio = elapsed.as_secs_f64() / baseline_elapsed.as_secs_f64().max(f64::EPSILON);
    assert!(
        ratio < 0.5,
        "multi-property index ratio {ratio:.2} (intersection {elapsed:?}, broad baseline \
         {baseline_elapsed:?}); index intersection may be lost"
    );
}

/// Throughput guard verifying that `adj_cache` pre-warming on `Graph::open`
/// eliminates O(N) page scans on cache miss.
///
/// File-backed graph: 200 nodes, 400 edges, reopened. 1 000 calls to
/// `outgoing_edges` must complete in < 0.5 s. Without pre-warming each call
/// would scan all adjacency pages; with pre-warming every lookup is O(1).
#[test]
fn adj_pointer_no_page_scan_guard() {
    let tmp = TempDir::new().unwrap();
    let config = GraphConfig {
        memory_limit_bytes: 4 * 1024 * 1024,
        create_if_missing: true,
        adj_cache_capacity: 1024,
        wal_enabled: false,
        ..GraphConfig::new()
    };

    let mut node_ids: Vec<ermya_graph::NodeId> = Vec::with_capacity(200);

    {
        let mut g = Graph::open(tmp.path(), &config).unwrap();
        g.begin_batch();
        for _ in 0..200 {
            node_ids.push(g.add_node("N", Properties::new()).unwrap());
        }
        // Add 2 outgoing edges per node (400 edges total)
        for i in 0..200_usize {
            let src = node_ids[i];
            let t1 = node_ids[(i + 1) % 200];
            let t2 = node_ids[(i + 2) % 200];
            g.add_edge("R", src, t1, Properties::new()).unwrap();
            g.add_edge("R", src, t2, Properties::new()).unwrap();
        }
        g.end_batch().unwrap();
        g.flush().unwrap();
    }

    // Reopen — adj_cache is pre-warmed by rebuild_adj_cache at open time.
    let g = Graph::open(tmp.path(), &config).unwrap();

    let measure = |ids: &[ermya_graph::NodeId]| {
        let start = Instant::now();
        for _ in 0..50 {
            for &nid in ids {
                let _ = g.outgoing_edges(nid).unwrap();
            }
        }
        start.elapsed()
    };
    let small = measure(&node_ids[..100]);
    let large = measure(&node_ids);
    let ratio = large.as_secs_f64() / small.as_secs_f64().max(f64::EPSILON);

    // Doubling the lookup set should stay close to linear. A page scan on each
    // miss makes the work grow quadratically and pushes this ratio toward 4x.
    assert!(
        ratio < 3.0,
        "adjacency lookup scaling ratio {ratio:.2} (100 nodes {small:?}, 200 nodes {large:?}); \
         adj_cache pre-warming may be broken"
    );
}

// ── 3e/3f C5: scalar-function & list-predicate throughput guards ─────────────

/// A `RETURN coalesce(n.prop, fallback)` projection must not be more than 5x
/// slower than a bare `RETURN n.prop` over the same scan. Ratio-based to avoid
/// flaky absolute thresholds under CI/host load (see the file header pattern).
#[test]
fn scalar_function_projection_throughput_guard() {
    let mut g = Graph::new();
    for i in 0..1000_i64 {
        g.add_node("N", props! { "v" => i }).unwrap();
    }
    let iterations = 50;

    let baseline = "MATCH (n:N) RETURN n.v";
    let baseline_start = Instant::now();
    for _ in 0..iterations {
        let _ = execute_query(&g, baseline);
    }
    let baseline_elapsed = baseline_start.elapsed();

    let subject = "MATCH (n:N) RETURN coalesce(n.missing, toLower('FALLBACK'))";
    let subject_start = Instant::now();
    for _ in 0..iterations {
        let _ = execute_query(&g, subject);
    }
    let subject_elapsed = subject_start.elapsed();

    let ratio = subject_elapsed.as_secs_f64() / baseline_elapsed.as_secs_f64().max(f64::EPSILON);
    assert!(
        ratio < 5.0,
        "scalar-function projection is {ratio:.1}x slower than bare projection (threshold: 5x)"
    );
}

/// An `ALL(x IN list WHERE …)` WHERE filter must not be more than 10x slower
/// than a trivial constant WHERE over the same scan. Exercises the per-element
/// quantifier loop on the hot path.
#[test]
fn list_predicate_where_throughput_guard() {
    let mut g = Graph::new();
    for i in 0..1000_i64 {
        g.add_node("N", props! { "v" => i }).unwrap();
    }
    let iterations = 50;

    let baseline = "MATCH (n:N) WHERE n.v >= 0 RETURN n.v";
    let baseline_start = Instant::now();
    for _ in 0..iterations {
        let _ = execute_query(&g, baseline);
    }
    let baseline_elapsed = baseline_start.elapsed();

    let subject = "MATCH (n:N) WHERE ALL(x IN [1, 2, 3, 4, 5] WHERE x > n.v - 10) RETURN n.v";
    let subject_start = Instant::now();
    for _ in 0..iterations {
        let _ = execute_query(&g, subject);
    }
    let subject_elapsed = subject_start.elapsed();

    let ratio = subject_elapsed.as_secs_f64() / baseline_elapsed.as_secs_f64().max(f64::EPSILON);
    assert!(
        ratio < 10.0,
        "list-predicate WHERE is {ratio:.1}x slower than trivial WHERE (threshold: 10x)"
    );
}
