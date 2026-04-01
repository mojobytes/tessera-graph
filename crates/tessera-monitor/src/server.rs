// Copyright 2026 BelowZero Security OU. All rights reserved.

//! Minimal HTTP/1.1 server for the Prometheus `/metrics` endpoint.
//!
//! Uses raw `tokio::net::TcpListener` — no external HTTP framework dependency.
//! Only handles `GET /metrics`; all other paths return 404.

use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::health::HealthProvider;
use crate::registry::MetricsRegistry;
use crate::render::render_prometheus;

/// Maximum request size (8 KiB) — security guard against memory exhaustion.
const MAX_REQUEST_SIZE: usize = 8192;

/// Start the metrics HTTP server on the given address.
///
/// This function binds a TCP listener and serves forever (until the task is cancelled).
/// It is designed to be spawned as a background task.
///
/// # Errors
///
/// Returns `io::Error` if the address cannot be bound.
pub async fn serve_metrics(
    addr: &str,
    registry: Arc<MetricsRegistry>,
    health: Arc<dyn HealthProvider>,
) -> std::io::Result<()> {
    let listener = TcpListener::bind(addr).await?;
    serve_metrics_on(listener, registry, health).await;
    Ok(())
}

/// Serve metrics on a pre-bound listener (useful for testing with port 0).
pub async fn serve_metrics_on(
    listener: TcpListener,
    registry: Arc<MetricsRegistry>,
    health: Arc<dyn HealthProvider>,
) {
    loop {
        let Ok((stream, _addr)) = listener.accept().await else {
            continue;
        };
        let reg = Arc::clone(&registry);
        let h = Arc::clone(&health);
        tokio::spawn(async move {
            let _ = handle_connection(stream, &reg, h.as_ref()).await;
        });
    }
}

async fn handle_connection(
    mut stream: TcpStream,
    registry: &MetricsRegistry,
    health: &dyn HealthProvider,
) -> std::io::Result<()> {
    // Read request headers (up to MAX_REQUEST_SIZE)
    let mut buf = vec![0u8; MAX_REQUEST_SIZE];
    let mut total = 0;

    loop {
        if total >= MAX_REQUEST_SIZE {
            // Request too large — drop connection
            return Ok(());
        }
        let n = stream.read(&mut buf[total..]).await?;
        if n == 0 {
            return Ok(());
        }
        total += n;

        // Check if we've received the full header (ends with \r\n\r\n)
        if buf[..total].windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }

    let request = String::from_utf8_lossy(&buf[..total]);
    let first_line = request.lines().next().unwrap_or("");

    if first_line.starts_with("GET /metrics") {
        let body = render_prometheus(registry);
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain; version=0.0.4\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body,
        );
        stream.write_all(response.as_bytes()).await?;
    } else if first_line.starts_with("GET /health") {
        let version = env!("CARGO_PKG_VERSION");
        let (status, body) = if health.is_healthy() {
            ("200 OK", format!(r#"{{"status":"healthy","version":"{version}"}}"#))
        } else {
            ("503 Service Unavailable", format!(r#"{{"status":"degraded","version":"{version}"}}"#))
        };
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len(),
        );
        stream.write_all(response.as_bytes()).await?;
    } else {
        let response = "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
        stream.write_all(response.as_bytes()).await?;
    }

    stream.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::health::StaticHealth;
    use std::time::Duration;

    fn healthy() -> Arc<dyn HealthProvider> {
        Arc::new(StaticHealth::new(true))
    }

    fn degraded() -> Arc<dyn HealthProvider> {
        Arc::new(StaticHealth::new(false))
    }

    async fn spawn_server(
        registry: Arc<MetricsRegistry>,
        health: Arc<dyn HealthProvider>,
    ) -> std::net::SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind"); // OK: test
        let addr = listener.local_addr().expect("addr"); // OK: test
        tokio::spawn(async move {
            serve_metrics_on(listener, registry, health).await;
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        addr
    }

    async fn http_get(addr: std::net::SocketAddr, path: &str) -> String {
        let mut stream = TcpStream::connect(addr).await.expect("connect"); // OK: test
        let req = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\n\r\n");
        stream.write_all(req.as_bytes()).await.expect("write"); // OK: test
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).await.expect("read"); // OK: test
        String::from_utf8(buf).expect("utf8") // OK: test
    }

    #[tokio::test]
    async fn get_metrics_returns_200_with_prometheus_content_type() {
        let registry = Arc::new(MetricsRegistry::new(256));
        let addr = spawn_server(Arc::clone(&registry), healthy()).await;
        let response = http_get(addr, "/metrics").await;

        assert!(response.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(response.contains("Content-Type: text/plain; version=0.0.4\r\n"));
        assert!(response.contains("tessera_connections_max 256"));
    }

    #[tokio::test]
    async fn non_known_path_returns_404() {
        let registry = Arc::new(MetricsRegistry::new(256));
        let addr = spawn_server(registry, healthy()).await;
        let response = http_get(addr, "/unknown").await;

        assert!(response.starts_with("HTTP/1.1 404 Not Found\r\n"));
    }

    #[tokio::test]
    async fn serve_metrics_invalid_address_returns_error() {
        let registry = Arc::new(MetricsRegistry::new(64));
        let result = serve_metrics("not-a-valid-address:99999", registry, healthy()).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn metrics_reflect_live_counter_updates() {
        let registry = Arc::new(MetricsRegistry::new(100));
        registry
            .connections_accepted
            .fetch_add(42, std::sync::atomic::Ordering::Relaxed);
        let addr = spawn_server(Arc::clone(&registry), healthy()).await;
        let response = http_get(addr, "/metrics").await;

        assert!(response.contains("tessera_connections_accepted_total 42"));
    }

    // --- Health endpoint tests ---

    #[tokio::test]
    async fn get_health_returns_200_when_healthy() {
        let registry = Arc::new(MetricsRegistry::new(64));
        let addr = spawn_server(registry, healthy()).await;
        let response = http_get(addr, "/health").await;

        assert!(response.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(response.contains("Content-Type: application/json\r\n"));
        assert!(response.contains(r#""status":"healthy""#));
        assert!(response.contains(&format!(r#""version":"{}""#, env!("CARGO_PKG_VERSION"))));
    }

    #[tokio::test]
    async fn get_health_returns_503_when_degraded() {
        let registry = Arc::new(MetricsRegistry::new(64));
        let addr = spawn_server(registry, degraded()).await;
        let response = http_get(addr, "/health").await;

        assert!(response.starts_with("HTTP/1.1 503 Service Unavailable\r\n"));
        assert!(response.contains("Content-Type: application/json\r\n"));
        assert!(response.contains(r#""status":"degraded""#));
    }
}
