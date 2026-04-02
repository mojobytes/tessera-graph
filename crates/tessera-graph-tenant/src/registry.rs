// Copyright 2026 BelowZero Security OU. All rights reserved.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use tessera_graph::{Graph, GraphConfig};

use crate::error::Result;
use crate::{DatabaseAddress, DatabaseName, TenantError, TenantId};

/// Registry that maps [`DatabaseAddress`] values to live [`Graph`] instances.
///
/// Each graph is stored behind an `Arc<RwLock<Graph>>` so multiple callers can
/// hold references to the same graph concurrently.  The registry itself is
/// guarded by an `RwLock<HashMap<…>>` so reads (cache hits) never block each
/// other.
///
/// When `max_loaded > 0`, the registry enforces an LRU eviction policy:
/// the least recently used graph is flushed and unloaded when the cap is
/// exceeded (HIGH #6 — unbounded memory growth).
///
/// All disk I/O (`Graph::open`) is performed **outside** any lock to avoid
/// blocking unrelated tenants.
pub struct TenantRegistry {
    base_dir: PathBuf,
    graphs: RwLock<HashMap<DatabaseAddress, Arc<RwLock<Graph>>>>,
    graph_config: GraphConfig,
    /// Maximum number of loaded graphs. 0 = no limit (default).
    max_loaded: usize,
    /// LRU access order — front is least recently used.
    access_order: RwLock<VecDeque<DatabaseAddress>>,
}

impl TenantRegistry {
    /// Creates a new registry backed by `base_dir` with no eviction limit.
    ///
    /// No directories are created and no disk I/O is performed at construction
    /// time — everything is lazy.
    #[must_use]
    pub fn new(base_dir: impl Into<PathBuf>, graph_config: GraphConfig) -> Self {
        Self::new_with_cap(base_dir, graph_config, 0)
    }

    /// Creates a new registry with an LRU eviction cap.
    ///
    /// When `max_loaded > 0`, loading a new graph beyond the cap evicts
    /// (flushes + removes) the least recently used entry. `0` = no limit.
    #[must_use]
    pub fn new_with_cap(base_dir: impl Into<PathBuf>, graph_config: GraphConfig, max_loaded: usize) -> Self {
        Self {
            base_dir: base_dir.into(),
            graphs: RwLock::new(HashMap::new()),
            graph_config,
            max_loaded,
            access_order: RwLock::new(VecDeque::new()),
        }
    }

    /// Number of graphs currently loaded in memory.
    #[must_use]
    pub fn loaded_count(&self) -> usize {
        self.graphs.read().map(|g| g.len()).unwrap_or(0)
    }

    /// Returns the on-disk path for a given database address.
    fn db_path(&self, addr: &DatabaseAddress) -> PathBuf {
        self.base_dir
            .join(addr.tenant.as_str())
            .join(addr.database.as_str())
    }

    /// Returns the cached graph for `addr`, loading it from disk if necessary.
    ///
    /// The graph directory is created automatically if it does not exist
    /// (`create_if_missing` semantics from [`GraphConfig`]).
    ///
    /// This method is safe to call from multiple threads simultaneously for the
    /// same address: at most one thread will open the graph, while the rest
    /// observe the winner's result.
    ///
    /// # Errors
    ///
    /// - [`TenantError::LockPoisoned`] if the internal `RwLock` is poisoned.
    /// - [`TenantError::Graph`] if the graph cannot be opened.
    #[allow(clippy::significant_drop_tightening)]
    pub fn get_or_load(&self, addr: &DatabaseAddress) -> Result<Arc<RwLock<Graph>>> {
        // Fast path: cache hit under a read lock.
        {
            let guard = self
                .graphs
                .read()
                .map_err(|_| TenantError::LockPoisoned("TenantRegistry graphs"))?;
            if let Some(arc) = guard.get(addr) {
                self.touch_access_order(addr);
                return Ok(Arc::clone(arc));
            }
        }

        // Slow path: open the graph outside any lock so disk I/O does not
        // block threads that are accessing different databases.
        let path = self.db_path(addr);
        let mut config = self.graph_config.clone();
        config.create_if_missing = true;
        let graph = Graph::open(&path, &config)?;
        let new_arc = Arc::new(RwLock::new(graph));

        // Write lock — double-check in case another thread raced us.
        let mut guard = self
            .graphs
            .write()
            .map_err(|_| TenantError::LockPoisoned("TenantRegistry graphs"))?;
        if let Some(existing) = guard.get(addr) {
            self.touch_access_order(addr);
            return Ok(Arc::clone(existing));
        }

        // LRU eviction: if cap is exceeded, evict the least recently used.
        if self.max_loaded > 0 && guard.len() >= self.max_loaded {
            if let Ok(mut order) = self.access_order.write() {
                if let Some(victim) = order.pop_front() {
                    if let Some(evicted) = guard.remove(&victim) {
                        // Best-effort flush before eviction.
                        if let Ok(mut g) = evicted.write() {
                            let _ = g.flush();
                        }
                    }
                }
            }
        }

        guard.insert(addr.clone(), Arc::clone(&new_arc));
        drop(guard);
        self.touch_access_order(addr);
        Ok(new_arc)
    }

    /// Move `addr` to the back of the access order (most recently used).
    fn touch_access_order(&self, addr: &DatabaseAddress) {
        if let Ok(mut order) = self.access_order.write() {
            order.retain(|a| a != addr);
            order.push_back(addr.clone());
        }
    }

    /// Creates a new database at `addr`, failing if it already exists on disk.
    ///
    /// # Errors
    ///
    /// - [`TenantError::DatabaseAlreadyExists`] if the directory already exists.
    /// - [`TenantError::Graph`] if the graph cannot be opened.
    /// - [`TenantError::Io`] if the directory cannot be created.
    /// - [`TenantError::LockPoisoned`] if the internal `RwLock` is poisoned.
    pub fn create_database(&self, addr: &DatabaseAddress) -> Result<Arc<RwLock<Graph>>> {
        let path = self.db_path(addr);

        // Ensure the tenant directory exists first (idempotent).
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        // `create_dir` is atomic at the OS level: if two threads race, exactly
        // one succeeds and the other gets `ErrorKind::AlreadyExists`.
        match std::fs::create_dir(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                return Err(TenantError::DatabaseAlreadyExists {
                    tenant: addr.tenant.to_string(),
                    database: addr.database.to_string(),
                });
            }
            Err(e) => return Err(TenantError::Io(e)),
        }

        let mut config = self.graph_config.clone();
        config.create_if_missing = true;
        let graph = Graph::open(&path, &config)?;
        let arc = Arc::new(RwLock::new(graph));

        self.graphs
            .write()
            .map_err(|_| TenantError::LockPoisoned("TenantRegistry graphs"))?
            .insert(addr.clone(), Arc::clone(&arc));

        Ok(arc)
    }

    /// Lists all tenant directories under `base_dir`.
    ///
    /// Returns an empty vec if `base_dir` does not yet exist.
    ///
    /// # Errors
    ///
    /// Returns an error on unexpected I/O failures (e.g. permission denied).
    pub fn list_tenants(&self) -> Result<Vec<TenantId>> {
        // Use read_dir directly instead of exists() + read_dir() to
        // eliminate the TOCTOU window where the directory could disappear
        // between the check and the read.
        let read_dir = match std::fs::read_dir(&self.base_dir) {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Vec::new());
            }
            Err(e) => return Err(e.into()),
        };

        let mut tenants = Vec::new();
        for entry in read_dir {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                if let Some(name) = entry.file_name().to_str() {
                    if let Ok(id) = TenantId::new(name) {
                        tenants.push(id);
                    }
                }
            }
        }
        Ok(tenants)
    }

    /// Lists all database directories under `base_dir/<tenant>`.
    ///
    /// # Errors
    ///
    /// - [`TenantError::TenantNotFound`] if the tenant directory does not exist.
    /// - [`TenantError::Io`] on unexpected I/O failures.
    pub fn list_databases(&self, tenant: &TenantId) -> Result<Vec<DatabaseName>> {
        let tenant_dir = self.base_dir.join(tenant.as_str());

        // Use read_dir directly instead of exists() + read_dir() to
        // eliminate the TOCTOU window where the directory could disappear
        // between the check and the read.
        let read_dir = match std::fs::read_dir(&tenant_dir) {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(TenantError::TenantNotFound(tenant.to_string()));
            }
            Err(e) => return Err(e.into()),
        };

        let mut databases = Vec::new();
        for entry in read_dir {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                if let Some(name) = entry.file_name().to_str() {
                    if let Ok(db) = DatabaseName::new(name) {
                        databases.push(db);
                    }
                }
            }
        }
        Ok(databases)
    }

    /// Flushes the graph at `addr` to persistent storage.
    ///
    /// # Errors
    ///
    /// - [`TenantError::DatabaseNotLoaded`] if the graph is not in the cache.
    /// - [`TenantError::Graph`] if the flush fails.
    /// - [`TenantError::LockPoisoned`] if any internal `RwLock` is poisoned.
    #[allow(clippy::significant_drop_tightening)]
    pub fn flush(&self, addr: &DatabaseAddress) -> Result<()> {
        let arc = {
            let guard = self
                .graphs
                .read()
                .map_err(|_| TenantError::LockPoisoned("TenantRegistry graphs"))?;
            guard.get(addr).map(Arc::clone)
        };

        let arc = arc.ok_or_else(|| TenantError::DatabaseNotLoaded {
            tenant: addr.tenant.to_string(),
            database: addr.database.to_string(),
        })?;

        arc.write()
            .map_err(|_| TenantError::LockPoisoned("Graph"))?
            .flush()?;
        Ok(())
    }

    /// Flushes every loaded graph.  Never short-circuits: all graphs are
    /// attempted even if some fail.
    ///
    /// Returns a vec of `(address, error)` pairs for every graph that failed.
    /// Lock-poisoned graphs are included as [`tessera_graph::Error::LockPoisoned`]
    /// if the graph engine exposes it, otherwise they are silently skipped.
    ///
    /// # Errors
    ///
    /// Returns [`TenantError::LockPoisoned`] if the registry-level lock is
    /// poisoned (cannot enumerate graphs at all).
    pub fn flush_all(
        &self,
    ) -> std::result::Result<Vec<(DatabaseAddress, tessera_graph::Error)>, TenantError> {
        let pairs: Vec<(DatabaseAddress, Arc<RwLock<Graph>>)> = {
            let guard = self
                .graphs
                .read()
                .map_err(|_| TenantError::LockPoisoned("TenantRegistry graphs"))?;
            guard
                .iter()
                .map(|(addr, arc)| (addr.clone(), Arc::clone(arc)))
                .collect()
        };

        let mut errors = Vec::new();
        for (addr, arc) in pairs {
            let Ok(mut graph) = arc.write() else {
                // Individual graph lock poisoned — skip this graph, log the
                // failure alongside the others.
                continue;
            };
            if let Err(e) = graph.flush() {
                errors.push((addr, e));
            }
        }
        Ok(errors)
    }

    /// Flushes and removes the graph at `addr` from the in-memory cache.
    ///
    /// # Errors
    ///
    /// - [`TenantError::DatabaseNotLoaded`] if the graph is not in the cache.
    /// - [`TenantError::Graph`] if the flush fails.
    /// - [`TenantError::LockPoisoned`] if any internal `RwLock` is poisoned.
    pub fn unload(&self, addr: &DatabaseAddress) -> Result<()> {
        let arc = {
            let mut guard = self
                .graphs
                .write()
                .map_err(|_| TenantError::LockPoisoned("TenantRegistry graphs"))?;
            guard.remove(addr)
        };

        // Keep access_order in sync with graphs to prevent LRU divergence.
        if let Ok(mut order) = self.access_order.write() {
            order.retain(|a| a != addr);
        }

        let arc = arc.ok_or_else(|| TenantError::DatabaseNotLoaded {
            tenant: addr.tenant.to_string(),
            database: addr.database.to_string(),
        })?;

        arc.write()
            .map_err(|_| TenantError::LockPoisoned("Graph"))?
            .flush()?;
        Ok(())
    }
}
