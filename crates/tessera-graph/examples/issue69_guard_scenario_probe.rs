// SPDX-License-Identifier: MIT

//! Issue #69 probe: does each perf guard's scenario actually exercise the
//! optimization it claims to protect?
//!
//! A guard is only a guard if disabling its optimization makes it fail. This
//! probe measures each scenario twice — optimization on, optimization off —
//! and reports the ratio. A ratio near 1.0 means the guard is decorative: it
//! cannot fail for the reason it exists, no matter how the ceiling is tuned.
//!
//! For guards whose scenario is suspected of being under-built, a corrected
//! variant is measured alongside the current one, to separate "the engine's
//! optimization is worth little" from "the scenario doesn't let it show".
//!
//! Measuring an arm requires temporarily gating the optimization at its source
//! behind an env var, then reverting. The three used for issue #69 were:
//!
//! - `TESSERA_DISABLE_PUSHDOWN` — aggregate pushdown, in `gql::compiler`
//! - `TESSERA_DISABLE_INTERSECTION` — multi-index intersection, in `query::pattern`
//! - `TESSERA_DISABLE_LABEL_FASTPATH` — label-only fast path, in `query::pattern`
//!
//! Run the baseline, then one arm per optimization:
//!
//! ```text
//! cargo run --example issue69_guard_scenario_probe -p tessera-graph
//! TESSERA_DISABLE_PUSHDOWN=1 cargo run --example issue69_guard_scenario_probe -p tessera-graph
//! ```
//!
//! Build it the way the guards are built. `cargo test` uses the dev profile, so
//! a `--release` probe reports timings ~4x faster than the ceilings must allow.

// A measurement probe: casts on counts are intentional and lossless here.
#![allow(clippy::cast_precision_loss)]

use std::time::{Duration, Instant};

use tessera_graph::{Graph, Properties, props};

/// Realistic node payload: the kind of data a real node carries. Decoding it
/// costs something, which is exactly what the guarded optimizations avoid.
fn realistic_props(i: i64) -> Properties {
    props! {
        "name" => format!("item-{i}").as_str(),
        "sku" => format!("SKU-{i:08}").as_str(),
        "qty" => i,
        "notes" => "lorem ipsum dolor sit amet consectetur adipiscing elit sed do",
    }
}

/// Heavier payload: more keys and longer strings, so the string-heap and
/// overflow resolution that the label check skips costs more.
fn heavy_props(i: i64) -> Properties {
    let filler = "lorem ipsum dolor sit amet consectetur adipiscing elit sed do eiusmod tempor incididunt ut labore et dolore magna aliqua";
    props! {
        "name" => format!("item-{i}").as_str(),
        "sku" => format!("SKU-{i:08}").as_str(),
        "qty" => i,
        "notes" => filler,
        "description" => filler,
        "summary" => filler,
        "tags" => format!("{filler}-{i}").as_str(),
    }
}

fn execute(graph: &Graph, q: &str) -> usize {
    let query = tessera_graph::gql::parse(q).unwrap();
    tessera_graph::gql::execute(graph, &query, 0).unwrap().len()
}

/// Times `iterations` passes of `query`, after one untimed warm-up pass so both
/// arms reach steady state alike.
fn time_query(g: &Graph, query: &str, iterations: usize, expect_rows: usize) -> Duration {
    assert_eq!(execute(g, query), expect_rows);
    let start = Instant::now();
    for _ in 0..iterations {
        assert_eq!(execute(g, query), expect_rows);
    }
    start.elapsed()
}

/// Runs a cell three times, reports min/med/max so one unlucky pass cannot
/// masquerade as a result.
fn repeat<F: FnMut() -> Duration>(label: &str, mut f: F) {
    let mut t: Vec<f64> = (0..3).map(|_| f().as_secs_f64()).collect();
    t.sort_by(f64::total_cmp);
    println!(
        "  {label:<38} min {:.3}s  med {:.3}s  max {:.3}s",
        t[0],
        t[t.len() / 2],
        t[t.len() - 1]
    );
}

// ── Guard 1: COUNT 1-hop pushdown ───────────────────────────────────────────

fn build_count(g: &mut Graph, with_props: bool, batched: bool) {
    if batched {
        g.begin_batch();
    }
    let mut i: i64 = 0;
    for _ in 0..1_000 {
        let container = g.add_node("Container", Properties::new()).unwrap();
        for _ in 0..5 {
            let p = if with_props {
                realistic_props(i)
            } else {
                Properties::new()
            };
            let item = g.add_node("Item", p).unwrap();
            g.add_edge("CONTAINS", container, item, Properties::new())
                .unwrap();
            i += 1;
        }
    }
    if batched {
        g.end_batch().unwrap();
    }
}

// ── Guard 2: multi-property index intersection ──────────────────────────────

fn build_index(g: &mut Graph, with_props: bool) {
    g.begin_batch();
    for i in 0_i64..10_000 {
        let status = if i % 2 == 0 { "Active" } else { "Inactive" };
        let p = if with_props {
            props! {
                "id" => i,
                "status" => status,
                "bio" => "lorem ipsum dolor sit amet consectetur adipiscing elit sed do eiusmod",
                "email" => format!("person{i}@example.com").as_str(),
                "score" => i * 7,
            }
        } else {
            props! { "id" => i, "status" => status }
        };
        g.add_node("Person", p).unwrap();
    }
    g.end_batch().unwrap();
}

// ── Guard 3: label-only fast path (hub → 999 fillers + 1 target) ────────────

fn build_label_filter(g: &mut Graph, fat_fillers: bool) {
    let hub = g.add_node("Hub", Properties::new()).unwrap();
    g.begin_batch();
    for i in 0..999_i64 {
        // The guard as written gives fillers two small properties. `fat_fillers`
        // makes them realistic, which is what the fast path avoids decoding.
        let p = if fat_fillers {
            realistic_props(i)
        } else {
            props! { "idx" => i, "data" => "padding" }
        };
        let t = g.add_node("Filler", p).unwrap();
        g.add_edge("LINK", hub, t, Properties::new()).unwrap();
    }
    let target = g
        .add_node("Target", props! { "idx" => 999_i64, "data" => "special" })
        .unwrap();
    g.add_edge("LINK", hub, target, Properties::new()).unwrap();
    g.end_batch().unwrap();
}

// ── Guard 4: expand-hop clone (100 sources x 20 targets) ────────────────────

fn build_expand(g: &mut Graph, with_props: bool) {
    g.begin_batch();
    let mut i: i64 = 0;
    for _ in 0..100 {
        let src = g.add_node("S", Properties::new()).unwrap();
        for _ in 0..20 {
            let p = if with_props {
                realistic_props(i)
            } else {
                Properties::new()
            };
            let tgt = g.add_node("T", p).unwrap();
            g.add_edge("KNOWS", src, tgt, Properties::new()).unwrap();
            i += 1;
        }
    }
    g.end_batch().unwrap();
}

const COUNT_Q: &str = "MATCH (p:Container)-[:CONTAINS]->(c:Item) RETURN count(c)";

/// The selective predicate (`id`, 1 match) comes FIRST, so the candidate set is
/// already 1 before intersection runs and intersecting only ADDS a second index
/// lookup materialising 5 000 ids.
const IDX_SEL_FIRST: &str = "MATCH (p:Person {id: 42, status: 'Active'}) RETURN id(p)";

/// Broad predicate first — the case the guard's comment actually describes.
const IDX_SEL_LAST: &str = "MATCH (p:Person {status: 'Active', id: 42}) RETURN id(p)";

/// Chain-graph patterns of growing hop count: the bindings map grows with the
/// number of bound variables, which is where `Arc` sharing could pay off.
const CHAIN_HOPS: [(&str, &str, usize); 3] = [
    (
        "2 hops (3 vars bound)",
        "MATCH (a:N)-[:R]->(b:N)-[:R]->(c:N) RETURN id(c)",
        398,
    ),
    (
        "4 hops (5 vars bound)",
        "MATCH (a:N)-[:R]->(b:N)-[:R]->(c:N)-[:R]->(d:N)-[:R]->(e:N) RETURN id(e)",
        396,
    ),
    (
        "6 hops (7 vars bound)",
        "MATCH (a:N)-[:R]->(b:N)-[:R]->(c:N)-[:R]->(d:N)-[:R]->(e:N)-[:R]->(f:N)-[:R]->(g:N) RETURN id(g)",
        394,
    ),
];

const ALL_ARMS: [&str; 7] = [
    "TESSERA_DISABLE_PUSHDOWN",
    "TESSERA_DISABLE_INTERSECTION",
    "TESSERA_DISABLE_LABEL_FASTPATH",
    "TESSERA_DISABLE_ARC_BINDINGS",
    "TESSERA_DISABLE_SORTKEY_PRECOMPUTE",
    "TESSERA_DISABLE_DOUBLE_BUFFER",
    "TESSERA_DISABLE_EDGE_LABEL_FILTER",
];

fn main() {
    let arms: Vec<&str> = ALL_ARMS
        .into_iter()
        .filter(|v| std::env::var_os(v).is_some())
        .collect();
    println!(
        "\n=== issue #69 probe — disabled: {} ===",
        if arms.is_empty() {
            "nothing (baseline)".to_owned()
        } else {
            arms.join(", ")
        }
    );

    println!("\nCOUNT 1-hop pushdown guard (50 iterations):");
    // The guard as written builds WITHOUT a batch. Batching changes adjacency
    // layout (edges per node land contiguous), so both are measured: the ratio
    // that matters is the one for the layout the guard actually uses.
    repeat("no batch, empty Items (as the guard is)", || {
        let mut g = Graph::new();
        build_count(&mut g, false, false);
        time_query(&g, COUNT_Q, 50, 1)
    });
    repeat("no batch, Items carry properties", || {
        let mut g = Graph::new();
        build_count(&mut g, true, false);
        time_query(&g, COUNT_Q, 50, 1)
    });
    repeat("batched, empty Items", || {
        let mut g = Graph::new();
        build_count(&mut g, false, true);
        time_query(&g, COUNT_Q, 50, 1)
    });
    repeat("batched, Items carry properties", || {
        let mut g = Graph::new();
        build_count(&mut g, true, true);
        time_query(&g, COUNT_Q, 50, 1)
    });

    println!("\nMulti-property index guard (200 iterations):");
    // The selective property (`id`, 1 match) comes FIRST in this query, so the
    // candidate set is already 1 before intersection runs. Intersecting then
    // only ADDS work: a second index lookup materialising 5 000 ids for
    // `status`. The reversed order is measured too, since that is the case the
    // guard's comment actually describes.
    repeat("selective first (as the guard is)", || {
        let mut g = Graph::new();
        build_index(&mut g, false);
        time_query(&g, IDX_SEL_FIRST, 200, 1)
    });
    repeat("selective last (broad prop first)", || {
        let mut g = Graph::new();
        build_index(&mut g, false);
        time_query(&g, IDX_SEL_LAST, 200, 1)
    });
    repeat("selective last, realistic payload", || {
        let mut g = Graph::new();
        build_index(&mut g, true);
        time_query(&g, IDX_SEL_LAST, 200, 1)
    });

    println!("\nLabel-only fast-path guard (200 iterations):");
    repeat("current scenario (thin fillers)", || {
        let mut g = Graph::new();
        build_label_filter(&mut g, false);
        time_query(&g, "MATCH (h:Hub)-[:LINK]->(t:Target) RETURN id(t)", 200, 1)
    });
    repeat("corrected (fat fillers)", || {
        let mut g = Graph::new();
        build_label_filter(&mut g, true);
        time_query(&g, "MATCH (h:Hub)-[:LINK]->(t:Target) RETURN id(t)", 200, 1)
    });

    probe_expand_hop();
    probe_previously_unverified();
    println!();
}

/// Deterministic check, no timing: does sharing the bindings map with `Arc`
/// actually avoid the deep copy in `expand_hop`'s shape?
///
/// It does not, and that is the point. `Arc::make_mut` copies whenever another
/// reference is alive, and in `expand_hop` the source bindings outlive the
/// neighbour loop, so every iteration copies. This is why the guard's signal is
/// flat at 1.2-1.3x regardless of how many variables are bound: the real saving
/// lives in the materialized iterator (`Arc::try_unwrap` moving the map out
/// instead of copying it), which is per RESULT ROW, not per bound variable.
fn probe_arc_copy_semantics() {
    use std::collections::HashMap;
    use std::sync::Arc;

    const NEIGHBOURS: i64 = 20;
    let prev: Arc<HashMap<String, i64>> =
        Arc::new((0..5).map(|i| (format!("var{i}"), i)).collect());

    let mut copies = 0;
    for n in 0..NEIGHBOURS {
        let mut b = Arc::clone(&prev);
        let before = Arc::as_ptr(&b);
        Arc::make_mut(&mut b).insert(format!("n{n}"), n);
        if before != Arc::as_ptr(&b) {
            copies += 1;
        }
    }

    println!("\nArc bindings — deterministic copy check (no timing):");
    println!("  deep copies while the source is alive   {copies}/{NEIGHBOURS}");
    println!("  → sharing avoids no copy in this shape; the saving is in the iterator");
}

/// Expand-hop clone guard: does the Arc-sharing of the bindings map show
/// up at all, and does it scale with the number of bound variables?
fn probe_expand_hop() {
    probe_arc_copy_semantics();

    println!("\nExpand-hop clone guard (50 iterations):");
    // The Arc-bindings optimization (f34bc51) avoids deep-copying the bindings
    // map on every produced match. Its saving scales with HOW MANY variables are
    // already bound when the hop expands. The guard uses a 1-hop pattern, so the
    // map holds a single entry — copying one entry costs about the same as
    // sharing it. The multi-hop variants below are where the map actually grows.
    repeat("1 hop (as the guard is)", || {
        let mut g = Graph::new();
        build_expand(&mut g, false);
        time_query(&g, "MATCH (a:S)-[:KNOWS]->(b:T) RETURN id(b)", 50, 2000)
    });
    repeat("1 hop, targets carry properties", || {
        let mut g = Graph::new();
        build_expand(&mut g, true);
        time_query(&g, "MATCH (a:S)-[:KNOWS]->(b:T) RETURN id(b)", 50, 2000)
    });

    // Chain graph: every node has one outgoing R, so an n-hop pattern yields a
    // predictable number of matches while the bindings map grows with n.
    println!("\nExpand-hop clone, multi-hop patterns (50 iterations, 400-node chain):");
    for (label, query, rows) in CHAIN_HOPS {
        repeat(label, || {
            let mut g = Graph::new();
            g.begin_batch();
            let mut prev = g.add_node("N", realistic_props(0)).unwrap();
            for i in 1..400_i64 {
                let next = g.add_node("N", realistic_props(i)).unwrap();
                g.add_edge("R", prev, next, Properties::new()).unwrap();
                prev = next;
            }
            g.end_batch().unwrap();
            time_query(&g, query, 50, rows)
        });
    }

    // ── The three guards never verified (issue #69 follow-up) ───────────────
}

/// The three guards that had never been checked against their own
/// optimization (issue #69 follow-up).
fn probe_previously_unverified() {
    println!("\nORDER BY guard — sort-key pre-computation (100 iterations):");
    // Pre-computing the sort keys turns O(N log N) expression evaluations inside
    // the comparator into O(N) before the sort. The saving scales with N log N /
    // N, so a bigger row count separates the two more.
    for (label, n) in [
        ("500 rows (as the guard is)", 500_i64),
        ("5 000 rows", 5_000),
    ] {
        repeat(label, || {
            let mut g = Graph::new();
            g.begin_batch();
            for i in 0..n {
                g.add_node("Item", props! { "score" => i }).unwrap();
            }
            g.end_batch().unwrap();
            #[allow(clippy::cast_sign_loss)]
            // Probe: `n` is a literal row count from the table above.
            #[allow(clippy::cast_possible_truncation)]
            let rows = n as usize;
            time_query(
                &g,
                "MATCH (n:Item) RETURN n.score ORDER BY n.score DESC",
                100,
                rows,
            )
        });
    }

    println!("\nMulti-hop guard — double-buffered hop expansion (50 iterations):");
    // Reusing one buffer per hop instead of allocating a fresh Vec. The saving is
    // one allocation per hop, so it can only show with many hops over many rows.
    for (label, chain, query, rows) in [
        (
            "100-node chain, 3 hops (as the guard is)",
            100_i64,
            "MATCH (a:N)-[:R]->(b:N)-[:R]->(c:N)-[:R]->(d:N) RETURN id(d)",
            97_usize,
        ),
        (
            "2 000-node chain, 6 hops",
            2_000,
            "MATCH (a:N)-[:R]->(b:N)-[:R]->(c:N)-[:R]->(d:N)-[:R]->(e:N)-[:R]->(f:N)-[:R]->(g:N) RETURN id(g)",
            1_994,
        ),
    ] {
        repeat(label, || {
            let mut g = Graph::new();
            g.begin_batch();
            let mut prev = g.add_node("N", Properties::new()).unwrap();
            for _ in 1..chain {
                let next = g.add_node("N", Properties::new()).unwrap();
                g.add_edge("R", prev, next, Properties::new()).unwrap();
                prev = next;
            }
            g.end_batch().unwrap();
            time_query(&g, query, 50, rows)
        });
    }

    println!("\nNeighbor label-filter guard — edge label hash check (200 iterations):");
    // Checking the edge's label hash before deserializing skips string-heap and
    // property-overflow resolution for non-matching edges. Edge PROPERTIES are
    // what makes that skip worth anything, so the guard's property-less edges
    // are the suspect part.
    for (label, edge_props) in [
        ("bare edges (as the guard is)", 0_usize),
        ("edges carry properties", 1),
        ("edges carry heavy properties", 2),
    ] {
        repeat(label, || {
            let mut g = Graph::new();
            let hub = g.add_node("Hub", Properties::new()).unwrap();
            g.begin_batch();
            for i in 0..500_i64 {
                let p = match edge_props {
                    0 => Properties::new(),
                    1 => realistic_props(i),
                    _ => heavy_props(i),
                };
                let t = g.add_node("K", Properties::new()).unwrap();
                g.add_edge("KNOWS", hub, t, p).unwrap();
            }
            for i in 0..500_i64 {
                let p = match edge_props {
                    0 => Properties::new(),
                    1 => realistic_props(i),
                    _ => heavy_props(i),
                };
                let t = g.add_node("L", Properties::new()).unwrap();
                g.add_edge("LIKES", hub, t, p).unwrap();
            }
            g.end_batch().unwrap();

            let start = Instant::now();
            for _ in 0..200 {
                let edges = g
                    .neighbors(hub)
                    .direction(tessera_graph::Direction::Outgoing)
                    .label("KNOWS")
                    .collect()
                    .unwrap();
                assert_eq!(edges.len(), 500);
            }
            start.elapsed()
        });
    }
}
