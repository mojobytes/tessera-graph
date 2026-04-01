// Copyright 2026 BelowZero Security OU. All rights reserved.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rustls::ServerConfig;

use crate::error::{ProtocolError, Result};

/// Client authentication mode for TLS connections.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientAuth {
    /// No client certificate required.
    None,
    /// Client certificate required (mutual TLS).
    /// Not yet implemented — `build()` returns an error if this is selected.
    Required,
}

/// Wraps a configured `rustls::ServerConfig`. Enforces TLS 1.3 only.
pub struct TlsConfig {
    inner: Arc<ServerConfig>,
}

impl TlsConfig {
    /// Access the underlying `rustls::ServerConfig`.
    #[must_use]
    pub const fn server_config(&self) -> &Arc<ServerConfig> {
        &self.inner
    }
}

/// Builder for `TlsConfig`.
pub struct TlsConfigBuilder {
    cert_path: Option<PathBuf>,
    key_path: Option<PathBuf>,
    client_auth: ClientAuth,
}

impl TlsConfigBuilder {
    /// Create a new builder with default settings (no client auth).
    #[must_use]
    pub const fn new() -> Self {
        Self {
            cert_path: None,
            key_path: None,
            client_auth: ClientAuth::None,
        }
    }

    /// Set the path to the PEM-encoded certificate file.
    #[must_use]
    pub fn cert_file(mut self, path: impl AsRef<Path>) -> Self {
        self.cert_path = Some(path.as_ref().to_path_buf());
        self
    }

    /// Set the path to the PEM-encoded private key file.
    #[must_use]
    pub fn key_file(mut self, path: impl AsRef<Path>) -> Self {
        self.key_path = Some(path.as_ref().to_path_buf());
        self
    }

    /// Set the client authentication mode.
    #[must_use]
    pub const fn client_auth(mut self, mode: ClientAuth) -> Self {
        self.client_auth = mode;
        self
    }

    /// Build the `TlsConfig`.
    ///
    /// Enforces TLS 1.3 only. Loads the certificate chain and private key
    /// from the configured PEM files.
    ///
    /// # Errors
    ///
    /// Returns `ProtocolError` if files are missing, malformed, mismatched,
    /// or if `ClientAuth::Required` is selected (mTLS not yet implemented).
    pub fn build(self) -> Result<TlsConfig> {
        if self.client_auth == ClientAuth::Required {
            return Err(ProtocolError::TlsConfig(
                "mTLS not yet implemented: ClientAuth::Required requires a CA \
                 certificate store for client verification"
                    .to_owned(),
            ));
        }

        let cert_path = self
            .cert_path
            .ok_or_else(|| ProtocolError::TlsConfig("certificate path not set".to_owned()))?;
        let key_path = self
            .key_path
            .ok_or_else(|| ProtocolError::TlsConfig("key path not set".to_owned()))?;

        let certs = load_certs(&cert_path)?;
        let key = load_private_key(&key_path)?;

        let builder = ServerConfig::builder_with_protocol_versions(&[&rustls::version::TLS13]);

        let config = builder
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .map_err(|e| ProtocolError::TlsConfig(format!("server config: {e}")))?;

        Ok(TlsConfig {
            inner: Arc::new(config),
        })
    }
}

impl Default for TlsConfigBuilder {
    fn default() -> Self {
        Self::new()
    }
}

fn load_certs(path: &Path) -> Result<Vec<rustls::pki_types::CertificateDer<'static>>> {
    let file = std::fs::File::open(path)
        .map_err(|e| ProtocolError::CertificateLoad(format!("{}: {e}", path.display())))?;
    let mut reader = std::io::BufReader::new(file);

    let certs: Vec<_> = rustls_pemfile::certs(&mut reader)
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|e| ProtocolError::CertificateLoad(format!("parse certs: {e}")))?;

    if certs.is_empty() {
        return Err(ProtocolError::CertificateLoad(
            "no certificates found in file".to_owned(),
        ));
    }

    Ok(certs)
}

fn load_private_key(path: &Path) -> Result<rustls::pki_types::PrivateKeyDer<'static>> {
    let file = std::fs::File::open(path)
        .map_err(|e| ProtocolError::KeyLoad(format!("{}: {e}", path.display())))?;
    let mut reader = std::io::BufReader::new(file);

    rustls_pemfile::private_key(&mut reader)
        .map_err(|e| ProtocolError::KeyLoad(format!("parse key: {e}")))?
        .ok_or_else(|| ProtocolError::KeyLoad("no private key found in file".to_owned()))
}
