// Copyright 2026 BelowZero Security OU. All rights reserved.

//! Shared test helpers for `tessera-server` integration tests.

use std::sync::Arc;
use std::time::Duration;

use tessera_audit::AuditLog;
use tessera_auth::credentials::{Password, PasswordPolicy};
use tessera_auth::policy::AuthPolicy;
use tessera_auth::rate_limit::LoginPolicy;
use tessera_auth::rbac::{RoleStore, RoleStoreHandle};
use tessera_auth::session::SessionManager;
use tessera_auth::user::UserStoreHandle;
use tessera_graph::GraphConfig;
use tessera_protocol::bolt_frame::{BoltChunkedReader, BoltChunkedWriter};
use tessera_protocol::bolt_message::{BoltRequest, BoltResponse};
use tessera_protocol::{BOLT_MAGIC, decode_response, encode_request};
use tessera_server::BoltConnectionHandler;
use tessera_server::context::ServerContext;
use tessera_tenant::TenantRegistry;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

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

/// Create a test `TenantRegistry` backed by a temporary directory.
///
/// Returns the `TempDir` guard alongside the registry so the directory stays
/// alive as long as the test needs it, and is cleaned up when dropped.
#[allow(dead_code)]
pub fn test_registry() -> (tempfile::TempDir, Arc<TenantRegistry>) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_path_buf();
    let registry = Arc::new(TenantRegistry::new(path, GraphConfig::new()));
    (dir, registry)
}

/// Create a test `ServerContext` with a fresh `TenantRegistry`.
///
/// Returns the `TempDir` guard so the directory stays alive for the test.
#[allow(dead_code)]
pub fn test_context() -> (tempfile::TempDir, Arc<ServerContext>) {
    let (dir, registry) = test_registry();
    let ctx = test_context_with_registry(registry);
    (dir, ctx)
}

/// Create a test `ServerContext` sharing the given `TenantRegistry`.
#[allow(dead_code)]
pub fn test_context_with_registry(registry: Arc<TenantRegistry>) -> Arc<ServerContext> {
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

    let audit = Arc::new(AuditLog::new_null());
    let tls = test_tls_config();

    let metrics = Arc::new(tessera_monitor::MetricsRegistry::new(256));
    Arc::new(ServerContext::new(
        policy, sessions, audit, tls, user_store, metrics, registry,
    ))
}

/// Create a test `ServerContext` with a custom login policy for rate-limit tests.
#[allow(dead_code)]
pub fn test_context_with_rate_limit(
    max_attempts: u32,
    lockout_secs: u64,
) -> (tempfile::TempDir, Arc<ServerContext>) {
    let (dir, registry) = test_registry();
    let ctx = test_context_with_registry(registry);
    // Unwrap the Arc to modify the policy — this only works when refcount == 1,
    // which is guaranteed here because we just created it.
    let inner = Arc::try_unwrap(ctx).unwrap_or_else(|_| panic!("refcount must be 1"));
    (
        dir,
        Arc::new(inner.with_login_policy(LoginPolicy::new(max_attempts, lockout_secs))),
    )
}

/// Spawn a `BoltConnectionHandler` on a duplex stream, perform the client-side
/// Bolt 4.4 handshake, and return the client-side chunked reader/writer.
///
/// The handshake is done inside this function: the server reads 20 bytes and
/// writes 4 bytes, while this function writes 20 bytes and reads 4 bytes —
/// all concurrently via `tokio::spawn`.
#[allow(dead_code)]
pub async fn spawn_bolt_handler(
    ctx: Arc<ServerContext>,
) -> (
    BoltChunkedWriter<tokio::io::WriteHalf<tokio::io::DuplexStream>>,
    BoltChunkedReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
    tokio::sync::watch::Sender<bool>,
) {
    let (client_stream, server_stream) = tokio::io::duplex(65_536);
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    // Spawn the server side — it reads the 20-byte handshake internally.
    tokio::spawn(async move {
        match BoltConnectionHandler::new_with_handshake(
            server_stream,
            ctx,
            "default".to_owned(),
            Duration::from_secs(30),
            shutdown_rx,
        )
        .await
        {
            Ok(mut handler) => {
                let _ = handler.run().await;
            }
            Err(e) => {
                eprintln!("bolt handler error: {e}");
            }
        }
    });

    // Client side: send the 20-byte Bolt 4.4 handshake, then read the 4-byte
    // response, before wrapping the halves in chunked framing.
    let (mut client_read, mut client_write) = tokio::io::split(client_stream);

    let mut handshake = [0u8; 20];
    handshake[..4].copy_from_slice(&BOLT_MAGIC);
    // Version proposal: 0x00_04_04_04 = major=4, range=4, minor=4
    // This says "I support Bolt 4.0 through 4.4".
    handshake[4..8].copy_from_slice(&0x0004_0404_u32.to_be_bytes());
    client_write.write_all(&handshake).await.unwrap();
    client_write.flush().await.unwrap();

    let mut resp = [0u8; 4];
    client_read.read_exact(&mut resp).await.unwrap();
    // Server responds with [0x00, major, minor, 0x00] = [0x00, 0x04, 0x04, 0x00]
    assert_eq!(
        resp,
        [0x00, 0x04, 0x04, 0x00],
        "bolt handshake version mismatch"
    );

    (
        BoltChunkedWriter::new(client_write),
        BoltChunkedReader::new(client_read),
        shutdown_tx,
    )
}

/// Send a [`BoltRequest`] over the chunked writer.
#[allow(dead_code)]
pub async fn bolt_send(
    writer: &mut BoltChunkedWriter<tokio::io::WriteHalf<tokio::io::DuplexStream>>,
    req: &BoltRequest,
) {
    let data = encode_request(req).unwrap();
    writer.write_message(&data).await.unwrap();
}

/// Read a [`BoltResponse`] from the chunked reader.
#[allow(dead_code)]
pub async fn bolt_recv(
    reader: &mut BoltChunkedReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
) -> BoltResponse {
    let data = reader
        .read_message()
        .await
        .unwrap()
        .expect("expected message");
    decode_response(&data).unwrap()
}
