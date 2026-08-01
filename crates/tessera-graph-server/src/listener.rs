// SPDX-License-Identifier: BSL-1.1

//! TCP/TLS listener with connection management.
//!
//! [`TesseraListener`] provides:
//! - [`serve_with`](TesseraListener::serve_with): generic accept loop — the
//!   extension point for enterprise to plug in its own handler.
//! - [`serve_plain`](TesseraListener::serve_plain): plain TCP for development
//!   and testing (feature-gated behind `plain-tcp`).
//! - [`serve_tls`](TesseraListener::serve_tls): TLS-enforced for production.

use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::task::JoinSet;

use crate::error::Result;

/// Outcome of the per-IP connection-cap check run by the accept loops
/// before the (TLS and/or Bolt) handshake. v0.6.0 Fase 2 Task 5 eje 3.
enum ConnAdmission {
    /// The connection is admitted. Carries the RAII guard (or `None` when
    /// the cap is disabled or the peer IP was unavailable) which the
    /// handler holds for the connection's lifetime.
    Admit(Option<crate::rate_limiter::ConnectionGuard>),
    /// The per-IP cap is hit; the caller must drop the socket without
    /// spawning a handler. The audit event and metric were already emitted.
    Reject,
}

/// Run the per-IP connection-cap check for a freshly accepted socket.
///
/// `peer_ip == None` (the `peer_addr()` lookup failed) is **fail-open**:
/// the connection is admitted with no guard, because the alternative —
/// rejecting every connection whose source IP we cannot read — is a worse
/// failure mode than skipping the cap for that rare case.
///
/// On rejection, emits `tessera_rate_limit_hits_total{axis="conn_ip"}`
/// and an `AuditEvent::ConnectionThrottled` before returning.
fn check_connection_cap(
    rate_limiter: &Arc<crate::rate_limiter::RateLimiter>,
    audit: &crate::audit::AuditSink,
    peer_ip: Option<std::net::IpAddr>,
) -> ConnAdmission {
    let Some(ip) = peer_ip else {
        return ConnAdmission::Admit(None);
    };
    if let Some(guard) = rate_limiter.try_acquire_connection(ip) {
        ConnAdmission::Admit(Some(guard))
    } else {
        crate::metrics::rate_limit_hit("conn_ip");
        audit.connection_throttled(crate::audit::ConnectionThrottledDetails {
            client_ip: ip.to_string(),
            live_connections: rate_limiter.live_connections(ip),
            cap: rate_limiter.conn_per_ip_cap(),
        });
        ConnAdmission::Reject
    }
}

/// A Bolt-protocol TCP listener with connection limits and graceful shutdown.
pub struct TesseraListener {
    inner: TcpListener,
}

impl TesseraListener {
    /// Bind to the given address.
    ///
    /// # Errors
    ///
    /// Returns an error if the address cannot be bound.
    pub async fn bind(addr: &str) -> Result<Self> {
        let inner = TcpListener::bind(addr).await?;
        Ok(Self { inner })
    }

    /// Return the local address this listener is bound to.
    ///
    /// # Errors
    ///
    /// Returns an error if the address cannot be retrieved.
    pub fn local_addr(&self) -> Result<SocketAddr> {
        Ok(self.inner.local_addr()?)
    }

    /// Accept a single connection.
    ///
    /// # Errors
    ///
    /// Returns an error on accept failure.
    pub async fn accept(&self) -> Result<(tokio::net::TcpStream, SocketAddr)> {
        let (stream, addr) = self.inner.accept().await?;
        Ok((stream, addr))
    }

    /// Generic accept loop — the **extension point** for enterprise.
    ///
    /// For each accepted TCP connection, `handler` is called in a spawned
    /// task. A [`Semaphore`](tokio::sync::Semaphore) enforces `max_connections`.
    /// The loop exits when `shutdown` fires or an unrecoverable error occurs.
    ///
    /// # Errors
    ///
    /// Returns an error on unrecoverable accept failures.
    pub async fn serve_with<F, Fut>(
        self,
        handler: F,
        mut shutdown: watch::Receiver<bool>,
        max_connections: usize,
    ) -> Result<()>
    where
        F: Fn(tokio::net::TcpStream) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let semaphore = Arc::new(tokio::sync::Semaphore::new(max_connections));
        let handler = Arc::new(handler);
        let mut tasks = JoinSet::new();

        loop {
            let accepted = tokio::select! {
                biased;

                result = shutdown.changed() => {
                    if result.is_err() || *shutdown.borrow() {
                        break;
                    }
                    continue;
                }

                result = self.inner.accept() => result,
            };

            let (stream, _peer) = match accepted {
                Ok(pair) => pair,
                Err(e) => {
                    tracing::warn!("accept error: {e}");
                    continue;
                }
            };

            let Ok(permit) = semaphore.clone().try_acquire_owned() else {
                tracing::warn!("max connections reached, dropping connection");
                drop(stream);
                continue;
            };

            let h = Arc::clone(&handler);
            tasks.spawn(async move {
                crate::metrics::connection_opened();
                h(stream).await;
                crate::metrics::connection_closed();
                drop(permit);
            });
        }

        // Drain in-flight tasks with a timeout.
        let drain_timeout = Duration::from_secs(5);
        let _ = tokio::time::timeout(drain_timeout, async {
            while tasks.join_next().await.is_some() {}
        })
        .await;

        Ok(())
    }

    /// Plain TCP accept loop — **for development and testing only**.
    ///
    /// Feature-gated behind `plain-tcp`. The `registry` is bound to every
    /// spawned handler so HELLO routes through the multi-database
    /// catalogue instead of the legacy single-graph fallback.
    ///
    /// # Errors
    ///
    /// Returns an error on unrecoverable failures.
    #[cfg(feature = "plain-tcp")]
    #[allow(clippy::too_many_arguments)] // listener config is cohesive
    pub async fn serve_plain<A>(
        self,
        auth: Arc<A>,
        auth_store: Arc<dyn crate::auth::UserStore>,
        audit: crate::audit::AuditSink,
        registry: Arc<dyn crate::registry::GraphRegistry>,
        multi_tenant: crate::registry_handle::MultiTenantHandle,
        // Constructor del despachador administrativo de pago, si esta edición
        // lo trae. Se transporta sin mirarlo dentro.
        paid_admin: Option<crate::admin_dispatch::PaidDispatcherBuilder>,
        rate_limiter: Arc<crate::rate_limiter::RateLimiter>,
        shutdown: watch::Receiver<bool>,
        max_connections: usize,
        idle_timeout: Duration,
        slow_threshold_ms: u64,
        max_slow_events_per_minute: u32,
        max_result_rows: u64,
        queries_max_per_second: u32,
        max_bytes_per_second: u64,
        query_timeout_ms: u64,
        server_agent: String,
    ) -> Result<()>
    where
        A: crate::auth::AuthProvider + ?Sized,
    {
        let query_cache = Arc::new(tessera_graph_cypher::cache::QueryCache::new(4096));
        let shutdown_for_handlers = shutdown.clone();
        self.serve_with(
            move |stream| {
                let auth = Arc::clone(&auth);
                let auth_store = Arc::clone(&auth_store);
                let audit = audit.clone();
                let registry = Arc::clone(&registry);
                let multi_tenant = multi_tenant.clone();
                let paid_admin = paid_admin.clone();
                let cache = Arc::clone(&query_cache);
                let rl = Arc::clone(&rate_limiter);
                let shutdown_rx = shutdown_for_handlers.clone();
                let server_agent = server_agent.clone();
                async move {
                    let peer = stream.peer_addr().ok();
                    let peer_ip = peer.map(|p| p.ip());
                    // Task 5 eje 3: enforce the per-IP connection cap before
                    // the Bolt handshake; a rejected socket is dropped here.
                    let conn_guard = match check_connection_cap(&rl, &audit, peer_ip) {
                        ConnAdmission::Admit(guard) => guard,
                        ConnAdmission::Reject => return,
                    };
                    match crate::BoltHandler::new_with_handshake(
                        stream,
                        auth,
                        auth_store,
                        audit.clone(),
                        registry,
                        multi_tenant,
                        paid_admin,
                        cache,
                        idle_timeout,
                        slow_threshold_ms,
                        max_slow_events_per_minute,
                        max_result_rows,
                        queries_max_per_second,
                        max_bytes_per_second,
                        query_timeout_ms,
                        server_agent,
                        Some(rl),
                        peer_ip,
                        shutdown_rx,
                    )
                    .await
                    {
                        Ok(handler) => {
                            let mut handler = handler.with_connection_guard(conn_guard);
                            if let Some(peer_addr) = peer {
                                audit.connection_open(
                                    handler.connection_id(),
                                    peer_addr,
                                    false,
                                );
                            }
                            let _ = handler.run().await;
                        }
                        Err(e) => {
                            tracing::debug!("handshake failed: {e}");
                            audit.connection_close(
                                0,
                                None,
                                crate::audit::CloseReason::HandshakeFailed,
                                0,
                            );
                        }
                    }
                }
            },
            shutdown,
            max_connections,
        )
        .await
    }

    /// TLS-enforced accept loop — **for production**.
    ///
    /// Each accepted connection is wrapped with TLS before passing to the
    /// Bolt handler. Connections that fail the TLS handshake are dropped.
    /// The `registry` is bound to every spawned handler so HELLO routes
    /// through the multi-database catalogue.
    ///
    /// # Errors
    ///
    /// Returns an error on unrecoverable failures.
    #[allow(clippy::too_many_arguments)] // listener config is cohesive
    pub async fn serve_tls<A>(
        self,
        auth: Arc<A>,
        auth_store: Arc<dyn crate::auth::UserStore>,
        audit: crate::audit::AuditSink,
        registry: Arc<dyn crate::registry::GraphRegistry>,
        multi_tenant: crate::registry_handle::MultiTenantHandle,
        // Constructor del despachador administrativo de pago, si esta edición
        // lo trae. Se transporta sin mirarlo dentro.
        paid_admin: Option<crate::admin_dispatch::PaidDispatcherBuilder>,
        tls_config: Arc<tokio_rustls::rustls::ServerConfig>,
        rate_limiter: Arc<crate::rate_limiter::RateLimiter>,
        shutdown: watch::Receiver<bool>,
        max_connections: usize,
        idle_timeout: Duration,
        slow_threshold_ms: u64,
        max_slow_events_per_minute: u32,
        max_result_rows: u64,
        queries_max_per_second: u32,
        max_bytes_per_second: u64,
        query_timeout_ms: u64,
        server_agent: String,
    ) -> Result<()>
    where
        A: crate::auth::AuthProvider + ?Sized,
    {
        let acceptor = tokio_rustls::TlsAcceptor::from(tls_config);
        let query_cache = Arc::new(tessera_graph_cypher::cache::QueryCache::new(4096));
        let shutdown_for_handlers = shutdown.clone();
        self.serve_with(
            move |stream| {
                let acceptor = acceptor.clone();
                let auth = Arc::clone(&auth);
                let auth_store = Arc::clone(&auth_store);
                let audit = audit.clone();
                let registry = Arc::clone(&registry);
                let multi_tenant = multi_tenant.clone();
                let paid_admin = paid_admin.clone();
                let cache = Arc::clone(&query_cache);
                let rl = Arc::clone(&rate_limiter);
                let shutdown_rx = shutdown_for_handlers.clone();
                let server_agent = server_agent.clone();
                async move {
                    let peer = stream.peer_addr().ok();
                    let peer_ip = peer.map(|p| p.ip());
                    // Task 5 eje 3: enforce the per-IP cap before the TLS
                    // handshake so a flooding peer cannot consume TLS CPU.
                    let conn_guard = match check_connection_cap(&rl, &audit, peer_ip) {
                        ConnAdmission::Admit(guard) => guard,
                        ConnAdmission::Reject => return,
                    };
                    let tls_stream = match acceptor.accept(stream).await {
                        Ok(s) => s,
                        Err(e) => {
                            tracing::debug!("TLS handshake failed: {e}");
                            audit.connection_close(
                                0,
                                None,
                                crate::audit::CloseReason::HandshakeFailed,
                                0,
                            );
                            return;
                        }
                    };
                    match crate::BoltHandler::new_with_handshake(
                        tls_stream,
                        auth,
                        auth_store,
                        audit.clone(),
                        registry,
                        multi_tenant,
                        paid_admin,
                        cache,
                        idle_timeout,
                        slow_threshold_ms,
                        max_slow_events_per_minute,
                        max_result_rows,
                        queries_max_per_second,
                        max_bytes_per_second,
                        query_timeout_ms,
                        server_agent,
                        Some(rl),
                        peer_ip,
                        shutdown_rx,
                    )
                    .await
                    {
                        Ok(handler) => {
                            let mut handler = handler.with_connection_guard(conn_guard);
                            if let Some(peer_addr) = peer {
                                audit.connection_open(
                                    handler.connection_id(),
                                    peer_addr,
                                    true,
                                );
                            }
                            let _ = handler.run().await;
                        }
                        Err(e) => {
                            tracing::debug!("bolt handshake failed: {e}");
                            audit.connection_close(
                                0,
                                None,
                                crate::audit::CloseReason::HandshakeFailed,
                                0,
                            );
                        }
                    }
                }
            },
            shutdown,
            max_connections,
        )
        .await
    }
}
