// SPDX-License-Identifier: BSL-1.1

//! The `single-lock-A` benchmark variant.
//!
//! This is a benchmark-only shim, NOT a production code path. It exists to
//! measure the cost of Option A — holding one write lock across the whole
//! `MATCH … CREATE/SET` mutation instead of the current read-lock-then-write-lock
//! discipline. Crucially it reuses the exact same application function as
//! production (`graph_accessor::apply_match_mutation_body`), so the only
//! difference between this variant and `execute_match_mutation` is the lock
//! discipline — which is the single variable the benchmark isolates.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use tessera_graph::gql::{self, GqlValue};
use tessera_graph::Graph;

use tessera_graph::gql::{apply_match_mutation_body, ResultRow};

/// Runs a `MATCH … CREATE/SET` mutation under a SINGLE write lock held across
/// both the MATCH binding phase and the write phase (the Option-A discipline).
///
/// Compiles the MATCH bindings via the same `gql::compile_match_rows` the
/// production path uses, then applies them via the shared
/// [`apply_match_mutation_body`] — the identical write logic. The one and only
/// difference from `execute_match_mutation` is that the read phase runs under
/// the write lock instead of a separately-acquired read lock.
///
/// # Errors
/// Propagates the mutation's own error string.
// `implicit_hasher`: `params` is forwarded verbatim to `apply_match_mutation_body`,
// whose signature fixes the default `RandomState` hasher.
#[allow(clippy::implicit_hasher)]
pub fn execute_match_mutation_single_lock(
    shared: &Arc<RwLock<Graph>>,
    mutation: &gql::MutationStatement,
    params: &HashMap<String, GqlValue>,
) -> Result<(Vec<ResultRow>, gql::GqlMutationResult), String> {
    let match_clause = mutation.match_clause.as_ref().ok_or_else(|| {
        "execute_match_mutation_single_lock invoked without a MATCH clause".to_owned()
    })?;

    // Single write lock held across BOTH phases — the Option-A discipline.
    let mut graph = shared.write().map_err(|_| "graph lock poisoned".to_owned())?;

    let rows = gql::compile_match_rows(&*graph, match_clause, None)
        .map_err(|e| e.to_string())?;
    if rows.is_empty() {
        return Ok((Vec::new(), gql::GqlMutationResult::default()));
    }
    apply_match_mutation_body(&mut graph, mutation, &rows, params, None).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bench_support::test_helpers::{parse_mutation, seeded_shared};
    use crate::graph_accessor::execute_match_mutation;

    #[test]
    fn single_lock_variant_produces_same_mutation_result_as_two_lock_variant() {
        let m = parse_mutation("MATCH (n:BenchNode) CREATE (x:Tagged)");

        let two_lock = seeded_shared(10);
        let (_r1, s1) =
            execute_match_mutation(&two_lock, &m, &HashMap::new(), None).unwrap();

        let single = seeded_shared(10);
        let (_r2, s2) =
            execute_match_mutation_single_lock(&single, &m, &HashMap::new()).unwrap();

        assert_eq!(
            (s1.nodes_created, s1.edges_created),
            (s2.nodes_created, s2.edges_created),
            "same nodes/edges created by both variants"
        );
        assert_eq!(s1.nodes_created, 10, "one Tagged per matched BenchNode");
    }

    #[test]
    fn single_lock_variant_calls_the_same_apply_match_mutation_body_as_production() {
        // `RETURN after MATCH … CREATE` is rejected inside `apply_match_mutation_body`.
        // If the shim reimplemented the write logic instead of delegating, the two
        // error strings could drift. Asserting they are byte-identical proves both
        // paths run the same body.
        let m = parse_mutation("MATCH (n:BenchNode) CREATE (x:Tagged) RETURN x");

        let two_lock = seeded_shared(3);
        let err_prod = execute_match_mutation(&two_lock, &m, &HashMap::new(), None).unwrap_err();

        let single = seeded_shared(3);
        let err_shim = execute_match_mutation_single_lock(&single, &m, &HashMap::new()).unwrap_err();

        assert_eq!(err_prod, err_shim, "same error from the same shared body");
    }
}
