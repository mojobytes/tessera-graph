// SPDX-License-Identifier: BSL-1.1

//! v0.6.0 Fase 2 Task 5 eje 4 — cooperative per-connection bandwidth cap.
//!
//! [`RateLimited`] wraps any [`AsyncRead`]/[`AsyncWrite`] (the read/write
//! half of a Bolt connection's `TcpStream` or `TlsStream`) and meters the
//! bytes that flow through it against a shared [`TokenBucket`]. When the
//! bucket is short, the I/O is **slowed** with a cooperative
//! `tokio::time::sleep` rather than rejected — the spec §6 "tc-like"
//! behavior. The driver never sees a wire error; it just observes a
//! lower sustained throughput.
//!
//! ## Why the wrapper lives in the server crate
//!
//! [`TokenBucket`] lives in [`crate::rate_limiter`]. The `BoltChunkedReader`
//! / `BoltChunkedWriter` framing types are generic over any
//! `AsyncRead`/`AsyncWrite`, so the server wraps each split half with
//! `RateLimited` **before** handing it to the framing layer. This keeps the
//! `tessera-graph-protocol` crate free of any dependency on the server (the
//! dependency edge only goes server → protocol), avoiding a cycle.
//!
//! ## Shared bucket, shared counter
//!
//! The read half and the write half of one connection share a single
//! `Arc<Mutex<TokenBucket>>` (the cap is per-connection, counting both
//! directions, per spec §6.1) and a single `Arc<AtomicU64>` sleep counter.
//! The handler reads that counter on `Drop` to emit one aggregate
//! `BandwidthThrottled` audit event.
//!
//! ## Approximation
//!
//! Tokens are reserved for the *requested* byte count before the inner
//! `poll_write`/`poll_read` runs. If the inner I/O transfers fewer bytes,
//! the wrapper has charged slightly too much — never too little. For a
//! bandwidth cap this conservative bias is the safe direction and matches
//! the spec's "approximate, tc-style" intent.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::Instant;

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::time::Sleep;

use crate::rate_limiter::TokenBucket;

/// Shared state between the read and write halves of one connection.
///
/// `bucket` is the per-connection byte token bucket; `sleeps` counts how
/// many times either half had to sleep waiting for tokens (read by the
/// handler's `Drop` for the aggregate audit event).
#[derive(Clone)]
pub struct BandwidthLimiter {
    bucket: Arc<Mutex<TokenBucket>>,
    sleeps: Arc<AtomicU64>,
    /// Total nanoseconds slept across both halves, accumulated for the
    /// aggregate `BandwidthThrottled` audit event (spec §6.2).
    total_sleep_nanos: Arc<AtomicU64>,
    /// `true` when the cap is active (`> 0`). Cached so the hot path can
    /// skip the mutex entirely on the disabled (pass-through) case.
    active: bool,
}

impl BandwidthLimiter {
    /// Build a limiter for `max_bytes_per_second`. `0` is pass-through:
    /// the wrapper forwards I/O untouched and never sleeps.
    #[must_use]
    pub fn new(max_bytes_per_second: u64) -> Self {
        Self {
            bucket: Arc::new(Mutex::new(TokenBucket::new(
                max_bytes_per_second,
                std::time::Duration::from_secs(1),
            ))),
            sleeps: Arc::new(AtomicU64::new(0)),
            total_sleep_nanos: Arc::new(AtomicU64::new(0)),
            active: max_bytes_per_second > 0,
        }
    }

    /// Number of times a half had to sleep for tokens so far. Read by the
    /// handler on `Drop` to decide whether to emit `BandwidthThrottled`.
    #[must_use]
    pub fn sleep_count(&self) -> u64 {
        self.sleeps.load(Ordering::Relaxed)
    }

    /// Total time slept across both halves so far, in milliseconds.
    /// Read by the handler on `Drop` for the aggregate audit event.
    #[must_use]
    pub fn total_sleep_ms(&self) -> u64 {
        self.total_sleep_nanos.load(Ordering::Relaxed) / 1_000_000
    }

    /// Reserve `n` byte-tokens at `now`, returning the duration to sleep
    /// before the bytes may flow (`Duration::ZERO` when tokens were
    /// available or the cap is disabled). Increments the sleep counter
    /// and accumulates the slept duration when a non-zero wait is incurred.
    fn reserve(&self, n: u64) -> std::time::Duration {
        if !self.active {
            return std::time::Duration::ZERO;
        }
        let wait = {
            let mut bucket = self.bucket.lock().expect("BandwidthLimiter bucket poisoned");
            bucket.take(n, Instant::now())
        };
        if !wait.is_zero() {
            self.sleeps.fetch_add(1, Ordering::Relaxed);
            self.total_sleep_nanos.fetch_add(
                u64::try_from(wait.as_nanos()).unwrap_or(u64::MAX),
                Ordering::Relaxed,
            );
            crate::metrics::rate_limit_hit("bytes_conn");
        }
        wait
    }
}

/// An `AsyncRead`/`AsyncWrite` wrapper that meters bytes through a shared
/// [`BandwidthLimiter`], sleeping cooperatively when the bucket is short.
///
/// `I: Unpin` keeps every projection safe (no `unsafe`, consistent with
/// the crate's `forbid(unsafe_code)`).
pub struct RateLimited<I> {
    inner: I,
    limiter: BandwidthLimiter,
    /// A pending sleep created by the last short reservation. Polled to
    /// completion before the wrapped I/O is allowed to proceed.
    pending: Option<Pin<Box<Sleep>>>,
    /// `true` once tokens have been reserved for the *current* I/O attempt
    /// (the same `buf` the caller will re-present after a `Pending`). Guards
    /// against double-charging: `AsyncWriteExt::write_all` re-polls
    /// `poll_write` with the same buffer after every `Pending`, so without
    /// this flag each re-poll would reserve tokens again, compounding the
    /// sleep without bound. Cleared once the inner I/O actually runs.
    reserved: bool,
}

impl<I> RateLimited<I> {
    /// Wrap `inner`, sharing `limiter` with the connection's other half.
    pub fn new(inner: I, limiter: BandwidthLimiter) -> Self {
        Self {
            inner,
            limiter,
            pending: None,
            reserved: false,
        }
    }

    /// Reserve tokens for `requested` bytes (once per I/O attempt) and, if
    /// the bucket is short, drive the cooperative sleep to completion.
    ///
    /// Returns `Poll::Ready(())` when the bytes may now flow (tokens were
    /// available or the sleep elapsed), `Poll::Pending` while a sleep is
    /// still running. The reservation happens exactly once per attempt —
    /// re-polls after `Pending` resume the existing sleep rather than
    /// reserving again (see [`Self::reserved`]).
    fn poll_gate(&mut self, requested: u64, cx: &mut Context<'_>) -> Poll<()> {
        // Resume an in-flight sleep first.
        if let Some(sleep) = self.pending.as_mut() {
            match sleep.as_mut().poll(cx) {
                Poll::Ready(()) => self.pending = None,
                Poll::Pending => return Poll::Pending,
            }
        }
        // Reserve once per attempt.
        if !self.reserved {
            self.reserved = true;
            if requested > 0 {
                let wait = self.limiter.reserve(requested);
                if !wait.is_zero() {
                    let mut sleep = Box::pin(tokio::time::sleep(wait));
                    if sleep.as_mut().poll(cx).is_pending() {
                        self.pending = Some(sleep);
                        return Poll::Pending;
                    }
                }
            }
        }
        Poll::Ready(())
    }
}

impl<I: AsyncRead + Unpin> AsyncRead for RateLimited<I> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        if !this.limiter.active {
            return Pin::new(&mut this.inner).poll_read(cx, buf);
        }
        // Reserve tokens for the room the caller offered (the inner read may
        // fill less; charging for the offered capacity is the safe,
        // conservative bias) and wait out any short-token sleep.
        let requested = buf.remaining() as u64;
        if this.poll_gate(requested, cx).is_pending() {
            return Poll::Pending;
        }
        let result = Pin::new(&mut this.inner).poll_read(cx, buf);
        if result.is_ready() {
            this.reserved = false;
        }
        result
    }
}

impl<I: AsyncWrite + Unpin> AsyncWrite for RateLimited<I> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        if !this.limiter.active {
            return Pin::new(&mut this.inner).poll_write(cx, buf);
        }
        let requested = buf.len() as u64;
        if this.poll_gate(requested, cx).is_pending() {
            return Poll::Pending;
        }
        // Tokens for this attempt are reserved. Only release the
        // reservation flag once the inner write actually completes
        // (`Ready`); if the inner returns `Pending` (e.g. the peer's
        // receive buffer is full), the SAME bytes will be re-presented and
        // must NOT be charged again.
        let result = Pin::new(&mut this.inner).poll_write(cx, buf);
        if result.is_ready() {
            this.reserved = false;
        }
        result
    }

    fn poll_flush(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        Pin::new(&mut this.inner).poll_flush(cx)
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        Pin::new(&mut this.inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    /// Writing far more than the bucket capacity must take roughly
    /// `(total - capacity) / rate` seconds of cooperative sleeping. This
    /// isolates the wrapper from the handler/duplex/PackStream stack.
    #[tokio::test]
    async fn write_past_capacity_is_throttled() {
        // rate = 4096 B/s → capacity 8192. Write 24 KiB → ~ (24576-8192)/4096
        // ≈ 4s of sleeps.
        let limiter = BandwidthLimiter::new(4096);
        let sink = tokio::io::sink();
        let mut w = RateLimited::new(sink, limiter.clone());

        let start = tokio::time::Instant::now();
        let payload = vec![0u8; 24 * 1024];
        w.write_all(&payload).await.unwrap();
        w.flush().await.unwrap();
        let elapsed = start.elapsed();

        assert!(
            elapsed >= std::time::Duration::from_secs(3),
            "24 KiB at 4096 B/s (cap 8192) must take ≥3s, took {elapsed:?}"
        );
        assert!(
            limiter.sleep_count() > 0,
            "at least one cooperative sleep must have occurred"
        );
    }

    /// Many small writes (the handler's per-record pattern) must throttle
    /// just like one big write — the bucket meters total bytes regardless
    /// of how they are chunked. Reproduces the eje-4 E2E path without the
    /// handler. rate=512 → capacity 1024. 30 × 463 B ≈ 13.5 KiB →
    /// ~(13890-1024)/512 ≈ 25s; assert a conservative ≥3s floor.
    #[tokio::test]
    async fn many_small_writes_are_throttled() {
        let limiter = BandwidthLimiter::new(512);
        let mut w = RateLimited::new(tokio::io::sink(), limiter.clone());
        let start = tokio::time::Instant::now();
        let record = vec![0u8; 463];
        for _ in 0..30 {
            w.write_all(&record).await.unwrap();
        }
        let elapsed = start.elapsed();
        assert!(
            elapsed >= std::time::Duration::from_secs(3),
            "30 × 463 B at 512 B/s must throttle (≥3s), took {elapsed:?}"
        );
        assert!(
            limiter.sleep_count() > 0,
            "small-write path must incur cooperative sleeps, got 0"
        );
    }

    /// cap = 0 is a pass-through: no sleeps, no measurable delay.
    #[tokio::test]
    async fn cap_zero_is_passthrough() {
        let limiter = BandwidthLimiter::new(0);
        let mut w = RateLimited::new(tokio::io::sink(), limiter.clone());
        let start = tokio::time::Instant::now();
        w.write_all(&vec![0u8; 1024 * 1024]).await.unwrap();
        let elapsed = start.elapsed();
        assert!(
            elapsed < std::time::Duration::from_secs(1),
            "cap=0 must not throttle, took {elapsed:?}"
        );
        assert_eq!(limiter.sleep_count(), 0, "cap=0 must never sleep");
    }
}
