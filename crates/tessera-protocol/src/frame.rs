// Copyright 2026 BelowZero Security OU. All rights reserved.

//! Length-prefixed binary frame codec for the `TesseraGraph` wire protocol.
//!
//! Wire format: `[u32 big-endian length][payload bytes]`
//! Maximum frame size: 16 MiB.

use bytes::{Buf, BytesMut};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::error::{ProtocolError, Result};

/// Maximum payload size in bytes (16 MiB).
pub const MAX_FRAME_SIZE: u32 = 16 * 1024 * 1024;

/// Header size: 4 bytes for the u32 length prefix.
const HEADER_SIZE: usize = 4;

/// Encode a payload into a length-prefixed frame.
///
/// # Errors
///
/// Returns `ProtocolError::FrameTooLarge` if `payload.len()` exceeds [`MAX_FRAME_SIZE`].
pub fn encode(payload: &[u8]) -> Result<Vec<u8>> {
    let len = u32::try_from(payload.len())
        .map_err(|_| ProtocolError::FrameTooLarge { declared: u32::MAX })?;
    if len > MAX_FRAME_SIZE {
        return Err(ProtocolError::FrameTooLarge { declared: len });
    }
    let mut buf = Vec::with_capacity(HEADER_SIZE + payload.len());
    buf.extend_from_slice(&len.to_be_bytes());
    buf.extend_from_slice(payload);
    Ok(buf)
}

/// Attempt to decode a single frame from a buffer.
///
/// Returns `Ok(Some(payload))` if a complete frame is available, advancing the
/// buffer past the consumed bytes. Returns `Ok(None)` if there is not enough
/// data yet. Returns `Err` if the declared length exceeds [`MAX_FRAME_SIZE`].
///
/// # Errors
///
/// Returns `ProtocolError::FrameTooLarge` if the frame exceeds the maximum size.
pub fn decode(buf: &mut BytesMut) -> Result<Option<Vec<u8>>> {
    if buf.len() < HEADER_SIZE {
        return Ok(None);
    }

    let length = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]);

    if length > MAX_FRAME_SIZE {
        return Err(ProtocolError::FrameTooLarge { declared: length });
    }

    let total = HEADER_SIZE + length as usize;
    if buf.len() < total {
        return Ok(None);
    }

    buf.advance(HEADER_SIZE);
    let payload = buf.split_to(length as usize).to_vec();
    Ok(Some(payload))
}

/// Async framed reader. Reads length-prefixed frames from an `AsyncRead` stream.
pub struct FramedReader<R> {
    reader: R,
    buf: BytesMut,
}

impl<R: AsyncRead + Unpin> FramedReader<R> {
    /// Create a new framed reader wrapping the given stream.
    #[must_use]
    pub fn new(reader: R) -> Self {
        Self {
            reader,
            buf: BytesMut::with_capacity(4096),
        }
    }

    /// Read a single frame from the stream.
    ///
    /// Returns `Ok(Some(payload))` on success, `Ok(None)` on clean EOF.
    ///
    /// # Errors
    ///
    /// Returns `ProtocolError::FrameTooLarge` or `ProtocolError::Io` on failure.
    pub async fn read_frame(&mut self) -> Result<Option<Vec<u8>>> {
        loop {
            if let Some(payload) = decode(&mut self.buf)? {
                return Ok(Some(payload));
            }

            let n = self.reader.read_buf(&mut self.buf).await?;
            if n == 0 {
                return if self.buf.is_empty() {
                    Ok(None)
                } else {
                    Err(ProtocolError::Io(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "connection closed mid-frame",
                    )))
                };
            }
        }
    }
}

/// Async framed writer. Writes length-prefixed frames to an `AsyncWrite` stream.
pub struct FramedWriter<W> {
    writer: W,
}

impl<W: AsyncWrite + Unpin> FramedWriter<W> {
    /// Create a new framed writer wrapping the given stream.
    #[must_use]
    pub const fn new(writer: W) -> Self {
        Self { writer }
    }

    /// Write a single frame to the stream.
    ///
    /// # Errors
    ///
    /// Returns `ProtocolError::Io` on write failure.
    pub async fn write_frame(&mut self, payload: &[u8]) -> Result<()> {
        let frame = encode(payload)?;
        self.writer.write_all(&frame).await?;
        self.writer.flush().await?;
        Ok(())
    }
}
