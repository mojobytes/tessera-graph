// SPDX-License-Identifier: BSL-1.1

//! Lock-contention benchmark harness (Scenario 1, in-process).
//!
//! Measures how much the read phase of the Cypher mutation path slows down
//! when it contends with concurrent writers under the server's `RwLock`, to
//! decide — with evidence — between collapsing the current two-lock discipline
//! into a single write lock (Option A) or keeping the MATCH read phase under a
//! read lock (Option B).
//!
//! The whole module is gated behind the `bench-support` cargo feature so none
//! of this benchmark-only code links into the released artefact. See
//! `.private/tdd-plan-lock-contention-bench.md`.

pub mod contention_runner;
pub mod dataset;
pub mod latency;
pub mod matrix;
pub mod report;
pub mod timed_mutation;
pub mod variant_shim;

#[cfg(test)]
mod test_helpers;
