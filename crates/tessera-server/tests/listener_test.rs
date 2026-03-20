// Copyright 2026 BelowZero Security OU. All rights reserved.

use std::sync::{Arc, RwLock};
use std::time::Duration;

use tessera_audit::AuditLog;
use tessera_auth::credentials::{Password, PasswordPolicy};
use tessera_auth::policy::AuthPolicy;
use tessera_auth::rbac::{RoleStore, RoleStoreHandle};
use tessera_auth::session::SessionManager;
use tessera_auth::user::UserStoreHandle;
use tessera_graph::Graph;
use tessera_protocol::frame::{FramedReader, FramedWriter};
use tessera_protocol::message::{ClientMessage, ServerMessage};
use tessera_server::TesseraListener;
use tessera_server::context::ServerContext;

fn test_tls_config() -> tessera_protocol::TlsConfig {
    let dir = tempfile::tempdir().unwrap();
    let key_pair = rcgen::KeyPair::generate().unwrap();
    let params = rcgen::CertificateParams::new(vec!["localhost".to_owned()]).unwrap();
    let cert = params.self_signed(&key_pair).unwrap();
    let cert_path = dir.path().join("cert.pem");
    let key_path = dir.path().join("key.pem");
    std::fs::write(&cert_path, cert.pem()).unwrap();
    std::fs::write(&key_path, key_pair.serialize_pem()).unwrap();

    tessera_protocol::tls::TlsConfigBuilder::new()
        .cert_file(cert_path)
        .key_file(key_path)
        .build()
        .unwrap()
}

fn test_context() -> Arc<ServerContext> {
    let admin_pw = Password::new("Admin@Init1!").unwrap();
    let user_store =
        Arc::new(UserStoreHandle::new("admin", &admin_pw, &PasswordPolicy::default()).unwrap());
    user_store
        .assign_role("admin", RoleStore::ADMIN_ROLE_ID)
        .unwrap();
    let sessions = Arc::new(SessionManager::new(3600));
    let policy = Arc::new(AuthPolicy::new(
        Arc::clone(&user_store),
        RoleStoreHandle::with_defaults(),
    ));

    let dir = tempfile::tempdir().unwrap();
    let audit = Arc::new(AuditLog::open(&dir.path().join("audit.ndjson")).unwrap());
    let tls = test_tls_config();

    Arc::new(ServerContext::new(policy, sessions, audit, tls, user_store))
}

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

    // Start the server in the background (plain TCP, no TLS for this test)
    tokio::spawn(async move {
        let _ = listener
            .serve(ctx, graph, shutdown_rx, 10, Duration::from_secs(30))
            .await;
    });

    // Give server a moment to start accepting
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Connect two clients and send Ping from each
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

    // First connection — should succeed (send Ping, get Pong)
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
        matches!(response2, ServerMessage::AuthError { ref reason } if reason.contains("capacity")),
        "expected capacity error, got {response2:?}"
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

    // Connect and ping to verify it's running
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

    // Server should stop within a reasonable time
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
    // After Bye, the next read should return None (EOF)
    let eof = reader.read_frame().await.unwrap();
    assert_eq!(eof, None);

    let _ = shutdown_tx.send(true);
}
