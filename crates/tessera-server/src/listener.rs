// Copyright 2026 BelowZero Security OU. All rights reserved.

//! TCP listener and accept loop for the `TesseraGraph` server.

use std::net::SocketAddr;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use tokio::sync::{Semaphore, watch};
use tokio::task::JoinSet;
use tokio_rustls::TlsAcceptor;

use tessera_graph::Graph;
use tessera_protocol::frame::FramedWriter;
use tessera_protocol::message::ServerMessage;

use crate::connection::ConnectionHandler;
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
        graph: Arc<RwLock<Graph>>,
        mut shutdown: watch::Receiver<bool>,
        max_connections: usize,
        idle_timeout: Duration,
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
                Self::send_capacity_error(stream).await;
                continue;
            };

            let ctx = Arc::clone(&ctx);
            let graph = Arc::clone(&graph);
            let shutdown_rx = shutdown.clone();

            tasks.spawn(async move {
                let _permit = permit;
                let mut handler =
                    ConnectionHandler::new(stream, ctx, graph, idle_timeout, shutdown_rx);
                let _ = handler.run().await;
            });
        }
    }

    /// TLS-enabled accept loop — mandatory for production.
    ///
    /// Each accepted `TcpStream` is wrapped with a TLS handshake before being
    /// passed to `ConnectionHandler`. Connections that fail the TLS handshake
    /// are dropped without spawning a handler.
    ///
    /// # Errors
    ///
    /// Returns `ServerError` on unrecoverable listener failure.
    pub async fn serve_tls(
        self,
        ctx: Arc<ServerContext>,
        graph: Arc<RwLock<Graph>>,
        mut shutdown: watch::Receiver<bool>,
        max_connections: usize,
        idle_timeout: Duration,
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
                Self::send_capacity_error(stream).await;
                continue;
            };

            let tls_acceptor = tls_acceptor.clone();
            let ctx = Arc::clone(&ctx);
            let graph = Arc::clone(&graph);
            let shutdown_rx = shutdown.clone();

            tasks.spawn(async move {
                let _permit = permit;
                match tls_acceptor.accept(stream).await {
                    Ok(tls_stream) => {
                        let mut handler = ConnectionHandler::new(
                            tls_stream,
                            ctx,
                            graph,
                            idle_timeout,
                            shutdown_rx,
                        );
                        let _ = handler.run().await;
                    }
                    Err(e) => {
                        tracing::warn!("TLS handshake failed: {e}");
                    }
                }
            });
        }
    }

    /// Write a capacity error to a stream and close it.
    async fn send_capacity_error(stream: tokio::net::TcpStream) {
        let (_, write_half) = tokio::io::split(stream);
        let mut writer = FramedWriter::new(write_half);
        let msg = ServerMessage::CapacityError {
            reason: "server at capacity".into(),
        };
        if let Ok(json) = serde_json::to_vec(&msg) {
            let _ = writer.write_frame(&json).await;
        }
    }
}
