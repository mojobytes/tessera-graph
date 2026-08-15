// SPDX-License-Identifier: BSL-1.1

//! Integration tests for [`ErmyaListener`].

mod common;

use std::sync::Arc;
use std::time::Duration;

#[cfg(feature = "plain-tcp")]
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_rustls::rustls::pki_types::pem::PemObject;

#[cfg(feature = "plain-tcp")]
use ermya_graph_protocol::bolt_message::{BoltRequest, BoltResponse};
use ermya_graph_server::ErmyaListener;

// ── Cycle 5.1: bind + local_addr ────────────────────────────────────────────

#[tokio::test]
async fn listener_binds_to_ephemeral_port() {
    let listener = ErmyaListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    assert!(addr.port() > 0);
    assert_eq!(addr.ip(), std::net::Ipv4Addr::LOCALHOST);
}

// ── Cycle 5.2: serve_with — generic accept loop ────────────────────────────

#[tokio::test]
async fn serve_with_accepts_connection() {
    let listener = ErmyaListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    let accepted = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let accepted_clone = Arc::clone(&accepted);

    tokio::spawn(async move {
        let _ = listener
            .serve_with(
                move |_stream| {
                    accepted_clone.store(true, std::sync::atomic::Ordering::Relaxed);
                    async {}
                },
                shutdown_rx,
                10,
            )
            .await;
    });

    // Give the server a moment to start accepting.
    tokio::time::sleep(Duration::from_millis(50)).await;
    let _client = tokio::net::TcpStream::connect(addr).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;

    assert!(
        accepted.load(std::sync::atomic::Ordering::Relaxed),
        "handler should have been called"
    );

    let _ = shutdown_tx.send(true);
}

#[tokio::test]
async fn serve_with_stops_on_shutdown() {
    let listener = ErmyaListener::bind("127.0.0.1:0").await.unwrap();
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    let handle = tokio::spawn(async move {
        listener
            .serve_with(|_stream| async {}, shutdown_rx, 10)
            .await
    });

    tokio::time::sleep(Duration::from_millis(50)).await;
    let _ = shutdown_tx.send(true);

    let result = tokio::time::timeout(Duration::from_secs(5), handle).await;
    assert!(result.is_ok(), "server did not shut down within 5 seconds");
}

// ── Cycle 5.3: Semaphore max_connections ────────────────────────────────────

#[tokio::test]
async fn serve_with_respects_max_connections() {
    let listener = ErmyaListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    // max_connections = 1, handler blocks forever (holds the permit).
    tokio::spawn(async move {
        let _ = listener
            .serve_with(
                |_stream| async {
                    // Hold the connection (and its semaphore permit) indefinitely.
                    tokio::time::sleep(Duration::from_secs(300)).await;
                },
                shutdown_rx,
                1, // max 1 connection
            )
            .await;
    });

    tokio::time::sleep(Duration::from_millis(50)).await;

    // First connection occupies the single permit.
    let _c1 = tokio::net::TcpStream::connect(addr).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Second connection: TCP accepted but server drops it (no permit).
    let mut c2 = tokio::net::TcpStream::connect(addr).await.unwrap();
    let mut buf = [0u8; 1];
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        tokio::io::AsyncReadExt::read(&mut c2, &mut buf),
    )
    .await;

    // Expect EOF (0 bytes) or error — the server dropped the stream.
    assert!(
        result.is_err() || matches!(result, Ok(Ok(0) | Err(_))),
        "expected c2 to be dropped by server"
    );

    let _ = shutdown_tx.send(true);
}

// ── Cycle 5.4: serve_plain (feature-gated) ──────────────────────────────────

#[cfg(feature = "plain-tcp")]
#[tokio::test]
async fn serve_plain_handles_bolt_hello() {
    // Cycle 7: serve_plain now requires a `DatabaseRegistry`, and HELLO
    // routes through it — `extras.database` is mandatory and must name
    // a row in the catalogue. The legacy "no database" HELLO is covered
    // by the handler-level tests; this test still earns its keep as the
    // one TCP-end-to-end exercise of `serve_plain`.
    let components = common::fresh_listener_components("tenanta").await;
    let listener = ErmyaListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    let auth = components.auth;
    let auth_store = components.auth_store;
    let registry = components.registry;
    let _tmp = components.tmp;
    let rate_limiter = ermya_graph_server::rate_limiter::RateLimiter::new(64, 0, 0);
    // Montaje público: ni gestor de pago ni despachador de pago. El oyente
    // recibe el gestor por su interfaz y no distingue cuál le dan.
    tokio::spawn(async move {
        let _ = listener
            .serve_plain(
                auth,
                auth_store,
                ermya_graph_server::audit::AuditSink::off(),
                registry,
                None,
                None,
                rate_limiter,
                shutdown_rx,
                10,
                Duration::from_secs(30),
                0,
                0,
                0,
                0,
                0,
                0, // query_timeout_ms (Task 6 — disabled in this test)
                format!("Neo4j/{}", env!("CARGO_PKG_VERSION")), // server_agent (Block 1)
            )
            .await;
    });

    tokio::time::sleep(Duration::from_millis(100)).await;

    // Connect as Bolt client through real TCP.
    let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let (mut read, mut write) = tokio::io::split(stream);

    // Bolt 4.4 handshake.
    let mut handshake = [0u8; 20];
    handshake[..4].copy_from_slice(&ermya_graph_protocol::BOLT_MAGIC);
    handshake[4..8].copy_from_slice(&0x0004_0404_u32.to_be_bytes());
    write.write_all(&handshake).await.unwrap();
    write.flush().await.unwrap();

    let mut resp = [0u8; 4];
    read.read_exact(&mut resp).await.unwrap();
    assert_eq!(resp, [0x00, 0x00, 0x04, 0x04], "bolt version mismatch");

    // Wrap in chunked framing.
    let mut cw = ermya_graph_protocol::bolt_frame::BoltChunkedWriter::new(write);
    let mut cr = ermya_graph_protocol::bolt_frame::BoltChunkedReader::new(read);

    // HELLO with mandatory `extras.database = "tenanta"` → SUCCESS.
    let hello = BoltRequest::Hello {
        extra: vec![
            (
                "principal".to_owned(),
                ermya_graph_protocol::PackStreamValue::String("admin".to_owned()),
            ),
            (
                "credentials".to_owned(),
                ermya_graph_protocol::PackStreamValue::String("ignored".to_owned()),
            ),
            (
                "database".to_owned(),
                ermya_graph_protocol::PackStreamValue::String("tenanta".to_owned()),
            ),
        ],
    };
    let data = ermya_graph_protocol::encode_request(&hello).unwrap();
    cw.write_message(&data).await.unwrap();

    let resp_data = cr.read_message().await.unwrap().expect("expected message");
    let resp = ermya_graph_protocol::decode_response(&resp_data).unwrap();
    assert!(
        matches!(resp, BoltResponse::Success { .. }),
        "expected SUCCESS for HELLO, got {resp:?}"
    );

    // GOODBYE.
    let goodbye = BoltRequest::Goodbye;
    let data = ermya_graph_protocol::encode_request(&goodbye).unwrap();
    cw.write_message(&data).await.unwrap();

    let _ = shutdown_tx.send(true);
}

#[cfg(feature = "plain-tcp")]
#[tokio::test]
// One end-to-end HELLO → RUN(CREATE) → RUN(MATCH) → PULL handshake that must
// run as a single linear sequence on one session; splitting it would obscure
// the very flow it verifies. Pre-existing 101/100 line count surfaced once the
// gate ran `--features plain-tcp --all-targets` on this binary.
#[allow(clippy::too_many_lines)]
async fn serve_plain_create_and_query_through_tcp() {
    // Cycle 7: end-to-end CREATE + MATCH through plain TCP, now routed
    // through the real `DatabaseRegistry`. The query graph the registry
    // hands back is per-database, so CREATE and MATCH share the same
    // backing graph as long as they ride the same HELLO session.
    let components = common::fresh_listener_components("tenanta").await;
    let listener = ErmyaListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    let auth = components.auth;
    let auth_store = components.auth_store;
    let registry = components.registry;
    let _tmp = components.tmp;
    let rate_limiter = ermya_graph_server::rate_limiter::RateLimiter::new(64, 0, 0);
    // Montaje público: ni gestor de pago ni despachador de pago. El oyente
    // recibe el gestor por su interfaz y no distingue cuál le dan.
    tokio::spawn(async move {
        let _ = listener
            .serve_plain(
                auth,
                auth_store,
                ermya_graph_server::audit::AuditSink::off(),
                registry,
                None,
                None,
                rate_limiter,
                shutdown_rx,
                10,
                Duration::from_secs(30),
                0,
                0,
                0,
                0,
                0,
                0, // query_timeout_ms (Task 6 — disabled in this test)
                format!("Neo4j/{}", env!("CARGO_PKG_VERSION")), // server_agent (Block 1)
            )
            .await;
    });

    tokio::time::sleep(Duration::from_millis(100)).await;

    // Connect as Bolt client through TCP — full roundtrip.
    let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let (mut read, mut write) = tokio::io::split(stream);

    // Bolt handshake.
    let mut hs = [0u8; 20];
    hs[..4].copy_from_slice(&ermya_graph_protocol::BOLT_MAGIC);
    hs[4..8].copy_from_slice(&0x0004_0404_u32.to_be_bytes());
    write.write_all(&hs).await.unwrap();
    write.flush().await.unwrap();
    let mut ver = [0u8; 4];
    read.read_exact(&mut ver).await.unwrap();

    let mut cw = ermya_graph_protocol::bolt_frame::BoltChunkedWriter::new(write);
    let mut cr = ermya_graph_protocol::bolt_frame::BoltChunkedReader::new(read);

    // HELLO with mandatory `extras.database = "tenanta"`.
    tcp_send(
        &mut cw,
        &BoltRequest::Hello {
            extra: vec![
                (
                    "principal".to_owned(),
                    ermya_graph_protocol::PackStreamValue::String("admin".to_owned()),
                ),
                (
                    "credentials".to_owned(),
                    ermya_graph_protocol::PackStreamValue::String("ignored".to_owned()),
                ),
                (
                    "database".to_owned(),
                    ermya_graph_protocol::PackStreamValue::String("tenanta".to_owned()),
                ),
            ],
        },
    )
    .await;
    let _ = tcp_recv(&mut cr).await;

    // CREATE. v0.5.0 Task 10-bis cycle 7 requires `extra["db"]` on the
    // first RUN of the session (HELLO no longer binds the database).
    tcp_send(
        &mut cw,
        &common::run_message_with_db("CREATE (:City {name: 'Berlin'})", "tenanta"),
    )
    .await;
    let run_resp = tcp_recv(&mut cr).await;
    assert!(
        matches!(run_resp, BoltResponse::Success { .. }),
        "expected SUCCESS for CREATE RUN, got {run_resp:?}"
    );

    // PULL the CREATE result — drain RECORD(s) + trailing SUCCESS.
    tcp_send(&mut cw, &BoltRequest::Pull { extra: vec![] }).await;
    loop {
        let r = tcp_recv(&mut cr).await;
        if matches!(r, BoltResponse::Success { .. }) {
            break;
        }
    }

    // MATCH — `db_handle` already bound, no rebind needed.
    tcp_send(
        &mut cw,
        &common::run_message("MATCH (n:City) RETURN n.name"),
    )
    .await;
    let match_resp = tcp_recv(&mut cr).await;
    assert!(
        matches!(match_resp, BoltResponse::Success { .. }),
        "expected SUCCESS for MATCH RUN, got {match_resp:?}"
    );

    // PULL — expect at least 1 RECORD.
    tcp_send(&mut cw, &BoltRequest::Pull { extra: vec![] }).await;
    let mut got_record = false;
    loop {
        let r = tcp_recv(&mut cr).await;
        if matches!(r, BoltResponse::Record { .. }) {
            got_record = true;
        }
        if matches!(r, BoltResponse::Success { .. }) {
            break;
        }
    }
    assert!(got_record, "expected at least one RECORD from MATCH");

    // GOODBYE.
    tcp_send(&mut cw, &BoltRequest::Goodbye).await;

    let _ = shutdown_tx.send(true);
}

// ── TCP test helpers (concrete types, not generic) ──────────────────────────

#[cfg(feature = "plain-tcp")]
async fn tcp_send(
    writer: &mut ermya_graph_protocol::bolt_frame::BoltChunkedWriter<
        tokio::io::WriteHalf<tokio::net::TcpStream>,
    >,
    req: &BoltRequest,
) {
    let data = ermya_graph_protocol::encode_request(req).unwrap();
    writer.write_message(&data).await.unwrap();
}

#[cfg(feature = "plain-tcp")]
async fn tcp_recv(
    reader: &mut ermya_graph_protocol::bolt_frame::BoltChunkedReader<
        tokio::io::ReadHalf<tokio::net::TcpStream>,
    >,
) -> BoltResponse {
    let data = reader
        .read_message()
        .await
        .unwrap()
        .expect("expected message");
    ermya_graph_protocol::decode_response(&data).unwrap()
}

// ── Cycle 5.5: serve_tls rejects plain TCP ──────────────────────────────────

#[tokio::test]
async fn serve_tls_rejects_plain_client() {
    // Generate self-signed cert for testing.
    let key_pair = rcgen::KeyPair::generate().unwrap();
    let params = rcgen::CertificateParams::new(vec!["localhost".to_owned()]).unwrap();
    let cert = params.self_signed(&key_pair).unwrap();

    let cert_pem = cert.pem();
    let key_pem = key_pair.serialize_pem();

    // Build rustls ServerConfig.
    let certs =
        tokio_rustls::rustls::pki_types::CertificateDer::pem_slice_iter(cert_pem.as_bytes())
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap();
    let key =
        tokio_rustls::rustls::pki_types::PrivateKeyDer::from_pem_slice(key_pem.as_bytes()).unwrap();

    let tls_config = Arc::new(
        tokio_rustls::rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .unwrap(),
    );

    // The plain-TCP probe never reaches the handler (TLS rejects it
    // first), so the registry is a placeholder; fresh_listener_components
    // is the simplest way to assemble the auth + registry combo without
    // re-deriving in-line.
    let components = common::fresh_listener_components("tenanta").await;
    let listener = ErmyaListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    let auth = components.auth;
    let auth_store = components.auth_store;
    let registry = components.registry;
    let tmp_guard = components.tmp;
    let rate_limiter = ermya_graph_server::rate_limiter::RateLimiter::new(64, 0, 0);
    // Montaje público: ni gestor de pago ni despachador de pago. El oyente
    // recibe el gestor por su interfaz y no distingue cuál le dan.
    tokio::spawn(async move {
        let _ = listener
            .serve_tls(
                auth,
                auth_store,
                ermya_graph_server::audit::AuditSink::off(),
                registry,
                None,
                None,
                tls_config,
                rate_limiter,
                shutdown_rx,
                10,
                Duration::from_secs(30),
                0,
                0,
                0,
                0,
                0,
                0, // query_timeout_ms (Task 6 — disabled in this test)
                format!("Neo4j/{}", env!("CARGO_PKG_VERSION")), // server_agent (Block 1)
            )
            .await;
    });

    tokio::time::sleep(Duration::from_millis(100)).await;

    // Connect with plain TCP (no TLS) and send garbage.
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    tokio::io::AsyncWriteExt::write_all(&mut stream, b"not a tls handshake")
        .await
        .unwrap();

    // Server should reject — the stream closes without a valid Bolt response.
    // We may receive a TLS alert (a few bytes) before EOF, so we just verify
    // we cannot read a full Bolt handshake response (4 bytes of version).
    let mut buf = [0u8; 64];
    let mut total = 0usize;
    loop {
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            tokio::io::AsyncReadExt::read(&mut stream, &mut buf[total..]),
        )
        .await;
        match result {
            Ok(Ok(n)) if n > 0 => total += n, // got some bytes (TLS alert)
            _ => break,                       // EOF, I/O error, or timeout
        }
    }
    // Whatever we received should NOT be a valid Bolt version response.
    // A valid response is [0x00, 0x00, 0x04, 0x04].
    let is_bolt = total >= 4 && buf[..4] == [0x00, 0x00, 0x04, 0x04];
    assert!(
        !is_bolt,
        "plain TCP client should not get a valid Bolt handshake"
    );

    let _ = shutdown_tx.send(true);
    drop(tmp_guard);
}

// ── Task 5 Cycle 4: connection-IP cap E2E ────────────────────────────────────

/// Complete a Bolt 4.4 handshake + HELLO over `stream`, returning the
/// chunked reader/writer so the caller can keep the connection alive.
/// Used by [`serve_plain_caps_connections_per_ip`] to hold connections
/// open while a third one is attempted.
#[cfg(feature = "plain-tcp")]
async fn open_bolt_session(
    addr: std::net::SocketAddr,
    db: &str,
) -> (
    ermya_graph_protocol::bolt_frame::BoltChunkedWriter<
        tokio::io::WriteHalf<tokio::net::TcpStream>,
    >,
    ermya_graph_protocol::bolt_frame::BoltChunkedReader<
        tokio::io::ReadHalf<tokio::net::TcpStream>,
    >,
) {
    let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let (mut read, mut write) = tokio::io::split(stream);

    let mut handshake = [0u8; 20];
    handshake[..4].copy_from_slice(&ermya_graph_protocol::BOLT_MAGIC);
    handshake[4..8].copy_from_slice(&0x0004_0404_u32.to_be_bytes());
    write.write_all(&handshake).await.unwrap();
    write.flush().await.unwrap();

    let mut resp = [0u8; 4];
    read.read_exact(&mut resp).await.unwrap();
    assert_eq!(resp, [0x00, 0x00, 0x04, 0x04], "bolt version mismatch");

    let mut cw = ermya_graph_protocol::bolt_frame::BoltChunkedWriter::new(write);
    let mut cr = ermya_graph_protocol::bolt_frame::BoltChunkedReader::new(read);

    let hello = BoltRequest::Hello {
        extra: vec![
            (
                "principal".to_owned(),
                ermya_graph_protocol::PackStreamValue::String("admin".to_owned()),
            ),
            (
                "credentials".to_owned(),
                ermya_graph_protocol::PackStreamValue::String("ignored".to_owned()),
            ),
            (
                "database".to_owned(),
                ermya_graph_protocol::PackStreamValue::String(db.to_owned()),
            ),
        ],
    };
    let data = ermya_graph_protocol::encode_request(&hello).unwrap();
    cw.write_message(&data).await.unwrap();
    let resp_data = cr
        .read_message()
        .await
        .unwrap()
        .expect("expected HELLO reply");
    let resp = ermya_graph_protocol::decode_response(&resp_data).unwrap();
    assert!(
        matches!(resp, BoltResponse::Success { .. }),
        "expected SUCCESS for HELLO, got {resp:?}"
    );

    (cw, cr)
}

#[cfg(feature = "plain-tcp")]
#[tokio::test]
async fn serve_plain_caps_connections_per_ip() {
    // conn_per_ip = 2: the third concurrent connection from 127.0.0.1 is
    // rejected by the accept loop *before* the Bolt handshake. The client
    // sees an immediate EOF when it tries to read the 4-byte version reply.
    let components = common::fresh_listener_components("tenanta").await;
    let listener = ErmyaListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    let auth = components.auth;
    let auth_store = components.auth_store;
    let registry = components.registry;
    let tmp = components.tmp;

    // Capture audit events to a file so the test can assert the shape of
    // the `connection_throttled` event (Task 5 eje 3), not just the wire
    // rejection. The new `live_connections` field must reflect the count
    // the IP held at the rejection boundary (= cap when the cap is hit).
    let audit_path = tmp.path().join("conn_throttle_audit.log");
    let (audit_shutdown_tx, audit_shutdown_rx) = tokio::sync::watch::channel(false);
    let audit = ermya_graph_server::audit::AuditSink::file(
        audit_path.clone(),
        1_000_000,
        3,
        0,
        audit_shutdown_rx,
    )
    .expect("audit sink");

    // ip_cap 64, auth 0 (disabled), conn_per_ip 2.
    let rate_limiter = ermya_graph_server::rate_limiter::RateLimiter::new(64, 0, 2);
    // Montaje público: ni gestor de pago ni despachador de pago. El oyente
    // recibe el gestor por su interfaz y no distingue cuál le dan.
    tokio::spawn(async move {
        let _ = listener
            .serve_plain(
                auth,
                auth_store,
                audit,
                registry,
                None,
                None,
                rate_limiter,
                shutdown_rx,
                10,
                Duration::from_secs(30),
                0,
                0,
                0,
                0,
                0,
                0, // query_timeout_ms (Task 6 — disabled in this test)
                format!("Neo4j/{}", env!("CARGO_PKG_VERSION")), // server_agent (Block 1)
            )
            .await;
    });

    tokio::time::sleep(Duration::from_millis(100)).await;

    // Two sessions succeed and stay open (guards held by their handlers).
    let s1 = open_bolt_session(addr, "tenanta").await;
    let _s2 = open_bolt_session(addr, "tenanta").await;

    // The third connection is accepted at TCP level, then dropped by the
    // cap check before the handshake completes. Reading the version reply
    // must therefore yield EOF (0 bytes) or an error — never the 4-byte
    // Bolt version response.
    let mut third = tokio::net::TcpStream::connect(addr).await.unwrap();
    let mut handshake = [0u8; 20];
    handshake[..4].copy_from_slice(&ermya_graph_protocol::BOLT_MAGIC);
    handshake[4..8].copy_from_slice(&0x0004_0404_u32.to_be_bytes());
    // The write may succeed (kernel buffer) even though the server will
    // not read it; we don't assert on the write outcome.
    let _ = third.write_all(&handshake).await;
    let _ = third.flush().await;

    let mut resp = [0u8; 4];
    let read_result =
        tokio::time::timeout(Duration::from_secs(2), third.read_exact(&mut resp)).await;
    assert!(
        matches!(read_result, Ok(Err(_)) | Err(_)),
        "3rd connection over the per-IP cap must not receive a Bolt \
         version reply; got {read_result:?}"
    );

    // The rejection must have emitted a `connection_throttled` audit event
    // carrying both the configured `cap` (2) and the `live_connections`
    // the peer IP held at the rejection boundary (2 — the two sessions
    // still open). Drain the audit log and assert the full shape.
    let events = common::read_audit_events(&audit_shutdown_tx, &audit_path).await;
    let throttle = events
        .iter()
        .find(|e| {
            e.get("event_type").and_then(serde_json::Value::as_str) == Some("connection_throttled")
        })
        .unwrap_or_else(|| panic!("expected a connection_throttled audit event, got: {events:#?}"));
    assert_eq!(
        throttle.get("cap").and_then(serde_json::Value::as_u64),
        Some(2),
        "connection_throttled.cap must be the configured per-IP cap (2): {throttle:#?}"
    );
    assert_eq!(
        throttle
            .get("live_connections")
            .and_then(serde_json::Value::as_u64),
        Some(2),
        "connection_throttled.live_connections must be the count held at the \
         rejection boundary (2): {throttle:#?}"
    );
    assert_eq!(
        throttle
            .get("client_ip")
            .and_then(serde_json::Value::as_str),
        Some("127.0.0.1"),
        "connection_throttled.client_ip must be the loopback peer: {throttle:#?}"
    );

    // Re-arm the audit channel so the listener task keeps running for the
    // post-drop reconnection below (read_audit_events flipped it to true).
    let _ = audit_shutdown_tx.send(false);

    // Dropping one held session frees a slot; a new connection succeeds.
    drop(s1);
    // Give the handler task a moment to observe the closed socket and run
    // the ConnectionGuard Drop.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let _s3 = open_bolt_session(addr, "tenanta").await;

    let _ = shutdown_tx.send(true);
}
