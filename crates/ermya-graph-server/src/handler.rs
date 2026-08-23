// SPDX-License-Identifier: BSL-1.1

//! Bolt 4.4 connection handler — state machine for a single client session.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::watch;

use ermya_graph::gql::GqlValue;
use ermya_graph::gql::param_substitution::{self, ParamError};
use ermya_graph_protocol::bolt_frame::{BoltChunkedReader, BoltChunkedWriter};
use ermya_graph_protocol::bolt_handshake::{encode_version_response, negotiate_version};
use ermya_graph_protocol::bolt_message::{
    BoltDict, BoltRequest, BoltResponse, decode_request, encode_response,
};
use ermya_graph_protocol::packstream::PackStreamValue;

use ermya_graph_cypher::cache::QueryCache;

use crate::audit::{AccessDeniedReason, AuditSink, AuthFailureReason, CloseReason, QueryOutcome};
use crate::auth::{AccessLevel, AuthError, AuthProvider, UserStore};
use crate::error::Result;
use crate::graph_accessor::GraphAccessor;
use crate::params::bolt_dict_to_value_map;
use crate::registry::{DbHandle, GraphRegistry, RegistryError};
use crate::registry_handle::MultiTenantHandle;
use crate::wire::gql_value_to_packstream;

/// Maximum number of mutations before an implicit batch is auto-flushed.
const AUTO_FLUSH_OPS: u32 = 500;

/// Maximum age of the first un-synced mutation before auto-flush.
/// Set high to avoid premature flushing during bulk loads where each
/// Bolt round-trip takes tens of milliseconds.
const AUTO_FLUSH_WINDOW: Duration = Duration::from_secs(5);

// ── Batch state ─────────────────────────────────────────────────────────────

/// Per-connection batch state for deferred WAL sync.
struct BatchState {
    /// Number of mutations since the last WAL sync.
    dirty_count: u32,
    /// Timestamp of the first un-synced mutation.
    first_dirty_at: Option<Instant>,
}

impl BatchState {
    const fn new() -> Self {
        Self {
            dirty_count: 0,
            first_dirty_at: None,
        }
    }

    fn mark_dirty(&mut self) {
        self.dirty_count += 1;
        if self.first_dirty_at.is_none() {
            self.first_dirty_at = Some(Instant::now());
        }
    }

    fn should_auto_flush(&self, max_ops: u32, max_age: Duration) -> bool {
        if self.dirty_count >= max_ops {
            return true;
        }
        if let Some(first) = self.first_dirty_at
            && first.elapsed() >= max_age
        {
            return true;
        }
        false
    }

    fn reset_dirty(&mut self) {
        self.dirty_count = 0;
        self.first_dirty_at = None;
    }
}

/// The authenticated principal for this session — captured on
/// successful HELLO and consulted by the admin dispatcher.
#[derive(Debug, Clone)]
struct AuthenticatedUser {
    /// Opaque stable identifier (`UUIDv7` when backed by the system graph).
    /// Not yet surfaced on the wire; reserved for future audit enrichment
    /// and per-user telemetry.
    #[allow(dead_code)]
    user_id: String,
    /// Normalised username — the key the handler uses for `admin_action`
    /// and `query_exec` audit events.
    username: String,
    /// True when the user carries the admin flag in the system graph.
    /// Gates the admin-statement dispatch path.
    is_admin: bool,
}

/// Converts the configured query-timeout (milliseconds, `0` = disabled) into
/// an `Option<Duration>` for the handler. `0` → `None` (no deadline checks).
/// (v0.6.0 Fase 2 Task 6.)
fn query_timeout_from_ms(query_timeout_ms: u64) -> Option<Duration> {
    if query_timeout_ms == 0 {
        None
    } else {
        Some(Duration::from_millis(query_timeout_ms))
    }
}

/// Builds a per-session [`GraphAccessor`] from the bound [`crate::registry::DbHandle`].
///
/// Stored on the handler so [`crate::DefaultGraphAccessor`] construction is the
/// default while the `test-util`-gated [`BoltHandler::with_accessor_factory`]
/// can swap in a test double. `Send + Sync` so the handler stays `Send` across
/// `.await`.
pub type AccessorFactory =
    Arc<dyn Fn(&crate::registry::DbHandle) -> Arc<dyn GraphAccessor> + Send + Sync>;

/// Maps a transaction engine-error string (from `engine_err_to_string`) to a
/// Bolt failure `(code, message)`. Transaction misuse — an inactive `txn_id`,
/// MVCC disabled, or the per-transaction memory cap tripping — is a client
/// request problem, so it maps to `Neo.ClientError.Request.Invalid`; the
/// engine's message is passed through verbatim.
fn map_txn_error(engine_msg: &str) -> (&'static str, String) {
    ("Neo.ClientError.Request.Invalid", engine_msg.to_owned())
}

/// Per-connection Bolt handler.
///
/// Generic over the transport stream `S` and authentication provider `A` so
/// that the enterprise edition can inject RBAC, LBAC, and audit without
/// modifying this crate. The per-session graph accessor is built through
/// [`AccessorFactory`] (defaulting to [`crate::DefaultGraphAccessor`]).
pub struct BoltHandler<S, A: ?Sized>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
    A: AuthProvider,
{
    reader: BoltChunkedReader<crate::rate_limited_io::RateLimited<tokio::io::ReadHalf<S>>>,
    writer: BoltChunkedWriter<crate::rate_limited_io::RateLimited<tokio::io::WriteHalf<S>>>,
    /// v0.6.0 Fase 2 Task 5 eje 4 — shared per-connection bandwidth limiter.
    /// Held so the `Drop` impl can read its sleep counter and emit one
    /// aggregate `BandwidthThrottled` audit event. The read/write halves
    /// wrap clones of this same limiter, so the counter is connection-wide.
    bandwidth: crate::rate_limited_io::BandwidthLimiter,
    auth: Arc<A>,
    /// Local user management — **the Community identity surface**, not the
    /// combined one. The normal session path needs exactly one thing from
    /// identity: listing users to resolve the caller's admin flag after a
    /// successful login.
    ///
    /// Grants and the multi-database catalogue are Enterprise, and the one
    /// place that needs them (admin dispatch) reaches them through the
    /// concrete multi-tenant manager in `admin_registry`, which already
    /// carries the full surface and is `None` in Community. Widening this
    /// field back would make an open Community server depend on machinery it
    /// does not ship.
    auth_store: Arc<dyn UserStore>,
    /// El despachador administrativo de esta edición, inyectado al montar.
    ///
    /// Es lo que permite que este fichero —el camino de consulta, que viaja al
    /// árbol público— no sepa qué sentencias son de pago ni tenga que elegir
    /// entre dos despachadores. Ver [`crate::admin_dispatch`].
    admin_dispatcher: Arc<dyn crate::admin_dispatch::AdminDispatcher>,
    audit: AuditSink,
    query_cache: Arc<QueryCache>,
    /// The database manager, behind the abstract [`GraphRegistry`] seam.
    /// Every session lazy-binds a per-RUN [`DbHandle`] from it on the first
    /// RUN that carries `extra["db"]` — HELLO itself remains pure
    /// authentication, matching the Bolt 4.x/5.x contract that every
    /// official Neo4j driver speaks. (The single-database legacy path was
    /// removed: the server is always registry-backed.)
    ///
    /// This is the ONLY field the normal query path (`try_bind_database`,
    /// `acquire_and_bind`) touches, and it only ever calls `acquire`
    /// through the trait — never the concrete manager. Admin
    /// DDL and online backup need multi-tenant-only operations the trait
    /// deliberately does not expose; see [`Self::admin_registry`].
    registry: Arc<dyn GraphRegistry>,
    /// El gestor de pago, guardado ADEMÁS de [`Self::registry`] para las dos
    /// vías que necesitan operaciones que la interfaz común no ofrece: las
    /// sentencias administrativas de catálogo —borrar y listar bases, que
    /// desalojan las que estén abiertas— y la copia en caliente.
    ///
    /// **En esta edición está siempre vacío y nadie lo lee**, y eso no es un
    /// descuido: el tipo que lo rellenaría no tiene ningún valor posible aquí,
    /// así que el hueco no puede llenarse ni por error. El campo viaja porque
    /// el manejador es el mismo fichero en las dos ediciones; lo que cambia es
    /// si hay algo que meter dentro.
    ///
    /// El aviso de "campo que nadie lee" se silencia a propósito y **sólo sobre
    /// este campo**: apagarlo para el fichero entero taparía descuidos de
    /// verdad. En el árbol de pago el aviso no salta, porque allí sí se lee.
    #[allow(dead_code)]
    admin_registry: MultiTenantHandle,
    /// The database acquired on the first RUN of this session — `None`
    /// until the client sends a RUN carrying `extra["db"]`. Dropping
    /// the handler releases the underlying connection slot through
    /// `DbHandle`'s RAII guard.
    db_handle: Option<DbHandle>,
    /// Per-session accessor wrapping `db_handle.graph()`. Built on the
    /// first successful RUN (in `try_bind_database`) so subsequent RUNs
    /// dispatch through it without reconstructing a `DefaultGraphAccessor`
    /// per statement. `None` until that first bind; the query/mutation
    /// dispatchers are only reached after a successful bind, so they expect
    /// it to be `Some`. Spec §4.2 requires execution against
    /// `handle.graph()`.
    /// Per-session accessor wrapping `db_handle.graph()`. Built on the first
    /// successful RUN (in `try_bind_database`) so subsequent RUNs dispatch
    /// through it without reconstructing a [`crate::DefaultGraphAccessor`] per
    /// statement. `None` until that first bind; the query/mutation dispatchers
    /// run strictly after a successful bind, so they expect it to be `Some`.
    session_accessor: Option<Arc<dyn GraphAccessor>>,
    /// Factory that builds the per-session accessor from a [`crate::registry::DbHandle`].
    /// Defaults to `|h| Arc::new(DefaultGraphAccessor::new(h.graph()))`. The
    /// `#[cfg(test)]`-gated [`Self::with_accessor_factory`] builder replaces it
    /// so integration tests can inject a [`crate::GraphAccessor`] test double
    /// (e.g. one returning a timeout sentinel) without touching the real clock.
    accessor_factory: AccessorFactory,
    authenticated: bool,
    authenticated_user: Option<AuthenticatedUser>,
    failed: bool,
    pending_result: Option<PendingResult>,
    batch_state: BatchState,
    /// Block 4 MVCC: the session's open explicit transaction, set by a `BEGIN`
    /// and cleared by `COMMIT`/`ROLLBACK`. While `Some`, every `RUN` executes
    /// inside this transaction (via the accessor's `_in_txn` methods) instead of
    /// auto-commit. `None` (the default) means auto-commit, the legacy path.
    open_txn: Option<u64>,
    shutdown: watch::Receiver<bool>,
    idle_timeout: Duration,
    connection_id: u64,
    queries_executed: u64,
    /// v0.6.0 Fase 2 Task 3 — slow query threshold in milliseconds.
    /// `0` disables `AuditEvent::SlowQuery` emission entirely. Captured
    /// at construction from `ServerConfig.slow_query_threshold_ms` and
    /// stable for the lifetime of the connection.
    slow_threshold_ms: u64,
    /// v0.6.0 Fase 2 Task 3 — per-connection rate gate for
    /// `AuditEvent::SlowQuery`. Cap matches
    /// `ServerConfig.max_slow_events_per_minute`. Single-threaded
    /// access via this handler's task — no internal locking required.
    slow_gate: crate::audit::SlowQueryGate,
    /// v0.6.0 Fase 2 Task 4 — defensive result-row cap. `0` disables it.
    /// Captured at construction from `ServerConfig.max_result_rows` and
    /// passed to every `execute_*` dispatch. The engine applies the
    /// match-count guard (Cap A); the `GraphAccessor` boundary applies the
    /// output-row guard (Cap B).
    max_result_rows: u64,
    /// v0.6.0 Fase 2 Task 5 — shared rate limiter for auth-IP / conn-IP
    /// axes. Cloned from the process-global `Arc<RateLimiter>` at
    /// construction. `None` only in legacy single-graph paths that
    /// pre-date Task 5 (kept Option-typed so existing tests with no
    /// rate limiter compile). Production wiring always populates it.
    rate_limiter: Option<Arc<crate::rate_limiter::RateLimiter>>,
    /// v0.6.0 Fase 2 Task 5 — peer IP of this connection, captured at
    /// accept time. `None` when the connection's `peer_addr()` lookup
    /// failed (rare; the handler logs and proceeds without rate limit
    /// checks — fail-open on missing IP is preferable to fail-closed,
    /// since the alternative locks out the connection entirely).
    peer_ip: Option<std::net::IpAddr>,
    /// v0.6.0 Fase 2 Task 5 eje 3 — RAII guard holding this connection's
    /// per-IP slot in the global rate limiter. Acquired by the accept loop
    /// *before* the handshake; dropped when the handler is dropped (on
    /// connection close), which decrements the peer IP's live-connection
    /// count. `None` when the per-IP cap is disabled or the peer IP was
    /// unavailable. Never read directly — its sole purpose is the `Drop`,
    /// which `ConnectionGuard` implements; the field is therefore live
    /// despite having no read site.
    conn_guard: Option<crate::rate_limiter::ConnectionGuard>,
    /// v0.6.0 Fase 2 Task 5 eje 2 — per-connection token bucket for
    /// RUN/PULL/DISCARD rate limiting. Capacity = `queries_max_per_second * 2`;
    /// refill rate = `queries_max_per_second` tokens/sec. `cap = 0` disables
    /// (pass-through). Constructed once at handler creation and mutated by
    /// every `handle_run`, `handle_pull`, and `handle_discard` call.
    query_bucket: crate::rate_limiter::TokenBucket,
    /// Configured query rate cap (tokens per second). Stored alongside the
    /// bucket so the FAILURE message can quote the configured limit.
    queries_max_per_second: u32,
    /// SHA-256 hex of the last successfully hashed RUN statement. Set by
    /// `handle_run` after `sha256_hex`; read by `handle_pull` and
    /// `handle_discard` so the `QueryThrottled` audit event carries a
    /// statement fingerprint even when the throttle fires on a PULL or
    /// DISCARD rather than a RUN. `None` before the first RUN.
    last_stmt_hash: Option<String>,
    /// v0.6.0 Fase 2 Task 6 — per-query cooperative timeout. `None` (the
    /// default, `query_timeout_ms = 0`) disables it. When `Some(d)`, each RUN
    /// computes a deadline `Instant::now() + d` and threads it into the engine
    /// via the `GraphAccessor` dispatch; the engine aborts the query if it
    /// overruns and the abort surfaces as
    /// `Neo.ClientError.Statement.ExecutionFailed`.
    query_timeout: Option<Duration>,
    /// v0.7.0 Block 1 — agent string sent in the HELLO `server` metadata
    /// field. Captured from [`ServerConfig::server_agent`] at construction;
    /// stable for the connection lifetime.
    ///
    /// [`ServerConfig::server_agent`]: crate::config::ServerConfig::server_agent
    server_agent: String,
}

/// Stores the result of a RUN until PULL drains it.
///
/// `pub(crate)` para que las vías de pago de `handler_enterprise` puedan dejar
/// preparada su propia fila de resultado; el tipo no sale del paquete.
pub(crate) struct PendingResult {
    /// Pre-encoded fields metadata for the RUN Success response.
    /// Avoids re-cloning column names on every RUN.
    fields_psv: Vec<PackStreamValue>,
    rows: Vec<Vec<PackStreamValue>>,
    /// Index of the next unserved row; `rows[cursor..]` are still
    /// pending. Bolt 4.4 incremental fetch (Task 4 C7): a PULL/DISCARD
    /// with `n` advances the cursor by up to `n` rows and reports
    /// `has_more` while `cursor < rows.len()`. Initialised to `0`.
    cursor: usize,
    /// Mutation counters carried from RUN until the final PULL/DISCARD, where
    /// they are emitted as the Neo4j-style `stats` dict in the `SUCCESS`
    /// metadata. `None` for reads and non-mutating statements.
    stats: Option<ermya_graph::gql::GqlMutationResult>,
}

impl PendingResult {}

/// Global connection counter.
static CONNECTION_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

impl<S, A: ?Sized> BoltHandler<S, A>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
    A: AuthProvider,
{
    /// v0.6.0 Fase 2 Task 3 — poll the slow-query gate for the previous
    /// window's drop count and, when non-zero, emit a single throttling
    /// warning per closed window. Called before every `emit_query_pair`
    /// and one last time in `Drop` so connections closing mid-window
    /// still surface the drops they accumulated. Delegates the actual
    /// gate poll + warning to the free function
    /// [`emit_slow_query_throttle_warning`] so the emission logic is
    /// unit-testable without constructing a full `BoltHandler`.
    fn drain_slow_query_drops(&mut self, now: std::time::Instant) {
        emit_slow_query_throttle_warning(&mut self.slow_gate, self.connection_id, now);
    }

    /// Create a handler by performing the Bolt 4.4 handshake on `stream`,
    /// binding it to the shared database manager. Every RUN lazy-binds a
    /// per-session [`DbHandle`] via `extra["db"]` (see `try_bind_database`),
    /// and mutating statements are gated by the handle's [`AccessLevel`].
    ///
    /// The manager arrives in two parts. `registry` is the edition-neutral
    /// seam used by the whole query path. `multi_tenant` is the concrete
    /// el gestor concreto, que sólo trae la edición que lo lleva; es
    /// what the admin and online-backup call sites require, and passing
    /// `None` makes them fail closed.
    ///
    /// Reads the 20-byte handshake, negotiates the version, sends the 4-byte
    /// response, then splits the stream for chunked framing.
    ///
    /// # Errors
    ///
    /// Returns error if the handshake fails or no supported version is found.
    ///
    /// # Wiring
    ///
    /// `serve_plain`/`serve_tls` invoke this constructor for every accepted
    /// connection. The integration tests under `tests/` reuse it directly to
    /// drive the wire surface without spinning up a TCP listener.
    #[allow(clippy::too_many_arguments)] // constructor config is cohesive
    pub async fn new_with_handshake(
        mut stream: S,
        auth: Arc<A>,
        auth_store: Arc<dyn UserStore>,
        audit: AuditSink,
        registry: Arc<dyn GraphRegistry>,
        multi_tenant: MultiTenantHandle,
        // Cómo construir el despachador administrativo de esta edición, ya
        // resuelto por quien monta el servidor. Vacío en la edición pública,
        // que entonces usa el suyo. Este fichero no nombra ninguno de los dos.
        paid_admin: Option<crate::admin_dispatch::PaidDispatcherBuilder>,
        query_cache: Arc<QueryCache>,
        idle_timeout: Duration,
        slow_threshold_ms: u64,
        max_slow_events_per_minute: u32,
        max_result_rows: u64,
        queries_max_per_second: u32,
        max_bytes_per_second: u64,
        query_timeout_ms: u64,
        server_agent: String,
        rate_limiter: Option<Arc<crate::rate_limiter::RateLimiter>>,
        peer_ip: Option<std::net::IpAddr>,
        shutdown: watch::Receiver<bool>,
    ) -> Result<Self> {
        // Read 20-byte handshake from client.
        let mut handshake = [0u8; 20];
        stream
            .read_exact(&mut handshake)
            .await
            .map_err(crate::error::ServerError::Io)?;

        // Negotiate version.
        let version = negotiate_version(&handshake).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "no supported Bolt version in handshake",
            )
        })?;

        // Send 4-byte version response.
        let resp = encode_version_response(Some(version));
        stream
            .write_all(&resp)
            .await
            .map_err(crate::error::ServerError::Io)?;
        stream
            .flush()
            .await
            .map_err(crate::error::ServerError::Io)?;

        // Split for framed communication.
        let (read_half, write_half) = tokio::io::split(stream);

        // Task 5 eje 4: wrap each half with the shared bandwidth limiter
        // (cap `0` = pass-through). See the registry constructor for the
        // rationale on the shared clone.
        let bandwidth = crate::rate_limited_io::BandwidthLimiter::new(max_bytes_per_second);
        let read_half = crate::rate_limited_io::RateLimited::new(read_half, bandwidth.clone());
        let write_half = crate::rate_limited_io::RateLimited::new(write_half, bandwidth.clone());

        let connection_id = CONNECTION_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        // The caller supplies the two roles separately. `registry` is the
        // abstract seam every edition has, and it is the only thing the
        // normal query path (`try_bind_database`/`acquire_and_bind`) ever
        // touches. `multi_tenant` is the concrete manager, present only in
        // editions that have one; it backs the two Enterprise-only call
        // sites (admin DDL, online backup) that need operations
        // `GraphRegistry` deliberately does not expose. Community passes
        // `None` here and those call sites fail closed — see
        // `dispatch_backup_call` and the admin dispatcher.
        let admin_registry = multi_tenant;

        // El despachador administrativo llega ya elegido: con constructor de
        // pago, el de pago; sin él, el público. Este fichero no nombra ninguno
        // de los dos, que es lo que le permite viajar al árbol público.
        let admin_dispatcher =
            crate::admin_dispatch::build_dispatcher(paid_admin.as_ref(), &auth_store);

        Ok(Self {
            reader: BoltChunkedReader::new(read_half),
            writer: BoltChunkedWriter::new(write_half),
            bandwidth,
            auth,
            auth_store,
            admin_dispatcher,
            audit,
            query_cache,
            registry,
            admin_registry,
            db_handle: None,
            session_accessor: None,
            accessor_factory: Arc::new(|h| {
                Arc::new(crate::DefaultGraphAccessor::new(h.graph())) as Arc<dyn GraphAccessor>
            }),
            authenticated: false,
            authenticated_user: None,
            failed: false,
            pending_result: None,
            batch_state: BatchState::new(),
            open_txn: None,
            shutdown,
            idle_timeout,
            connection_id,
            queries_executed: 0,
            slow_threshold_ms,
            slow_gate: crate::audit::SlowQueryGate::new(max_slow_events_per_minute),
            max_result_rows,
            rate_limiter,
            peer_ip,
            conn_guard: None,
            query_bucket: crate::rate_limiter::TokenBucket::new(
                u64::from(queries_max_per_second),
                std::time::Duration::from_secs(1),
            ),
            queries_max_per_second,
            last_stmt_hash: None,
            query_timeout: query_timeout_from_ms(query_timeout_ms),
            server_agent,
        })
    }

    /// Accessor used by the listener when the handler exits naturally,
    /// so the audit `connection_close` event can carry the username.
    #[must_use]
    pub fn connection_id(&self) -> u64 {
        self.connection_id
    }

    /// v0.6.0 Fase 2 Task 5 eje 3 — attach the per-IP connection guard
    /// acquired by the accept loop. The guard is held for the lifetime of
    /// the handler and decrements the peer IP's live-connection count on
    /// `Drop`. Builder style so the accept loop can wire it without
    /// widening the already-cohesive handshake constructors.
    #[must_use]
    pub fn with_connection_guard(
        mut self,
        guard: Option<crate::rate_limiter::ConnectionGuard>,
    ) -> Self {
        self.conn_guard = guard;
        self
    }

    /// Override the per-session accessor factory. **Test-only.** The factory
    /// is called once per `try_bind_database` invocation; returning a
    /// [`crate::GraphAccessor`] test double lets integration tests assert
    /// timeout, error-mapping, and audit-event behaviour without executing
    /// real engine code or touching the system clock. Production builds never
    /// reach this method — it is compiled only under `cfg(test)` (the crate's
    /// own unit tests) or the `test-util` feature (the integration-test targets,
    /// via the self dev-dependency). The released binary enables neither.
    #[cfg(any(test, feature = "test-util"))]
    #[must_use]
    pub fn with_accessor_factory(mut self, factory: AccessorFactory) -> Self {
        self.accessor_factory = factory;
        self
    }

    /// Whether the session currently has an open explicit transaction. Test-only
    /// observer of `open_txn` used to assert `BEGIN`/`COMMIT`/`ROLLBACK` state
    /// transitions without reaching into private fields.
    #[cfg(any(test, feature = "test-util"))]
    #[must_use]
    pub const fn has_open_txn(&self) -> bool {
        self.open_txn.is_some()
    }

    /// Run the handler until disconnect, timeout, or shutdown.
    ///
    /// # Errors
    ///
    /// Returns error on unrecoverable I/O or protocol errors.
    pub async fn run(&mut self) -> Result<()> {
        let (reason, inner_result) = match self.run_inner().await {
            Ok(r) => (r, Ok(())),
            Err(e) => (CloseReason::IoError, Err(e)),
        };
        // Flush any pending implicit batch so mutations are durable even if
        // the client disconnects without a clean GOODBYE.
        let _ = self.flush_pending_batch();
        let user = self
            .authenticated_user
            .as_ref()
            .map(|u| u.username.as_str());
        self.audit
            .connection_close(self.connection_id, user, reason, self.queries_executed);
        inner_result
    }

    async fn run_inner(&mut self) -> Result<CloseReason> {
        loop {
            let data = tokio::select! {
                biased;

                result = self.shutdown.changed() => {
                    if result.is_err() || *self.shutdown.borrow() {
                        return Ok(CloseReason::Shutdown);
                    }
                    continue;
                }

                result = tokio::time::timeout(self.idle_timeout, self.reader.read_message()) => {
                    match result {
                        Ok(inner) => match inner? {
                            Some(d) => d,
                            None => return Ok(CloseReason::IoError),
                        },
                        Err(_timeout) => return Ok(CloseReason::IdleTimeout),
                    }
                }
            };

            let should_exit = self.dispatch(&data).await?;
            if should_exit {
                return Ok(CloseReason::Goodbye);
            }
        }
    }

    /// Dispatch a single Bolt message. Returns `true` on GOODBYE.
    ///
    /// `ermya_bolt_messages_total{type, outcome}` is emitted exactly
    /// once per message before returning. The outcome is derived from
    /// the wire-level effect:
    ///
    /// - `"error"`   — `self.failed` flipped to `true` inside the handler
    ///   (i.e. the handler called `send_failure`), or decoding the
    ///   request itself failed (logged as `type="UNKNOWN"`).
    /// - `"ignored"` — the connection was already in `failed` state and
    ///   this message was neither RESET nor GOODBYE (server sent IGNORED).
    /// - `"success"` — everything else, including GOODBYE.
    async fn dispatch(&mut self, data: &[u8]) -> Result<bool> {
        let request = match decode_request(data) {
            Ok(r) => r,
            Err(e) => {
                self.failed = true;
                self.send_failure(
                    "Neo.ClientError.Request.Invalid",
                    &format!("cannot decode request: {e}"),
                )
                .await?;
                crate::metrics::bolt_message("UNKNOWN", "error");
                return Ok(false);
            }
        };

        let msg_type = crate::metrics::bolt_request_type_str(&request);
        let failed_before = self.failed;

        let dispatch_result = self.dispatch_request(request).await;

        let outcome = if failed_before {
            // In failed state, only RESET and GOODBYE actually run; any
            // other message gets IGNORED (no failure flip, no success).
            match msg_type {
                "RESET" | "GOODBYE" => "success",
                _ => "ignored",
            }
        } else if self.failed {
            "error"
        } else {
            "success"
        };
        crate::metrics::bolt_message(msg_type, outcome);

        dispatch_result
    }

    /// Dispatch the decoded request to the matching handler.
    ///
    /// Extracted from [`dispatch`] so the surrounding instrumentation
    /// (counter emission with the failure-flip outcome) stays linear.
    async fn dispatch_request(&mut self, request: BoltRequest) -> Result<bool> {
        // In FAILED state, only RESET and GOODBYE are processed.
        if self.failed {
            return match request {
                BoltRequest::Reset => self.handle_reset().await,
                BoltRequest::Goodbye => Ok(true),
                _ => {
                    self.send_ignored().await?;
                    Ok(false)
                }
            };
        }

        match request {
            BoltRequest::Hello { extra } => self.handle_hello(&extra).await,
            BoltRequest::Logon { .. } => {
                // Logon is a Bolt 5.x feature. This server speaks Bolt 4.4 and
                // does NOT process Logon credentials — authentication is handled
                // solely via HELLO. This response acknowledges the message to
                // avoid protocol errors but does NOT set `self.authenticated = true`.
                self.send_response(&BoltResponse::Success { metadata: vec![] })
                    .await?;
                Ok(false)
            }
            BoltRequest::Run {
                query,
                params,
                extra,
                ..
            } => self.handle_run(&query, params, &extra).await,
            BoltRequest::Pull { extra } => self.handle_pull(&extra).await,
            BoltRequest::Discard { extra } => self.handle_discard(&extra).await,
            BoltRequest::Reset => self.handle_reset().await,
            BoltRequest::Goodbye => Ok(true),
            BoltRequest::Begin { .. } => self.handle_begin().await,
            BoltRequest::Commit => self.handle_commit().await,
            BoltRequest::Rollback => self.handle_rollback().await,
        }
    }

    // ── Message handlers ────────────────────────────────────────────────

    // The HELLO handler is a single linear flow (auth → admin lookup →
    // multi-database routing → success metadata). Splitting any of the
    // arms into a helper would either pull `&mut self` through extra
    // signatures or leak intermediate state we deliberately keep
    // local; the body stays cohesive.
    #[allow(clippy::too_many_lines)]
    async fn handle_hello(&mut self, extra: &[(String, PackStreamValue)]) -> Result<bool> {
        // v0.6.0 Fase 2 Task 5 eje 1 — auth-IP throttle.
        // Short-circuit BEFORE evaluating credentials so an attacker cannot
        // burn CPU on argon2 with arbitrary inputs once they've hit the cap.
        if let (Some(rl), Some(ip)) = (self.rate_limiter.as_ref(), self.peer_ip)
            && rl.auth_cap_active().await
            && rl.is_auth_blocked(ip).await
        {
            crate::metrics::rate_limit_hit("auth_ip");
            let failures = rl.auth_failures_in_window(ip).await;
            self.audit
                .auth_throttled(crate::audit::AuthThrottledDetails {
                    client_ip: ip.to_string(),
                    failures_in_window: failures,
                    retry_after_seconds: 60,
                });
            self.failed = true;
            self.send_failure(
                "Neo.ClientError.Security.AuthorizationExpired",
                "too many authentication failures from this address; \
                     try again in 60 seconds",
            )
            .await?;
            return Ok(false);
        }

        let principal = extra
            .iter()
            .find(|(k, _)| k == "principal")
            .and_then(|(_, v)| match v {
                PackStreamValue::String(s) => Some(s.as_str()),
                _ => None,
            })
            .unwrap_or("anonymous");

        let credentials = extra
            .iter()
            .find(|(k, _)| k == "credentials")
            .and_then(|(_, v)| match v {
                PackStreamValue::String(s) => Some(s.as_str()),
                _ => None,
            })
            .unwrap_or("");

        let outcome = match self.auth.authenticate(principal, credentials).await {
            Ok(o) => o,
            Err(e) => {
                let reason = match e {
                    AuthError::InvalidCredentials | AuthError::Backend(_) => {
                        AuthFailureReason::InvalidCredentials
                    }
                    AuthError::UnknownUser => AuthFailureReason::UnknownUser,
                    AuthError::UserDisabled => AuthFailureReason::UserDisabled,
                };
                self.audit
                    .auth_failure(self.connection_id, principal, reason);
                crate::metrics::auth_attempt("failed");
                // v0.6.0 Fase 2 Task 5 eje 1 — record the failure for future
                // throttle decisions. Return value is ignored: we are already
                // committed to sending the Unauthorized response for this
                // attempt; the recorded count blocks the NEXT attempt if the
                // cap is reached.
                if let (Some(rl), Some(ip)) = (self.rate_limiter.as_ref(), self.peer_ip) {
                    let _ = rl.record_auth_failure(ip).await;
                }
                self.failed = true;
                self.send_failure(
                    "Neo.ClientError.Security.Unauthorized",
                    "authentication failed",
                )
                .await?;
                return Ok(false);
            }
        };

        // Discover is_admin from the store so the admin dispatcher (Task
        // 8.2) can gate privileged statements without another round-trip.
        // The principal is lower-cased here to match the store's
        // normalisation rule; AuthProvider returned success against
        // whatever the provider considers canonical, so we just re-check
        // the same key.
        let username = principal.trim().to_ascii_lowercase();
        let is_admin = match self.auth_store.list_users().await {
            Ok(users) => users.iter().any(|u| u.username == username && u.is_admin),
            Err(_) => false,
        };

        self.authenticated_user = Some(AuthenticatedUser {
            user_id: outcome.user_id,
            username: username.clone(),
            is_admin,
        });

        // v0.5.0 Task 10-bis: HELLO is pure authentication. The target
        // database lives in `extra["db"]` on the first RUN (Bolt 4.x/5.x
        // contract — see `try_bind_database`). HELLO `extras.database`
        // is silently ignored (decision D1 of the routing-rewire plan)
        // so misconfigured clients fail at the first RUN with a clearer
        // error than a HELLO-time rejection would give.
        self.authenticated = true;
        self.audit
            .auth_success_with_database(self.connection_id, &username, principal, None);
        crate::metrics::auth_attempt("success");
        // v0.6.0 Fase 2 Task 5 eje 1 — successful auth clears the failure
        // window for this IP so a legitimate user who mistyped a few times
        // does not carry penalty into their session.
        if let (Some(rl), Some(ip)) = (self.rate_limiter.as_ref(), self.peer_ip) {
            rl.record_auth_success(ip).await;
        }

        let metadata = vec![
            (
                "server".to_owned(),
                PackStreamValue::String(self.server_agent.clone()),
            ),
            (
                "connection_id".to_owned(),
                // Bolt spec: connection_id is a String, not an Int. Format
                // chosen for greppability in logs: `conn-<hex>` so shell
                // filtering can distinguish connection ids from other
                // numeric metadata on the same line.
                PackStreamValue::String(format!("conn-{:x}", self.connection_id)),
            ),
        ];

        self.send_response(&BoltResponse::Success { metadata })
            .await?;
        Ok(false)
    }

    /// Lazy-bind the per-session [`DbHandle`] using `extra["db"]` from
    /// the RUN metadata (Bolt 4.0+ contract — see Task 10-bis).
    ///
    /// Returns `Ok(true)` when a FAILURE has been sent on the wire and
    /// the caller must abort the RUN. Returns `Ok(false)` when binding
    /// succeeded (or no action was needed — `db_handle` was already
    /// bound and the RUN omits `db` or carries the same value).
    ///
    /// Per Bolt 5.x the binding is **per-RUN**, not per-session: the
    /// server has no notion of a connection being "owned" by one
    /// database. Drivers reuse pooled TCP connections across logical
    /// sessions targeting different databases, so a RUN naming a new
    /// database on a connection that already served another one must
    /// rebind, not reject. Refusing the rebind would make every
    /// official driver fail as soon as the application opens its
    /// second `session(database=...)` against a different name.
    ///
    /// (Mid-**transaction** switching — a different `db` arriving on a
    /// RUN inside an open BEGIN/COMMIT — is a different rule and stays
    /// reserved for the day explicit transactions land.)
    ///
    /// Cases:
    ///
    /// - `db_handle` already `Some`, `extra["db"]` absent → no-op.
    ///   Subsequent RUNs on the same session inherit the bound handle.
    /// - `db_handle` already `Some`, `extra["db"]` matches the bound
    ///   name → no-op. Drivers re-send `db` on every RUN; the server
    ///   treats that as confirmation.
    /// - `db_handle` already `Some`, `extra["db"]` names a different
    ///   database → **rebind**. Drop the old handle (RAII releases the
    ///   underlying connection slot), validate the new name, call
    ///   `registry.acquire`, swap in the new handle.
    /// - `db_handle` is `None`, `extra["db"]` absent → `DatabaseNotFound`
    ///   ("database parameter required on first RUN — use
    ///   session(database=...)"). Decision D4 of the routing-rewire
    ///   plan: the error names the driver-side fix explicitly because
    ///   the most common cause is forgetting `database=` on the
    ///   session constructor.
    /// - `db_handle` is `None`, `extra["db"]` present → validate +
    ///   `registry.acquire`. Errors map through
    ///   `registry_error_to_bolt_code` / `_to_wire_message` exactly as
    ///   the pre-Task-10-bis HELLO path did, preserving the wire
    ///   contract for every error code other than the location of the
    ///   `db` argument.
    async fn try_bind_database(&mut self, run_extra: &[(String, PackStreamValue)]) -> Result<bool> {
        // Clone the registry Arc into a local so the match below stays readable.
        let registry = self.registry.clone();

        let requested_db = run_extra
            .iter()
            .find(|(k, _)| k == "db")
            .and_then(|(_, v)| match v {
                PackStreamValue::String(s) => Some(s.as_str()),
                _ => None,
            });

        let username = self.current_username().to_owned();

        match (&self.db_handle, requested_db) {
            (Some(_), None) => Ok(false),
            (Some(handle), Some(name)) if handle.database_name() == name => Ok(false),
            (Some(_), Some(name)) => {
                // Per-RUN rebind. Drop the previous handle so RAII
                // releases its connection slot before we ask the
                // registry for the new one — otherwise both slots
                // would be held briefly and a max_connections=N
                // database could see N+1 acquires under heavy
                // multi-DB churn from the same session.
                self.db_handle = None;
                self.session_accessor = None;
                self.acquire_and_bind(&registry, name, &username).await
            }
            (None, None) => {
                self.failed = true;
                self.audit.access_denied(
                    self.connection_id,
                    Some(&username),
                    AccessDeniedReason::InvalidDatabaseName,
                    None,
                );
                self.send_failure(
                    "Neo.ClientError.Database.DatabaseNotFound",
                    "database parameter required on first RUN — use session(database=...)",
                )
                .await?;
                Ok(true)
            }
            (None, Some(db_name)) => self.acquire_and_bind(&registry, db_name, &username).await,
        }
    }

    /// Validate `db_name`, call `registry.acquire`, and on success
    /// install the resulting [`DbHandle`] + accessor onto the session.
    /// Shared by the first-bind and the rebind branches of
    /// [`Self::try_bind_database`] so the wire-error mapping and the
    /// `auth_success_with_database` audit event stay in lockstep.
    ///
    /// Returns `Ok(true)` when a FAILURE was sent on the wire (caller
    /// must abort the RUN), `Ok(false)` when binding succeeded.
    async fn acquire_and_bind(
        &mut self,
        registry: &Arc<dyn GraphRegistry>,
        db_name: &str,
        username: &str,
    ) -> Result<bool> {
        if !is_valid_database_name(db_name) {
            self.failed = true;
            self.audit.access_denied(
                self.connection_id,
                Some(username),
                AccessDeniedReason::InvalidDatabaseName,
                Some(db_name),
            );
            self.send_failure(
                "Neo.ClientError.Database.DatabaseNotFound",
                "invalid or reserved database name",
            )
            .await?;
            return Ok(true);
        }
        match registry.acquire(db_name, username).await {
            Ok(handle) => {
                // Audit the name of the database that was actually opened,
                // taken from the handle — NOT `db_name`, which is only what
                // the client asked for. The two diverge under a manager that
                // does not treat the request as a lookup key: the Community
                // single-database manager serves its one database whatever
                // name arrives, so auditing the request would record access
                // to a database that does not exist. Under the multi-tenant
                // registry they always match, because the catalogue rejects
                // anything else — which is why this went unnoticed.
                let bound_db = handle.database_name().to_owned();
                let accessor = (self.accessor_factory)(&handle);
                self.session_accessor = Some(accessor);
                self.db_handle = Some(handle);
                // Re-emit the auth-success event with the freshly-bound
                // database name so audit consumers that correlate
                // sessions with databases (the common case in v0.4.x)
                // can still recover the mapping. The HELLO-time event
                // carried `database=None`; every bind/rebind emits one
                // of these, all sharing the same `connection_id` so log
                // analysers can stitch them by that key.
                self.audit.auth_success_with_database(
                    self.connection_id,
                    username,
                    username,
                    Some(&bound_db),
                );
                Ok(false)
            }
            Err(e) => {
                let code = registry_error_to_bolt_code(&e);
                let wire_msg = registry_error_to_wire_message(&e);
                self.failed = true;
                if let Some(reason) = registry_error_to_access_denied_reason(&e) {
                    self.audit.access_denied(
                        self.connection_id,
                        Some(username),
                        reason,
                        Some(db_name),
                    );
                }
                self.send_failure(code, &wire_msg).await?;
                Ok(true)
            }
        }
    }

    // allow: cohesive state machine, splitting would fragment logic
    #[allow(clippy::too_many_lines)]
    #[tracing::instrument(
        skip_all,
        fields(
            connection_id = self.connection_id,
            database = tracing::field::Empty,
            statement_sha256 = tracing::field::Empty,
            kind = tracing::field::Empty,
            duration_ms = tracing::field::Empty,
        )
    )]
    async fn handle_run(
        &mut self,
        query: &str,
        params: BoltDict,
        run_extra: &[(String, PackStreamValue)],
    ) -> Result<bool> {
        use ermya_graph::gql::GqlStatement;

        if !self.authenticated {
            // Spec §8: a missing-HELLO is a protocol-violation, not a
            // credentials issue. Drivers in the Neo4j family treat
            // `Request.Invalid` as a permanent error (no retry, no
            // re-auth prompt), which matches the actual cause: the
            // session never completed handshake.
            self.failed = true;
            self.audit.access_denied(
                self.connection_id,
                None,
                AccessDeniedReason::NotAuthenticated,
                None,
            );
            self.send_failure(
                "Neo.ClientError.Request.Invalid",
                "not authenticated — send HELLO first",
            )
            .await?;
            return Ok(false);
        }

        // Las sentencias administrativas (catálogo, permisos y cuentas)
        // USERS/DATABASES/GRANTS, user DDL) run against the system graph
        // regardless of the session's selected user database
        // (docs/multi-database.md §"Admin statements"). They must therefore
        // be EXEMPT from the per-RUN user-database bind: otherwise a fresh
        // server with zero user databases could never accept the FIRST
        // crear una base por la conexión — el cliente siempre manda la base
        // on the first RUN, the bind would `registry.acquire` a non-existent
        // (or reserved, e.g. "system") database, and fail before the admin
        // statement reached `dispatch_admin`. The detection is a cheap
        // prefix match (no full parse, no cache interaction); the canonical
        // parse + dispatch still happens below.
        let skip_db_bind = matches!(ermya_graph_cypher::try_parse_admin(query), Ok(Some(_)));

        // v0.5.0 Task 10-bis: multi-database routing happens on the
        // first RUN of the session, not on HELLO. `extra["db"]` is the
        // canonical wire key — every official Neo4j driver puts the
        // session database there. See `try_bind_database` for the full
        // rule table (binding, mid-session switch, missing key).
        if !skip_db_bind && self.try_bind_database(run_extra).await? {
            // `try_bind_database` returned `true` to signal it already
            // sent a FAILURE on the wire; the session is now in FAILED
            // state and waits for RESET.
            return Ok(false);
        }

        let stmt_hash = sha256_hex(query);
        // Store for PULL/DISCARD throttle audit events on this session.
        self.last_stmt_hash = Some(stmt_hash.clone());
        let started = Instant::now();
        tracing::Span::current().record("statement_sha256", stmt_hash.as_str());
        // `database` is known once the session has bound a DbHandle
        // (registry path) — record it here so even early error returns
        // carry it. `None` leaves the field Empty.
        if let Some(handle) = self.db_handle.as_ref() {
            tracing::Span::current().record("database", handle.database_name());
        }

        // v0.6.0 Fase 2 Task 5 eje 2 — per-connection query token bucket.
        // Checked after database binding so the audit event can carry the
        // database name; checked before any engine work so throttled
        // requests consume no executor resources.
        {
            let now = std::time::Instant::now();
            if !self.query_bucket.try_take(1, now) {
                let tokens_available = self.query_bucket.available(now);
                let user = self.current_username().to_owned();
                let database = self
                    .db_handle
                    .as_ref()
                    .map(|h| h.database_name().to_owned());
                crate::metrics::rate_limit_hit("query_conn");
                self.audit
                    .query_throttled(crate::audit::QueryThrottledDetails {
                        connection_id: self.connection_id,
                        user,
                        statement_sha256: stmt_hash.clone(),
                        database,
                        tokens_available,
                    });
                self.failed = true;
                self.send_failure(
                    "Neo.ClientError.Security.TooManyRequests",
                    &format!(
                        "query rate limit exceeded ({} queries/s cap)",
                        self.queries_max_per_second
                    ),
                )
                .await?;
                return Ok(false);
            }
        }

        // Wire-level RUN.params → engine HashMap<String, GqlValue>.
        // Unrepresentable PackStream variants surface as TypeError per
        // spec section 7 (Bytes, Dict, Struct).
        let param_map = match bolt_dict_to_value_map(&params) {
            Ok(m) => m,
            Err(e) => {
                let elapsed_ms = elapsed_ms(started);
                let user = self.current_username().to_owned();
                self.drain_slow_query_drops(started);
                self.audit.emit_query_pair(
                    &mut self.slow_gate,
                    self.slow_threshold_ms,
                    started,
                    self.connection_id,
                    &user,
                    &stmt_hash,
                    None,
                    elapsed_ms,
                    0,
                    QueryOutcome::Error {
                        error_code: "Neo.ClientError.Statement.TypeError".to_owned(),
                    },
                );
                let database = self
                    .db_handle
                    .as_ref()
                    .map(|h| h.database_name().to_owned());
                crate::metrics::query_executed(database.as_deref(), "error");
                self.queries_executed += 1;
                self.failed = true;
                self.send_failure("Neo.ClientError.Statement.TypeError", &e.to_string())
                    .await?;
                return Ok(false);
            }
        };

        // Parse via cypher compat layer (cache-through to skip re-parsing
        // repeated statements during bulk loads). The cache returns a
        // clone of the cached AST so the in-place substitution below is
        // local to this RUN and never poisons other connections.
        let params_signature = ermya_graph_cypher::cache::hash_params(&param_map);
        let mut stmt = match ermya_graph_cypher::parse_with_mode_cached(
            query,
            ermya_graph_config::QueryLanguage::CypherCompat,
            params_signature,
            &self.query_cache,
        ) {
            Ok(s) => s,
            Err(e) => {
                let elapsed_ms = elapsed_ms(started);
                let user = self.current_username().to_owned();
                let database = self
                    .db_handle
                    .as_ref()
                    .map(|h| h.database_name().to_owned());
                self.drain_slow_query_drops(started);
                self.audit.emit_query_pair(
                    &mut self.slow_gate,
                    self.slow_threshold_ms,
                    started,
                    self.connection_id,
                    &user,
                    &stmt_hash,
                    database.as_deref(),
                    elapsed_ms,
                    0,
                    QueryOutcome::Error {
                        error_code: "Neo.ClientError.Statement.SyntaxError".to_owned(),
                    },
                );
                // Syntax errors never produce an AST so we cannot
                // attach `kind`; the counter alone is enough to
                // surface the failure rate. Histograms are emitted
                // only on paths where the executor ran.
                crate::metrics::query_executed(database.as_deref(), "error");
                self.queries_executed += 1;
                self.failed = true;
                self.send_failure("Neo.ClientError.Statement.SyntaxError", &e.to_string())
                    .await?;
                return Ok(false);
            }
        };

        // Apply $param substitution before compilation. Failures map to
        // stable Bolt wire codes per spec section 7. On error the AST is
        // partially substituted; we drop it via the early `return` so the
        // compiler never sees an unsubstituted ParamRef.
        if let Err(perr) = param_substitution::apply(&mut stmt, &param_map) {
            let (code, message) = param_error_to_wire(&perr);
            let elapsed_ms = elapsed_ms(started);
            let user = self.current_username().to_owned();
            self.drain_slow_query_drops(started);
            self.audit.emit_query_pair(
                &mut self.slow_gate,
                self.slow_threshold_ms,
                started,
                self.connection_id,
                &user,
                &stmt_hash,
                None,
                elapsed_ms,
                0,
                QueryOutcome::Error {
                    error_code: code.to_owned(),
                },
            );
            let database = self
                .db_handle
                .as_ref()
                .map(|h| h.database_name().to_owned());
            crate::metrics::query_executed(database.as_deref(), "error");
            self.queries_executed += 1;
            self.failed = true;
            self.send_failure(code, &message).await?;
            return Ok(false);
        }

        // Admin statements are dispatched separately so they emit
        // `admin_action` events rather than `query_exec`, and so that
        // store-level errors (last-admin, not-found, bad-password) map
        // to the correct Bolt error codes instead of the generic
        // `Neo.ClientError.Statement.ExecutionFailed`.
        //
        // Block 4 MVCC: inside an open explicit transaction, only the
        // read/write statement path (Query/Mutation/Pipeline/ConstReturn, which
        // the four `dispatch_*` route through the txn snapshot) is supported.
        // DDL/CALL/Admin operate outside that path (schema catalog, procedures,
        // system graph), so reject them with a clear error rather than run them
        // in auto-commit and silently break the transaction's isolation. This
        // guard runs before the Admin/DDL/CALL dispatch below so those never
        // execute while a transaction is open.
        if self.open_txn.is_some()
            && matches!(
                stmt,
                GqlStatement::Ddl(_) | GqlStatement::Call(_) | GqlStatement::Admin(_)
            )
        {
            self.failed = true;
            self.send_failure(
                "Neo.ClientError.Statement.NotSupported",
                "DDL, CALL, and administrative statements are not supported inside \
                 an explicit transaction; COMMIT or ROLLBACK first",
            )
            .await?;
            return Ok(false);
        }

        // SAFETY CONTRACT: this early return is the *only* code path that
        // consumes `GqlStatement::Admin`. The match arm below (~line 658)
        // uses `unreachable!()` as a defence-in-depth net; if anyone weakens
        // or removes this guard, the unreachable will fire at runtime
        // rather than silently producing wrong wire codes or skipping audit
        // emission. Do not remove this guard without removing the
        // unreachable arm and routing Admin through the match.
        if let GqlStatement::Admin(admin_stmt) = stmt {
            return self.dispatch_admin(admin_stmt).await;
        }

        // DDL statements (CREATE/DROP INDEX, CONSTRAINT, SHOW INDEX/CONSTRAINT
        // INFO) are dispatched before the main match — they need direct access
        // to the schema catalog on the Graph, which the query/mutation path
        // does not expose. This guard is the only consumer of
        // `GqlStatement::Ddl`; the match arm below uses `unreachable!()` as a
        // defence-in-depth net. DDL is its own gate, so it deliberately
        // bypasses the WRITE-gate check below (mirrors the Admin path).
        if let GqlStatement::Ddl(ddl_stmt) = stmt {
            return self.dispatch_ddl(ddl_stmt).await;
        }

        // CALL statements (built-in introspection procedures with an optional
        // UNWIND+RETURN pipeline) are dispatched before the main match — they
        // read the session's selected database graph directly, like DDL. This
        // guard is the only consumer of `GqlStatement::Call`; the match arm
        // below uses `unreachable!()` as a defence-in-depth net. CALL is
        // read-only, so it bypasses the WRITE-gate check below.
        if let GqlStatement::Call(call_stmt) = stmt {
            return self.dispatch_call(call_stmt).await;
        }

        // Spec §4.2 WRITE-gate: when a registry-backed session holds a
        // `Read` grant, refuse mutating statements before they reach
        // the engine. The check is dispatcher-side because the parser
        // is identity-agnostic. Sessions without a `db_handle` (legacy
        // single-database path) skip this branch and rely on the
        // existing engine-level semantics.
        if let Some(handle) = self.db_handle.as_ref()
            && handle.access_level() == AccessLevel::Read
            && ast_is_mutating(&stmt)
        {
            let database = handle.database_name().to_owned();
            let user = self.current_username().to_owned();
            self.failed = true;
            self.audit.access_denied(
                self.connection_id,
                Some(&user),
                AccessDeniedReason::WriteGateForbidden,
                Some(&database),
            );
            self.send_failure(
                "Neo.ClientError.Security.Forbidden",
                "write operation not permitted with READ-only grant",
            )
            .await?;
            return Ok(false);
        }

        // Capture the statement kind before the executor consumes
        // `stmt` so the histogram label is available regardless of
        // which arm runs. `Admin` is impossible here (the early-return
        // guard above dispatches it through `dispatch_admin`), but the
        // helper covers it for exhaustiveness.
        let stmt_kind = crate::metrics::gql_statement_kind(&stmt);
        tracing::Span::current().record("kind", stmt_kind);

        let exec_result: std::result::Result<PendingResult, String> = match stmt {
            GqlStatement::Query(ref q) => {
                // Flush pending implicit batch before reading so that
                // read-after-write on the same connection sees its own writes.
                if self.batch_state.dirty_count > 0 {
                    if let Err(e) = self.dispatch_end_batch() {
                        tracing::warn!(conn = self.connection_id, "flush before read failed: {e}");
                    }
                    self.batch_state.reset_dirty();
                }

                let cols = return_items_columns(&q.return_clause.items);
                self.dispatch_query(q, param_map)
                    .map(|rows| gql_result_to_pending_with_columns(&rows, &cols))
            }
            GqlStatement::Mutation(ref m) => {
                // Open an implicit batch before the first mutation so that
                // wal_sync inside add_node/etc is a no-op (batch_depth > 0).
                // Skipped inside an explicit transaction: transactional writes
                // go to the delta chain and touch no page/WAL until COMMIT, so
                // the auto-commit WAL-sync coalescing is inapplicable there.
                if self.open_txn.is_none() && self.batch_state.dirty_count == 0 {
                    tracing::debug!("opening implicit batch");
                    if let Err(e) = self.dispatch_begin_batch() {
                        // Batch open failed: subsequent mutations run with
                        // per-op WAL sync until the next begin_batch succeeds.
                        tracing::warn!(error = %e, "begin_batch failed — falling back to per-op WAL sync");
                    }
                }

                let result = self.dispatch_mutation(m, param_map).map(|(rows, stats)| {
                    if rows.is_empty() {
                        // Non-returning mutation (CREATE / SET / DELETE / bare
                        // MERGE): no data row. The counts travel in the `stats`
                        // dict of the final PULL SUCCESS metadata, matching Neo4j.
                        PendingResult {
                            fields_psv: Vec::new(),
                            rows: Vec::new(),
                            cursor: 0,
                            stats: Some(stats),
                        }
                    } else {
                        // MERGE ... RETURN var: project the returned rows using
                        // the column names carried in the first row, and still
                        // carry the counts so the driver's summary is correct.
                        let cols: Vec<String> = rows[0].keys().cloned().collect();
                        let mut pending = gql_result_to_pending_with_columns(&rows, &cols);
                        pending.stats = Some(stats);
                        pending
                    }
                });

                if result.is_ok() {
                    self.batch_state.mark_dirty();
                }

                // Auto-flush: if threshold reached, issue a single WAL sync
                // and re-open a new implicit batch for subsequent mutations.
                if self
                    .batch_state
                    .should_auto_flush(AUTO_FLUSH_OPS, AUTO_FLUSH_WINDOW)
                {
                    tracing::debug!(
                        dirty = self.batch_state.dirty_count,
                        "auto-flushing implicit batch"
                    );
                    if let Err(e) = self.dispatch_end_batch() {
                        // end_batch failure means the final WAL sync did not
                        // happen; leave `dirty_count` untouched so the caller
                        // keeps the in-memory view consistent with disk.
                        tracing::warn!(error = %e, "auto-flush end_batch failed");
                    } else {
                        self.batch_state.reset_dirty();
                    }
                    if let Err(e) = self.dispatch_begin_batch() {
                        tracing::warn!(error = %e, "auto-flush begin_batch failed — next mutations run with per-op WAL sync");
                    }
                }

                result
            }
            GqlStatement::Pipeline(ref pq) => {
                // Flush any pending implicit batch so read-after-write sees
                // its own writes, and so that a subsequent pipeline mutation
                // operates on a consistent view. Pipeline execution manages
                // locking internally (read lock for Return, write lock for
                // Set), so we don't open a new implicit batch here.
                if self.batch_state.dirty_count > 0 {
                    if let Err(e) = self.dispatch_end_batch() {
                        tracing::warn!(
                            conn = self.connection_id,
                            "flush before pipeline failed: {e}"
                        );
                    }
                    self.batch_state.reset_dirty();
                }

                // Derive preferred column order from the pipeline's RETURN
                // terminal if present; non-RETURN terminals (SET/CREATE/DELETE)
                // produce a counts-only row with a fixed schema.
                let cols: Vec<String> = match &pq.terminal {
                    ermya_graph::gql::PipelineTerminal::Return { clause, .. } => {
                        return_items_columns(&clause.items)
                    }
                    _ => Vec::new(),
                };

                self.dispatch_pipeline(pq, param_map).map(|(rows, stats)| {
                    if rows.is_empty() {
                        // Mutation terminal (SET): no data row. Counts travel in
                        // the `stats` dict of the final PULL SUCCESS metadata.
                        PendingResult {
                            fields_psv: Vec::new(),
                            rows: Vec::new(),
                            cursor: 0,
                            stats: Some(stats),
                        }
                    } else {
                        gql_result_to_pending_with_columns(&rows, &cols)
                    }
                })
            }
            // Admin statements are intercepted above and dispatched
            // through `dispatch_admin`; reaching this arm would mean a
            // regression in the early-return guard.
            GqlStatement::Admin(_) => {
                unreachable!("Admin statements must be handled by dispatch_admin")
            }
            GqlStatement::Ddl(_) => {
                unreachable!("DDL statements must be handled by dispatch_ddl")
            }
            GqlStatement::Call(_) => {
                unreachable!("CALL statements must be handled by dispatch_call")
            }
            // `RETURN <expr-list>` root statement — one row, evaluated
            // against an empty binding context. Flush any pending implicit
            // batch first so the row count appears in a consistent
            // serialised position relative to prior mutations on the same
            // connection (matters mostly for tests asserting ordering).
            GqlStatement::ConstReturn(ref c) => {
                if self.batch_state.dirty_count > 0 {
                    if let Err(e) = self.dispatch_end_batch() {
                        tracing::warn!(
                            conn = self.connection_id,
                            "flush before ConstReturn failed: {e}"
                        );
                    }
                    self.batch_state.reset_dirty();
                }
                let cols = return_items_columns(&c.items);
                self.dispatch_const_return(c, param_map)
                    .map(|rows| gql_result_to_pending_with_columns(&rows, &cols))
            }
        };

        let elapsed_ms = elapsed_ms(started);
        tracing::Span::current().record("duration_ms", elapsed_ms);
        {
            let user = self.current_username().to_owned();
            let database = self
                .db_handle
                .as_ref()
                .map(|h| h.database_name().to_owned());
            let outcome: &'static str = if exec_result.is_ok() {
                "success"
            } else {
                "error"
            };
            // Surface any drops from a window that closed between the
            // previous RUN and this one. Covers both terminal branches
            // below with a single poll (same `started` instant, one
            // possible warning per closed window).
            self.drain_slow_query_drops(started);
            if let Ok(pending) = &exec_result {
                let row_count = pending.rows.len() as u64;
                self.audit.emit_query_pair(
                    &mut self.slow_gate,
                    self.slow_threshold_ms,
                    started,
                    self.connection_id,
                    &user,
                    &stmt_hash,
                    database.as_deref(),
                    elapsed_ms,
                    row_count,
                    QueryOutcome::Success,
                );
            } else {
                self.audit.emit_query_pair(
                    &mut self.slow_gate,
                    self.slow_threshold_ms,
                    started,
                    self.connection_id,
                    &user,
                    &stmt_hash,
                    database.as_deref(),
                    elapsed_ms,
                    0,
                    QueryOutcome::Error {
                        error_code: "Neo.ClientError.Statement.ExecutionFailed".to_owned(),
                    },
                );
            }
            // Mirror the audit emission on the metrics side so dashboards
            // and the audit log agree on per-statement counts. The
            // histogram observation is unconditional: the executor ran
            // long enough to produce either rows or an executor error,
            // so the duration is meaningful in both cases.
            crate::metrics::query_executed(database.as_deref(), outcome);
            #[allow(clippy::cast_precision_loss)]
            let secs = (elapsed_ms as f64) / 1000.0;
            crate::metrics::query_duration(database.as_deref(), stmt_kind, secs);
        }
        self.queries_executed += 1;

        match exec_result {
            Ok(pending) => {
                let fields = pending.fields_psv.clone();
                self.pending_result = Some(pending);

                self.send_response(&BoltResponse::Success {
                    metadata: vec![("fields".to_owned(), PackStreamValue::List(fields))],
                })
                .await?;
            }
            Err(msg) => {
                self.failed = true;
                // Task 15: a `ermya_graph::Error::QuotaExceeded`
                // surfaces from the executor with a sentinel prefix
                // (see graph_accessor::engine_err_to_string). Map it
                // to the dedicated Bolt wire code; otherwise fall
                // back to the generic ExecutionFailed.
                let (code, wire) = if let Some(rest) =
                    msg.strip_prefix(crate::graph_accessor::ENGINE_RESULT_CAPPED_PREFIX)
                {
                    // Task 4: query aborted by the defensive result-row cap
                    // (Cap A match-count guard or Cap B output guard). Same
                    // sentinel-stripping treatment as the quota case. Emit a
                    // dedicated metric + audit event so operators can tell a
                    // cap abort apart from any other execution failure; the
                    // row count is parsed from the abort message tail.
                    let database = self
                        .db_handle
                        .as_ref()
                        .map(|h| h.database_name().to_owned());
                    let user = self.current_username().to_owned();
                    let row_count_seen = parse_capped_row_count(rest);
                    crate::metrics::result_capped(database.as_deref());
                    self.audit.result_capped(
                        self.connection_id,
                        Some(&user),
                        &stmt_hash,
                        row_count_seen,
                        self.max_result_rows,
                        database.as_deref(),
                    );
                    (
                        "Neo.ClientError.General.ResultExhausted",
                        sanitize_engine_error_for_wire(rest),
                    )
                } else if let Some(rest) =
                    msg.strip_prefix(crate::graph_accessor::ENGINE_QUERY_TIMEOUT_PREFIX)
                {
                    // Task 6: query aborted by the cooperative per-query
                    // timeout. Emit a dedicated metric + audit event so
                    // operators can tell a timeout apart from any other
                    // execution failure. Surfaced as a non-retryable
                    // `ClientError` (ExecutionFailed) so the driver does not
                    // re-run the same expensive query.
                    let database = self
                        .db_handle
                        .as_ref()
                        .map(|h| h.database_name().to_owned());
                    let user = self.current_username().to_owned();
                    let timeout_ms = self
                        .query_timeout
                        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX));
                    crate::metrics::query_timed_out(database.as_deref());
                    self.audit
                        .query_timed_out(crate::audit::QueryTimedOutDetails {
                            connection_id: self.connection_id,
                            user,
                            statement_sha256: stmt_hash.clone(),
                            database,
                            timeout_ms,
                        });
                    (
                        "Neo.ClientError.Statement.ExecutionFailed",
                        sanitize_engine_error_for_wire(rest),
                    )
                } else {
                    // The remaining sentinels carry no side effects, so their
                    // mapping lives in a pure function that can be pinned by a
                    // unit test without driving a whole Bolt session.
                    map_sideeffect_free_engine_error(&msg)
                };
                self.send_failure(code, &wire).await?;
            }
        }

        Ok(false)
    }

    async fn handle_pull(&mut self, extra: &[(String, PackStreamValue)]) -> Result<bool> {
        // v0.6.0 Fase 2 Task 5 eje 2 — consume one query token before
        // serving rows. PULL is a continuation of a query, so it counts
        // against the same per-connection budget as RUN.
        {
            let now = std::time::Instant::now();
            if !self.query_bucket.try_take(1, now) {
                let tokens_available = self.query_bucket.available(now);
                let user = self.current_username().to_owned();
                let database = self
                    .db_handle
                    .as_ref()
                    .map(|h| h.database_name().to_owned());
                let stmt_hash = self.last_stmt_hash.clone().unwrap_or_default();
                crate::metrics::rate_limit_hit("query_conn");
                self.audit
                    .query_throttled(crate::audit::QueryThrottledDetails {
                        connection_id: self.connection_id,
                        user,
                        statement_sha256: stmt_hash,
                        database,
                        tokens_available,
                    });
                self.failed = true;
                self.send_failure(
                    "Neo.ClientError.Security.TooManyRequests",
                    &format!(
                        "query rate limit exceeded ({} queries/s cap)",
                        self.queries_max_per_second
                    ),
                )
                .await?;
                return Ok(false);
            }
        }
        // Bolt 4.4 incremental fetch: serve up to `n` rows from
        // `rows[cursor..]`, advance the cursor, and report `has_more`
        // while rows remain. `n = -1` (or absent) means "all remaining"
        // — the legacy `PULL {}` contract. The `PendingResult` survives
        // across PULLs (cursor model), so rows are cloned rather than
        // moved; they are already bounded by the result-row cap (C4).
        let Some(pending) = self.pending_result.as_mut() else {
            self.send_response(&BoltResponse::Success {
                metadata: vec![("has_more".to_owned(), PackStreamValue::Bool(false))],
            })
            .await?;
            return Ok(false);
        };

        let n = read_n(extra);
        let remaining = pending.rows.len() - pending.cursor;
        let take = if n < 0 {
            remaining
        } else {
            (usize::try_from(n).unwrap_or(0)).min(remaining)
        };
        let end = pending.cursor + take;

        // Write Records without flushing — single flush at the end keeps
        // the syscall count at 1 for the whole batch.
        for row in &pending.rows[pending.cursor..end] {
            let data = encode_response(&BoltResponse::Record {
                fields: row.clone(),
            })?;
            self.writer
                .write_message_no_flush(&data)
                .await
                .map_err(crate::error::ServerError::Io)?;
        }
        pending.cursor = end;
        let has_more = pending.cursor < pending.rows.len();
        // On the final PULL (no more rows), capture the mutation counters so
        // they can be emitted as the Neo4j-style `stats` dict below. Neo4j only
        // sends stats in the last SUCCESS, so intermediate PULLs carry none.
        let stats_snapshot = if has_more { None } else { pending.stats.take() };
        if !has_more {
            self.pending_result = None;
        }

        let mut metadata = vec![("has_more".to_owned(), PackStreamValue::Bool(has_more))];
        if let Some(stats) = stats_snapshot {
            metadata.push(("stats".to_owned(), mutation_stats_to_dict(&stats)));
        }
        self.send_response(&BoltResponse::Success { metadata })
            .await?;

        Ok(false)
    }

    async fn handle_discard(&mut self, extra: &[(String, PackStreamValue)]) -> Result<bool> {
        // v0.6.0 Fase 2 Task 5 eje 2 — consume one query token before
        // discarding rows. DISCARD is a continuation of a query, so it
        // counts against the same per-connection budget as RUN.
        {
            let now = std::time::Instant::now();
            if !self.query_bucket.try_take(1, now) {
                let tokens_available = self.query_bucket.available(now);
                let user = self.current_username().to_owned();
                let database = self
                    .db_handle
                    .as_ref()
                    .map(|h| h.database_name().to_owned());
                let stmt_hash = self.last_stmt_hash.clone().unwrap_or_default();
                crate::metrics::rate_limit_hit("query_conn");
                self.audit
                    .query_throttled(crate::audit::QueryThrottledDetails {
                        connection_id: self.connection_id,
                        user,
                        statement_sha256: stmt_hash,
                        database,
                        tokens_available,
                    });
                self.failed = true;
                self.send_failure(
                    "Neo.ClientError.Security.TooManyRequests",
                    &format!(
                        "query rate limit exceeded ({} queries/s cap)",
                        self.queries_max_per_second
                    ),
                )
                .await?;
                return Ok(false);
            }
        }
        // Cursor-aware DISCARD: drop up to `n` pending rows by advancing
        // the cursor without serving them, and report `has_more` while
        // rows remain. `n = -1` (or absent) discards everything — the
        // legacy `DISCARD {}` contract.
        let Some(pending) = self.pending_result.as_mut() else {
            self.send_response(&BoltResponse::Success {
                metadata: vec![("has_more".to_owned(), PackStreamValue::Bool(false))],
            })
            .await?;
            return Ok(false);
        };

        let n = read_n(extra);
        let remaining = pending.rows.len() - pending.cursor;
        let drop = if n < 0 {
            remaining
        } else {
            (usize::try_from(n).unwrap_or(0)).min(remaining)
        };
        pending.cursor += drop;
        let has_more = pending.cursor < pending.rows.len();
        // DISCARD to exhaustion still surfaces the mutation counters: a driver
        // may consume a result without pulling rows (`ConsumeAsync`), so the
        // `stats` dict must ride the final DISCARD SUCCESS just like PULL.
        let stats_snapshot = if has_more { None } else { pending.stats.take() };
        if !has_more {
            self.pending_result = None;
        }

        let mut metadata = vec![("has_more".to_owned(), PackStreamValue::Bool(has_more))];
        if let Some(stats) = stats_snapshot {
            metadata.push(("stats".to_owned(), mutation_stats_to_dict(&stats)));
        }
        self.send_response(&BoltResponse::Success { metadata })
            .await?;
        Ok(false)
    }

    async fn handle_reset(&mut self) -> Result<bool> {
        self.failed = false;
        self.pending_result = None;
        // RESET returns the session to a clean state: an open explicit
        // transaction is abandoned (rolled back), matching how a driver expects
        // RESET to clear in-flight work.
        if let Some(txn_id) = self.open_txn.take()
            && let Some(accessor) = self.session_accessor.as_deref()
            && let Err(e) = accessor.rollback_txn(txn_id)
        {
            tracing::warn!(
                conn = self.connection_id,
                "rollback of open txn during RESET failed: {e}"
            );
        }
        if let Err(e) = self.flush_pending_batch() {
            tracing::warn!(conn = self.connection_id, "flush during RESET failed: {e}");
        }
        self.send_response(&BoltResponse::Success { metadata: vec![] })
            .await?;
        Ok(false)
    }

    /// Handles `BEGIN`: opens an explicit transaction on the session's bound
    /// database and stores its `txn_id` so subsequent statements know a
    /// transaction is open.
    ///
    /// A `BEGIN` before the session has bound a database (no `RUN` yet) has no
    /// graph to open the transaction on, so it fails with `Request.Invalid`
    /// rather than panicking — the driver must select a database first.
    async fn handle_begin(&mut self) -> Result<bool> {
        if self.open_txn.is_some() {
            self.failed = true;
            self.send_failure(
                "Neo.ClientError.Request.Invalid",
                "a transaction is already open on this session",
            )
            .await?;
            return Ok(false);
        }
        let Some(accessor) = self.session_accessor.as_deref() else {
            self.failed = true;
            self.send_failure(
                "Neo.ClientError.Request.Invalid",
                "no database selected; run a statement selecting a database before BEGIN",
            )
            .await?;
            return Ok(false);
        };
        match accessor.begin_txn() {
            Ok(txn_id) => {
                self.open_txn = Some(txn_id);
                self.send_response(&BoltResponse::Success { metadata: vec![] })
                    .await?;
            }
            Err(e) => {
                self.failed = true;
                let (code, message) = map_txn_error(&e);
                self.send_failure(code, &message).await?;
            }
        }
        Ok(false)
    }

    /// Handles `COMMIT`: commits the session's open transaction, making its
    /// writes visible, and clears the open-transaction state. Fails with
    /// `Request.Invalid` when no transaction is open (no prior `BEGIN`).
    async fn handle_commit(&mut self) -> Result<bool> {
        self.finish_txn(true).await
    }

    /// Handles `ROLLBACK`: discards the session's open transaction and clears
    /// the open-transaction state. Fails with `Request.Invalid` when no
    /// transaction is open.
    async fn handle_rollback(&mut self) -> Result<bool> {
        self.finish_txn(false).await
    }

    /// Shared body of `COMMIT`/`ROLLBACK`: resolve the open transaction, apply
    /// commit-or-rollback, and clear `open_txn`. `commit == true` commits;
    /// `false` rolls back.
    async fn finish_txn(&mut self, commit: bool) -> Result<bool> {
        let Some(txn_id) = self.open_txn else {
            self.failed = true;
            self.send_failure(
                "Neo.ClientError.Request.Invalid",
                "no open transaction; send BEGIN first",
            )
            .await?;
            return Ok(false);
        };
        // Clear the session state up front: whether the engine call succeeds or
        // fails, the transaction is no longer the session's open one.
        self.open_txn = None;
        let accessor = self
            .session_accessor
            .as_deref()
            .expect("open_txn implies a bound session accessor");
        let outcome = if commit {
            accessor.commit_txn(txn_id)
        } else {
            accessor.rollback_txn(txn_id)
        };
        match outcome {
            Ok(()) => {
                self.send_response(&BoltResponse::Success { metadata: vec![] })
                    .await?;
            }
            Err(e) => {
                self.failed = true;
                let (code, message) = map_txn_error(&e);
                self.send_failure(code, &message).await?;
            }
        }
        Ok(false)
    }

    /// Sirve una sentencia administrativa por el punto de extensión.
    ///
    /// Enforces the admin-flag gate before calling the dispatcher so
    /// non-admin users see `Neo.ClientError.Security.Forbidden` and the
    /// audit trail records a `Denied` action with the attempted
    /// statement (redacted via `Debug`, which never prints passwords).
    ///
    /// Returns `Ok(false)` in all cases so the handler loop continues —
    /// even on failure, because Bolt allows a new RUN after a Failure
    /// provided the client has RESET or the handler surfaces it as a
    /// non-fatal error.
    async fn dispatch_admin(&mut self, stmt: ermya_graph::gql::AdminStatement) -> Result<bool> {
        let Some(user) = self.authenticated_user.clone() else {
            self.failed = true;
            self.send_failure("Neo.ClientError.Security.Unauthorized", "not authenticated")
                .await?;
            return Ok(false);
        };

        // El manejador no sabe qué edición está sirviendo: entrega la
        // sentencia al despachador que le inyectaron al montar el servidor y
        // ya. Antes decidía aquí —preguntaba si la sentencia era de pago,
        // miraba si había gestor concreto y elegía entre dos despachadores—,
        // que es tres decisiones sobre ediciones metidas en el camino que
        // sirve las consultas normales.
        //
        // Cada edición monta el suyo: el público sirve las seis de cuentas y
        // falla cerrado en las demás; el de pago sirve las doce.
        let dispatched = self
            .admin_dispatcher
            .dispatch(
                stmt,
                crate::admin_dispatch::AdminCaller {
                    username: &user.username,
                    is_admin: user.is_admin,
                    connection_id: self.connection_id,
                },
                &self.audit,
            )
            .await;

        match dispatched {
            Ok(pending) => {
                let fields_psv = pending.fields_psv;
                let rows = pending.rows;
                let fields_for_response = fields_psv.clone();
                self.pending_result = Some(PendingResult {
                    fields_psv,
                    rows,
                    cursor: 0,
                    stats: None,
                });
                self.send_response(&BoltResponse::Success {
                    metadata: vec![(
                        "fields".to_owned(),
                        PackStreamValue::List(fields_for_response),
                    )],
                })
                .await?;
            }
            Err((code, message)) => {
                self.failed = true;
                self.send_failure(&code, &message).await?;
            }
        }
        Ok(false)
    }

    /// Dispatch a DDL statement (CREATE/DROP INDEX/CONSTRAINT, SHOW
    /// INDEX/CONSTRAINT INFO) against the session's selected database graph.
    ///
    /// MULTI-TENANT (verified 2026-06-15): the registry owns one
    /// `Arc<RwLock<Graph>>` per open database, and `SchemaCatalog` lives ON
    /// the `Graph`, so DDL MUST target the SESSION's selected database (set via
    /// the driver's `WithDatabase("plantA")`), NOT a default graph — otherwise
    /// tenant A's `CREATE CONSTRAINT` would land in tenant B's catalog. The
    /// session accessor resolves to the selected DB's graph; DDL is reached only
    /// after a successful per-RUN bind, so the accessor is always present.
    async fn dispatch_ddl(&mut self, stmt: ermya_graph::gql::DdlStatement) -> Result<bool> {
        let graph_arc = self.current_accessor().graph_arc();
        match crate::ddl_handler::dispatch_ddl(stmt, &graph_arc) {
            Ok(pending) => {
                let fields_psv = pending.fields_psv;
                let rows = pending.rows;
                let fields_for_response = fields_psv.clone();
                self.pending_result = Some(PendingResult {
                    fields_psv,
                    rows,
                    cursor: 0,
                    stats: None,
                });
                self.send_response(&BoltResponse::Success {
                    metadata: vec![(
                        "fields".to_owned(),
                        PackStreamValue::List(fields_for_response),
                    )],
                })
                .await?;
            }
            Err((code, message)) => {
                self.failed = true;
                self.send_failure(&code, &message).await?;
            }
        }
        Ok(false)
    }

    /// Dispatch a CALL statement against the session's selected database graph.
    ///
    /// MULTI-TENANT: uses `session_accessor.graph_arc()` when a database is
    /// selected (identical to `dispatch_ddl`), falling back to the default graph
    /// for single-database sessions. The built-in procedures
    /// (`vertex_labels`/`edge_types`) read only that database's label indexes.
    async fn dispatch_call(&mut self, stmt: Box<ermya_graph::gql::CallStatement>) -> Result<bool> {
        use ermya_graph::call::{ProcedureKind, resolve_procedure};

        // Los procedimientos de copia en caliente son asíncronos, cuelgan del
        // gestor concreto y sólo los sirve un administrador — no pasan por el
        // despachador de llamadas, que es síncrono y de sólo lectura.
        // `resolve_procedure` es la única fuente de nombres de procedimiento,
        // compartida con ese despachador.
        //
        // **Quién los sirve no se decide aquí**: se pregunta a la edición. La
        // de pago los atiende; la pública no tiene con qué —ni gestor concreto
        // ni ficheros de inquilino que copiar— y los rechaza diciéndolo. Antes
        // este fichero llamaba directamente a la vía de pago, y como viaja al
        // árbol público, allí nombraba un método que no existe.
        let kind = resolve_procedure(stmt.namespace.as_deref(), &stmt.procedure);
        if matches!(kind, Some(ProcedureKind::Snapshot | ProcedureKind::Restore)) {
            return self.dispatch_registry_scoped_call(kind, &stmt).await;
        }

        let graph_arc = self.current_accessor().graph_arc();
        match crate::call_handler::dispatch_call(&stmt, &graph_arc, self.max_result_rows) {
            Ok(pending) => {
                let fields_psv = pending.fields_psv;
                let rows = pending.rows;
                let fields_for_response = fields_psv.clone();
                self.pending_result = Some(PendingResult {
                    fields_psv,
                    rows,
                    cursor: 0,
                    stats: None,
                });
                self.send_response(&BoltResponse::Success {
                    metadata: vec![(
                        "fields".to_owned(),
                        PackStreamValue::List(fields_for_response),
                    )],
                })
                .await?;
            }
            Err((code, message)) => {
                self.failed = true;
                self.send_failure(&code, &message).await?;
            }
        }
        Ok(false)
    }

    // ── Session-aware graph dispatchers ────────────────────────────────
    //
    // Every RUN executes against the per-session `session_accessor` (wrapping
    // `db_handle.graph()`, spec §4.2). It is populated by `try_bind_database`
    // on the first RUN; these dispatchers are only reached AFTER a successful
    // bind, so `current_accessor` expects it to be present. One thin dispatcher
    // per trait method keeps the call sites uniform.

    /// The bound session accessor. Panics only on a logic error: the
    /// query/mutation dispatchers run strictly after `try_bind_database`
    /// installed it, so a `None` here means the bind-gate was bypassed.
    fn current_accessor(&self) -> &dyn GraphAccessor {
        self.session_accessor
            .as_deref()
            .expect("session accessor must be bound before dispatch (try_bind_database)")
    }

    /// v0.6.0 Fase 2 Task 6 — compute this RUN's query-timeout deadline.
    ///
    /// Returns `Some(Instant::now() + query_timeout)` when the per-query
    /// timeout is configured, else `None` (disabled). Called once per dispatch
    /// so the clock is read at execution start, not at handler construction.
    fn compute_deadline(&self) -> Option<Instant> {
        self.query_timeout.map(|d| Instant::now() + d)
    }

    fn dispatch_query(
        &self,
        q: &ermya_graph::gql::GqlQuery,
        params: HashMap<String, ermya_graph::gql::GqlValue>,
    ) -> std::result::Result<Vec<crate::graph_accessor::ResultRow>, String> {
        let deadline = self.compute_deadline();
        match self.open_txn {
            Some(txn_id) => self.current_accessor().execute_query_in_txn(
                txn_id,
                q,
                params,
                self.max_result_rows,
                deadline,
            ),
            None => {
                self.current_accessor()
                    .execute_query(q, params, self.max_result_rows, deadline)
            }
        }
    }

    fn dispatch_mutation(
        &self,
        m: &ermya_graph::gql::MutationStatement,
        params: HashMap<String, ermya_graph::gql::GqlValue>,
    ) -> std::result::Result<
        (
            Vec<crate::graph_accessor::ResultRow>,
            ermya_graph::gql::GqlMutationResult,
        ),
        String,
    > {
        let deadline = self.compute_deadline();
        match self.open_txn {
            Some(txn_id) => self
                .current_accessor()
                .execute_mutation_in_txn(txn_id, m, params, deadline),
            None => self
                .current_accessor()
                .execute_mutation(m, params, deadline),
        }
    }

    fn dispatch_pipeline(
        &self,
        pq: &ermya_graph::gql::PipelineQuery,
        params: HashMap<String, ermya_graph::gql::GqlValue>,
    ) -> std::result::Result<
        (
            Vec<crate::graph_accessor::ResultRow>,
            ermya_graph::gql::GqlMutationResult,
        ),
        String,
    > {
        let deadline = self.compute_deadline();
        match self.open_txn {
            Some(txn_id) => self.current_accessor().execute_pipeline_in_txn(
                txn_id,
                pq,
                params,
                self.max_result_rows,
                deadline,
            ),
            None => {
                self.current_accessor()
                    .execute_pipeline(pq, params, self.max_result_rows, deadline)
            }
        }
    }

    fn dispatch_const_return(
        &self,
        c: &ermya_graph::gql::ConstReturnQuery,
        params: HashMap<String, ermya_graph::gql::GqlValue>,
    ) -> std::result::Result<Vec<crate::graph_accessor::ResultRow>, String> {
        let deadline = self.compute_deadline();
        match self.open_txn {
            Some(txn_id) => self.current_accessor().execute_const_return_in_txn(
                txn_id,
                c,
                params,
                self.max_result_rows,
                deadline,
            ),
            None => self.current_accessor().execute_const_return(
                c,
                params,
                self.max_result_rows,
                deadline,
            ),
        }
    }

    fn dispatch_begin_batch(&self) -> std::result::Result<(), String> {
        self.current_accessor().begin_batch()
    }

    fn dispatch_end_batch(&self) -> std::result::Result<(), String> {
        self.current_accessor().end_batch()
    }

    /// Flush any outstanding implicit batch so mutations are durable.
    fn flush_pending_batch(&mut self) -> std::result::Result<(), String> {
        if self.batch_state.dirty_count > 0 {
            let result = self.dispatch_end_batch();
            self.batch_state.reset_dirty();
            return result;
        }
        Ok(())
    }

    // ── Response helpers ────────────────────────────────────────────────

    pub(crate) async fn send_response(&mut self, resp: &BoltResponse) -> Result<()> {
        let data = encode_response(resp)?;
        self.writer.write_message(&data).await?;
        Ok(())
    }

    async fn send_failure(&mut self, code: &str, message: &str) -> Result<()> {
        self.send_response(&BoltResponse::Failure {
            metadata: vec![
                ("code".to_owned(), PackStreamValue::String(code.to_owned())),
                (
                    "message".to_owned(),
                    PackStreamValue::String(message.to_owned()),
                ),
            ],
        })
        .await
    }

    async fn send_ignored(&mut self) -> Result<()> {
        self.send_response(&BoltResponse::Ignored).await
    }

    pub(crate) fn current_username(&self) -> &str {
        self.authenticated_user
            .as_ref()
            .map_or("", |u| u.username.as_str())
    }

    /// Marca la sesión como fallida y manda el fallo.
    ///
    /// Único punto de la puerta hacia el gemelo de edición que esta edición
    /// necesita: el fichero público que responde a los procedimientos de copia
    /// en caliente lo usa para rechazarlos. Los demás métodos de aquella puerta
    /// sólo servían a las vías de pago y no viajan.
    ///
    /// Las dos cosas van juntas —marcar y mandar— para que ninguna rama futura
    /// mande el fallo y se olvide de marcar.
    pub(crate) async fn fail_with(&mut self, code: &str, message: &str) -> Result<()> {
        self.failed = true;
        self.send_failure(code, message).await
    }
}

impl<S, A: ?Sized> Drop for BoltHandler<S, A>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
    A: AuthProvider,
{
    /// v0.6.0 Fase 2 Task 3 — final flush of the throttle warning so a
    /// connection closing mid-window still surfaces the slow-query
    /// events it dropped. Synthesises a `now` past the 60-second window
    /// so `closed_window_drops` reports unconditionally when drops
    /// accumulated; a window with zero drops stays silent.
    fn drop(&mut self) {
        self.drain_slow_query_drops(std::time::Instant::now() + std::time::Duration::from_secs(61));
        // v0.6.0 Fase 2 Task 5 eje 4 — one aggregate bandwidth-throttle
        // audit entry per connection, emitted only when the cap actually
        // slowed I/O at least once. Silent when the cap never bit.
        let total_sleeps = self.bandwidth.sleep_count();
        if total_sleeps > 0 {
            self.audit
                .bandwidth_throttled(crate::audit::BandwidthThrottledDetails {
                    connection_id: self.connection_id,
                    total_sleeps,
                    total_sleep_duration_ms: self.bandwidth.total_sleep_ms(),
                });
        }
    }
}

/// Lightweight validator for the `db` field of RUN `extra` (Task 10-bis
/// moved the routing trigger from HELLO to the first RUN).
///
/// Equivalent to `validate_database_name` in
/// `crates/ermya-graph-server/src/auth/system_graph.rs` (intentionally
/// private to that module). The plan authorises duplication here so the
/// auth-store helper does not have to leak through the public surface
/// for the sole benefit of bind-time routing — promoting it to `pub`
/// would outlive the WRITE-gate path and erode the boundary between
/// auth storage and protocol handling.
///
/// Rules per spec §6.1: `^[a-zA-Z_][a-zA-Z0-9_-]{0,62}$`. Reserved
/// names: `system`, `default`. Case-sensitive comparison — Bolt clients
/// see whatever they sent.
fn is_valid_database_name(name: &str) -> bool {
    if matches!(name, "system" | "default") {
        return false;
    }
    if name.is_empty() || name.len() > 63 {
        return false;
    }
    let mut chars = name.chars();
    let first = chars.next().expect("non-empty (checked above)");
    if !(first.is_ascii_alphabetic() || first == '_') {
        return false;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Spec §4.2: returns `true` when executing the statement would write
/// to the underlying graph. Used by the RUN dispatcher to gate
/// mutating statements against `AccessLevel::Read` grants before
/// touching the engine.
///
/// `Mutation` is unconditionally mutating. `Pipeline` mutates iff its
/// terminal is `Set`/`Create`/`Delete` — `Return` is read-only.
/// `Query` is read-only by construction. `Admin` never reaches this
/// helper: the dispatcher routes admin statements to
/// `dispatch_admin` first, where authorization is checked against
/// `is_admin` rather than per-database grants.
fn ast_is_mutating(stmt: &ermya_graph::gql::GqlStatement) -> bool {
    use ermya_graph::gql::{GqlStatement, PipelineTerminal};
    match stmt {
        GqlStatement::Mutation(_) => true,
        GqlStatement::Pipeline(p) => !matches!(p.terminal, PipelineTerminal::Return { .. }),
        GqlStatement::Query(_)
        | GqlStatement::Admin(_)
        | GqlStatement::ConstReturn(_)
        | GqlStatement::Ddl(_)
        // CALL invokes read-only introspection procedures (vertex_labels /
        // edge_types) — never mutates the graph.
        | GqlStatement::Call(_) => false,
    }
}

/// Maps the engine-error sentinels that carry no side effects to their Bolt
/// wire code, falling back to the generic execution failure.
///
/// The sentinel-bearing branches that also emit metrics or audit events stay
/// inline in `handle_run`; only these pure ones live here, so the mapping can
/// be pinned by a unit test rather than only end-to-end through a Bolt session.
/// The returned message has the internal sentinel stripped and is scrubbed by
/// [`sanitize_engine_error_for_wire`].
#[must_use]
pub fn map_sideeffect_free_engine_error(msg: &str) -> (&'static str, String) {
    if let Some(rest) = msg.strip_prefix(crate::graph_accessor::ENGINE_QUOTA_EXCEEDED_PREFIX) {
        // The sentinel is internal; clients see the human-readable Display form
        // only (after the colon-space delimiter that follows the prefix).
        // `sanitize_engine_error_for_wire` still runs to scrub any incidental
        // file paths.
        (
            "Neo.ClientError.General.StorageExhausted",
            sanitize_engine_error_for_wire(rest),
        )
    } else if let Some(rest) =
        msg.strip_prefix(crate::graph_accessor::ENGINE_CONSTRAINT_VIOLATED_PREFIX)
    {
        // 3c: a write rejected by a unique constraint. The Neo4j/.NET driver
        // expects this dedicated schema wire code so the application can
        // distinguish a uniqueness failure from a generic execution error.
        (
            "Neo.ClientError.Schema.ConstraintValidationFailed",
            sanitize_engine_error_for_wire(rest),
        )
    } else if let Some(rest) = msg.strip_prefix(crate::graph_accessor::ENGINE_TXN_MEMORY_CAP_PREFIX)
    {
        // Issue #43 A11: the transaction outgrew its memory cap and was rolled
        // back. Reported as TRANSIENT on purpose — the driver may retry, and
        // the same work split into smaller transactions can succeed.
        (
            "Neo.TransientError.General.MemoryPoolOutOfMemory",
            sanitize_engine_error_for_wire(rest),
        )
    } else if let Some(rest) = msg.strip_prefix(crate::graph_accessor::ENGINE_BATCH_LIMIT_PREFIX) {
        // Issue #43 A12: the batch exceeded its cap. NOT transient — replaying
        // the same oversized batch fails identically; the client must split it.
        (
            "Neo.ClientError.Request.Invalid",
            sanitize_engine_error_for_wire(rest),
        )
    } else if let Some(rest) =
        msg.strip_prefix(crate::graph_accessor::ENGINE_DELETE_CONNECTED_PREFIX)
    {
        // Issue #43 A10: deleting a node that still has relationships, without
        // DETACH. Neo4j reports this as a constraint violation, and so do we —
        // it is an integrity violation, not a transient execution failure the
        // driver should consider retrying.
        (
            "Neo.ClientError.Schema.ConstraintValidationFailed",
            sanitize_engine_error_for_wire(rest),
        )
    } else if let Some(rest) =
        msg.strip_prefix(crate::graph_accessor::ENGINE_APPEND_ONLY_IN_TXN_PREFIX)
    {
        // Issue #43: a write inside a transaction against an append-only label.
        // The request itself is invalid — no amount of retrying makes it
        // succeed — so the client is told that rather than being given a
        // generic execution failure it might reasonably retry.
        (
            "Neo.ClientError.Request.Invalid",
            sanitize_engine_error_for_wire(rest),
        )
    } else {
        (
            "Neo.ClientError.Statement.ExecutionFailed",
            sanitize_engine_error_for_wire(msg),
        )
    }
}

/// Redact engine error strings before they reach the wire. The
/// accessor and engine emit messages that include implementation
/// details (Rust lock state, host filesystem paths, opaque internal
/// IDs) which have no business reaching a remote client. The audit
/// log keeps the original detail; this helper governs what the Bolt
/// FAILURE message field carries.
///
/// Rules (first match wins):
///
/// - Mentions `"poisoned"` / `"corrupt"` / `"checksum"` → flatten to
///   `"internal storage error"`. These are subsystem-level failures
///   the operator must investigate; the client cannot act on the
///   detail and the detail itself (paths, page numbers) is sensitive.
/// - Contains an internal ID token (`NodeId(`, `EdgeId(`, `PageId(`,
///   `LSN(`) → flatten to `"query execution failed"`. Numeric IDs are
///   meaningful only with disk access; surfacing them weakens
///   abstractions a tenant should not pierce.
/// - Otherwise: pass through. GQL compile / unsupported / mutation
///   errors and pattern-variable diagnostics are user-facing and
///   stay verbatim so the client can act on them.
///
/// `pub` (and re-exported from the crate root) so integration tests
/// can pin the contract without round-tripping through Bolt.
#[must_use]
pub fn sanitize_engine_error_for_wire(message: &str) -> String {
    if message.contains("poisoned") || message.contains("corrupt") || message.contains("checksum") {
        return "internal storage error".to_owned();
    }
    if message.contains("NodeId(")
        || message.contains("EdgeId(")
        || message.contains("PageId(")
        || message.contains("LSN(")
    {
        return "query execution failed".to_owned();
    }
    message.to_owned()
}

/// Map a [`RegistryError`] to an [`AccessDeniedReason`] when the
/// failure represents an authorization denial worth recording in the
/// audit log. Resource exhaustion / availability failures
/// (`TransactionResourceExhausted`, `OpenCapExceeded`,
/// `DatabaseUnavailable`, `Io`, `AuthStore`, `StorageExhausted`,
/// `DatabaseInUse`) are not access-denied events — they have their
/// own observability story (counters in `RegistryStats`,
/// transient-error wire codes) — so this returns `None` for them.
const fn registry_error_to_access_denied_reason(e: &RegistryError) -> Option<AccessDeniedReason> {
    match e {
        RegistryError::Unauthorized => Some(AccessDeniedReason::Unauthorized),
        RegistryError::DatabaseNotFound(_) => Some(AccessDeniedReason::DatabaseNotFound),
        RegistryError::StorageExhausted(_)
        | RegistryError::DatabaseUnavailable(_)
        | RegistryError::Io(_)
        | RegistryError::AuthStore(_) => None,
    }
}

/// Map a [`RegistryError`] to the Bolt wire `code`.
///
/// Mapping table per spec §8 of
/// `docs/specs/2026-04-23-multi-database-design.md`. Kept
/// next to the handler so a reviewer can check the wire contract
/// without crossing module boundaries; pinned by an exhaustive unit
/// test in this file's `tests` module.
pub(crate) const fn registry_error_to_bolt_code(e: &RegistryError) -> &'static str {
    match e {
        RegistryError::Unauthorized => "Neo.ClientError.Security.Unauthorized",
        RegistryError::DatabaseNotFound(_) => "Neo.ClientError.Database.DatabaseNotFound",
        RegistryError::StorageExhausted(_) => "Neo.ClientError.General.StorageExhausted",
        RegistryError::DatabaseUnavailable(_)
        | RegistryError::Io(_)
        | RegistryError::AuthStore(_) => "Neo.TransientError.Database.DatabaseUnavailable",
    }
}

/// Sanitise a [`RegistryError`] for the Bolt wire `message` field.
///
/// `RegistryError::Display` embeds storage internals (file paths from
/// `Graph::open` failures, the configured `max_open_databases` cap,
/// active session counts, auth-store error chains). None of that is
/// safe to echo back to a connecting client — it is a fingerprint of
/// the server's internals. The full error is preserved by the audit
/// log and `tracing::error` calls upstream; this helper produces the
/// trimmed form for the wire only.
///
/// Variants whose Display is intrinsically client-controlled
/// (`Unauthorized`, `DatabaseNotFound(<name>)` where `<name>` was
/// supplied in HELLO) pass through verbatim.
pub(crate) fn registry_error_to_wire_message(e: &RegistryError) -> String {
    match e {
        RegistryError::Unauthorized => "unauthorized".to_owned(),
        // The `name` was supplied by the client in HELLO; echoing it
        // is informative, not leaky.
        RegistryError::DatabaseNotFound(name) => format!("database not found: {name}"),
        // Strip the inner detail — it carries the underlying storage
        // error chain.
        RegistryError::StorageExhausted(_) => "storage exhausted".to_owned(),
        // `DatabaseUnavailable` also carries the trimmed multi-tenant cases
        // (max_connections / max_open_databases) after the seam mapping; its
        // inner detail is server-internal, so the wire message stays generic.
        RegistryError::DatabaseUnavailable(_) => "database unavailable".to_owned(),
        RegistryError::AuthStore(_) => "authentication backend error".to_owned(),
        RegistryError::Io(_) => "internal i/o error".to_owned(),
    }
}

/// v0.6.0 Fase 2 Task 3 — poll `gate` for the previous window's drop
/// count and, when non-zero, emit a single `tracing::warn!` targeted at
/// `slow_query_throttle`. Free function (not a method) so it can be
/// unit-tested with a synthetic gate + `Instant` without constructing a
/// `BoltHandler`. `BoltHandler::drain_slow_query_drops` is the only
/// production caller.
fn emit_slow_query_throttle_warning(
    gate: &mut crate::audit::SlowQueryGate,
    connection_id: u64,
    now: Instant,
) {
    if let Some(dropped) = gate.closed_window_drops(now) {
        tracing::warn!(
            target: "slow_query_throttle",
            connection_id = connection_id,
            dropped_events = dropped,
            window_seconds = 60u64,
            "slow-query audit emissions throttled"
        );
    }
}

fn sha256_hex(s: &str) -> String {
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    let out = h.finalize();
    let mut hex = String::with_capacity(out.len() * 2);
    for b in &out {
        hex.push(char::from_digit(u32::from(b >> 4), 16).unwrap());
        hex.push(char::from_digit(u32::from(b & 0x0f), 16).unwrap());
    }
    hex
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// Read the Bolt `n` field from a PULL/DISCARD `extra` dict. Returns the
/// requested batch size, or `-1` ("all remaining") when the key is
/// absent or not an integer — the Bolt 4.4 contract for legacy clients
/// that send `PULL {}`. Drivers that drive `fetch_size` send
/// `PULL {n: <size>}`.
fn read_n(extra: &[(String, PackStreamValue)]) -> i64 {
    extra
        .iter()
        .find(|(k, _)| k == "n")
        .and_then(|(_, v)| match v {
            PackStreamValue::Int(i) => Some(*i),
            _ => None,
        })
        .unwrap_or(-1)
}

/// Extract the row count the engine reported in a result-cap abort
/// message. Both cap messages have the shape
/// `"... <verb> {count} rows, exceeds max_result_rows={cap}"`, so the
/// first run of ASCII digits is the count (the second, `{cap}`, is the
/// configured limit the handler already knows). Returns `0` when no
/// digit run is present — the audit event still carries the cap and the
/// statement hash, so a missing count degrades gracefully rather than
/// dropping the event.
fn parse_capped_row_count(msg: &str) -> u64 {
    let bytes = msg.as_bytes();
    let Some(start) = bytes.iter().position(u8::is_ascii_digit) else {
        return 0;
    };
    let end = bytes[start..]
        .iter()
        .position(|b| !b.is_ascii_digit())
        .map_or(bytes.len(), |off| start + off);
    msg[start..end].parse().unwrap_or(0)
}

// ── Conversion helpers ──────────────────────────────────────────────────

/// Builds the Neo4j-style `stats` metadata value for a mutation's counters.
///
/// Emits only the numeric keys whose count is non-zero (a driver reads an
/// absent key as `0`), plus `contains-updates` always. Key names use the
/// hyphenated Bolt wire form (`nodes-created`, `properties-set`, …) that the
/// official drivers read into `summary.Counters`.
fn mutation_stats_to_dict(stats: &ermya_graph::gql::GqlMutationResult) -> PackStreamValue {
    let mut entries: Vec<(String, PackStreamValue)> = Vec::new();
    #[allow(clippy::cast_possible_wrap)]
    let mut push_nonzero = |key: &str, count: u64| {
        if count > 0 {
            entries.push((key.to_owned(), PackStreamValue::Int(count as i64)));
        }
    };
    push_nonzero("nodes-created", stats.nodes_created);
    push_nonzero("nodes-deleted", stats.nodes_deleted);
    push_nonzero("relationships-created", stats.edges_created);
    push_nonzero("relationships-deleted", stats.edges_deleted);
    push_nonzero("properties-set", stats.properties_set);
    push_nonzero("labels-added", stats.labels_added);
    entries.push((
        "contains-updates".to_owned(),
        PackStreamValue::Bool(stats.contains_updates()),
    ));
    PackStreamValue::Dict(entries)
}

/// Convert GQL query result rows to a `PendingResult`.
///
/// The caller passes `preferred_columns` derived from the parsed RETURN
/// items so that Bolt clients see the column order the user wrote. Any
/// row keys absent from `preferred_columns` are appended, sorted
/// lexicographically for determinism.
///
/// Rationale: inspecting only `rows[0]` loses columns that happen to be
/// absent from the first row because of `PropAccess` projections that
/// resolved to `Null` on some bindings but not others. The union of all
/// keys gives every projected column a fixed position, and the preferred
/// ordering preserves the RETURN-clause order clients expect from Bolt.
fn gql_result_to_pending_with_columns(
    rows: &[std::collections::HashMap<String, GqlValue>],
    preferred_columns: &[String],
) -> PendingResult {
    if rows.is_empty() && preferred_columns.is_empty() {
        return PendingResult {
            fields_psv: vec![],
            rows: vec![],
            cursor: 0,
            stats: None,
        };
    }

    // Union of all column names across every row, so a column missing from
    // `rows[0]` (because PropAccess produced Null there) is still surfaced.
    let mut columns: Vec<String> = preferred_columns.to_vec();
    let mut seen: std::collections::HashSet<&str> = columns.iter().map(String::as_str).collect();
    let mut extras: Vec<&str> = Vec::new();
    for row in rows {
        for k in row.keys() {
            if seen.insert(k.as_str()) {
                extras.push(k.as_str());
            }
        }
    }
    extras.sort_unstable();
    columns.extend(extras.into_iter().map(str::to_owned));

    let fields_psv: Vec<PackStreamValue> = columns
        .iter()
        .map(|c| PackStreamValue::String(c.clone()))
        .collect();

    let packed_rows: Vec<Vec<PackStreamValue>> = rows
        .iter()
        .map(|row| {
            columns
                .iter()
                .map(|col| gql_value_to_packstream(row.get(col)))
                .collect()
        })
        .collect();

    PendingResult {
        fields_psv,
        rows: packed_rows,
        cursor: 0,
        stats: None,
    }
}

/// Derives the preferred column order from a RETURN clause: each item
/// contributes its alias (if any) or its surface expression name, in the
/// order the user wrote them.
fn return_items_columns(items: &[ermya_graph::gql::ReturnItem]) -> Vec<String> {
    items
        .iter()
        .map(|it| {
            it.alias
                .clone()
                .unwrap_or_else(|| ermya_graph::gql::expr_surface_name(&it.expr))
        })
        .collect()
}

/// Maps a [`ParamError`] to its stable Bolt wire `(code, message)` pair per
/// spec section 7. Centralised here so both the parser-fix code and any
/// future caller (e.g. an enterprise accessor that re-validates params)
/// stay aligned with the same wire contract.
fn param_error_to_wire(err: &ParamError) -> (&'static str, String) {
    match err {
        ParamError::MissingParameter(name) => (
            "Neo.ClientError.Statement.ParameterMissing",
            format!("Expected parameter: ${name}"),
        ),
        ParamError::MissingPositionalParameter(n) => (
            "Neo.ClientError.Statement.ParameterMissing",
            format!("Expected positional parameter: ${n}"),
        ),
        ParamError::UnsupportedParamValue { name, got } => (
            "Neo.ClientError.Statement.TypeError",
            format!("Parameter ${name} is of type {got}, cannot be used as a literal"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::emit_slow_query_throttle_warning;
    use crate::audit::SlowQueryGate;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    /// `std::io::Write` sink that appends every byte to a shared buffer,
    /// so a `tracing-subscriber` fmt layer can be captured in-process.
    struct InMemWriter {
        buf: Arc<Mutex<Vec<u8>>>,
    }
    impl std::io::Write for InMemWriter {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.buf.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Run `f` with a fmt subscriber writing into an in-memory buffer,
    /// returning whatever was captured as a UTF-8 string.
    fn capture_tracing(f: impl FnOnce()) -> String {
        use tracing_subscriber::layer::SubscriberExt as _;
        let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        let writer_buf = buf.clone();
        let make_writer = move || -> Box<dyn std::io::Write + Send> {
            Box::new(InMemWriter {
                buf: writer_buf.clone(),
            })
        };
        let layer = tracing_subscriber::fmt::layer()
            .with_writer(make_writer)
            .with_ansi(false);
        let subscriber = tracing_subscriber::registry().with(layer);
        let guard = tracing::subscriber::set_default(subscriber);
        f();
        drop(guard);
        String::from_utf8(buf.lock().unwrap().clone()).unwrap_or_default()
    }

    /// A gate with `cap` that has already seen `cap + extra` slow events
    /// inside one window, so `extra` events were dropped and a closed
    /// window will report exactly `extra`.
    fn gate_with_drops(cap: u32, extra: u32, window_start: Instant) -> SlowQueryGate {
        let mut gate = SlowQueryGate::new(cap);
        for _ in 0..(cap + extra) {
            gate.allow(window_start);
        }
        gate
    }

    /// Issue #43: a write refused for targeting an append-only label answers
    /// with the invalid-request wire code, not the generic execution failure a
    /// driver might retry. The label reaches the client.
    #[test]
    fn append_only_rejection_maps_to_request_invalid() {
        let msg = format!(
            "{}label 'Event' is append-only and cannot be written inside a transaction",
            crate::graph_accessor::ENGINE_APPEND_ONLY_IN_TXN_PREFIX
        );
        let (code, wire) = super::map_sideeffect_free_engine_error(&msg);
        assert_eq!(code, "Neo.ClientError.Request.Invalid");
        assert!(
            wire.contains("Event"),
            "the offending label must reach the client: {wire}"
        );
        assert!(
            !wire.contains("__TG_"),
            "the internal sentinel must never reach the wire: {wire}"
        );
    }

    /// Cycles A11/A12: two caps that look alike but must answer differently.
    /// Exceeding the transaction memory cap is transient — the driver may retry
    /// and smaller transactions can succeed. Exceeding the batch cap is not —
    /// replaying the same oversized batch fails identically. Getting these
    /// backwards would either send drivers into futile retry loops or deny them
    /// a retry that would have worked.
    #[test]
    fn memory_cap_is_transient_but_batch_cap_is_not() {
        let mem = format!(
            "{}transaction 4 exceeded the memory cap",
            crate::graph_accessor::ENGINE_TXN_MEMORY_CAP_PREFIX
        );
        let (mem_code, _) = super::map_sideeffect_free_engine_error(&mem);
        assert_eq!(mem_code, "Neo.TransientError.General.MemoryPoolOutOfMemory");
        assert!(
            mem_code.contains("TransientError"),
            "the memory cap must be retryable: {mem_code}"
        );

        let batch = format!(
            "{}batch ops limit exceeded",
            crate::graph_accessor::ENGINE_BATCH_LIMIT_PREFIX
        );
        let (batch_code, _) = super::map_sideeffect_free_engine_error(&batch);
        assert_eq!(batch_code, "Neo.ClientError.Request.Invalid");
        assert!(
            batch_code.contains("ClientError"),
            "the batch cap must NOT invite a retry: {batch_code}"
        );
    }

    /// Cycle A10: a connected-node delete is an integrity violation.
    #[test]
    fn delete_connected_node_maps_to_constraint_validation_failed() {
        let msg = format!(
            "{}Cannot delete node 7, because it still has relationships",
            crate::graph_accessor::ENGINE_DELETE_CONNECTED_PREFIX
        );
        let (code, wire) = super::map_sideeffect_free_engine_error(&msg);
        assert_eq!(code, "Neo.ClientError.Schema.ConstraintValidationFailed");
        assert!(
            !wire.contains("__TG_"),
            "sentinel must not reach the wire: {wire}"
        );
    }

    /// The neighbouring pure mappings, pinned alongside so a future edit to the
    /// chain cannot silently reshuffle which sentinel yields which code.
    #[test]
    fn sideeffect_free_engine_errors_map_to_their_wire_codes() {
        let quota = format!(
            "{}database is full",
            crate::graph_accessor::ENGINE_QUOTA_EXCEEDED_PREFIX
        );
        assert_eq!(
            super::map_sideeffect_free_engine_error(&quota).0,
            "Neo.ClientError.General.StorageExhausted"
        );

        let constraint = format!(
            "{}duplicate value",
            crate::graph_accessor::ENGINE_CONSTRAINT_VIOLATED_PREFIX
        );
        assert_eq!(
            super::map_sideeffect_free_engine_error(&constraint).0,
            "Neo.ClientError.Schema.ConstraintValidationFailed"
        );

        assert_eq!(
            super::map_sideeffect_free_engine_error("some other failure").0,
            "Neo.ClientError.Statement.ExecutionFailed"
        );
    }

    #[test]
    fn throttle_warning_emitted_once_with_dropped_count_after_window_closes() {
        let t0 = Instant::now();
        // cap=2, 5 events in the window → 3 dropped.
        let mut gate = gate_with_drops(2, 3, t0);
        // Polling within the window reports nothing.
        let captured_in_window = capture_tracing(|| {
            emit_slow_query_throttle_warning(&mut gate, 42, t0 + Duration::from_secs(10));
        });
        assert!(
            captured_in_window.is_empty(),
            "no warning before the window closes; got: {captured_in_window}"
        );
        // Polling after the window closes reports the 3 drops once.
        let captured_after = capture_tracing(|| {
            emit_slow_query_throttle_warning(&mut gate, 42, t0 + Duration::from_secs(61));
        });
        assert!(
            captured_after.contains("slow_query_throttle"),
            "warning must target slow_query_throttle; got: {captured_after}"
        );
        assert!(
            captured_after.contains("dropped_events=3"),
            "warning must report dropped_events=3; got: {captured_after}"
        );
        assert!(
            captured_after.contains("connection_id=42"),
            "warning must carry connection_id=42; got: {captured_after}"
        );
        // A second poll after the window is silent (reported once).
        let captured_twice = capture_tracing(|| {
            emit_slow_query_throttle_warning(&mut gate, 42, t0 + Duration::from_secs(62));
        });
        assert!(
            captured_twice.is_empty(),
            "drops are reported exactly once per closed window; got: {captured_twice}"
        );
    }

    #[test]
    fn throttle_warning_silent_when_no_drops() {
        let t0 = Instant::now();
        // cap=5, exactly 5 events in the window → none dropped.
        let mut gate = gate_with_drops(5, 0, t0);
        let captured = capture_tracing(|| {
            emit_slow_query_throttle_warning(&mut gate, 7, t0 + Duration::from_secs(61));
        });
        assert!(
            captured.is_empty(),
            "a closed window with zero drops stays silent; got: {captured}"
        );
    }

    // Task 1: the default `accessor_factory` (the closure installed by
    // `new_with_handshake`) must satisfy the [`AccessorFactory`] alias — i.e.
    // build a `dyn GraphAccessor` from a `DbHandle` and be `Send + Sync` so the
    // handler stays `Send` across `.await`. Binding the default closure to the
    // alias type checks the production wiring's type contract at compile time
    // without needing a live `DbHandle`.
    #[test]
    fn default_accessor_factory_matches_alias() {
        fn assert_send_sync<T: Send + Sync>(_: &T) {}

        let factory: super::AccessorFactory = Arc::new(|h| {
            Arc::new(crate::DefaultGraphAccessor::new(h.graph())) as Arc<dyn crate::GraphAccessor>
        });
        assert_send_sync(&factory);
    }
}
