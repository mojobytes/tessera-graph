// Copyright 2026 BelowZero Security OU. All rights reserved.

//! Shared test helpers for `tessera-server` integration tests.

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
use tessera_server::ConnectionHandler;
use tessera_server::context::ServerContext;

#[allow(dead_code)]
pub fn test_tls_config() -> tessera_protocol::TlsConfig {
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

#[allow(dead_code)]
pub fn test_context() -> Arc<ServerContext> {
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

/// Send a `ClientMessage` over a framed writer and read back a `ServerMessage`.
#[allow(dead_code)]
pub async fn send_recv(
    writer: &mut FramedWriter<tokio::io::WriteHalf<tokio::io::DuplexStream>>,
    reader: &mut FramedReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
    msg: &ClientMessage,
) -> ServerMessage {
    let json = serde_json::to_vec(msg).unwrap();
    writer.write_frame(&json).await.unwrap();
    let frame = reader.read_frame().await.unwrap().expect("expected frame");
    serde_json::from_slice(&frame).unwrap()
}

/// Spawn a connection handler on a duplex stream and return the client-side halves.
#[allow(dead_code)]
pub fn spawn_handler(
    ctx: Arc<ServerContext>,
    graph: Arc<RwLock<Graph>>,
) -> (
    FramedWriter<tokio::io::WriteHalf<tokio::io::DuplexStream>>,
    FramedReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
    tokio::sync::watch::Sender<bool>,
) {
    let (client_stream, server_stream) = tokio::io::duplex(8192);
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    tokio::spawn(async move {
        let mut handler = ConnectionHandler::new(
            server_stream,
            ctx,
            graph,
            Duration::from_secs(30),
            shutdown_rx,
        );
        let _ = handler.run().await;
    });

    let (read_half, write_half) = tokio::io::split(client_stream);
    (
        FramedWriter::new(write_half),
        FramedReader::new(read_half),
        shutdown_tx,
    )
}
