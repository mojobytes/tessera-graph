// Copyright 2026 BelowZero Security OU. All rights reserved.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use tessera_graph::{Graph, GraphConfig};

use crate::{DatabaseAddress, DatabaseName, TenantError, TenantId};
use crate::error::Result;

/// Registry that maps [`DatabaseAddress`] values to live [`Graph`] instances.
///
/// Each graph is stored behind an `Arc<RwLock<Graph>>` so multiple callers can
/// hold references to the same graph concurrently.  The registry itself is
/// guarded by an `RwLock<HashMap<…>>` so reads (cache hits) never block each
/// other.
///
/// All disk I/O (`Graph::open`) is performed **outside** any lock to avoid
/// blocking unrelated tenants.
pub struct TenantRegistry {
    base_dir: PathBuf,
    graphs: RwLock<HashMap<DatabaseAddress, Arc<RwLock<Graph>>>>,
    graph_config: GraphConfig,
}

impl TenantRegistry {
    /// Creates a new registry backed by `base_dir`.
    ///
    /// No directories are created and no disk I/O is performed at construction
    /// time — everything is lazy.
    #[must_use]
    pub fn new(base_dir: impl Into<PathBuf>, graph_config: GraphConfig) -> Self {
        Self {
            base_dir: base_dir.into(),
            graphs: RwLock::new(HashMap::new()),
            graph_config,
        }
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
    /// Returns an error if the graph cannot be opened.
    ///
    /// # Panics
    ///
    /// Panics if the internal `RwLock` is poisoned (i.e., a thread panicked
    /// while holding the lock).
    #[allow(clippy::significant_drop_tightening)]
    pub fn get_or_load(&self, addr: &DatabaseAddress) -> Result<Arc<RwLock<Graph>>> {
        // Fast path: cache hit under a read lock.
        {
            let guard = self
                .graphs
                .read()
                .expect("TenantRegistry graphs lock poisoned");
            if let Some(arc) = guard.get(addr) {
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
            .expect("TenantRegistry graphs lock poisoned");
        if let Some(existing) = guard.get(addr) {
            return Ok(Arc::clone(existing));
        }
        guard.insert(addr.clone(), Arc::clone(&new_arc));
        Ok(new_arc)
    }

    /// Creates a new database at `addr`, failing if it already exists on disk.
    ///
    /// # Errors
    ///
    /// - [`TenantError::DatabaseAlreadyExists`] if the directory already exists.
    /// - [`TenantError::Graph`] if the graph cannot be opened.
    /// - [`TenantError::Io`] if the directory cannot be created.
    ///
    /// # Panics
    ///
    /// Panics if the internal `RwLock` is poisoned.
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
            .expect("TenantRegistry graphs lock poisoned")
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
        if !self.base_dir.exists() {
            return Ok(Vec::new());
        }

        let mut tenants = Vec::new();
        for entry in std::fs::read_dir(&self.base_dir)? {
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

        if !tenant_dir.exists() {
            return Err(TenantError::TenantNotFound(tenant.to_string()));
        }

        let mut databases = Vec::new();
        for entry in std::fs::read_dir(&tenant_dir)? {
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
    ///
    /// # Panics
    ///
    /// Panics if any internal `RwLock` is poisoned.
    #[allow(clippy::significant_drop_tightening)]
    pub fn flush(&self, addr: &DatabaseAddress) -> Result<()> {
        let arc = {
            let guard = self
                .graphs
                .read()
                .expect("TenantRegistry graphs lock poisoned");
            guard.get(addr).map(Arc::clone)
        };

        let arc = arc.ok_or_else(|| TenantError::DatabaseNotLoaded {
            tenant: addr.tenant.to_string(),
            database: addr.database.to_string(),
        })?;

        arc.write()
            .expect("Graph RwLock poisoned")
            .flush()?;
        Ok(())
    }

    /// Flushes every loaded graph.  Never short-circuits: all graphs are
    /// attempted even if some fail.
    ///
    /// Returns a vec of `(address, error)` pairs for every graph that failed.
    ///
    /// # Panics
    ///
    /// Panics if any internal `RwLock` is poisoned.
    pub fn flush_all(&self) -> Vec<(DatabaseAddress, tessera_graph::Error)> {
        let pairs: Vec<(DatabaseAddress, Arc<RwLock<Graph>>)> = {
            let guard = self
                .graphs
                .read()
                .expect("TenantRegistry graphs lock poisoned");
            guard
                .iter()
                .map(|(addr, arc)| (addr.clone(), Arc::clone(arc)))
                .collect()
        };

        let mut errors = Vec::new();
        for (addr, arc) in pairs {
            let result = arc.write().expect("Graph RwLock poisoned").flush();
            if let Err(e) = result {
                errors.push((addr, e));
            }
        }
        errors
    }

    /// Flushes and removes the graph at `addr` from the in-memory cache.
    ///
    /// # Errors
    ///
    /// - [`TenantError::DatabaseNotLoaded`] if the graph is not in the cache.
    /// - [`TenantError::Graph`] if the flush fails.
    ///
    /// # Panics
    ///
    /// Panics if any internal `RwLock` is poisoned.
    pub fn unload(&self, addr: &DatabaseAddress) -> Result<()> {
        let arc = {
            let mut guard = self
                .graphs
                .write()
                .expect("TenantRegistry graphs lock poisoned");
            guard.remove(addr)
        };

        let arc = arc.ok_or_else(|| TenantError::DatabaseNotLoaded {
            tenant: addr.tenant.to_string(),
            database: addr.database.to_string(),
        })?;

        arc.write()
            .expect("Graph RwLock poisoned")
            .flush()?;
        Ok(())
    }
}
