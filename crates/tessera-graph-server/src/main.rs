// Copyright 2026 BelowZero Security OU. All rights reserved.

//! TCP server binary for tessera-graph-enterprise.
//!
//! Starts a TLS-enabled TCP server speaking the Bolt 4.4 protocol with
//! mandatory authentication and LBAC enforcement.

use std::sync::Arc;
use std::time::Duration;

use tessera_graph_audit::AuditLog;
use tessera_graph_config::AuditConfig;
use tessera_graph_auth::credentials::{Password, PasswordPolicy};
use tessera_graph_auth::policy::AuthPolicy;
use tessera_graph_auth::rbac::RoleStoreHandle;
use tessera_graph_auth::session::SessionManager;
use tessera_graph_auth::user::UserStoreHandle;
use tessera_graph_protocol::tls::TlsConfigBuilder;
use tessera_graph_server::config::PersistenceConfig;
use tessera_graph_server::context::ServerContext;
use tessera_graph_server::listener::TesseraListener;
use tessera_graph_tenant::TenantRegistry;

#[tokio::main]
#[allow(clippy::too_many_lines)] // Server bootstrap — splitting would obscure the startup sequence.
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
    let audit_config = AuditConfig::from_env();
    let audit = if audit_config.enabled {
        let audit_sync = std::env::var("TESSERA_AUDIT_SYNC")
            .map(|v| v != "false" && v != "0")
            .unwrap_or(true);
        let (log, writer_task) = AuditLog::open_with_sync(
            &audit_config.log_path,
            audit_config.rotation_max_size_bytes,
            audit_config.max_rotated_files,
            audit_config.channel_capacity,
            audit_sync,
        )
        .expect("audit log init"); // OK: server cannot start without audit
        tokio::spawn(writer_task.run());
        Arc::new(log)
    } else {
        tracing::warn!("audit logging is DISABLED — set TESSERA_AUDIT_ENABLED=true for production");
        Arc::new(AuditLog::new_null())
    };

    // --- Tenant registry (replaces single-graph approach) ---
    let persistence = PersistenceConfig::from_env();
    let flush_interval_ms = persistence.flush_interval_ms;
    let query_cache_capacity = persistence.query_cache_capacity;
    let base_dir = persistence
        .data_dir
        .unwrap_or_else(|| std::env::temp_dir().join("tessera-data"));
    tracing::info!("tenant data dir: {}", base_dir.display());
    let max_loaded_tenants: usize = std::env::var("TESSERA_MAX_LOADED_TENANTS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let registry = Arc::new(TenantRegistry::new_with_cap(
        &base_dir,
        persistence.graph_config,
        max_loaded_tenants,
    ));
    let default_tenant = persistence.default_tenant;

    // --- Metrics ---
    let metrics = Arc::new(tessera_graph_monitor::MetricsRegistry::new(
        max_connections as u64,
    ));

    // --- Health flag (shared with flush task and metrics server) ---
    let health = Arc::new(tessera_graph_monitor::AtomicHealthFlag::new());

    // --- Metrics HTTP server (optional) ---
    if let Ok(metrics_bind) = std::env::var("TESSERA_METRICS_BIND") {
        let metrics_token = std::env::var("TESSERA_METRICS_TOKEN").ok();
        if metrics_token.is_none() {
            tracing::warn!(
                "TESSERA_METRICS_TOKEN not set — metrics endpoint is unauthenticated. \
                 Set this variable to require Bearer token auth on /metrics and /health."
            );
        }
        tracing::info!("Prometheus metrics + health on {metrics_bind}");
        let m = Arc::clone(&metrics);
        let h = Arc::clone(&health);
        tokio::spawn(async move {
            if let Err(e) =
                tessera_graph_monitor::serve_metrics(&metrics_bind, m, h, metrics_token).await
            {
                tracing::error!("metrics server failed: {e}");
            }
        });
    }

    // --- Clones for background metrics/cleanup task ---
    let sessions_bg = Arc::clone(&sessions);
    let audit_bg = Arc::clone(&audit);

    // --- Server context ---
    let ctx = Arc::new(ServerContext::new(
        auth_policy,
        sessions,
        audit,
        tls,
        user_store,
        metrics,
        Arc::clone(&registry),
        query_cache_capacity,
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

    // --- Background flush (WAL ensures durability; this amortises page-file I/O) ---
    let _flush_handle = tessera_graph_server::flush_task::spawn_background_flush(
        Arc::clone(&registry),
        flush_interval_ms,
        shutdown_rx.clone(),
        Arc::clone(&health),
        base_dir.clone(),
    );

    // --- Background metrics + session cleanup (30s interval) ---
    {
        let metrics_bg = Arc::clone(ctx.metrics());
        let registry_bg = Arc::clone(&registry);
        let mut shutdown_bg = shutdown_rx.clone();

        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(30));
            interval.tick().await; // skip immediate first tick
            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        // Session cleanup (CRITICAL #4)
                        let purged = sessions_bg.purge_expired();
                        if purged > 0 {
                            tracing::info!(purged, "expired sessions purged");
                        }

                        // System metrics (MEDIUM #15)
                        metrics_bg.tenants_loaded.store(
                            registry_bg.loaded_count() as u64,
                            std::sync::atomic::Ordering::Relaxed,
                        );
                        metrics_bg.audit_entries_dropped.store(
                            audit_bg.dropped_count(),
                            std::sync::atomic::Ordering::Relaxed,
                        );

                        // RSS and FD count (platform-specific, best-effort)
                        #[cfg(target_os = "linux")]
                        {
                            if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
                                if let Some(line) = status.lines().find(|l| l.starts_with("VmRSS:")) {
                                    let kb: u64 = line.split_whitespace()
                                        .nth(1)
                                        .and_then(|v| v.parse().ok())
                                        .unwrap_or(0);
                                    metrics_bg.process_rss_bytes.store(
                                        kb * 1024,
                                        std::sync::atomic::Ordering::Relaxed,
                                    );
                                }
                            }
                            if let Ok(entries) = std::fs::read_dir("/proc/self/fd") {
                                metrics_bg.open_fds.store(
                                    entries.count() as u64,
                                    std::sync::atomic::Ordering::Relaxed,
                                );
                            }
                        }
                        #[cfg(target_os = "macos")]
                        {
                            // On macOS, use libc::proc_pidinfo for RSS.
                            // Simplified: just report 0 (metric present but empty).
                            // FD count: not trivially available without lsof.
                        }
                    }
                    _ = shutdown_bg.changed() => {
                        break;
                    }
                }
            }
        });
    }

    // --- Listen ---
    let listener = TesseraListener::bind(&bind_addr)
        .await
        .expect("failed to bind");
    let addr = listener.local_addr().expect("local addr");
    tracing::info!("TesseraGraph listening on {addr} (TLS, Bolt 4.4)");

    if let Err(e) = listener
        .serve_tls(
            ctx,
            shutdown_rx,
            max_connections,
            Duration::from_secs(idle_timeout_secs),
            default_tenant,
        )
        .await
    {
        tracing::error!("server error: {e}");
    }

    // --- Graceful shutdown: flush all databases to disk ---
    tessera_graph_server::shutdown::flush_all_on_shutdown(&registry);
}
