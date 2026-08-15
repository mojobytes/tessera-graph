// SPDX-License-Identifier: BSL-1.1

//! Unit tests for [`ServerConfig`].

use std::collections::HashMap;
use std::io::Write as _;
use std::path::PathBuf;

use ermya_graph_server::config::{AuditSinkKind, ServerConfig};

// ── Cycle 4.1: Defaults ─────────────────────────────────────────────────────

#[test]
fn server_config_default_has_expected_values() {
    let cfg = ServerConfig::default();
    assert_eq!(cfg.bind_addr, "127.0.0.1:7687");
    assert!(cfg.tls_cert.is_none());
    assert!(cfg.tls_key.is_none());
    assert!(cfg.password.is_none());
    // v0.7.0: default is now file-backed, not in-memory.
    assert_eq!(cfg.data_dir, Some(PathBuf::from("/var/lib/ermya/data")));
    assert_eq!(cfg.max_connections, 256);
    assert_eq!(cfg.idle_timeout_secs, 300);
    assert_eq!(cfg.audit_sink, AuditSinkKind::Stdout);
    assert!(cfg.audit_file.is_none());
    assert_eq!(cfg.audit_max_bytes, 100_000_000);
    assert_eq!(cfg.audit_keep_files, 10);
    assert_eq!(cfg.audit_fsync_every, 0);
    assert!(!cfg.no_auth);
    // v0.7.0 Block 1: default agent string is Neo4j/<semver> so the official
    // Neo4j Python driver connects without patching its product check.
    assert_eq!(
        cfg.server_agent,
        format!("Neo4j/{}", env!("CARGO_PKG_VERSION")),
        "server_agent default must be Neo4j/<semver> for Python driver compat"
    );
}

// ── Block 1: server_agent ───────────────────────────────────────────────────

#[test]
fn from_map_parses_server_agent() {
    let mut m = HashMap::new();
    m.insert("ERMYA_SERVER_AGENT".into(), "ErmyaGraph/0.7.0".into());
    let cfg = ServerConfig::from_map(&m);
    assert_eq!(cfg.server_agent, "ErmyaGraph/0.7.0");
}

#[test]
fn server_agent_default_survives_empty_map() {
    let cfg = ServerConfig::from_map(&HashMap::new());
    assert!(
        cfg.server_agent.starts_with("Neo4j/"),
        "default server_agent must start with Neo4j/, got {:?}",
        cfg.server_agent,
    );
}

// ── Cycle 4.2: from_map ─────────────────────────────────────────────────────

#[test]
fn from_map_parses_all_fields() {
    let mut m = HashMap::new();
    m.insert("ERMYA_BIND".into(), "0.0.0.0:9999".into());
    m.insert("ERMYA_TLS_CERT".into(), "/tmp/cert.pem".into());
    m.insert("ERMYA_TLS_KEY".into(), "/tmp/key.pem".into());
    m.insert("ERMYA_PASSWORD".into(), "s3cret".into());
    m.insert("ERMYA_DATA_DIR".into(), "/data/ermya".into());
    m.insert("ERMYA_MAX_CONNECTIONS".into(), "512".into());
    m.insert("ERMYA_IDLE_TIMEOUT".into(), "600".into());

    let cfg = ServerConfig::from_map(&m);

    assert_eq!(cfg.bind_addr, "0.0.0.0:9999");
    assert_eq!(cfg.tls_cert, Some(PathBuf::from("/tmp/cert.pem")));
    assert_eq!(cfg.tls_key, Some(PathBuf::from("/tmp/key.pem")));
    assert_eq!(cfg.password, Some("s3cret".to_owned()));
    assert_eq!(cfg.data_dir, Some(PathBuf::from("/data/ermya")));
    assert_eq!(cfg.max_connections, 512);
    assert_eq!(cfg.idle_timeout_secs, 600);
}

#[test]
fn max_txn_memory_bytes_default_is_reasonable() {
    // The 64 MiB default lives in `impl Default` (same order of magnitude as
    // the buffer-pool page cap). `from_map`, like the other optional quotas,
    // treats an absent key as "unlimited" (`None`) rather than re-applying the
    // Default, so the default is asserted through `ServerConfig::default`.
    assert_eq!(
        ServerConfig::default().max_txn_memory_bytes,
        Some(64 * 1024 * 1024)
    );
    assert_eq!(
        ServerConfig::from_map(&HashMap::new()).max_txn_memory_bytes,
        None
    );
}

#[test]
fn max_txn_memory_bytes_parsed_from_env() {
    let m = HashMap::from([(
        "ERMYA_MAX_TXN_MEMORY_BYTES".to_owned(),
        "1048576".to_owned(),
    )]);
    let cfg = ServerConfig::from_map(&m);
    assert_eq!(cfg.max_txn_memory_bytes, Some(1_048_576));
}

// ── Issue #37: batch caps ────────────────────────────────────────────────────

#[test]
fn default_config_has_batch_limit_defaults() {
    let cfg = ServerConfig::default();
    assert_eq!(cfg.max_batch_operations, Some(100_000));
    assert_eq!(cfg.max_batch_memory_bytes, Some(256 * 1024 * 1024));
}

#[test]
fn from_map_parses_batch_limit_env_vars() {
    let m = HashMap::from([
        ("ERMYA_MAX_BATCH_OPERATIONS".to_owned(), "5000".to_owned()),
        (
            "ERMYA_MAX_BATCH_MEMORY_BYTES".to_owned(),
            "1048576".to_owned(),
        ),
    ]);
    let cfg = ServerConfig::from_map(&m);
    assert_eq!(cfg.max_batch_operations, Some(5000));
    assert_eq!(cfg.max_batch_memory_bytes, Some(1_048_576));
}

#[test]
fn from_map_missing_batch_limit_vars_falls_back_to_default_not_unlimited() {
    // Deliberate asymmetry vs `max_txn_memory_bytes` (which becomes `None` when
    // absent): a batch cap is a DoS guard, so an absent var must keep the
    // protective default, never disable the cap. "Secure by default."
    let cfg = ServerConfig::from_map(&HashMap::new());
    assert_eq!(cfg.max_batch_operations, Some(100_000));
    assert_eq!(cfg.max_batch_memory_bytes, Some(256 * 1024 * 1024));
}

#[test]
fn from_map_uses_defaults_for_missing_keys() {
    let m = HashMap::new();
    let cfg = ServerConfig::from_map(&m);

    assert_eq!(cfg.bind_addr, "127.0.0.1:7687");
    assert!(cfg.tls_cert.is_none());
    assert!(cfg.tls_key.is_none());
    assert!(cfg.password.is_none());
    // v0.7.0: default is now file-backed, not in-memory.
    assert_eq!(cfg.data_dir, Some(PathBuf::from("/var/lib/ermya/data")));
    assert_eq!(cfg.max_connections, 256);
    assert_eq!(cfg.idle_timeout_secs, 300);
}

#[test]
fn from_map_ignores_invalid_numeric_values_and_uses_defaults() {
    let mut m = HashMap::new();
    m.insert("ERMYA_MAX_CONNECTIONS".into(), "not-a-number".into());
    m.insert("ERMYA_IDLE_TIMEOUT".into(), String::new());

    let cfg = ServerConfig::from_map(&m);

    assert_eq!(cfg.max_connections, 256);
    assert_eq!(cfg.idle_timeout_secs, 300);
}

#[test]
fn from_map_partial_override() {
    let mut m = HashMap::new();
    m.insert("ERMYA_BIND".into(), "0.0.0.0:7688".into());
    m.insert("ERMYA_PASSWORD".into(), "pw".into());

    let cfg = ServerConfig::from_map(&m);

    assert_eq!(cfg.bind_addr, "0.0.0.0:7688");
    assert_eq!(cfg.password, Some("pw".to_owned()));
    // Remaining fields should be defaults.
    assert!(cfg.tls_cert.is_none());
    assert_eq!(cfg.max_connections, 256);
}

// ── Cycle 4.3: Audit + no_auth (Task 9) ─────────────────────────────────────

#[test]
fn audit_sink_defaults_to_stdout() {
    let cfg = ServerConfig::from_map(&HashMap::new());
    assert_eq!(cfg.audit_sink, AuditSinkKind::Stdout);
}

#[test]
fn audit_sink_off_parses() {
    let mut vars = HashMap::new();
    vars.insert("ERMYA_AUDIT_SINK".to_owned(), "off".to_owned());
    let cfg = ServerConfig::from_map(&vars);
    assert_eq!(cfg.audit_sink, AuditSinkKind::Off);
}

#[test]
fn audit_sink_file_parses() {
    let mut vars = HashMap::new();
    vars.insert("ERMYA_AUDIT_SINK".to_owned(), "file".to_owned());
    vars.insert(
        "ERMYA_AUDIT_FILE".to_owned(),
        "/var/log/ermya/audit.log".to_owned(),
    );
    vars.insert("ERMYA_AUDIT_MAX_BYTES".to_owned(), "50".to_owned());
    vars.insert("ERMYA_AUDIT_KEEP_FILES".to_owned(), "3".to_owned());
    vars.insert("ERMYA_AUDIT_FSYNC_EVERY".to_owned(), "100".to_owned());
    let cfg = ServerConfig::from_map(&vars);
    assert_eq!(cfg.audit_sink, AuditSinkKind::File);
    assert_eq!(
        cfg.audit_file,
        Some(PathBuf::from("/var/log/ermya/audit.log"))
    );
    assert_eq!(cfg.audit_max_bytes, 50);
    assert_eq!(cfg.audit_keep_files, 3);
    assert_eq!(cfg.audit_fsync_every, 100);
}

#[test]
fn audit_sink_unknown_falls_back_to_stdout() {
    let mut vars = HashMap::new();
    vars.insert("ERMYA_AUDIT_SINK".to_owned(), "cloudwatch".to_owned());
    let cfg = ServerConfig::from_map(&vars);
    assert_eq!(cfg.audit_sink, AuditSinkKind::Stdout);
}

#[test]
fn no_auth_flag_requires_exact_string_one() {
    let mut vars = HashMap::new();
    vars.insert("ERMYA_NO_AUTH".to_owned(), "1".to_owned());
    assert!(ServerConfig::from_map(&vars).no_auth);

    vars.insert("ERMYA_NO_AUTH".to_owned(), "true".to_owned());
    assert!(!ServerConfig::from_map(&vars).no_auth);

    vars.insert("ERMYA_NO_AUTH".to_owned(), "yes".to_owned());
    assert!(!ServerConfig::from_map(&vars).no_auth);

    vars.insert("ERMYA_NO_AUTH".to_owned(), "0".to_owned());
    assert!(!ServerConfig::from_map(&vars).no_auth);
}

// ── v0.5.0 Task 3: multi-database runtime keys ──────────────────────────────

#[test]
fn multi_database_defaults_match_spec() {
    let cfg = ServerConfig::default();
    assert_eq!(cfg.paid.idle_ttl_seconds, 900);
    assert_eq!(cfg.paid.default_max_connections, 100);
    assert_eq!(cfg.paid.default_max_size_bytes, None);
    assert_eq!(cfg.shutdown_timeout_seconds, 30);
    assert_eq!(cfg.paid.registry_sweep_interval_seconds, 60);
    assert_eq!(cfg.paid.ttl_disabled_warn_threshold, 50);
    assert_eq!(cfg.paid.max_open_databases, None);
}

// ── v0.5.0 Task 11 cycle 9: max_open_databases env wiring ───────────────────

#[test]
fn max_open_databases_default_is_none() {
    let vars: HashMap<String, String> = HashMap::new();
    let cfg = ServerConfig::from_map(&vars);
    assert_eq!(
        cfg.paid.max_open_databases, None,
        "absence of ERMYA_MAX_OPEN_DATABASES must mean unlimited"
    );
}

// ── v0.6.0 Fase 2 Task 1 cycle 1: metrics endpoint config ───────────────────

#[test]
fn metrics_addr_defaults_to_none() {
    let cfg = ServerConfig::default();
    assert!(
        cfg.metrics_addr.is_none(),
        "metrics endpoint must be disabled by default"
    );
}

#[test]
fn metrics_addr_parsed_from_env_var() {
    let mut m = HashMap::new();
    m.insert("ERMYA_METRICS_ADDR".into(), "0.0.0.0:9090".into());
    let cfg = ServerConfig::from_map(&m);
    assert_eq!(cfg.metrics_addr.as_deref(), Some("0.0.0.0:9090"));
}

#[test]
fn metrics_addr_absent_means_disabled() {
    let cfg = ServerConfig::from_map(&HashMap::new());
    assert!(cfg.metrics_addr.is_none());
}

// ── v0.6.0 Fase 2 Task 3: slow-query log config ─────────────────────────────

#[test]
fn slow_query_threshold_ms_defaults_to_one_thousand() {
    let cfg = ServerConfig::default();
    assert_eq!(cfg.slow_query_threshold_ms, 1000);
}

#[test]
fn slow_query_threshold_ms_reads_from_env_override() {
    let map = HashMap::from([("ERMYA_SLOW_QUERY_THRESHOLD_MS".to_owned(), "250".to_owned())]);
    let cfg = ServerConfig::from_map(&map);
    assert_eq!(cfg.slow_query_threshold_ms, 250);
}

#[test]
fn slow_query_threshold_ms_falls_back_to_default_on_invalid_value() {
    let map = HashMap::from([(
        "ERMYA_SLOW_QUERY_THRESHOLD_MS".to_owned(),
        "not-a-number".to_owned(),
    )]);
    let cfg = ServerConfig::from_map(&map);
    assert_eq!(cfg.slow_query_threshold_ms, 1000);
}

#[test]
fn max_slow_events_per_minute_defaults_to_sixty() {
    let cfg = ServerConfig::default();
    assert_eq!(cfg.max_slow_events_per_minute, 60);
}

#[test]
fn max_slow_events_per_minute_reads_from_env_override() {
    let map = HashMap::from([(
        "ERMYA_SLOW_QUERY_MAX_EVENTS_PER_MINUTE".to_owned(),
        "10".to_owned(),
    )]);
    let cfg = ServerConfig::from_map(&map);
    assert_eq!(cfg.max_slow_events_per_minute, 10);
}

#[test]
fn max_slow_events_per_minute_falls_back_to_default_on_invalid_value() {
    let map = HashMap::from([(
        "ERMYA_SLOW_QUERY_MAX_EVENTS_PER_MINUTE".to_owned(),
        "abc".to_owned(),
    )]);
    let cfg = ServerConfig::from_map(&map);
    assert_eq!(cfg.max_slow_events_per_minute, 60);
}

// ── Task 4 C1: max_result_rows (defensive result-row cap) ───────────────────

#[test]
fn max_result_rows_defaults_to_ten_million() {
    let cfg = ServerConfig::default();
    assert_eq!(
        cfg.max_result_rows, 10_000_000,
        "default cap must be 10M rows"
    );
}

#[test]
fn max_result_rows_parsed_from_env_map() {
    let map = HashMap::from([("ERMYA_MAX_RESULT_ROWS".to_owned(), "250000".to_owned())]);
    let cfg = ServerConfig::from_map(&map);
    assert_eq!(cfg.max_result_rows, 250_000);
}

#[test]
fn max_result_rows_zero_disables_cap() {
    let map = HashMap::from([("ERMYA_MAX_RESULT_ROWS".to_owned(), "0".to_owned())]);
    let cfg = ServerConfig::from_map(&map);
    assert_eq!(
        cfg.max_result_rows, 0,
        "0 must be accepted as the disable sentinel"
    );
}

#[test]
fn max_result_rows_invalid_env_falls_back_to_default() {
    let map = HashMap::from([(
        "ERMYA_MAX_RESULT_ROWS".to_owned(),
        "not-a-number".to_owned(),
    )]);
    let cfg = ServerConfig::from_map(&map);
    assert_eq!(cfg.max_result_rows, 10_000_000);
}

// ── v0.6.0 Task 5: rate limiting config ─────────────────────────────────────

#[test]
fn auth_max_failures_per_minute_defaults_to_5() {
    let cfg = ServerConfig::from_map(&HashMap::new());
    assert_eq!(
        cfg.auth_max_failures_per_minute, 5,
        "default auth failure cap should be 5/min, got {}",
        cfg.auth_max_failures_per_minute
    );
}

#[test]
fn auth_max_failures_per_minute_parsed_from_env_map() {
    let mut vars = HashMap::new();
    vars.insert(
        "ERMYA_AUTH_MAX_FAILURES_PER_MINUTE".to_owned(),
        "12".to_owned(),
    );
    let cfg = ServerConfig::from_map(&vars);
    assert_eq!(cfg.auth_max_failures_per_minute, 12);
}

#[test]
fn queries_max_per_second_defaults_to_100() {
    let cfg = ServerConfig::from_map(&HashMap::new());
    assert_eq!(cfg.queries_max_per_second, 100);
}

#[test]
fn queries_max_per_second_parsed_from_env_map() {
    let mut vars = HashMap::new();
    vars.insert("ERMYA_QUERIES_MAX_PER_SECOND".to_owned(), "50".to_owned());
    let cfg = ServerConfig::from_map(&vars);
    assert_eq!(cfg.queries_max_per_second, 50);
}

#[test]
fn max_connections_per_ip_defaults_to_16() {
    let cfg = ServerConfig::from_map(&HashMap::new());
    assert_eq!(cfg.max_connections_per_ip, 16);
}

#[test]
fn max_connections_per_ip_parsed_from_env_map() {
    let mut vars = HashMap::new();
    vars.insert("ERMYA_MAX_CONNECTIONS_PER_IP".to_owned(), "8".to_owned());
    let cfg = ServerConfig::from_map(&vars);
    assert_eq!(cfg.max_connections_per_ip, 8);
}

#[test]
fn max_bytes_per_second_defaults_to_1_mib() {
    let cfg = ServerConfig::from_map(&HashMap::new());
    assert_eq!(cfg.max_bytes_per_second, 1_048_576);
}

#[test]
fn max_bytes_per_second_parsed_from_env_map() {
    let mut vars = HashMap::new();
    vars.insert(
        "ERMYA_MAX_BYTES_PER_SECOND".to_owned(),
        "2097152".to_owned(),
    );
    let cfg = ServerConfig::from_map(&vars);
    assert_eq!(cfg.max_bytes_per_second, 2_097_152);
}

#[test]
fn rate_limit_ip_cap_defaults_to_256() {
    let cfg = ServerConfig::from_map(&HashMap::new());
    assert_eq!(cfg.rate_limit_ip_cap, 256);
}

#[test]
fn rate_limit_ip_cap_parsed_from_env_map() {
    let mut vars = HashMap::new();
    vars.insert("ERMYA_RATE_LIMIT_IP_CAP".to_owned(), "512".to_owned());
    let cfg = ServerConfig::from_map(&vars);
    assert_eq!(cfg.rate_limit_ip_cap, 512);
}

#[test]
fn rate_limit_invalid_env_falls_back_to_default() {
    let mut vars = HashMap::new();
    vars.insert(
        "ERMYA_QUERIES_MAX_PER_SECOND".to_owned(),
        "not-a-number".to_owned(),
    );
    let cfg = ServerConfig::from_map(&vars);
    assert_eq!(
        cfg.queries_max_per_second, 100,
        "invalid value falls back to default"
    );
}

// ── v0.6.0 Fase 2 Task 6: query timeout config ──────────────────────────────

#[test]
fn query_timeout_ms_defaults_to_zero_disabled() {
    let cfg = ServerConfig::default();
    assert_eq!(
        cfg.query_timeout_ms, 0,
        "query timeout must default to 0 (disabled, opt-in)"
    );
}

#[test]
fn query_timeout_ms_parsed_from_env_map() {
    let map = HashMap::from([("ERMYA_QUERY_TIMEOUT_MS".to_owned(), "5000".to_owned())]);
    let cfg = ServerConfig::from_map(&map);
    assert_eq!(cfg.query_timeout_ms, 5_000);
}

#[test]
fn query_timeout_ms_zero_keeps_timeout_disabled() {
    let map = HashMap::from([("ERMYA_QUERY_TIMEOUT_MS".to_owned(), "0".to_owned())]);
    let cfg = ServerConfig::from_map(&map);
    assert_eq!(
        cfg.query_timeout_ms, 0,
        "0 is the explicit disable sentinel"
    );
}

#[test]
fn query_timeout_ms_invalid_env_falls_back_to_default() {
    let map = HashMap::from([(
        "ERMYA_QUERY_TIMEOUT_MS".to_owned(),
        "not-a-number".to_owned(),
    )]);
    let cfg = ServerConfig::from_map(&map);
    assert_eq!(
        cfg.query_timeout_ms, 0,
        "invalid value falls back to the disabled default"
    );
}

#[test]
fn query_timeout_ms_does_not_perturb_other_defaults() {
    let map = HashMap::from([("ERMYA_QUERY_TIMEOUT_MS".to_owned(), "250".to_owned())]);
    let cfg = ServerConfig::from_map(&map);
    assert_eq!(cfg.query_timeout_ms, 250);
    // Sibling query-safety fields keep their defaults.
    assert_eq!(cfg.max_result_rows, 10_000_000);
    assert_eq!(cfg.queries_max_per_second, 100);
}

// ── v0.7.0 Fase 3 Feature B: TOML config file (Cycle 1) ──────────────────────

#[test]
fn file_config_is_ignored_when_no_toml_exists() {
    // A path that does not exist means "no file" — fall back to env-only.
    let cfg = ServerConfig::from_file_and_env("/nonexistent/ermya.toml", &HashMap::new());
    assert_eq!(
        cfg.bind_addr,
        ServerConfig::default().bind_addr,
        "missing config file must yield the env/default config, not an error"
    );
    assert_eq!(cfg.max_connections, ServerConfig::default().max_connections);
}

#[test]
fn file_config_parses_bind_and_max_connections() {
    let mut f = tempfile::NamedTempFile::new().unwrap();
    writeln!(f, "bind = \"0.0.0.0:8888\"").unwrap();
    writeln!(f, "max_connections = 64").unwrap();
    let path = f.path().to_str().unwrap();

    let cfg = ServerConfig::from_file_and_env(path, &HashMap::new());
    assert_eq!(cfg.bind_addr, "0.0.0.0:8888");
    assert_eq!(cfg.max_connections, 64);
}

#[test]
fn file_config_env_overrides_toml() {
    let mut f = tempfile::NamedTempFile::new().unwrap();
    writeln!(f, "max_connections = 64").unwrap();
    let path = f.path().to_str().unwrap();

    let env = HashMap::from([("ERMYA_MAX_CONNECTIONS".to_owned(), "128".to_owned())]);
    let cfg = ServerConfig::from_file_and_env(path, &env);
    assert_eq!(
        cfg.max_connections, 128,
        "env var must take precedence over the TOML file value"
    );
}

// ── v0.7.0 Fase 3 Feature B: exhaustive 30-field coverage (Cycle 4) ──────────
//
// These non-default values exercise EVERY mappable field. A typo in the
// `file_config_to_map` translation table (config.rs) silently drops a field to
// its default; the per-field asserts below name the exact field that failed
// instead of letting it slip into production unnoticed. ("Si es molesto, se
// mide" — exhaustive, not representative.)

// ── v0.7.0 Fase 3 Feature B: file-backed default + :memory: sentinel (C6) ────

#[test]
fn data_dir_default_is_file_backed() {
    let cfg = ServerConfig::default();
    assert_eq!(
        cfg.data_dir,
        Some(PathBuf::from("/var/lib/ermya/data")),
        "the default must be file-backed (persistent), not in-memory"
    );
}

#[test]
fn data_dir_memory_sentinel_produces_none() {
    let m = HashMap::from([("ERMYA_DATA_DIR".to_owned(), ":memory:".to_owned())]);
    let cfg = ServerConfig::from_map(&m);
    assert!(
        cfg.data_dir.is_none(),
        "ERMYA_DATA_DIR=:memory: must select in-memory mode (None)"
    );
}

#[test]
fn data_dir_explicit_path_overrides_default() {
    let m = HashMap::from([("ERMYA_DATA_DIR".to_owned(), "/custom/path".to_owned())]);
    let cfg = ServerConfig::from_map(&m);
    assert_eq!(cfg.data_dir, Some(PathBuf::from("/custom/path")));
}

#[test]
fn data_dir_absent_key_uses_file_backed_default() {
    // Omitting ERMYA_DATA_DIR must NOT mean in-memory — it uses the default.
    let cfg = ServerConfig::from_map(&HashMap::new());
    assert_eq!(
        cfg.data_dir,
        Some(PathBuf::from("/var/lib/ermya/data")),
        "an unset ERMYA_DATA_DIR must fall back to the file-backed default"
    );
}

#[test]
fn data_dir_memory_sentinel_in_toml_produces_none() {
    let mut f = tempfile::NamedTempFile::new().unwrap();
    writeln!(f, "data_dir = \":memory:\"").unwrap();
    let path = f.path().to_str().unwrap();
    let cfg = ServerConfig::from_file_and_env(path, &HashMap::new());
    assert!(
        cfg.data_dir.is_none(),
        ":memory: in a TOML data_dir must also select in-memory mode"
    );
}

// ── MVCC vacuum interval ─────────────────────────────────────────────────────

#[test]
fn vacuum_interval_defaults_to_300() {
    let cfg = ServerConfig::default();
    assert_eq!(cfg.vacuum_interval_seconds, 300);
}

#[test]
fn vacuum_interval_read_from_env() {
    let mut m = HashMap::new();
    m.insert("ERMYA_VACUUM_INTERVAL_SECONDS".to_owned(), "42".to_owned());
    let cfg = ServerConfig::from_map(&m);
    assert_eq!(cfg.vacuum_interval_seconds, 42);
}

#[test]
fn vacuum_interval_default_survives_empty_map() {
    let cfg = ServerConfig::from_map(&HashMap::new());
    assert_eq!(cfg.vacuum_interval_seconds, 300);
}

// ── Ajustes que sólo la edición de pago aplica ──────────────────────────────
//
// Siete ajustes gobiernan el gestor multi-base: expulsión por inactividad,
// topes por base, barrido, tope de bases abiertas, sondeo de medidas por base.
// Un servidor Community no tiene ninguna de esas cosas, así que los acepta al
// arrancar y no hacen nada.
//
// Ese es el patrón "ajuste que se lee y nunca llega al motor" que este código
// ya ha sufrido cuatro veces. La diferencia es que aquí no se puede "cablear":
// la funcionalidad no existe en esa edición. Lo que sí se puede —y se debe— es
// que el operador se entere en vez de creer que su tope está aplicado.

/// Todo ajuste está clasificado: o lo aplica cualquier edición, o se reporta.
///
/// La comprobación que impide que el patrón vuelva. Alguien añade un ajuste
/// que sólo consume el gestor multi-base, no lo mete en `paid_settings_in_use`,
/// y Community vuelve a aceptar en silencio algo que no hace — sin que ningún
/// test se entere, porque los de arriba sólo miran los siete que hay hoy.
///
/// La lista de abajo es el inventario de los 36 ajustes. Añadir uno nuevo
/// rompe este test hasta que se decida a qué lado pertenece, que es
/// exactamente la decisión que se olvida cuando no hay nada que la exija.
/// Los nombres de campo declarados en un fichero de configuración, leídos de su
/// propio texto.
///
/// Se lee el fuente en vez de usar reflexión porque en Rust no la hay: es la
/// única forma de que añadir un campo y olvidar clasificarlo haga fallar algo.
fn declared_fields(source: &str) -> Vec<String> {
    source
        .lines()
        .filter_map(|l| {
            let t = l.trim();
            t.strip_prefix("pub ")
                .and_then(|r| r.split(':').next())
                .filter(|name| {
                    !name.is_empty()
                        && name.chars().all(|c| c.is_ascii_lowercase() || c == '_')
                        && l.starts_with("    pub ")
                })
                .map(str::to_owned)
        })
        .collect()
}

#[test]
fn every_setting_is_classified_by_edition() {
    // Los siete que gobiernan el gestor multi-base. Desde el 2026-07-28 no
    // viven en la estructura común sino en la suya, analizada por factoría
    // (apartado 5.10 del inventario): esta lista debe coincidir uno a uno con
    // los campos declarados en `config_enterprise.rs`.
    const PAID: &[&str] = &[
        "idle_ttl_seconds",
        "default_max_connections",
        "default_max_size_bytes",
        "registry_sweep_interval_seconds",
        "ttl_disabled_warn_threshold",
        "max_open_databases",
        "metrics_poll_interval",
    ];

    // Los que aplica cualquier edición: red, identidad, auditoría básica,
    // topes del motor, límites de tráfico.
    const COMMON: &[&str] = &[
        "bind_addr",
        "tls_cert",
        "tls_key",
        "password",
        "data_dir",
        "max_connections",
        "idle_timeout_secs",
        "audit_sink",
        "audit_file",
        "audit_max_bytes",
        "audit_keep_files",
        "audit_fsync_every",
        "no_auth",
        "max_txn_memory_bytes",
        "max_batch_operations",
        "max_batch_memory_bytes",
        "shutdown_timeout_seconds",
        "vacuum_interval_seconds",
        "metrics_addr",
        "slow_query_threshold_ms",
        "max_slow_events_per_minute",
        "max_result_rows",
        "auth_max_failures_per_minute",
        "queries_max_per_second",
        "max_connections_per_ip",
        "max_bytes_per_second",
        "rate_limit_ip_cap",
        "query_timeout_ms",
        "server_agent",
    ];

    // Los campos reales, leídos de la declaración del tipo. Si alguien añade
    // uno y no lo clasifica, este recuento deja de cuadrar.
    let declared = declared_fields(include_str!("../src/config.rs"));

    // Lo mismo para los del gestor multi-base. Se leen del fichero que declara
    // su **forma**, no del que la analiza: al partirlos en dos (apartado 5.10
    // del inventario) esta ruta se quedó apuntando a la factoría, que no
    // declara ningún campo. El recuento salía 0 contra 7 esperados.
    let declared_paid = declared_fields(include_str!("../src/config_paid_settings.rs"));

    // El campo que agrupa los de pago dentro de la estructura común no es un
    // ajuste: es el transporte. Se descuenta para que los recuentos cuadren.
    let declared_common: Vec<&String> = declared.iter().filter(|d| *d != "paid").collect();

    let unclassified: Vec<&&String> = declared_common
        .iter()
        .filter(|d| !COMMON.contains(&d.as_str()))
        .collect();
    assert!(
        unclassified.is_empty(),
        "ajuste(s) comunes sin clasificar: {unclassified:?}\n\
         Cada uno va a COMMON (lo aplica cualquier edición) o, si sólo lo \
         aplica el gestor multi-base, se declara en `config_enterprise.rs` \
         para que no viaje al árbol público."
    );

    let unclassified_paid: Vec<&String> = declared_paid
        .iter()
        .filter(|d| !PAID.contains(&d.as_str()))
        .collect();
    assert!(
        unclassified_paid.is_empty(),
        "ajuste(s) de pago sin clasificar: {unclassified_paid:?}"
    );

    assert_eq!(
        declared_common.len(),
        COMMON.len(),
        "la lista COMMON tiene {} ajustes y la estructura común declara {}: \
         sobra o falta alguno.\ndeclarados: {declared_common:?}",
        COMMON.len(),
        declared_common.len()
    );
    assert_eq!(
        declared_paid.len(),
        PAID.len(),
        "la lista PAID tiene {} ajustes y `config_enterprise.rs` declara {}: \
         sobra o falta alguno.\ndeclarados: {declared_paid:?}",
        PAID.len(),
        declared_paid.len()
    );
}
