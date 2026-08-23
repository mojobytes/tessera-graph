// SPDX-License-Identifier: BSL-1.1

//! Integration tests for the Prometheus metrics endpoint.
//!
//! All tests gate on `plain-tcp` because the Bolt-side fixtures in
//! sibling tests (`startup_test.rs`, `listener_test.rs`) use the same
//! gate. The metrics endpoint itself is plain HTTP regardless.
//!
//! Cycle layout (see `docs/plans/2026-05-18-observability-task-1-metrics.md`):
//! - C2: endpoint serves 200 + Prometheus text format.
//! - C3: `ermya_active_connections` gauge — delta-assertion E2E.
//! - C4+: counters/gauges/histograms (added by later cycles).
//!
//! The `metrics` recorder is a process-global singleton (see
//! `metrics.rs::install_or_get_recorder`). Tests therefore use
//! **delta-based assertions** (`snapshot` before, `scrape_value` after,
//! compare the diff) so they are order-independent and tolerate state
//! left over from sibling tests in the same binary.

#![cfg(feature = "plain-tcp")]

#[cfg(unix)]
#[path = "common/mod.rs"]
mod common;

use std::net::SocketAddr;
use std::time::Duration;

use ermya_graph_protocol::bolt_frame::{BoltChunkedReader, BoltChunkedWriter};
use ermya_graph_protocol::bolt_message::{BoltRequest, BoltResponse};
use ermya_graph_protocol::{BOLT_MAGIC, PackStreamValue, decode_response, encode_request};
use ermya_graph_server::{ServerConfig, ServerHandle};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{Mutex, oneshot, watch};

static BOLT_METRICS_TEST_LOCK: Mutex<()> = Mutex::const_new(());

// ── Cycle 2: exporter endpoint ──────────────────────────────────────────────

#[tokio::test]
async fn metrics_endpoint_returns_200_with_prometheus_content_type() {
    let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    let bound_addr = ermya_graph_server::metrics::spawn_metrics_server(addr, shutdown_rx)
        .await
        .expect("metrics server bind");

    let resp = reqwest::get(format!("http://{bound_addr}/metrics"))
        .await
        .expect("GET /metrics");
    assert_eq!(resp.status(), 200);
    let ct = resp
        .headers()
        .get("content-type")
        .expect("content-type")
        .to_str()
        .unwrap()
        .to_owned();
    assert!(
        ct.contains("text/plain"),
        "expected text/plain content-type, got: {ct}",
    );

    let _ = shutdown_tx.send(true);
}

#[tokio::test]
async fn metrics_endpoint_returns_success_on_cold_start() {
    // Without emitting any metric, /metrics must still respond 200.
    // The body may be empty or contain only HELP/TYPE comments — the
    // test asserts no panic and no 5xx.
    let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    let bound_addr = ermya_graph_server::metrics::spawn_metrics_server(addr, shutdown_rx)
        .await
        .expect("metrics server bind");

    let resp = reqwest::get(format!("http://{bound_addr}/metrics"))
        .await
        .expect("GET /metrics");
    assert!(resp.status().is_success(), "got status {}", resp.status());

    let _ = shutdown_tx.send(true);
}

#[tokio::test]
async fn metrics_endpoint_returns_404_for_unknown_path() {
    // Plan decision 2: anything other than GET /metrics returns 404.
    let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    let bound_addr = ermya_graph_server::metrics::spawn_metrics_server(addr, shutdown_rx)
        .await
        .expect("metrics server bind");

    let resp = reqwest::get(format!("http://{bound_addr}/healthz"))
        .await
        .expect("GET /healthz");
    assert_eq!(resp.status(), 404);

    let _ = shutdown_tx.send(true);
}

// ── Cycle 8: graceful shutdown of the exporter ──────────────────────────────

#[tokio::test]
async fn metrics_server_stops_accepting_on_shutdown() {
    // Before shutdown, GET /metrics succeeds. After flipping the watch
    // channel to `true`, the accept_loop in `metrics.rs` exits and the
    // listener is dropped — subsequent connection attempts must fail.
    //
    // We poll instead of relying on a fixed sleep because the listener
    // drop is asynchronous (one yield from the `tokio::select!` to the
    // scope exit). The shutdown path is `biased` on the channel branch,
    // so convergence is sub-50 ms in practice; a 2 s budget is generous
    // for CI under load.
    let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    let bound_addr = ermya_graph_server::metrics::spawn_metrics_server(addr, shutdown_rx)
        .await
        .expect("metrics server bind");

    // Sanity: endpoint serves before shutdown.
    let r1 = reqwest::get(format!("http://{bound_addr}/metrics"))
        .await
        .expect("GET /metrics pre-shutdown");
    assert!(r1.status().is_success(), "expected 2xx pre-shutdown");

    // Signal shutdown.
    shutdown_tx.send(true).expect("shutdown_tx send");

    // After shutdown, the listener is gone. reqwest may return a
    // ConnectionRefused / connect timeout / dropped-stream error — we
    // accept any `Err` outcome, which is the contract the plan documents.
    let stopped = poll_until(
        Duration::from_secs(2),
        Duration::from_millis(20),
        || async {
            // Use a short per-attempt timeout so a stray TIME_WAIT'd
            // socket cannot hang the poll forever.
            let client = reqwest::Client::builder()
                .timeout(Duration::from_millis(250))
                .build()
                .expect("reqwest client");
            client
                .get(format!("http://{bound_addr}/metrics"))
                .send()
                .await
                .is_err()
        },
    )
    .await;
    assert!(
        stopped,
        "metrics endpoint still reachable 2s after shutdown",
    );
}

// ── Cycle 3: active_connections gauge ───────────────────────────────────────

/// Spawn a no-auth, in-memory server on ephemeral Bolt + metrics ports.
///
/// Returns `(bolt_addr, metrics_addr, shutdown_tx, server_join)`. The
/// caller is responsible for sending `true` on `shutdown_tx` and
/// awaiting `server_join` to release the registry sweeper.
///
/// Helpers live in this file (not `tests/common/mod.rs`) because that
/// module wraps `BoltHandler` on a `DuplexStream`, while these tests
/// need un arranque real con TCP enlazado + medidas
/// endpoint. Sharing would couple two unrelated test surfaces.
async fn spawn_test_server() -> (
    SocketAddr,
    SocketAddr,
    watch::Sender<bool>,
    tokio::task::JoinHandle<ermya_graph_server::Result<ServerHandle>>,
) {
    let cfg = ServerConfig {
        bind_addr: "127.0.0.1:0".to_owned(),
        metrics_addr: Some("127.0.0.1:0".to_owned()),
        no_auth: true,
        // In-memory: no data_dir means no system-graph flock, no
        // audit file, no migration marker. Fast and isolated per test.
        data_dir: None,
        ..Default::default()
    };

    let (ready_tx, ready_rx) = oneshot::channel();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let server_join = tokio::spawn(async move {
        ermya_graph_server::start_server_with_registry(
            cfg,
            shutdown_rx,
            Some(ready_tx),
            ermya_graph_server::single_database_factory(),
            ermya_graph_server::startup::PaidStartupHooks::default(),
        )
        .await
    });

    let ready = tokio::time::timeout(Duration::from_secs(5), ready_rx)
        .await
        .expect("ready channel timed out")
        .expect("ready sender dropped");
    let metrics_addr = ready
        .metrics_addr
        .expect("metrics endpoint must bind when ServerConfig.metrics_addr is Some");

    (ready.bolt_addr, metrics_addr, shutdown_tx, server_join)
}

/// Scrape `/metrics` and return the value of `metric_name{labels…}`.
///
/// Returns `0.0` when the series does not appear in the exporter output
/// (counter never incremented, gauge never set). The Prometheus text
/// format is line-oriented; we match by exact prefix `name{labels…}` or
/// bare `name` when `labels` is empty. Comment lines (`# HELP` /
/// `# TYPE`) are skipped naturally because they do not start with the
/// metric name without a leading `#`.
async fn scrape_value(
    metrics_addr: &SocketAddr,
    metric_name: &str,
    labels: &[(&str, &str)],
) -> f64 {
    let body = reqwest::get(format!("http://{metrics_addr}/metrics"))
        .await
        .expect("GET /metrics")
        .text()
        .await
        .expect("metrics body");

    let prefix = if labels.is_empty() {
        format!("{metric_name} ")
    } else {
        // Prometheus emits labels as `name{k1="v1",k2="v2"} value`.
        // We construct the expected substring; the recorder may order
        // labels differently across versions, so we match by checking
        // every label appears between `{` and `}` rather than by exact
        // string equality on the whole bracket payload. For tests in
        // C3–C9 this is enough because the metric names are unique per
        // assertion.
        format!("{metric_name}{{")
    };

    for line in body.lines() {
        if !line.starts_with(&prefix) {
            continue;
        }
        if !labels.is_empty() {
            // Verify every (k, v) pair is present in the bracketed
            // label set. `name{a="1",b="2"} 3.14` ⇒ check `a="1"` and
            // `b="2"` are substrings of the segment between `{` and `}`.
            let Some(open) = line.find('{') else { continue };
            let Some(close) = line.find('}') else {
                continue;
            };
            let label_seg = &line[open + 1..close];
            if !labels
                .iter()
                .all(|(k, v)| label_seg.contains(&format!("{k}=\"{v}\"")))
            {
                continue;
            }
        }
        // Trailing token is the value.
        let Some(value_tok) = line.split_whitespace().nth(1) else {
            continue;
        };
        return value_tok.parse().unwrap_or(0.0);
    }
    0.0
}

/// Semantic alias for the "before" sample of a delta assertion.
async fn snapshot(metrics_addr: &SocketAddr, name: &str, labels: &[(&str, &str)]) -> f64 {
    scrape_value(metrics_addr, name, labels).await
}

/// Open a raw TCP connection to the Bolt port and write the 20-byte
/// Bolt 4.4 handshake.
///
/// The gauge `ermya_active_connections` is incremented in
/// `listener.rs::serve_with` immediately when the accept loop spawns
/// the per-connection task — **before** the handshake completes — so a
/// bare TCP connect already counts. We still send the handshake bytes
/// so the server-side `BoltHandler::new_with_handshake` future makes
/// forward progress instead of stalling on `read_exact`, which keeps
/// the connection "live" for the during-snapshot.
async fn open_raw_bolt_connection(addr: SocketAddr) -> TcpStream {
    let mut stream = TcpStream::connect(addr)
        .await
        .expect("connect to bolt port");
    let mut handshake = [0u8; 20];
    handshake[..4].copy_from_slice(&BOLT_MAGIC);
    handshake[4..8].copy_from_slice(&0x0004_0404_u32.to_be_bytes());
    stream
        .write_all(&handshake)
        .await
        .expect("write bolt handshake");
    stream.flush().await.expect("flush bolt handshake");
    let mut selected_version = [0u8; 4];
    stream
        .read_exact(&mut selected_version)
        .await
        .expect("read bolt handshake response");
    assert_eq!(selected_version, [0x00, 0x00, 0x04, 0x04]);
    stream
}

/// Wait for `predicate` to hold or `timeout` to elapse, polling every
/// `step`. The metric pipeline is eventually-consistent at the
/// 10–50 ms scale (task spawn → atomic `fetch_add` → render). Plain
/// `sleep(N)` makes tests flaky on loaded CI; this helper retries until
/// the assertion converges.
async fn poll_until<F, Fut>(timeout: Duration, step: Duration, mut predicate: F) -> bool
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if predicate().await {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(step).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn active_connections_gauge_increments_on_connect_and_decrements_on_close() {
    let _serial = BOLT_METRICS_TEST_LOCK.lock().await;
    let (bolt_addr, metrics_addr, shutdown_tx, server_join) = spawn_test_server().await;

    // Snapshot before opening any connection. Delta-based so the
    // recorder state from sibling tests (C2 endpoint tests, other C3+
    // cycles) does not pollute the assertion.
    let before = snapshot(&metrics_addr, "ermya_active_connections", &[]).await;

    // Open + hold a connection. The server-side gauge increments inside
    // the task spawned by `listener::serve_with` — that spawn happens
    // immediately after `accept()` returns, but is not synchronous with
    // the client-side `connect()` completing, so we poll until visible.
    let mut stream = open_raw_bolt_connection(bolt_addr).await;

    let saw_increment = poll_until(Duration::from_secs(2), Duration::from_millis(20), || {
        // `metrics_addr: SocketAddr` is `Copy`, so the `async move`
        // future captures it by copy on every call — `FnMut`-safe.
        async move {
            let now = scrape_value(&metrics_addr, "ermya_active_connections", &[]).await;
            now > before
        }
    })
    .await;
    let during = scrape_value(&metrics_addr, "ermya_active_connections", &[]).await;
    assert!(
        saw_increment,
        "gauge did not increment within 2 s (before={before}, during={during})",
    );

    // Close the client side explicitly. Sending FIN makes the EOF
    // observable by the server even when the runner delays destruction
    // of the local socket under load.
    stream.shutdown().await.expect("shutdown bolt connection");
    drop(stream);

    let saw_decrement = poll_until(
        Duration::from_secs(2),
        Duration::from_millis(20),
        || async move {
            let now = scrape_value(&metrics_addr, "ermya_active_connections", &[]).await;
            now <= before
        },
    )
    .await;
    let after = scrape_value(&metrics_addr, "ermya_active_connections", &[]).await;
    assert!(
        saw_decrement,
        "gauge did not return to baseline within 2 s (before={before}, after={after})",
    );

    let _ = shutdown_tx.send(true);
    let _ = tokio::time::timeout(Duration::from_secs(5), server_join).await;
}

// ── Cycle 4: bolt_messages_total counter ────────────────────────────────────

/// Spawn an auth-enabled server with a single pre-created database
/// (`db_name`, owned by `admin`). Returns the same tuple as
/// [`spawn_test_server`] plus a `TempDir` whose `Drop` cleans the
/// on-disk state — the caller MUST keep it in scope.
///
/// Sólo Unix: `common::prepopulate_system` impone permisos 0o700 en el
/// directorio de sistema.
///
/// El nombre de base se recibe y no se usa: esta edición sirve una sola, con
/// nombre fijo. Se conserva en la firma para que las llamadas digan a qué base
/// va cada prueba, que es lo que se lee al diagnosticar un fallo.
#[cfg(unix)]
async fn spawn_test_server_with_db(
    _db_name: &str,
) -> (
    SocketAddr,
    SocketAddr,
    watch::Sender<bool>,
    tokio::task::JoinHandle<ermya_graph_server::Result<ServerHandle>>,
    tempfile::TempDir,
) {
    let tmp = tempfile::tempdir().expect("create test tempdir");
    let data_dir = tmp.path().to_path_buf();

    // Bootstrap admin + database before the server opens
    // the system graph flock. `prepopulate_system` drops its store
    // before returning, releasing the lock for the server.
    common::prepopulate_system(&data_dir, "admin-pw-12chars").await;

    let cfg = ServerConfig {
        bind_addr: "127.0.0.1:0".to_owned(),
        metrics_addr: Some("127.0.0.1:0".to_owned()),
        data_dir: Some(data_dir.clone()),
        password: Some("admin-pw-12chars".to_owned()),
        ..Default::default()
    };

    let (ready_tx, ready_rx) = oneshot::channel();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let server_join = tokio::spawn(async move {
        ermya_graph_server::start_server_with_registry(
            cfg,
            shutdown_rx,
            Some(ready_tx),
            ermya_graph_server::single_database_factory(),
            ermya_graph_server::startup::PaidStartupHooks::default(),
        )
        .await
    });

    let ready = tokio::time::timeout(Duration::from_secs(5), ready_rx)
        .await
        .expect("ready channel timed out")
        .expect("ready sender dropped");
    let metrics_addr = ready
        .metrics_addr
        .expect("metrics endpoint must bind when ServerConfig.metrics_addr is Some");

    (ready.bolt_addr, metrics_addr, shutdown_tx, server_join, tmp)
}

/// Establish a Bolt 4.4 session, send `request`, return the first reply.
///
/// Used by tests that drive HELLO end-to-end against `no_auth=true`
/// servers (credentials are accepted but ignored).
async fn open_bolt_session(
    addr: SocketAddr,
) -> (
    BoltChunkedWriter<tokio::io::WriteHalf<TcpStream>>,
    BoltChunkedReader<tokio::io::ReadHalf<TcpStream>>,
) {
    let stream = TcpStream::connect(addr)
        .await
        .expect("connect to bolt port");
    let (mut read, mut write) = tokio::io::split(stream);
    let mut hs = [0u8; 20];
    hs[..4].copy_from_slice(&BOLT_MAGIC);
    hs[4..8].copy_from_slice(&0x0004_0404_u32.to_be_bytes());
    write.write_all(&hs).await.expect("write handshake");
    write.flush().await.expect("flush handshake");
    let mut ver = [0u8; 4];
    read.read_exact(&mut ver).await.expect("read version");
    assert_eq!(ver, [0x00, 0x00, 0x04, 0x04], "bolt version mismatch");
    (BoltChunkedWriter::new(write), BoltChunkedReader::new(read))
}

async fn bolt_send_recv<W, R>(
    cw: &mut BoltChunkedWriter<W>,
    cr: &mut BoltChunkedReader<R>,
    request: &BoltRequest,
) -> BoltResponse
where
    W: tokio::io::AsyncWrite + Unpin,
    R: tokio::io::AsyncRead + Unpin,
{
    cw.write_message(&encode_request(request).expect("encode request"))
        .await
        .expect("write message");
    let data = cr
        .read_message()
        .await
        .expect("read reply")
        .expect("reply message");
    decode_response(&data).expect("decode reply")
}

/// HELLO with admin credentials over a `no_auth=true` server. The
/// server accepts the HELLO regardless of credential content because
/// `NoAuthProvider` short-circuits the check; the test only needs the
/// HELLO/success counter to bump.
async fn send_bolt_hello_no_auth(addr: SocketAddr) {
    let (mut cw, mut cr) = open_bolt_session(addr).await;
    let hello = BoltRequest::Hello {
        extra: vec![
            (
                "principal".to_owned(),
                PackStreamValue::String("anyone".to_owned()),
            ),
            (
                "credentials".to_owned(),
                PackStreamValue::String("ignored".to_owned()),
            ),
        ],
    };
    let resp = bolt_send_recv(&mut cw, &mut cr, &hello).await;
    assert!(
        matches!(resp, BoltResponse::Success { .. }),
        "expected SUCCESS for HELLO under no_auth, got {resp:?}"
    );
}

/// HELLO admin + RUN `extra["db"]=db` + PULL drained to trailing SUCCESS.
///
/// Used by C4/C6 tests that need to bump both `bolt_messages_total`
/// (RUN+PULL) and the query counters. Caller-supplied `db` must exist
/// in the catalogue with admin grants — see `spawn_test_server_with_db`.
async fn send_bolt_run_and_pull(addr: SocketAddr, db: &str, query: &str) {
    let (mut cw, mut cr) = open_bolt_session(addr).await;
    let hello = BoltRequest::Hello {
        extra: vec![
            (
                "principal".to_owned(),
                PackStreamValue::String("admin".to_owned()),
            ),
            (
                "credentials".to_owned(),
                PackStreamValue::String("admin-pw-12chars".to_owned()),
            ),
        ],
    };
    let hello_resp = bolt_send_recv(&mut cw, &mut cr, &hello).await;
    assert!(
        matches!(hello_resp, BoltResponse::Success { .. }),
        "HELLO must succeed before RUN, got {hello_resp:?}"
    );

    let run = BoltRequest::Run {
        query: query.to_owned(),
        params: vec![],
        extra: vec![("db".to_owned(), PackStreamValue::String(db.to_owned()))],
    };
    let run_resp = bolt_send_recv(&mut cw, &mut cr, &run).await;
    assert!(
        matches!(run_resp, BoltResponse::Success { .. }),
        "RUN must succeed, got {run_resp:?}"
    );

    let pull_resp = bolt_send_recv(&mut cw, &mut cr, &BoltRequest::Pull { extra: vec![] }).await;
    // Drain RECORD(s) until the trailing SUCCESS.
    let mut last = pull_resp;
    while matches!(last, BoltResponse::Record { .. }) {
        let data = cr
            .read_message()
            .await
            .expect("read record")
            .expect("record message");
        last = decode_response(&data).expect("decode record");
    }
    assert!(
        matches!(last, BoltResponse::Success { .. }),
        "PULL must terminate with SUCCESS, got {last:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bolt_messages_counter_increments_on_hello_success() {
    let _serial = BOLT_METRICS_TEST_LOCK.lock().await;
    let (bolt_addr, metrics_addr, shutdown_tx, server_join) = spawn_test_server().await;

    let before = snapshot(
        &metrics_addr,
        "ermya_bolt_messages_total",
        &[("type", "HELLO"), ("outcome", "success")],
    )
    .await;

    send_bolt_hello_no_auth(bolt_addr).await;

    let saw_bump = poll_until(
        Duration::from_secs(2),
        Duration::from_millis(20),
        || async move {
            let now = scrape_value(
                &metrics_addr,
                "ermya_bolt_messages_total",
                &[("type", "HELLO"), ("outcome", "success")],
            )
            .await;
            (now - before) >= 1.0
        },
    )
    .await;
    let after = scrape_value(
        &metrics_addr,
        "ermya_bolt_messages_total",
        &[("type", "HELLO"), ("outcome", "success")],
    )
    .await;
    assert!(
        saw_bump,
        "HELLO counter did not increment (before={before}, after={after})"
    );

    let _ = shutdown_tx.send(true);
    let _ = tokio::time::timeout(Duration::from_secs(5), server_join).await;
}

/// HELLO with explicit credentials, return the wire reply. Used by
/// C5 `auth_attempts` tests where the outcome (success vs failed) is
/// what drives the counter.
async fn send_bolt_hello_with_creds(addr: SocketAddr, user: &str, pass: &str) -> BoltResponse {
    let (mut cw, mut cr) = open_bolt_session(addr).await;
    let hello = BoltRequest::Hello {
        extra: vec![
            (
                "principal".to_owned(),
                PackStreamValue::String(user.to_owned()),
            ),
            (
                "credentials".to_owned(),
                PackStreamValue::String(pass.to_owned()),
            ),
        ],
    };
    bolt_send_recv(&mut cw, &mut cr, &hello).await
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auth_attempts_counter_increments_on_success_and_failure() {
    let _serial = BOLT_METRICS_TEST_LOCK.lock().await;
    let (bolt_addr, metrics_addr, shutdown_tx, server_join, _tmp) =
        spawn_test_server_with_db("authdb").await;

    let before_ok = snapshot(
        &metrics_addr,
        "ermya_auth_attempts_total",
        &[("outcome", "success")],
    )
    .await;
    let before_fail = snapshot(
        &metrics_addr,
        "ermya_auth_attempts_total",
        &[("outcome", "failed")],
    )
    .await;

    // Successful HELLO with the bootstrapped admin credentials.
    let ok = send_bolt_hello_with_creds(bolt_addr, "admin", "admin-pw-12chars").await;
    assert!(
        matches!(ok, BoltResponse::Success { .. }),
        "expected SUCCESS for HELLO admin, got {ok:?}"
    );

    // Failed HELLO with wrong password.
    let fail = send_bolt_hello_with_creds(bolt_addr, "admin", "definitely-not-the-pw").await;
    assert!(
        matches!(fail, BoltResponse::Failure { .. }),
        "expected FAILURE for HELLO admin/wrong-pw, got {fail:?}"
    );

    let saw_ok = poll_until(
        Duration::from_secs(2),
        Duration::from_millis(20),
        || async move {
            let now = scrape_value(
                &metrics_addr,
                "ermya_auth_attempts_total",
                &[("outcome", "success")],
            )
            .await;
            (now - before_ok) >= 1.0
        },
    )
    .await;
    let saw_fail = poll_until(
        Duration::from_secs(2),
        Duration::from_millis(20),
        || async move {
            let now = scrape_value(
                &metrics_addr,
                "ermya_auth_attempts_total",
                &[("outcome", "failed")],
            )
            .await;
            (now - before_fail) >= 1.0
        },
    )
    .await;
    assert!(saw_ok, "success counter did not increment");
    assert!(saw_fail, "failed counter did not increment");

    let _ = shutdown_tx.send(true);
    let _ = tokio::time::timeout(Duration::from_secs(5), server_join).await;
}

/// HELLO admin + RUN `extra["db"]=db` (no PULL). Returns the RUN
/// reply. Used by C6 tests that exercise the RUN error path — the
/// query must already be rejected before PULL is sent.
async fn send_bolt_run_no_pull(addr: SocketAddr, db: &str, query: &str) -> BoltResponse {
    let (mut cw, mut cr) = open_bolt_session(addr).await;
    let hello = BoltRequest::Hello {
        extra: vec![
            (
                "principal".to_owned(),
                PackStreamValue::String("admin".to_owned()),
            ),
            (
                "credentials".to_owned(),
                PackStreamValue::String("admin-pw-12chars".to_owned()),
            ),
        ],
    };
    let hello_resp = bolt_send_recv(&mut cw, &mut cr, &hello).await;
    assert!(
        matches!(hello_resp, BoltResponse::Success { .. }),
        "HELLO must succeed, got {hello_resp:?}"
    );
    let run = BoltRequest::Run {
        query: query.to_owned(),
        params: vec![],
        extra: vec![("db".to_owned(), PackStreamValue::String(db.to_owned()))],
    };
    bolt_send_recv(&mut cw, &mut cr, &run).await
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queries_counter_and_histogram_on_successful_run() {
    let _serial = BOLT_METRICS_TEST_LOCK.lock().await;
    let (bolt_addr, metrics_addr, shutdown_tx, server_join, _tmp) =
        spawn_test_server_with_db("qdb").await;

    // Las medidas se etiquetan con la base que el servidor sirvió. Esta edición
    // sirve siempre la misma, con nombre fijo, y el nombre que la consulta pide
    // se ignora al resolver — de ahí que la etiqueta no sea "qdb".

    let before_total = snapshot(
        &metrics_addr,
        "ermya_queries_total",
        &[
            ("database", ermya_graph_server::registry::COMMUNITY_DATABASE),
            ("outcome", "success"),
        ],
    )
    .await;
    let before_count = snapshot(
        &metrics_addr,
        "ermya_query_duration_seconds_count",
        &[
            ("database", ermya_graph_server::registry::COMMUNITY_DATABASE),
            ("kind", "query"),
        ],
    )
    .await;
    let before_sum = snapshot(
        &metrics_addr,
        "ermya_query_duration_seconds_sum",
        &[
            ("database", ermya_graph_server::registry::COMMUNITY_DATABASE),
            ("kind", "query"),
        ],
    )
    .await;

    send_bolt_run_and_pull(bolt_addr, "qdb", "MATCH (n) RETURN count(n) AS c").await;

    let saw_total = poll_until(
        Duration::from_secs(2),
        Duration::from_millis(20),
        || async move {
            let now = scrape_value(
                &metrics_addr,
                "ermya_queries_total",
                &[
                    ("database", ermya_graph_server::registry::COMMUNITY_DATABASE),
                    ("outcome", "success"),
                ],
            )
            .await;
            (now - before_total) >= 1.0
        },
    )
    .await;
    let saw_count = poll_until(
        Duration::from_secs(2),
        Duration::from_millis(20),
        || async move {
            let now = scrape_value(
                &metrics_addr,
                "ermya_query_duration_seconds_count",
                &[
                    ("database", ermya_graph_server::registry::COMMUNITY_DATABASE),
                    ("kind", "query"),
                ],
            )
            .await;
            (now - before_count) >= 1.0
        },
    )
    .await;
    assert!(saw_total, "queries_total{{outcome=success}} did not bump");
    assert!(saw_count, "histogram count for kind=query did not bump");

    // The histogram sum is monotonic non-decreasing. We do not assert
    // a strict bump because a sub-microsecond execution could round to
    // 0.0 on some platforms; `count` already proves an observation was
    // recorded.
    let after_sum = scrape_value(
        &metrics_addr,
        "ermya_query_duration_seconds_sum",
        &[
            ("database", ermya_graph_server::registry::COMMUNITY_DATABASE),
            ("kind", "query"),
        ],
    )
    .await;
    assert!(
        after_sum >= before_sum,
        "histogram sum must be monotonic (before={before_sum}, after={after_sum})"
    );

    let _ = shutdown_tx.send(true);
    let _ = tokio::time::timeout(Duration::from_secs(5), server_join).await;
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queries_counter_outcome_error_on_syntax_failure() {
    let _serial = BOLT_METRICS_TEST_LOCK.lock().await;
    let (bolt_addr, metrics_addr, shutdown_tx, server_join, _tmp) =
        spawn_test_server_with_db("edb").await;

    let before = snapshot(
        &metrics_addr,
        "ermya_queries_total",
        &[
            ("database", ermya_graph_server::registry::COMMUNITY_DATABASE),
            ("outcome", "error"),
        ],
    )
    .await;

    // Syntax error path: the dispatcher emits the counter from the
    // SyntaxError branch (with the database already bound by
    // try_bind_database before the parser runs).
    let resp = send_bolt_run_no_pull(bolt_addr, "edb", "THIS IS NOT GQL").await;
    assert!(
        matches!(resp, BoltResponse::Failure { .. }),
        "syntax error must FAIL, got {resp:?}"
    );

    let saw = poll_until(
        Duration::from_secs(2),
        Duration::from_millis(20),
        || async move {
            let now = scrape_value(
                &metrics_addr,
                "ermya_queries_total",
                &[
                    ("database", ermya_graph_server::registry::COMMUNITY_DATABASE),
                    ("outcome", "error"),
                ],
            )
            .await;
            (now - before) >= 1.0
        },
    )
    .await;
    assert!(
        saw,
        "queries_total{{outcome=error}} did not bump on syntax error"
    );

    let _ = shutdown_tx.send(true);
    let _ = tokio::time::timeout(Duration::from_secs(5), server_join).await;
}

#[test]
fn label_guard_caps_database_label_to_other() {
    // Pure unit test (no server) — exercises the cap directly via
    // the constructor. The 257th distinct name must collapse to
    // `"_other"` so the Prometheus registry never grows past
    // `METRICS_DATABASE_LABEL_CAP` series for the `database` label.
    let guard = ermya_graph_server::metrics::LabelGuard::new();
    for i in 0..ermya_graph_server::metrics::METRICS_DATABASE_LABEL_CAP {
        let name = format!("db{i:04}");
        let label = guard.resolve_database_label(&name);
        assert_ne!(
            &*label,
            "_other",
            "db{i:04} (i={i}) collapsed too early — cap should not bite below {}",
            ermya_graph_server::metrics::METRICS_DATABASE_LABEL_CAP,
        );
    }
    let overflow = guard.resolve_database_label("db_overflow");
    assert_eq!(
        &*overflow, "_other",
        "the first name past METRICS_DATABASE_LABEL_CAP must collapse to _other"
    );
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bolt_messages_counter_increments_on_run_and_pull() {
    let _serial = BOLT_METRICS_TEST_LOCK.lock().await;
    let (bolt_addr, metrics_addr, shutdown_tx, server_join, _tmp) =
        spawn_test_server_with_db("qdb").await;

    let before_run = snapshot(
        &metrics_addr,
        "ermya_bolt_messages_total",
        &[("type", "RUN"), ("outcome", "success")],
    )
    .await;
    let before_pull = snapshot(
        &metrics_addr,
        "ermya_bolt_messages_total",
        &[("type", "PULL"), ("outcome", "success")],
    )
    .await;

    send_bolt_run_and_pull(bolt_addr, "qdb", "RETURN 1 AS x").await;

    let saw_run = poll_until(
        Duration::from_secs(2),
        Duration::from_millis(20),
        || async move {
            let now = scrape_value(
                &metrics_addr,
                "ermya_bolt_messages_total",
                &[("type", "RUN"), ("outcome", "success")],
            )
            .await;
            (now - before_run) >= 1.0
        },
    )
    .await;
    let saw_pull = poll_until(
        Duration::from_secs(2),
        Duration::from_millis(20),
        || async move {
            let now = scrape_value(
                &metrics_addr,
                "ermya_bolt_messages_total",
                &[("type", "PULL"), ("outcome", "success")],
            )
            .await;
            (now - before_pull) >= 1.0
        },
    )
    .await;
    assert!(saw_run, "RUN counter did not increment");
    assert!(saw_pull, "PULL counter did not increment");

    let _ = shutdown_tx.send(true);
    let _ = tokio::time::timeout(Duration::from_secs(5), server_join).await;
}

// ── Cycle C4: WAL fsync histogram E2E ───────────────────────────────────────

/// Drives the `ermya_wal_fsync_duration_seconds` histogram by running N
/// `CREATE` statements over Bolt against a file-backed database. Each
/// `CREATE` triggers a real WAL fsync (writes happen outside an explicit
/// batch; `handle_run` does not wrap them in `begin_batch`), so the
/// histogram `_count` series must grow by at least N.
///
/// Pre-condition for the green run: `registry::open_and_insert` installs
/// a `WalObserver` closure that calls `metrics::wal_fsync_observed`.
/// Without that wiring the histogram never receives samples and the
/// assertion fails.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wal_fsync_histogram_has_samples_after_writes() {
    let _serial = BOLT_METRICS_TEST_LOCK.lock().await;
    // `N_WRITES` lives at the top of the body so clippy's
    // `items_after_statements` lint stays quiet. The `_f64` companion
    // is declared as a literal (not a cast) to satisfy
    // `clippy::cast_precision_loss` while keeping the assertion in
    // `f64` arithmetic (histograms expose float counters).
    const N_WRITES: u32 = 3;
    const N_WRITES_F64: f64 = 3.0;

    let (bolt_addr, metrics_addr, shutdown_tx, server_join, _tmp) =
        spawn_test_server_with_db("fsync_db").await;

    // Snapshot before any writes. The recorder is process-global so
    // sibling tests in the same binary may have already produced samples
    // — we assert a delta, not an absolute count.
    let before = snapshot(&metrics_addr, "ermya_wal_fsync_duration_seconds_count", &[]).await;

    for i in 0..N_WRITES {
        // Each call opens a fresh Bolt session, runs CREATE, drains
        // PULL. The CREATE happens outside any explicit batch, so the
        // engine performs a synchronous fsync on commit.
        send_bolt_run_and_pull(
            bolt_addr,
            "fsync_db",
            &format!("CREATE (:FsyncNode {{seq: {i}}})"),
        )
        .await;
    }

    let saw_samples = poll_until(
        Duration::from_secs(5),
        Duration::from_millis(50),
        || async move {
            let now =
                scrape_value(&metrics_addr, "ermya_wal_fsync_duration_seconds_count", &[]).await;
            (now - before) >= N_WRITES_F64
        },
    )
    .await;

    let after = scrape_value(&metrics_addr, "ermya_wal_fsync_duration_seconds_count", &[]).await;
    assert!(
        saw_samples,
        "expected >= {N_WRITES} new fsync samples in histogram, \
         got delta {} (before={before}, after={after})",
        after - before
    );

    let _ = shutdown_tx.send(true);
    let _ = tokio::time::timeout(Duration::from_secs(5), server_join).await;
}

// ── Cycle 5: slow query log (AuditEvent::SlowQuery via emit_query_pair) ──────
//
// The gating contract of `emit_query_pair` (threshold comparison, gate
// pass-through, outcome propagation) is exercised deterministically by
// unit tests in `audit.rs::tests` — they feed a synthetic `duration_ms`
// and an `Instant`, so they never depend on wall-clock timing. A real
// RUN over the batched + WAL backend measures `duration_ms = 0` for a
// trivial statement on an SSD, so a wall-clock-driven E2E "slow" assert
// would be flaky by construction.
//
// This E2E covers the *wiring*: it proves the handler routes RUNs
// through `emit_query_pair` by exercising the `threshold_ms = 0`
// kill-switch, which is the one slow-query outcome that is deterministic
// regardless of how fast or slow the query runs (zero `slow_query`
// lines, always). Combined with the unit tests, the success/error
// emission branches are fully covered without timing flakiness.

/// Read the file-backed audit log and return every JSON line whose
/// `event_type` is `slow_query`. Missing file → empty. Synchronous
/// `std::fs` read — `tokio`'s `fs` feature is not enabled in dev-deps
/// and the log is small + off the hot path (mirrors `admin_test.rs`).
fn slow_query_events_in_audit_log(audit_path: &std::path::Path) -> Vec<serde_json::Value> {
    let mut out = Vec::new();
    let Ok(content) = std::fs::read_to_string(audit_path) else {
        return out;
    };
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if v.get("event_type").and_then(serde_json::Value::as_str) == Some("slow_query") {
            out.push(v);
        }
    }
    out
}

/// Spawn a server with a file-backed audit sink and slow-query config.
/// `audit_fsync_every: 1` makes every event durable immediately so the
/// scan sees it without waiting on a fsync window. Returns the
/// audit-log path so the test can scan it for `slow_query` lines.
#[cfg(unix)]
async fn spawn_test_server_with_slow_log(
    threshold_ms: u64,
    max_per_minute: u32,
) -> (
    SocketAddr,
    std::path::PathBuf,
    watch::Sender<bool>,
    tokio::task::JoinHandle<ermya_graph_server::Result<ServerHandle>>,
    tempfile::TempDir,
) {
    let tmp = tempfile::tempdir().expect("create test tempdir");
    let data_dir = tmp.path().to_path_buf();
    let audit_path = data_dir.join("audit.log");
    common::prepopulate_system(&data_dir, "admin-pw-12chars").await;
    let cfg = ServerConfig {
        bind_addr: "127.0.0.1:0".to_owned(),
        data_dir: Some(data_dir.clone()),
        password: Some("admin-pw-12chars".to_owned()),
        audit_sink: ermya_graph_server::config::AuditSinkKind::File,
        audit_file: Some(audit_path.clone()),
        audit_fsync_every: 1,
        slow_query_threshold_ms: threshold_ms,
        max_slow_events_per_minute: max_per_minute,
        ..Default::default()
    };
    let (ready_tx, ready_rx) = oneshot::channel();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let server_join = tokio::spawn(async move {
        ermya_graph_server::start_server_with_registry(
            cfg,
            shutdown_rx,
            Some(ready_tx),
            ermya_graph_server::single_database_factory(),
            ermya_graph_server::startup::PaidStartupHooks::default(),
        )
        .await
    });
    let ready = tokio::time::timeout(Duration::from_secs(5), ready_rx)
        .await
        .expect("ready timed out")
        .expect("ready sender dropped");
    (ready.bolt_addr, audit_path, shutdown_tx, server_join, tmp)
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn slow_query_threshold_zero_is_a_kill_switch() {
    let _serial = BOLT_METRICS_TEST_LOCK.lock().await;
    // threshold=0 must short-circuit emit_query_pair: no slow_query line
    // ever lands, regardless of how slow the RUNs are. This is the
    // deterministic proof that handle_run is wired through
    // emit_query_pair — the query_exec lines still appear (verified in
    // the audit-log content), but the slow_query branch is suppressed.
    let (bolt_addr, audit_path, shutdown_tx, server_join, _tmp) =
        spawn_test_server_with_slow_log(0, 0).await;
    for i in 0..5 {
        send_bolt_run_and_pull(
            bolt_addr,
            "slow_db",
            &format!("CREATE (:KillSwitch {{n: {i}}})"),
        )
        .await;
    }
    // Also drive a failing RUN so the error call sites are exercised.
    let resp =
        send_bolt_run_no_pull(bolt_addr, "slow_db", "NOT A VALID GQL STATEMENT AT ALL").await;
    assert!(
        matches!(resp, BoltResponse::Failure { .. }),
        "RUN must fail with a parser error; got {resp:?}"
    );
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Sanity: the regular query_exec stream did land, so the handler ran
    // the statements (otherwise the kill-switch assert would be vacuous).
    let raw = std::fs::read_to_string(&audit_path).unwrap_or_default();
    assert!(
        raw.contains("\"event_type\":\"query_exec\""),
        "expected query_exec lines proving the handler ran; raw log:\n{raw}"
    );
    let events = slow_query_events_in_audit_log(&audit_path);
    assert!(
        events.is_empty(),
        "threshold=0 must suppress every slow_query event; found {}",
        events.len()
    );

    let _ = shutdown_tx.send(true);
    let _ = tokio::time::timeout(Duration::from_secs(5), server_join).await;
}

// ── Cycle 7: #[tracing::instrument] on handle_run with dynamic fields ────────

use std::sync::OnceLock;

type SpanFields = std::collections::HashMap<String, String>;

/// Shared store of captured spans, keyed by span id so `on_record`
/// merges dynamic fields into the right entry. Behind a `OnceLock`
/// because the capturing subscriber is installed **globally** — the
/// server runs `handle_run` in a `JoinSet` task that a thread-local
/// `set_default` would not reach, so only a process-global subscriber
/// observes those spans.
static CAPTURED_SPANS: OnceLock<
    std::sync::Mutex<std::collections::HashMap<u64, (String, SpanFields)>>,
> = OnceLock::new();

fn captured_spans()
-> &'static std::sync::Mutex<std::collections::HashMap<u64, (String, SpanFields)>> {
    CAPTURED_SPANS.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

struct SpanFieldVisitor<'a>(&'a mut SpanFields);
impl tracing::field::Visit for SpanFieldVisitor<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.0.insert(field.name().to_owned(), format!("{value:?}"));
    }
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.0.insert(field.name().to_owned(), value.to_owned());
    }
    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        self.0.insert(field.name().to_owned(), value.to_string());
    }
}

struct CaptureLayer;
impl<S> tracing_subscriber::Layer<S> for CaptureLayer
where
    S: tracing::Subscriber + for<'lookup> tracing_subscriber::registry::LookupSpan<'lookup>,
{
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        id: &tracing::Id,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut fields = SpanFields::new();
        attrs.record(&mut SpanFieldVisitor(&mut fields));
        captured_spans()
            .lock()
            .unwrap()
            .insert(id.into_u64(), (attrs.metadata().name().to_owned(), fields));
    }
    fn on_record(
        &self,
        id: &tracing::Id,
        values: &tracing::span::Record<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        if let Some(entry) = captured_spans().lock().unwrap().get_mut(&id.into_u64()) {
            values.record(&mut SpanFieldVisitor(&mut entry.1));
        }
    }
}

/// Install the global capturing subscriber exactly once for this test
/// binary. Idempotent: a second call is a no-op (the first install wins,
/// `set_global_default` errors on the rest and we ignore it).
fn install_capture_subscriber_once() {
    use tracing_subscriber::layer::SubscriberExt as _;
    static INSTALLED: OnceLock<()> = OnceLock::new();
    INSTALLED.get_or_init(|| {
        let subscriber = tracing_subscriber::registry().with(CaptureLayer);
        // Ignore the error: another test in the same binary may have
        // installed a global subscriber first. In that case this test
        // cannot capture and will surface that as a missing span.
        let _ = tracing::subscriber::set_global_default(subscriber);
    });
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn handle_run_emits_tracing_span_with_dynamic_fields() {
    let _serial = BOLT_METRICS_TEST_LOCK.lock().await;
    install_capture_subscriber_once();

    // threshold huge so the slow-query path never fires; we only want the
    // span. RETURN 1 AS x exercises the ConstReturn terminal branch.
    let (bolt_addr, _audit_path, shutdown_tx, server_join, _tmp) =
        spawn_test_server_with_slow_log(1_000_000, 0).await;
    send_bolt_run_and_pull(bolt_addr, "slow_db", "RETURN 1 AS x").await;
    let _ = shutdown_tx.send(true);
    let _ = tokio::time::timeout(Duration::from_secs(5), server_join).await;

    // Find a handle_run span carrying all five fields. The global store
    // may hold spans from sibling tests; we only need one complete one.
    let store = captured_spans().lock().unwrap();
    let handle_run_spans: Vec<&(String, SpanFields)> = store
        .values()
        .filter(|(name, _)| name == "handle_run")
        .collect();
    assert!(
        !handle_run_spans.is_empty(),
        "a handle_run span must be captured; captured span names: {:?}",
        store.values().map(|(n, _)| n).collect::<Vec<_>>()
    );
    let complete = handle_run_spans.iter().find(|(_, f)| {
        f.contains_key("connection_id")
            && f.contains_key("database")
            && f.contains_key("statement_sha256")
            && f.contains_key("kind")
            && f.contains_key("duration_ms")
    });
    assert!(
        complete.is_some(),
        "no handle_run span carried all five dynamic fields; captured: {handle_run_spans:?}"
    );
}

// ── Cycle 8: JSON tracing subscriber + EnvFilter opt-in ──────────────────────

#[test]
fn init_tracing_with_json_format_produces_parseable_json_lines() {
    use tracing_subscriber::fmt::MakeWriter;

    #[derive(Clone)]
    struct SharedBuf(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
    impl MakeWriter<'_> for SharedBuf {
        type Writer = SharedBufWriter;
        fn make_writer(&self) -> Self::Writer {
            SharedBufWriter(self.0.clone())
        }
    }
    struct SharedBufWriter(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
    impl std::io::Write for SharedBufWriter {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let buf = SharedBuf(std::sync::Arc::new(std::sync::Mutex::new(Vec::new())));
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_env_filter(tracing_subscriber::EnvFilter::new("info"))
        .with_writer(buf.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    tracing::info!(target: "task3_test", connection_id = 7u64, "json sanity");

    let captured = String::from_utf8(buf.0.lock().unwrap().clone()).expect("utf-8");
    let line = captured.lines().next().expect("at least one line emitted");
    let parsed: serde_json::Value =
        serde_json::from_str(line).expect("emitted line must be valid JSON");
    assert_eq!(
        parsed.get("level").and_then(serde_json::Value::as_str),
        Some("INFO"),
        "JSON record must carry the event level"
    );
    assert_eq!(
        parsed.get("target").and_then(serde_json::Value::as_str),
        Some("task3_test"),
        "JSON record must carry the event target"
    );
    assert_eq!(
        parsed
            .get("fields")
            .and_then(|f| f.get("message"))
            .and_then(serde_json::Value::as_str),
        Some("json sanity"),
        "JSON record must nest the message under fields"
    );
    assert_eq!(
        parsed
            .get("fields")
            .and_then(|f| f.get("connection_id"))
            .and_then(serde_json::Value::as_u64),
        Some(7),
        "JSON record must nest structured u64 fields under fields"
    );
}

// ── Task 4 Cycle 6: ermya_result_capped_total ─────────────────────────────

/// Like [`spawn_test_server_with_db`] but with a finite
/// `max_result_rows`, so a query whose output exceeds it is aborted by
/// the defensive result-row cap. Auth-enabled, single pre-created
/// database owned by `admin`.
#[cfg(unix)]
async fn spawn_test_server_with_db_and_cap(
    _db_name: &str,
    cap: u64,
) -> (
    SocketAddr,
    SocketAddr,
    watch::Sender<bool>,
    tokio::task::JoinHandle<ermya_graph_server::Result<ServerHandle>>,
    tempfile::TempDir,
) {
    let tmp = tempfile::tempdir().expect("create test tempdir");
    let data_dir = tmp.path().to_path_buf();
    common::prepopulate_system(&data_dir, "admin-pw-12chars").await;

    let cfg = ServerConfig {
        bind_addr: "127.0.0.1:0".to_owned(),
        metrics_addr: Some("127.0.0.1:0".to_owned()),
        data_dir: Some(data_dir.clone()),
        password: Some("admin-pw-12chars".to_owned()),
        max_result_rows: cap,
        ..Default::default()
    };

    let (ready_tx, ready_rx) = oneshot::channel();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let server_join = tokio::spawn(async move {
        ermya_graph_server::start_server_with_registry(
            cfg,
            shutdown_rx,
            Some(ready_tx),
            ermya_graph_server::single_database_factory(),
            ermya_graph_server::startup::PaidStartupHooks::default(),
        )
        .await
    });

    let ready = tokio::time::timeout(Duration::from_secs(5), ready_rx)
        .await
        .expect("ready channel timed out")
        .expect("ready sender dropped");
    let metrics_addr = ready
        .metrics_addr
        .expect("metrics endpoint must bind when ServerConfig.metrics_addr is Some");

    (ready.bolt_addr, metrics_addr, shutdown_tx, server_join, tmp)
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn result_capped_counter_increments_on_over_cap_query() {
    let _serial = BOLT_METRICS_TEST_LOCK.lock().await;
    let db = "capdb";
    let (bolt_addr, metrics_addr, shutdown_tx, server_join, _tmp) =
        spawn_test_server_with_db_and_cap(db, 3).await;

    // La medida lleva el nombre de la base que el servidor SIRVIÓ, no el que
    // pidió la consulta: esta edición tiene una sola base y su nombre es fijo,
    // así que el nombre pedido se ignora al resolver.
    let labels: &[(&str, &str)] = &[("database", ermya_graph_server::registry::COMMUNITY_DATABASE)];
    let before = snapshot(&metrics_addr, "ermya_result_capped_total", labels).await;

    // Seed 10 nodes then MATCH them on the SAME connection so the read
    // sees its own writes. A literal list is used (not `range()`) per
    // error-log 2026-05-27. The over-cap MATCH fails at RUN time.
    let (mut cw, mut cr) = open_bolt_session(bolt_addr).await;
    let hello = BoltRequest::Hello {
        extra: vec![
            (
                "principal".to_owned(),
                PackStreamValue::String("admin".to_owned()),
            ),
            (
                "credentials".to_owned(),
                PackStreamValue::String("admin-pw-12chars".to_owned()),
            ),
        ],
    };
    assert!(
        matches!(
            bolt_send_recv(&mut cw, &mut cr, &hello).await,
            BoltResponse::Success { .. }
        ),
        "HELLO must succeed"
    );
    let with_db = |q: &str| BoltRequest::Run {
        query: q.to_owned(),
        params: vec![],
        extra: vec![("db".to_owned(), PackStreamValue::String(db.to_owned()))],
    };
    let seed_resp = bolt_send_recv(
        &mut cw,
        &mut cr,
        &with_db("UNWIND [1,2,3,4,5,6,7,8,9,10] AS i CREATE (:N {i:i})"),
    )
    .await;
    assert!(
        matches!(seed_resp, BoltResponse::Success { .. }),
        "seeding CREATE must succeed, got {seed_resp:?}"
    );
    // Drain the CREATE result fully: one PULL yields Record(counts)
    // followed by a trailing Success. `bolt_send_recv` reads only the
    // first reply, so read the trailing Success explicitly to keep the
    // message stream aligned for the next RUN.
    let pull = BoltRequest::Pull { extra: vec![] };
    cw.write_message(&encode_request(&pull).expect("encode pull"))
        .await
        .expect("write pull");
    loop {
        let data = cr.read_message().await.expect("read").expect("msg");
        if matches!(
            decode_response(&data).expect("decode"),
            BoltResponse::Success { .. }
        ) {
            break;
        }
    }

    let resp = bolt_send_recv(&mut cw, &mut cr, &with_db("MATCH (n:N) RETURN n")).await;
    assert!(
        matches!(resp, BoltResponse::Failure { .. }),
        "over-cap query must FAIL at RUN, got {resp:?}"
    );

    let saw_increment = poll_until(
        Duration::from_secs(2),
        Duration::from_millis(20),
        || async move {
            let now = scrape_value(&metrics_addr, "ermya_result_capped_total", labels).await;
            (now - before) >= 1.0
        },
    )
    .await;
    let after = scrape_value(&metrics_addr, "ermya_result_capped_total", labels).await;
    assert!(
        saw_increment,
        "result_capped counter must increment after over-cap query (before={before}, after={after})"
    );

    let _ = shutdown_tx.send(true);
    let _ = tokio::time::timeout(Duration::from_secs(5), server_join).await;
}
