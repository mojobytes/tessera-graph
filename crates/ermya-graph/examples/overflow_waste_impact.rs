// SPDX-License-Identifier: MIT

//! Property-overflow waste — performance impact harness.
//!
//! Companion to `issue54_thrashing`, which answered the same class of question
//! for adjacency pages. This one targets the property-overflow file, where
//! three separate defects were confirmed by inspection and by an in-process
//! experiment:
//!
//! 1. A node whose encoded properties exceed the inline cap (38 bytes for
//!    nodes, 30 for edges) gets a whole 4096-byte page **to itself** — a
//!    39-byte payload costs 4096 bytes on disk (99.05% waste).
//! 2. Updating such a node writes a **fresh** chain and abandons the old one.
//!    The orphans are never referenced again and never freed.
//! 3. Deleting the node frees nothing either: the page allocator is a
//!    monotonically increasing counter with no free list anywhere in the
//!    storage layer.
//!
//! Disk waste is already established. The open question this harness answers is
//! **what it costs in time**, and the mechanism it tests is the buffer pool: a
//! page that is allocated but dead still occupies a pool frame and still
//! competes for cache residency. If orphaned pages evict live ones, the cost is
//! not merely disk — it is extra disk *reads* on the hot path.
//!
//! # Method
//!
//! Every arm is paired with a control that differs in exactly ONE variable, so
//! a difference can be attributed. Measuring an overflow arm alone would
//! conflate the overflow path with the ordinary cost of writing a bigger value.
//!
//! - `inline` vs `overflow`: identical operation counts, property payload
//!   sized just BELOW vs just ABOVE the inline cap. The payload differs by a
//!   single byte, so any gap is the overflow path itself, not payload size.
//! - `churn`: repeated updates of the SAME node set. Live data is constant by
//!   construction; everything the overflow file gains is orphaned. This is
//!   where defect 2 shows up, and it is the one that worsens over time.
//! - `read_after_churn`: reads over that churned graph, against reads over a
//!   freshly written graph holding the same live data. Same live working set,
//!   different amount of dead weight around it — this isolates whether the
//!   orphans actually hurt the read path or are merely inert.
//!
//! WAL is off in every arm, matching the #51 and #54 benches: fsync would
//! dominate and mask the pool behaviour under test.
//!
//! Run:
//!
//! ```sh
//! cargo run --release --features pool-instrumentation \
//!   --example overflow_waste_impact -p ermya-graph
//! ```

// Diagnostic harness: metric arithmetic tolerates lossy int->float casts.
#![allow(clippy::cast_precision_loss)]

use std::time::Instant;

use ermya_graph::{Graph, GraphConfig, NodeId, Properties, Property};

/// Bytes in a page. Mirrors `storage::page::PAGE_SIZE`, which is not public.
const PAGE_SIZE: u64 = 4096;

/// Node property inline capacity, from `node_codec::NODE_PROP_INLINE_MAX`.
/// A property set encoding to this many bytes or fewer stays in the slot.
const NODE_PROP_INLINE_MAX: usize = 38;

/// Value length whose encoded property set lands just UNDER the inline cap.
///
/// Measured, not derived: the in-process experiment found that a single
/// `name` property tips into overflow at 29 value characters (39 encoded
/// bytes), so 28 is the largest value that still stays inline.
const VALUE_LEN_INLINE: usize = 28;

/// Value length whose encoded property set lands just OVER the inline cap.
/// One byte more than [`VALUE_LEN_INLINE`] — the minimal difference that
/// changes the storage strategy.
const VALUE_LEN_OVERFLOW: usize = 29;

/// Default pool: 64 MB / 16384 pages, matching the #54 harness.
const POOL_DEFAULT: usize = 64 * 1024 * 1024;

/// Constrained pool: 8 MB / 2048 pages.
///
/// Needed because a pool large enough to hold everything cannot show the
/// effect under test. Waste only costs *time* when it pushes the working set
/// past cache capacity; with an oversized pool the orphaned pages are inert
/// and the read arms report a flat 0% miss rate no matter how much dead weight
/// the file carries. The constrained arms are the ones that answer the
/// question — the roomy ones establish the floor.
const POOL_SMALL: usize = 8 * 1024 * 1024;

fn open_graph_with_pool(memory_limit_bytes: usize) -> (Graph, tempfile::TempDir) {
    let dir = tempfile::tempdir_in("/private/tmp").expect("tempdir");
    let config = GraphConfig {
        memory_limit_bytes,
        create_if_missing: true,
        wal_enabled: false,
        ..GraphConfig::default()
    };
    let graph = Graph::open(dir.path(), &config).expect("open");
    (graph, dir)
}

fn open_graph() -> (Graph, tempfile::TempDir) {
    open_graph_with_pool(POOL_DEFAULT)
}

fn props_of_len(len: usize) -> Properties {
    let mut p = Properties::new();
    p.insert("name".into(), Property::String("x".repeat(len)));
    p
}

/// Confirms the two payload sizes really do straddle the inline cap.
///
/// Without this the whole harness could be comparing two inline arms (or two
/// overflow arms) and reporting the difference as noise. The rig is validated
/// before any number it produces is believed.
fn validate_rig() {
    let (mut graph, _dir) = open_graph();

    let inline_id = graph
        .add_node("P", props_of_len(VALUE_LEN_INLINE))
        .expect("add");
    let after_inline = graph.overflow_page_count();
    assert_eq!(
        after_inline, 0,
        "rig invalid: the '{VALUE_LEN_INLINE}-char' arm was supposed to stay inline \
         but allocated {after_inline} overflow page(s); NODE_PROP_INLINE_MAX={NODE_PROP_INLINE_MAX}"
    );

    let _ = graph
        .add_node("P", props_of_len(VALUE_LEN_OVERFLOW))
        .expect("add");
    let after_overflow = graph.overflow_page_count();
    assert_eq!(
        after_overflow, 1,
        "rig invalid: the '{VALUE_LEN_OVERFLOW}-char' arm was supposed to overflow \
         into exactly one page, got {after_overflow}"
    );

    // And the inline node must still read back correctly, so the two arms
    // really are storing equivalent data by different routes.
    let back = graph.node(inline_id).expect("read back");
    assert_eq!(
        back.properties().len(),
        1,
        "rig invalid: inline node lost its property"
    );

    println!(
        "rig validated: {VALUE_LEN_INLINE} chars => inline (0 overflow pages), \
         {VALUE_LEN_OVERFLOW} chars => 1 overflow page\n"
    );
}

struct Arm {
    label: &'static str,
    ops: u64,
    elapsed_s: f64,
    overflow_pages: u32,
    live_payload_bytes: u64,
    hits: u64,
    misses: u64,
    evictions: u64,
}

impl Arm {
    fn us_per_op(&self) -> f64 {
        (self.elapsed_s * 1e6) / (self.ops as f64)
    }

    /// Ratio of overflow bytes on disk to bytes of live property data.
    /// 1.0 would be perfect packing; the defect makes this grow without bound.
    fn amplification(&self) -> f64 {
        if self.live_payload_bytes == 0 {
            return 0.0;
        }
        (u64::from(self.overflow_pages) * PAGE_SIZE) as f64 / self.live_payload_bytes as f64
    }

    fn miss_rate(&self) -> f64 {
        let total = self.hits + self.misses;
        if total == 0 {
            return 0.0;
        }
        (self.misses as f64 / total as f64) * 100.0
    }
}

fn finish(
    graph: &Graph,
    label: &'static str,
    start: Instant,
    ops: u64,
    live_payload_bytes: u64,
) -> Arm {
    let elapsed_s = start.elapsed().as_secs_f64();
    let (hits, misses, evictions) = graph.pool_instrumentation();
    Arm {
        label,
        ops,
        elapsed_s,
        overflow_pages: graph.overflow_page_count(),
        live_payload_bytes,
        hits,
        misses,
        evictions,
    }
}

// ---------------------------------------------------------------------------
// Arm 1 — write cost: inline vs overflow, one byte apart
// ---------------------------------------------------------------------------

fn run_write(label: &'static str, n: u64, value_len: usize) -> Arm {
    let (mut graph, _dir) = open_graph();
    graph.reset_pool_instrumentation();
    let start = Instant::now();
    for _ in 0..n {
        graph
            .add_node("P", props_of_len(value_len))
            .expect("add_node");
    }
    // Live payload: one encoded property set per node. The value plus its key
    // and framing; using the value length alone would understate it, so the
    // measured encoded size is used instead.
    let per_node = encoded_len(value_len);
    finish(&graph, label, start, n, n * per_node)
}

/// Encoded size of one property set of the given value length.
///
/// Measured by running the real codec rather than assuming a formula: the
/// amplification figures are only meaningful if the denominator is the number
/// of bytes the engine actually stores, framing included.
fn encoded_len(value_len: usize) -> u64 {
    let encoded =
        ermya_graph::storage::codec::property_codec::encode_properties(&props_of_len(value_len))
            .expect("encoding a single small property cannot fail");
    encoded.len() as u64
}

// ---------------------------------------------------------------------------
// Arm 2 — churn: repeated updates of a FIXED node set
// ---------------------------------------------------------------------------

/// Updates the same `n` nodes `rounds` times. Live data never grows; every
/// overflow page gained after the first round is an orphan.
fn run_churn(
    label: &'static str,
    n: u64,
    rounds: u64,
    value_len: usize,
) -> (Arm, Vec<NodeId>, Graph, tempfile::TempDir) {
    run_churn_with_pool(label, n, rounds, value_len, POOL_DEFAULT)
}

fn run_churn_with_pool(
    label: &'static str,
    n: u64,
    rounds: u64,
    value_len: usize,
    pool_bytes: usize,
) -> (Arm, Vec<NodeId>, Graph, tempfile::TempDir) {
    let (mut graph, dir) = open_graph_with_pool(pool_bytes);
    let ids: Vec<NodeId> = (0..n)
        .map(|_| {
            graph
                .add_node("P", props_of_len(value_len))
                .expect("add_node")
        })
        .collect();

    graph.reset_pool_instrumentation();
    let start = Instant::now();
    for r in 0..rounds {
        for &id in &ids {
            let mut node = graph.node(id).expect("read");
            // Same length every round, so the live payload is constant and the
            // only thing that can grow is waste.
            let v = format!("{}{:02}", "y".repeat(value_len.saturating_sub(2)), r % 100);
            node.properties_mut()
                .insert("name".into(), Property::String(v));
            graph.update_node(id, &node).expect("update");
        }
    }
    let per_node = encoded_len(value_len);
    let arm = finish(&graph, label, start, n * rounds, n * per_node);
    (arm, ids, graph, dir)
}

// ---------------------------------------------------------------------------
// Arm 3 — read cost over a churned graph vs a fresh one with the same live data
// ---------------------------------------------------------------------------

fn run_reads(graph: &Graph, label: &'static str, ids: &[NodeId], passes: u64) -> Arm {
    graph.reset_pool_instrumentation();
    let start = Instant::now();
    let mut checksum = 0_u64;
    for _ in 0..passes {
        for &id in ids {
            let node = graph.node(id).expect("read");
            // Touch the property so the overflow chain is actually resolved;
            // a read that never looks at the value could skip the chain and
            // measure nothing.
            checksum += node.properties().len() as u64;
        }
    }
    assert!(checksum > 0, "reads must observe properties");
    let per_node = encoded_len(VALUE_LEN_OVERFLOW);
    finish(
        graph,
        label,
        start,
        passes * ids.len() as u64,
        ids.len() as u64 * per_node,
    )
}

/// A graph holding the same live data as the churned one, written once.
fn run_fresh_equivalent(n: u64, value_len: usize) -> (Vec<NodeId>, Graph, tempfile::TempDir) {
    run_fresh_equivalent_with_pool(n, value_len, POOL_DEFAULT)
}

fn run_fresh_equivalent_with_pool(
    n: u64,
    value_len: usize,
    pool_bytes: usize,
) -> (Vec<NodeId>, Graph, tempfile::TempDir) {
    let (mut graph, dir) = open_graph_with_pool(pool_bytes);
    let ids: Vec<NodeId> = (0..n)
        .map(|_| {
            graph
                .add_node("P", props_of_len(value_len))
                .expect("add_node")
        })
        .collect();
    (ids, graph, dir)
}

// ---------------------------------------------------------------------------

fn print_header(title: &str) {
    println!("\n{title}");
    println!("{}", "-".repeat(118));
    println!(
        "{:>22} | {:>9} | {:>9} | {:>10} | {:>9} | {:>7} | {:>9} | {:>9}",
        "arm", "ops", "us/op", "ovf_pages", "ovf_MB", "amp", "miss%", "evictions"
    );
}

fn print_arm(a: &Arm) {
    println!(
        "{:>22} | {:>9} | {:>9.2} | {:>10} | {:>9.2} | {:>6.1}x | {:>8.2}% | {:>9}",
        a.label,
        a.ops,
        a.us_per_op(),
        a.overflow_pages,
        (u64::from(a.overflow_pages) * PAGE_SIZE) as f64 / (1024.0 * 1024.0),
        a.amplification(),
        a.miss_rate(),
        a.evictions
    );
}

fn main() {
    println!(
        "Property-overflow waste — performance impact (WAL off; pool 64 MB for arms 1-3, \
         8 MB for arm 4)"
    );
    println!(
        "Inline cap: {NODE_PROP_INLINE_MAX} bytes. Arms straddle it by ONE byte \
         ({VALUE_LEN_INLINE} vs {VALUE_LEN_OVERFLOW} chars).\n"
    );

    validate_rig();

    // --- Arm 1: write cost, inline vs overflow -----------------------------
    print_header("Arm 1 — write path: identical work, one byte either side of the inline cap");
    for n in [10_000_u64, 50_000] {
        print_arm(&run_write("inline (28 chars)", n, VALUE_LEN_INLINE));
        print_arm(&run_write("overflow (29 chars)", n, VALUE_LEN_OVERFLOW));
        println!();
    }

    // --- Arm 2: churn ------------------------------------------------------
    print_header("Arm 2 — update churn: SAME nodes updated repeatedly, live data constant");
    let n_churn = 2_000_u64;
    for rounds in [1_u64, 5, 20] {
        let label: &'static str = match rounds {
            1 => "churn x1 (inline)",
            5 => "churn x5 (inline)",
            _ => "churn x20 (inline)",
        };
        let (arm, _, _g, _d) = run_churn(label, n_churn, rounds, VALUE_LEN_INLINE);
        print_arm(&arm);
    }
    println!();
    for rounds in [1_u64, 5, 20] {
        let label: &'static str = match rounds {
            1 => "churn x1 (overflow)",
            5 => "churn x5 (overflow)",
            _ => "churn x20 (overflow)",
        };
        let (arm, _, _g, _d) = run_churn(label, n_churn, rounds, VALUE_LEN_OVERFLOW);
        print_arm(&arm);
    }

    // --- Arm 3: reads over churned vs fresh --------------------------------
    print_header("Arm 3 — read path: same live data, with vs without orphaned pages around it");
    let n_read = 2_000_u64;
    let passes = 5_u64;

    let (churned_arm, churned_ids, churned_graph, _cd) =
        run_churn("(setup) churn x20", n_read, 20, VALUE_LEN_OVERFLOW);
    let (fresh_ids, fresh_graph, _fd) = run_fresh_equivalent(n_read, VALUE_LEN_OVERFLOW);

    print_arm(&run_reads(
        &fresh_graph,
        "read / fresh graph",
        &fresh_ids,
        passes,
    ));
    print_arm(&run_reads(
        &churned_graph,
        "read / churned graph",
        &churned_ids,
        passes,
    ));
    println!(
        "\n  (churned graph carries {} overflow pages = {:.2} MB for the same live data \
         as the fresh one)",
        churned_arm.overflow_pages,
        (u64::from(churned_arm.overflow_pages) * PAGE_SIZE) as f64 / (1024.0 * 1024.0)
    );

    // --- Arm 4: the same comparison under memory pressure ------------------
    //
    // Arm 3 runs with a pool that comfortably holds everything, so it reports
    // the floor: with unlimited memory the orphans are inert. That is a real
    // result but not the interesting one. Here the pool is deliberately too
    // small for the churned file, which is the condition under which waste
    // stops being a disk-space matter and starts costing read latency.
    print_header(
        "Arm 4 — read path under memory pressure (pool = 8 MB, smaller than the churned file)",
    );
    let (churned_arm_s, churned_ids_s, churned_graph_s, _cds) = run_churn_with_pool(
        "(setup) churn x20",
        n_read,
        20,
        VALUE_LEN_OVERFLOW,
        POOL_SMALL,
    );
    let (fresh_ids_s, fresh_graph_s, _fds) =
        run_fresh_equivalent_with_pool(n_read, VALUE_LEN_OVERFLOW, POOL_SMALL);

    print_arm(&run_reads(
        &fresh_graph_s,
        "read / fresh (8MB)",
        &fresh_ids_s,
        passes,
    ));
    print_arm(&run_reads(
        &churned_graph_s,
        "read / churned (8MB)",
        &churned_ids_s,
        passes,
    ));
    println!(
        "\n  (churned graph carries {:.2} MB of overflow against an {:.0} MB pool)",
        (u64::from(churned_arm_s.overflow_pages) * PAGE_SIZE) as f64 / (1024.0 * 1024.0),
        POOL_SMALL as f64 / (1024.0 * 1024.0)
    );

    println!(
        "\nReading the table: 'amp' is overflow bytes on disk per byte of live property data \
         (1.0x = perfect packing). 'miss%' is buffer-pool read misses."
    );
}
