// SPDX-License-Identifier: BSL-1.1

//! Shared test fixtures for the `bench_support` unit tests, so each helper has
//! a single definition instead of being copied across sibling modules.

use std::sync::{Arc, RwLock};

use tessera_graph::gql::{self, GqlStatement};
use tessera_graph::Graph;
use tessera_graph_config::QueryLanguage;

use crate::bench_support::dataset::build_dataset;
use crate::bench_support::matrix::{MatrixPoint, Scenario, Transport, Variant};

/// A graph pre-seeded with the deterministic ring dataset of `n` nodes, wrapped
/// in the `Arc<RwLock<_>>` the mutation/timing paths expect.
pub(crate) fn seeded_shared(n: u32) -> Arc<RwLock<Graph>> {
    let mut g = Graph::new();
    build_dataset(&mut g, n).unwrap();
    Arc::new(RwLock::new(g))
}

/// Parses `cypher` and returns its `MutationStatement`, panicking if it is not a
/// mutation — a programming error in the test, not an input error.
pub(crate) fn parse_mutation(cypher: &str) -> gql::MutationStatement {
    let stmt = tessera_graph_cypher::parse_with_mode(cypher, QueryLanguage::CypherCompat)
        .expect("parse");
    match stmt {
        GqlStatement::Mutation(m) => m,
        other => panic!("expected mutation, got {other:?}"),
    }
}

/// A representative in-process matrix point used across the matrix/report tests.
pub(crate) fn sample_point() -> MatrixPoint {
    MatrixPoint {
        scenario: Scenario::MatchCreate,
        readers: 4,
        writers: 2,
        dataset_size: 1000,
        variant: Variant::TwoLockCurrent,
        transport: Transport::InProcess,
        runnable: true,
    }
}
