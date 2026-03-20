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

    let key_pair = rcgen::KeyPair::generate().unwrap();
    let params = rcgen::CertificateParams::new(vec!["localhost".to_owned()]).unwrap();
    let cert = params.self_signed(&key_pair).unwrap();
    let cert_path = dir.path().join("cert.pem");
    let key_path = dir.path().join("key.pem");
    std::fs::write(&cert_path, cert.pem()).unwrap();
    std::fs::write(&key_path, key_pair.serialize_pem()).unwrap();

    let tls = tessera_protocol::tls::TlsConfigBuilder::new()
        .cert_file(cert_path)
        .key_file(key_path)
        .build()
        .unwrap();

    Arc::new(ServerContext::new(policy, sessions, audit, tls, user_store))
}

#[tokio::test]
async fn ping_pong_throughput_guard() {
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

    let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let (read_half, write_half) = tokio::io::split(stream);
    let mut writer = FramedWriter::new(write_half);
    let mut reader = FramedReader::new(read_half);

    let ping_json = serde_json::to_vec(&ClientMessage::Ping).unwrap();

    let n: u64 = 10_000;
    let start = std::time::Instant::now();

    for _ in 0..n {
        writer.write_frame(&ping_json).await.unwrap();
        let frame = reader.read_frame().await.unwrap().unwrap();
        let response: ServerMessage = serde_json::from_slice(&frame).unwrap();
        assert_eq!(response, ServerMessage::Pong);
    }

    let elapsed = start.elapsed();
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    let rps = (n as f64 / elapsed.as_secs_f64()) as u64;

    let min_rps: u64 = if cfg!(debug_assertions) {
        2_000
    } else {
        20_000
    };

    assert!(
        rps >= min_rps,
        "ping-pong regression: {rps} rps < {min_rps}"
    );

    let _ = shutdown_tx.send(true);
}
