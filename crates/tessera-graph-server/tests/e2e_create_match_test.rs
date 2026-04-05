// Copyright 2026 BelowZero Security OU. All rights reserved.

//! End-to-end test: real TCP server + real BoltClient.
//!
//! Reproduces the `TesseraBoltTarget` flow: CREATE then MATCH over a real
//! TCP connection (no DuplexStream). This isolates whether the bug is in
//! the TCP/framing layer vs the handler logic.

mod common;

use std::sync::Arc;
use std::time::Duration;

use tessera_graph_protocol::BoltClient;
use tessera_graph_server::TesseraListener;
use tokio_rustls::rustls;
use tokio_rustls::rustls::client::danger::{
    HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier,
};
use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use tokio_rustls::rustls::{DigitallySignedStruct, SignatureScheme};

use common::test_context;

/// No-op certificate verifier — mirrors `TesseraBoltTarget::NoCertVerifier`.
#[derive(Debug)]
struct NoCertVerifier;

impl ServerCertVerifier for NoCertVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        rustls::crypto::aws_lc_rs::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[tokio::test]
async fn e2e_tcp_create_then_match_returns_node() {
    let (_dir, ctx) = test_context();
    let listener = TesseraListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    // Spawn server (plain TCP, no TLS — isolates protocol logic)
    tokio::spawn(async move {
        let _ = listener
            .serve(
                ctx,
                shutdown_rx,
                10,
                Duration::from_secs(30),
                "default".to_owned(),
            )
            .await;
    });

    // Give the server time to start accepting.
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Connect a real BoltClient over TCP.
    let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
    let (reader, writer) = tokio::io::split(tcp);
    let mut client = BoltClient::connect_split(reader, writer)
        .await
        .expect("Bolt handshake failed");

    client
        .hello("admin", "Admin@Init1!", None)
        .await
        .expect("Bolt auth failed");

    // CREATE — same as TesseraBoltTarget::create_node
    let create_result = client
        .run_query("CREATE (:N)")
        .await
        .expect("CREATE query failed");
    assert!(
        !create_result.rows.is_empty(),
        "CREATE must return a summary row, got 0 rows"
    );

    // MATCH — same as TesseraBoltTarget::resolve_node_ids
    let match_result = client
        .run_query("MATCH (n) RETURN id(n) AS nid ORDER BY id(n) ASC")
        .await
        .expect("MATCH query failed");

    assert_eq!(
        match_result.rows.len(),
        1,
        "MATCH after CREATE must return 1 row, got {}. Columns: {:?}, Rows: {:?}",
        match_result.rows.len(),
        match_result.columns,
        match_result.rows,
    );

    // Cleanup
    let _ = client.goodbye().await;
    let _ = shutdown_tx.send(true);
}

#[tokio::test]
async fn e2e_tcp_multiple_creates_then_match_returns_all() {
    let (_dir, ctx) = test_context();
    let listener = TesseraListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    tokio::spawn(async move {
        let _ = listener
            .serve(
                ctx,
                shutdown_rx,
                10,
                Duration::from_secs(30),
                "default".to_owned(),
            )
            .await;
    });

    tokio::time::sleep(Duration::from_millis(50)).await;

    let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
    let (reader, writer) = tokio::io::split(tcp);
    let mut client = BoltClient::connect_split(reader, writer)
        .await
        .expect("Bolt handshake failed");

    client
        .hello("admin", "Admin@Init1!", None)
        .await
        .expect("auth failed");

    // CREATE 5 nodes sequentially (same as benchmark setup)
    for i in 0..5 {
        let result = client
            .run_query(&format!("CREATE (:N {{idx: {i}}})"))
            .await
            .unwrap_or_else(|e| panic!("CREATE {i} failed: {e}"));
        assert!(
            !result.rows.is_empty(),
            "CREATE {i} must return summary row"
        );
    }

    // MATCH all — same query as resolve_node_ids
    let match_result = client
        .run_query("MATCH (n) RETURN id(n) AS nid ORDER BY id(n) ASC")
        .await
        .expect("MATCH query failed");

    assert_eq!(
        match_result.rows.len(),
        5,
        "MATCH after 5 CREATEs must return 5 rows, got {}. Rows: {:?}",
        match_result.rows.len(),
        match_result.rows,
    );

    // Also test label-filtered MATCH
    let label_result = client
        .run_query("MATCH (n:N) RETURN n.idx")
        .await
        .expect("label MATCH failed");

    assert_eq!(
        label_result.rows.len(),
        5,
        "MATCH (n:N) must also return 5 rows, got {}",
        label_result.rows.len(),
    );

    let _ = client.goodbye().await;
    let _ = shutdown_tx.send(true);
}

// ── TLS E2E — same path as TesseraBoltTarget ────────────────────────────────

#[tokio::test]
async fn e2e_tls_create_then_match_returns_node() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    let (_dir, ctx) = test_context();
    let listener = TesseraListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    // Spawn TLS server — exactly as production
    tokio::spawn(async move {
        let _ = listener
            .serve_tls(
                ctx,
                shutdown_rx,
                10,
                Duration::from_secs(30),
                "default".to_owned(),
            )
            .await;
    });

    tokio::time::sleep(Duration::from_millis(50)).await;

    // TLS client — same as TesseraBoltTarget::connect
    let tls_config = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(NoCertVerifier))
        .with_no_client_auth();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(tls_config));

    let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
    let server_name = ServerName::try_from("localhost").unwrap();
    let tls_stream = connector
        .connect(server_name, tcp)
        .await
        .expect("TLS handshake failed");

    let (reader, writer) = tokio::io::split(tls_stream);
    let mut client = BoltClient::connect_split(reader, writer)
        .await
        .expect("Bolt handshake failed");

    client
        .hello("admin", "Admin@Init1!", None)
        .await
        .expect("Bolt auth failed");

    // CREATE — same as TesseraBoltTarget::create_node
    let create_result = client
        .run_query("CREATE (:N)")
        .await
        .expect("CREATE failed");
    assert!(
        !create_result.rows.is_empty(),
        "CREATE must return a summary row"
    );

    // MATCH — same as TesseraBoltTarget::resolve_node_ids
    let match_result = client
        .run_query("MATCH (n) RETURN id(n) AS nid ORDER BY id(n) ASC")
        .await
        .expect("MATCH failed");

    assert_eq!(
        match_result.rows.len(),
        1,
        "MATCH after CREATE over TLS must return 1 row, got {}. Rows: {:?}",
        match_result.rows.len(),
        match_result.rows,
    );

    let _ = client.goodbye().await;
    let _ = shutdown_tx.send(true);
}

#[tokio::test]
async fn e2e_tls_5_creates_then_match_returns_all() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    let (_dir, ctx) = test_context();
    let listener = TesseraListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    tokio::spawn(async move {
        let _ = listener
            .serve_tls(
                ctx,
                shutdown_rx,
                10,
                Duration::from_secs(30),
                "default".to_owned(),
            )
            .await;
    });

    tokio::time::sleep(Duration::from_millis(50)).await;

    let tls_config = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(NoCertVerifier))
        .with_no_client_auth();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(tls_config));

    let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
    let server_name = ServerName::try_from("localhost").unwrap();
    let tls_stream = connector
        .connect(server_name, tcp)
        .await
        .expect("TLS handshake failed");

    let (reader, writer) = tokio::io::split(tls_stream);
    let mut client = BoltClient::connect_split(reader, writer)
        .await
        .expect("Bolt handshake failed");

    client
        .hello("admin", "Admin@Init1!", None)
        .await
        .expect("auth failed");

    // CREATE 5 nodes — exact benchmark pattern
    for i in 0..5 {
        client
            .run_query(&format!("CREATE (:N {{idx: {i}}})"))
            .await
            .unwrap_or_else(|e| panic!("CREATE {i} failed: {e}"));
    }

    // MATCH all
    let match_result = client
        .run_query("MATCH (n) RETURN id(n) AS nid ORDER BY id(n) ASC")
        .await
        .expect("MATCH failed");

    assert_eq!(
        match_result.rows.len(),
        5,
        "MATCH after 5 CREATEs over TLS must return 5 rows, got {}. Rows: {:?}",
        match_result.rows.len(),
        match_result.rows,
    );

    let _ = client.goodbye().await;
    let _ = shutdown_tx.send(true);
}
