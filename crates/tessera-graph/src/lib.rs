// SPDX-License-Identifier: MIT

//! # `TesseraGraph`
//!
//! Embeddable graph database for Rust.
//! No server, no schema migrations, no infrastructure. Just add it to your project.
//!
//! ## Quick Start
//!
//! ```
//! use tessera_graph::{Graph, props};
//!
//! let mut graph = Graph::new();
//!
//! let plant  = graph.add_node("Plant",  props! { "name" => "Solar Plant A" }).unwrap();
//! let system = graph.add_node("System", props! { "name" => "Inverter Bank 1" }).unwrap();
//!
//! graph.add_edge("HAS_SYSTEM", plant, system, props! {}).unwrap();
//!
//! assert_eq!(graph.node_count(), 2);
//! assert_eq!(graph.edge_count(), 1);
//! ```

mod access;
mod adj_cache;
mod adj_tail_cache;
mod edge;
mod error;
mod graph;
mod node;
mod property;

mod index;
pub(crate) mod mvcc;
mod query;
mod wal;

pub mod backup;
pub mod call;
pub mod gql;
pub mod schema;

#[cfg(not(feature = "benchmarks"))]
mod storage;
#[cfg(feature = "benchmarks")]
#[doc(hidden)]
pub mod storage;

// Public API re-exports.
pub use access::GraphAccess;
pub use call::{ProcedureKind, resolve_procedure};
pub use edge::Edge;
pub use error::{BatchLimitKind, EdgeId, Error, NodeId, Result};
pub use graph::{FsyncCause, Graph, GraphConfig, QuotaHook, SharedGraph, WalObserver};
pub use index::PropertyIndex;
pub use node::Node;
pub use property::{Properties, Property};
pub use schema::{AppendOnlyDecl, ConstraintDecl, IndexDecl, SchemaCatalog};

pub use query::neighbor::NeighborQuery;
pub use query::pattern::{PatternBuilder, PatternMatch, PatternMatchIter};
pub use query::shortest_path::ShortestPathQuery;
pub use query::subgraph::{Subgraph, SubgraphQuery};
pub use query::traversal::{Strategy, TraversalBuilder};
pub use query::weighted_path::WeightedPathQuery;
pub use query::{Direction, Path};

// Storage types exposed for external adjacency-index implementations.
pub use storage::codec::adjacency_codec::AdjacencyPointer;

// WAL types — needed by external crates for transaction boundaries and recovery.
pub use wal::reader::{WalReadResult, WalReader, WalRecordIter};
pub use wal::record::WalRecord;
pub use wal::writer::WalWriter;

pub use gql::{
    AdminStatement, GqlMutationResult, GqlNode, GqlPath, GqlQuery, GqlRelationship, GqlResult,
    GqlRow, GqlStatement, GqlValue, SecretPlainPassword,
};

pub use gql::{compile_match_bindings, compile_match_for_mutation, resolve_create_props};

// Deprecated literal-only SET helper. Kept for API compatibility with
// 0.2.x consumers; new code should rely on `apply_pipeline_set`.
#[allow(deprecated)]
pub use gql::eval_set_value;

#[doc(hidden)]
pub use gql::literal_to_property;
