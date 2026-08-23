// SPDX-License-Identifier: BSL-1.1

//! Bolt 4.4 client — performs the handshake and provides typed request/response
//! methods for the Bolt protocol.
//!
//! The client is generic over `AsyncRead`/`AsyncWrite` so it works with
//! `DuplexStream` (tests), `TcpStream`, or `TlsStream`.

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::bolt_frame::{BoltChunkedReader, BoltChunkedWriter};
use crate::bolt_handshake::{
    BOLT_MAGIC, NO_VERSION_RESPONSE, SUPPORTED_VERSION, parse_version_proposal,
};
use crate::bolt_message::{BoltRequest, BoltResponse, decode_response, encode_request};
use crate::error::ProtocolError;
use crate::packstream::PackStreamValue;

/// Version proposal sent by the client during the handshake.
///
/// Wire format (big-endian u32): `[padding=0, range, minor, major]`.
/// With `range=4` we propose support for minor versions
/// `SUPPORTED_VERSION.minor - 4` through `SUPPORTED_VERSION.minor`.
///
/// Only one proposal slot is populated — the remaining three are zeroed.
/// This is intentional: we support a single version family (Bolt 4.x) and
/// do not offer fallback to older major versions. If future compatibility
/// requires fallback proposals, populate slots 2–4 here.
const CLIENT_PROPOSAL: [u8; 4] = [
    0x00,                    // padding
    4,                       // range
    SUPPORTED_VERSION.minor, // minor
    SUPPORTED_VERSION.major, // major
];

/// Result of draining a pipelined batch of RUN+PULL pairs.
#[derive(Debug)]
pub struct PipelineResult {
    /// Number of statements that executed successfully.
    pub success_count: usize,
    /// Number of statements that failed (FAILURE + recovered via RESET).
    pub failure_count: usize,
}

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
    /// Database name to inject as `extra["db"]` on every RUN. Set by
    /// [`Self::hello`] when the caller passes a non-`None` `db` argument
    /// (Task 10-bis: routing moved from HELLO to RUN/BEGIN per Bolt 4.x).
    /// `None` means RUNs go without a `db` field — appropriate for
    /// single-database (legacy) servers; against a multi-database
    /// server the first RUN will be rejected with `DatabaseNotFound`.
    current_db: Option<String>,
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
        handshake[4..8].copy_from_slice(&CLIENT_PROPOSAL);
        // Proposals 2–4 are intentionally zeroed (no fallback). See CLIENT_PROPOSAL doc.

        writer.write_all(&handshake).await?;
        writer.flush().await?;

        let mut resp = [0u8; 4];
        reader.read_exact(&mut resp).await?;

        if resp == NO_VERSION_RESPONSE {
            return Err(ProtocolError::BoltInvalidHandshake {
                reason: "server rejected all proposed Bolt versions",
            });
        }

        // Validate the negotiated version matches what we support.
        let (major, minor, _range, _padding) = parse_version_proposal(u32::from_be_bytes(resp));
        if major != SUPPORTED_VERSION.major || minor != SUPPORTED_VERSION.minor {
            return Err(ProtocolError::BoltInvalidHandshake {
                reason: "server negotiated an unsupported Bolt version",
            });
        }

        Ok(Self {
            reader: BoltChunkedReader::new(reader),
            writer: BoltChunkedWriter::new(writer),
            current_db: None,
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
        let data = self.reader.read_message().await?.ok_or_else(|| {
            ProtocolError::Io(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "server closed connection",
            ))
        })?;
        decode_response(&data)
    }

    /// Authenticate with the server via HELLO and remember `db` so it
    /// gets injected as `extra["db"]` on every subsequent RUN.
    ///
    /// HELLO itself carries only `principal` + `credentials`. The
    /// target database is declared on the first RUN (Bolt 4.x/5.x
    /// contract — Task 10-bis routing rewire). Passing `db: Some(name)`
    /// here is shorthand for "every RUN should target `name`"; pass
    /// `None` for single-database (legacy) servers, where the field
    /// is not consulted.
    ///
    /// # Errors
    ///
    /// - [`ProtocolError::BoltAuthFailure`] if the server responds with FAILURE.
    /// - [`ProtocolError::BoltUnexpectedResponse`] if the server responds with
    ///   IGNORED or RECORD (connection state error, not a handshake issue).
    /// - [`ProtocolError::Io`] on I/O or protocol errors.
    pub async fn hello(
        &mut self,
        username: &str,
        password: &str,
        db: Option<&str>,
    ) -> crate::Result<()> {
        let extra = vec![
            (
                "principal".to_owned(),
                PackStreamValue::String(username.to_owned()),
            ),
            (
                "credentials".to_owned(),
                PackStreamValue::String(password.to_owned()),
            ),
        ];

        self.send_request(&BoltRequest::Hello { extra }).await?;

        match self.recv_response().await? {
            BoltResponse::Success { .. } => {
                self.current_db = db.map(str::to_owned);
                Ok(())
            }
            BoltResponse::Failure { metadata } => {
                let message = extract_string(&metadata, "message")
                    .unwrap_or_else(|| "authentication failed".to_owned());
                Err(ProtocolError::BoltAuthFailure { message })
            }
            BoltResponse::Ignored => Err(ProtocolError::BoltUnexpectedResponse {
                request: "HELLO",
                got: "IGNORED",
            }),
            BoltResponse::Record { .. } => Err(ProtocolError::BoltUnexpectedResponse {
                request: "HELLO",
                got: "RECORD",
            }),
        }
    }

    /// Execute a query: sends RUN + PULL and collects all RECORD rows.
    ///
    /// # Errors
    ///
    /// - [`ProtocolError::BoltQueryFailure`] if the server responds with
    ///   FAILURE to RUN or PULL, or sends an unexpected RECORD to RUN.
    /// - [`ProtocolError::BoltConnectionIgnored`] if the server responds with
    ///   IGNORED to RUN or PULL (connection is in FAILED state — send RESET
    ///   to recover).
    /// - [`ProtocolError::Io`] on I/O or protocol errors.
    pub async fn run_query(&mut self, query: &str) -> crate::Result<QueryResult> {
        // --- RUN ---
        self.send_request(&BoltRequest::Run {
            query: query.to_owned(),
            params: vec![],
            extra: self.run_extra(),
        })
        .await?;

        let columns = match self.recv_response().await? {
            BoltResponse::Success { metadata } => extract_fields(&metadata),
            BoltResponse::Failure { metadata } => {
                let message = extract_string(&metadata, "message")
                    .unwrap_or_else(|| "query execution failed".to_owned());
                return Err(ProtocolError::BoltQueryFailure { message });
            }
            BoltResponse::Ignored => {
                return Err(ProtocolError::BoltConnectionIgnored);
            }
            BoltResponse::Record { .. } => {
                return Err(ProtocolError::BoltQueryFailure {
                    message: "unexpected RECORD response to RUN".to_owned(),
                });
            }
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
                BoltResponse::Ignored => {
                    return Err(ProtocolError::BoltConnectionIgnored);
                }
                BoltResponse::Success { .. } => break,
            }
        }

        Ok(QueryResult { columns, rows })
    }

    /// Send RESET to clear the FAILED state.
    ///
    /// # Errors
    ///
    /// - [`ProtocolError::BoltResetFailure`] if the server responds with
    ///   FAILURE.
    /// - [`ProtocolError::BoltConnectionIgnored`] if the server responds with
    ///   IGNORED (unexpected — RESET should always be accepted).
    /// - [`ProtocolError::BoltUnexpectedResponse`] if the server responds with
    ///   RECORD (protocol violation).
    /// - [`ProtocolError::Io`] on I/O or protocol errors.
    pub async fn reset(&mut self) -> crate::Result<()> {
        self.send_request(&BoltRequest::Reset).await?;
        match self.recv_response().await? {
            BoltResponse::Success { .. } => Ok(()),
            BoltResponse::Failure { metadata } => {
                let message = extract_string(&metadata, "message")
                    .unwrap_or_else(|| "RESET failed".to_owned());
                Err(ProtocolError::BoltResetFailure { message })
            }
            BoltResponse::Ignored => Err(ProtocolError::BoltConnectionIgnored),
            BoltResponse::Record { .. } => Err(ProtocolError::BoltUnexpectedResponse {
                request: "RESET",
                got: "RECORD",
            }),
        }
    }

    // ── Pipelining ───────────────────────────────────────────────────

    /// Queue a RUN + PULL pair into the write buffer **without flushing**.
    ///
    /// Call [`flush_pipeline`](Self::flush_pipeline) after queuing a batch
    /// to push all messages in one syscall, then
    /// [`drain_pipeline`](Self::drain_pipeline) to read the responses.
    ///
    /// # Errors
    ///
    /// Returns [`ProtocolError::Io`] on write failure.
    pub async fn pipeline_run(&mut self, query: &str) -> crate::Result<()> {
        let run_data = encode_request(&BoltRequest::Run {
            query: query.to_owned(),
            params: vec![],
            extra: self.run_extra(),
        })?;
        self.writer
            .write_message_no_flush(&run_data)
            .await
            .map_err(ProtocolError::Io)?;

        let pull_data = encode_request(&BoltRequest::Pull { extra: vec![] })?;
        self.writer
            .write_message_no_flush(&pull_data)
            .await
            .map_err(ProtocolError::Io)?;

        Ok(())
    }

    /// Flush all queued pipeline messages to the server.
    ///
    /// # Errors
    ///
    /// Returns [`ProtocolError::Io`] on flush failure.
    pub async fn flush_pipeline(&mut self) -> crate::Result<()> {
        self.writer.flush().await.map_err(ProtocolError::Io)
    }

    /// Drain responses for `count` pipelined RUN+PULL pairs.
    ///
    /// For each pair, reads:
    /// 1. RUN response (SUCCESS or FAILURE)
    /// 2. PULL responses (RECORD\* + SUCCESS, or nothing on FAILURE)
    ///
    /// Returns the number of successful statements and the number of failures.
    /// On FAILURE the connection enters FAILED state — subsequent pipeline
    /// responses will be IGNORED.  This method sends RESET to recover and
    /// continues draining.
    ///
    /// # Errors
    ///
    /// Returns [`ProtocolError`] only on unrecoverable I/O or protocol errors.
    pub async fn drain_pipeline(&mut self, count: usize) -> crate::Result<PipelineResult> {
        let mut success_count = 0usize;
        let mut failure_count = 0usize;
        let mut in_failed_state = false;

        for _ in 0..count {
            if in_failed_state {
                // After a FAILURE, the server responds IGNORED to subsequent
                // messages until we send RESET.
                // Drain the IGNORED responses for this RUN+PULL pair.
                let _run_resp = self.recv_response().await?;
                let _pull_resp = self.recv_response().await?;
                // Send RESET to recover
                self.send_request(&BoltRequest::Reset).await?;
                match self.recv_response().await? {
                    BoltResponse::Success { .. } => {
                        in_failed_state = false;
                    }
                    _ => {
                        // RESET itself failed — unrecoverable
                        return Err(ProtocolError::BoltResetFailure {
                            message: "RESET failed during pipeline drain".to_owned(),
                        });
                    }
                }
                failure_count += 1;
                continue;
            }

            // Read RUN response
            match self.recv_response().await? {
                BoltResponse::Success { .. } => {
                    // RUN succeeded — now drain PULL responses
                    loop {
                        match self.recv_response().await? {
                            BoltResponse::Record { .. } => {}
                            BoltResponse::Success { .. } => break,
                            BoltResponse::Failure { .. } | BoltResponse::Ignored => {
                                in_failed_state = true;
                                break;
                            }
                        }
                    }
                    if in_failed_state {
                        failure_count += 1;
                    } else {
                        success_count += 1;
                    }
                }
                BoltResponse::Failure { .. } => {
                    // RUN failed — PULL will get IGNORED
                    let _pull_resp = self.recv_response().await?;
                    in_failed_state = true;
                    failure_count += 1;
                }
                BoltResponse::Ignored => {
                    // Should not happen at start, but handle gracefully
                    let _pull_resp = self.recv_response().await?;
                    in_failed_state = true;
                    failure_count += 1;
                }
                BoltResponse::Record { .. } => {
                    return Err(ProtocolError::BoltQueryFailure {
                        message: "unexpected RECORD response to RUN".to_owned(),
                    });
                }
            }
        }

        Ok(PipelineResult {
            success_count,
            failure_count,
        })
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

    /// Build the `extra` map for a RUN, injecting `("db", current_db)`
    /// when the session was authenticated against a multi-database
    /// server. Empty for legacy single-database servers (where
    /// [`Self::hello`] was called with `db = None`).
    fn run_extra(&self) -> Vec<(String, PackStreamValue)> {
        self.current_db.as_ref().map_or_else(Vec::new, |name| {
            vec![("db".to_owned(), PackStreamValue::String(name.clone()))]
        })
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
        if k != key {
            return None;
        }
        if let PackStreamValue::String(s) = v {
            Some(s.clone())
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
            if k != "fields" {
                return None;
            }
            if let PackStreamValue::List(items) = v {
                Some(items)
            } else {
                None
            }
        })
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    if let PackStreamValue::String(s) = item {
                        Some(s.clone())
                    } else {
                        None
                    }
                })
                .collect()
        })
        .unwrap_or_default()
}
