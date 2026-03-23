// Copyright 2026 BelowZero Security OU. All rights reserved.

//! Per-connection handler for the `TesseraGraph` TCP protocol.

use std::sync::{Arc, RwLock};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::watch;

use tessera_auth::credentials::Password;
use tessera_auth::session::SessionToken;
use tessera_graph::{GqlStatement, GqlValue, Graph};
use tessera_protocol::frame::{FramedReader, FramedWriter};
use tessera_protocol::message::{ClientMessage, ServerMessage};
use tessera_storage_enterprise::lbac::{SecureGraph, SecureGraphRef};

use crate::context::ServerContext;
use crate::error::{Result, ServerError};

/// Generic message returned for all authentication failures.
/// Must never include internal details — prevents user enumeration and info leakage.
const AUTH_FAILURE_MSG: &str = "authentication failed";

/// Handles a single client connection over the `TesseraGraph` wire protocol.
pub struct ConnectionHandler<S: AsyncRead + AsyncWrite + Unpin> {
    reader: FramedReader<tokio::io::ReadHalf<S>>,
    writer: FramedWriter<tokio::io::WriteHalf<S>>,
    ctx: Arc<ServerContext>,
    graph: Arc<RwLock<Graph>>,
    session_token: Option<SessionToken>,
    idle_timeout: Duration,
    shutdown: watch::Receiver<bool>,
}

impl<S: AsyncRead + AsyncWrite + Unpin> ConnectionHandler<S> {
    /// Create a new connection handler.
    #[must_use]
    pub fn new(
        stream: S,
        ctx: Arc<ServerContext>,
        graph: Arc<RwLock<Graph>>,
        idle_timeout: Duration,
        shutdown: watch::Receiver<bool>,
    ) -> Self {
        let (read_half, write_half) = tokio::io::split(stream);
        Self {
            reader: FramedReader::new(read_half),
            writer: FramedWriter::new(write_half),
            ctx,
            graph,
            session_token: None,
            idle_timeout,
            shutdown,
        }
    }

    /// Drive the connection to completion.
    ///
    /// Reads frames in a loop, dispatches messages, and writes responses.
    /// Returns when the client disconnects, the idle timeout fires, or
    /// a shutdown signal is received.
    ///
    /// # Errors
    ///
    /// Returns `ServerError` on unrecoverable I/O or protocol errors.
    pub async fn run(&mut self) -> Result<()> {
        loop {
            let frame = tokio::select! {
                biased;

                _ = self.shutdown.changed() => {
                    if *self.shutdown.borrow() {
                        self.send_message(&ServerMessage::Bye).await.ok();
                        return Ok(());
                    }
                    continue;
                }

                result = tokio::time::timeout(self.idle_timeout, self.reader.read_frame()) => {
                    match result {
                        Ok(inner) => inner?,
                        Err(_elapsed) => {
                            self.send_message(&ServerMessage::Bye).await.ok();
                            return Ok(());
                        }
                    }
                }
            };

            let Some(frame) = frame else {
                // Clean EOF
                return Ok(());
            };

            let Ok(msg) = serde_json::from_slice::<ClientMessage>(&frame) else {
                self.send_message(&ServerMessage::ProtocolError {
                    reason: "invalid message format".into(),
                })
                .await?;
                continue;
            };

            match msg {
                ClientMessage::Ping => {
                    self.send_message(&ServerMessage::Pong).await?;
                }
                ClientMessage::Login { username, password } => {
                    self.handle_login(&username, &password).await?;
                }
                ClientMessage::Logout => {
                    self.handle_logout().await?;
                    return Ok(());
                }
                ClientMessage::Query { query, language } => {
                    if self.session_token.is_none() {
                        self.send_message(&ServerMessage::AuthError {
                            reason: "not authenticated".into(),
                        })
                        .await?;
                        continue;
                    }
                    self.handle_query(&query, &language).await?;
                }
            }
        }
    }

    async fn handle_login(&mut self, username: &str, password: &str) -> Result<()> {
        // External auth path (LDAP or OIDC)
        if let Some(provider) = self.ctx.external_provider() {
            return self
                .handle_external_login(username, password, provider.clone())
                .await;
        }

        // Local auth path (Argon2id)
        let password = match Password::new(password) {
            Ok(p) => p,
            Err(e) => {
                return self
                    .send_auth_failure(&format!("auth failure for user {username:?}: {e}"))
                    .await;
            }
        };

        let user_store = self.ctx.user_store();
        match user_store.authenticate(username, &password) {
            Ok(user_id) => {
                let sessions = self.ctx.sessions();
                match sessions.create_session(user_id) {
                    Ok(token) => {
                        let token_str = token.as_str().to_owned();
                        self.session_token = Some(token);
                        self.send_message(&ServerMessage::AuthOk { token: token_str })
                            .await?;
                        self.ctx.metrics().auth_success.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                    Err(e) => {
                        return self
                            .send_auth_failure(&format!(
                                "session creation failed for user {username:?}: {e}"
                            ))
                            .await;
                    }
                }
            }
            Err(e) => {
                return self
                    .send_auth_failure(&format!("auth failure for user {username:?}: {e}"))
                    .await;
            }
        }
        Ok(())
    }

    async fn handle_external_login(
        &mut self,
        username: &str,
        credential: &str,
        provider: std::sync::Arc<dyn tessera_auth::providers::ExternalAuthProvider>,
    ) -> Result<()> {
        match crate::auth_dispatch::authenticate_external(
            username,
            credential,
            &provider,
            self.ctx.group_mapping(),
            self.ctx.sessions(),
        )
        .await
        {
            Ok((_user_id, token)) => {
                let token_str = token.as_str().to_owned();
                self.session_token = Some(token);
                self.send_message(&ServerMessage::AuthOk { token: token_str })
                    .await?;
                self.ctx.metrics().auth_success.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            Err(e) => {
                return self
                    .send_auth_failure(&format!(
                        "external auth failure for user {username:?}: {e}"
                    ))
                    .await;
            }
        }
        Ok(())
    }

    /// Send a generic auth failure response and log the internal detail to the audit log.
    async fn send_auth_failure(&mut self, audit_detail: &str) -> Result<()> {
        self.ctx.metrics().auth_failure.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let _ = self
            .ctx
            .audit()
            .record_error(None, "login", None, audit_detail);
        self.send_message(&ServerMessage::AuthError {
            reason: AUTH_FAILURE_MSG.into(),
        })
        .await
    }

    async fn handle_logout(&mut self) -> Result<()> {
        if let Some(ref token) = self.session_token {
            let _ = self.ctx.sessions().revoke(token);
        }
        self.session_token = None;
        self.send_message(&ServerMessage::Bye).await?;
        Ok(())
    }

    /// Extract the caller's LBAC `Clearance` from the current session token.
    ///
    /// On failure, writes an `AuthError` response, records a denied audit entry,
    /// and returns `Ok(None)`. The caller must return `Ok(())` immediately on `None`.
    async fn resolve_clearance_or_deny(
        &mut self,
        operation: &'static str,
    ) -> Result<Option<tessera_auth::lbac::Clearance>> {
        let Some(token) = self.session_token.as_ref() else {
            self.send_message(&ServerMessage::AuthError {
                reason: "not authenticated".into(),
            })
            .await?;
            return Ok(None);
        };
        match self.ctx.resolve_clearance(token) {
            Ok(c) => Ok(Some(c)),
            Err(e) => {
                let _ = self.ctx.audit().record_denied(
                    None,
                    operation,
                    None,
                    &format!("clearance resolution failed: {e}"),
                );
                self.send_message(&ServerMessage::AuthError {
                    reason: "access denied".into(),
                })
                .await?;
                Ok(None)
            }
        }
    }

    #[allow(clippy::significant_drop_tightening)]
    async fn handle_query(&mut self, query_str: &str, language: &str) -> Result<()> {
        let query_start = std::time::Instant::now();
        let lang = match language {
            "gql" | "GQL" => tessera_config::QueryLanguage::Gql,
            "cypher" | "cypher_compat" => tessera_config::QueryLanguage::CypherCompat,
            _ => {
                self.send_message(&ServerMessage::QueryError {
                    reason: format!("unsupported language: {language}"),
                })
                .await?;
                return Ok(());
            }
        };

        // Parse the statement to determine if it is a read query or mutation
        let stmt = match tessera_cypher::parse_with_mode(query_str, lang) {
            Ok(s) => s,
            Err(e) => {
                self.send_message(&ServerMessage::QueryError {
                    reason: e.to_string(),
                })
                .await?;
                return Ok(());
            }
        };

        // Resolve the caller's LBAC clearance before executing any query.
        let Some(clearance) = self.resolve_clearance_or_deny("gql_query").await? else {
            return Ok(());
        };

        // SAFETY: std::sync::RwLock is held only within the synchronous block below.
        // The guard is dropped at the closing `}`, before the `.await` in `send_message`.
        // If this invariant is violated, `clippy::await_holding_lock` will catch it.
        let response = match stmt {
            GqlStatement::Query(ref q) => {
                let result = {
                    let graph = self.graph.read().map_err(|_| {
                        ServerError::Auth(tessera_auth::AuthError::LockPoisoned("graph"))
                    })?;
                    let secure = SecureGraphRef::new(&*graph, clearance);
                    tessera_graph::gql::execute(&secure, q)
                        .map(|rows| gql_result_to_json(&rows))
                };
                match result {
                    Ok((columns, json_rows)) => ServerMessage::QueryResult {
                        columns,
                        rows: json_rows,
                    },
                    Err(e) => ServerMessage::QueryError {
                        reason: e.to_string(),
                    },
                }
            }
            GqlStatement::Mutation(ref m) => {
                let result = {
                    let mut graph = self.graph.write().map_err(|_| {
                        ServerError::Auth(tessera_auth::AuthError::LockPoisoned("graph"))
                    })?;
                    let mut secure = SecureGraph::new(&mut *graph, clearance);
                    let r = tessera_storage_enterprise::gql::execute_mut(&mut secure, m);
                    if r.is_ok() {
                        // Release SecureGraph borrow before flushing
                        drop(secure);
                        graph.flush()?;
                    }
                    r
                };
                match result {
                    Ok(r) => ServerMessage::QueryResult {
                        columns: vec!["summary".into()],
                        rows: vec![vec![serde_json::json!({
                            "nodes_created": r.nodes_created,
                            "edges_created": r.edges_created,
                            "nodes_deleted": r.nodes_deleted,
                            "edges_deleted": r.edges_deleted,
                            "properties_set": r.properties_set,
                        })]],
                    },
                    Err(e) => ServerMessage::QueryError {
                        reason: e.to_string(),
                    },
                }
            }
        };

        // Record metrics
        let duration = query_start.elapsed().as_secs_f64();
        self.ctx.metrics().record_query_duration(duration);
        let is_error = matches!(response, ServerMessage::QueryError { .. });
        if is_error {
            self.ctx.metrics().query_errors.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        } else {
            let is_gql = matches!(lang, tessera_config::QueryLanguage::Gql);
            let is_mutation = matches!(stmt, GqlStatement::Mutation(_));
            match (is_gql, is_mutation) {
                (true, false) => self.ctx.metrics().queries_gql_read.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
                (true, true) => self.ctx.metrics().queries_gql_mutation.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
                (false, false) => self.ctx.metrics().queries_cypher_read.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
                (false, true) => self.ctx.metrics().queries_cypher_mutation.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            };
        }

        self.send_message(&response).await?;
        Ok(())
    }

    async fn send_message(&mut self, msg: &ServerMessage) -> Result<()> {
        let json = serde_json::to_vec(msg)?;
        self.writer.write_frame(&json).await?;
        Ok(())
    }
}

/// Convert a `GqlResult` (`Vec<HashMap<String, GqlValue>>`) to JSON columns + rows.
fn gql_result_to_json(
    result: &[std::collections::HashMap<String, GqlValue>],
) -> (Vec<String>, Vec<Vec<serde_json::Value>>) {
    if result.is_empty() {
        return (Vec::new(), Vec::new());
    }

    // Collect all column names from the first row (deterministic order via sort)
    let mut columns: Vec<String> = result[0].keys().cloned().collect();
    columns.sort();

    let rows = result
        .iter()
        .map(|row| {
            columns
                .iter()
                .map(|col| {
                    row.get(col)
                        .map_or(serde_json::Value::Null, gql_value_to_json)
                })
                .collect()
        })
        .collect();

    (columns, rows)
}

fn gql_value_to_json(v: &GqlValue) -> serde_json::Value {
    match v {
        GqlValue::Null => serde_json::Value::Null,
        GqlValue::Bool(b) => serde_json::Value::Bool(*b),
        GqlValue::Int(i) => serde_json::json!(i),
        GqlValue::Float(f) => serde_json::json!(f),
        GqlValue::Str(s) => serde_json::Value::String(s.clone()),
        GqlValue::List(items) => {
            serde_json::Value::Array(items.iter().map(gql_value_to_json).collect())
        }
    }
}
