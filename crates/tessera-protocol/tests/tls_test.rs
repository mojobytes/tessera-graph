// Copyright 2026 BelowZero Security OU. All rights reserved.

use tessera_protocol::tls::{ClientAuth, TlsConfigBuilder};

/// Generate a self-signed cert+key pair and write to temp files.
fn generate_test_cert(dir: &std::path::Path) -> (std::path::PathBuf, std::path::PathBuf) {
    let key_pair = rcgen::KeyPair::generate().unwrap();
    let params = rcgen::CertificateParams::new(vec!["localhost".to_owned()]).unwrap();
    let cert = params.self_signed(&key_pair).unwrap();

    let cert_path = dir.join("cert.pem");
    let key_path = dir.join("key.pem");

    std::fs::write(&cert_path, cert.pem()).unwrap();
    std::fs::write(&key_path, key_pair.serialize_pem()).unwrap();

    (cert_path, key_path)
}

/// Generate a mismatched cert and key (cert signed by one key, file has a different key).
fn generate_mismatched_cert_key(dir: &std::path::Path) -> (std::path::PathBuf, std::path::PathBuf) {
    let kp1 = rcgen::KeyPair::generate().unwrap();
    let params1 = rcgen::CertificateParams::new(vec!["localhost".to_owned()]).unwrap();
    let cert1 = params1.self_signed(&kp1).unwrap();

    let kp2 = rcgen::KeyPair::generate().unwrap();

    let cert_path = dir.join("cert.pem");
    let key_path = dir.join("key.pem");

    std::fs::write(&cert_path, cert1.pem()).unwrap();
    std::fs::write(&key_path, kp2.serialize_pem()).unwrap();

    (cert_path, key_path)
}

#[test]
fn tls_config_loads_valid_cert_and_key() {
    let dir = tempfile::tempdir().unwrap();
    let (cert, key) = generate_test_cert(dir.path());
    let result = TlsConfigBuilder::new()
        .cert_file(cert)
        .key_file(key)
        .build();
    assert!(result.is_ok());
}

#[test]
fn tls_config_rejects_mismatched_cert_and_key() {
    let dir = tempfile::tempdir().unwrap();
    let (cert, key) = generate_mismatched_cert_key(dir.path());
    let result = TlsConfigBuilder::new()
        .cert_file(cert)
        .key_file(key)
        .build();
    assert!(result.is_err());
}

#[test]
fn tls_config_rejects_missing_cert_file() {
    let result = TlsConfigBuilder::new()
        .cert_file("/nonexistent/cert.pem")
        .key_file("/nonexistent/key.pem")
        .build();
    assert!(result.is_err());
}

#[test]
fn tls_config_without_client_auth_builds_successfully() {
    let dir = tempfile::tempdir().unwrap();
    let (cert, key) = generate_test_cert(dir.path());
    let result = TlsConfigBuilder::new()
        .cert_file(cert)
        .key_file(key)
        .client_auth(ClientAuth::None)
        .build();
    assert!(result.is_ok());
}

#[test]
fn mtls_required_returns_not_implemented_error() {
    let dir = tempfile::tempdir().unwrap();
    let (cert, key) = generate_test_cert(dir.path());
    let result = TlsConfigBuilder::new()
        .cert_file(cert)
        .key_file(key)
        .client_auth(ClientAuth::Required)
        .build();
    let Err(err) = result else {
        panic!("ClientAuth::Required must return error until CA verification is implemented")
    };
    assert!(
        err.to_string().contains("mTLS"),
        "error must mention mTLS, got: {err}"
    );
}

#[test]
fn tls_config_is_send_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<tessera_protocol::tls::TlsConfig>();
}
