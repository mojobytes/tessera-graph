// Copyright 2026 BelowZero Security OU. All rights reserved.

//! TCP listener and accept loop for the `TesseraGraph` server.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use tokio::net::TcpListener;
use tokio::sync::watch;

use tessera_graph::Graph;
use tessera_protocol::frame::FramedWriter;
use tessera_protocol::message::ServerMessage;

use crate::connection::ConnectionHandler;
use crate::context::ServerContext;
use crate::error::Result;

/// TCP listener for `TesseraGraph`.
pub struct TesseraListener {
    inner: TcpListener,
}

impl TesseraListener {
    /// Bind to the given address.
    ///
    /// # Errors
    ///
    /// Returns `ServerError::Io` if binding fails.
    pub async fn bind(addr: &str) -> Result<Self> {
        let listener = TcpListener::bind(addr).await?;
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

    /// Run the accept loop, spawning a handler task per connection.
    ///
    /// Accepts connections until a shutdown signal is received.
    /// Enforces `max_connections` and `idle_timeout` per connection.
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
        let active = Arc::new(AtomicUsize::new(0));

        loop {
            let stream = tokio::select! {
                biased;

                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        return Ok(());
                    }
                    continue;
                }

                result = self.inner.accept() => {
                    match result {
                        Ok((stream, _addr)) => stream,
                        Err(e) => {
                            // Transient accept errors — log and continue
                            eprintln!("accept error: {e}");
                            continue;
                        }
                    }
                }
            };

            // Enforce connection limit
            let current = active.load(Ordering::SeqCst);
            if current >= max_connections {
                // Write capacity error and close
                let (_, write_half) = tokio::io::split(stream);
                let mut writer = FramedWriter::new(write_half);
                let msg = ServerMessage::AuthError {
                    reason: "server at capacity".into(),
                };
                let json = serde_json::to_vec(&msg).unwrap_or_default();
                let _ = writer.write_frame(&json).await;
                continue;
            }

            active.fetch_add(1, Ordering::SeqCst);

            let ctx = Arc::clone(&ctx);
            let graph = Arc::clone(&graph);
            let active = Arc::clone(&active);
            let shutdown_rx = shutdown.clone();

            tokio::spawn(async move {
                let mut handler =
                    ConnectionHandler::new(stream, ctx, graph, idle_timeout, shutdown_rx);
                let _ = handler.run().await;
                active.fetch_sub(1, Ordering::SeqCst);
            });
        }
    }
}
