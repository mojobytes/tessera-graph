// Copyright 2026 BelowZero Security OU. All rights reserved.

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

    /// Write a complete message as one or more chunks followed by a zero terminator.
    ///
    /// An empty `data` slice writes only the zero terminator (`[0x00, 0x00]`).
    ///
    /// # Errors
    ///
    /// Returns `io::Error` if any underlying write fails.
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
}

impl<R: AsyncRead + Unpin> BoltChunkedReader<R> {
    /// Create a new [`BoltChunkedReader`] wrapping `inner`.
    #[must_use]
    pub const fn new(inner: R) -> Self {
        Self { inner }
    }

    /// Read chunks until a zero-length terminator, then return the reassembled message.
    ///
    /// - Returns `Ok(None)` on clean EOF (connection closed before any chunk header).
    /// - Returns `Err(io::ErrorKind::UnexpectedEof)` if EOF occurs mid-message.
    ///
    /// # Errors
    ///
    /// Returns `io::Error` on any read failure or unexpected EOF inside a message.
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

            // Read exactly chunk_size bytes.
            let prev_len = buf.len();
            buf.resize(prev_len + chunk_size, 0);
            self.inner.read_exact(&mut buf[prev_len..]).await?;

            first_chunk = false;
        }
    }
}
