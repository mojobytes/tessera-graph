// SPDX-License-Identifier: Apache-2.0

//! Multi-version concurrency control (MVCC) for snapshot-isolation transactions.
//!
//! This module implements the versioning substrate that replaces the engine's
//! global write lock and backs explicit `BEGIN`/`COMMIT`/`ROLLBACK` transactions
//! over Bolt (roadmap Block 4).
//!
//! The model, in one paragraph: a monotonic [`clock::TxnClock`] hands out
//! `start_ts` (a reader's snapshot) and `commit_ts` (when a transaction's
//! writes become visible). Each uncommitted mutation lives as an in-memory
//! delta chained off the record it touches; the on-disk page keeps only the
//! committed version, so existing databases open untouched. A read walks the
//! delta chain and returns the newest version visible to its `start_ts`.
//!
//! Build order note: Phase 3 wired the delta table, clock, registry, and
//! visibility into the `Graph` read/write paths, so the module-wide `dead_code`
//! allow is gone. A few items are consumed only by later phases (commit/rollback
//! in Phase 4, vacuum in Phase 5); each carries a narrow `#[allow(dead_code)]`
//! naming its future caller, so a grep for that attribute is the Phase 9 audit's
//! exact list of not-yet-wired surface.

mod chain;
mod clock;
mod delta;
mod delta_table;
mod registry;
mod visibility;

// `pub` (not `pub(crate)`) because the `mvcc` module is itself only
// `pub(crate)` in `lib.rs`: these re-exports cannot escape the crate, and
// clippy's `redundant_pub_crate` prefers `pub` in this confined position.
pub use clock::TxnClock;
pub use delta::{Delta, DeltaOp, EntitySnapshot};
pub use delta_table::{DeltaTable, EntityKey};
pub use registry::TxnRegistry;
pub use visibility::apply_deltas_for_read;
