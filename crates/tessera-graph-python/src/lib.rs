// SPDX-License-Identifier: MIT

use pyo3::prelude::*;

mod batch;
mod errors;
mod gql;
mod graph;
mod query;
mod shared_graph;
mod types;

/// `tessera_graph` — Python bindings for the tessera-graph embeddable graph database.
#[pymodule]
fn tessera_graph(m: &Bound<'_, PyModule>) -> PyResult<()> {
    // Exceptions
    errors::register(m)?;

    // Types
    m.add_class::<types::node_id::PyNodeId>()?;
    m.add_class::<types::edge_id::PyEdgeId>()?;
    m.add_class::<types::node::PyNode>()?;
    m.add_class::<types::edge::PyEdge>()?;
    m.add_class::<types::direction::PyDirection>()?;
    m.add_class::<types::strategy::PyStrategy>()?;
    m.add_class::<types::path::PyPath>()?;
    m.add_class::<types::path::PyPathIter>()?;
    m.add_class::<types::subgraph::PySubgraph>()?;
    m.add_class::<types::pattern_match::PyPatternMatch>()?;

    // Query builders
    m.add_class::<query::neighbor::PyNeighborQuery>()?;
    m.add_class::<query::traversal::PyTraversalBuilder>()?;
    m.add_class::<query::shortest_path::PyShortestPathQuery>()?;
    m.add_class::<query::weighted_path::PyWeightedPathQuery>()?;
    m.add_class::<query::subgraph::PySubgraphQuery>()?;
    m.add_class::<query::pattern::PyPatternBuilder>()?;

    // Graph
    m.add_class::<graph::PyGraph>()?;
    m.add_class::<batch::PyBatchContext>()?;
    m.add_class::<shared_graph::PySharedGraph>()?;
    m.add_class::<shared_graph::PyReadGuard>()?;
    m.add_class::<shared_graph::PyWriteGuard>()?;

    // GQL
    m.add_class::<gql::PyGqlResult>()?;
    m.add_class::<gql::PyGqlResultIter>()?;
    m.add_class::<gql::PyGqlRow>()?;
    m.add_class::<gql::PyGqlMutationResult>()?;
    m.add_function(wrap_pyfunction!(gql::execute, m)?)?;
    m.add_function(wrap_pyfunction!(gql::validate, m)?)?;

    Ok(())
}
