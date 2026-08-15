// SPDX-License-Identifier: BSL-1.1

//! `GraphRegistry` — the abstract database-manager seam.
//!
//! The generic Bolt server talks to whatever manager is plugged in
//! through this trait: open a database for a user, and close everything on
//! shutdown. That is the whole surface. Multi-tenant-only operations (idle
//! eviction, per-database stats, online backup) are deliberately NOT here —
//! they live on the concrete Enterprise `DatabaseRegistry` and are reached
//! by the Enterprise startup by its concrete type.
//!
//! # Why identity is not on this trait
//!
//! An earlier revision required every manager to expose an identity backend
//! covering user management **plus** grants **plus** the database catalogue.
//! That forced the Community manager to depend on the two Enterprise
//! surfaces it has no use for: it grants unconditional access without
//! consulting any policy, and serves one database without any catalogue.
//!
//! Nothing on the generic server path ever called it — the only caller was a
//! test that already held the concrete Enterprise manager. So the requirement
//! bought nothing and cost the Community edition a dependency on machinery it
//! does not ship. Each manager now owns whatever identity it needs privately.

use std::time::Duration;

use async_trait::async_trait;

use super::{DbHandle, RegistryError};

/// Abstract database manager. Community plugs in a single-database
/// manager; Enterprise plugs in the full multi-tenant `DatabaseRegistry`.
#[async_trait]
pub trait GraphRegistry: Send + Sync {
    /// Open `db` for `user` and return a session-scoped [`DbHandle`].
    ///
    /// Whether `user` is authorised — and whether `db` is even looked up —
    /// is the manager's business. The multi-tenant manager resolves grants
    /// and consults its catalogue; the Community manager grants full access
    /// to its one database.
    ///
    /// # Errors
    ///
    /// Propagates [`RegistryError`] — unauthorised access, database not
    /// found, capacity exhaustion, or a backing I/O/auth failure.
    async fn acquire(&self, db: &str, user: &str) -> Result<DbHandle, RegistryError>;

    /// Drain and close every open database, waiting up to `timeout` for
    /// active sessions to finish.
    async fn close_all(&self, timeout: Duration);
}
