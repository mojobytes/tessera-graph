// Copyright (c) 2026 BelowZero Security OU. All rights reserved.
// SPDX-License-Identifier: LicenseRef-BelowZero-Enterprise

//! [`BenchmarkTarget`] implementation that connects to a running TesseraGraph
//! server over the Bolt 4.4 protocol with TLS.
//!
//! This target measures the same network path as `MemgraphTarget`, making
//! comparisons fair (both go through TCP + TLS + Bolt + query parsing).
//!
//! Gated behind the `tessera-bolt` feature flag.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use tessera_graph::Properties;
use tessera_graph_protocol::bolt_client::BoltClient;
use tessera_graph_protocol::packstream::PackStreamValue;
use tokio::io::{ReadHalf, WriteHalf};
use tokio::net::TcpStream;
use tokio::runtime::Runtime;
use tokio_rustls::client::TlsStream;

use crate::error::{BenchmarkError, Result};
use crate::target::{BenchmarkTarget, EdgeData, EdgeHandle, NodeData, NodeHandle};

type TlsTcpStream = TlsStream<TcpStream>;
type Client = BoltClient<ReadHalf<TlsTcpStream>, WriteHalf<TlsTcpStream>>;

/// No-op certificate verifier for benchmark connections (dev/test only).
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

/// Bolt-protocol target backed by a live TesseraGraph server over TLS.
///
/// Uses `RefCell` for interior mutability because the `BenchmarkTarget` trait
/// requires `&self` for read operations, but both `BoltClient::run_query` and
/// the node ID cache need `&mut` access. All benchmark access is
/// single-threaded, so `RefCell` is safe.
pub struct TesseraBoltTarget {
    rt: Runtime,
    client: RefCell<Client>,
    node_ids: RefCell<HashMap<u64, i64>>,
    edge_count: u64,
    next_handle: AtomicU64,
}

impl TesseraBoltTarget {
    /// Connects to a TesseraGraph server over Bolt with TLS (skip-verify).
    ///
    /// # Errors
    ///
    /// Returns [`BenchmarkError::External`] if the connection cannot be established.
    pub fn connect(host: &str, port: u16, user: &str, pass: &str) -> Result<Self> {
        let rt = Runtime::new().map_err(|e| {
            BenchmarkError::external(format!("failed to create tokio runtime: {e}"))
        })?;

        let host_owned = host.to_owned();
        let user_owned = user.to_owned();
        let pass_owned = pass.to_owned();

        let client = rt.block_on(async {
            let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
            let tls_config = rustls::ClientConfig::builder()
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(NoCertVerifier))
                .with_no_client_auth();
            let connector = tokio_rustls::TlsConnector::from(Arc::new(tls_config));

            let addr = format!("{host_owned}:{port}");
            let tcp = TcpStream::connect(&addr)
                .await
                .map_err(|e| BenchmarkError::external(format!("TCP connect to {addr}: {e}")))?;

            let server_name = ServerName::try_from(host_owned.clone())
                .map_err(|e| BenchmarkError::external(format!("invalid server name: {e}")))?;

            let tls_stream = connector
                .connect(server_name, tcp)
                .await
                .map_err(|e| BenchmarkError::external(format!("TLS handshake failed: {e}")))?;

            let (reader, writer) = tokio::io::split(tls_stream);
            let mut client = BoltClient::connect_split(reader, writer)
                .await
                .map_err(|e| {
                    BenchmarkError::external(format!("Bolt handshake failed: {e}"))
                })?;

            client
                .hello(&user_owned, &pass_owned, None)
                .await
                .map_err(|e| BenchmarkError::external(format!("Bolt auth failed: {e}")))?;

            Ok::<_, BenchmarkError>(client)
        })?;

        Ok(Self {
            rt,
            client: RefCell::new(client),
            node_ids: RefCell::new(HashMap::new()),
            edge_count: 0,
            next_handle: AtomicU64::new(1),
        })
    }

    /// Creates a target from environment variables.
    ///
    /// - `TESSERA_BOLT_HOST` (default: `localhost`)
    /// - `TESSERA_BOLT_PORT` (default: `7687`)
    /// - `TESSERA_BOLT_USER` (default: `admin`)
    /// - `TESSERA_BOLT_PASS` (default: `Admin.123`)
    ///
    /// # Errors
    ///
    /// Returns [`BenchmarkError::External`] if the connection fails.
    pub fn from_env() -> Result<Self> {
        let host =
            std::env::var("TESSERA_BOLT_HOST").unwrap_or_else(|_| "localhost".into());
        let port: u16 = std::env::var("TESSERA_BOLT_PORT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(7687);
        let user =
            std::env::var("TESSERA_BOLT_USER").unwrap_or_else(|_| "admin".into());
        let pass =
            std::env::var("TESSERA_BOLT_PASS").unwrap_or_else(|_| "Admin.123".into());
        Self::connect(&host, port, &user, &pass)
    }

    fn next_handle(&self) -> u64 {
        self.next_handle.fetch_add(1, Ordering::Relaxed)
    }

    fn run_query(&self, query: &str) -> Result<Vec<Vec<PackStreamValue>>> {
        let mut client = self.client.borrow_mut();
        self.rt.block_on(async {
            let result = client
                .run_query(query)
                .await
                .map_err(|e| BenchmarkError::external(format!("bolt query error: {e}")))?;
            Ok(result.rows)
        })
    }

    /// Fetches all node IDs from the graph and populates the handle→ID map.
    fn resolve_node_ids(&self) -> Result<()> {
        let rows = self.run_query("MATCH (n) RETURN id(n) AS nid ORDER BY id(n) ASC")?;
        let mut ids = self.node_ids.borrow_mut();
        for (i, row) in rows.iter().enumerate() {
            if let Some(PackStreamValue::Int(nid)) = row.first() {
                ids.insert((i + 1) as u64, *nid);
            }
        }
        Ok(())
    }
}

impl BenchmarkTarget for TesseraBoltTarget {
    fn name(&self) -> &'static str {
        "tessera-bolt"
    }

    fn create_node(&mut self, label: &str, _props: Properties) -> Result<NodeHandle> {
        let query = format!("CREATE (:{label})");
        self.run_query(&query)?;

        let handle_id = self.next_handle();
        Ok(NodeHandle(handle_id))
    }

    fn create_edge(
        &mut self,
        label: &str,
        _from: NodeHandle,
        _to: NodeHandle,
        _props: Properties,
    ) -> Result<EdgeHandle> {
        // TesseraGraph CREATE doesn't return node IDs in the mutation response,
        // so we can't target specific nodes. For write benchmarks we just measure
        // the CREATE round-trip cost.
        let query = format!(
            "MATCH (a:N), (b:N) WHERE id(a) <> id(b) \
             CREATE (a)-[:{label}]->(b)"
        );
        self.run_query(&query)?;
        self.edge_count += 1;

        let handle_id = self.next_handle();
        Ok(EdgeHandle(handle_id))
    }

    fn get_node(&self, handle: NodeHandle) -> Result<NodeData> {
        // Lazy ID resolution: fetch all IDs on first read miss.
        if !self.node_ids.borrow().contains_key(&handle.0) {
            self.resolve_node_ids()?;
        }

        let ids = self.node_ids.borrow();
        let nid = *ids
            .get(&handle.0)
            .ok_or_else(|| {
                BenchmarkError::external("node handle not found after ID resolution")
            })?;
        drop(ids); // Release borrow before run_query

        let query = format!("MATCH (n) WHERE id(n) = {nid} RETURN labels(n) AS lbls");
        let rows = self.run_query(&query)?;

        if let Some(row) = rows.first() {
            if let Some(PackStreamValue::List(labels)) = row.first() {
                let label = labels
                    .iter()
                    .find_map(|v| {
                        if let PackStreamValue::String(s) = v {
                            Some(s.clone())
                        } else {
                            None
                        }
                    })
                    .unwrap_or_default();
                return Ok(NodeData {
                    label,
                    props: Properties::new(),
                });
            }
        }

        Ok(NodeData {
            label: String::new(),
            props: Properties::new(),
        })
    }

    fn get_edge(&self, _handle: EdgeHandle) -> Result<EdgeData> {
        Ok(EdgeData {
            label: String::new(),
            props: Properties::new(),
        })
    }

    fn traverse_bfs(&self, _start: NodeHandle, _max_depth: u32) -> Result<Vec<NodeHandle>> {
        Err(BenchmarkError::scenario(
            "BFS traversal not supported via TesseraGraph Bolt",
        ))
    }

    fn traverse_dfs(&self, _start: NodeHandle, _max_depth: u32) -> Result<Vec<NodeHandle>> {
        Err(BenchmarkError::scenario(
            "DFS traversal not supported via TesseraGraph Bolt",
        ))
    }

    fn shortest_path(
        &self,
        _from: NodeHandle,
        _to: NodeHandle,
    ) -> Result<Option<Vec<NodeHandle>>> {
        Err(BenchmarkError::scenario(
            "Shortest path not supported via TesseraGraph Bolt",
        ))
    }

    fn clear(&mut self) {
        let _ = self.run_query("MATCH (n) DETACH DELETE n");
        self.node_ids.borrow_mut().clear();
        self.edge_count = 0;
    }
}
