// Copyright (c) 2026 BelowZero Security OU. All rights reserved.
// SPDX-License-Identifier: LicenseRef-BelowZero-Enterprise

//! Micro-benchmark comparing GQL-direct vs `CypherCompat` parsing overhead.
//!
//! Measures `parse_with_mode` for identical semantics expressed in GQL syntax
//! vs Cypher syntax (which requires the preprocessor step).
#![allow(
    clippy::cast_precision_loss,     // Intentional: benchmark stats don't need exact precision.
    clippy::cast_lossless,           // u32→f64 is lossless but clippy suggests From.
    clippy::doc_markdown,            // Benchmark binary, not a library.
)]

use std::time::Instant;

use tessera_config::QueryLanguage;
use tessera_cypher::parse_with_mode;

const ITERATIONS: u32 = 10_000;

/// Queries expressed in GQL-native syntax (no preprocessor needed).
const GQL_QUERIES: &[&str] = &[
    "INSERT (:N {name: 'Alice'})",
    "MATCH (n:N) RETURN n",
    "MATCH (n:N) WHERE n.name = 'Alice' RETURN n",
    "MATCH (a:N)-[r:KNOWS]->(b:N) RETURN a, r, b",
    "INSERT (:Person {name: 'Bob', age: 30})",
    "MATCH (n) RETURN id(n), labels(n)",
];

/// Equivalent queries in Cypher syntax (requires cypher_to_gql preprocessor).
const CYPHER_QUERIES: &[&str] = &[
    "CREATE (:N {name: 'Alice'})",
    "MATCH (n:N) RETURN n",
    "MATCH (n:N) WHERE n.name = 'Alice' RETURN n",
    "MATCH (a:N)-[r:KNOWS]->(b:N) RETURN a, r, b",
    "CREATE (:Person {name: 'Bob', age: 30})",
    "MATCH (n) RETURN id(n), labels(n)",
];

struct BenchResult {
    mode: &'static str,
    total_ns: u128,
    iterations: u32,
    queries_per_iteration: usize,
}

impl BenchResult {
    fn ops_per_sec(&self) -> f64 {
        let total_ops = self.iterations as f64 * self.queries_per_iteration as f64;
        total_ops / (self.total_ns as f64 / 1_000_000_000.0)
    }

    fn mean_ns(&self) -> f64 {
        self.total_ns as f64 / (self.iterations as f64 * self.queries_per_iteration as f64)
    }
}

fn bench_parse(queries: &[&str], mode: QueryLanguage, label: &'static str) -> BenchResult {
    // Warm up
    for q in queries {
        let _ = parse_with_mode(q, mode);
    }

    let start = Instant::now();
    for _ in 0..ITERATIONS {
        for q in queries {
            let _ = std::hint::black_box(parse_with_mode(q, mode));
        }
    }
    let elapsed = start.elapsed();

    BenchResult {
        mode: label,
        total_ns: elapsed.as_nanos(),
        iterations: ITERATIONS,
        queries_per_iteration: queries.len(),
    }
}

fn main() {
    println!("GQL vs CypherCompat Parsing Benchmark");
    println!("======================================");
    println!("Iterations: {ITERATIONS} x {} queries each\n", GQL_QUERIES.len());

    let gql = bench_parse(GQL_QUERIES, QueryLanguage::Gql, "Gql (direct)");
    let cypher = bench_parse(CYPHER_QUERIES, QueryLanguage::CypherCompat, "CypherCompat");
    let strict = bench_parse(GQL_QUERIES, QueryLanguage::StrictGql, "StrictGql");

    println!("mode,ops_per_sec,mean_ns");
    for r in &[&gql, &cypher, &strict] {
        println!("{},{:.0},{:.0}", r.mode, r.ops_per_sec(), r.mean_ns());
    }

    println!();
    let overhead_pct = ((cypher.mean_ns() / gql.mean_ns()) - 1.0) * 100.0;
    println!(
        "CypherCompat overhead vs Gql: {:.1}% ({:.0} ns → {:.0} ns per query)",
        overhead_pct,
        gql.mean_ns(),
        cypher.mean_ns()
    );

    let strict_overhead_pct = ((strict.mean_ns() / gql.mean_ns()) - 1.0) * 100.0;
    println!(
        "StrictGql overhead vs Gql: {:.1}% ({:.0} ns → {:.0} ns per query)",
        strict_overhead_pct,
        gql.mean_ns(),
        strict.mean_ns()
    );
}
