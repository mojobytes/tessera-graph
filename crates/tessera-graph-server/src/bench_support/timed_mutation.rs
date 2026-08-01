// SPDX-License-Identifier: BSL-1.1

//! Times a single MATCH…CREATE/SET mutation through the real production path
//! (`graph_accessor::execute_match_mutation`), so the contention runner and
//! the single-lock shim both measure the same code the server runs.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use tessera_graph::gql::{self, GqlValue};
use tessera_graph::Graph;

use crate::graph_accessor::{execute_match_mutation, ResultRow};

/// Runs `execute_match_mutation` (the two-lock production path) once and
/// reports how long it took plus the `(nodes_created, edges_created)` counts.
///
/// # Errors
/// Propagates the mutation's own error string.
// `implicit_hasher`: `params` is forwarded verbatim to `execute_match_mutation`,
// whose signature fixes the default `RandomState` hasher; generalising here
// would not compile against that call.
#[allow(clippy::implicit_hasher)]
pub fn time_match_mutation(
    shared: &Arc<RwLock<Graph>>,
    mutation: &gql::MutationStatement,
    params: &HashMap<String, GqlValue>,
) -> Result<(Duration, u64, u64), String> {
    let start = Instant::now();
    let (_rows, stats): (Vec<ResultRow>, gql::GqlMutationResult) =
        execute_match_mutation(shared, mutation, params, None)?;
    Ok((start.elapsed(), stats.nodes_created, stats.edges_created))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bench_support::test_helpers::{parse_mutation, seeded_shared};

    // NOTE: the server's MATCH…CREATE path (`execute_match_mutation` →
    // `compile_match_bindings` → `compile_match`) binds by label/shape only; it
    // does NOT evaluate a WHERE predicate (verified in `compiler.rs::compile_match`).
    // So CREATE runs once per label-matched row. The tests below control the
    // created count via the dataset size and the matched label, not via WHERE.
    fn match_create_over_label(label: &str) -> gql::MutationStatement {
        parse_mutation(&format!("MATCH (n:{label}) CREATE (m:Tagged)"))
    }

    #[test]
    fn time_single_match_create_returns_positive_duration() {
        // One BenchNode → MATCH binds one row → CREATE runs once.
        let shared = seeded_shared(1);
        let m = match_create_over_label("BenchNode");
        let (dur, _n, _e) = time_match_mutation(&shared, &m, &HashMap::new()).unwrap();
        assert!(dur > Duration::ZERO);
    }

    #[test]
    fn time_single_match_create_reports_nodes_created_one() {
        let shared = seeded_shared(1);
        let m = match_create_over_label("BenchNode");
        let (_dur, nodes, _e) = time_match_mutation(&shared, &m, &HashMap::new()).unwrap();
        assert_eq!(nodes, 1);
    }

    #[test]
    fn time_single_match_create_on_empty_match_returns_zero_created_and_still_measures() {
        // No node carries this label → MATCH binds zero rows → CREATE never runs.
        let shared = seeded_shared(100);
        let m = match_create_over_label("NoSuchLabel");
        let (dur, nodes, _e) = time_match_mutation(&shared, &m, &HashMap::new()).unwrap();
        assert_eq!(nodes, 0);
        assert!(dur >= Duration::ZERO);
    }
}
