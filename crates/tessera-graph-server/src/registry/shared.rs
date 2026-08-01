// SPDX-License-Identifier: BSL-1.1

//! Los tipos del gestor de bases que **usan las dos ediciones**.
//!
//! # Por qué existe este fichero
//!
//! Última costura del servidor (apartado 5.12 del inventario). El fichero que
//! declaraba el módulo del gestor es de pago —1.354 líneas de gestor
//! multi-base— pero dentro tenía cuatro cosas que el árbol público necesita: el
//! nombre de la base que sirve la edición pública, el asa de una base abierta,
//! los topes que el operador fija sobre el motor, y los errores comunes.
//!
//! Es el mismo patrón que ya apareció cuatro veces en esta mudanza: **partición
//! de tipo, no de colocación**. El gestor de una sola base y la interfaz sí
//! viajaban, pero sin su raíz no hay módulo, y la raíz era de pago.
//!
//! # El criterio
//!
//! Aquí está lo que **ninguna edición puede no tener**: un asa de base abierta
//! y unos topes de motor los necesita cualquier servidor, tenga una base o
//! cien. Lo que sólo tiene sentido con varias —el catálogo, los permisos por
//! base, la expulsión por inactividad, las cuentas por base— se queda en el
//! gestor multi-base, que no viaja.
//!
//! La prueba de que el corte es el correcto: la edición pública compila sin el
//! resto del módulo. Si algo de aquí no fuera realmente compartido, no lo haría.

use std::sync::Arc;
use std::sync::RwLock as StdRwLock;
use std::time::Duration;

use tessera_graph::{Graph, GraphConfig};

use crate::auth::{AccessLevel, AuthStoreError};

/// Name of the one database a Community server serves.
///
/// A single-database server has no catalogue, so this is not a lookup key:
/// [`SingleDatabaseManager::acquire`] ignores the requested name and always
/// serves this one database. The constant decides two observable things —
/// the directory under `{data_dir}/databases/` where the database lives,
/// and the name reported back on the session handle (so audit records and
/// Bolt metadata carry a stable label).
pub const COMMUNITY_DATABASE: &str = "neo4j";

/// Soft timeout on `Graph::open` inside the blocking pool. Prevents
/// a stuck filesystem (hung NFS, fuse bug) from wedging every waiter
/// on the same `Notify` indefinitely.
///
/// Shared with the Community manager (`single.rs`), which opens its one
/// database eagerly at build time: the same stuck filesystem would otherwise
/// hang server startup with no bound and no diagnostic.
pub(crate) const OPEN_TIMEOUT: Duration = Duration::from_secs(30);

/// The in-memory engine limits the server applies to every database it opens,
/// sourced from [`ServerConfig`]. Grouped so [`open_graph_with_mvcc`] takes one
/// value instead of three loose `Option<u64>` arguments.
///
/// Public because the Community manager is built from outside this module
/// (by the startup factory, and by the Enterprise repository, which can only
/// consume public API) and must be handed the same operator caps the
/// multi-tenant registry applies. `None` on a field means unlimited.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EngineLimits {
    /// Per-transaction memory cap (issue: wired here for the first time; the
    /// config field was previously parsed but never applied). `None` = unlimited.
    pub txn_memory_bytes: Option<u64>,
    /// Max operations one outermost batch may accumulate (issue #37).
    pub batch_operations: Option<u64>,
    /// Max estimated bytes one outermost batch may accumulate (issue #37).
    pub batch_memory_bytes: Option<u64>,
}

/// Opens the database at `db_dir` and returns it in MVCC (transactional) mode
/// with the WAL-fsync observer installed.
///
/// When `max_size_bytes` is `Some`, a quota hook is attached so every write op
/// runs `check_quota` and surfaces `QuotaExceeded` with rich context. Block 4
/// MVCC is then enabled on every database the registry opens, so Bolt
/// `BEGIN`/`COMMIT`/`ROLLBACK` have a delta table and clock to work against;
/// auto-commit statements keep their exact behaviour (the read/write fast paths
/// gate the MVCC branches behind an `Option::is_none()` check). The `limits`
/// bound a transaction's uncommitted memory and a batch's size (issue #37);
/// each is applied to the engine after MVCC is enabled. Runs inside a
/// `spawn_blocking` task — `Graph::open` is blocking file I/O plus index
/// rebuild.
pub(crate) fn open_graph_with_mvcc(
    db_dir: &std::path::Path,
    graph_config: &GraphConfig,
    quota: Option<tessera_graph::QuotaHook>,
    limits: EngineLimits,
    wal_observer: tessera_graph::WalObserver,
) -> std::result::Result<Graph, tessera_graph::Error> {
    // El tope de tamaño por base es de la edición de pago: quien lo tenga trae
    // su comprobador ya hecho. Antes esta función lo construía aquí, y para eso
    // nombraba el módulo del tope — que no viaja al árbol público, así que un
    // fichero compartido acababa dependiendo de la edición de pago.
    //
    // El gestor de una sola base pasa siempre "sin tope": no hay catálogo donde
    // fijar uno, así que la rama de abajo era además código muerto allí.
    let graph = match quota {
        Some(hook) => Graph::open_with_hook(db_dir, graph_config, hook)?,
        None => Graph::open(db_dir, graph_config)?,
    };
    let mut graph = graph;
    graph.enable_mvcc();
    graph.set_txn_memory_cap(limits.txn_memory_bytes);
    graph.set_batch_limits(limits.batch_operations, limits.batch_memory_bytes);
    Ok(graph.with_wal_observer(wal_observer))
}

/// Minimum sweep interval. `tokio::time::interval(Duration::ZERO)`
/// panics, so a misconfigured `TESSERA_REGISTRY_SWEEP_INTERVAL_SECONDS=0`
/// is clamped to this value and a `warn` is emitted. Production
/// deployments should configure something well above this — the
/// constant exists purely as a fail-safe against the panic.
/// `pub(crate)` so the Community manager clamps with the same value: two
/// managers with divergent floors is the shape of defect this codebase has
/// already hit (see `OPEN_TIMEOUT` above, shared for the same reason).
pub(crate) const MIN_SWEEP_INTERVAL: Duration = Duration::from_secs(1);

/// Errors any database manager can surface — the error type carried by the
/// abstract [`GraphRegistry::acquire`] seam, so it holds only failures that
/// exist independently of multi-tenancy. Multi-tenant-only failures live in
/// [`MultiTenantError`] and are mapped into this type at the trait boundary
/// (see [`DatabaseRegistry::acquire`]).
///
/// Mapped to Bolt error codes by the handler:
/// - `Unauthorized` → `Neo.ClientError.Security.Unauthorized`.
/// - `DatabaseNotFound` → `Neo.ClientError.Database.DatabaseNotFound`.
/// - `DatabaseUnavailable` → `Neo.TransientError.Database.DatabaseUnavailable`.
/// - `StorageExhausted` → `Neo.TransientError.General.OutOfDiskSpace` — an
///   out-of-disk condition is a basic operational fault any persistent
///   manager can hit, so it stays in the common list, not the Enterprise one.
#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("unauthorized")]
    Unauthorized,
    #[error("database not found: {0}")]
    DatabaseNotFound(String),
    #[error("storage exhausted: {0}")]
    StorageExhausted(String),
    #[error("database unavailable: {0}")]
    DatabaseUnavailable(String),
    #[error("auth store error: {0}")]
    AuthStore(#[from] AuthStoreError),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// RAII guard for an acquired database. Dropping it runs the manager's
/// release closure (Enterprise: decrement the session count; Community:
/// no-op). The `Arc` inside `graph` keeps the `Graph` alive until every
/// handle is dropped.
#[must_use = "dropping the handle releases the connection slot immediately"]
pub struct DbHandle {
    graph: Arc<StdRwLock<Graph>>,
    access: AccessLevel,
    database: String,
    /// Opaque per-manager release guard. Its `Drop` runs the manager's
    /// slot-release logic. The generic server never inspects it.
    _guard: Box<dyn Send + Sync>,
}

impl std::fmt::Debug for DbHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `graph` (no useful Debug beyond the lock state) and `_guard`
        // (opaque, manager-specific) are intentionally omitted —
        // `finish_non_exhaustive` documents that omission instead of
        // silently dropping fields.
        f.debug_struct("DbHandle")
            .field("database", &self.database)
            .field("access", &self.access)
            .finish_non_exhaustive()
    }
}

impl DbHandle {
    /// Build a handle. `guard` carries the manager-specific release logic
    /// and runs on drop; the generic server never inspects it.
    pub(crate) fn new(
        graph: Arc<StdRwLock<Graph>>,
        access: AccessLevel,
        database: String,
        guard: Box<dyn Send + Sync>,
    ) -> Self {
        Self {
            graph,
            access,
            database,
            _guard: guard,
        }
    }

    #[must_use]
    pub fn graph(&self) -> Arc<StdRwLock<Graph>> {
        Arc::clone(&self.graph)
    }

    #[must_use]
    pub const fn access_level(&self) -> AccessLevel {
        self.access
    }

    #[must_use]
    pub fn database_name(&self) -> &str {
        &self.database
    }
}
