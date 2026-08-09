// SPDX-License-Identifier: BSL-1.1

//! Bolt chunked transport framing for Neo4j Bolt protocol compatibility.
//!
//! Wire format per message:
//! ```text
//! [chunk_size: u16 BE][chunk_data] ... [0x00 0x00]
//! ```
//!
//! - Each chunk carries a 2-byte big-endian length header (max 65 535 bytes payload).
//! - A zero-length chunk (`0x00 0x00`) terminates the message.
//! - Messages larger than 65 535 bytes are split into multiple chunks.

use std::io;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Maximum chunk payload size in bytes (`u16::MAX`).
pub const MAX_CHUNK_SIZE: usize = 65_535;

/// Default upper bound on a single reassembled Bolt message (64 MiB).
///
/// This prevents a malicious peer from causing unbounded memory growth by
/// sending an endless stream of non-terminating chunks.
pub const MAX_BOLT_MESSAGE_SIZE: usize = 64 * 1024 * 1024;

// ── Writer ───────────────────────────────────────────────────────────────────

/// Writes Bolt-framed messages as chunks over an async writer.
pub struct BoltChunkedWriter<W: AsyncWrite + Unpin> {
    inner: W,
}

impl<W: AsyncWrite + Unpin> BoltChunkedWriter<W> {
    /// Create a new [`BoltChunkedWriter`] wrapping `inner`.
    #[must_use]
    pub const fn new(inner: W) -> Self {
        Self { inner }
    }

    /// Write a complete message as one or more chunks followed by a zero terminator,
    /// then flush the underlying writer.
    ///
    /// An empty `data` slice writes only the zero terminator (`[0x00, 0x00]`).
    ///
    /// # Errors
    ///
    /// Returns `io::Error` if any underlying write or flush fails.
    pub async fn write_message(&mut self, data: &[u8]) -> io::Result<()> {
        // Split into at most MAX_CHUNK_SIZE-byte slices and write each as a chunk.
        // When data is empty this iterator yields no items, which is correct —
        // we still write the zero terminator below.
        let mut offset = 0;
        loop {
            if offset >= data.len() {
                break;
            }
            let end = (offset + MAX_CHUNK_SIZE).min(data.len());
            let chunk = &data[offset..end];
            // Safety: chunk.len() <= MAX_CHUNK_SIZE <= u16::MAX, cast is lossless.
            #[allow(clippy::cast_possible_truncation)]
            let header = (chunk.len() as u16).to_be_bytes();
            self.inner.write_all(&header).await?;
            self.inner.write_all(chunk).await?;
            offset = end;
        }

        // Zero terminator — always present, even for empty messages.
        self.inner.write_all(&[0x00, 0x00]).await?;
        // Flush ensures the entire message (including terminator) is pushed to
        // the OS send buffer before returning to the caller.
        self.inner.flush().await?;
        Ok(())
    }

    /// Write a complete message without flushing.
    ///
    /// Use this for pipelining: queue multiple messages in the write buffer,
    /// then call [`flush`](Self::flush) once to push them all in a single
    /// syscall.  Reduces round-trips from N to 1 for N pipelined messages.
    ///
    /// # Errors
    ///
    /// Returns `io::Error` if any underlying write fails.
    pub async fn write_message_no_flush(&mut self, data: &[u8]) -> io::Result<()> {
        let mut offset = 0;
        loop {
            if offset >= data.len() {
                break;
            }
            let end = (offset + MAX_CHUNK_SIZE).min(data.len());
            let chunk = &data[offset..end];
            #[allow(clippy::cast_possible_truncation)]
            let header = (chunk.len() as u16).to_be_bytes();
            self.inner.write_all(&header).await?;
            self.inner.write_all(chunk).await?;
            offset = end;
        }
        self.inner.write_all(&[0x00, 0x00]).await?;
        Ok(())
    }

    /// Flush the underlying writer.
    ///
    /// # Errors
    ///
    /// Returns `io::Error` if the flush fails.
    pub async fn flush(&mut self) -> io::Result<()> {
        self.inner.flush().await
    }
}

// ── Reader ───────────────────────────────────────────────────────────────────

/// Reads Bolt-framed messages by reassembling chunks from an async reader.
pub struct BoltChunkedReader<R: AsyncRead + Unpin> {
    inner: R,
    max_message_size: usize,
}

impl<R: AsyncRead + Unpin> BoltChunkedReader<R> {
    /// Create a new [`BoltChunkedReader`] wrapping `inner`.
    ///
    /// The reader enforces [`MAX_BOLT_MESSAGE_SIZE`] by default. Use
    /// [`Self::with_max_message_size`] to override the limit.
    #[must_use]
    pub const fn new(inner: R) -> Self {
        Self {
            inner,
            max_message_size: MAX_BOLT_MESSAGE_SIZE,
        }
    }

    /// Override the maximum reassembled message size.
    ///
    /// If a message's total accumulated bytes would exceed `max`, reading is
    /// aborted with `io::ErrorKind::InvalidData`. This prevents a malicious
    /// peer from causing unbounded memory growth.
    #[must_use]
    pub const fn with_max_message_size(mut self, max: usize) -> Self {
        self.max_message_size = max;
        self
    }

    /// Read chunks until a zero-length terminator, then return the reassembled message.
    ///
    /// - Returns `Ok(None)` on clean EOF (connection closed before any chunk header).
    /// - Returns `Err(io::ErrorKind::UnexpectedEof)` if EOF occurs mid-message.
    /// - Returns `Err(io::ErrorKind::InvalidData)` if the message exceeds
    ///   the configured maximum size (see [`Self::with_max_message_size`]).
    ///
    /// # Errors
    ///
    /// Returns `io::Error` on any read failure, unexpected EOF inside a message,
    /// or when the assembled message would exceed the size limit.
    pub async fn read_message(&mut self) -> io::Result<Option<Vec<u8>>> {
        let mut buf: Vec<u8> = Vec::new();
        let mut first_chunk = true;

        loop {
            // Read the 2-byte chunk header.
            let mut header = [0u8; 2];
            match self.inner.read_exact(&mut header).await {
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                    if first_chunk {
                        // Clean EOF before any data — connection closed gracefully.
                        return Ok(None);
                    }
                    // EOF in the middle of a message.
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "EOF mid-message in Bolt chunked framing",
                    ));
                }
                Err(e) => return Err(e),
            }

            let chunk_size = u16::from_be_bytes(header) as usize;

            if chunk_size == 0 {
                // Zero terminator — message is complete.
                return Ok(Some(buf));
            }

            // Guard against unbounded memory growth: reject messages that
            // exceed the configured limit before allocating more space.
            if buf.len() + chunk_size > self.max_message_size {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "Bolt message exceeds maximum allowed size of {} bytes",
                        self.max_message_size
                    ),
                ));
            }

            // Read exactly chunk_size bytes.
            let prev_len = buf.len();
            buf.resize(prev_len + chunk_size, 0);
            self.inner.read_exact(&mut buf[prev_len..]).await?;

            first_chunk = false;
        }
    }
}
