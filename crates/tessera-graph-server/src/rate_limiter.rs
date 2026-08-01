// SPDX-License-Identifier: BSL-1.1

//! Rate-limiting primitives for the `TesseraGraph` server.
//!
//! # Overview
//!
//! This module provides three independent primitives that are combined
//! into [`RateLimiter`], the per-server singleton:
//!
//! - [`SlidingWindow`]: count-within-duration gate (auth failure tracking).
//! - [`TokenBucket`]: burst-tolerant throughput limiter (query rate, bytes).
//! - [`RateLimiter`]: global store keyed by peer IP with LRU eviction.
//!
//! All timing-sensitive paths accept an [`Instant`] from a [`Clock`]
//! so that unit tests can inject a [`MockClock`] and control time
//! deterministically without `thread::sleep`.
//!
//! ## cap = 0 contract
//!
//! For every primitive, `cap = 0` is a pass-through: all operations
//! succeed unconditionally. This mirrors the [`crate::audit::SlowQueryGate`]
//! convention already established in the codebase.
//!
//! ## Drop safety
//!
//! [`ConnectionGuard`] must decrement a connection counter in its [`Drop`]
//! impl. `Drop` is synchronous; calling `tokio::sync::RwLock::blocking_write`
//! from an async context panics on a single-threaded runtime. To avoid this,
//! connection counts are stored in a dedicated `std::sync::Mutex`-backed map
//! (`conn_counts`) that is always safe to lock from `Drop`. Auth windows
//! remain in the `tokio::sync::RwLock<AuthStore>` because they are only
//! accessed from async code.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::RwLock;

// ── Clock abstraction ────────────────────────────────────────────────────────

/// A source of wall-clock [`Instant`]s.
///
/// The trait is intentionally minimal — callers only need `now()`.
/// Production code uses [`SystemClock`]; tests inject [`MockClock`].
pub trait Clock: Send + Sync + 'static {
    /// Returns the current instant according to this clock.
    fn now(&self) -> Instant;
}

/// The real system clock. Thin wrapper over [`Instant::now`].
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    #[inline]
    fn now(&self) -> Instant {
        Instant::now()
    }
}

/// A manually-advanced clock for deterministic tests.
///
/// Constructed at a fixed base instant; `advance` moves it forward by
/// any duration without involving wall time. Interior mutability via
/// `std::sync::Mutex` so that shared `Arc<MockClock>` can be advanced
/// from one place while being read through the [`Clock`] trait from
/// another.
///
/// Always compiled and exported so that integration test crates in the
/// same workspace can import it without activating a feature flag.
/// Production code never constructs this type.
#[derive(Debug)]
pub struct MockClock {
    current: std::sync::Mutex<Instant>,
}

impl MockClock {
    /// Creates a new clock anchored at the current wall-clock instant.
    ///
    /// The anchor is captured once at construction; all subsequent
    /// `now()` calls return the anchor plus accumulated advances.
    #[must_use]
    pub fn new() -> Self {
        Self {
            current: std::sync::Mutex::new(Instant::now()),
        }
    }

    /// Moves the clock forward by `duration`.
    ///
    /// # Panics
    ///
    /// Panics if the internal mutex is poisoned (a test panic occurred
    /// while holding the lock).
    pub fn advance(&self, duration: Duration) {
        let mut guard = self.current.lock().expect("MockClock mutex poisoned");
        *guard += duration;
    }
}

impl Clock for MockClock {
    fn now(&self) -> Instant {
        *self.current.lock().expect("MockClock mutex poisoned")
    }
}

impl Default for MockClock {
    fn default() -> Self {
        Self::new()
    }
}

// ── SlidingWindow ────────────────────────────────────────────────────────────

/// A sliding-window event counter.
///
/// Tracks individual event timestamps in a [`VecDeque`] so that the
/// effective rate is computed exactly — no bucketing artifacts.
///
/// `cap = 0` disables the gate (every `try_add` returns `true`).
#[derive(Debug)]
pub struct SlidingWindow {
    pub(crate) cap: u32,
    window: Duration,
    /// Timestamps of the events still within the current window.
    timestamps: VecDeque<Instant>,
}

impl SlidingWindow {
    /// Creates a new window with the given capacity and duration.
    ///
    /// - `cap = 0` → pass-through (always admits).
    /// - `cap > 0` → at most `cap` events per `window`.
    #[must_use]
    pub fn new(cap: u32, window: Duration) -> Self {
        Self {
            cap,
            window,
            timestamps: VecDeque::new(),
        }
    }

    /// Attempts to record one event at `now`.
    ///
    /// Returns `true` when the event is admitted (within the cap).
    /// Returns `false` when the cap would be exceeded.
    /// Always returns `true` when `cap = 0`.
    #[must_use]
    pub fn try_add(&mut self, now: Instant) -> bool {
        if self.cap == 0 {
            return true;
        }
        // Expire entries older than the window.
        self.expire(now);
        if self.timestamps.len() < self.cap as usize {
            self.timestamps.push_back(now);
            true
        } else {
            false
        }
    }

    /// Resets the window, discarding all recorded events.
    ///
    /// Used by [`RateLimiter::record_auth_success`] to clear an IP's
    /// failure counter immediately upon successful authentication.
    pub fn reset(&mut self) {
        self.timestamps.clear();
    }

    /// Returns `true` when a new event at `now` would be rejected
    /// because the cap is already reached within the current window.
    /// Read-only — does not record the event and does not slide.
    /// Always returns `false` when `cap = 0`.
    #[must_use]
    pub fn would_block(&self, now: Instant) -> bool {
        if self.cap == 0 {
            return false;
        }
        // Count only timestamps still inside the window.
        self.count_in_window(now) >= self.cap
    }

    /// Returns the number of events still inside the current window
    /// at time `now`. Read-only; does not slide.
    #[must_use]
    pub fn count_in_window(&self, now: Instant) -> u32 {
        u32::try_from(
            self.timestamps
                .iter()
                .filter(|&&t| now.duration_since(t) < self.window)
                .count(),
        )
        .unwrap_or(u32::MAX)
    }

    /// Removes timestamps that have fallen outside the window.
    fn expire(&mut self, now: Instant) {
        while let Some(&front) = self.timestamps.front() {
            if now.duration_since(front) >= self.window {
                self.timestamps.pop_front();
            } else {
                break;
            }
        }
    }
}

// ── TokenBucket ──────────────────────────────────────────────────────────────

/// A token-bucket rate limiter.
///
/// Capacity is `rate * 2` (burst tolerance). Tokens refill at a
/// constant rate derived from `rate` tokens per `refill_period`.
///
/// `cap = 0` disables the limiter (every take succeeds and costs
/// zero sleep time).
#[derive(Debug)]
pub struct TokenBucket {
    /// Rate in tokens per `refill_period`. Also half the bucket capacity.
    rate: u64,
    /// Duration for a full refill cycle.
    refill_period: Duration,
    /// Current token balance. Starts at full capacity (`rate * 2`).
    tokens: u64,
    /// When `tokens` was last updated. `None` until the first operation,
    /// so the bucket anchors its clock to the caller's first `now` rather
    /// than to `Instant::now()` at construction time. This makes the
    /// arithmetic fully deterministic when a [`MockClock`] is injected.
    last_refill: Option<Instant>,
}

impl TokenBucket {
    /// Creates a token bucket.
    ///
    /// - `cap` — tokens per `refill_period` (and half the burst capacity).
    /// - `cap = 0` → pass-through: all takes succeed instantly.
    ///
    /// The bucket starts full (`cap * 2` tokens).
    #[must_use]
    pub fn new(cap: u64, refill_period: Duration) -> Self {
        Self {
            rate: cap,
            refill_period,
            tokens: cap.saturating_mul(2),
            last_refill: None,
        }
    }

    /// Attempts to consume `n` tokens at time `now`.
    ///
    /// Returns `true` on success; `false` if the bucket lacks sufficient
    /// tokens. Always returns `true` when `cap = 0`.
    #[must_use]
    pub fn try_take(&mut self, n: u64, now: Instant) -> bool {
        if self.rate == 0 {
            return true;
        }
        self.refill(now);
        if self.tokens >= n {
            self.tokens -= n;
            true
        } else {
            false
        }
    }

    /// Returns the number of tokens currently available at time `now`,
    /// refilling first. Used to populate the `QueryThrottled` audit event
    /// so operators see how many tokens were left when the request was
    /// rejected. `cap = 0` returns `0` (the audit event only fires when
    /// throttled, which implies `cap > 0`; `0` is a safe sentinel).
    #[must_use]
    pub fn available(&mut self, now: Instant) -> u64 {
        if self.rate == 0 {
            return 0;
        }
        self.refill(now);
        self.tokens
    }

    /// Take `n` tokens, returning the `Duration` the caller should sleep
    /// before the tokens are considered available.
    ///
    /// When the bucket has enough tokens (`tokens >= n`), they are consumed
    /// immediately and `Duration::ZERO` is returned.
    ///
    /// When the bucket is short by `n - tokens`, the available tokens are
    /// drained to 0 and the returned `Duration` covers the time needed for
    /// the shortfall to be refilled at the configured rate. **The shortfall
    /// is NOT reserved against future tokens** — concurrent callers during
    /// the sleep window may each receive a sleep covering the same future
    /// nanoseconds, double-issuing the refill. This is acceptable for the
    /// intended use case (per-connection bandwidth throttling where each
    /// connection has its own bucket so contention is naturally absent) but
    /// callers sharing a bucket across tasks must be aware.
    ///
    /// Always returns [`Duration::ZERO`] when `cap = 0`.
    pub fn take(&mut self, n: u64, now: Instant) -> Duration {
        if self.rate == 0 {
            return Duration::ZERO;
        }
        self.refill(now);
        if self.tokens >= n {
            self.tokens -= n;
            return Duration::ZERO;
        }
        let shortfall = n - self.tokens;
        self.tokens = 0;
        // sleep = shortfall * refill_period / rate
        let nanos = u128::from(shortfall)
            .saturating_mul(self.refill_period.as_nanos())
            / u128::from(self.rate);
        Duration::from_nanos(u64::try_from(nanos).unwrap_or(u64::MAX))
    }

    /// Adds tokens accumulated since `last_refill`, capped at `rate * 2`.
    /// On the first call, anchors `last_refill` to `now` without earning tokens.
    ///
    /// `last_refill` is advanced **only when at least one token is earned**.
    /// This preserves sub-token fractional time so that callers polling at
    /// high frequency (e.g. every 1 ms with rate=100/sec) accumulate credit
    /// rather than silently discarding the remainder on every call.
    fn refill(&mut self, now: Instant) {
        let Some(last) = self.last_refill else {
            self.last_refill = Some(now);
            return;
        };
        let elapsed = now.duration_since(last);
        if elapsed.is_zero() {
            return;
        }
        // tokens_earned = elapsed * rate / refill_period
        let earned = elapsed
            .as_nanos()
            .saturating_mul(u128::from(self.rate))
            / self.refill_period.as_nanos();
        // Only advance last_refill when we actually earn ≥1 token.
        // Leaving it unchanged when earned == 0 lets the unspent fractional
        // time accumulate, preventing permanent token loss under high-frequency
        // polling.
        if earned == 0 {
            return;
        }
        let capacity = self.rate.saturating_mul(2);
        self.tokens = self
            .tokens
            .saturating_add(u64::try_from(earned).unwrap_or(u64::MAX))
            .min(capacity);
        self.last_refill = Some(now);
    }
}

// ── Per-IP auth entry ────────────────────────────────────────────────────────

/// Auth-window state tracked per peer IP in [`AuthStore`].
struct AuthEntry {
    ip: IpAddr,
    /// Failure counter for authentication rate-limiting.
    window: SlidingWindow,
    /// Logical LRU order: higher value = more recently used.
    lru_seq: u64,
}

// ── AuthStore — async-safe, tokio RwLock ─────────────────────────────────────

/// Async store for per-IP authentication failure windows.
///
/// Guarded by [`tokio::sync::RwLock`]; only accessed from async code.
struct AuthStore {
    entries: Vec<AuthEntry>,
    ip_cap: usize,
    seq: u64,
    auth_cap: u32,
}

impl AuthStore {
    fn new(ip_cap: usize) -> Self {
        Self::with_cap(ip_cap, 0)
    }

    /// Constructs an empty store with both the IP-slot cap and the
    /// initial auth cap pre-set. Used by the production constructor
    /// [`RateLimiter::new`] to avoid the async `set_caps` round-trip
    /// (which would require a `blocking_write` and panic when called
    /// from inside an existing tokio runtime).
    fn with_cap(ip_cap: usize, auth_cap: u32) -> Self {
        Self {
            entries: Vec::with_capacity(ip_cap.min(256)),
            ip_cap,
            seq: 0,
            auth_cap,
        }
    }

    fn next_seq(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }

    /// Finds the entry for `ip`, or creates one (evicting LRU if full).
    fn touch_or_insert(&mut self, ip: IpAddr) -> &mut AuthEntry {
        if let Some(pos) = self.entries.iter().position(|e| e.ip == ip) {
            let seq = self.next_seq();
            self.entries[pos].lru_seq = seq;
            return &mut self.entries[pos];
        }

        if self.ip_cap > 0 && self.entries.len() >= self.ip_cap {
            let lru_pos = self
                .entries
                .iter()
                .enumerate()
                .min_by_key(|(_, e)| e.lru_seq)
                .map(|(i, _)| i)
                .expect("entries non-empty when cap reached");
            self.entries.remove(lru_pos);
        }

        let seq = self.next_seq();
        self.entries.push(AuthEntry {
            ip,
            window: SlidingWindow::new(self.auth_cap, Duration::from_secs(60)),
            lru_seq: seq,
        });
        self.entries.last_mut().expect("just pushed")
    }
}

// ── ConnStore — sync-safe, std Mutex ─────────────────────────────────────────

/// Synchronous store for per-IP active connection counts.
///
/// Guarded by [`std::sync::Mutex`] so that [`ConnectionGuard::drop`]
/// can decrement without blocking the async executor.
struct ConnStore {
    counts: HashMap<IpAddr, u32>,
    conn_per_ip_cap: u32,
}

impl ConnStore {
    fn new() -> Self {
        Self::with_cap(0)
    }

    /// Constructs an empty store with the per-IP connection cap pre-set.
    /// Used by [`RateLimiter::new`] so the cap is live before any
    /// connection is accepted.
    fn with_cap(conn_per_ip_cap: u32) -> Self {
        Self {
            counts: HashMap::new(),
            conn_per_ip_cap,
        }
    }
}

// ── ConnectionGuard ──────────────────────────────────────────────────────────

/// RAII guard returned by [`RateLimiter::try_acquire_connection`].
///
/// Dropping the guard decrements the active-connection count for the
/// peer IP. The decrement uses [`std::sync::Mutex::lock`], which is
/// always safe from a synchronous `Drop` — even inside an async task.
pub struct ConnectionGuard<C: Clock = SystemClock> {
    ip: IpAddr,
    limiter: Arc<RateLimiter<C>>,
}

impl<C: Clock> Drop for ConnectionGuard<C> {
    fn drop(&mut self) {
        let mut conns = self
            .limiter
            .conn_store
            .lock()
            .expect("ConnStore mutex poisoned");
        let count = conns.counts.entry(self.ip).or_insert(0);
        *count = count.saturating_sub(1);
        // Remove the entry once it hits zero so the HashMap does not grow
        // unboundedly under IP churn (port scanners, rotating NAT clients).
        // After removal, ConnStore holds at most one entry per *live* connection,
        // which is naturally bounded by MAX_CONNECTIONS.
        if *count == 0 {
            conns.counts.remove(&self.ip);
        }
    }
}

// ── RateLimiter ──────────────────────────────────────────────────────────────

/// Global rate-limiter store, keyed by peer IP.
///
/// Holds up to `ip_cap` distinct IP entries; when full, the
/// least-recently-touched entry is evicted to make room.
///
/// Internally uses two stores:
/// - `auth_store` (tokio `RwLock`): async-accessed auth failure windows.
/// - `conn_store` (std `Mutex`): sync-accessible connection counts, safe
///   to decrement from `Drop`.
///
/// # Construction
///
/// ```rust,ignore
/// let rl = Arc::new(RateLimiter::with_clock(256, Arc::new(SystemClock)));
/// rl.set_caps(5, 16).await;
/// ```
pub struct RateLimiter<C: Clock = SystemClock> {
    auth_store: RwLock<AuthStore>,
    conn_store: std::sync::Mutex<ConnStore>,
    clock: Arc<C>,
}

impl<C: Clock> RateLimiter<C> {
    /// Creates a new limiter with the given IP-cap and clock.
    ///
    /// Call [`set_caps`] to configure per-axis caps before use.
    ///
    /// [`set_caps`]: RateLimiter::set_caps
    #[must_use]
    pub fn with_clock(ip_cap: usize, clock: Arc<C>) -> Self {
        Self {
            auth_store: RwLock::new(AuthStore::new(ip_cap)),
            conn_store: std::sync::Mutex::new(ConnStore::new()),
            clock,
        }
    }

    /// Creates a new limiter with caps pre-applied at construction.
    /// Used by [`RateLimiter::new`] (production constructor) so the
    /// caller does not need an async or blocking lock-acquire to seed
    /// the caps — important for callers inside a tokio runtime.
    #[must_use]
    pub fn with_clock_and_caps(
        ip_cap: usize,
        auth_cap: u32,
        conn_per_ip_cap: u32,
        clock: Arc<C>,
    ) -> Self {
        Self {
            auth_store: RwLock::new(AuthStore::with_cap(ip_cap, auth_cap)),
            conn_store: std::sync::Mutex::new(ConnStore::with_cap(conn_per_ip_cap)),
            clock,
        }
    }

    /// Configures per-axis caps.
    ///
    /// - `auth_cap`: max auth failures per IP in a 60-second window. `0` disables.
    /// - `conn_per_ip_cap`: max simultaneous connections per IP. `0` disables.
    ///
    /// Can be called at any time; new caps apply to the next access.
    /// Existing per-IP windows ARE retroactively resized: setting a smaller
    /// `auth_cap` immediately tightens the cap for already-tracked IPs.
    ///
    /// # Panics
    ///
    /// Panics if the `conn_store` mutex is poisoned.
    pub async fn set_caps(&self, auth_cap: u32, conn_per_ip_cap: u32) {
        {
            let mut auth = self.auth_store.write().await;
            auth.auth_cap = auth_cap;
            for entry in &mut auth.entries {
                entry.window.cap = auth_cap;
            }
        }
        {
            let mut conns = self.conn_store.lock().expect("ConnStore mutex poisoned");
            conns.conn_per_ip_cap = conn_per_ip_cap;
        }
    }

    /// Records one authentication failure for `ip`.
    ///
    /// Returns `true` when the failure is within the allowed cap.
    /// Returns `false` when the cap is exceeded (fail-fast).
    /// Always returns `true` when the auth cap is `0`.
    pub async fn record_auth_failure(&self, ip: IpAddr) -> bool {
        let now = self.clock.now();
        let mut auth = self.auth_store.write().await;
        let entry = auth.touch_or_insert(ip);
        entry.window.try_add(now)
    }

    /// Records a successful authentication for `ip`, resetting its
    /// failure window so subsequent failures start a fresh count.
    pub async fn record_auth_success(&self, ip: IpAddr) {
        let mut auth = self.auth_store.write().await;
        if let Some(entry) = auth.entries.iter_mut().find(|e| e.ip == ip) {
            entry.window.reset();
        }
    }

    /// Attempts to acquire a connection slot for `ip`.
    ///
    /// Returns `Some(ConnectionGuard)` on success; the guard's [`Drop`]
    /// releases the slot when the connection closes.
    /// Returns `None` when the per-IP connection cap is reached.
    /// Always returns `Some` when the connection cap is `0`.
    ///
    /// # Panics
    ///
    /// Panics if the `conn_store` mutex is poisoned.
    #[must_use]
    pub fn try_acquire_connection(self: &Arc<Self>, ip: IpAddr) -> Option<ConnectionGuard<C>> {
        let mut conns = self.conn_store.lock().expect("ConnStore mutex poisoned");
        let cap = conns.conn_per_ip_cap;
        let count = conns.counts.entry(ip).or_insert(0);
        if cap > 0 && *count >= cap {
            return None;
        }
        *count += 1;
        drop(conns);

        Some(ConnectionGuard {
            ip,
            limiter: Arc::clone(self),
        })
    }

    // ── Read-only inspection helpers (used by handler/listener for
    //    early-skip + audit-event population) ────────────────────────

    /// Returns `true` when the auth cap is non-zero (i.e. the auth
    /// gate is active). Handlers use this to skip the throttle check
    /// path entirely when the operator disabled the eje.
    pub async fn auth_cap_active(&self) -> bool {
        self.auth_store.read().await.auth_cap > 0
    }

    /// Returns `true` when `ip` has already exceeded its auth-failure
    /// cap in the current sliding window. Does NOT record a new
    /// failure — call `record_auth_failure` for that. Used by
    /// `handle_hello` to fail-fast before evaluating credentials.
    pub async fn is_auth_blocked(&self, ip: IpAddr) -> bool {
        let auth = self.auth_store.read().await;
        if auth.auth_cap == 0 {
            return false;
        }
        let now = self.clock.now();
        auth.entries
            .iter()
            .find(|e| e.ip == ip)
            .is_some_and(|e| e.window.would_block(now))
    }

    /// Returns the current count of recorded auth failures for `ip`
    /// in the sliding window. Used to populate the `AuthThrottled`
    /// audit event so operators see "5/5 failures" rather than just
    /// "throttled".
    pub async fn auth_failures_in_window(&self, ip: IpAddr) -> u32 {
        let auth = self.auth_store.read().await;
        let now = self.clock.now();
        auth.entries
            .iter()
            .find(|e| e.ip == ip)
            .map_or(0, |e| e.window.count_in_window(now))
    }

    /// Returns the active per-IP connection cap. `0` when disabled.
    /// Used to populate the `ConnectionThrottled` audit event.
    ///
    /// # Panics
    ///
    /// Panics if the `conn_store` mutex is poisoned.
    #[must_use]
    pub fn conn_per_ip_cap(&self) -> u32 {
        self.conn_store
            .lock()
            .expect("ConnStore mutex poisoned")
            .conn_per_ip_cap
    }

    /// Returns the current live-connection count for `ip` (`0` when the
    /// IP has no tracked connections). Used to populate the
    /// `ConnectionThrottled` audit event so operators see the count that
    /// triggered the rejection, distinct from the configured `cap`. Read
    /// under the same `Mutex` as `try_acquire_connection`, so a rejection
    /// followed immediately by this read observes the count that caused it.
    ///
    /// # Panics
    ///
    /// Panics if the `conn_store` mutex is poisoned.
    #[must_use]
    pub fn live_connections(&self, ip: IpAddr) -> u32 {
        self.conn_store
            .lock()
            .expect("ConnStore mutex poisoned")
            .counts
            .get(&ip)
            .copied()
            .unwrap_or(0)
    }
}

impl RateLimiter<SystemClock> {
    /// Production constructor. Builds the limiter with `SystemClock`
    /// and the caps applied at construction time (no async or blocking
    /// locks involved). Returns an `Arc` ready to be cloned into the
    /// listener and per-connection handlers.
    ///
    /// Safe to call from inside an async runtime — unlike the prior
    /// implementation that relied on `blocking_write`, which panicked
    /// when invoked from a tokio task.
    #[must_use]
    pub fn new(ip_cap: usize, auth_cap: u32, conn_per_ip_cap: u32) -> Arc<Self> {
        Arc::new(Self::with_clock_and_caps(
            ip_cap,
            auth_cap,
            conn_per_ip_cap,
            Arc::new(SystemClock),
        ))
    }
}
