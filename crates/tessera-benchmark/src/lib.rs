//! Multi-target benchmark harness for `TesseraGraph` Enterprise.
//!
//! Provides [`BenchmarkTarget`](target::BenchmarkTarget), dataset generators,
//! scenario runners, and a report generator. Use the `tessera-bench` binary
//! to run comparisons. The `memgraph` feature enables the Bolt-protocol target.

pub mod dataset;
pub mod error;
pub mod measure;
pub mod report;
pub mod scenario;
pub mod target;
pub mod tessera_target;

#[cfg(feature = "memgraph")]
pub mod memgraph_target;
