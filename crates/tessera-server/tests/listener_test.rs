// Copyright 2026 BelowZero Security OU. All rights reserved.

mod common;

use std::time::Duration;

use tessera_server::TesseraListener;

use common::test_context;

#[tokio::test]
async fn listener_binds_to_address() {
    let listener = TesseraListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    assert!(addr.port() > 0);
    assert_eq!(addr.ip(), std::net::Ipv4Addr::LOCALHOST);
}

#[tokio::test]
async fn listener_accepts_and_dispatches_connection() {
    let listener = TesseraListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let handle = tokio::spawn(async move {
        let (stream, peer_addr) = listener.accept().await.unwrap();
        assert!(peer_addr.port() > 0);
        drop(stream);
    });

    let _client = tokio::net::TcpStream::connect(addr).await.unwrap();
    handle.await.unwrap();
}

/// Verify the listener starts and can be connected to.
/// The full Bolt handshake flow is tested in `bolt_handler_test.rs`.
#[tokio::test]
async fn listener_serves_plain_connections() {
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

    // A raw TCP connection can be established (Bolt handshake would follow).
    let _stream = tokio::net::TcpStream::connect(addr).await.unwrap();

    let _ = shutdown_tx.send(true);
}

#[tokio::test]
async fn graceful_shutdown_stops_accept_loop() {
    let (_dir, ctx) = test_context();
    let listener = TesseraListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    let server_handle = tokio::spawn(async move {
        listener
            .serve(
                ctx,
                shutdown_rx,
                10,
                Duration::from_secs(30),
                "default".to_owned(),
            )
            .await
    });

    tokio::time::sleep(Duration::from_millis(50)).await;

    // Verify the listener is running.
    let _stream = tokio::net::TcpStream::connect(addr).await.unwrap();

    // Send shutdown.
    let _ = shutdown_tx.send(true);

    let result = tokio::time::timeout(Duration::from_secs(5), server_handle).await;
    assert!(result.is_ok(), "server did not shut down within 5 seconds");
}
