// SPDX-License-Identifier: MIT

//! Issue #54 verification harness — adjacency density by graph shape.
//!
//! Confirms whether the adjacency-page waste (issue #54 thrashing root cause)
//! depends on the *graph type* or only on *node degree*. The format is the same
//! for every graph; what differs between graphs is their shape — how many edges
//! per source node, and in which direction.
//!
//! Measured finding: a node's adjacency chain DOES pack (~508 edges/page); the
//! waste comes from **degree-1 nodes**, each materialising a whole 4096-byte
//! adjacency page for its single edge. A file-level density near 1 edge/page is
//! therefore an average dominated by many low-degree nodes, not a packing bug.
//! Independent of graph type — driven purely by the fraction of low-degree
//! nodes.
//!
//! Three shapes, mirroring ermya's two graphs plus a dense contrast:
//!
//! - `audit`: N event nodes, each with ONE outgoing edge to a shared principal
//!   (the reporter's VLS insert). Every source node is degree-1 → worst case.
//! - `perms`: ermya's authz perf test — 3 subjects granted over N resource
//!   nodes with a homogeneous spread (~N/3 out-edges per subject). The subject
//!   chains pack densely, but the N degree-1 resource nodes each waste a page.
//! - `dense`: a single source node with all N edges — one packed chain, the
//!   best case for the source chain (the N degree-1 targets still waste pages).
//!
//! Reports adjacency pages, total edges, and edges-per-adjacency-page (density).
//! A full single page holds 508 edges; anything far below that is wasted space.
//!
//! Run:
//!
//! ```sh
//! cargo run --release --features pool-instrumentation \
//!   --example issue54_thrashing -p ermya-graph
//! ```
//!
//! No WAL (isolates buffer-pool behaviour from fsync, same as the #51 benches).

// Diagnostic harness: metric arithmetic tolerates lossy int->float casts.
#![allow(clippy::cast_precision_loss)]

use std::time::Instant;

use ermya_graph::{Graph, GraphConfig, Properties};

/// Max edges a full single adjacency page holds (from `adjacency_codec.rs`).
const EDGES_PER_FULL_PAGE: f64 = 508.0;

fn open_graph() -> (Graph, tempfile::TempDir) {
    let dir = tempfile::tempdir_in("/private/tmp").expect("tempdir");
    let config = GraphConfig {
        memory_limit_bytes: 64 * 1024 * 1024, // default: 16384 pages
        create_if_missing: true,
        wal_enabled: false,
        ..GraphConfig::default()
    };
    let graph = Graph::open(dir.path(), &config).expect("open");
    (graph, dir)
}

struct ShapeResult {
    edges: u64,
    us_per_edge: f64,
    adj_pages: u32,
    total_pages: u64,
    evictions: u64,
    edges_per_adj_page: f64,
}

/// `audit`: N events, each degree-1 outgoing to one of a few shared principals.
fn run_audit(n_events: u64) -> ShapeResult {
    let (mut graph, _dir) = open_graph();
    let principals: Vec<_> = (0..3)
        .map(|p| {
            graph
                .add_node(format!("Principal{p}"), Properties::new())
                .expect("add")
        })
        .collect();
    graph.reset_pool_instrumentation();
    let start = Instant::now();
    for i in 0..n_events {
        let event = graph
            .add_node(format!("Event{}", i % 8), Properties::new())
            .expect("add_node");
        let target = principals[(i % 3) as usize];
        graph
            .add_edge("ACCESS", event, target, Properties::new())
            .expect("add_edge");
    }
    finish(&graph, start, n_events, n_events)
}

/// `perms`: `subjects` principal nodes, each with `n_edges / subjects` outgoing
/// grant edges to distinct resource nodes. High out-degree per source.
///
/// Mirrors ermya's authz perf test: 3 subjects granted over 100k vectors with
/// a homogeneous `i % subjects` distribution → ~33k out-edges per subject. This
/// is the deliberate worst case that put the system against the ropes.
fn run_perms(n_edges: u64) -> ShapeResult {
    let (mut graph, _dir) = open_graph();
    let subjects = 3_u64; // ermya authz test: 3 subjects, homogeneous spread
    let subject_ids: Vec<_> = (0..subjects)
        .map(|s| {
            graph
                .add_node(format!("Subject{s}"), Properties::new())
                .expect("add")
        })
        .collect();
    graph.reset_pool_instrumentation();
    let start = Instant::now();
    for i in 0..n_edges {
        let resource = graph
            .add_node("Resource", Properties::new())
            .expect("add_node");
        let subject = subject_ids[(i % subjects) as usize];
        graph
            .add_edge("GRANT", subject, resource, Properties::new())
            .expect("add_edge");
    }
    finish(&graph, start, n_edges, n_edges)
}

/// `dense`: one source node with all N outgoing edges — one packed chain.
fn run_dense(n_edges: u64) -> ShapeResult {
    let (mut graph, _dir) = open_graph();
    let hub = graph.add_node("Hub", Properties::new()).expect("add");
    graph.reset_pool_instrumentation();
    let start = Instant::now();
    for _ in 0..n_edges {
        let leaf = graph.add_node("Leaf", Properties::new()).expect("add_node");
        graph
            .add_edge("LINK", hub, leaf, Properties::new())
            .expect("add_edge");
    }
    finish(&graph, start, n_edges, n_edges)
}

fn finish(graph: &Graph, start: Instant, ops: u64, edges: u64) -> ShapeResult {
    let elapsed = start.elapsed().as_secs_f64();
    let (_hits, _misses, evictions) = graph.pool_instrumentation();
    let (p_nodes, p_edges, p_adj, p_strings) = graph.data_file_page_counts();
    let total_pages =
        u64::from(p_nodes) + u64::from(p_edges) + u64::from(p_adj) + u64::from(p_strings);
    let edges_per_adj_page = if p_adj == 0 {
        0.0
    } else {
        edges as f64 / f64::from(p_adj)
    };
    ShapeResult {
        edges,
        us_per_edge: (elapsed * 1e6) / (ops as f64),
        adj_pages: p_adj,
        total_pages,
        evictions,
        edges_per_adj_page,
    }
}

fn print_row(shape: &str, n: u64, r: &ShapeResult) {
    let fill_pct = (r.edges_per_adj_page / EDGES_PER_FULL_PAGE) * 100.0;
    println!(
        "{shape:>7} | {n:>9} | {:>10} | {:>9} | {:>8} | {:>8.2} | {:>5.1}% | {:>9.2}   (evict={})",
        r.edges,
        r.adj_pages,
        r.total_pages,
        r.edges_per_adj_page,
        fill_pct,
        r.us_per_edge,
        r.evictions
    );
}

fn main() {
    println!("Issue #54 — adjacency density by graph shape (WAL off, pool=16384 pages)");
    println!("Full single adjacency page holds {EDGES_PER_FULL_PAGE:.0} edges.\n");
    println!(
        "{:>7} | {:>9} | {:>10} | {:>9} | {:>7} | {:>8} | {:>6} | {:>9}",
        "shape", "n", "edges", "adj_pages", "total_pg", "edg/page", "fill", "us/edge"
    );
    println!("{}", "-".repeat(92));

    for n in [20_000_u64, 100_000] {
        print_row("audit", n, &run_audit(n));
        print_row("perms", n, &run_perms(n));
        print_row("dense", n, &run_dense(n));
        println!();
    }
}
