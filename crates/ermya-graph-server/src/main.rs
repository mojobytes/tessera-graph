// SPDX-License-Identifier: BSL-1.1

//! `ErmyaGraph` Server — standalone entry point.

use std::collections::HashMap;

use tokio::sync::watch;
use tracing::{info, warn};

use ermya_graph_server::config::ServerConfig;
use ermya_graph_server::edition_community::{parse_paid_settings, start_server};

/// Default path for the optional TOML configuration file. Absolute by design
/// (a daemon must not depend on its working directory); override with the
/// `ERMYA_CONFIG_FILE` environment variable.
const DEFAULT_CONFIG_PATH: &str = "/etc/ermya/ermya.toml";

/// Los ajustes leídos de un fichero más el entorno, con los del gestor
/// multi-base ya puestos por la edición.
///
/// **La factoría recibe la mezcla, no sólo el entorno.** Antes este camino no
/// llamaba siquiera a la edición: un servidor arrancado con fichero de ajustes
/// se quedaba con los siete valores por defecto del gestor multi-base y no
/// aplicaba ninguno de los escritos en el fichero, sin decir nada. Es
/// exactamente el fallo que la separación de ediciones quería evitar — un
/// ajuste aceptado y no aplicado — y estaba en el camino que usa cualquier
/// despliegue de verdad.
fn paid_config_from_file(path: &str, env: &HashMap<String, String>) -> ServerConfig {
    ServerConfig::from_file_and_env(path, env).with_paid_settings(parse_paid_settings(
        &ServerConfig::merged_env_map(path, env),
    ))
}

/// Build the [`ServerConfig`], merging an optional TOML file with the
/// environment (env wins over file, file wins over defaults).
///
/// Resolution:
/// - If `ERMYA_CONFIG_FILE` is set, that path is authoritative: a missing
///   or unreadable file there is an operator error and aborts startup.
/// - Otherwise the default `/etc/ermya/ermya.toml` is consulted; its
///   absence is normal (env-only deployments) and is not an error.
fn load_config() -> ServerConfig {
    let env: HashMap<String, String> = std::env::vars()
        .filter(|(k, _)| k.starts_with("ERMYA_") && k != "ERMYA_CONFIG_FILE")
        .collect();

    match std::env::var("ERMYA_CONFIG_FILE") {
        Ok(path) => {
            if !std::path::Path::new(&path).is_file() {
                eprintln!(
                    "ERMYA_CONFIG_FILE points to '{path}' but no readable file \
                     exists there; refusing to start with a misconfigured path"
                );
                std::process::exit(1);
            }
            info!(config_file = %path, "loading configuration from ERMYA_CONFIG_FILE");
            paid_config_from_file(&path, &env)
        }
        Err(_) => {
            if std::path::Path::new(DEFAULT_CONFIG_PATH).is_file() {
                info!(
                    config_file = DEFAULT_CONFIG_PATH,
                    "loading configuration from default file"
                );
                paid_config_from_file(DEFAULT_CONFIG_PATH, &env)
            } else {
                warn!(
                    default_path = DEFAULT_CONFIG_PATH,
                    "no config file found; using environment variables and defaults"
                );
                // Los ajustes del gestor multi-base los lee la edición, no
                // este fichero: la de pago con su analizador, la pública
                // devolviendo los de por defecto porque no tiene gestor que
                // gobernar. El binario transporta lo que salga sin mirarlo.
                ServerConfig::from_map(&env).with_paid_settings(parse_paid_settings(&env))
            }
        }
    }
}

/// Initialise the global tracing subscriber from environment.
///
/// `ERMYA_LOG_FORMAT=json` switches to the `tracing-subscriber` JSON
/// formatter (Loki / Elastic / Datadog ingestion); anything else (or
/// unset) keeps the human-readable text format. Filter directives come
/// from `ERMYA_LOG_FILTER` first, then the legacy `RUST_LOG`, then a
/// default of `info`. An unparseable filter falls back to `info` rather
/// than failing startup.
fn init_tracing() {
    let filter_str = std::env::var("ERMYA_LOG_FILTER")
        .or_else(|_| std::env::var("RUST_LOG"))
        .unwrap_or_else(|_| "info".to_owned());
    let env_filter = tracing_subscriber::EnvFilter::try_new(&filter_str)
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    if std::env::var("ERMYA_LOG_FORMAT").as_deref() == Ok("json") {
        tracing_subscriber::fmt()
            .json()
            .with_env_filter(env_filter)
            .init();
    } else {
        tracing_subscriber::fmt().with_env_filter(env_filter).init();
    }
}

#[tokio::main]
async fn main() {
    init_tracing();

    let config = load_config();
    info!(
        bind = %config.bind_addr,
        tls = config.tls_cert.is_some(),
        auth = config.password.is_some(),
        data_dir = ?config.data_dir,
        "starting ErmyaGraph Server"
    );

    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    // Spawn signal handler.
    tokio::spawn(async move {
        shutdown_signal().await;
        info!("shutdown signal received");
        let _ = shutdown_tx.send(true);
    });

    if let Err(e) = start_server(config, shutdown_rx).await {
        eprintln!("server error: {e}");
        std::process::exit(1);
    }
}

/// Wait for a shutdown signal (Ctrl-C on all platforms, SIGTERM on Unix).
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut sigterm = signal(SignalKind::terminate()).expect("failed to register SIGTERM");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = sigterm.recv() => {}
        }
    }

    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to listen for Ctrl-C");
    }
}
