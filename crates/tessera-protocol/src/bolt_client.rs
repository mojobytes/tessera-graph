// Copyright 2026 BelowZero Security OU. All rights reserved.

//! Bolt 4.4 client — performs the handshake and provides typed request/response
//! methods for the Bolt protocol.
//!
//! The client is generic over `AsyncRead`/`AsyncWrite` so it works with
//! `DuplexStream` (tests), `TcpStream`, or `TlsStream`.

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::bolt_frame::{BoltChunkedReader, BoltChunkedWriter};
use crate::bolt_handshake::BOLT_MAGIC;
use crate::bolt_message::{BoltRequest, BoltResponse, decode_response, encode_request};
use crate::error::ProtocolError;
use crate::packstream::PackStreamValue;

/// Result of a successful `run_query` call.
#[derive(Debug)]
pub struct QueryResult {
    /// Column names from the SUCCESS metadata `fields` list.
    pub columns: Vec<String>,
    /// Rows of `PackStreamValue` collected from RECORD responses.
    pub rows: Vec<Vec<PackStreamValue>>,
}

/// A Bolt 4.4 client connected to a server.
///
/// Use [`BoltClient::connect`] to perform the handshake on a full-duplex stream,
/// or [`BoltClient::connect_split`] on pre-split read/write halves.
pub struct BoltClient<R: AsyncRead + Unpin, W: AsyncWrite + Unpin> {
    reader: BoltChunkedReader<R>,
    writer: BoltChunkedWriter<W>,
}

impl<R, W> BoltClient<R, W>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    /// Perform the Bolt 4.4 handshake on pre-split read/write halves.
    ///
    /// Writes the 20-byte handshake (magic + version proposal), reads the
    /// 4-byte response, and validates the negotiated version.
    ///
    /// # Errors
    ///
    /// - [`ProtocolError::BoltInvalidHandshake`] if the server rejects all
    ///   proposed versions.
    /// - [`ProtocolError::Io`] on I/O failure.
    pub async fn connect_split(mut reader: R, mut writer: W) -> crate::Result<Self> {
        // Build handshake: 4-byte magic + 4 version proposals (16 bytes).
        let mut handshake = [0u8; 20];
        handshake[..4].copy_from_slice(&BOLT_MAGIC);
        // Proposal 1: Bolt 4.0–4.4  (major=4, range=4, minor=4)
        handshake[4..8].copy_from_slice(&0x0004_0404_u32.to_be_bytes());
        // Proposals 2–4: zeroes (no fallback)

        writer.write_all(&handshake).await?;
        writer.flush().await?;

        let mut resp = [0u8; 4];
        reader.read_exact(&mut resp).await?;

        // Expected: [0x00, 0x04, 0x04, 0x00] for Bolt 4.4
        if resp == [0x00, 0x00, 0x00, 0x00] {
            return Err(ProtocolError::BoltInvalidHandshake {
                reason: "server rejected all proposed Bolt versions",
            });
        }

        Ok(Self {
            reader: BoltChunkedReader::new(reader),
            writer: BoltChunkedWriter::new(writer),
        })
    }

    /// Send a `BoltRequest` to the server.
    ///
    /// # Errors
    ///
    /// Returns [`ProtocolError`] on encoding or I/O failure.
    pub async fn send_request(&mut self, request: &BoltRequest) -> crate::Result<()> {
        let data = encode_request(request)?;
        self.writer
            .write_message(&data)
            .await
            .map_err(ProtocolError::Io)
    }

    /// Read a `BoltResponse` from the server.
    ///
    /// # Errors
    ///
    /// - [`ProtocolError::Io`] with `UnexpectedEof` if the server closed the
    ///   connection.
    /// - [`ProtocolError`] on decoding failure.
    pub async fn recv_response(&mut self) -> crate::Result<BoltResponse> {
        let data = self
            .reader
            .read_message()
            .await?
            .ok_or_else(|| {
                ProtocolError::Io(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "server closed connection",
                ))
            })?;
        decode_response(&data)
    }

    /// Authenticate with the server via HELLO.
    ///
    /// # Errors
    ///
    /// - [`ProtocolError::BoltAuthFailure`] if the server responds with FAILURE.
    /// - [`ProtocolError::Io`] on I/O or protocol errors.
    pub async fn hello(
        &mut self,
        username: &str,
        password: &str,
        db: Option<&str>,
    ) -> crate::Result<()> {
        let mut extra = vec![
            (
                "principal".to_owned(),
                PackStreamValue::String(username.to_owned()),
            ),
            (
                "credentials".to_owned(),
                PackStreamValue::String(password.to_owned()),
            ),
        ];
        if let Some(db_name) = db {
            extra.push(("db".to_owned(), PackStreamValue::String(db_name.to_owned())));
        }

        self.send_request(&BoltRequest::Hello { extra }).await?;

        match self.recv_response().await? {
            BoltResponse::Success { .. } => Ok(()),
            BoltResponse::Failure { metadata } => {
                let message = extract_string(&metadata, "message")
                    .unwrap_or_else(|| "authentication failed".to_owned());
                Err(ProtocolError::BoltAuthFailure { message })
            }
            BoltResponse::Ignored => Err(ProtocolError::BoltInvalidHandshake {
                reason: "server sent IGNORED to HELLO",
            }),
            BoltResponse::Record { .. } => Err(ProtocolError::BoltInvalidHandshake {
                reason: "unexpected RECORD response to HELLO",
            }),
        }
    }

    /// Execute a query: sends RUN + PULL and collects all RECORD rows.
    ///
    /// # Errors
    ///
    /// - [`ProtocolError::BoltQueryFailure`] if the server responds with
    ///   FAILURE to RUN or PULL.
    /// - [`ProtocolError::Io`] on I/O or protocol errors.
    pub async fn run_query(&mut self, query: &str) -> crate::Result<QueryResult> {
        // --- RUN ---
        self.send_request(&BoltRequest::Run {
            query: query.to_owned(),
            params: vec![],
            extra: vec![],
        })
        .await?;

        let columns = match self.recv_response().await? {
            BoltResponse::Success { metadata } => extract_fields(&metadata),
            BoltResponse::Failure { metadata } => {
                let message = extract_string(&metadata, "message")
                    .unwrap_or_else(|| "query execution failed".to_owned());
                return Err(ProtocolError::BoltQueryFailure { message });
            }
            _ => Vec::new(),
        };

        // --- PULL ---
        self.send_request(&BoltRequest::Pull { extra: vec![] })
            .await?;

        let mut rows = Vec::new();
        loop {
            match self.recv_response().await? {
                BoltResponse::Record { fields } => rows.push(fields),
                BoltResponse::Failure { metadata } => {
                    let message = extract_string(&metadata, "message")
                        .unwrap_or_else(|| "pull failed".to_owned());
                    return Err(ProtocolError::BoltQueryFailure { message });
                }
                BoltResponse::Success { .. } | BoltResponse::Ignored => break,
            }
        }

        Ok(QueryResult { columns, rows })
    }

    /// Send RESET to clear the FAILED state.
    ///
    /// # Errors
    ///
    /// Returns [`ProtocolError`] on I/O or protocol errors.
    pub async fn reset(&mut self) -> crate::Result<()> {
        self.send_request(&BoltRequest::Reset).await?;
        // Consume the SUCCESS response.
        let _ = self.recv_response().await?;
        Ok(())
    }

    /// Send GOODBYE to close the connection gracefully.
    ///
    /// Does not wait for a response (the server closes the connection).
    ///
    /// # Errors
    ///
    /// Returns [`ProtocolError::Io`] if the send fails.
    pub async fn goodbye(&mut self) -> crate::Result<()> {
        self.send_request(&BoltRequest::Goodbye).await
    }
}

/// Perform the Bolt 4.4 handshake on a full-duplex stream.
///
/// The stream is split internally into read/write halves.
///
/// # Errors
///
/// Same as [`BoltClient::connect_split`].
pub async fn connect<S: AsyncRead + AsyncWrite + Unpin>(
    stream: S,
) -> crate::Result<BoltClient<tokio::io::ReadHalf<S>, tokio::io::WriteHalf<S>>> {
    let (reader, writer) = tokio::io::split(stream);
    BoltClient::connect_split(reader, writer).await
}

// ── Helpers ──────────────────────────────────────────────────────────────────

/// Extract a string value from a `BoltDict` (vec of key-value pairs).
fn extract_string(metadata: &[(String, PackStreamValue)], key: &str) -> Option<String> {
    metadata.iter().find_map(|(k, v)| {
        if k == key {
            if let PackStreamValue::String(s) = v {
                Some(s.clone())
            } else {
                None
            }
        } else {
            None
        }
    })
}

/// Extract the `fields` list from a SUCCESS metadata dict.
fn extract_fields(metadata: &[(String, PackStreamValue)]) -> Vec<String> {
    metadata
        .iter()
        .find_map(|(k, v)| {
            if k == "fields" {
                if let PackStreamValue::List(items) = v {
                    Some(
                        items
                            .iter()
                            .filter_map(|item| {
                                if let PackStreamValue::String(s) = item {
                                    Some(s.clone())
                                } else {
                                    None
                                }
                            })
                            .collect(),
                    )
                } else {
                    None
                }
            } else {
                None
            }
        })
        .unwrap_or_default()
}
