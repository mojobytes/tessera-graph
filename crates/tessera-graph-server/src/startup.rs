// SPDX-License-Identifier: BSL-1.1

//! Server startup orchestration.
//!
//! This module assembles the four pillars of a running server from a
//! [`ServerConfig`]:
//!
//! 1. **Audit sink** — off / stdout / rotating file.
//! 2. **System graph** — persistent store backing `SystemGraphAuthStore`,
//!    opened at `{data_dir}/system/` with strict Unix permissions and
//!    guarded by an `fs2` exclusive advisory lock so two server processes
//!    can never share identity state.
//! 3. **Auth** — [`NoAuthProvider`] when the operator explicitly opts out
//!    via `TESSERA_NO_AUTH=1`; otherwise `SystemGraphAuthProvider` on top
//!    of the system graph. A bootstrap admin is created on first start.
//! 4. **User graph** — `DefaultGraphAccessor` over `{data_dir}/data/` (or
//!    in-memory when no `data_dir`). Kept strictly separate from the
//!    system graph so user queries can never observe or mutate auth data.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use base64::Engine;
use tokio::sync::{oneshot, watch};
use tokio_rustls::rustls::pki_types::pem::PemObject;

use tessera_graph::Graph;

use crate::audit::AuditSink;
use crate::auth::{
    AuthProvider, NoAuthProvider, SecretString, SystemGraphAuthProvider, SystemGraphAuthStore,
    UserStore,
};
use crate::config::{AuditSinkKind, ServerConfig};
use crate::error::{Result, ServerError};
use crate::listener::TesseraListener;
use crate::migration;
use crate::registry::{COMMUNITY_DATABASE, GraphRegistry};
use crate::registry_handle::StartupPaidRegistry as PaidRegistry;
use crate::system_lock::{self, SystemLockGuard};

/// Minimum bootstrap password length. Must match the minimum enforced
/// by [`SystemGraphAuthStore::create_user`] — if the store raises this
/// threshold, the random-bytes calculation below should be revisited.
const BOOTSTRAP_PASSWORD_BYTES: usize = 32;

/// Payload delivered through the [`start_server_with_ready`] `oneshot`
/// once both the optional metrics endpoint and the Bolt listener have
/// bound. Tests on ephemeral ports use this to discover the resolved
/// ports before the accept loop is entered.
#[derive(Debug, Clone, Copy)]
pub struct ServerReady {
    /// Resolved address of the Bolt listener (`config.bind_addr`).
    pub bolt_addr: SocketAddr,
    /// Resolved address of the Prometheus metrics endpoint when
    /// [`ServerConfig::metrics_addr`] was set, otherwise `None`.
    pub metrics_addr: Option<SocketAddr>,
}

/// Builds the database manager this server runs on.
///
/// The startup path does not know which manager it is assembling: the
/// caller injects one. Community passes [`single_database_factory`] (one
/// database, full access); Enterprise passes
/// [`crate::startup_enterprise::multi_tenant_factory`],
/// which builds the multi-tenant manager and spawns its
/// background tasks. Same shape as [`crate::AccessorFactory`], for the
/// same reason — no compile-time flags, no edition switch inside the
/// binary. Each edition's binary carries exactly one factory.
///
/// The returned [`RegistryBundle`] carries the trait object the generic
/// server talks to, plus an optional concrete handle for the
/// multi-tenant-only operations (per-database stats, online backup,
/// admin DDL) that [`GraphRegistry`] deliberately does not expose.
///
/// # Why the identity store arrives concrete
///
/// The two factories need different slices of identity: the multi-tenant one
/// resolves grants and reads the database catalogue; the Community one needs
/// only local user management. Passing the concrete store lets each narrow it
/// to what it actually uses, instead of forcing the widest surface on both —
/// which is what made the Community manager depend on grants and the
/// catalogue, machinery its edition does not ship.
pub type RegistryFactory = Arc<
    dyn for<'a> Fn(
            &'a ServerConfig,
            Arc<SystemGraphAuthStore>,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<RegistryBundle>> + Send + 'a>,
        > + Send
        + Sync,
>;

/// What a [`RegistryFactory`] produces: the abstract manager the server
/// runs on, plus the concrete multi-tenant registry when the edition has
/// one.
///
/// `multi_tenant` is `None` for Community. It is not an optimisation —
/// it is the type-level statement that a single-database server has no
/// catalogue to report on, no live entry to evict, and no online backup.
/// The Enterprise-only call sites already treat its absence as "no
/// registry to consult" and fail closed.
pub struct RegistryBundle {
    /// The manager every session acquires its database through.
    pub registry: Arc<dyn GraphRegistry>,
    /// El mismo gestor por su tipo de pago, cuando lo hay. Vacío en Community.
    /// El montaje común sólo lo transporta; ver [`PaidRegistry`].
    pub multi_tenant: Option<PaidRegistry>,
}

/// Lo que la edición de pago engancha al arranque común.
///
/// # Por qué existe en vez de tres llamadas
///
/// El arranque común llamaba por su nombre a tres funciones del módulo de pago:
/// lanzar las medidas por base, construir el despachador administrativo y
/// reponer restauraciones interrumpidas. Tres nombres de pago escritos en un
/// fichero que viaja al árbol público — con lo que el árbol público no
/// compilaba, y aunque compilara, **describiría las tres funciones** de las que
/// carece.
///
/// La solución es la misma que ya gobernaba el gestor: **cada edición trae lo
/// suyo hecho**. El arranque llama a lo que le hayan dado sin saber qué es.
/// Vacío no significa "no implementado": significa que un servidor de una sola
/// base no tiene bases que medir, ni catálogo que administrar, ni
/// restauraciones a medias que reponer, porque nada ha podido dejarlas.
///
/// # Por qué llega aparte del gestor, y no dentro de lo que produce la factoría
///
/// Porque uno de los tres **corre antes que la factoría**: reponer una
/// restauración a medias tiene que pasar antes de que el gestor abra ninguna
/// base, que es justamente su motivo de ser. Colgarlo del producto de la
/// factoría lo dejaría corriendo demasiado tarde — reponiendo una base que ya
/// está abierta. Van los tres juntos y por delante, en un parámetro propio.
#[derive(Default, Clone)]
pub struct PaidStartupHooks {
    /// Lanza el vigía que publica las medidas por base.
    pub spawn_per_database_metrics: Option<PerDatabaseMetricsHook>,
    /// Construye el despachador de sentencias administrativas de catálogo y
    /// permisos. El camino de consulta lo transporta sin mirarlo dentro.
    pub admin_builder: Option<PaidAdminBuilderHook>,
    /// Repone las bases que se quedaron a medio restaurar. Sólo la edición que
    /// sabe restaurar puede haber dejado esos restos.
    pub reconcile_restore_artifacts: Option<ReconcileRestoreHook>,
}

/// Lanza el vigía de las medidas por base.
///
/// Recibe el gestor concreto, si hay un punto de medidas sirviendo, los ajustes
/// y el aviso de apagado. Las dos primeras condiciones las comprueba el
/// enganche, no el montaje: sin gestor no hay nada que medir, y sin punto de
/// medidas el vigía trabajaría para nadie.
pub type PerDatabaseMetricsHook =
    Arc<dyn Fn(Option<&PaidRegistry>, bool, &ServerConfig, watch::Receiver<bool>) + Send + Sync>;

/// Construye el despachador administrativo de pago a partir del gestor concreto.
/// Vacío cuando esta edición no trae gestor.
pub type PaidAdminBuilderHook = Arc<
    dyn Fn(Option<&PaidRegistry>) -> Option<crate::admin_dispatch::PaidDispatcherBuilder>
        + Send
        + Sync,
>;

/// Repasa el directorio de datos reponiendo bases que se quedaron a medio
/// restaurar. Corre antes de que el gestor abra ninguna.
pub type ReconcileRestoreHook = Arc<dyn Fn(&Path) + Send + Sync>;

impl std::fmt::Debug for PaidStartupHooks {
    /// A mano porque los enganches son funciones y no se pueden imprimir. Lo
    /// que importa de cara a un diagnóstico es **si los hay**, no cuáles.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PaidStartupHooks")
            .field("metrics", &self.spawn_per_database_metrics.is_some())
            .field("admin", &self.admin_builder.is_some())
            .field("reconcile", &self.reconcile_restore_artifacts.is_some())
            .finish()
    }
}

/// What [`start_server`] returns: the bound address and the live
/// database manager backing this server.
///
/// The handle is the contract that multi-database wiring depends on:
/// tests and operators inspect the manager through this struct rather
/// than reaching into private state. Holding the manager past server
/// shutdown is safe — the sweeper task self-terminates as soon as every
/// `Arc` to the registry has been dropped.
///
/// Dropping the handle is a no-op for the running server (it does not
/// own the accept loop) but releases the only `Arc` to the manager the
/// caller has, which terminates the sweeper task; `#[must_use]`
/// because letting it slip silently is rarely intentional.
#[must_use = "dropping ServerHandle releases the registry Arc and terminates the sweeper"]
pub struct ServerHandle {
    pub addr: SocketAddr,
    /// The database manager, through the abstract seam. Present in every
    /// edition — this is what a generic caller should reach for.
    pub registry: Arc<dyn GraphRegistry>,
    /// El gestor concreto multi-base cuando este servidor lleva uno.
    /// Vacío en Community.
    ///
    /// `pub(crate)` y **sin lector público aquí**: el lector vive en el módulo
    /// de pago (`startup_enterprise`), que es donde nombrar el gestor concreto
    /// es legítimo. El árbol público sostiene el campo —lo rellena la factoría
    /// al montar— pero no ofrece forma de leerlo, así que nadie de fuera puede
    /// depender de algo que su edición no trae.
    pub(crate) multi_tenant: Option<PaidRegistry>,
    /// Bound socket address of the Prometheus metrics endpoint, when
    /// [`ServerConfig::metrics_addr`] was set. `None` means the
    /// endpoint was not requested. Tests that exercise the endpoint on
    /// an ephemeral port read the resolved port from this field.
    pub metrics_addr: Option<SocketAddr>,
}

impl std::fmt::Debug for ServerHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Neither manager is `Debug` — their internal locks make a derived
        // impl risky and noisy. Two derived facts stand in: the strong
        // count (the operationally interesting datum for tests and panics
        // that print the handle) and whether a concrete multi-tenant
        // manager is present, which is what tells the two editions apart
        // in a diagnostic dump.
        f.debug_struct("ServerHandle")
            .field("addr", &self.addr)
            .field("metrics_addr", &self.metrics_addr)
            .field("registry_strong_count", &Arc::strong_count(&self.registry))
            .field("multi_tenant", &self.multi_tenant.is_some())
            .finish()
    }
}

/// Factory for the single-database manager. **Community edition.**
///
/// Opens the one configured database eagerly and hands back a manager with
/// no catalogue and no grants. It does run the version-memory reclaim task:
/// that one is not multi-tenant — it serves explicit transactions, which are
/// Community. `multi_tenant`
/// is `None`, so every Enterprise-only call site fails closed rather than
/// silently degrading.
///
/// The database lives at `{data_dir}/databases/{COMMUNITY_DATABASE}`, the
/// same `databases/<name>/` layout the multi-tenant registry uses, so a
/// directory written by one edition is readable by the other. In-memory
/// mode (no `data_dir`) anchors on a per-process scratch path, matching
/// [`crate::startup_enterprise::build_registry`].
#[must_use]
pub fn single_database_factory() -> RegistryFactory {
    Arc::new(|config, system_store| {
        Box::pin(async move {
            // Ya no hace falta avisar de ajustes que no se aplican: los del
            // gestor multi-base los analiza su propia factoría (apartado 5.10
            // del inventario), así que en esta edición ni siquiera se leen.

            // Community narrows identity to local user management. Grants and
            // the catalogue are Enterprise surfaces this edition does not
            // ship, and the manager consults neither.
            let users: Arc<dyn UserStore> = system_store;
            let data_dir = config.data_dir.clone().unwrap_or_else(|| {
                std::env::temp_dir().join(format!("tessera-inmem-{}", std::process::id()))
            });
            let db_name = COMMUNITY_DATABASE.to_owned();
            let db_dir = data_dir.join("databases").join(&db_name);

            let manager = crate::registry::SingleDatabaseManager::new(
                users,
                db_dir,
                db_name,
                // The operator's engine caps, read from the same
                // `ServerConfig` fields the multi-tenant registry uses.
                // A Community server must honour a configured
                // transaction/batch cap exactly like an Enterprise one —
                // accepting the setting and not applying it is how an
                // unbounded transaction reaches OOM with the operator
                // believing it was capped.
                crate::registry::EngineLimits {
                    txn_memory_bytes: config.max_txn_memory_bytes,
                    batch_operations: config.max_batch_operations,
                    batch_memory_bytes: config.max_batch_memory_bytes,
                },
            )
            .await
            .map_err(ServerError::Registry)?;

            let manager = Arc::new(manager);
            // Reclaim committed-version memory periodically, exactly as the
            // multi-tenant path does. Explicit transactions are Community —
            // the whole engine ships in this edition — so without this task a
            // Community server accumulates those versions for the life of the
            // process while the paid one does not. `0` disables it, matching
            // the multi-tenant gate on the same setting.
            if config.vacuum_interval_seconds > 0 {
                // El asa se descarta a propósito: la tarea vive mientras
                // alguien conserve el gestor, y termina sola en cuanto se
                // suelta la última referencia. Igual que en el multi-base.
                drop(manager.spawn_vacuum(Duration::from_secs(config.vacuum_interval_seconds)));
            }

            Ok(RegistryBundle {
                registry: manager as Arc<dyn GraphRegistry>,
                multi_tenant: None,
            })
        })
    })
}

/// Start the server on a caller-supplied database manager.
///
/// This is the edition-neutral entry point: the startup path assembles
/// audit, the system graph, auth and the listener, but the manager comes
/// from `factory`. Community injects [`single_database_factory`];
/// Enterprise injects [`crate::startup_enterprise::multi_tenant_factory`].
///
/// `ready_tx`, when supplied, fires exactly once with the resolved bind
/// address before the accept loop begins — see
/// [`start_server_with_ready`].
///
/// # Errors
///
/// Same as [`start_server`], plus any error the injected factory returns
/// while opening its database.
pub async fn start_server_with_registry(
    config: ServerConfig,
    shutdown: watch::Receiver<bool>,
    ready_tx: Option<oneshot::Sender<ServerReady>>,
    factory: RegistryFactory,
    paid: PaidStartupHooks,
) -> Result<ServerHandle> {
    start_server_inner(config, shutdown, ready_tx, factory, paid).await
}

#[cfg(feature = "plain-tcp")]
async fn start_server_inner(
    config: ServerConfig,
    shutdown: watch::Receiver<bool>,
    ready_tx: Option<oneshot::Sender<ServerReady>>,
    factory: RegistryFactory,
    paid: PaidStartupHooks,
) -> Result<ServerHandle> {
    let runtime = build_runtime(&config, shutdown.clone(), factory, &paid).await?;
    let registry = Arc::clone(&runtime.registry);

    let metrics_addr = bind_metrics_endpoint(&config, shutdown.clone()).await?;
    // The registry-derived gauges (`tessera_open_databases`,
    // `tessera_database_size_bytes`) report per-database state, which only a
    // multi-tenant registry has. A Community server keeps one database open
    // for the life of the process and has no catalogue, so the poller is not
    // spawned there — publishing a constant `1` and a single size would be
    // noise dressed as observability. Every other metric is edition-neutral
    // and keeps working.
    if let Some(spawn) = paid.spawn_per_database_metrics.as_ref() {
        spawn(
            runtime.multi_tenant.as_ref(),
            metrics_addr.is_some(),
            &config,
            shutdown.clone(),
        );
    }

    // El constructor del despachador administrativo de pago, si esta edición lo
    // trae. Se resuelve aquí —al montar— y viaja hasta el manejador sin que el
    // camino de consulta sepa qué es.
    let paid_admin = paid
        .admin_builder
        .as_ref()
        .and_then(|build| build(runtime.multi_tenant.as_ref()));

    let listener = TesseraListener::bind(&config.bind_addr).await?;
    let addr = listener.local_addr()?;
    if let Some(tx) = ready_tx {
        // Receiver gone is not a bind failure — the operator simply lost
        // interest in the resolved address. Drop the error.
        let _ = tx.send(ServerReady {
            bolt_addr: addr,
            metrics_addr,
        });
    }
    let idle = Duration::from_secs(config.idle_timeout_secs);
    let max = config.max_connections;
    let slow_threshold_ms = config.slow_query_threshold_ms;
    let max_slow_events = config.max_slow_events_per_minute;
    let max_result_rows = config.max_result_rows;
    let queries_max_per_second = config.queries_max_per_second;
    let max_bytes_per_second = config.max_bytes_per_second;
    let query_timeout_ms = config.query_timeout_ms;
    let server_agent = config.server_agent.clone();
    // v0.6.0 Fase 2 Task 5 — build the process-global rate limiter and
    // clone its Arc into every spawned handler via serve_plain/serve_tls.
    let rate_limiter = crate::rate_limiter::RateLimiter::new(
        config.rate_limit_ip_cap,
        config.auth_max_failures_per_minute,
        config.max_connections_per_ip,
    );

    if config.tls_cert.is_some() && config.tls_key.is_some() {
        let tls_config = build_tls(&config)?;
        listener
            .serve_tls(
                runtime.auth,
                runtime.auth_store,
                runtime.audit,
                Arc::clone(&registry),
                runtime.multi_tenant.clone(),
                paid_admin.clone(),
                tls_config,
                Arc::clone(&rate_limiter),
                shutdown,
                max,
                idle,
                slow_threshold_ms,
                max_slow_events,
                max_result_rows,
                queries_max_per_second,
                max_bytes_per_second,
                query_timeout_ms,
                server_agent.clone(),
            )
            .await?;
    } else {
        listener
            .serve_plain(
                runtime.auth,
                runtime.auth_store,
                runtime.audit,
                Arc::clone(&registry),
                runtime.multi_tenant.clone(),
                paid_admin.clone(),
                Arc::clone(&rate_limiter),
                shutdown,
                max,
                idle,
                slow_threshold_ms,
                max_slow_events,
                max_result_rows,
                queries_max_per_second,
                max_bytes_per_second,
                query_timeout_ms,
                server_agent.clone(),
            )
            .await?;
    }

    // Drain live database entries before releasing the system-graph
    // flock. `close_all` waits up to `shutdown_timeout_seconds` for
    // active sessions to finish, then evicts every remaining entry —
    // any `Arc<DatabaseEntry>` clones held by in-flight handles keep
    // the underlying `Graph` alive until they drop, so no data is lost.
    // The Community manager has no sessions to drain but still
    // consolidates its journal here.
    registry
        .close_all(Duration::from_secs(config.shutdown_timeout_seconds))
        .await;

    // Keep the system-graph flock alive until the accept loop exits.
    drop(runtime.system_lock);
    Ok(ServerHandle {
        addr,
        registry,
        multi_tenant: runtime.multi_tenant,
        metrics_addr,
    })
}

#[cfg(not(feature = "plain-tcp"))]
async fn start_server_inner(
    config: ServerConfig,
    shutdown: watch::Receiver<bool>,
    ready_tx: Option<oneshot::Sender<ServerReady>>,
    factory: RegistryFactory,
    paid: PaidStartupHooks,
) -> Result<ServerHandle> {
    if config.tls_cert.is_none() || config.tls_key.is_none() {
        return Err(ServerError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "TLS certificate and key are required \
             (set TESSERA_TLS_CERT and TESSERA_TLS_KEY)",
        )));
    }

    let runtime = build_runtime(&config, shutdown.clone(), factory, &paid).await?;
    let registry = Arc::clone(&runtime.registry);

    let metrics_addr = bind_metrics_endpoint(&config, shutdown.clone()).await?;
    if let Some(spawn) = paid.spawn_per_database_metrics.as_ref() {
        spawn(
            runtime.multi_tenant.as_ref(),
            metrics_addr.is_some(),
            &config,
            shutdown.clone(),
        );
    }

    // El constructor del despachador administrativo de pago, si esta edición lo
    // trae. Se resuelve aquí —al montar— y viaja hasta el manejador sin que el
    // camino de consulta sepa qué es.
    let paid_admin = paid
        .admin_builder
        .as_ref()
        .and_then(|build| build(runtime.multi_tenant.as_ref()));

    let listener = TesseraListener::bind(&config.bind_addr).await?;
    let addr = listener.local_addr()?;
    if let Some(tx) = ready_tx {
        let _ = tx.send(ServerReady {
            bolt_addr: addr,
            metrics_addr,
        });
    }
    let idle = Duration::from_secs(config.idle_timeout_secs);
    let max = config.max_connections;
    let slow_threshold_ms = config.slow_query_threshold_ms;
    let max_slow_events = config.max_slow_events_per_minute;
    let max_result_rows = config.max_result_rows;
    let queries_max_per_second = config.queries_max_per_second;
    let max_bytes_per_second = config.max_bytes_per_second;
    let query_timeout_ms = config.query_timeout_ms;
    let server_agent = config.server_agent.clone();
    // v0.6.0 Fase 2 Task 5 — process-global rate limiter (TLS-only path).
    let rate_limiter = crate::rate_limiter::RateLimiter::new(
        config.rate_limit_ip_cap,
        config.auth_max_failures_per_minute,
        config.max_connections_per_ip,
    );

    let tls_config = build_tls(&config)?;
    listener
        .serve_tls(
            runtime.auth,
            runtime.auth_store,
            runtime.audit,
            Arc::clone(&registry),
            runtime.multi_tenant.clone(),
            paid_admin.clone(),
            tls_config,
            Arc::clone(&rate_limiter),
            shutdown,
            max,
            idle,
            slow_threshold_ms,
            max_slow_events,
            max_result_rows,
            queries_max_per_second,
            max_bytes_per_second,
            query_timeout_ms,
            server_agent,
        )
        .await?;

    // Drain live database entries before releasing the system-graph
    // flock — see the matching block in the plain-tcp entry point above.
    registry
        .close_all(Duration::from_secs(config.shutdown_timeout_seconds))
        .await;

    drop(runtime.system_lock);
    Ok(ServerHandle {
        addr,
        registry,
        multi_tenant: runtime.multi_tenant,
        metrics_addr,
    })
}

/// Bind the optional Prometheus metrics endpoint and return its
/// resolved [`SocketAddr`].
///
/// Returns `Ok(None)` when [`ServerConfig::metrics_addr`] is unset. The
/// endpoint is bound before the Bolt listener so a malformed address
/// fails fast instead of leaving a half-started server. The exporter
/// task itself receives the same shutdown channel as the rest of the
/// server and stops cleanly on signal.
///
/// # Errors
///
/// Returns an error if the configured address is not a valid socket
/// address or the listener cannot bind.
async fn bind_metrics_endpoint(
    config: &ServerConfig,
    shutdown: watch::Receiver<bool>,
) -> Result<Option<SocketAddr>> {
    let Some(raw) = config.metrics_addr.as_deref() else {
        return Ok(None);
    };
    let parsed: SocketAddr = raw.parse().map_err(|e: std::net::AddrParseError| {
        ServerError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("TESSERA_METRICS_ADDR={raw:?} is not a valid socket address: {e}"),
        ))
    })?;
    let bound = crate::metrics::spawn_metrics_server(parsed, shutdown)
        .await
        .map_err(ServerError::Io)?;
    // Match the unauth warn TESSERA_NO_AUTH already emits at startup:
    // the metrics endpoint serves database names (as the `database`
    // label) and aggregate query counts with no authentication, so
    // binding to all interfaces without a firewall is the kind of
    // operator misstep that deserves a loud breadcrumb in the logs.
    if parsed.ip().is_unspecified() {
        tracing::warn!(
            metrics_endpoint = %bound,
            "metrics endpoint bound to all interfaces ({}) with no \
             authentication — restrict access with a firewall or bind \
             to a loopback / internal interface for Prometheus scraping",
            parsed.ip(),
        );
    }
    tracing::info!(
        metrics_endpoint = %bound,
        "metrics: Prometheus exporter listening on http://{bound}/metrics",
    );
    Ok(Some(bound))
}

/// Bundle of runtime components assembled from [`ServerConfig`].
struct ServerRuntime {
    auth: Arc<dyn AuthProvider>,
    auth_store: Arc<dyn UserStore>,
    audit: AuditSink,
    /// The database manager, through the abstract seam. Built by the
    /// injected [`RegistryFactory`], not by this module.
    registry: Arc<dyn GraphRegistry>,
    /// El mismo gestor por su tipo de pago cuando lo hay; vacío en Community.
    multi_tenant: Option<PaidRegistry>,
    /// RAII guard; keep the advisory lock for the server's lifetime.
    system_lock: Option<SystemLockGuard>,
}

/// Assemble the audit sink, system graph / auth store, bootstrap the
/// admin user when empty, and build the user graph. Shared by the
/// TLS and plain-tcp entry points.
/// Ensure the persistent data directory exists and is writable, mapping any
/// failure to the actionable [`ServerError::DataDir`].
///
/// `create_dir_all` is idempotent, so an already-present writable directory is
/// a no-op. It does not, however, detect an *existing* directory the process
/// cannot write to, so a brief write-probe (create + remove a dotfile) covers
/// that case. The probe file uses a fixed name and is removed immediately; a
/// leftover from a crashed prior run is harmless (it is overwritten).
///
/// # Errors
///
/// Returns [`ServerError::DataDir`] if the directory cannot be created or a
/// test file cannot be written inside it.
fn ensure_data_dir_prepared(data_dir: &Path) -> Result<()> {
    std::fs::create_dir_all(data_dir).map_err(|source| ServerError::DataDir {
        path: data_dir.to_path_buf(),
        source,
    })?;
    let probe = data_dir.join(".tessera-write-probe");
    std::fs::write(&probe, b"")
        .and_then(|()| std::fs::remove_file(&probe))
        .map_err(|source| ServerError::DataDir {
            path: data_dir.to_path_buf(),
            source,
        })?;
    Ok(())
}

async fn build_runtime(
    config: &ServerConfig,
    shutdown: watch::Receiver<bool>,
    factory: RegistryFactory,
    paid: &PaidStartupHooks,
) -> Result<ServerRuntime> {
    let audit = build_audit_sink(config, shutdown)?;

    // Persistent deployments must carry a `.tessera-version` marker
    // matching `migration::CURRENT_DISK_LAYOUT`. The check runs before
    // `open_system_graph` so a server upgraded against an out-of-date
    // data dir refuses to touch any on-disk state. In-memory mode
    // (no `data_dir`) has no on-disk layout to gate.
    //
    // Fresh installs are a separate case: a `data_dir` with no
    // `system/` subdirectory and no marker is unambiguously empty, so
    // we stamp the marker and proceed. A *populated* v0.4 dir (has
    // `system/` but no marker) keeps tripping the guard so the
    // operator runs the CLI migrator instead of silently inheriting
    // v0.4 pages under a v0.5 binary.
    if let Some(ref data_dir) = config.data_dir {
        // Prepare and validate the data dir up front so an unwritable path
        // (common now that file-backed is the default) fails with an
        // actionable error naming the path and both escape hatches, rather
        // than surfacing later as a cryptic `migration: io: permission denied`
        // from `read_or_reject`. `create_dir_all` is idempotent — an existing
        // writable dir is a no-op; a write probe catches the existing-but-
        // unwritable case that `create_dir_all` alone would miss.
        ensure_data_dir_prepared(data_dir)?;

        // Fresh-install heuristic: only stamp the marker on a genuinely
        // empty directory. Use `metadata` + `ErrorKind::NotFound` rather
        // than `exists()` so the absence-check is not racy against a
        // local actor with write access. `write_if_missing` itself uses
        // `O_EXCL` so even if both halves of the heuristic disagree the
        // worst case is `AlreadyExists` — never an overwrite.
        let absent = |p: &Path| match std::fs::metadata(p) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
            // Permission errors / dangling symlinks: treat as present
            // and let the layout guard surface the real error.
            Ok(_) | Err(_) => false,
        };
        let marker_path = data_dir.join(".tessera-version");
        let system_path = data_dir.join("system");
        if absent(&marker_path) && absent(&system_path) {
            migration::write_if_missing(data_dir, migration::CURRENT_DISK_LAYOUT)
                .map_err(ServerError::Io)?;
        }
        migration::read_or_reject(data_dir).map_err(ServerError::Migration)?;

        // Recover any database left mid-restore by a crashed process: a `<db>.bak`
        // sibling means a restore moved the original aside but a crash prevented
        // commit/rollback. `reconcile_restore_artifacts` reinstates the original
        // (or clears a residual `.bak` if the restore had committed) so the
        // database is up before the registry opens it, instead of staying down
        // with an orphan backup nobody reconciles.
        if let Some(reconcile) = paid.reconcile_restore_artifacts.as_ref() {
            reconcile(data_dir);
        }
    }

    let (system_graph, system_lock) = open_system_graph(config)?;
    let system_store = Arc::new(
        SystemGraphAuthStore::new(Arc::clone(&system_graph))
            .map_err(|e| ServerError::Io(std::io::Error::other(e.to_string())))?,
    );

    bootstrap_admin_if_empty(&system_store, config).await?;

    let auth: Arc<dyn AuthProvider> = if config.no_auth {
        tracing::warn!(
            "TESSERA_NO_AUTH=1: authentication disabled. \
             All connections will be accepted without credentials."
        );
        Arc::new(NoAuthProvider)
    } else {
        // The local provider now depends only on the user-management surface
        // (`UserStore`), not the full identity store. `from_store` is generic,
        // so the concrete `Arc<SystemGraphAuthStore>` passes directly.
        Arc::new(SystemGraphAuthProvider::from_store(Arc::clone(
            &system_store,
        )))
    };
    // What the runtime keeps is the **user-management** surface: that is all
    // the connection handler needs (listing users to resolve the caller's
    // admin flag). Grants and the catalogue reach admin dispatch through the
    // concrete multi-tenant manager, which owns them and is absent in
    // Community.
    let auth_store: Arc<dyn UserStore> = Arc::clone(&system_store) as Arc<dyn UserStore>;

    // The manager comes from the injected factory: this module assembles
    // audit, the system graph and auth, but does not decide which database
    // manager the server runs on. Community and Enterprise each supply
    // their own — no edition switch inside the binary. The factory receives
    // the concrete store and narrows it to whatever its edition uses.
    let bundle = factory(config, system_store).await?;

    Ok(ServerRuntime {
        auth,
        auth_store,
        audit,
        registry: bundle.registry,
        multi_tenant: bundle.multi_tenant,
        system_lock,
    })
}

/// Construct the audit sink declared by the config. `File` requires
/// either an explicit [`ServerConfig::audit_file`] or a `data_dir` to
/// derive `audit.log` from — a missing destination is a hard error so
/// that audit is never silently disabled.
fn build_audit_sink(config: &ServerConfig, shutdown: watch::Receiver<bool>) -> Result<AuditSink> {
    match config.audit_sink {
        AuditSinkKind::Off => {
            tracing::warn!(
                "TESSERA_AUDIT_SINK=off: audit log disabled. \
                 Only use this in tests."
            );
            Ok(AuditSink::off())
        }
        AuditSinkKind::Stdout => Ok(AuditSink::stdout(shutdown)),
        AuditSinkKind::File => {
            let path = resolve_audit_path(config)?;
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(ServerError::Io)?;
            }
            AuditSink::file(
                path,
                config.audit_max_bytes,
                config.audit_keep_files,
                config.audit_fsync_every,
                shutdown,
            )
            .map_err(|e| ServerError::Io(std::io::Error::other(e.to_string())))
        }
    }
}

fn resolve_audit_path(config: &ServerConfig) -> Result<PathBuf> {
    if let Some(ref p) = config.audit_file {
        return Ok(p.clone());
    }
    if let Some(ref dir) = config.data_dir {
        return Ok(dir.join("audit.log"));
    }
    Err(ServerError::Io(std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        "TESSERA_AUDIT_SINK=file requires TESSERA_AUDIT_FILE or TESSERA_DATA_DIR",
    )))
}

/// Open the persistent system graph under `{data_dir}/system/`, verify
/// its permissions on Unix, and acquire an exclusive advisory lock on
/// `{data_dir}/system/system.lock` so no second process can race on
/// user mutations. When `data_dir` is absent, falls back to an in-memory
/// graph and emits a warn — authoritative identity data must be
/// persistent in any real deployment.
fn open_system_graph(
    config: &ServerConfig,
) -> Result<(Arc<RwLock<Graph>>, Option<SystemLockGuard>)> {
    let Some(ref data_dir) = config.data_dir else {
        tracing::warn!(
            "no TESSERA_DATA_DIR configured: system graph is in-memory. \
             Users created at runtime will be lost on restart."
        );
        return Ok((Arc::new(RwLock::new(Graph::new())), None));
    };

    let system_dir = data_dir.join("system");
    let pre_existing = system_dir.exists();
    std::fs::create_dir_all(&system_dir).map_err(ServerError::Io)?;
    // If the server itself just created the directory, tighten its
    // permissions so the inherited umask (typically 0o022 → 0o755)
    // cannot leak identity data. When the operator pre-created the
    // directory we skip tightening and let `enforce_system_dir_perms`
    // refuse loose permissions explicitly — changing an operator's
    // permissions would be surprising and papers over misconfiguration.
    if !pre_existing {
        tighten_system_dir_perms(&system_dir)?;
    }
    enforce_system_dir_perms(&system_dir)?;

    let guard = system_lock::acquire_exclusive(&system_dir).map_err(ServerError::Io)?;

    let graph_config = tessera_graph::GraphConfig::new();
    let graph = Graph::open(&system_dir, &graph_config).map_err(|e| {
        ServerError::Io(std::io::Error::other(format!(
            "failed to open system graph at {}: {e}",
            system_dir.display()
        )))
    })?;

    Ok((Arc::new(RwLock::new(graph)), Some(guard)))
}

/// Fail-fast if the system directory grants anyone but the owner any
/// permission. Matches the OpenSSH-style refusal for `~/.ssh` — the
/// error message names the offending mode so operators can fix it
/// without guessing.
#[cfg(unix)]
fn enforce_system_dir_perms(dir: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let md = std::fs::metadata(dir).map_err(ServerError::Io)?;
    let mode = md.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(ServerError::Io(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "refusing to start: {} has permissions {:#o} (must be 0o700 or stricter)",
                dir.display(),
                mode
            ),
        )));
    }
    Ok(())
}

#[cfg(not(unix))]
fn enforce_system_dir_perms(_dir: &Path) -> Result<()> {
    tracing::warn!(
        "system dir permission enforcement is Unix-only; \
         rely on filesystem ACLs on this platform"
    );
    Ok(())
}

#[cfg(unix)]
fn tighten_system_dir_perms(dir: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).map_err(ServerError::Io)
}

#[cfg(not(unix))]
fn tighten_system_dir_perms(_dir: &Path) -> Result<()> {
    Ok(())
}

/// Create the bootstrap `admin` user when the system graph is empty.
/// When the operator has supplied `TESSERA_PASSWORD` we use it and emit
/// a warn (the variable is ignored on subsequent starts). Otherwise we
/// generate a 32-byte cryptographic random password, base64url-encode
/// it, and print it to stderr — the only place it is ever surfaced.
async fn bootstrap_admin_if_empty(
    store: &Arc<SystemGraphAuthStore>,
    config: &ServerConfig,
) -> Result<()> {
    let existing = UserStore::list_users(&**store)
        .await
        .map_err(|e| ServerError::Io(std::io::Error::other(e.to_string())))?;

    if !existing.is_empty() {
        if config.password.is_some() {
            tracing::warn!(
                "TESSERA_PASSWORD set but ignored: system graph already has \
                 {} user(s). Use 'ALTER USER admin SET PASSWORD' to rotate.",
                existing.len()
            );
        }
        return Ok(());
    }

    let (password, source) = if let Some(explicit) = config.password.as_ref() {
        tracing::warn!(
            "bootstrapping admin from TESSERA_PASSWORD. \
             This is the *only* run that will honour that variable — \
             rotate via 'ALTER USER admin SET PASSWORD' and unset."
        );
        (SecretString::new(explicit.clone()), "TESSERA_PASSWORD")
    } else {
        let generated = generate_random_password()?;
        eprintln!(
            "────────────────────────────────────────────────────────\n\
             TesseraGraph: bootstrapped admin user.\n\
             username: admin\n\
             password: {generated}\n\
             Store it now — it will not be printed again.\n\
             Rotate via: ALTER USER admin SET PASSWORD '<new-password>'\n\
             ────────────────────────────────────────────────────────"
        );
        (SecretString::new(generated), "random")
    };

    UserStore::create_user(&**store, "admin", &password, true)
        .await
        .map_err(|e| ServerError::Io(std::io::Error::other(e.to_string())))?;

    tracing::info!(source, "bootstrapped admin user");
    Ok(())
}

/// Generate a URL-safe base64 password from 32 cryptographically random
/// bytes (≈43 printable ASCII characters, ≈256 bits of entropy).
fn generate_random_password() -> Result<String> {
    let mut bytes = [0u8; BOOTSTRAP_PASSWORD_BYTES];
    getrandom::getrandom(&mut bytes).map_err(|e| {
        ServerError::Io(std::io::Error::other(format!(
            "failed to read OS CSPRNG: {e}"
        )))
    })?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
}

/// Build TLS server config from cert/key paths.
fn build_tls(config: &ServerConfig) -> Result<Arc<tokio_rustls::rustls::ServerConfig>> {
    let cert_path = config.tls_cert.as_ref().ok_or_else(|| {
        ServerError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "TLS certificate path not configured",
        ))
    })?;
    let key_path = config.tls_key.as_ref().ok_or_else(|| {
        ServerError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "TLS key path not configured",
        ))
    })?;

    let cert_pem = std::fs::read(cert_path).map_err(ServerError::Io)?;
    let key_pem = std::fs::read(key_path).map_err(ServerError::Io)?;

    let certs: Vec<_> = tokio_rustls::rustls::pki_types::CertificateDer::pem_slice_iter(&cert_pem)
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|e| {
            ServerError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("invalid certificate PEM: {e}"),
            ))
        })?;
    let key =
        tokio_rustls::rustls::pki_types::PrivateKeyDer::from_pem_slice(&key_pem).map_err(|e| {
            ServerError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("invalid private key PEM: {e}"),
            ))
        })?;

    let tls = tokio_rustls::rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| {
            ServerError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("TLS config error: {e}"),
            ))
        })?;

    Ok(Arc::new(tls))
}
