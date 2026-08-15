// SPDX-License-Identifier: BSL-1.1

//! Server configuration parsed from environment variables.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::Deserialize;

/// Where the audit log goes. `Stdout` is the container-friendly default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuditSinkKind {
    /// One JSON event per line to stdout. Default.
    Stdout,
    /// Rotating file sink; requires [`ServerConfig::audit_file`] or a
    /// `data_dir` (startup derives `audit.log` under it).
    File,
    /// Audit log disabled. Discouraged in production; primarily for tests.
    Off,
}

/// Configuration for the `ErmyaGraph` server.
///
/// Use [`ServerConfig::from_env`] to read from environment variables,
/// or [`ServerConfig::from_map`] for testable parsing without touching
/// the process environment.
pub struct ServerConfig {
    /// Socket address to bind (default `127.0.0.1:7687`).
    pub bind_addr: String,
    /// Path to the TLS certificate PEM file.
    pub tls_cert: Option<PathBuf>,
    /// Path to the TLS private key PEM file.
    pub tls_key: Option<PathBuf>,
    /// Bootstrap password injected at first startup when the system
    /// graph has no users. Ignored (with a warn) on subsequent runs.
    /// `None` triggers a random password printed to stderr.
    pub password: Option<String>,
    /// Persistent data directory. `None` means in-memory only.
    pub data_dir: Option<PathBuf>,
    /// Maximum concurrent connections (default 256).
    pub max_connections: usize,
    /// Idle timeout per connection in seconds (default 300).
    pub idle_timeout_secs: u64,
    /// Audit sink selector (default `Stdout`).
    pub audit_sink: AuditSinkKind,
    /// Explicit path for the file audit sink. When unset and `audit_sink`
    /// is `File`, startup derives `{data_dir}/audit.log`.
    pub audit_file: Option<PathBuf>,
    /// Max bytes per audit file before rotation (default 100 MB).
    pub audit_max_bytes: u64,
    /// Rotated audit files to retain (default 10).
    pub audit_keep_files: u32,
    /// Fsync the audit file every N events. `0` disables periodic fsync
    /// (rotation still fsyncs). Default `0`.
    pub audit_fsync_every: u32,
    /// Bypass authentication (`ERMYA_NO_AUTH=1`). Emits a warn on startup.
    pub no_auth: bool,

    // ── v0.5.0: multi-database runtime (spec §11) ──────────────────────
    /// Los ajustes que sólo gobiernan el gestor multi-base.
    ///
    /// **Los analiza una factoría, igual que al propio gestor** (apartado
    /// 5.10 del inventario). El montaje común no sabe qué hay dentro: sólo lo
    /// transporta hasta quien sí lo sabe.
    ///
    /// Vacío en la edición pública, y eso es la respuesta correcta: un
    /// servidor de una sola base no tiene expulsión por inactividad, ni topes
    /// por base, ni catálogo. Antes se aceptaban los siete y se avisaba de que
    /// no se aplicaban — que es cargar con ellos igualmente.
    pub paid: crate::config_paid_settings::PaidSettings,
    /// Per-transaction memory cap: the estimated bytes an explicit
    /// transaction's UNCOMMITTED delta chain may hold before it is aborted
    /// (an implicit rollback). Bounds the memory a single long-running
    /// transaction can pin under MVCC. `None` means unlimited. Default 64 MiB.
    pub max_txn_memory_bytes: Option<u64>,
    /// Issue #37: max operations one outermost batch (`begin_batch`/`end_batch`)
    /// may accumulate before a mutation is rejected. The primary, predictable
    /// batch cap. `None` means unlimited. Default 100 000.
    pub max_batch_operations: Option<u64>,
    /// Issue #37: max estimated bytes one outermost batch may accumulate. A high
    /// safety fuse against a batch of a few huge entities exhausting process
    /// memory, not an everyday limit. `None` means unlimited. Default 256 MiB.
    pub max_batch_memory_bytes: Option<u64>,
    /// Max wall-clock seconds the server waits during graceful
    /// shutdown for in-flight statements to drain. Default 30.
    pub shutdown_timeout_seconds: u64,
    /// How often the background MVCC vacuum wakes to reclaim the memory
    /// held by committed transaction versions no live transaction still
    /// needs. Default 300 seconds (5 minutes). `0` disables the vacuum
    /// entirely — the task is never spawned, and committed-version memory
    /// grows for the life of the process, so only set `0` for short-lived
    /// or read-only deployments.
    pub vacuum_interval_seconds: u64,

    // ── v0.6.0 Fase 2 Task 1: observability ─────────────────────────────
    /// Socket address for the Prometheus metrics HTTP endpoint
    /// (`ERMYA_METRICS_ADDR`). `None` keeps the endpoint disabled — the
    /// default. When set (e.g. `"0.0.0.0:9090"`), the server spawns a
    /// minimal HTTP/1.1 listener that serves Prometheus text format on
    /// `GET /metrics`. The endpoint is HTTP-plain by design: Prometheus
    /// scraping in containerized deployments runs on the internal pod
    /// network; the Bolt port keeps its TLS contract unchanged.
    pub metrics_addr: Option<String>,

    /// Interval between snapshots of [`DatabaseRegistry::stats`] taken
    /// by the metrics poller. Each tick refreshes
    /// `ermya_open_databases` and the per-database
    /// `ermya_database_size_bytes` gauges, and recycles label-guard
    /// slots whose database has been closed since the previous tick.
    ///
    /// The default (15s) matches the cadence Prometheus is typically
    /// configured to scrape at; lowering it makes the gauges fresher
    /// but increases the cost of `dir_size_bytes` stats on large
    /// fleets. **Not** exposed as an env var — the plan keeps this
    /// internal in Task 1 (Decision §C7). Tests that need sub-second
    /// resolution override the field directly when constructing
    /// `ServerConfig`.
    ///
    /// [`DatabaseRegistry::stats`]: crate::registry::DatabaseRegistry::stats

    // ── v0.6.0 Fase 2 Task 3: slow query log + tracing ──────────────────────

    /// Threshold in milliseconds at or above which a successful or
    /// failed RUN emits a second `AuditEvent::SlowQuery` audit line in
    /// addition to the regular `query_exec` event. The comparison is
    /// `duration_ms >= slow_query_threshold_ms`. `0` disables the
    /// slow-query log entirely (the handler never enters the emission
    /// path). Default 1000 ms. Env: `ERMYA_SLOW_QUERY_THRESHOLD_MS`.
    pub slow_query_threshold_ms: u64,

    /// Per-connection cap on `AuditEvent::SlowQuery` emissions inside a
    /// sliding 60-second window. Caps audit-log volume against a
    /// flooding attacker on a single connection. `0` disables the cap
    /// (every event over `slow_query_threshold_ms` passes). Default 60
    /// events/minute. Env: `ERMYA_SLOW_QUERY_MAX_EVENTS_PER_MINUTE`.
    pub max_slow_events_per_minute: u32,

    /// v0.6.0 Fase 2 Task 4 — maximum rows a single query may produce
    /// before it is aborted with `Neo.ClientError.General.ResultExhausted`.
    /// `0` disables the cap. Default `10_000_000` (~1 GB at ~100 B/row)
    /// protects against catastrophic OOM without rejecting mid-size
    /// analytical queries. Enforced in two places: a match-count guard in
    /// the engine (`gql::execute`, against cross-join explosion) and an
    /// output-row guard at the `GraphAccessor` boundary (against UNWIND /
    /// pipeline expansion). Env: `ERMYA_MAX_RESULT_ROWS`.
    pub max_result_rows: u64,

    // ── v0.6.0 Fase 2 Task 5: rate limiting ────────────────────────────
    /// Max HELLO failures per IP in a sliding 60-second window before
    /// further HELLOs from that IP fail-fast with
    /// `Neo.ClientError.Security.AuthorizationExpired`. `0` disables.
    /// Default `5`. Env: `ERMYA_AUTH_MAX_FAILURES_PER_MINUTE`.
    pub auth_max_failures_per_minute: u32,

    /// Token-bucket cap on RUN/PULL/DISCARD per connection, refill rate
    /// in tokens/sec. Bucket capacity = `cap * 2` for burst tolerance.
    /// Excess RUNs receive `Neo.ClientError.Security.TooManyRequests`.
    /// `0` disables. Default `100`. Env: `ERMYA_QUERIES_MAX_PER_SECOND`.
    pub queries_max_per_second: u32,

    /// Max simultaneous Bolt connections per peer IP. The accept loop
    /// rejects the TCP socket before handshake when the cap is hit.
    /// `0` disables. Default `16`. Env: `ERMYA_MAX_CONNECTIONS_PER_IP`.
    pub max_connections_per_ip: u32,

    /// Per-connection bandwidth cap (bytes/sec). Token-bucket with
    /// capacity = `cap * 2` and refill = `cap` bytes/sec. Cap hits cause
    /// cooperative `tokio::time::sleep` rather than a wire error.
    /// `0` disables. Default `1_048_576` (1 MiB/s). Env:
    /// `ERMYA_MAX_BYTES_PER_SECOND`.
    pub max_bytes_per_second: u64,

    /// Cap on the number of distinct peer IPs the global rate-limiter
    /// tracks. When the cap is reached, an overflow IP evicts the
    /// least-recently-touched entry from the store. Mirrors the
    /// `LabelGuard` cap pattern from Task 1 metrics. Default `256`.
    /// Env: `ERMYA_RATE_LIMIT_IP_CAP`.
    pub rate_limit_ip_cap: usize,

    // ── v0.6.0 Fase 2 Task 6: query timeout ────────────────────────────
    /// Per-query cooperative timeout in **milliseconds**. When `> 0`, every
    /// RUN computes a deadline and the engine aborts the query if it overruns,
    /// surfacing `Neo.ClientError.Statement.ExecutionFailed` (a non-retryable
    /// `ClientError`). Covers the MATCH phase of mutations but never the write
    /// phase, so no mutation is cut mid-write. `0` (the default) disables the
    /// timeout — opt-in. Env: `ERMYA_QUERY_TIMEOUT_MS`.
    pub query_timeout_ms: u64,

    // ── v0.7.0 Block 1: configurable server agent string ────────────────
    /// Agent string sent in the HELLO `server` metadata field.
    ///
    /// Default `"Neo4j/<version>"`. The official Neo4j Python driver rejects
    /// any server product whose agent does not start with `"Neo4j/"`
    /// (`check_supported_server_product`), so this default lets it connect
    /// without patching the driver. Override with `ERMYA_SERVER_AGENT` for
    /// custom branding; the Neo4j .NET driver only requires a valid semver
    /// after the slash. Env: `ERMYA_SERVER_AGENT`.
    pub server_agent: String,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind_addr: "127.0.0.1:7687".to_owned(),
            tls_cert: None,
            tls_key: None,
            password: None,
            // v0.7.0: file-backed by default (secure-by-default — persisting
            // is the expected behaviour; losing data on restart must be an
            // explicit choice via ERMYA_DATA_DIR=:memory:). Unix production
            // path; override with ERMYA_DATA_DIR on other platforms.
            data_dir: Some(PathBuf::from("/var/lib/ermya/data")),
            max_connections: 256,
            idle_timeout_secs: 300,
            audit_sink: AuditSinkKind::Stdout,
            audit_file: None,
            audit_max_bytes: 100_000_000,
            audit_keep_files: 10,
            audit_fsync_every: 0,
            no_auth: false,
            paid: crate::config_paid_settings::PaidSettings::default(),
            max_txn_memory_bytes: Some(64 * 1024 * 1024),
            max_batch_operations: Some(100_000),
            max_batch_memory_bytes: Some(256 * 1024 * 1024),
            shutdown_timeout_seconds: 30,
            vacuum_interval_seconds: 300,
            metrics_addr: None,
            slow_query_threshold_ms: 1000,
            max_slow_events_per_minute: 60,
            max_result_rows: 10_000_000,
            auth_max_failures_per_minute: 5,
            queries_max_per_second: 100,
            max_connections_per_ip: 16,
            max_bytes_per_second: 1_048_576,
            rate_limit_ip_cap: 256,
            query_timeout_ms: 0,
            server_agent: format!("Neo4j/{}", env!("CARGO_PKG_VERSION")),
        }
    }
}

impl ServerConfig {
    /// Parse configuration from a key-value map.
    ///
    /// This is the testable core — [`from_env`](Self::from_env) delegates here.
    ///
    /// Unknown values for `ERMYA_AUDIT_SINK` fall back to the default
    /// (`Stdout`) rather than failing; invalid numbers follow the same
    /// fallback-to-default pattern already used for `ERMYA_MAX_CONNECTIONS`.
    #[must_use]
    pub fn from_map(vars: &HashMap<String, String>) -> Self {
        let defaults = Self::default();

        Self {
            bind_addr: vars
                .get("ERMYA_BIND")
                .cloned()
                .unwrap_or(defaults.bind_addr),
            tls_cert: vars.get("ERMYA_TLS_CERT").map(PathBuf::from),
            tls_key: vars.get("ERMYA_TLS_KEY").map(PathBuf::from),
            password: vars.get("ERMYA_PASSWORD").cloned(),
            data_dir: parse_data_dir(vars.get("ERMYA_DATA_DIR"), defaults.data_dir),
            max_connections: parse_num(vars, "ERMYA_MAX_CONNECTIONS", defaults.max_connections),
            idle_timeout_secs: parse_num(vars, "ERMYA_IDLE_TIMEOUT", defaults.idle_timeout_secs),
            audit_sink: parse_audit_sink(vars.get("ERMYA_AUDIT_SINK"))
                .unwrap_or(defaults.audit_sink),
            audit_file: vars.get("ERMYA_AUDIT_FILE").map(PathBuf::from),
            audit_max_bytes: parse_num(vars, "ERMYA_AUDIT_MAX_BYTES", defaults.audit_max_bytes),
            audit_keep_files: parse_num(
                vars,
                "ERMYA_AUDIT_KEEP_FILES",
                defaults.audit_keep_files,
            ),
            audit_fsync_every: parse_num(
                vars,
                "ERMYA_AUDIT_FSYNC_EVERY",
                defaults.audit_fsync_every,
            ),
            // Intentionally strict: only the literal `"1"` activates the
            // bypass so a stray `"true"` from a shell typo cannot disable
            // authentication in production.
            no_auth: vars.get("ERMYA_NO_AUTH").map(String::as_str) == Some("1"),
            // Optional: absence (or unparsable value) means "unlimited".
            max_txn_memory_bytes: parse_opt_num(vars, "ERMYA_MAX_TXN_MEMORY_BYTES"),
            // Issue #37: batch caps are "secure by default" — an absent env var
            // falls back to the built-in default (a protective cap), NOT to
            // unlimited. Leaving a batch uncapped is the DoS this issue closes,
            // so missing configuration must never leave the server unprotected.
            // A missing var therefore means "use the default cap", never "no
            // limit".
            max_batch_operations: parse_opt_num(vars, "ERMYA_MAX_BATCH_OPERATIONS")
                .or(defaults.max_batch_operations),
            max_batch_memory_bytes: parse_opt_num(vars, "ERMYA_MAX_BATCH_MEMORY_BYTES")
                .or(defaults.max_batch_memory_bytes),
            shutdown_timeout_seconds: parse_num(
                vars,
                "ERMYA_SHUTDOWN_TIMEOUT_SECONDS",
                defaults.shutdown_timeout_seconds,
            ),
            vacuum_interval_seconds: parse_num(
                vars,
                "ERMYA_VACUUM_INTERVAL_SECONDS",
                defaults.vacuum_interval_seconds,
            ),
            // Absence means the metrics HTTP endpoint is not started.
            // Parsing the socket address is deferred to startup (same
            // pattern as `bind_addr`); the field only carries the raw
            // string here.
            metrics_addr: vars.get("ERMYA_METRICS_ADDR").cloned(),
            // Los ajustes del gestor multi-base los analiza su propia
            // factoría, que vive en el lado de pago: aquí no se sabe qué
            // contienen ni cuántos son.
            // Los ajustes del gestor multi-base llegan con su valor por
            // defecto: quien monte un servidor de pago los rellena con
            // `with_paid_settings`, cuya factoría vive en el lado de pago.
            //
            // Antes se llamaba aquí a esa factoría, y eso ataba la
            // configuración común —que viaja al árbol público— a un módulo que
            // no viaja.
            paid: crate::config_paid_settings::PaidSettings::default(),
            slow_query_threshold_ms: parse_num(
                vars,
                "ERMYA_SLOW_QUERY_THRESHOLD_MS",
                defaults.slow_query_threshold_ms,
            ),
            max_slow_events_per_minute: parse_num(
                vars,
                "ERMYA_SLOW_QUERY_MAX_EVENTS_PER_MINUTE",
                defaults.max_slow_events_per_minute,
            ),
            max_result_rows: parse_num(vars, "ERMYA_MAX_RESULT_ROWS", defaults.max_result_rows),
            auth_max_failures_per_minute: parse_num(
                vars,
                "ERMYA_AUTH_MAX_FAILURES_PER_MINUTE",
                defaults.auth_max_failures_per_minute,
            ),
            queries_max_per_second: parse_num(
                vars,
                "ERMYA_QUERIES_MAX_PER_SECOND",
                defaults.queries_max_per_second,
            ),
            max_connections_per_ip: parse_num(
                vars,
                "ERMYA_MAX_CONNECTIONS_PER_IP",
                defaults.max_connections_per_ip,
            ),
            max_bytes_per_second: parse_num(
                vars,
                "ERMYA_MAX_BYTES_PER_SECOND",
                defaults.max_bytes_per_second,
            ),
            rate_limit_ip_cap: parse_num(
                vars,
                "ERMYA_RATE_LIMIT_IP_CAP",
                defaults.rate_limit_ip_cap,
            ),
            query_timeout_ms: parse_num(
                vars,
                "ERMYA_QUERY_TIMEOUT_MS",
                defaults.query_timeout_ms,
            ),
            server_agent: vars
                .get("ERMYA_SERVER_AGENT")
                .cloned()
                .unwrap_or(defaults.server_agent),
        }
    }

    /// Rellena los ajustes del gestor multi-base.
    ///
    /// Lo llama quien monta un servidor de la edición de pago, justo después de
    /// leer la configuración. La edición pública no lo llama nunca: sus valores
    /// por defecto no gobiernan nada porque no hay gestor multi-base.
    #[must_use]
    pub fn with_paid_settings(mut self, paid: crate::config_paid_settings::PaidSettings) -> Self {
        self.paid = paid;
        self
    }

    /// Parse configuration from process environment variables.
    #[must_use]
    pub fn from_env() -> Self {
        let vars: HashMap<String, String> = std::env::vars()
            .filter(|(k, _)| k.starts_with("ERMYA_"))
            .collect();
        Self::from_map(&vars)
    }

    /// Parse configuration from a TOML file at `path`, with `env` overrides.
    ///
    /// Precedence: `env` vars win over TOML file fields, which win over the
    /// compiled defaults. A missing file is not an error — it simply means
    /// "configure from `env` and defaults only". A file that exists but does
    /// not parse as valid TOML is logged and treated as absent, so a malformed
    /// config never silently swaps in a different shape of values.
    #[must_use]
    pub fn from_file_and_env(path: &str, env: &HashMap<String, String>) -> Self {
        let Ok(contents) = std::fs::read_to_string(path) else {
            // Missing or unreadable file → env-only.
            return Self::from_map(env);
        };
        let file_cfg: FileConfig = match toml::from_str(&contents) {
            Ok(fc) => fc,
            Err(e) => {
                tracing::warn!(
                    target: "config",
                    path,
                    error = %e,
                    "config file is not valid TOML; ignoring it and using env + defaults",
                );
                return Self::from_map(env);
            }
        };

        // Merge: TOML fields form the base map, env vars override key-by-key.
        let mut merged = file_config_to_map(&file_cfg);
        for (k, v) in env {
            merged.insert(k.clone(), v.clone());
        }
        Self::from_map(&merged)
    }

    /// El mapa de ajustes que resulta de mezclar el fichero con el entorno: el
    /// fichero pone la base y cada variable de entorno pisa su clave.
    ///
    /// Se expone porque **la factoría de los ajustes del gestor multi-base
    /// necesita ver esta mezcla**, no sólo el entorno. Quien monta el servidor
    /// lee la configuración por un lado y produce esos siete ajustes por otro;
    /// si al segundo sólo se le pasa el entorno, un ajuste escrito en el fichero
    /// y no en el entorno se pierde sin aviso — que es justo el fallo que la
    /// separación de ediciones quería evitar: un ajuste aceptado y no aplicado.
    ///
    /// Un fichero ausente o con TOML inválido da el entorno solo, igual que
    /// [`Self::from_file_and_env`].
    #[must_use]
    pub fn merged_env_map(path: &str, env: &HashMap<String, String>) -> HashMap<String, String> {
        let mut merged = std::fs::read_to_string(path)
            .ok()
            .and_then(|contents| toml::from_str::<FileConfig>(&contents).ok())
            .map(|fc| file_config_to_map(&fc))
            .unwrap_or_default();
        for (k, v) in env {
            merged.insert(k.clone(), v.clone());
        }
        merged
    }
}

/// Resolve `ERMYA_DATA_DIR` into the optional data directory.
///
/// `:memory:` is the explicit opt-out from the file-backed default, selecting
/// an in-memory instance (`None`). Any other value is taken as a path. An
/// absent key falls back to `default` (the file-backed default path).
/// Parse a single `ERMYA_*` value as `T`, falling back to `default` when the
/// key is absent or unparsable. Collapses the `get().and_then(parse).unwrap_or`
/// triplet repeated across the numeric fields of [`ServerConfig::from_map`] into
/// one call, keeping that constructor flat and under the line budget. `T` is
/// inferred from the destination field, so call sites need no turbofish.
fn parse_num<T: std::str::FromStr>(vars: &HashMap<String, String>, key: &str, default: T) -> T {
    vars.get(key)
        .and_then(|raw| raw.parse().ok())
        .unwrap_or(default)
}

/// Like [`parse_num`] but yields `None` (rather than a default) when the key is
/// absent or unparsable — for the "absence means unlimited" optional fields.
fn parse_opt_num<T: std::str::FromStr>(vars: &HashMap<String, String>, key: &str) -> Option<T> {
    vars.get(key).and_then(|raw| raw.parse().ok())
}

fn parse_data_dir(raw: Option<&String>, default: Option<PathBuf>) -> Option<PathBuf> {
    match raw.map(String::as_str) {
        Some(":memory:") => None,
        Some(p) => Some(PathBuf::from(p)),
        None => default,
    }
}

fn parse_audit_sink(raw: Option<&String>) -> Option<AuditSinkKind> {
    match raw.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
        Some("stdout") => Some(AuditSinkKind::Stdout),
        Some("file") => Some(AuditSinkKind::File),
        Some("off") => Some(AuditSinkKind::Off),
        _ => None,
    }
}

/// TOML file shape: every field is optional so an absent key means "fall back
/// to env / default" rather than overriding with a zero value. Each field maps
/// to exactly one `ERMYA_*` key consumed by [`ServerConfig::from_map`], so
/// the parsing/validation logic lives in one place. Values are kept as strings
/// (matching the env representation) and re-parsed by `from_map`; this avoids
/// duplicating the numeric/enum parsing and the fallback-to-default semantics.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    bind: Option<String>,
    tls_cert: Option<String>,
    tls_key: Option<String>,
    password: Option<String>,
    data_dir: Option<String>,
    max_connections: Option<u64>,
    idle_timeout: Option<u64>,
    audit_sink: Option<String>,
    audit_file: Option<String>,
    audit_max_bytes: Option<u64>,
    audit_keep_files: Option<u64>,
    audit_fsync_every: Option<u64>,
    no_auth: Option<bool>,
    idle_ttl_seconds: Option<u64>,
    default_max_connections: Option<u64>,
    default_max_size_bytes: Option<u64>,
    max_txn_memory_bytes: Option<u64>,
    max_batch_operations: Option<u64>,
    max_batch_memory_bytes: Option<u64>,
    shutdown_timeout_seconds: Option<u64>,
    registry_sweep_interval_seconds: Option<u64>,
    vacuum_interval_seconds: Option<u64>,
    ttl_disabled_warn_threshold: Option<u64>,
    max_open_databases: Option<u64>,
    metrics_addr: Option<String>,
    slow_query_threshold_ms: Option<u64>,
    max_slow_events_per_minute: Option<u64>,
    max_result_rows: Option<u64>,
    auth_max_failures_per_minute: Option<u64>,
    queries_max_per_second: Option<u64>,
    max_connections_per_ip: Option<u64>,
    max_bytes_per_second: Option<u64>,
    rate_limit_ip_cap: Option<u64>,
    query_timeout_ms: Option<u64>,
    server_agent: Option<String>,
}

/// Translate a parsed [`FileConfig`] into the `ERMYA_*` key/value map that
/// [`ServerConfig::from_map`] understands. Only `Some` fields emit a key, so an
/// absent TOML field leaves the corresponding env/default untouched after the
/// merge in [`ServerConfig::from_file_and_env`].
fn file_config_to_map(fc: &FileConfig) -> HashMap<String, String> {
    let mut m = HashMap::new();
    let mut put_str = |key: &str, val: &Option<String>| {
        if let Some(v) = val {
            m.insert(key.to_owned(), v.clone());
        }
    };
    put_str("ERMYA_BIND", &fc.bind);
    put_str("ERMYA_TLS_CERT", &fc.tls_cert);
    put_str("ERMYA_TLS_KEY", &fc.tls_key);
    put_str("ERMYA_PASSWORD", &fc.password);
    put_str("ERMYA_DATA_DIR", &fc.data_dir);
    put_str("ERMYA_AUDIT_SINK", &fc.audit_sink);
    put_str("ERMYA_AUDIT_FILE", &fc.audit_file);
    put_str("ERMYA_METRICS_ADDR", &fc.metrics_addr);
    put_str("ERMYA_SERVER_AGENT", &fc.server_agent);

    let mut put_num = |key: &str, val: Option<u64>| {
        if let Some(v) = val {
            m.insert(key.to_owned(), v.to_string());
        }
    };
    put_num("ERMYA_MAX_CONNECTIONS", fc.max_connections);
    put_num("ERMYA_IDLE_TIMEOUT", fc.idle_timeout);
    put_num("ERMYA_AUDIT_MAX_BYTES", fc.audit_max_bytes);
    put_num("ERMYA_AUDIT_KEEP_FILES", fc.audit_keep_files);
    put_num("ERMYA_AUDIT_FSYNC_EVERY", fc.audit_fsync_every);
    put_num("ERMYA_IDLE_TTL_SECONDS", fc.idle_ttl_seconds);
    put_num(
        "ERMYA_DEFAULT_MAX_CONNECTIONS",
        fc.default_max_connections,
    );
    put_num("ERMYA_DEFAULT_MAX_SIZE_BYTES", fc.default_max_size_bytes);
    put_num("ERMYA_MAX_TXN_MEMORY_BYTES", fc.max_txn_memory_bytes);
    put_num("ERMYA_MAX_BATCH_OPERATIONS", fc.max_batch_operations);
    put_num("ERMYA_MAX_BATCH_MEMORY_BYTES", fc.max_batch_memory_bytes);
    put_num(
        "ERMYA_SHUTDOWN_TIMEOUT_SECONDS",
        fc.shutdown_timeout_seconds,
    );
    put_num(
        "ERMYA_REGISTRY_SWEEP_INTERVAL_SECONDS",
        fc.registry_sweep_interval_seconds,
    );
    put_num(
        "ERMYA_VACUUM_INTERVAL_SECONDS",
        fc.vacuum_interval_seconds,
    );
    put_num(
        "ERMYA_TTL_DISABLED_WARN_THRESHOLD",
        fc.ttl_disabled_warn_threshold,
    );
    put_num("ERMYA_MAX_OPEN_DATABASES", fc.max_open_databases);
    put_num(
        "ERMYA_SLOW_QUERY_THRESHOLD_MS",
        fc.slow_query_threshold_ms,
    );
    put_num(
        "ERMYA_SLOW_QUERY_MAX_EVENTS_PER_MINUTE",
        fc.max_slow_events_per_minute,
    );
    put_num("ERMYA_MAX_RESULT_ROWS", fc.max_result_rows);
    put_num(
        "ERMYA_AUTH_MAX_FAILURES_PER_MINUTE",
        fc.auth_max_failures_per_minute,
    );
    put_num("ERMYA_QUERIES_MAX_PER_SECOND", fc.queries_max_per_second);
    put_num("ERMYA_MAX_CONNECTIONS_PER_IP", fc.max_connections_per_ip);
    put_num("ERMYA_MAX_BYTES_PER_SECOND", fc.max_bytes_per_second);
    put_num("ERMYA_RATE_LIMIT_IP_CAP", fc.rate_limit_ip_cap);
    put_num("ERMYA_QUERY_TIMEOUT_MS", fc.query_timeout_ms);

    // `no_auth` is a bool in TOML but `from_map` expects the literal "1".
    if fc.no_auth == Some(true) {
        m.insert("ERMYA_NO_AUTH".to_owned(), "1".to_owned());
    }
    m
}
