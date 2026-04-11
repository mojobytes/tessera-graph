// Copyright 2026 BelowZero Security OU. All rights reserved.

//! TCP listener for `TesseraGraph` Enterprise.
//!
//! Re-exports the MIT [`TesseraListener`] for bind/accept and provides
//! enterprise-specific serve functions that plug the enterprise
//! [`BoltConnectionHandler`] into the generic accept loop.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;
use tokio_rustls::TlsAcceptor;

pub use tessera_server_core::TesseraListener;

use crate::bolt_handler::BoltConnectionHandler;
use crate::context::ServerContext;
use crate::error::Result;

/// Plain TCP accept loop — **for testing only**.
///
/// Uses the MIT `serve_with` accept loop with an enterprise handler factory.
///
/// # Errors
///
/// Returns `ServerError` on unrecoverable listener failure.
pub async fn serve_enterprise(
    listener: TesseraListener,
    ctx: Arc<ServerContext>,
    shutdown: watch::Receiver<bool>,
    max_connections: usize,
    idle_timeout: Duration,
    default_tenant: String,
) -> Result<()> {
    let shutdown_for_handlers = shutdown.clone();
    listener
        .serve_with(
            move |stream| {
                let ctx = Arc::clone(&ctx);
                let shutdown_rx = shutdown_for_handlers.clone();
                let tenant = default_tenant.clone();
                async move {
                    match BoltConnectionHandler::new_with_handshake(
                        stream,
                        ctx,
                        tenant,
                        idle_timeout,
                        shutdown_rx,
                    )
                    .await
                    {
                        Ok(mut handler) => {
                            if let Err(e) = handler.run().await {
                                tracing::warn!("connection handler error: {e}");
                            }
                        }
                        Err(e) => {
                            tracing::warn!("Bolt handshake failed: {e}");
                        }
                    }
                }
            },
            shutdown,
            max_connections,
        )
        .await
        .map_err(|e| crate::error::ServerError::Io(std::io::Error::other(e.to_string())))
}

/// TLS-enabled accept loop — **mandatory for production**.
///
/// Each accepted `TcpStream` is wrapped with a TLS handshake before being
/// passed to the enterprise [`BoltConnectionHandler`].
///
/// # Errors
///
/// Returns `ServerError` on unrecoverable listener failure.
pub async fn serve_enterprise_tls(
    listener: TesseraListener,
    ctx: Arc<ServerContext>,
    shutdown: watch::Receiver<bool>,
    max_connections: usize,
    idle_timeout: Duration,
    default_tenant: String,
) -> Result<()> {
    let tls_acceptor = TlsAcceptor::from(Arc::clone(ctx.tls_config().server_config()));
    let shutdown_for_handlers = shutdown.clone();
    listener
        .serve_with(
            move |stream| {
                let acceptor = tls_acceptor.clone();
                let ctx = Arc::clone(&ctx);
                let shutdown_rx = shutdown_for_handlers.clone();
                let tenant = default_tenant.clone();
                async move {
                    let tls_stream = match acceptor.accept(stream).await {
                        Ok(s) => s,
                        Err(e) => {
                            tracing::warn!("TLS handshake failed: {e}");
                            return;
                        }
                    };
                    match BoltConnectionHandler::new_with_handshake(
                        tls_stream,
                        ctx,
                        tenant,
                        idle_timeout,
                        shutdown_rx,
                    )
                    .await
                    {
                        Ok(mut handler) => {
                            if let Err(e) = handler.run().await {
                                tracing::warn!("connection handler error: {e}");
                            }
                        }
                        Err(e) => {
                            tracing::warn!("Bolt handshake failed on TLS stream: {e}");
                        }
                    }
                }
            },
            shutdown,
            max_connections,
        )
        .await
        .map_err(|e| crate::error::ServerError::Io(std::io::Error::other(e.to_string())))
}
