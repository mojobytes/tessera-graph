// Copyright 2026 BelowZero Security OU. All rights reserved.

mod common;

use std::sync::{Arc, RwLock};
use std::time::Duration;

use tessera_graph::Graph;
use tessera_protocol::frame::{FramedReader, FramedWriter};
use tessera_protocol::message::{ClientMessage, ServerMessage};
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

#[tokio::test]
async fn listener_handles_two_concurrent_connections() {
    let ctx = test_context();
    let graph = Arc::new(RwLock::new(Graph::new()));
    let listener = TesseraListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    tokio::spawn(async move {
        let _ = listener
            .serve(ctx, graph, shutdown_rx, 10, Duration::from_secs(30))
            .await;
    });

    tokio::time::sleep(Duration::from_millis(50)).await;

    let mut handles = Vec::new();
    for _ in 0..2 {
        let handle = tokio::spawn(async move {
            let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
            let (read_half, write_half) = tokio::io::split(stream);
            let mut writer = FramedWriter::new(write_half);
            let mut reader = FramedReader::new(read_half);

            let json = serde_json::to_vec(&ClientMessage::Ping).unwrap();
            writer.write_frame(&json).await.unwrap();

            let frame = reader.read_frame().await.unwrap().unwrap();
            let response: ServerMessage = serde_json::from_slice(&frame).unwrap();
            assert_eq!(response, ServerMessage::Pong);
        });
        handles.push(handle);
    }

    for h in handles {
        h.await.unwrap();
    }

    let _ = shutdown_tx.send(true);
}

#[tokio::test]
async fn server_rejects_connection_when_at_capacity() {
    let ctx = test_context();
    let graph = Arc::new(RwLock::new(Graph::new()));
    let listener = TesseraListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    // max_connections = 1
    tokio::spawn(async move {
        let _ = listener
            .serve(ctx, graph, shutdown_rx, 1, Duration::from_secs(30))
            .await;
    });

    tokio::time::sleep(Duration::from_millis(50)).await;

    // First connection — should succeed
    let stream1 = tokio::net::TcpStream::connect(addr).await.unwrap();
    let (read_half, write_half) = tokio::io::split(stream1);
    let mut writer1 = FramedWriter::new(write_half);
    let mut reader1 = FramedReader::new(read_half);

    let json = serde_json::to_vec(&ClientMessage::Ping).unwrap();
    writer1.write_frame(&json).await.unwrap();
    let frame = reader1.read_frame().await.unwrap().unwrap();
    let response: ServerMessage = serde_json::from_slice(&frame).unwrap();
    assert_eq!(response, ServerMessage::Pong);

    // Second connection — should get capacity error
    let stream2 = tokio::net::TcpStream::connect(addr).await.unwrap();
    let (read_half2, _write_half2) = tokio::io::split(stream2);
    let mut reader2 = FramedReader::new(read_half2);

    let frame2 = reader2.read_frame().await.unwrap().unwrap();
    let response2: ServerMessage = serde_json::from_slice(&frame2).unwrap();
    assert!(
        matches!(response2, ServerMessage::CapacityError { ref reason } if reason.contains("capacity")),
        "expected CapacityError, got {response2:?}"
    );

    let _ = shutdown_tx.send(true);
}

#[tokio::test]
async fn graceful_shutdown_stops_accept_loop() {
    let ctx = test_context();
    let graph = Arc::new(RwLock::new(Graph::new()));
    let listener = TesseraListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    let server_handle = tokio::spawn(async move {
        listener
            .serve(ctx, graph, shutdown_rx, 10, Duration::from_secs(30))
            .await
    });

    tokio::time::sleep(Duration::from_millis(50)).await;

    // Verify it's running
    let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let (read_half, write_half) = tokio::io::split(stream);
    let mut writer = FramedWriter::new(write_half);
    let mut reader = FramedReader::new(read_half);

    let json = serde_json::to_vec(&ClientMessage::Ping).unwrap();
    writer.write_frame(&json).await.unwrap();
    let frame = reader.read_frame().await.unwrap().unwrap();
    let response: ServerMessage = serde_json::from_slice(&frame).unwrap();
    assert_eq!(response, ServerMessage::Pong);

    // Send shutdown
    let _ = shutdown_tx.send(true);

    let result = tokio::time::timeout(Duration::from_secs(5), server_handle).await;
    assert!(result.is_ok(), "server did not shut down within 5 seconds");
}

#[tokio::test]
async fn idle_connection_is_closed_after_timeout() {
    let ctx = test_context();
    let graph = Arc::new(RwLock::new(Graph::new()));
    let listener = TesseraListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    // Very short idle timeout
    tokio::spawn(async move {
        let _ = listener
            .serve(ctx, graph, shutdown_rx, 10, Duration::from_millis(200))
            .await;
    });

    tokio::time::sleep(Duration::from_millis(50)).await;

    let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let (read_half, _write_half) = tokio::io::split(stream);
    let mut reader = FramedReader::new(read_half);

    // Wait for idle timeout — should get Bye then EOF
    let frame = reader.read_frame().await.unwrap();
    if let Some(data) = frame {
        let response: ServerMessage = serde_json::from_slice(&data).unwrap();
        assert_eq!(response, ServerMessage::Bye);
    }
    let eof = reader.read_frame().await.unwrap();
    assert_eq!(eof, None);

    let _ = shutdown_tx.send(true);
}

// --- Quality fix tests ---

#[tokio::test]
async fn graceful_shutdown_drains_active_connections() {
    let ctx = test_context();
    let graph = Arc::new(RwLock::new(Graph::new()));
    let listener = TesseraListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    tokio::spawn(async move {
        let _ = listener
            .serve(ctx, graph, shutdown_rx, 10, Duration::from_secs(30))
            .await;
    });

    tokio::time::sleep(Duration::from_millis(50)).await;

    // Connect 3 clients and verify they are active
    let mut readers = Vec::new();
    for _ in 0..3 {
        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let (read_half, write_half) = tokio::io::split(stream);
        let mut writer = FramedWriter::new(write_half);
        let mut reader = FramedReader::new(read_half);

        let json = serde_json::to_vec(&ClientMessage::Ping).unwrap();
        writer.write_frame(&json).await.unwrap();
        let frame = reader.read_frame().await.unwrap().unwrap();
        let response: ServerMessage = serde_json::from_slice(&frame).unwrap();
        assert_eq!(response, ServerMessage::Pong);

        readers.push(reader);
    }

    // Send shutdown
    let _ = shutdown_tx.send(true);

    // Each active connection should receive Bye
    for mut reader in readers {
        let frame = tokio::time::timeout(Duration::from_secs(3), reader.read_frame())
            .await
            .expect("timed out waiting for Bye")
            .unwrap()
            .expect("expected Bye frame");
        let msg: ServerMessage = serde_json::from_slice(&frame).unwrap();
        assert_eq!(msg, ServerMessage::Bye, "expected Bye on shutdown");
    }
}
