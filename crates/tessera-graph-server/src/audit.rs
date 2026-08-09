// SPDX-License-Identifier: BSL-1.1

//! Audit log. One JSON event per line. Sinks: stdout, file (with
//! size-based rotation), off. Non-blocking via an MPSC channel +
//! dedicated writer task.
//!
//! Handler code calls into an `AuditSink` from the hot path; the sink
//! emits via `mpsc::Sender::try_send` and returns immediately. A single
//! writer task per sink drains the channel, serialises to JSON, and
//! writes to the configured destination. When the channel is full the
//! event is dropped and counted in an atomic; the writer emits a
//! dedicated `audit_backpressure` record the next time it has room so
//! losses are never silent.

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::Serialize;
use tokio::sync::{mpsc, watch};

use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

const CHANNEL_CAPACITY: usize = 10_000;
const PRINCIPAL_MAX_BYTES: usize = 256;
/// Upper bound on the `reason` field of [`AuditOutcome::Failed`]. The
/// reason is built from `format!("op: {e}")` where `e` is a store or
/// registry error; `AuthStoreError::Backend(String)` wraps arbitrary
/// I/O messages which can include long filesystem paths. Bounding the
/// length here keeps a single audit line predictable on disk and
/// prevents log bloat from chained I/O errors.
const REASON_MAX_BYTES: usize = 512;

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "event_type", rename_all = "snake_case")]
pub enum AuditEvent {
    ConnectionOpen {
        timestamp: String,
        connection_id: u64,
        user: Option<String>,
        details: ConnectionOpenDetails,
    },
    AuthSuccess {
        timestamp: String,
        connection_id: u64,
        user: Option<String>,
        details: AuthSuccessDetails,
    },
    AuthFailure {
        timestamp: String,
        connection_id: u64,
        user: Option<String>,
        details: AuthFailureDetails,
    },
    QueryExec {
        timestamp: String,
        connection_id: u64,
        user: Option<String>,
        details: QueryExecDetails,
    },
    AdminAction {
        timestamp: String,
        connection_id: u64,
        user: Option<String>,
        details: AdminActionDetails,
    },
    ConnectionClose {
        timestamp: String,
        connection_id: u64,
        user: Option<String>,
        details: ConnectionCloseDetails,
    },
    AuditBackpressure {
        timestamp: String,
        connection_id: u64,
        user: Option<String>,
        details: BackpressureDetails,
    },
    /// Spec §6.3 + §4.2/§8: a request was rejected by an
    /// authorization gate (HELLO database routing, RUN write-gate, or
    /// the RUN-before-HELLO guard). Wire-side error code is independent
    /// from this event — see [`AccessDeniedReason`] for the dispatch
    /// decision and the `database` field for the candidate name when
    /// known.
    AccessDenied {
        timestamp: String,
        connection_id: u64,
        user: Option<String>,
        details: AccessDeniedDetails,
    },
    /// Spec §6.3: a CREATE DATABASE catalog mutation was attempted.
    /// `details.outcome` distinguishes success from failure so audit
    /// log analysis can spot rejected creates (duplicate name, reserved
    /// identifier, store I/O error). `user` is the admin or CLI
    /// operator that issued the request.
    DatabaseCreated {
        timestamp: String,
        connection_id: u64,
        user: Option<String>,
        details: DatabaseCreatedDetails,
    },
    /// Spec §6.3: a DROP DATABASE catalog mutation was attempted.
    /// Mirrors [`AuditEvent::DatabaseCreated`] semantics for the
    /// removal side; failure outcome captures `database_in_use`,
    /// `database_not_found`, and store errors.
    DatabaseDropped {
        timestamp: String,
        connection_id: u64,
        user: Option<String>,
        details: DatabaseDroppedDetails,
    },
    /// Spec §6.3: a GRANT or REVOKE statement was attempted. The
    /// `details.action` discriminator (`grant` | `revoke`) keeps the
    /// two flows in a single event type so log analysers can compute
    /// per-user grant churn with one filter; `details.outcome` records
    /// whether the mutation actually took effect.
    GrantChanged {
        timestamp: String,
        connection_id: u64,
        user: Option<String>,
        details: GrantChangedDetails,
    },
    /// Block 3 Feature B: an online `tessera.snapshot` / `tessera.restore`
    /// admin procedure was attempted. Modelled as its own top-level event
    /// (like [`AuditEvent::DatabaseDropped`]) so log analysers can filter
    /// backup activity by `event_type`; `details.operation` distinguishes
    /// snapshot from restore and `details.outcome` records whether the
    /// physical copy actually completed. Restore is destructive and
    /// irreversible, so a success event is as important as a failure one.
    DatabaseBackup {
        timestamp: String,
        connection_id: u64,
        user: Option<String>,
        details: DatabaseBackupDetails,
    },
    /// v0.6.0 Fase 2 Task 3 — slow query observation. Emitted in
    /// addition to (not instead of) `QueryExec` when the configured
    /// `slow_query_threshold_ms` is exceeded and the per-connection
    /// rate gate allows it. Same field set as `QueryExec` plus the
    /// active `threshold_ms` so the audit consumer sees the threshold
    /// that triggered the line.
    SlowQuery {
        timestamp: String,
        connection_id: u64,
        user: Option<String>,
        details: SlowQueryDetails,
    },
    /// v0.6.0 Fase 2 Task 4 — query aborted by the defensive
    /// result-row cap (the Cap A match-count guard in the engine or
    /// the Cap B output guard at the `GraphAccessor` boundary). Emitted
    /// in addition to the regular `QueryExec` (with an `Error` outcome)
    /// so audit consumers can distinguish a cap abort from any other
    /// execution failure and see the row count that tripped it.
    ResultCapped {
        timestamp: String,
        connection_id: u64,
        user: Option<String>,
        details: ResultCappedDetails,
    },
    /// v0.6.0 Fase 2 Task 5 eje 1 — HELLO rejected because peer IP
    /// exceeded `auth_max_failures_per_minute` within the sliding window.
    #[serde(rename = "auth_throttled")]
    AuthThrottled(AuthThrottledDetails),
    /// v0.6.0 Fase 2 Task 5 eje 3 — TCP connection rejected by the accept
    /// loop because the peer IP already held `max_connections_per_ip`
    /// live connections. Emitted before the Bolt handshake.
    #[serde(rename = "connection_throttled")]
    ConnectionThrottled(ConnectionThrottledDetails),
    /// v0.6.0 Fase 2 Task 5 eje 2 — RUN/PULL/DISCARD rejected because the
    /// per-connection `TokenBucket` was exhausted. Emitted before the engine
    /// dispatch; the connection is kept alive so the driver can back off and
    /// retry. Wire code: `Neo.ClientError.Security.TooManyRequests`.
    #[serde(rename = "query_throttled")]
    QueryThrottled(QueryThrottledDetails),
    /// v0.6.0 Fase 2 Task 5 eje 4 — one aggregate entry emitted on
    /// connection close when the per-connection bandwidth cap caused at
    /// least one cooperative sleep. No per-byte event is emitted (the
    /// cardinality would be absurd); the I/O is slowed, never rejected.
    #[serde(rename = "bandwidth_throttled")]
    BandwidthThrottled(BandwidthThrottledDetails),
    /// v0.6.0 Fase 2 Task 6 — query aborted by the cooperative per-query
    /// timeout (the engine's deadline checks). Emitted in addition to the
    /// regular `QueryExec` (with an `Error` outcome) so audit consumers can
    /// distinguish a timeout abort from any other execution failure. Wire
    /// code: `Neo.ClientError.Statement.ExecutionFailed` (non-retryable).
    #[serde(rename = "query_timed_out")]
    QueryTimedOut(QueryTimedOutDetails),
}

#[derive(Debug, Clone, Serialize)]
pub struct ConnectionOpenDetails {
    pub peer_addr: String,
    pub tls: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct AuthSuccessDetails {
    pub principal: String,
    /// Database the session is routed to, when the server runs in
    /// multi-database mode (v0.5.0+). `None` for the legacy
    /// single-graph mode and for any auth flow that completes before
    /// the registry has acquired a handle.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub database: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AuthFailureDetails {
    pub principal_attempted: String,
    pub reason: AuthFailureReason,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthFailureReason {
    InvalidCredentials,
    UnknownUser,
    UserDisabled,
}

#[derive(Debug, Clone, Serialize)]
pub struct QueryExecDetails {
    pub statement_sha256: String,
    pub duration_ms: u64,
    pub row_count: u64,
    /// Database the statement ran against, or `None` for the legacy
    /// single-database path. Spec §6.3: every post-HELLO event in
    /// multi-database mode (v0.5.0+) carries this so audit log
    /// analysis can attribute traffic to tenants. Skipped on the
    /// wire when `None` to keep the schema additive for existing
    /// consumers.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub database: Option<String>,
    #[serde(flatten)]
    pub outcome: QueryOutcome,
}

/// Wire shape for the `slow_query` audit event. Mirrors
/// `QueryExecDetails` and adds the active `threshold_ms` so the
/// log consumer knows which threshold the line passed.
#[derive(Debug, Clone, Serialize)]
pub struct SlowQueryDetails {
    pub statement_sha256: String,
    pub duration_ms: u64,
    pub row_count: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub database: Option<String>,
    #[serde(flatten)]
    pub outcome: QueryOutcome,
    pub threshold_ms: u64,
}

/// Wire shape for the `result_capped` audit event. `row_count_seen` is
/// the row count the engine reported in the abort message (match count
/// for Cap A, output count for Cap B); `cap` is the active
/// `max_result_rows` that tripped it.
#[derive(Debug, Clone, Serialize)]
pub struct ResultCappedDetails {
    pub statement_sha256: String,
    pub row_count_seen: u64,
    pub cap: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub database: Option<String>,
}

/// v0.6.0 Fase 2 Task 5 eje 1 — payload of `AuditEvent::AuthThrottled`.
#[derive(Debug, Clone, Serialize)]
pub struct AuthThrottledDetails {
    pub client_ip: String,
    pub failures_in_window: u32,
    pub retry_after_seconds: u32,
}

/// v0.6.0 Fase 2 Task 5 eje 3 — payload of `AuditEvent::ConnectionThrottled`.
#[derive(Debug, Clone, Serialize)]
pub struct ConnectionThrottledDetails {
    pub client_ip: String,
    /// Live connections the peer IP held at the moment the next one was
    /// rejected (equals `cap` at the rejection boundary, surfaced
    /// separately so operators see the observed count, not just config).
    pub live_connections: u32,
    pub cap: u32,
}

/// v0.6.0 Fase 2 Task 5 eje 2 — payload of `AuditEvent::QueryThrottled`.
#[derive(Debug, Clone, Serialize)]
pub struct QueryThrottledDetails {
    pub connection_id: u64,
    pub user: String,
    pub statement_sha256: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub database: Option<String>,
    pub tokens_available: u64,
}

/// v0.6.0 Fase 2 Task 5 eje 4 — payload of `AuditEvent::BandwidthThrottled`.
/// Aggregated over the whole connection: how many times an I/O half had to
/// sleep for byte-tokens and the total time spent sleeping.
#[derive(Debug, Clone, Serialize)]
pub struct BandwidthThrottledDetails {
    pub connection_id: u64,
    pub total_sleeps: u64,
    pub total_sleep_duration_ms: u64,
}

/// v0.6.0 Fase 2 Task 6 — payload of `AuditEvent::QueryTimedOut`. Same shape
/// family as [`QueryThrottledDetails`] / [`ResultCappedDetails`], plus the
/// configured `timeout_ms` that the query overran.
#[derive(Debug, Clone, Serialize)]
pub struct QueryTimedOutDetails {
    pub connection_id: u64,
    pub user: String,
    pub statement_sha256: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub database: Option<String>,
    pub timeout_ms: u64,
}

/// v0.6.0 Fase 2 Task 3 — per-connection sliding-window rate limiter
/// for `AuditEvent::SlowQuery` emissions. Caps the number of events
/// per 60-second window to `cap`; `cap = 0` disables the cap.
///
/// The gate is single-threaded by construction — one instance lives in
/// each `BoltHandler`, accessed only from that handler's task. No
/// internal locking.
#[derive(Debug)]
pub struct SlowQueryGate {
    cap: u32,
    window_start: Option<std::time::Instant>,
    count: u32,
    drops_in_window: u32,
}

impl SlowQueryGate {
    const WINDOW: std::time::Duration = std::time::Duration::from_secs(60);

    /// Construct a gate with the given per-window cap. `cap = 0`
    /// turns the gate into a pass-through.
    #[must_use]
    pub const fn new(cap: u32) -> Self {
        Self {
            cap,
            window_start: None,
            count: 0,
            drops_in_window: 0,
        }
    }

    /// Decide whether the slow-query event at `now` should be
    /// emitted. Returns `true` when the gate permits emission.
    /// `cap = 0` always returns `true`. Increments internal counters
    /// as a side effect.
    pub fn allow(&mut self, now: std::time::Instant) -> bool {
        if self.cap == 0 {
            return true;
        }
        match self.window_start {
            Some(start) if now.duration_since(start) < Self::WINDOW => {
                if self.count < self.cap {
                    self.count += 1;
                    true
                } else {
                    self.drops_in_window += 1;
                    false
                }
            }
            _ => {
                // First call or window expired: open a new window.
                self.window_start = Some(now);
                self.count = 1;
                self.drops_in_window = 0;
                true
            }
        }
    }

    /// When the previous window has expired AND drops accumulated in
    /// it, return `Some(drops)` exactly once and reset the counter.
    /// Otherwise returns `None`. The handler polls this before each
    /// `allow` (and one final time on `Drop`) to surface a single
    /// `tracing::warn!` per closed window with drops.
    pub fn closed_window_drops(&mut self, now: std::time::Instant) -> Option<u32> {
        match self.window_start {
            Some(start)
                if now.duration_since(start) >= Self::WINDOW && self.drops_in_window > 0 =>
            {
                let drops = self.drops_in_window;
                self.drops_in_window = 0;
                Some(drops)
            }
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum QueryOutcome {
    Success,
    Error { error_code: String },
}

#[derive(Debug, Clone, Serialize)]
pub struct AdminActionDetails {
    #[serde(flatten)]
    pub action: AdminAction,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum AdminAction {
    CreateUser {
        target: String,
    },
    DropUser {
        target: String,
    },
    AlterUserPassword {
        target: String,
    },
    AlterUserStatus {
        target: String,
        enabled: bool,
    },
    AlterUserAdmin {
        target: String,
        is_admin: bool,
    },
    ShowUsers,
    // Task 14 (spec §6.3): CREATE/DROP DATABASE and GRANT/REVOKE now
    // emit their own top-level [`AuditEvent`] variants
    // ([`AuditEvent::DatabaseCreated`], [`AuditEvent::DatabaseDropped`],
    // [`AuditEvent::GrantChanged`]) so log analysers can filter by
    // `event_type` without parsing nested `action` discriminators.
    /// `SHOW DATABASES` — no target.
    ShowDatabases,
    /// `SHOW GRANTS [FOR <user>]` — `filter_user` is the argument if
    /// any was supplied, `None` for the unfiltered variant (admin-only).
    ShowGrants {
        filter_user: Option<String>,
    },
    Denied {
        attempted: String,
    },
    Failed {
        reason: String,
    },
}

/// Spec §6.3 details for [`AuditEvent::DatabaseCreated`]. `options`
/// mirrors the catalog `DatabaseOptions` without coupling audit
/// serialisation to the auth-store type — the audit schema is part
/// of the public surface and must evolve independently.
#[derive(Debug, Clone, Serialize)]
pub struct DatabaseCreatedDetails {
    pub name: String,
    pub options: DatabaseOptionsAudit,
    #[serde(flatten)]
    pub outcome: AuditOutcome,
}

/// Audit-facing mirror of `crate::auth::DatabaseOptions`. Each field
/// uses `skip_serializing_if = "Option::is_none"` so unset quotas do
/// not bloat the log; consumers must treat absence as "unbounded".
///
/// **Deployment invariant — 64-bit host.** `max_connections` is
/// `Option<usize>` to match the auth-store type exactly (any conversion
/// risks silent truncation when a quota is later read back from the
/// catalog). The whole server is built and shipped as a 64-bit binary
/// (Docker images, release tarballs), so `usize` and `u64` serialise
/// identically on the wire. A 32-bit deployment is not a supported
/// configuration; if that changes the audit schema must move to
/// explicit `u64` and the auth store with it.
#[derive(Debug, Clone, Serialize)]
pub struct DatabaseOptionsAudit {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_size_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_connections: Option<usize>,
}

/// Spec §6.3 details for [`AuditEvent::DatabaseDropped`].
#[derive(Debug, Clone, Serialize)]
pub struct DatabaseDroppedDetails {
    pub name: String,
    #[serde(flatten)]
    pub outcome: AuditOutcome,
}

/// Spec §6.3 details for [`AuditEvent::GrantChanged`]. `access_level`
/// carries the `READ` / `READ_WRITE` token for grants; for revokes the
/// caller should pass `""` (empty string) — the level is meaningless
/// when the grant row is being removed.
#[derive(Debug, Clone, Serialize)]
pub struct GrantChangedDetails {
    pub user_target: String,
    pub database: String,
    pub access_level: String,
    pub action: GrantChangeAction,
    #[serde(flatten)]
    pub outcome: AuditOutcome,
}

/// Discriminator for [`AuditEvent::GrantChanged`]. Snake-case wire
/// form (`"grant"` / `"revoke"`) lets log analysers branch on the
/// mutation direction without inspecting `access_level`.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GrantChangeAction {
    Grant,
    Revoke,
}

/// Discriminator for [`AuditEvent::DatabaseBackup`]. Snake-case wire form
/// (`"snapshot"` / `"restore"`) lets log analysers separate the read-only
/// snapshot path from the destructive restore path with a single filter.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BackupOperation {
    Snapshot,
    Restore,
}

/// Details for [`AuditEvent::DatabaseBackup`]. `outcome` is flattened
/// (same convention as the Task 14 catalog events), so the JSON carries
/// `outcome` (always) and `reason` (on failure) as siblings of `name` and
/// `operation`. The destination/source path is deliberately **not**
/// recorded here: it can contain operator-private filesystem layout and the
/// audit stream may be shipped off-box.
#[derive(Debug, Clone, Serialize)]
pub struct DatabaseBackupDetails {
    pub name: String,
    pub operation: BackupOperation,
    #[serde(flatten)]
    pub outcome: AuditOutcome,
}

/// Outcome discriminator shared by the three Task 14 catalog events.
/// Modelled like [`QueryOutcome`]: `#[serde(tag = "outcome")]` so the
/// flattened `outcome` field is emitted as a sibling of the action-
/// specific fields, and `Failed { reason }` carries a free-text
/// snapshot of the error for forensic analysis.
///
/// **Reserved field names (do not reuse in detail structs that flatten
/// this enum).** Because [`DatabaseCreatedDetails`],
/// [`DatabaseDroppedDetails`] and [`GrantChangedDetails`] all carry
/// `#[serde(flatten)] outcome: AuditOutcome`, the JSON output will
/// contain `"outcome"` (always) and `"reason"` (when `Failed`) as
/// top-level keys of `details`. Detail structs MUST NOT introduce
/// fields named `outcome` or `reason` — serde flatten resolves
/// duplicates silently and the resulting JSON would have non-
/// deterministic field ordering.
///
/// `reason` is truncated to [`REASON_MAX_BYTES`] in [`bound_outcome`];
/// callers may pass an arbitrary `String` without pre-truncating.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum AuditOutcome {
    Success,
    Failed { reason: String },
}

#[derive(Debug, Clone, Serialize)]
pub struct ConnectionCloseDetails {
    pub reason: CloseReason,
    pub queries_executed: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CloseReason {
    Goodbye,
    IdleTimeout,
    Shutdown,
    IoError,
    HandshakeFailed,
}

#[derive(Debug, Clone, Serialize)]
pub struct AccessDeniedDetails {
    pub reason: AccessDeniedReason,
    /// Database the request targeted, when known. `None` for the
    /// `not_authenticated` path because the session has no `DbHandle`
    /// at the time of denial. Skipped on the wire when absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub database: Option<String>,
}

/// Why a request was denied. Snake-case wire form lets log analysers
/// branch by reason without parsing free-text messages.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AccessDeniedReason {
    /// HELLO regex/reserved name guard rejected the candidate.
    InvalidDatabaseName,
    /// `registry.acquire` returned `Unauthorized` — non-admin user
    /// without a matching grant. Spec §8 keeps this code ambiguous on
    /// the wire so the existence of a database is not leaked, but
    /// the audit event records the actual cause.
    Unauthorized,
    /// `registry.acquire` returned `DatabaseNotFound` — wildcard or
    /// admin path with an unknown name. Distinct from `Unauthorized`
    /// so log analysers can reason about catalogue churn.
    DatabaseNotFound,
    /// RUN dispatcher refused a mutating statement because the
    /// session's `DbHandle` carries `AccessLevel::Read`.
    WriteGateForbidden,
    /// RUN arrived before HELLO completed — protocol-violation, not
    /// a credentials issue.
    NotAuthenticated,
    /// A non-admin user attempted an admin-only operation (e.g. a backup
    /// procedure `tessera.snapshot`/`tessera.restore`).
    NotAdmin,
}

#[derive(Debug, Clone, Serialize)]
pub struct BackpressureDetails {
    pub dropped: u64,
}

fn now_ts() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_owned())
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_owned();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_owned()
}

/// Apply [`REASON_MAX_BYTES`] to the `reason` field of a `Failed`
/// outcome, leaving `Success` untouched. Centralising the bound here
/// (rather than at every emission site) keeps the truncation policy in
/// one place; the three Task 14 sink methods forward through this.
fn bound_outcome(outcome: AuditOutcome) -> AuditOutcome {
    match outcome {
        AuditOutcome::Success => AuditOutcome::Success,
        AuditOutcome::Failed { reason } => AuditOutcome::Failed {
            reason: truncate(&reason, REASON_MAX_BYTES),
        },
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AuditError {
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
}

/// Pluggable audit destination — the extension point for Enterprise.
///
/// Community ships the basic destinations directly on [`AuditSink`]
/// (`off`, `stdout`, rotating `file`, synchronous `file_sync_oneshot`): writing
/// audit events to a local file or stdout is baseline infrastructure and stays
/// open. Compliance-grade auditing — log rotation with signing, retention
/// guarantees, forwarding to an external SIEM — is Enterprise, and plugs in as
/// a `Custom` backend via [`AuditSink::custom`]. The trait is deliberately tiny
/// (one method) and public so the separate Enterprise crate can implement it.
///
/// Implementations must be non-blocking on the hot path or do their own
/// buffering: `emit` is called inline from connection and query handling.
pub trait AuditBackend: Send + Sync + 'static {
    /// Record one audit event. Must not panic; errors are the backend's own
    /// concern (an audit-write failure must never abort the primary operation).
    fn emit(&self, event: &AuditEvent);
}

enum SinkKind {
    Off,
    Channel(mpsc::Sender<AuditEvent>, Arc<AtomicU64>),
    /// Enterprise-supplied destination (compliance logging, SIEM forwarding).
    /// Community never constructs this variant; the Enterprise crate builds an
    /// `AuditSink` around its own [`AuditBackend`] via [`AuditSink::custom`].
    Custom(Arc<dyn AuditBackend>),
    /// Task 14 ciclo 4: synchronous file append used by the CLI offline
    /// path. Each emission opens the file in append mode, writes the
    /// JSON line, fsyncs, and closes. No tokio task, no MPSC channel —
    /// the CLI emits 1–2 events per invocation with zero concurrent
    /// producers, so backpressure and batching are unnecessary.
    Sync(PathBuf),
    /// Test-only unbounded channel. No backpressure, no I/O tasks —
    /// the receiver drains synchronously in the same test thread.
    #[cfg(test)]
    TestChannel(tokio::sync::mpsc::UnboundedSender<AuditEvent>),
}

pub struct AuditSink {
    kind: SinkKind,
}

impl Clone for AuditSink {
    fn clone(&self) -> Self {
        let kind = match &self.kind {
            SinkKind::Off => SinkKind::Off,
            SinkKind::Channel(tx, d) => SinkKind::Channel(tx.clone(), Arc::clone(d)),
            SinkKind::Sync(path) => SinkKind::Sync(path.clone()),
            SinkKind::Custom(backend) => SinkKind::Custom(Arc::clone(backend)),
            #[cfg(test)]
            SinkKind::TestChannel(tx) => SinkKind::TestChannel(tx.clone()),
        };
        Self { kind }
    }
}

impl AuditSink {
    #[must_use]
    pub const fn off() -> Self {
        Self {
            kind: SinkKind::Off,
        }
    }

    /// Build a sink that forwards every event to an Enterprise-supplied
    /// [`AuditBackend`] (compliance logging, SIEM forwarding). Community never
    /// calls this; it is the seam the Enterprise crate uses to plug in its own
    /// destination while reusing the whole event-construction surface
    /// (`connection_open`, `query_exec`, …) unchanged.
    #[must_use]
    pub fn custom(backend: Arc<dyn AuditBackend>) -> Self {
        Self {
            kind: SinkKind::Custom(backend),
        }
    }

    #[must_use]
    pub fn stdout(shutdown: watch::Receiver<bool>) -> Self {
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        let dropped = Arc::new(AtomicU64::new(0));
        let d2 = Arc::clone(&dropped);
        tokio::spawn(stdout_writer_task(rx, shutdown, d2));
        Self {
            kind: SinkKind::Channel(tx, dropped),
        }
    }

    /// Spawn a writer task that emits events to `path`.
    ///
    /// # Errors
    ///
    /// Returns [`AuditError::Io`] if the parent directory cannot be
    /// created or the initial file cannot be opened.
    pub fn file(
        path: PathBuf,
        max_bytes: u64,
        keep_files: u32,
        fsync_every: u32,
        shutdown: watch::Receiver<bool>,
    ) -> Result<Self, AuditError> {
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        let dropped = Arc::new(AtomicU64::new(0));
        let d2 = Arc::clone(&dropped);
        let state = FileState::open(&path)?;
        tokio::spawn(file_writer_task(
            state,
            path,
            max_bytes,
            keep_files,
            fsync_every,
            rx,
            shutdown,
            d2,
        ));
        Ok(Self {
            kind: SinkKind::Channel(tx, dropped),
        })
    }

    /// Synchronous one-shot file sink for the CLI offline path
    /// (Task 14 ciclo 4). Each emission opens the file in append
    /// mode, writes a JSON line, fsyncs, and closes. No tokio task,
    /// no MPSC channel, no backpressure counter — the CLI emits 1–2
    /// events per invocation and has no concurrent producers.
    ///
    /// The constructor validates that the parent directory exists or
    /// can be created and that the file can be opened in append
    /// mode. Subsequent per-event I/O failures during [`send`] are
    /// swallowed: the CLI already surfaces the primary operational
    /// result via stdout/stderr and an exit code; an audit-write
    /// failure should not mask it.
    ///
    /// # Errors
    ///
    /// Returns [`AuditError::Io`] if the parent directory cannot be
    /// created or the file cannot be opened for append.
    pub fn file_sync_oneshot(path: &Path) -> Result<Self, AuditError> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        // Open-and-close the file once to surface permission errors at
        // construction time rather than at first emission. The actual
        // append-write happens per event in `send`.
        let _f = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self {
            kind: SinkKind::Sync(path.to_path_buf()),
        })
    }

    fn send(&self, event: AuditEvent) {
        match &self.kind {
            SinkKind::Off => {}
            SinkKind::Channel(tx, dropped) => {
                if tx.try_send(event).is_err() {
                    dropped.fetch_add(1, Ordering::Relaxed);
                }
            }
            SinkKind::Sync(path) => {
                // CLI offline path: open-append-write-fsync-close per
                // event. Errors are swallowed (the CLI already prints
                // the operational result to stdout/stderr and exits
                // with the right code; failing to write the audit
                // line should not mask the primary outcome). The path
                // existence and parent-dir creation were validated at
                // construction time in `file_sync_oneshot`.
                let _ = sync_append_event(path, &event);
            }
            SinkKind::Custom(backend) => backend.emit(&event),
            #[cfg(test)]
            SinkKind::TestChannel(tx) => {
                let _ = tx.send(event);
            }
        }
    }

    pub fn connection_open(&self, conn_id: u64, peer: SocketAddr, tls: bool) {
        self.send(AuditEvent::ConnectionOpen {
            timestamp: now_ts(),
            connection_id: conn_id,
            user: None,
            details: ConnectionOpenDetails {
                peer_addr: peer.to_string(),
                tls,
            },
        });
    }

    pub fn auth_success(&self, conn_id: u64, user: &str, principal: &str) {
        self.auth_success_with_database(conn_id, user, principal, None);
    }

    /// Multi-database (v0.5.0) variant of [`auth_success`]. Records the
    /// database the session is routed to when the server runs in
    /// multi-database mode. The legacy [`auth_success`] delegates here
    /// with `database = None` so existing callers keep their semantics.
    pub fn auth_success_with_database(
        &self,
        conn_id: u64,
        user: &str,
        principal: &str,
        database: Option<&str>,
    ) {
        self.send(AuditEvent::AuthSuccess {
            timestamp: now_ts(),
            connection_id: conn_id,
            user: Some(truncate(user, PRINCIPAL_MAX_BYTES)),
            details: AuthSuccessDetails {
                principal: truncate(principal, PRINCIPAL_MAX_BYTES),
                // 64 = `validate_database_name` upper bound + 1 for
                // truncation safety. A truncated value never matters at
                // the audit layer because the registry already rejected
                // any name beyond the bound; only adversarial inputs
                // would be longer here.
                database: database.map(|d| truncate(d, 64)),
            },
        });
    }

    pub fn auth_failure(&self, conn_id: u64, principal: &str, reason: AuthFailureReason) {
        self.send(AuditEvent::AuthFailure {
            timestamp: now_ts(),
            connection_id: conn_id,
            user: None,
            details: AuthFailureDetails {
                principal_attempted: truncate(principal, PRINCIPAL_MAX_BYTES),
                reason,
            },
        });
    }

    pub fn query_exec(
        &self,
        conn_id: u64,
        user: &str,
        stmt_hash: &str,
        duration_ms: u64,
        row_count: u64,
        outcome: QueryOutcome,
    ) {
        self.query_exec_with_database(
            conn_id,
            user,
            None,
            stmt_hash,
            duration_ms,
            row_count,
            outcome,
        );
    }

    /// Multi-database (v0.5.0) variant of [`query_exec`]. Records the
    /// database the statement ran against. Spec section 6.3 mandates
    /// every post-HELLO event in multi-database mode carry this field
    /// so audit log analysis can attribute traffic to tenants. Legacy
    /// [`query_exec`] delegates here with `database = None` and the
    /// field is skipped on the wire to keep the schema additive.
    #[allow(clippy::too_many_arguments)]
    pub fn query_exec_with_database(
        &self,
        conn_id: u64,
        user: &str,
        database: Option<&str>,
        stmt_hash: &str,
        duration_ms: u64,
        row_count: u64,
        outcome: QueryOutcome,
    ) {
        self.send(AuditEvent::QueryExec {
            timestamp: now_ts(),
            connection_id: conn_id,
            user: Some(truncate(user, PRINCIPAL_MAX_BYTES)),
            details: QueryExecDetails {
                statement_sha256: stmt_hash.to_owned(),
                duration_ms,
                row_count,
                // 64 = validate_database_name upper bound + 1 for
                // truncation safety. Mirrors AuthSuccessDetails.
                database: database.map(|d| truncate(d, 64)),
                outcome,
            },
        });
    }

    pub fn admin_action(&self, conn_id: u64, user: &str, action: AdminAction) {
        self.send(AuditEvent::AdminAction {
            timestamp: now_ts(),
            connection_id: conn_id,
            user: Some(truncate(user, PRINCIPAL_MAX_BYTES)),
            details: AdminActionDetails { action },
        });
    }

    /// Emit an `access_denied` event. `user` is `None` when the
    /// principal is not yet known (RUN-before-HELLO path); otherwise
    /// the authenticated username. `database` is the candidate name
    /// when relevant, omitted on the wire when `None`.
    pub fn access_denied(
        &self,
        conn_id: u64,
        user: Option<&str>,
        reason: AccessDeniedReason,
        database: Option<&str>,
    ) {
        self.send(AuditEvent::AccessDenied {
            timestamp: now_ts(),
            connection_id: conn_id,
            user: user.map(|u| truncate(u, PRINCIPAL_MAX_BYTES)),
            details: AccessDeniedDetails {
                reason,
                // 64 = validate_database_name upper bound + 1 for
                // truncation safety. Mirrors AuthSuccessDetails.
                database: database.map(|d| truncate(d, 64)),
            },
        });
    }

    /// Emit a `database_created` event (spec §6.3). `outcome` carries
    /// the catalog mutation result so failed creates (duplicate name,
    /// store I/O) are auditable. The CLI offline path and the Bolt
    /// admin handler both call this — the `user` field disambiguates
    /// the two via the `"cli:{uid}@{hostname}"` convention for the
    /// offline path.
    pub fn database_created(
        &self,
        conn_id: u64,
        user: &str,
        name: &str,
        options: DatabaseOptionsAudit,
        outcome: AuditOutcome,
    ) {
        self.send(AuditEvent::DatabaseCreated {
            timestamp: now_ts(),
            connection_id: conn_id,
            user: Some(truncate(user, PRINCIPAL_MAX_BYTES)),
            details: DatabaseCreatedDetails {
                // 64 = validate_database_name upper bound + 1 for
                // truncation safety. Mirrors AuthSuccessDetails.
                name: truncate(name, 64),
                options,
                outcome: bound_outcome(outcome),
            },
        });
    }

    /// Emit a `database_dropped` event (spec §6.3).
    pub fn database_dropped(&self, conn_id: u64, user: &str, name: &str, outcome: AuditOutcome) {
        self.send(AuditEvent::DatabaseDropped {
            timestamp: now_ts(),
            connection_id: conn_id,
            user: Some(truncate(user, PRINCIPAL_MAX_BYTES)),
            details: DatabaseDroppedDetails {
                name: truncate(name, 64),
                outcome: bound_outcome(outcome),
            },
        });
    }

    /// Emit a `database_backup` event (Block 3 Feature B). `operation`
    /// distinguishes the read-only snapshot from the destructive restore;
    /// `outcome` records whether the physical copy completed. The
    /// destination/source path is intentionally omitted — see
    /// [`DatabaseBackupDetails`].
    pub fn database_backup(
        &self,
        conn_id: u64,
        user: &str,
        name: &str,
        operation: BackupOperation,
        outcome: AuditOutcome,
    ) {
        self.send(AuditEvent::DatabaseBackup {
            timestamp: now_ts(),
            connection_id: conn_id,
            user: Some(truncate(user, PRINCIPAL_MAX_BYTES)),
            details: DatabaseBackupDetails {
                name: truncate(name, 64),
                operation,
                outcome: bound_outcome(outcome),
            },
        });
    }

    /// Emit a `grant_changed` event (spec §6.3). For `REVOKE` the
    /// caller passes `access_level = ""` because the grant row is
    /// removed and the level is meaningless.
    #[allow(clippy::too_many_arguments)]
    pub fn grant_changed(
        &self,
        conn_id: u64,
        user: &str,
        user_target: &str,
        database: &str,
        access_level: &str,
        action: GrantChangeAction,
        outcome: AuditOutcome,
    ) {
        self.send(AuditEvent::GrantChanged {
            timestamp: now_ts(),
            connection_id: conn_id,
            user: Some(truncate(user, PRINCIPAL_MAX_BYTES)),
            details: GrantChangedDetails {
                user_target: truncate(user_target, PRINCIPAL_MAX_BYTES),
                database: truncate(database, 64),
                access_level: truncate(access_level, 32),
                action,
                outcome: bound_outcome(outcome),
            },
        });
    }

    pub fn connection_close(
        &self,
        conn_id: u64,
        user: Option<&str>,
        reason: CloseReason,
        queries_executed: u64,
    ) {
        self.send(AuditEvent::ConnectionClose {
            timestamp: now_ts(),
            connection_id: conn_id,
            user: user.map(|u| truncate(u, PRINCIPAL_MAX_BYTES)),
            details: ConnectionCloseDetails {
                reason,
                queries_executed,
            },
        });
    }

    /// Emit a `SlowQuery` audit event. Caller is responsible for
    /// gating the call on the threshold and the rate limiter; this
    /// helper unconditionally enqueues the event so the call site
    /// owns the decision.
    #[allow(clippy::too_many_arguments)]
    pub fn slow_query(
        &self,
        connection_id: u64,
        user: Option<&str>,
        statement_sha256: &str,
        database: Option<&str>,
        duration_ms: u64,
        row_count: u64,
        outcome: QueryOutcome,
        threshold_ms: u64,
    ) {
        self.send(AuditEvent::SlowQuery {
            timestamp: now_ts(),
            connection_id,
            user: user.map(str::to_owned),
            details: SlowQueryDetails {
                statement_sha256: statement_sha256.to_owned(),
                duration_ms,
                row_count,
                database: database.map(str::to_owned),
                outcome,
                threshold_ms,
            },
        });
    }

    /// Emit a `ResultCapped` audit event. Caller is responsible for
    /// detecting the cap abort (the engine sentinel prefix on the
    /// failure message); this helper unconditionally enqueues the
    /// event. `user` matches the `Option<&str>` convention of
    /// [`slow_query`].
    pub fn result_capped(
        &self,
        connection_id: u64,
        user: Option<&str>,
        statement_sha256: &str,
        row_count_seen: u64,
        cap: u64,
        database: Option<&str>,
    ) {
        self.send(AuditEvent::ResultCapped {
            timestamp: now_ts(),
            connection_id,
            user: user.map(str::to_owned),
            details: ResultCappedDetails {
                statement_sha256: statement_sha256.to_owned(),
                row_count_seen,
                cap,
                database: database.map(str::to_owned),
            },
        });
    }

    /// v0.6.0 Fase 2 Task 5 — emit an `auth_throttled` audit event.
    pub fn auth_throttled(&self, details: AuthThrottledDetails) {
        self.send(AuditEvent::AuthThrottled(details));
    }

    /// v0.6.0 Fase 2 Task 5 — emit a `connection_throttled` audit event.
    pub fn connection_throttled(&self, details: ConnectionThrottledDetails) {
        self.send(AuditEvent::ConnectionThrottled(details));
    }

    /// v0.6.0 Fase 2 Task 5 eje 2 — emit a `query_throttled` audit event.
    pub fn query_throttled(&self, details: QueryThrottledDetails) {
        self.send(AuditEvent::QueryThrottled(details));
    }

    /// v0.6.0 Fase 2 Task 5 eje 4 — emit a `bandwidth_throttled` audit event.
    pub fn bandwidth_throttled(&self, details: BandwidthThrottledDetails) {
        self.send(AuditEvent::BandwidthThrottled(details));
    }

    /// v0.6.0 Fase 2 Task 6 — emit a `query_timed_out` audit event.
    pub fn query_timed_out(&self, details: QueryTimedOutDetails) {
        self.send(AuditEvent::QueryTimedOut(details));
    }

    /// v0.6.0 Fase 2 Task 3 — emit the regular `query_exec` audit event
    /// AND, when the duration crosses `threshold_ms > 0` and the
    /// per-connection `gate` permits, a companion `SlowQuery` event.
    /// Centralises the slow-query branch so the five call sites in
    /// `handle_run` stay readable and consistent. `user` matches the
    /// `&str` convention of [`query_exec`]; the slow-query helper takes
    /// `Option<&str>`, so it is wrapped in `Some` here.
    #[allow(clippy::too_many_arguments)]
    pub fn emit_query_pair(
        &self,
        gate: &mut SlowQueryGate,
        threshold_ms: u64,
        now: std::time::Instant,
        connection_id: u64,
        user: &str,
        statement_sha256: &str,
        database: Option<&str>,
        duration_ms: u64,
        row_count: u64,
        outcome: QueryOutcome,
    ) {
        self.query_exec_with_database(
            connection_id,
            user,
            database,
            statement_sha256,
            duration_ms,
            row_count,
            outcome.clone(),
        );
        if threshold_ms > 0 && duration_ms >= threshold_ms && gate.allow(now) {
            self.slow_query(
                connection_id,
                Some(user),
                statement_sha256,
                database,
                duration_ms,
                row_count,
                outcome,
                threshold_ms,
            );
        }
    }

    /// Return a `(AuditSink, Receiver<AuditEvent>)` pair backed by an
    /// in-memory MPSC channel. The sink enqueues events synchronously
    /// so the receiver can drain them in the same thread without a
    /// Tokio runtime. Intended only for unit tests that assert event
    /// shape without spawning I/O tasks.
    #[cfg(test)]
    #[must_use]
    pub fn channel_for_testing() -> (Self, tokio::sync::mpsc::UnboundedReceiver<AuditEvent>) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let sink = Self {
            kind: SinkKind::TestChannel(tx),
        };
        (sink, rx)
    }
}

async fn stdout_writer_task(
    mut rx: mpsc::Receiver<AuditEvent>,
    mut shutdown: watch::Receiver<bool>,
    dropped: Arc<AtomicU64>,
) {
    let stdout = std::io::stdout();
    loop {
        tokio::select! {
            biased;
            _ = shutdown.changed() => break,
            event = rx.recv() => {
                let Some(event) = event else { break };
                let mut handle = stdout.lock();
                let _ = write_event(&event, &mut handle);
                drain_backpressure(&dropped, &mut handle);
            }
        }
    }
    while let Ok(event) = rx.try_recv() {
        let mut handle = stdout.lock();
        let _ = write_event(&event, &mut handle);
    }
}

fn write_event(event: &AuditEvent, w: &mut impl Write) -> std::io::Result<()> {
    let mut line = serde_json::to_vec(event).map_err(std::io::Error::other)?;
    line.push(b'\n');
    w.write_all(&line)?;
    w.flush()
}

/// Open-append-write-fsync-close for a single event. Used by
/// [`SinkKind::Sync`] (the CLI offline path). Each call is independent;
/// crash recovery is line-granular because every successful return
/// implies the line is durably on disk.
fn sync_append_event(path: &Path, event: &AuditEvent) -> std::io::Result<()> {
    let mut f = OpenOptions::new().create(true).append(true).open(path)?;
    write_event(event, &mut f)?;
    f.sync_data()?;
    Ok(())
}

fn drain_backpressure(dropped: &Arc<AtomicU64>, w: &mut impl Write) {
    let n = dropped.swap(0, Ordering::Relaxed);
    if n > 0 {
        let ev = AuditEvent::AuditBackpressure {
            timestamp: now_ts(),
            connection_id: 0,
            user: None,
            details: BackpressureDetails { dropped: n },
        };
        let _ = write_event(&ev, w);
    }
}

struct FileState {
    w: BufWriter<File>,
    written: u64,
}

impl FileState {
    fn open(path: &Path) -> std::io::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let f = OpenOptions::new().create(true).append(true).open(path)?;
        let written = f.metadata()?.len();
        Ok(Self {
            w: BufWriter::new(f),
            written,
        })
    }

    const fn would_exceed(&self, max: u64, incoming: u64) -> bool {
        self.written.saturating_add(incoming) > max
    }

    fn flush_and_sync(&mut self) -> std::io::Result<()> {
        self.w.flush()?;
        self.w.get_ref().sync_data()?;
        Ok(())
    }
}

fn with_suffix(base: &Path, n: u32) -> PathBuf {
    let mut s = base.as_os_str().to_owned();
    s.push(format!(".{n}"));
    PathBuf::from(s)
}

fn rotate(state: &mut FileState, base: &Path, keep: u32) -> std::io::Result<()> {
    state.flush_and_sync()?;
    let last = with_suffix(base, keep);
    if last.exists() {
        let _ = std::fs::remove_file(&last);
    }
    for i in (1..keep).rev() {
        let from = with_suffix(base, i);
        let to = with_suffix(base, i + 1);
        if from.exists() {
            std::fs::rename(&from, &to)?;
        }
    }
    if base.exists() {
        std::fs::rename(base, with_suffix(base, 1))?;
    }
    let f = OpenOptions::new().create(true).append(true).open(base)?;
    state.w = BufWriter::new(f);
    state.written = 0;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn file_writer_task(
    mut state: FileState,
    path: PathBuf,
    max_bytes: u64,
    keep_files: u32,
    fsync_every: u32,
    mut rx: mpsc::Receiver<AuditEvent>,
    mut shutdown: watch::Receiver<bool>,
    dropped: Arc<AtomicU64>,
) {
    let mut since_sync: u32 = 0;
    loop {
        tokio::select! {
            biased;
            _ = shutdown.changed() => break,
            event = rx.recv() => {
                let Some(event) = event else { break };
                handle_one(
                    &mut state, &path, max_bytes, keep_files, fsync_every,
                    &event, &mut since_sync,
                );
                drain_file_backpressure(
                    &mut state, &path, max_bytes, keep_files, fsync_every,
                    &dropped, &mut since_sync,
                );
            }
        }
    }
    while let Ok(event) = rx.try_recv() {
        handle_one(
            &mut state,
            &path,
            max_bytes,
            keep_files,
            fsync_every,
            &event,
            &mut since_sync,
        );
    }
    let _ = state.flush_and_sync();
}

fn handle_one(
    state: &mut FileState,
    path: &Path,
    max_bytes: u64,
    keep_files: u32,
    fsync_every: u32,
    event: &AuditEvent,
    since_sync: &mut u32,
) {
    let Ok(line) = serde_json::to_vec(event) else {
        return;
    };
    let len = line.len() as u64 + 1;
    if state.would_exceed(max_bytes, len) {
        let _ = rotate(state, path, keep_files);
    }
    if state.w.write_all(&line).is_ok() && state.w.write_all(b"\n").is_ok() {
        state.written += len;
        *since_sync += 1;
        if fsync_every > 0 && *since_sync >= fsync_every {
            let _ = state.flush_and_sync();
            *since_sync = 0;
        }
    }
}

fn drain_file_backpressure(
    state: &mut FileState,
    path: &Path,
    max_bytes: u64,
    keep_files: u32,
    fsync_every: u32,
    dropped: &Arc<AtomicU64>,
    since_sync: &mut u32,
) {
    let n = dropped.swap(0, Ordering::Relaxed);
    if n == 0 {
        return;
    }
    let ev = AuditEvent::AuditBackpressure {
        timestamp: now_ts(),
        connection_id: 0,
        user: None,
        details: BackpressureDetails { dropped: n },
    };
    handle_one(
        state,
        path,
        max_bytes,
        keep_files,
        fsync_every,
        &ev,
        since_sync,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slow_query_gate_allows_up_to_cap_within_window() {
        let cap = 3;
        let mut gate = SlowQueryGate::new(cap);
        let t0 = std::time::Instant::now();
        assert!(gate.allow(t0));
        assert!(gate.allow(t0 + std::time::Duration::from_secs(10)));
        assert!(gate.allow(t0 + std::time::Duration::from_secs(20)));
        assert!(!gate.allow(t0 + std::time::Duration::from_secs(30)));
    }

    #[test]
    fn slow_query_gate_resets_after_window() {
        let mut gate = SlowQueryGate::new(2);
        let t0 = std::time::Instant::now();
        assert!(gate.allow(t0));
        assert!(gate.allow(t0));
        assert!(!gate.allow(t0));
        let t_next = t0 + std::time::Duration::from_secs(61);
        assert!(gate.allow(t_next));
    }

    #[test]
    fn slow_query_gate_with_cap_zero_is_a_pass_through() {
        let mut gate = SlowQueryGate::new(0);
        let t0 = std::time::Instant::now();
        for i in 0..1000 {
            assert!(
                gate.allow(t0 + std::time::Duration::from_millis(i)),
                "cap=0 must let every event pass (i={i})"
            );
        }
    }

    #[test]
    fn slow_query_gate_closed_window_drops_reports_drops_once() {
        let mut gate = SlowQueryGate::new(1);
        let t0 = std::time::Instant::now();
        assert!(gate.allow(t0));
        assert!(!gate.allow(t0));
        assert!(!gate.allow(t0));
        assert!(!gate.allow(t0));
        assert_eq!(gate.closed_window_drops(t0), None);
        let t_after = t0 + std::time::Duration::from_secs(61);
        assert_eq!(gate.closed_window_drops(t_after), Some(3));
        assert_eq!(gate.closed_window_drops(t_after), None);
    }

    #[test]
    fn slow_query_event_serializes_with_expected_shape() {
        let (sink, mut rx) = AuditSink::channel_for_testing();
        sink.slow_query(
            42,
            Some("alice"),
            "ecdae70d000000000000000000000000000000000000000000000000000000ff",
            Some("fsync_db"),
            1543,
            2890,
            QueryOutcome::Success,
            1000,
        );
        let event = rx.try_recv().expect("event must be queued");
        let json = serde_json::to_value(&event).expect("serialize");
        let evt_type = json
            .get("event_type")
            .and_then(serde_json::Value::as_str)
            .expect("event_type field");
        assert_eq!(evt_type, "slow_query");
        let details = json.get("details").expect("details object");
        assert_eq!(
            details
                .get("statement_sha256")
                .and_then(serde_json::Value::as_str)
                .expect("statement_sha256"),
            "ecdae70d000000000000000000000000000000000000000000000000000000ff"
        );
        assert_eq!(
            details
                .get("duration_ms")
                .and_then(serde_json::Value::as_u64),
            Some(1543)
        );
        assert_eq!(
            details.get("row_count").and_then(serde_json::Value::as_u64),
            Some(2890)
        );
        assert_eq!(
            details.get("database").and_then(serde_json::Value::as_str),
            Some("fsync_db")
        );
        assert_eq!(
            details.get("outcome").and_then(serde_json::Value::as_str),
            Some("success")
        );
        assert_eq!(
            details
                .get("threshold_ms")
                .and_then(serde_json::Value::as_u64),
            Some(1000)
        );
    }

    #[test]
    fn database_backup_event_serializes_with_expected_shape() {
        let (sink, mut rx) = AuditSink::channel_for_testing();
        sink.database_backup(
            7,
            "admin",
            "mydb",
            BackupOperation::Restore,
            AuditOutcome::Success,
        );
        let event = rx.try_recv().expect("event must be queued");
        let json = serde_json::to_value(&event).expect("serialize");
        assert_eq!(
            json.get("event_type").and_then(serde_json::Value::as_str),
            Some("database_backup")
        );
        let details = json.get("details").expect("details object");
        assert_eq!(
            details.get("name").and_then(serde_json::Value::as_str),
            Some("mydb")
        );
        assert_eq!(
            details.get("operation").and_then(serde_json::Value::as_str),
            Some("restore")
        );
        assert_eq!(
            details.get("outcome").and_then(serde_json::Value::as_str),
            Some("success")
        );
    }

    #[test]
    fn database_backup_failed_carries_reason() {
        let (sink, mut rx) = AuditSink::channel_for_testing();
        sink.database_backup(
            7,
            "admin",
            "mydb",
            BackupOperation::Snapshot,
            AuditOutcome::Failed {
                reason: "boom".to_owned(),
            },
        );
        let event = rx.try_recv().expect("event must be queued");
        let json = serde_json::to_value(&event).expect("serialize");
        let details = json.get("details").expect("details object");
        assert_eq!(
            details.get("operation").and_then(serde_json::Value::as_str),
            Some("snapshot")
        );
        assert_eq!(
            details.get("outcome").and_then(serde_json::Value::as_str),
            Some("failed")
        );
        assert_eq!(
            details.get("reason").and_then(serde_json::Value::as_str),
            Some("boom")
        );
    }

    #[test]
    fn result_capped_event_serializes_with_expected_shape() {
        let (sink, mut rx) = AuditSink::channel_for_testing();
        sink.result_capped(
            42,
            Some("alice"),
            "abc123000000000000000000000000000000000000000000000000000000abc1",
            99,
            3,
            Some("neo4j"),
        );
        let event = rx.try_recv().expect("event must be queued");
        let json = serde_json::to_value(&event).expect("serialize");
        assert_eq!(
            json.get("event_type").and_then(serde_json::Value::as_str),
            Some("result_capped")
        );
        assert_eq!(
            json.get("connection_id")
                .and_then(serde_json::Value::as_u64),
            Some(42)
        );
        assert_eq!(
            json.get("user").and_then(serde_json::Value::as_str),
            Some("alice")
        );
        let details = json.get("details").expect("details object");
        assert_eq!(
            details
                .get("statement_sha256")
                .and_then(serde_json::Value::as_str),
            Some("abc123000000000000000000000000000000000000000000000000000000abc1")
        );
        assert_eq!(
            details
                .get("row_count_seen")
                .and_then(serde_json::Value::as_u64),
            Some(99)
        );
        assert_eq!(
            details.get("cap").and_then(serde_json::Value::as_u64),
            Some(3)
        );
        assert_eq!(
            details.get("database").and_then(serde_json::Value::as_str),
            Some("neo4j")
        );
    }

    #[test]
    fn result_capped_event_omits_database_when_none() {
        let (sink, mut rx) = AuditSink::channel_for_testing();
        sink.result_capped(7, None, "deadbeef", 1000, 500, None);
        let event = rx.try_recv().expect("event must be queued");
        let json = serde_json::to_value(&event).expect("serialize");
        let details = json.get("details").expect("details object");
        assert!(
            details.get("database").is_none(),
            "None database must be skipped on the wire, got {details:?}"
        );
        assert!(
            json.get("user").is_none() || json.get("user") == Some(&serde_json::Value::Null),
            "None user must serialize as absent/null"
        );
    }

    /// Collect every `event_type` currently queued in the test channel.
    fn drain_event_types(rx: &mut tokio::sync::mpsc::UnboundedReceiver<AuditEvent>) -> Vec<String> {
        let mut out = Vec::new();
        while let Ok(event) = rx.try_recv() {
            let json = serde_json::to_value(&event).expect("serialize");
            out.push(
                json.get("event_type")
                    .and_then(serde_json::Value::as_str)
                    .expect("event_type")
                    .to_owned(),
            );
        }
        out
    }

    #[test]
    fn emit_query_pair_emits_slow_query_when_duration_crosses_threshold() {
        let (sink, mut rx) = AuditSink::channel_for_testing();
        let mut gate = SlowQueryGate::new(0); // cap=0 -> pass-through
        // duration 1500 >= threshold 1000 -> both events.
        sink.emit_query_pair(
            &mut gate,
            1000,
            std::time::Instant::now(),
            7,
            "admin",
            "deadbeef",
            Some("slow_db"),
            1500,
            3,
            QueryOutcome::Success,
        );
        assert_eq!(
            drain_event_types(&mut rx),
            vec!["query_exec", "slow_query"],
            "duration over threshold with cap=0 must emit both the regular and the slow-query event in order"
        );
    }

    #[test]
    fn emit_query_pair_threshold_zero_is_a_kill_switch() {
        let (sink, mut rx) = AuditSink::channel_for_testing();
        let mut gate = SlowQueryGate::new(0);
        // threshold 0 must suppress slow_query even for a huge duration.
        sink.emit_query_pair(
            &mut gate,
            0,
            std::time::Instant::now(),
            7,
            "admin",
            "deadbeef",
            Some("slow_db"),
            u64::MAX,
            0,
            QueryOutcome::Success,
        );
        assert_eq!(
            drain_event_types(&mut rx),
            vec!["query_exec"],
            "threshold=0 must suppress the slow-query event even for the maximum possible duration"
        );
    }

    #[test]
    fn emit_query_pair_below_threshold_emits_only_query_exec() {
        let (sink, mut rx) = AuditSink::channel_for_testing();
        let mut gate = SlowQueryGate::new(0);
        // duration 999 < threshold 1000 -> only the regular event.
        sink.emit_query_pair(
            &mut gate,
            1000,
            std::time::Instant::now(),
            7,
            "admin",
            "deadbeef",
            Some("slow_db"),
            999,
            0,
            QueryOutcome::Success,
        );
        assert_eq!(
            drain_event_types(&mut rx),
            vec!["query_exec"],
            "duration below threshold must emit only the regular query_exec event"
        );
    }

    #[test]
    fn emit_query_pair_propagates_error_outcome_to_slow_query() {
        let (sink, mut rx) = AuditSink::channel_for_testing();
        let mut gate = SlowQueryGate::new(0);
        sink.emit_query_pair(
            &mut gate,
            10,
            std::time::Instant::now(),
            7,
            "admin",
            "deadbeef",
            Some("slow_db"),
            42,
            0,
            QueryOutcome::Error {
                error_code: "Neo.ClientError.Statement.SyntaxError".to_owned(),
            },
        );
        // First the query_exec, then the slow_query - both carry error.
        let regular = rx.try_recv().expect("query_exec queued");
        let slow = rx.try_recv().expect("slow_query queued");
        for event in [&regular, &slow] {
            let json = serde_json::to_value(event).expect("serialize");
            assert_eq!(
                json.get("details")
                    .and_then(|d| d.get("outcome"))
                    .and_then(serde_json::Value::as_str),
                Some("error"),
                "both events must carry outcome=error"
            );
        }
        let slow_json = serde_json::to_value(&slow).expect("serialize");
        assert_eq!(
            slow_json
                .get("event_type")
                .and_then(serde_json::Value::as_str),
            Some("slow_query"),
            "the second event of the pair must be the slow_query line"
        );
    }

    #[test]
    fn emit_query_pair_respects_gate_cap() {
        let (sink, mut rx) = AuditSink::channel_for_testing();
        let mut gate = SlowQueryGate::new(2); // cap=2 within the window
        let now = std::time::Instant::now();
        // 3 slow statements in the same window: only 2 slow_query lines.
        for _ in 0..3 {
            sink.emit_query_pair(
                &mut gate,
                10,
                now,
                7,
                "admin",
                "deadbeef",
                Some("slow_db"),
                42,
                0,
                QueryOutcome::Success,
            );
        }
        let types = drain_event_types(&mut rx);
        let regular = types.iter().filter(|t| *t == "query_exec").count();
        let slow = types.iter().filter(|t| *t == "slow_query").count();
        assert_eq!(regular, 3, "every RUN emits a query_exec");
        assert_eq!(slow, 2, "the gate caps slow_query at 2 per window");
    }

    /// The `custom` seam forwards every emitted event to the plugged-in
    /// backend. This is the Enterprise extension point: a compliance backend
    /// implements `AuditBackend`, is wrapped with `AuditSink::custom`, and
    /// then receives events through the unchanged convenience methods
    /// (`connection_open`, etc.).
    #[test]
    fn custom_backend_receives_emitted_events() {
        use std::net::{IpAddr, Ipv4Addr, SocketAddr};
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct CountingBackend {
            count: Arc<AtomicUsize>,
        }
        impl AuditBackend for CountingBackend {
            fn emit(&self, _event: &AuditEvent) {
                self.count.fetch_add(1, Ordering::Relaxed);
            }
        }

        let count = Arc::new(AtomicUsize::new(0));
        let sink = AuditSink::custom(Arc::new(CountingBackend {
            count: Arc::clone(&count),
        }));

        let peer = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 7687);
        sink.connection_open(1, peer, true);
        sink.auth_failure(1, "alice", AuthFailureReason::UnknownUser);

        assert_eq!(
            count.load(Ordering::Relaxed),
            2,
            "custom backend must receive every event routed through the sink"
        );
    }
}
