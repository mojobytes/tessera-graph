// SPDX-License-Identifier: BSL-1.1

//! Prometheus metrics endpoint — minimal HTTP/1.1 server.
//!
//! v0.6.0 Fase 2 Task 1 (plan
//! `docs/superpowers/plans/2026-05-18-observability-task-1-metrics.md`).
//!
//! This module owns:
//! - The process-global `PrometheusRecorder` installed via the `metrics`
//!   facade. Installation is idempotent across calls (`OnceLock`); the
//!   first caller wins, every subsequent call reuses the existing
//!   handle. Tests in the same process can therefore call
//!   [`spawn_metrics_server`] repeatedly without panicking the runtime.
//! - A custom HTTP/1.1 listener on `tokio::net::TcpListener`. We
//!   deliberately do **not** depend on `hyper` for the scrape endpoint
//!   (plan decision 2): the contract is `GET /metrics` returning
//!   Prometheus text format and nothing else, which fits in well under
//!   100 LOC and avoids three transitive crates that the
//!   `metrics-exporter-prometheus` `http-listener` feature would pull.
//!
//! The endpoint is HTTP-plain by design — Prometheus scraping in
//! containerised deployments runs on the internal pod network. The Bolt
//! port keeps its TLS contract unchanged; this listener is independent.

use std::borrow::Cow;
use std::collections::HashSet;
use std::io;
use std::net::SocketAddr;
use std::sync::{LazyLock, Mutex, OnceLock};
use std::time::Duration;

use metrics::{counter, gauge, histogram};
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio::time::timeout;
use tracing::{debug, warn};

// ── Metric name constants ──────────────────────────────────────────────────
//
// Centralised so emission sites and (future) docs share the same source of
// truth. Each constant is `pub(crate)` so tests can reference them by
// constant rather than literal.

pub(crate) const ACTIVE_CONNECTIONS: &str = "tessera_active_connections";
pub(crate) const BOLT_MESSAGES_TOTAL: &str = "tessera_bolt_messages_total";
pub(crate) const AUTH_ATTEMPTS_TOTAL: &str = "tessera_auth_attempts_total";
pub(crate) const QUERIES_TOTAL: &str = "tessera_queries_total";
pub(crate) const QUERY_DURATION_SECONDS: &str = "tessera_query_duration_seconds";
/// v0.6.0 Fase 2 Task 2: WAL fsync latency histogram. One observation
/// per fsync the engine actually performs (skips inside a batch are
/// not recorded — see `tessera_graph::Graph::wal_sync`). No labels:
/// the engine-side `WalObserver` signature is `Fn(FsyncCause, Duration)`
/// — it carries the fsync cause (individual vs batch-close) but no
/// database context, and this histogram records the same series for
/// both causes. A future iteration may widen the observer signature to
/// thread the database name through so the histogram can carry a
/// `database` label gated by `DB_LABEL_GUARD`.
pub(crate) const WAL_FSYNC_DURATION_SECONDS: &str = "tessera_wal_fsync_duration_seconds";
/// v0.6.0 Fase 2 Task 4: count of queries aborted by the defensive
/// result-row cap (either the Cap A match-count guard in the engine or
/// the Cap B output guard at the `GraphAccessor` boundary). Labelled by
/// `database` so dashboards can attribute over-cap traffic to a tenant.
pub(crate) const RESULT_CAPPED_TOTAL: &str = "tessera_result_capped_total";
/// v0.6.0 Fase 2 Task 5: count of requests rejected by a rate-limit
/// axis. Labelled by `axis` (`"auth_ip"`, `"query_conn"`,
/// `"conn_ip"`, `"bytes_conn"`). No cardinality risk — axis values
/// are `'static` strings chosen at compile time.
pub(crate) const RATE_LIMIT_HITS_TOTAL: &str = "tessera_rate_limit_hits_total";
/// v0.6.0 Fase 2 Task 6: count of queries aborted by the cooperative
/// per-query timeout (the engine's deadline checks). Labelled by
/// `database` so dashboards can attribute timeouts to a tenant.
pub(crate) const QUERY_TIMEOUTS_TOTAL: &str = "tessera_query_timeouts_total";

/// Maximum number of distinct values the `database` label may carry
/// across all metrics. The 257th distinct database collapses to the
/// sentinel `"_other"` so the Prometheus registry never grows past
/// `CAP + 1` series for this label.
///
/// The cap is per-process, not global: a process serving many tenants
/// with ephemeral databases recicla ranuras cuando una base se cierra
/// (ver el vigía de medidas por base). 256 covers the
/// realistic upper bound — Task 14's `max_open_databases` lands at the
/// same order of magnitude — while keeping the worst-case label
/// cardinality bounded and well below Prometheus' per-target series
/// guidance.
pub const METRICS_DATABASE_LABEL_CAP: usize = 256;

/// Tracks the set of `database` label values already emitted to the
/// Prometheus registry and collapses any value past
/// [`METRICS_DATABASE_LABEL_CAP`] to `"_other"`.
///
/// `pub` so the cycle-6 unit test in `tests/metrics_test.rs` can
/// construct a fresh guard and prove the cap bites at the documented
/// threshold without going through a live server. All emission helpers
/// in this module funnel database names through
/// [`DB_LABEL_GUARD`].`resolve_database_label` before passing them as
/// metric labels.
pub struct LabelGuard {
    known: Mutex<HashSet<String>>,
}

impl LabelGuard {
    /// Build a fresh guard with no known labels.
    #[must_use]
    pub fn new() -> Self {
        Self {
            known: Mutex::new(HashSet::new()),
        }
    }

    /// Return the label to use for `name`. If `name` was seen before
    /// (or fits under the cap and is now recorded), the original name
    /// is returned. Once the cap is reached, every previously-unseen
    /// name collapses to `"_other"`.
    ///
    /// The return type is `Cow<'_, str>` but every current path
    /// yields `Cow::Borrowed` — either the caller's `name` (when
    /// admitted or already known) or the static `"_other"` sentinel.
    /// The `Cow` is kept for API stability: emission helpers in this
    /// module materialise the label with `.into_owned()` because the
    /// `metrics::*` macros require `SharedString` / `&'static str`
    /// (the `&str` overload was dropped in `metrics` 0.24), so every
    /// emit allocates. A future micro-optimisation could cache the
    /// resolved label per `BoltHandler` session and avoid both the
    /// mutex acquisition and the allocation in the steady-state hot
    /// path; deferred until profiling shows contention.
    ///
    /// # Panics
    ///
    /// Panics if the internal mutex is poisoned. The mutex is only
    /// held for hashset operations on `String`s; the only way to
    /// poison it is a panic *inside* `resolve_database_label` itself,
    /// which has no panic-bearing paths.
    pub fn resolve_database_label<'a>(&self, name: &'a str) -> Cow<'a, str> {
        let mut known = self.known.lock().expect("LabelGuard mutex poisoned");
        if known.contains(name) {
            return Cow::Borrowed(name);
        }
        if known.len() >= METRICS_DATABASE_LABEL_CAP {
            return Cow::Borrowed("_other");
        }
        known.insert(name.to_owned());
        Cow::Borrowed(name)
    }
}

impl Default for LabelGuard {
    fn default() -> Self {
        Self::new()
    }
}

/// Process-global label guard for the `database` label. Every metric
/// helper that accepts a database name routes it through this guard
/// before emission so the Prometheus registry never grows past
/// `METRICS_DATABASE_LABEL_CAP + 1` `database=` series in aggregate.
pub(crate) static DB_LABEL_GUARD: LazyLock<LabelGuard> = LazyLock::new(LabelGuard::new);

/// Process-global handle to the installed Prometheus recorder.
///
/// `OnceLock` lets the production startup path and integration tests
/// share the same recorder without panicking on double-install: the
/// first caller installs it, every subsequent caller reuses the stored
/// handle.
static METRICS_HANDLE: OnceLock<PrometheusHandle> = OnceLock::new();

/// Cap on per-request bytes read before the parser bails. Plan decision 2.
const HTTP_MAX_REQUEST_BYTES: usize = 4 * 1024;

/// Per-request read deadline. Plan decision 2.
const HTTP_READ_TIMEOUT: Duration = Duration::from_secs(5);

/// Install the Prometheus recorder if no other recorder has been
/// installed yet, and return its render handle.
///
/// Idempotent and concurrency-safe: uses [`OnceLock::get_or_init`] so
/// the install path executes exactly once even if several tasks call
/// this function concurrently (the scenario integration tests in the
/// same binary trigger). The old `get` + `set` pair raced — a loser
/// could fall into the `build_recorder()` fallback and store a handle
/// pointing at a recorder disconnected from the global facade, which
/// silently emptied the scrapes of every subsequent caller in that
/// test process.
fn install_or_get_recorder() -> PrometheusHandle {
    if let Some(handle) = METRICS_HANDLE.get() {
        return handle.clone();
    }
    METRICS_HANDLE
        .get_or_init(|| match PrometheusBuilder::new().install_recorder() {
            Ok(handle) => handle,
            Err(err) => {
                warn!(
                    error = %err,
                    "metrics: a global recorder was already installed; \
                     building a detached handle for rendering",
                );
                // Another `metrics` recorder owns the global slot
                // (typically a sibling test in the same binary that
                // installed first). We still need *some*
                // `PrometheusHandle` to render with; counters/gauges
                // emitted via the `metrics` facade go to the
                // globally-installed recorder, not this detached one.
                // The branch is intentionally limited to that
                // test-only scenario — production startup paths only
                // call this once.
                PrometheusBuilder::new().build_recorder().handle()
            }
        })
        .clone()
}

/// Spawn a minimal HTTP/1.1 listener that serves Prometheus text format
/// on `GET /metrics`. Returns the resolved [`SocketAddr`] once the
/// socket has bound (so callers — including tests on ephemeral port 0 —
/// can read the actual port).
///
/// `shutdown` is the same `watch::Receiver<bool>` plumbed through the
/// rest of the server. The listener stops accepting as soon as the
/// channel transitions to `true`; in-flight requests are allowed to
/// finish within [`HTTP_READ_TIMEOUT`].
///
/// # Errors
///
/// Returns an error if the listener cannot bind.
pub async fn spawn_metrics_server(
    addr: SocketAddr,
    shutdown: watch::Receiver<bool>,
) -> io::Result<SocketAddr> {
    let handle = install_or_get_recorder();
    let listener = TcpListener::bind(addr).await?;
    let bound = listener.local_addr()?;
    tokio::spawn(accept_loop(listener, handle, shutdown));
    Ok(bound)
}

async fn accept_loop(
    listener: TcpListener,
    handle: PrometheusHandle,
    mut shutdown: watch::Receiver<bool>,
) {
    loop {
        tokio::select! {
            biased;
            res = shutdown.changed() => {
                if res.is_err() || *shutdown.borrow() {
                    debug!("metrics: shutdown signalled, accept loop exiting");
                    return;
                }
            }
            accept = listener.accept() => {
                match accept {
                    Ok((stream, _peer)) => {
                        let handle = handle.clone();
                        tokio::spawn(async move {
                            if let Err(err) = handle_request(stream, &handle).await {
                                debug!(error = %err, "metrics: request handler error");
                            }
                        });
                    }
                    Err(err) => {
                        warn!(error = %err, "metrics: accept error");
                    }
                }
            }
        }
    }
}

async fn handle_request(mut stream: TcpStream, handle: &PrometheusHandle) -> io::Result<()> {
    let mut buf = Vec::with_capacity(512);
    let mut tmp = [0u8; 512];
    let request_line = loop {
        match timeout(HTTP_READ_TIMEOUT, stream.read(&mut tmp)).await {
            Ok(Ok(0)) => break parse_request_line(&buf),
            Ok(Ok(n)) => {
                buf.extend_from_slice(&tmp[..n]);
                if buf.len() > HTTP_MAX_REQUEST_BYTES {
                    return write_response(&mut stream, 413, "text/plain", b"").await;
                }
                if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    break parse_request_line(&buf);
                }
            }
            Ok(Err(e)) => return Err(e),
            Err(_) => {
                return write_response(&mut stream, 408, "text/plain", b"").await;
            }
        }
    };

    match request_line {
        Some((method, path)) if !is_printable_ascii(path) => {
            // Reject paths with control bytes / non-printable chars
            // before any downstream code touches them. `std::str::from_utf8`
            // upstream only validates structural UTF-8 — it admits ANSI
            // escape sequences and other control bytes that would become
            // a log-injection vector if a future cycle starts tracing
            // the request path. Centralising the gate here means the
            // 404 fall-through below stays clean and the method/path
            // pair flowing into any new instrumentation is guaranteed
            // printable.
            let _ = method;
            let _ = path;
            write_response(&mut stream, 400, "text/plain", b"").await
        }
        Some(("GET", "/metrics")) => {
            let body = handle.render();
            write_response(
                &mut stream,
                200,
                "text/plain; version=0.0.4",
                body.as_bytes(),
            )
            .await
        }
        Some(_) | None => write_response(&mut stream, 404, "text/plain", b"").await,
    }
}

/// Returns `true` when every byte of `s` is in the printable ASCII
/// range `0x20..=0x7E` (space through `~`, exclusive of DEL). Used by
/// [`handle_request`] to reject HTTP paths carrying control bytes or
/// non-ASCII payloads before they reach any logging or matching path.
fn is_printable_ascii(s: &str) -> bool {
    s.bytes().all(|b| (0x20..=0x7E).contains(&b))
}

fn parse_request_line(buf: &[u8]) -> Option<(&str, &str)> {
    let end = buf.iter().position(|b| *b == b'\r')?;
    let line = std::str::from_utf8(&buf[..end]).ok()?;
    let mut parts = line.split(' ');
    let method = parts.next()?;
    let path = parts.next()?;
    // Discard the HTTP version token; we only respond HTTP/1.1.
    Some((method, path))
}

// ── Emit helpers ───────────────────────────────────────────────────────────
//
// Wrapping the `metrics::*` macros gives us a single point to swap the
// implementation in tests, mirroring how `audit.rs` wraps the MPSC sink.

/// Increment the `tessera_active_connections` gauge by 1.
///
/// Called from the per-connection task in `listener.rs` immediately
/// before the handler future is awaited, so the gauge reflects the
/// number of connections currently being processed (not merely
/// accepted on the kernel socket queue).
pub(crate) fn connection_opened() {
    gauge!(ACTIVE_CONNECTIONS).increment(1.0);
}

/// Decrement the `tessera_active_connections` gauge by 1.
///
/// Called from the same per-connection task in `listener.rs` right
/// after the handler future returns (success or panic-aborted) so the
/// gauge always returns to baseline.
pub(crate) fn connection_closed() {
    gauge!(ACTIVE_CONNECTIONS).decrement(1.0);
}

/// Increment `tessera_bolt_messages_total{type, outcome}` by 1.
///
/// `msg_type` is the wire-level Bolt message name (`HELLO`, `RUN`,
/// `PULL`, …). `outcome` is one of `"success"`, `"error"`, `"ignored"`.
/// Both arguments are `&'static str` so the recorder can intern them
/// without allocating per-emit — the call sites in `handler.rs` use
/// constants returned by [`bolt_request_type_str`] and a literal
/// outcome chosen per branch.
pub(crate) fn bolt_message(msg_type: &'static str, outcome: &'static str) {
    counter!(BOLT_MESSAGES_TOTAL, "type" => msg_type, "outcome" => outcome).increment(1);
}

/// Increment `tessera_auth_attempts_total{outcome}` by 1.
///
/// Emitted from `handle_hello` alongside the existing audit-sink
/// records, with `outcome ∈ {"success", "failed"}`. Bumped before
/// the wire reply so a slow client cannot cause the counter and the
/// audit log to diverge.
pub(crate) fn auth_attempt(outcome: &'static str) {
    counter!(AUTH_ATTEMPTS_TOTAL, "outcome" => outcome).increment(1);
}

/// v0.6.0 Fase 2 Task 5 — increment the rate-limit hit counter for
/// the given axis. `axis` is a static string (`"auth_ip"`,
/// `"query_conn"`, `"conn_ip"`, `"bytes_conn"`) — kept `'static` to
/// avoid label allocation on the hot path.
pub(crate) fn rate_limit_hit(axis: &'static str) {
    counter!(RATE_LIMIT_HITS_TOTAL, "axis" => axis).increment(1);
}

/// Label used in place of a database name when the session has not yet
/// bound a user database (HELLO succeeded but no RUN with `db=` has
/// been processed). Keeping the label populated avoids gaps in the
/// time series and matches the convention used by the rest of the
/// stack — see `audit::query_exec_with_database` where `None` is
/// rendered as the system catalogue.
const DB_LABEL_SYSTEM: &str = "_system";

/// Increment `tessera_queries_total{database, outcome}` by 1.
///
/// `database` is the name of the database the RUN targeted, or `None`
/// for sessions that have not yet bound one (legacy single-database
/// flows and the brief window between HELLO and the first
/// `try_bind_database`). `outcome ∈ {"success", "error"}`.
///
/// The `database` label flows through [`DB_LABEL_GUARD`] so unbounded
/// tenant creation cannot blow the Prometheus registry's series count
/// past [`METRICS_DATABASE_LABEL_CAP`] + 1 (the `"_other"` sentinel).
pub(crate) fn query_executed(database: Option<&str>, outcome: &'static str) {
    let raw = database.unwrap_or(DB_LABEL_SYSTEM);
    let label = DB_LABEL_GUARD.resolve_database_label(raw);
    counter!(
        QUERIES_TOTAL,
        "database" => label.into_owned(),
        "outcome" => outcome,
    )
    .increment(1);
}

/// Increment `tessera_result_capped_total{database}` by 1.
///
/// Called from the Bolt handler when a query is aborted by the
/// defensive result-row cap. `database` follows the same
/// [`DB_LABEL_GUARD`] cardinality gate as [`query_executed`] so
/// over-cap traffic from unbounded tenant creation cannot blow the
/// registry's series count.
pub(crate) fn result_capped(database: Option<&str>) {
    let raw = database.unwrap_or(DB_LABEL_SYSTEM);
    let label = DB_LABEL_GUARD.resolve_database_label(raw);
    counter!(
        RESULT_CAPPED_TOTAL,
        "database" => label.into_owned(),
    )
    .increment(1);
}

/// v0.6.0 Fase 2 Task 6 — increment `tessera_query_timeouts_total{database}`
/// once per query aborted by the cooperative per-query timeout. `database`
/// follows the same [`DB_LABEL_GUARD`] cardinality gate as [`result_capped`].
pub(crate) fn query_timed_out(database: Option<&str>) {
    let raw = database.unwrap_or(DB_LABEL_SYSTEM);
    let label = DB_LABEL_GUARD.resolve_database_label(raw);
    counter!(
        QUERY_TIMEOUTS_TOTAL,
        "database" => label.into_owned(),
    )
    .increment(1);
}

/// Record `tessera_query_duration_seconds{database, kind}` with the
/// observed duration of one statement.
///
/// `kind ∈ {"query", "mutation", "pipeline", "const_return"}` and
/// matches the discriminants of [`GqlStatement`]. The histogram uses
/// the default Prometheus buckets installed by
/// [`PrometheusBuilder`]; per-bucket tuning is deferred to Task 2 once
/// production traces inform realistic boundaries.
///
/// [`GqlStatement`]: tessera_graph::gql::GqlStatement
pub(crate) fn query_duration(database: Option<&str>, kind: &'static str, secs: f64) {
    let raw = database.unwrap_or(DB_LABEL_SYSTEM);
    let label = DB_LABEL_GUARD.resolve_database_label(raw);
    histogram!(
        QUERY_DURATION_SECONDS,
        "database" => label.into_owned(),
        "kind" => kind,
    )
    .record(secs);
}


/// The histogram uses the default buckets installed by
/// `PrometheusBuilder` which cover ~5 ms to ~50 ms — adequate for
/// SSD-backed fsync latencies. Per-bucket tuning is deferred to
/// production telemetry as documented for `query_duration_seconds`
/// in Task 1 C6.
pub(crate) fn wal_fsync_observed(
    _cause: tessera_graph::FsyncCause,
    duration: std::time::Duration,
) {
    histogram!(WAL_FSYNC_DURATION_SECONDS).record(duration.as_secs_f64());
}

/// Kind tag for a parsed [`GqlStatement`], used as the `kind` label on
/// [`QUERY_DURATION_SECONDS`].
///
/// Returned as `&'static str` so the recorder can intern it without
/// allocating per-emit. Mirrors the discriminants the dispatcher in
/// `handler.rs::handle_run` already pattern-matches on. `Admin`
/// statements are dispatched through `dispatch_admin` and never reach
/// the query metrics path; the arm is still listed so the match stays
/// exhaustive and future GQL variants force an update here.
///
/// [`GqlStatement`]: tessera_graph::gql::GqlStatement
pub(crate) const fn gql_statement_kind(stmt: &tessera_graph::gql::GqlStatement) -> &'static str {
    use tessera_graph::gql::GqlStatement as S;
    match stmt {
        S::Query(_) => "query",
        S::Mutation(_) => "mutation",
        S::Pipeline(_) => "pipeline",
        S::ConstReturn(_) => "const_return",
        S::Admin(_) => "admin",
        S::Ddl(_) => "ddl",
        S::Call(_) => "call",
    }
}

/// Wire-level type tag for a `BoltRequest`.
///
/// Returned as `&'static str` so it can be used directly as a metric
/// label without allocating. Mirrors the discriminants the dispatcher
/// in `handler.rs::dispatch` already pattern-matches on.
pub(crate) const fn bolt_request_type_str(
    req: &tessera_graph_protocol::bolt_message::BoltRequest,
) -> &'static str {
    use tessera_graph_protocol::bolt_message::BoltRequest as R;
    match req {
        R::Hello { .. } => "HELLO",
        R::Logon { .. } => "LOGON",
        R::Run { .. } => "RUN",
        R::Pull { .. } => "PULL",
        R::Discard { .. } => "DISCARD",
        R::Reset => "RESET",
        R::Goodbye => "GOODBYE",
        R::Begin { .. } => "BEGIN",
        R::Commit => "COMMIT",
        R::Rollback => "ROLLBACK",
    }
}

async fn write_response(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
) -> io::Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        408 => "Request Timeout",
        413 => "Payload Too Large",
        _ => "Error",
    };
    let header = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {len}\r\n\
         Connection: close\r\n\
         \r\n",
        len = body.len(),
    );
    stream.write_all(header.as_bytes()).await?;
    if !body.is_empty() {
        stream.write_all(body).await?;
    }
    stream.shutdown().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── v0.6.0 Fase 2 Task 2 C3: wal_fsync_observed helper ─────────────
    //
    // The end-to-end verification (observation reaches the Prometheus
    // scrape) lands in C4 once the registry wires the observer into a
    // real `Graph`. C3 only owns the public surface of the helper:
    // the constant name and a non-panicking emission path. Keeping
    // the test inline (vs. in `tests/metrics_test.rs`) is the cheapest
    // way to exercise the `pub(crate)` helper without widening its
    // visibility just for testing.

    #[test]
    fn wal_fsync_duration_seconds_constant_matches_metric_name() {
        // The metric name is part of the public Prometheus scrape
        // contract — pin it explicitly so a typo or rename breaks
        // this test instead of silently emitting under a different
        // series name.
        assert_eq!(
            WAL_FSYNC_DURATION_SECONDS,
            "tessera_wal_fsync_duration_seconds",
        );
    }

    #[test]
    fn wal_fsync_observed_does_not_panic_for_zero_and_typical_durations() {
        // The recorder lives behind a `OnceLock` initialised
        // lazily by `install_or_get_recorder`; this test does NOT
        // install one, so the emission goes through the `metrics`
        // facade's no-op recorder. The contract under test is
        // "the helper never panics for any reasonable duration",
        // independent of whether anything is scraping.
        use tessera_graph::FsyncCause;
        wal_fsync_observed(FsyncCause::Individual, std::time::Duration::ZERO);
        wal_fsync_observed(FsyncCause::Individual, std::time::Duration::from_micros(50));
        wal_fsync_observed(FsyncCause::Individual, std::time::Duration::from_millis(5));
        wal_fsync_observed(FsyncCause::Individual, std::time::Duration::from_secs(1));
    }

    #[test]
    fn wal_fsync_observed_accepts_batch_close_cause_and_duration() {
        // The helper takes the fsync cause the engine now hands its observer.
        // It records the same histogram regardless of cause (no per-cause label,
        // see the helper's docstring), so both variants must be accepted without
        // panicking.
        use tessera_graph::FsyncCause;
        wal_fsync_observed(
            FsyncCause::BatchClose { op_count: 5 },
            std::time::Duration::from_micros(120),
        );
        wal_fsync_observed(
            FsyncCause::BatchClose { op_count: 0 },
            std::time::Duration::ZERO,
        );
    }
}
