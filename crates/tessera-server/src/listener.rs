// Copyright 2026 BelowZero Security OU. All rights reserved.

//! TCP listener and accept loop for the `TesseraGraph` server.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Semaphore, watch};
use tokio::task::JoinSet;
use tokio_rustls::TlsAcceptor;

use tessera_tenant::TenantRegistry;

use crate::bolt_handler::BoltConnectionHandler;
use crate::context::ServerContext;
use crate::error::Result;

/// TCP listener for `TesseraGraph`.
pub struct TesseraListener {
    inner: tokio::net::TcpListener,
}

impl TesseraListener {
    /// Bind to the given address.
    ///
    /// # Errors
    ///
    /// Returns `ServerError::Io` if binding fails.
    pub async fn bind(addr: &str) -> Result<Self> {
        let listener = tokio::net::TcpListener::bind(addr).await?;
        Ok(Self { inner: listener })
    }

    /// Return the local address this listener is bound to.
    ///
    /// # Errors
    ///
    /// Returns `ServerError::Io` on failure.
    pub fn local_addr(&self) -> Result<SocketAddr> {
        Ok(self.inner.local_addr()?)
    }

    /// Accept a single connection.
    ///
    /// # Errors
    ///
    /// Returns `ServerError::Io` on failure.
    pub async fn accept(&self) -> Result<(tokio::net::TcpStream, SocketAddr)> {
        let (stream, addr) = self.inner.accept().await?;
        Ok((stream, addr))
    }

    /// Plain TCP accept loop — for testing only.
    ///
    /// Production code must use [`serve_tls`](Self::serve_tls).
    ///
    /// # Errors
    ///
    /// Returns `ServerError` on unrecoverable listener failure.
    pub async fn serve(
        self,
        ctx: Arc<ServerContext>,
        registry: Arc<TenantRegistry>,
        mut shutdown: watch::Receiver<bool>,
        max_connections: usize,
        idle_timeout: Duration,
        default_tenant: String,
    ) -> Result<()> {
        let semaphore = Arc::new(Semaphore::new(max_connections));
        let mut tasks: JoinSet<()> = JoinSet::new();

        loop {
            // Reap completed tasks (non-blocking)
            while tasks.try_join_next().is_some() {}

            let stream = tokio::select! {
                biased;

                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        let drain_timeout = Duration::from_secs(30);
                        let _ = tokio::time::timeout(drain_timeout, async {
                            while tasks.join_next().await.is_some() {}
                        })
                        .await;
                        return Ok(());
                    }
                    continue;
                }

                result = self.inner.accept() => {
                    match result {
                        Ok((stream, _addr)) => stream,
                        Err(e) => {
                            tracing::warn!("accept error: {e}");
                            continue;
                        }
                    }
                }
            };

            let Ok(permit) = Arc::clone(&semaphore).try_acquire_owned() else {
                // At capacity: close the TCP stream — client has not done the
                // Bolt handshake yet so no Bolt message can be sent.
                drop(stream);
                continue;
            };

            let ctx = Arc::clone(&ctx);
            let _registry = Arc::clone(&registry);
            let shutdown_rx = shutdown.clone();
            let default_tenant = default_tenant.clone();

            tasks.spawn(async move {
                let _permit = permit;
                match BoltConnectionHandler::new_with_handshake(
                    stream,
                    ctx,
                    default_tenant,
                    idle_timeout,
                    shutdown_rx,
                )
                .await
                {
                    Ok(mut handler) => {
                        let _ = handler.run().await;
                    }
                    Err(e) => {
                        tracing::warn!("Bolt handshake failed: {e}");
                    }
                }
            });
        }
    }

    /// TLS-enabled accept loop — mandatory for production.
    ///
    /// Each accepted `TcpStream` is wrapped with a TLS handshake before being
    /// passed to `BoltConnectionHandler`. Connections that fail the TLS handshake
    /// are dropped without spawning a handler.
    ///
    /// # Errors
    ///
    /// Returns `ServerError` on unrecoverable listener failure.
    pub async fn serve_tls(
        self,
        ctx: Arc<ServerContext>,
        registry: Arc<TenantRegistry>,
        mut shutdown: watch::Receiver<bool>,
        max_connections: usize,
        idle_timeout: Duration,
        default_tenant: String,
    ) -> Result<()> {
        let tls_acceptor = TlsAcceptor::from(Arc::clone(ctx.tls_config().server_config()));
        let semaphore = Arc::new(Semaphore::new(max_connections));
        let mut tasks: JoinSet<()> = JoinSet::new();

        loop {
            while tasks.try_join_next().is_some() {}

            let stream = tokio::select! {
                biased;

                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        let drain_timeout = Duration::from_secs(30);
                        let _ = tokio::time::timeout(drain_timeout, async {
                            while tasks.join_next().await.is_some() {}
                        })
                        .await;
                        return Ok(());
                    }
                    continue;
                }

                result = self.inner.accept() => {
                    match result {
                        Ok((stream, _addr)) => stream,
                        Err(e) => {
                            tracing::warn!("accept error: {e}");
                            continue;
                        }
                    }
                }
            };

            let Ok(permit) = Arc::clone(&semaphore).try_acquire_owned() else {
                drop(stream);
                continue;
            };

            let tls_acceptor = tls_acceptor.clone();
            let ctx = Arc::clone(&ctx);
            let _registry = Arc::clone(&registry);
            let shutdown_rx = shutdown.clone();
            let default_tenant = default_tenant.clone();

            tasks.spawn(async move {
                let _permit = permit;
                match tls_acceptor.accept(stream).await {
                    Ok(tls_stream) => {
                        match BoltConnectionHandler::new_with_handshake(
                            tls_stream,
                            ctx,
                            default_tenant,
                            idle_timeout,
                            shutdown_rx,
                        )
                        .await
                        {
                            Ok(mut handler) => {
                                let _ = handler.run().await;
                            }
                            Err(e) => {
                                tracing::warn!("Bolt handshake failed on TLS stream: {e}");
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!("TLS handshake failed: {e}");
                    }
                }
            });
        }
    }
}
