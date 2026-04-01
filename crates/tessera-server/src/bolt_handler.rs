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
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::watch;

use tessera_audit::{AuditEntry, AuditEvent};
use tessera_auth::credentials::Password;
use tessera_auth::session::SessionToken;
use tessera_auth::user::UserId;
use tessera_graph::{GqlStatement, GqlValue, Graph};
use tessera_protocol::bolt_frame::{BoltChunkedReader, BoltChunkedWriter};
use tessera_protocol::bolt_handshake::{encode_version_response, negotiate_version};
use tessera_protocol::bolt_message::{
    BoltDict, BoltRequest, BoltResponse, decode_request, encode_response,
};
use tessera_protocol::packstream::PackStreamValue;
use tessera_storage_enterprise::lbac::{SecureGraph, SecureGraphRef};
use tessera_tenant::{DatabaseAddress, DatabaseName, TenantId};

use crate::context::ServerContext;
use crate::error::{Result, ServerError};

/// Generic auth-failure message sent over the wire.
/// Must never include usernames, passwords, or internal details.
const AUTH_FAILURE_MSG: &str = "authentication failed";

/// Global connection counter for unique `connection_id` in HELLO responses.
static CONNECTION_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

// ── Pending result ────────────────────────────────────────────────────────────

/// Stores the result of a RUN command until a PULL (or DISCARD) arrives.
struct PendingResult {
    rows: Vec<Vec<PackStreamValue>>,
}

// ── Handler ───────────────────────────────────────────────────────────────────

/// Handles a single client connection speaking the Bolt 4.4 protocol.
pub struct BoltConnectionHandler<S: AsyncRead + AsyncWrite + Unpin + Send + Sync> {
    reader: BoltChunkedReader<tokio::io::ReadHalf<S>>,
    writer: BoltChunkedWriter<tokio::io::WriteHalf<S>>,
    ctx: Arc<ServerContext>,
    /// The graph instance selected during HELLO (via [`TenantRegistry`][tessera_tenant::TenantRegistry]).
    graph: Option<Arc<RwLock<Graph>>>,
    session_token: Option<SessionToken>,
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
            session_token: None,
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
        loop {
            let data = tokio::select! {
                biased;

                _ = self.shutdown.changed() => {
                    if *self.shutdown.borrow() {
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
                .send_failure("Neo.ClientError.Security.Unauthorized", AUTH_FAILURE_MSG)
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
                    AUTH_FAILURE_MSG,
                )
                .await;
        };

        self.graph = Some(graph);
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
            .map(|uid| uid.raw())
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
        let stmt = match tessera_cypher::parse_with_mode_cached(
            query,
            tessera_config::QueryLanguage::CypherCompat,
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
                    let graph = graph_arc.read().map_err(|_| {
                        ServerError::Auth(tessera_auth::AuthError::LockPoisoned("graph"))
                    })?;
                    let secure = SecureGraphRef::new(&*graph, clearance);
                    let result = tessera_graph::gql::execute(&secure, q)
                        .map(|r| gql_result_to_packstream(&r))
                        .map_err(|e| e.to_string());
                    drop(secure);
                    drop(graph); // Explicitly drop guard before any `.await`
                    result
                }
                GqlStatement::Mutation(ref m) => {
                    let mut graph = graph_arc.write().map_err(|_| {
                        ServerError::Auth(tessera_auth::AuthError::LockPoisoned("graph"))
                    })?;
                    let mut secure = SecureGraph::new(&mut *graph, clearance);
                    let r = tessera_storage_enterprise::gql::execute_mut(&mut secure, m);
                    // WAL guarantees durability; flush is handled by the
                    // background timer (see flush_task::spawn_background_flush).
                    drop(secure);
                    drop(graph); // Explicitly drop write guard before any `.await`
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
        self.pending_result = Some(PendingResult { rows });

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

    async fn handle_pull(&mut self, _extra: &BoltDict) -> Result<()> {
        let Some(result) = self.pending_result.take() else {
            return self
                .send_response(&BoltResponse::Success {
                    metadata: vec![("has_more".to_owned(), PackStreamValue::Bool(false))],
                })
                .await;
        };

        for row in result.rows {
            self.send_response(&BoltResponse::Record { fields: row })
                .await?;
        }

        self.send_response(&BoltResponse::Success {
            metadata: vec![("has_more".to_owned(), PackStreamValue::Bool(false))],
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
        // Explicit transactions are not yet implemented. Responding SUCCESS
        // would be a protocol lie: mutations auto-commit immediately and
        // ROLLBACK cannot revert them. Send FAILURE so clients enter the
        // FAILED state and cannot silently corrupt data.
        self.send_failure(
            "Neo.DatabaseError.Statement.ExecutionFailed",
            "explicit transactions are not supported; use auto-commit mode",
        )
        .await
    }

    async fn handle_commit(&mut self) -> Result<()> {
        // Unreachable for well-behaved clients (BEGIN fails → FAILED state).
        // If reached, the client sent COMMIT outside a transaction.
        self.send_ignored().await
    }

    async fn handle_rollback(&mut self) -> Result<()> {
        // Unreachable for well-behaved clients (BEGIN fails → FAILED state).
        // If reached, the client sent ROLLBACK outside a transaction.
        self.pending_result = None;
        self.send_ignored().await
    }

    // ── RESET ─────────────────────────────────────────────────────────────────

    async fn handle_reset(&mut self) -> Result<()> {
        self.failed = false;
        self.pending_result = None;
        self.send_response(&BoltResponse::Success { metadata: vec![] })
            .await
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
            tessera_tenant::TenantError::InvalidName(format!(
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
