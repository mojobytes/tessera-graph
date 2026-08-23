// SPDX-License-Identifier: BSL-1.1

//! `SingleDatabaseManager` — the Community database manager.
//!
//! One database, full access, no multi-tenancy: no idle eviction, no
//! per-database connection cap, no grants, no online backup. Opens the
//! single configured database eagerly at build time and hands out a
//! [`DbHandle`] with `ReadWrite` access and an empty release guard (there
//! is no session slot to release — the one database stays open for the
//! life of the process).

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::RwLock as StdRwLock;
use std::time::Duration;

use ermya_graph::{Graph, GraphConfig};

use crate::auth::{AccessLevel, UserStore};

use super::{
    DbHandle, EngineLimits, GraphRegistry, MIN_SWEEP_INTERVAL, OPEN_TIMEOUT, RegistryError,
    open_graph_with_mvcc,
};

/// Community manager: a single always-open database.
///
/// # Identity surface
///
/// The constructor takes an `Arc<dyn UserStore>` — **local user management
/// only** — and does not retain it. Grants and the multi-database catalogue
/// are Enterprise surfaces this manager has no use for: it grants full access
/// without consulting any policy, and serves one database without any
/// catalogue. The startup path owns the store and hands it to the connection
/// handler, which is what actually authenticates.
///
/// Taking it as a parameter (rather than not at all) keeps the two managers
/// constructible the same way, so the startup factory does not special-case
/// the edition. `single_registry_test.rs` pins the narrow surface by building
/// the manager from an `Arc<dyn UserStore>`: widening it back to the combined
/// identity surface breaks that test, which is the point.
pub struct SingleDatabaseManager {
    graph: Arc<StdRwLock<Graph>>,
    db_name: String,
}

impl SingleDatabaseManager {
    /// Open the single configured database and build the manager.
    ///
    /// `limits` carries the operator's engine caps (per-transaction memory,
    /// batch operations, batch memory) and is applied to the one database
    /// exactly as the multi-tenant registry applies it to each of its own.
    /// Passing [`EngineLimits::default`] leaves the engine uncapped.
    ///
    /// `_users` is the local user store the server authenticates against. It
    /// is not retained: this manager never consults identity (see the type
    /// docs). It is a parameter so both editions' managers are built the same
    /// way.
    ///
    /// Unlike the multi-tenant manager, this one runs **no identity
    /// bootstrap**. That bootstrap creates the singleton node that wildcard
    /// grants attach to — a grants concern, and grants are Enterprise. A
    /// Community server has no grants, so the node would be dead weight in
    /// its system graph.
    ///
    /// # Errors
    ///
    /// Returns [`RegistryError::DatabaseUnavailable`] if the engine open
    /// fails (including the open task panicking inside the blocking pool).
    pub async fn new(
        _users: Arc<dyn UserStore>,
        db_dir: PathBuf,
        db_name: String,
        limits: EngineLimits,
    ) -> Result<Self, RegistryError> {
        // `GraphConfig::default()` matches what the multi-tenant registry
        // passes for every database it opens; the operator-tunable caps
        // travel separately in `limits`.
        let graph_config = GraphConfig::default();
        // Same production WAL-fsync observer the multi-database registry
        // installs (`registry/mod.rs`): a stateless function pointer that
        // feeds the `ermya_wal_fsync_duration_seconds` histogram, so
        // Community gets the same durability metrics as Enterprise.
        let wal_observer: ermya_graph::WalObserver = Box::new(crate::metrics::wal_fsync_observed);
        // Open on the blocking pool: Graph::open is file I/O + index rebuild.
        // Bounded by the same `OPEN_TIMEOUT` the multi-database registry uses:
        // a stuck filesystem (hung NFS, fuse bug) must not hang server startup
        // forever with no diagnostic. Community opens eagerly at build time,
        // so an unbounded wait here wedges the whole process.
        let open_task = tokio::task::spawn_blocking(move || {
            open_graph_with_mvcc(&db_dir, &graph_config, None, limits, wal_observer)
        });
        let opened = match tokio::time::timeout(OPEN_TIMEOUT, open_task).await {
            Ok(Ok(Ok(g))) => g,
            Ok(Ok(Err(open_err))) => {
                tracing::error!(database = %db_name, error = %open_err, "Graph::open failed");
                return Err(RegistryError::DatabaseUnavailable(format!(
                    "opening {db_name}: {open_err}"
                )));
            }
            Ok(Err(join_err)) => {
                tracing::error!(database = %db_name, error = %join_err, "Graph::open panicked");
                return Err(RegistryError::DatabaseUnavailable(format!(
                    "{db_name}: internal error opening database"
                )));
            }
            Err(_elapsed) => {
                tracing::error!(
                    database = %db_name,
                    timeout_secs = OPEN_TIMEOUT.as_secs(),
                    "Graph::open timed out"
                );
                return Err(RegistryError::DatabaseUnavailable(format!(
                    "{db_name}: open timed out"
                )));
            }
        };
        Ok(Self {
            graph: Arc::new(StdRwLock::new(opened)),
            db_name,
        })
    }

    /// Start the background task that reclaims committed-version memory every
    /// `interval`, and return its handle.
    ///
    /// The task holds a `Weak` reference, so it does not keep the manager
    /// alive: once the last strong reference is dropped the loop exits on its
    /// next tick. Same discipline as the multi-tenant vacuum task.
    ///
    /// Startup calls this only when the operator's configured interval is
    /// positive; `0` disables the reclaim entirely, and this method clamps a
    /// zero duration as defence in depth because the timer panics on it.
    pub fn spawn_vacuum(self: &Arc<Self>, interval: Duration) -> tokio::task::JoinHandle<()> {
        let interval = if interval.is_zero() {
            tracing::warn!(
                "vacuum interval=0 reached spawn (should be gated off upstream); \
                 clamping to {MIN_SWEEP_INTERVAL:?}. Set \
                 ERMYA_VACUUM_INTERVAL_SECONDS to a positive value."
            );
            MIN_SWEEP_INTERVAL
        } else {
            interval
        };
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            // The timer fires immediately on construction; consume that tick so
            // the first pass happens one full interval after startup.
            ticker.tick().await;
            loop {
                ticker.tick().await;
                let Some(mgr) = weak.upgrade() else {
                    break;
                };
                mgr.vacuum_once().await;
            }
        })
    }

    /// Reclaim the memory held by committed versions no live transaction
    /// still needs. Returns how many version chains were freed.
    ///
    /// Explicit transactions are a Community capability — the whole engine
    /// ships in that edition — and each one that commits leaves versions in
    /// memory until something materialises them to the page. The engine owns
    /// that operation and its contract says skipping it "only costs memory",
    /// but the background task that calls it lived solely on the multi-tenant
    /// manager. A Community server therefore accumulated those versions for
    /// the life of the process while the paid one did not: a memory leak in
    /// the open edition, not a feature split.
    ///
    /// Mirrors the multi-tenant sweep for one database: skips a legacy
    /// (non-transactional) graph, and logs rather than propagates a failed
    /// pass — reclaiming memory is an optimisation, and a server that cannot
    /// vacuum must keep serving.
    pub async fn vacuum_once(&self) -> usize {
        let graph = Arc::clone(&self.graph);
        let db_name = self.db_name.clone();
        // Blocking pool: the graph lock is a std `RwLock` that must not be
        // held across an await point, and materialising pages is file I/O.
        let freed = tokio::task::spawn_blocking(move || {
            let Ok(mut g) = graph.write() else {
                tracing::warn!(database = %db_name, "vacuum skipped: graph lock poisoned");
                return 0;
            };
            if !g.mvcc_enabled() {
                return 0;
            }
            match g.vacuum_once() {
                Ok(freed) => freed,
                Err(e) => {
                    tracing::warn!(database = %db_name, error = %e, "vacuum pass failed");
                    0
                }
            }
        })
        .await;
        match freed {
            Ok(n) => n,
            Err(join_err) => {
                tracing::warn!(error = %join_err, "vacuum task panicked");
                0
            }
        }
    }
}

#[async_trait::async_trait]
impl GraphRegistry for SingleDatabaseManager {
    /// Acquire the one database. `db` is ignored — Community serves a
    /// single database and does not validate the name against a
    /// multi-database catalog.
    ///
    /// # Errors
    ///
    /// Never fails: kept `Result` to satisfy the shared [`GraphRegistry`]
    /// trait signature.
    async fn acquire(&self, _db: &str, _user: &str) -> Result<DbHandle, RegistryError> {
        // Community: one database, full access. The name argument is
        // ignored — there is only one database, and access control is an
        // Enterprise concern. Empty guard: nothing to release.
        Ok(DbHandle::new(
            Arc::clone(&self.graph),
            AccessLevel::ReadWrite,
            self.db_name.clone(),
            Box::new(()),
        ))
    }

    async fn close_all(&self, _timeout: Duration) {
        // Nothing to *drain*: there is no session accounting to wait on.
        // But the checkpoint is not optional. `Graph` has no `Drop` impl, and
        // this manager holds its `Arc` for the life of the process, so nothing
        // would consolidate the journal implicitly — a clean shutdown would
        // leave the whole session in `wal.log` for the next open to replay
        // (#58 measured 3.55 GB of journal against 22 min of startup).
        // `Graph::flush`'s own contract names shutdown as a call site.
        let graph = Arc::clone(&self.graph);
        let db_name = self.db_name.clone();
        // Blocking pool: flush is file I/O, and the lock is a std RwLock that
        // must not be held across an await point.
        let flushed = tokio::task::spawn_blocking(move || match graph.write() {
            Ok(mut g) => g.flush().map_err(|e| e.to_string()),
            Err(poisoned) => poisoned.into_inner().flush().map_err(|e| e.to_string()),
        })
        .await;
        match flushed {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                tracing::error!(database = %db_name, error = %e, "shutdown flush failed");
            }
            Err(join_err) => {
                tracing::error!(database = %db_name, error = %join_err, "shutdown flush panicked");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::SystemGraphAuthStore;
    use ermya_graph::Graph;

    fn test_users() -> Arc<dyn UserStore> {
        let g = Arc::new(StdRwLock::new(Graph::new()));
        Arc::new(SystemGraphAuthStore::new(g).expect("store"))
    }

    /// The eager open must be bounded, and a failure must surface as a
    /// diagnosable error rather than a hang or a panic.
    ///
    /// This pins the *shape* of the bounded path — that `new` returns
    /// `DatabaseUnavailable` naming the database instead of unwinding — using
    /// an unopenable path. It cannot simulate a wedged filesystem, so the
    /// bound itself (`OPEN_TIMEOUT`) is asserted separately below: a stuck
    /// mount is the case that motivated it and is not reproducible in-process.
    #[tokio::test]
    async fn open_failure_surfaces_as_database_unavailable_naming_the_db() {
        let tmp = tempfile::tempdir().unwrap();
        // A regular file where the database directory must be: the engine
        // cannot open this, and the error must be mapped, not propagated raw.
        let blocked = tmp.path().join("not-a-directory");
        std::fs::write(&blocked, b"x").unwrap();

        let result = SingleDatabaseManager::new(
            test_users(),
            blocked,
            "graph".to_string(),
            EngineLimits::default(),
        )
        .await;

        let Err(err) = result else {
            panic!("abrir un fichero como si fuera directorio debe fallar, no tener éxito");
        };

        match err {
            RegistryError::DatabaseUnavailable(msg) => {
                assert!(
                    msg.contains("graph"),
                    "el error debe nombrar la base para ser diagnosticable: {msg}"
                );
            }
            other => panic!("se esperaba DatabaseUnavailable, llegó {other:?}"),
        }
    }

    /// The Community open bound must match the Enterprise one.
    ///
    /// Community opens its single database eagerly at startup, so an unbounded
    /// open wedges the whole process with no diagnostic — the same hazard the
    /// multi-database registry already guards. Both paths must share one
    /// number; this fails if the shared constant is dropped or diverges.
    #[test]
    fn community_open_is_bounded_by_the_shared_timeout() {
        assert_eq!(
            OPEN_TIMEOUT,
            Duration::from_secs(30),
            "el tope de apertura es compartido con el registro multi-base; \
             cambiarlo aquí sin cambiarlo allí desalinea los dos gestores"
        );
    }
}
