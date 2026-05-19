// Copyright 2026 BelowZero Security OU. All rights reserved.

//! Bolt 4.4 connection handler for the `TesseraGraph` server.
//!
//! Each accepted TCP (or TLS) stream is handed to [`BoltConnectionHandler`] which:
//!
//! 1. Performs the Bolt version handshake on the raw stream.
//! 2. Wraps the stream in [`BoltChunkedReader`] / [`BoltChunkedWriter`].
//! 3. Runs the Bolt state machine: HELLO → RUN/PULL/… → GOODBYE.
//!
//! LBAC enforcement (Bell-LaPadula) is mandatory: all graph access goes through
//! [`SecureGraph`] / [`SecureGraphRef`].

use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::watch;

use tessera_graph_audit::{AuditEntry, AuditEvent};
use tessera_graph_auth::credentials::Password;
use tessera_graph_auth::session::SessionToken;
use tessera_graph_auth::user::UserId;
use tessera_graph::{GqlStatement, GqlValue, Graph};
use tessera_graph_protocol::bolt_frame::{BoltChunkedReader, BoltChunkedWriter};
use tessera_graph_protocol::bolt_handshake::{encode_version_response, negotiate_version};
use tessera_graph_protocol::bolt_message::{
    BoltDict, BoltRequest, BoltResponse, decode_request, encode_response,
};
use tessera_graph_protocol::packstream::PackStreamValue;
use tessera_graph_storage::lbac::{SecureGraph, SecureGraphRef};
use tessera_graph_tenant::{DatabaseAddress, DatabaseName, TenantId};

use crate::context::ServerContext;
use crate::error::{Result, ServerError};

/// Global connection counter for unique `connection_id` in HELLO responses.
static CONNECTION_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// Maximum number of mutations before an implicit batch is auto-flushed.
const AUTO_FLUSH_OPS: u32 = 500;

/// Maximum age of the first un-synced mutation before an implicit batch is
/// auto-flushed. Keeps worst-case data-loss window bounded.
const AUTO_FLUSH_WINDOW: Duration = Duration::from_millis(10);

// ── Batch state ──────────────────────────────────────────────────────────────

/// Per-connection batch state for deferred WAL sync.
///
/// Tracks whether the connection is inside an explicit `BEGIN`..`COMMIT` block
/// and, for auto-commit mode, how many mutations have been coalesced since the
/// last `wal_sync`.
struct BatchState {
    /// True when the client sent BEGIN and we called `graph.begin_batch()`.
    in_batch: bool,
    /// Number of mutations executed since the last WAL sync (auto-commit only).
    dirty_count: u32,
    /// Timestamp of the first un-synced mutation in the current implicit batch.
    first_dirty_at: Option<Instant>,
}

impl BatchState {
    const fn new() -> Self {
        Self {
            in_batch: false,
            dirty_count: 0,
            first_dirty_at: None,
        }
    }

    /// Enter an explicit batch (BEGIN).
    #[allow(clippy::missing_const_for_fn)]
    fn enter(&mut self) {
        self.in_batch = true;
        self.dirty_count = 0;
        self.first_dirty_at = None;
    }

    /// Exit a batch (COMMIT or ROLLBACK). Returns `true` if we were in a batch.
    #[allow(clippy::missing_const_for_fn)]
    fn exit(&mut self) -> bool {
        let was = self.in_batch;
        self.in_batch = false;
        self.dirty_count = 0;
        self.first_dirty_at = None;
        was
    }

    /// Record that a mutation was executed (for auto-flush tracking).
    fn mark_dirty(&mut self) {
        self.dirty_count += 1;
        if self.first_dirty_at.is_none() {
            self.first_dirty_at = Some(Instant::now());
        }
    }

    /// Check whether the implicit batch should be flushed.
    fn should_auto_flush(&self, max_ops: u32, max_age: Duration) -> bool {
        if self.dirty_count >= max_ops {
            return true;
        }
        if let Some(first) = self.first_dirty_at {
            if first.elapsed() >= max_age {
                return true;
            }
        }
        false
    }

    /// Reset dirty counters after an implicit flush.
    #[allow(clippy::missing_const_for_fn)]
    fn reset_dirty(&mut self) {
        self.dirty_count = 0;
        self.first_dirty_at = None;
    }
}

// ── Pending result ────────────────────────────────────────────────────────────

/// Stores the result of a RUN command until PULL (or DISCARD) arrives.
///
/// # Memory note
///
/// All rows are currently materialized eagerly in `handle_run`. A future
/// improvement is to replace `rows` with a streaming channel fed directly
/// from the query engine, which would allow incremental PULL without ever
/// holding more than `n` rows in memory (see resilience audit CRITICAL #1).
struct PendingResult {
    rows: Vec<Vec<PackStreamValue>>,
    /// Index of the next row to send (for paginated PULL via Bolt 4.4 `n`).
    cursor: usize,
}

// ── Handler ───────────────────────────────────────────────────────────────────

/// Handles a single client connection speaking the Bolt 4.4 protocol.
pub struct BoltConnectionHandler<S: AsyncRead + AsyncWrite + Unpin + Send + Sync> {
    reader: BoltChunkedReader<tokio::io::ReadHalf<S>>,
    writer: BoltChunkedWriter<tokio::io::WriteHalf<S>>,
    ctx: Arc<ServerContext>,
    /// The graph instance selected during HELLO (via [`TenantRegistry`][tessera_graph_tenant::TenantRegistry]).
    graph: Option<Arc<RwLock<Graph>>>,
    /// LBAC-scoped neighbor cache for accelerated traversal queries.
    neighbor_cache: Option<Arc<tessera_graph_storage::shared_cache::SharedNeighborCache>>,
    session_token: Option<SessionToken>,
    /// Per-connection batch state for deferred WAL sync.
    batch_state: BatchState,
    /// True after a FAILURE; commands other than RESET/GOODBYE are ignored.
    failed: bool,
    pending_result: Option<PendingResult>,
    default_tenant: String,
    idle_timeout: Duration,
    shutdown: watch::Receiver<bool>,
}

impl<S: AsyncRead + AsyncWrite + Unpin + Send + Sync> BoltConnectionHandler<S> {
    /// Perform the Bolt handshake on `stream`, then construct the handler.
    ///
    /// The handshake (20 bytes) is read from the raw stream **before** it is
    /// split for chunked framing, so the caller never needs to see the magic
    /// preamble bytes.
    ///
    /// # Errors
    ///
    /// Returns `io::Error` if the handshake I/O fails or no supported Bolt
    /// version is negotiated.
    pub async fn new_with_handshake(
        mut stream: S,
        ctx: Arc<ServerContext>,
        default_tenant: String,
        idle_timeout: Duration,
        shutdown: watch::Receiver<bool>,
    ) -> std::io::Result<Self> {
        // Bolt handshake happens on the raw stream, before chunked framing.
        let mut handshake_buf = [0u8; 20];
        stream.read_exact(&mut handshake_buf).await?;

        let version = negotiate_version(&handshake_buf);
        let response = encode_version_response(version);
        stream.write_all(&response).await?;
        stream.flush().await?;

        if version.is_none() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::ConnectionRefused,
                "no supported Bolt version",
            ));
        }

        // Split for chunked framing.
        let (read_half, write_half) = tokio::io::split(stream);
        Ok(Self {
            reader: BoltChunkedReader::new(read_half),
            writer: BoltChunkedWriter::new(write_half),
            ctx,
            graph: None,
            neighbor_cache: None,
            session_token: None,
            batch_state: BatchState::new(),
            failed: false,
            pending_result: None,
            default_tenant,
            idle_timeout,
            shutdown,
        })
    }

    /// Drive the connection until the client disconnects, the idle timeout
    /// fires, or a shutdown signal is received.
    ///
    /// # Errors
    ///
    /// Returns `ServerError` on unrecoverable I/O or protocol errors.
    pub async fn run(&mut self) -> Result<()> {
        let result = self.run_inner().await;
        // Flush any outstanding implicit or explicit batch on exit so that
        // mutations are durable even if the client disconnects without COMMIT.
        let _ = self.cleanup_batch_state();
        result
    }

    async fn run_inner(&mut self) -> Result<()> {
        loop {
            let data = tokio::select! {
                biased;

                result = self.shutdown.changed() => {
                    // Sender dropped (Err) or value changed to true → exit.
                    if result.is_err() || *self.shutdown.borrow() {
                        return Ok(());
                    }
                    continue;
                }

                result = tokio::time::timeout(self.idle_timeout, self.reader.read_message()) => {
                    match result {
                        Ok(inner) => {
                            match inner? {
                                Some(d) => d,
                                None => return Ok(()), // clean EOF
                            }
                        }
                        Err(_timeout) => return Ok(()),
                    }
                }
            };

            let should_exit = self.dispatch(&data).await?;
            if should_exit {
                return Ok(());
            }
        }
    }

    /// Decode `data` and call the appropriate handler.
    ///
    /// Returns `true` if the connection should close (GOODBYE received).
    #[allow(clippy::significant_drop_tightening)]
    async fn dispatch(&mut self, data: &[u8]) -> Result<bool> {
        let request = match decode_request(data) {
            Ok(r) => r,
            Err(e) => {
                self.send_failure(
                    "Neo.ClientError.Request.Invalid",
                    &format!("protocol error: {e}"),
                )
                .await?;
                return Ok(false);
            }
        };

        // In FAILED state only RESET and GOODBYE are processed.
        if self.failed {
            match &request {
                BoltRequest::Reset => {
                    self.handle_reset().await?;
                    return Ok(false);
                }
                BoltRequest::Goodbye => return Ok(true),
                _ => {
                    self.send_ignored().await?;
                    return Ok(false);
                }
            }
        }

        match request {
            BoltRequest::Hello { ref extra } => {
                self.handle_hello(extra).await?;
            }
            BoltRequest::Logon { ref auth } => {
                // LOGON re-authenticates on an existing connection. Clean up
                // any prior session state before processing to prevent stale
                // pending_result or session_token from leaking across sessions.
                self.pending_result = None;
                self.session_token = None;
                self.graph = None;
                self.neighbor_cache = None;
                self.handle_hello(auth).await?;
            }
            BoltRequest::Run {
                ref query,
                ref params,
                ref extra,
            } => {
                self.handle_run(query, params, extra).await?;
            }
            BoltRequest::Pull { ref extra } => {
                self.handle_pull(extra).await?;
            }
            BoltRequest::Discard { .. } => {
                self.handle_discard().await?;
            }
            BoltRequest::Begin { .. } => {
                self.handle_begin().await?;
            }
            BoltRequest::Commit => {
                self.handle_commit().await?;
            }
            BoltRequest::Rollback => {
                self.handle_rollback().await?;
            }
            BoltRequest::Reset => {
                self.handle_reset().await?;
            }
            BoltRequest::Goodbye => return Ok(true),
        }

        Ok(false)
    }

    // ── HELLO ─────────────────────────────────────────────────────────────────

    /// Authenticate the client, select the target database, and create a session.
    #[allow(clippy::too_many_lines)]
    async fn handle_hello(&mut self, extra: &BoltDict) -> Result<()> {
        let principal = dict_str(extra, "principal").unwrap_or("");
        let credentials = dict_str(extra, "credentials").unwrap_or("");

        // --- Authentication ---
        let Ok(user_id) = self.authenticate(principal, credentials).await else {
            self.ctx
                .metrics()
                .auth_failure
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return self
                .send_failure("Neo.ClientError.Security.Unauthorized", Self::auth_failure_msg())
                .await;
        };

        // --- Database selection ---
        let raw_db = dict_str(extra, "db");
        let addr = match parse_db_field(raw_db, &self.default_tenant) {
            Ok(a) => a,
            Err(e) => {
                return self
                    .send_failure("Neo.ClientError.Database.DatabaseNotFound", &e.to_string())
                    .await;
            }
        };

        let graph = match self.ctx.tenant_registry().get_or_load(&addr) {
            Ok(g) => g,
            Err(e) => {
                return self
                    .send_failure(
                        "Neo.ClientError.Database.DatabaseNotFound",
                        &format!("cannot open database {addr}: {e}"),
                    )
                    .await;
            }
        };

        // --- Session ---
        let Ok(token) = self.ctx.sessions().create_session(user_id) else {
            return self
                .send_failure(
                    "Neo.TransientError.General.DatabaseUnavailable",
                    Self::auth_failure_msg(),
                )
                .await;
        };

        self.graph = Some(graph);
        self.neighbor_cache = Some(self.ctx.neighbor_cache(&addr));
        self.session_token = Some(token);
        self.ctx
            .metrics()
            .auth_success
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        self.send_response(&BoltResponse::Success {
            metadata: vec![
                (
                    "server".to_owned(),
                    PackStreamValue::String("TesseraGraph/0.1.0".to_owned()),
                ),
                (
                    "connection_id".to_owned(),
                    PackStreamValue::String(format!(
                        "bolt-tessera-{}",
                        CONNECTION_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                    )),
                ),
            ],
        })
        .await
    }

    /// Resolve the current session's user ID for audit logging.
    ///
    /// Returns `None` if no session token is set or if validation fails.
    fn resolve_audit_user_id(&self) -> Option<u64> {
        self.session_token
            .as_ref()
            .and_then(|t| self.ctx.sessions().validate(t).ok())
            .map(tessera_graph_auth::user::UserId::raw)
    }

    /// Authenticate the client and return the `UserId`.
    ///
    /// Checks the login attempt tracker first: if the account is locked due to
    /// too many failed attempts, the request is rejected immediately without
    /// touching the credential store.
    ///
    /// Emits audit events for login success, failure, and rate-limiting.
    ///
    /// Returns `Err(())` on any authentication failure, so the caller can
    /// send the generic failure message without leaking details.
    ///
    /// Generic auth-failure message sent over the wire. Must never include
    /// usernames, passwords, or internal details — CWE-204 (observable
    /// response discrepancy) mandates that successful and failed auth
    /// attempts are indistinguishable to the caller.
    #[inline]
    const fn auth_failure_msg() -> &'static str {
        "authentication failed"
    }

    async fn authenticate(
        &self,
        principal: &str,
        credentials: &str,
    ) -> std::result::Result<UserId, ()> {
        // --- Rate-limit check (C3: audit rate-limited attempts) ---
        if self
            .ctx
            .login_tracker()
            .is_locked(principal, self.ctx.login_policy())
        {
            let _ = self.ctx.audit().record_event(AuditEntry::denied(
                None,
                AuditEvent::LoginRateLimited { username: principal.to_owned() },
                "account locked due to too many failed attempts".into(),
            ));
            return Err(());
        }

        if let Some(provider) = self.ctx.external_provider().cloned() {
            return crate::auth_dispatch::authenticate_external(
                principal,
                credentials,
                &provider,
                self.ctx.group_mapping(),
                self.ctx.sessions(),
            )
            .await
            .map(|(id, _token)| {
                self.ctx.login_tracker().record_success(principal);
                let _ = self.ctx.audit().record_event(AuditEntry::success(
                    Some(id.raw()),
                    AuditEvent::LoginSuccess { username: principal.to_owned() },
                ));
                id
            })
            .map_err(|_| {
                self.ctx.login_tracker().record_failure(principal);
                let _ = self.ctx.audit().record_event(AuditEntry::denied(
                    None,
                    AuditEvent::LoginFailure { username: principal.to_owned() },
                    "authentication failed".into(),
                ));
            });
        }

        // Local auth path.
        let Ok(password) = Password::new(credentials) else {
            self.ctx.login_tracker().record_failure(principal);
            let _ = self.ctx.audit().record_event(AuditEntry::denied(
                None,
                AuditEvent::LoginFailure { username: principal.to_owned() },
                "invalid credential format".into(),
            ));
            return Err(());
        };
        self.ctx
            .user_store()
            .authenticate(principal, &password)
            .inspect(|id| {
                self.ctx.login_tracker().record_success(principal);
                let _ = self.ctx.audit().record_event(AuditEntry::success(
                    Some(id.raw()),
                    AuditEvent::LoginSuccess { username: principal.to_owned() },
                ));
            })
            .map_err(|_| {
                self.ctx.login_tracker().record_failure(principal);
                let _ = self.ctx.audit().record_event(AuditEntry::denied(
                    None,
                    AuditEvent::LoginFailure { username: principal.to_owned() },
                    "authentication failed".into(),
                ));
            })
    }

    // ── RUN ───────────────────────────────────────────────────────────────────

    #[allow(clippy::significant_drop_tightening)]
    #[allow(clippy::too_many_lines)]
    async fn handle_run(
        &mut self,
        query: &str,
        params: &BoltDict,
        _extra: &BoltDict,
    ) -> Result<()> {
        // Parametrised queries are not yet implemented. Reject early so clients
        // don't silently get incorrect results from unsubstituted parameters.
        if !params.is_empty() {
            return self
                .send_failure(
                    "Neo.ClientError.Statement.ParameterMissing",
                    "parametrised queries are not yet supported; inline all values in the query text",
                )
                .await;
        }

        let query_start = std::time::Instant::now();

        // --- Session must exist ---
        let Some(ref token) = self.session_token else {
            return self
                .send_failure("Neo.ClientError.Security.Unauthorized", "not authenticated")
                .await;
        };

        // --- Clearance ---
        let Ok(clearance) = self.ctx.resolve_clearance(token) else {
            return self
                .send_failure("Neo.ClientError.Security.Unauthorized", "access denied")
                .await;
        };

        // --- Parse (server-wide LRU cache) ---
        // `params_signature = 0`: empty parameter map signature. We reject
        // non-empty params upstream, so this branch never aliases a cache
        // entry across different bindings.
        let stmt = match tessera_graph_cypher::parse_with_mode_cached(
            query,
            tessera_graph_config::QueryLanguage::CypherCompat,
            0,
            self.ctx.query_cache(),
        ) {
            Ok(s) => s,
            Err(e) => {
                return self
                    .send_failure("Neo.ClientError.Statement.SyntaxError", &e.to_string())
                    .await;
            }
        };

        // --- Graph lock and execution ---
        let Some(graph_arc) = self.graph.clone() else {
            return self
                .send_failure(
                    "Neo.ClientError.Database.DatabaseNotFound",
                    "no database selected",
                )
                .await;
        };

        // Execute the statement.  All lock guards are acquired and released
        // **synchronously** inside this match arm so that no guard is live
        // across any subsequent `.await` point (which would make the future
        // `!Send` and fail the JoinSet spawn).
        //
        // The result is `Ok((columns, rows))` on success, or `Err(message)` on
        // any execution-level error.  Lock-poisoning is propagated as a hard
        // `ServerError` via `?` before any guard is acquired.
        let exec_result: std::result::Result<(Vec<String>, Vec<Vec<PackStreamValue>>), String> =
            match stmt {
                GqlStatement::Query(ref q) => {
                    // Flush any pending implicit batch before reading, so
                    // read-after-write on the same connection sees its own writes.
                    if !self.batch_state.in_batch && self.batch_state.dirty_count > 0 {
                        let mut graph = graph_arc.write().map_err(|_| {
                            ServerError::Auth(tessera_graph_auth::AuthError::LockPoisoned("graph"))
                        })?;
                        graph.end_batch().map_err(ServerError::Storage)?;
                        self.batch_state.reset_dirty();
                        drop(graph);
                    }

                    let graph = graph_arc.read().map_err(|_| {
                        ServerError::Auth(tessera_graph_auth::AuthError::LockPoisoned("graph"))
                    })?;
                    let secure = SecureGraphRef::new(&*graph, clearance.clone());
                    let result = self.neighbor_cache.as_ref().map_or_else(
                        || tessera_graph_storage::gql::execute_query(&secure, q),
                        |cache| {
                            tessera_graph_storage::gql::execute_query_with_shared_cache(
                                &secure, cache, &clearance, q,
                            )
                        },
                    )
                    .map(|r| gql_result_to_packstream(&r))
                    .map_err(|e| e.to_string());
                    drop(secure);
                    drop(graph); // Explicitly drop guard before any `.await`
                    result
                }
                GqlStatement::Mutation(ref m) => {
                    let mut graph = graph_arc.write().map_err(|_| {
                        ServerError::Auth(tessera_graph_auth::AuthError::LockPoisoned("graph"))
                    })?;

                    // Auto-commit coalescing: open an implicit batch before the
                    // first mutation so that wal_sync inside add_node/etc is a
                    // no-op (batch_depth > 0). The batch is flushed when the
                    // ops/time threshold is reached or on RESET/LOGON/disconnect.
                    if !self.batch_state.in_batch && self.batch_state.dirty_count == 0 {
                        graph.begin_batch();
                    }

                    let mut secure = SecureGraph::new(&mut *graph, clearance);
                    let r = tessera_graph_storage::gql::execute_mut(&mut secure, m);
                    drop(secure);

                    // Track dirty state for auto-flush decisions.
                    if r.is_ok() {
                        self.batch_state.mark_dirty();
                    }

                    // Auto-flush: if not in an explicit batch and threshold is
                    // reached, issue a single fsync and re-open a new implicit
                    // batch for subsequent mutations.
                    if !self.batch_state.in_batch
                        && self.batch_state.should_auto_flush(AUTO_FLUSH_OPS, AUTO_FLUSH_WINDOW)
                    {
                        graph.end_batch().map_err(ServerError::Storage)?;
                        self.batch_state.reset_dirty();
                        // Re-open implicit batch for next mutation.
                        graph.begin_batch();
                    }

                    drop(graph); // Explicitly drop write guard before any `.await`

                    // Invalidate the neighbor cache after topology mutations.
                    if let Ok(ref result) = r {
                        if result.nodes_created > 0
                            || result.edges_created > 0
                            || result.nodes_deleted > 0
                            || result.edges_deleted > 0
                        {
                            if let Some(ref cache) = self.neighbor_cache {
                                cache.clear();
                            }
                        }
                    }

                    #[allow(clippy::cast_possible_wrap)]
                    r.map(|result| {
                        let summary_row = vec![
                            PackStreamValue::Int(result.nodes_created as i64),
                            PackStreamValue::Int(result.edges_created as i64),
                            PackStreamValue::Int(result.nodes_deleted as i64),
                            PackStreamValue::Int(result.edges_deleted as i64),
                            PackStreamValue::Int(result.properties_set as i64),
                        ];
                        (
                            vec![
                                "nodes_created".to_owned(),
                                "edges_created".to_owned(),
                                "nodes_deleted".to_owned(),
                                "edges_deleted".to_owned(),
                                "properties_set".to_owned(),
                            ],
                            vec![summary_row],
                        )
                    })
                    .map_err(|e| e.to_string())
                }
            };

        let (columns, rows) = match exec_result {
            Ok(r) => r,
            Err(e) => {
                let query_summary: String = query.chars().take(128).collect();
                let user_id = self.resolve_audit_user_id();
                let event = AuditEvent::QueryExecuted { query_preview: query_summary };
                let _ = self.ctx.audit().record_event(AuditEntry::error(user_id, event, e.clone()));
                return self
                    .send_failure("Neo.ClientError.Statement.ExecutionError", &e)
                    .await;
            }
        };

        // --- Audit ---
        // Truncate query to 128 chars to avoid storing PII in audit logs.
        let query_summary: String = query.chars().take(128).collect();
        let user_id = self.resolve_audit_user_id();
        let event = if matches!(stmt, GqlStatement::Mutation(_)) {
            AuditEvent::MutationExecuted { query_preview: query_summary }
        } else {
            AuditEvent::QueryExecuted { query_preview: query_summary }
        };
        let _ = self.ctx.audit().record_event(AuditEntry::success(user_id, event));

        // --- Metrics ---
        let duration = query_start.elapsed().as_secs_f64();
        self.ctx.metrics().record_query_duration(duration);
        let is_mutation = matches!(stmt, GqlStatement::Mutation(_));
        if is_mutation {
            self.ctx
                .metrics()
                .queries_cypher_mutation
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        } else {
            self.ctx
                .metrics()
                .queries_cypher_read
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }

        // --- Store result for PULL ---
        self.pending_result = Some(PendingResult { rows, cursor: 0 });

        self.send_response(&BoltResponse::Success {
            metadata: vec![(
                "fields".to_owned(),
                PackStreamValue::List(
                    columns
                        .iter()
                        .map(|c| PackStreamValue::String(c.clone()))
                        .collect(),
                ),
            )],
        })
        .await
    }

    // ── PULL ──────────────────────────────────────────────────────────────────

    async fn handle_pull(&mut self, extra: &BoltDict) -> Result<()> {
        let Some(result) = self.pending_result.as_mut() else {
            return self
                .send_response(&BoltResponse::Success {
                    metadata: vec![("has_more".to_owned(), PackStreamValue::Bool(false))],
                })
                .await;
        };

        // Bolt 4.4: `n` = number of records to fetch. -1 means "all".
        let n: i64 = extra
            .iter()
            .find(|(k, _)| k == "n")
            .and_then(|(_, v)| {
                if let PackStreamValue::Int(val) = v {
                    Some(*val)
                } else {
                    None
                }
            })
            .unwrap_or(-1);

        let batch_end = if n < 0 {
            result.rows.len()
        } else {
            let count = usize::try_from(n).unwrap_or(usize::MAX);
            result.cursor.saturating_add(count).min(result.rows.len())
        };

        // Collect the batch to avoid holding a mutable borrow on self across await.
        let batch: Vec<Vec<PackStreamValue>> =
            result.rows[result.cursor..batch_end].to_vec();
        result.cursor = batch_end;
        let has_more = result.cursor < result.rows.len();

        if !has_more {
            self.pending_result = None;
        }

        for row in batch {
            self.send_response(&BoltResponse::Record { fields: row })
                .await?;
        }

        self.send_response(&BoltResponse::Success {
            metadata: vec![("has_more".to_owned(), PackStreamValue::Bool(has_more))],
        })
        .await
    }

    // ── DISCARD ───────────────────────────────────────────────────────────────

    async fn handle_discard(&mut self) -> Result<()> {
        self.pending_result = None;
        self.send_response(&BoltResponse::Success { metadata: vec![] })
            .await
    }

    // ── BEGIN / COMMIT / ROLLBACK ─────────────────────────────────────────────

    async fn handle_begin(&mut self) -> Result<()> {
        if self.batch_state.in_batch {
            return self
                .send_failure(
                    "Neo.ClientError.Statement.ExecutionFailed",
                    "nested BEGIN is not supported",
                )
                .await;
        }

        let Some(graph_arc) = self.graph.clone() else {
            return self
                .send_failure(
                    "Neo.ClientError.Database.DatabaseNotFound",
                    "no database selected",
                )
                .await;
        };

        // Acquire write lock briefly — call begin_batch — release immediately.
        {
            let mut graph = graph_arc.write().map_err(|_| {
                ServerError::Auth(tessera_graph_auth::AuthError::LockPoisoned("graph"))
            })?;
            graph.begin_batch();
        }

        self.batch_state.enter();
        self.send_response(&BoltResponse::Success { metadata: vec![] })
            .await
    }

    async fn handle_commit(&mut self) -> Result<()> {
        if !self.batch_state.in_batch {
            return self
                .send_failure(
                    "Neo.ClientError.Statement.ExecutionFailed",
                    "no open transaction",
                )
                .await;
        }

        let Some(graph_arc) = self.graph.clone() else {
            return self
                .send_failure(
                    "Neo.ClientError.Database.DatabaseNotFound",
                    "no database selected",
                )
                .await;
        };

        // Acquire write lock — end_batch triggers single fsync — release.
        {
            let mut graph = graph_arc.write().map_err(|_| {
                ServerError::Auth(tessera_graph_auth::AuthError::LockPoisoned("graph"))
            })?;
            graph.end_batch().map_err(ServerError::Storage)?;
        }

        self.batch_state.exit();
        self.send_response(&BoltResponse::Success { metadata: vec![] })
            .await
    }

    async fn handle_rollback(&mut self) -> Result<()> {
        if !self.batch_state.in_batch {
            return self
                .send_failure(
                    "Neo.ClientError.Statement.ExecutionFailed",
                    "no open transaction",
                )
                .await;
        }

        let Some(graph_arc) = self.graph.clone() else {
            return self
                .send_failure(
                    "Neo.ClientError.Database.DatabaseNotFound",
                    "no database selected",
                )
                .await;
        };

        // end_batch syncs the WAL — mutations are already applied (no isolation),
        // so "rollback" just closes the batch. Data is made durable.
        {
            let mut graph = graph_arc.write().map_err(|_| {
                ServerError::Auth(tessera_graph_auth::AuthError::LockPoisoned("graph"))
            })?;
            graph.end_batch().map_err(ServerError::Storage)?;
        }

        self.batch_state.exit();
        self.pending_result = None;
        self.send_response(&BoltResponse::Success { metadata: vec![] })
            .await
    }

    // ── RESET ─────────────────────────────────────────────────────────────────

    async fn handle_reset(&mut self) -> Result<()> {
        self.cleanup_batch_state()?;
        self.failed = false;
        self.pending_result = None;
        self.send_response(&BoltResponse::Success { metadata: vec![] })
            .await
    }

    // ── Batch helpers ────────────────────────────────────────────────────────

    /// Flush and close any open batch (explicit or implicit).
    ///
    /// Called on RESET, LOGON, and connection teardown to ensure WAL
    /// durability and prevent `batch_depth` leaks.
    fn cleanup_batch_state(&mut self) -> Result<()> {
        let needs_flush =
            self.batch_state.in_batch || self.batch_state.dirty_count > 0;

        if needs_flush {
            if let Some(ref graph_arc) = self.graph {
                let mut graph = graph_arc.write().map_err(|_| {
                    ServerError::Auth(tessera_graph_auth::AuthError::LockPoisoned("graph"))
                })?;
                graph.end_batch().map_err(ServerError::Storage)?;
            }
        }

        self.batch_state.exit();
        Ok(())
    }

    // ── Response helpers ──────────────────────────────────────────────────────

    async fn send_response(&mut self, resp: &BoltResponse) -> Result<()> {
        let data = encode_response(resp)?;
        self.writer
            .write_message(&data)
            .await
            .map_err(ServerError::BoltIo)
    }

    async fn send_failure(&mut self, code: &str, message: &str) -> Result<()> {
        self.failed = true;
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
}

// ── Free functions ────────────────────────────────────────────────────────────

/// Parse the `db` field from a HELLO/RUN extra dict into a [`DatabaseAddress`].
///
/// Rules:
/// - `None` or empty string → `(default_tenant, "default")`
/// - `"production"` → `(default_tenant, "production")`
/// - `"acme/production"` → `("acme", "production")`
/// - More than one `/` → error
///
/// # Errors
///
/// Returns [`ServerError::Tenant`] on invalid name syntax or too many `/`.
pub fn parse_db_field(raw: Option<&str>, default_tenant: &str) -> Result<DatabaseAddress> {
    let s = raw.unwrap_or("").trim();
    if s.is_empty() {
        let tenant = TenantId::new(default_tenant)?;
        let database = DatabaseName::default_name();
        return Ok(DatabaseAddress { tenant, database });
    }

    let parts: Vec<&str> = s.splitn(3, '/').collect();
    match parts.as_slice() {
        [db_name] => {
            let tenant = TenantId::new(default_tenant)?;
            let database = DatabaseName::new(*db_name)?;
            Ok(DatabaseAddress { tenant, database })
        }
        [tenant_str, db_name] => {
            let tenant = TenantId::new(*tenant_str)?;
            let database = DatabaseName::new(*db_name)?;
            Ok(DatabaseAddress { tenant, database })
        }
        _ => Err(ServerError::Tenant(
            tessera_graph_tenant::TenantError::InvalidName(format!(
                "db field must be 'database' or 'tenant/database', got: {s}"
            )),
        )),
    }
}

/// Convert a GQL result (`Vec<HashMap<String, GqlValue>>`) to column names + `PackStream` rows.
fn gql_result_to_packstream(
    result: &[std::collections::HashMap<String, GqlValue>],
) -> (Vec<String>, Vec<Vec<PackStreamValue>>) {
    if result.is_empty() {
        return (Vec::new(), Vec::new());
    }

    let mut columns: Vec<String> = result[0].keys().cloned().collect();
    columns.sort();

    let rows = result
        .iter()
        .map(|row| {
            columns
                .iter()
                .map(|col| {
                    row.get(col)
                        .map_or(PackStreamValue::Null, gql_value_to_packstream)
                })
                .collect()
        })
        .collect();

    (columns, rows)
}

fn gql_value_to_packstream(v: &GqlValue) -> PackStreamValue {
    match v {
        GqlValue::Null => PackStreamValue::Null,
        GqlValue::Bool(b) => PackStreamValue::Bool(*b),
        GqlValue::Int(i) => PackStreamValue::Int(*i),
        GqlValue::Float(f) => PackStreamValue::Float(*f),
        GqlValue::Str(s) => PackStreamValue::String(s.clone()),
        GqlValue::List(items) => {
            PackStreamValue::List(items.iter().map(gql_value_to_packstream).collect())
        }
    }
}

/// Look up a string value in a `BoltDict` by key.
fn dict_str<'a>(dict: &'a BoltDict, key: &str) -> Option<&'a str> {
    dict.iter().find_map(|(k, v)| {
        if k == key {
            if let PackStreamValue::String(s) = v {
                Some(s.as_str())
            } else {
                None
            }
        } else {
            None
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batch_state_new_is_clean() {
        let bs = BatchState::new();
        assert!(!bs.in_batch);
        assert_eq!(bs.dirty_count, 0);
        assert!(bs.first_dirty_at.is_none());
    }

    #[test]
    fn batch_state_enter_exit_roundtrip() {
        let mut bs = BatchState::new();
        bs.enter();
        assert!(bs.in_batch);
        let was = bs.exit();
        assert!(was);
        assert!(!bs.in_batch);
    }

    #[test]
    fn batch_state_exit_without_enter_returns_false() {
        let mut bs = BatchState::new();
        assert!(!bs.exit());
    }

    #[test]
    fn batch_state_mark_dirty_tracks_count_and_timestamp() {
        let mut bs = BatchState::new();
        bs.mark_dirty();
        assert_eq!(bs.dirty_count, 1);
        assert!(bs.first_dirty_at.is_some());
        let first = bs.first_dirty_at.unwrap();
        bs.mark_dirty();
        assert_eq!(bs.dirty_count, 2);
        assert_eq!(bs.first_dirty_at.unwrap(), first);
    }

    #[test]
    fn batch_state_auto_flush_on_ops_threshold() {
        let mut bs = BatchState::new();
        for _ in 0..499 {
            bs.mark_dirty();
        }
        assert!(!bs.should_auto_flush(500, Duration::from_secs(60)));
        bs.mark_dirty();
        assert!(bs.should_auto_flush(500, Duration::from_secs(60)));
    }

    #[test]
    fn batch_state_auto_flush_on_time_threshold() {
        let mut bs = BatchState::new();
        bs.dirty_count = 1;
        bs.first_dirty_at = Some(
            Instant::now()
                .checked_sub(Duration::from_millis(20))
                .expect("test: clock"),
        );
        assert!(bs.should_auto_flush(500, Duration::from_millis(10)));
    }

    #[test]
    fn batch_state_reset_dirty_clears_counters() {
        let mut bs = BatchState::new();
        bs.mark_dirty();
        bs.mark_dirty();
        bs.reset_dirty();
        assert_eq!(bs.dirty_count, 0);
        assert!(bs.first_dirty_at.is_none());
    }
}
