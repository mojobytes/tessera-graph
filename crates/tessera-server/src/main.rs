// Copyright 2026 BelowZero Security OU. All rights reserved.

//! TCP server binary for tessera-graph-enterprise.
//!
//! Starts a TLS-enabled TCP server with mandatory authentication.

use std::sync::{Arc, RwLock};
use std::time::Duration;

use tessera_audit::AuditLog;
use tessera_auth::credentials::{Password, PasswordPolicy};
use tessera_auth::policy::AuthPolicy;
use tessera_auth::rbac::RoleStoreHandle;
use tessera_auth::session::SessionManager;
use tessera_auth::user::UserStoreHandle;
use tessera_graph::Graph;
use tessera_protocol::tls::TlsConfigBuilder;
use tessera_server::context::ServerContext;
use tessera_server::listener::TesseraListener;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let bind_addr = std::env::var("TESSERA_BIND").unwrap_or_else(|_| "127.0.0.1:7687".into());
    let cert_path = std::env::var("TESSERA_TLS_CERT").unwrap_or_else(|_| "certs/server.pem".into());
    let key_path = std::env::var("TESSERA_TLS_KEY").unwrap_or_else(|_| "certs/server.key".into());
    let max_connections: usize = std::env::var("TESSERA_MAX_CONNECTIONS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(256);
    let idle_timeout_secs: u64 = std::env::var("TESSERA_IDLE_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(300);

    // --- TLS (mandatory) ---
    let tls = TlsConfigBuilder::new()
        .cert_file(&cert_path)
        .key_file(&key_path)
        .build()
        .expect("TLS configuration failed — server cannot start without TLS");

    // --- Auth ---
    let policy = PasswordPolicy::default();
    let admin_pw = Password::new(
        &std::env::var("TESSERA_ADMIN_PASSWORD").expect("TESSERA_ADMIN_PASSWORD must be set"),
    )
    .expect("invalid admin password");
    let user_store =
        Arc::new(UserStoreHandle::new("admin", &admin_pw, &policy).expect("user store init"));
    let role_store = RoleStoreHandle::with_defaults();
    let auth_policy = Arc::new(AuthPolicy::new(Arc::clone(&user_store), role_store));
    let sessions = Arc::new(SessionManager::new(3600));

    // --- Audit ---
    let audit_path = std::env::var("TESSERA_AUDIT_PATH").unwrap_or_else(|_| "audit.ndjson".into());
    let audit =
        Arc::new(AuditLog::open(std::path::Path::new(&audit_path)).expect("audit log init"));

    // --- Graph ---
    let graph = Arc::new(RwLock::new(Graph::new()));

    // --- Metrics ---
    let metrics = Arc::new(tessera_monitor::MetricsRegistry::new(max_connections as u64));

    // --- Metrics HTTP server (optional) ---
    if let Ok(metrics_bind) = std::env::var("TESSERA_METRICS_BIND") {
        tracing::info!("Prometheus metrics on {metrics_bind}");
        let m = Arc::clone(&metrics);
        tokio::spawn(async move {
            if let Err(e) = tessera_monitor::serve_metrics(&metrics_bind, m).await {
                tracing::error!("metrics server failed: {e}");
            }
        });
    }

    // --- Server context ---
    let ctx = Arc::new(ServerContext::new(
        auth_policy,
        sessions,
        audit,
        tls,
        user_store,
        metrics,
    ));

    // --- Shutdown signal ---
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    tokio::spawn(async move {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
        tracing::info!("shutting down");
        let _ = shutdown_tx.send(true);
    });

    // --- Listen ---
    let listener = TesseraListener::bind(&bind_addr)
        .await
        .expect("failed to bind");
    let addr = listener.local_addr().expect("local addr");
    tracing::info!("TesseraGraph listening on {addr} (TLS)");

    if let Err(e) = listener
        .serve_tls(
            ctx,
            graph,
            shutdown_rx,
            max_connections,
            Duration::from_secs(idle_timeout_secs),
        )
        .await
    {
        tracing::error!("server error: {e}");
    }
}
