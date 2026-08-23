// SPDX-License-Identifier: BSL-1.1
//! Unit tests for the `rate_limiter` module (v0.6.0 Task 5 C2).
//!
//! All timing-sensitive tests use `MockClock`. Wall-clock is never read.

use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;
use std::time::Duration;

use ermya_graph_server::rate_limiter::{Clock, MockClock, RateLimiter, SlidingWindow, TokenBucket};

fn ip(n: u8) -> IpAddr {
    IpAddr::V4(Ipv4Addr::new(127, 0, 0, n))
}

// ── SlidingWindow ───────────────────────────────────────────────────────

#[test]
fn sliding_window_under_cap_admits() {
    let clock = Arc::new(MockClock::new());
    let mut w = SlidingWindow::new(5, Duration::from_secs(60));
    for i in 0..5 {
        assert!(w.try_add(clock.now()), "admission {i} of 5 must pass");
    }
    assert!(
        !w.try_add(clock.now()),
        "6th admission within the window must fail"
    );
}

#[test]
fn sliding_window_advances_after_full_window() {
    let clock = Arc::new(MockClock::new());
    let mut w = SlidingWindow::new(5, Duration::from_secs(60));
    for _ in 0..5 {
        assert!(w.try_add(clock.now()));
    }
    assert!(!w.try_add(clock.now()));

    clock.advance(Duration::from_secs(61));
    assert!(
        w.try_add(clock.now()),
        "after window expiry the first admission must pass again"
    );
}

#[test]
fn sliding_window_cap_zero_is_passthrough() {
    let clock = Arc::new(MockClock::new());
    let mut w = SlidingWindow::new(0, Duration::from_secs(60));
    for _ in 0..1000 {
        assert!(w.try_add(clock.now()), "cap=0 always admits");
    }
}

// ── TokenBucket ─────────────────────────────────────────────────────────

#[test]
fn token_bucket_try_take_under_capacity_succeeds() {
    let clock = Arc::new(MockClock::new());
    let mut b = TokenBucket::new(100u64, Duration::from_secs(1));
    // Initial fill = capacity = 100 * 2 = 200.
    for i in 0..200 {
        assert!(
            b.try_take(1, clock.now()),
            "take {i} of 200 must succeed (full bucket)"
        );
    }
    assert!(
        !b.try_take(1, clock.now()),
        "201st take with no refill must fail"
    );
}

#[test]
fn token_bucket_refills_over_time() {
    let clock = Arc::new(MockClock::new());
    let mut b = TokenBucket::new(100u64, Duration::from_secs(1));
    // Drain.
    for _ in 0..200 {
        assert!(b.try_take(1, clock.now()));
    }
    assert!(!b.try_take(1, clock.now()));
    // Advance 1 sec → refill 100 tokens (capped at capacity 200).
    clock.advance(Duration::from_secs(1));
    for i in 0..100 {
        assert!(
            b.try_take(1, clock.now()),
            "refilled take {i} of 100 must succeed"
        );
    }
    assert!(
        !b.try_take(1, clock.now()),
        "101st take after only 1 sec of refill must fail"
    );
}

#[test]
fn token_bucket_take_returns_sleep_duration() {
    let clock = Arc::new(MockClock::new());
    let mut b = TokenBucket::new(100u64, Duration::from_secs(1));
    // Drain to 0.
    for _ in 0..200 {
        assert!(b.try_take(1, clock.now()));
    }
    // Now ask for 50 with take: should return ~500ms (50 tokens at 100/sec).
    let sleep = b.take(50, clock.now());
    assert!(
        sleep >= Duration::from_millis(490) && sleep <= Duration::from_millis(510),
        "take(50) on empty bucket with refill=100/sec should sleep ~500ms, got {sleep:?}"
    );
}

#[test]
fn token_bucket_cap_zero_is_passthrough() {
    let clock = Arc::new(MockClock::new());
    let mut b = TokenBucket::new(0u64, Duration::from_secs(1));
    for _ in 0..10_000 {
        assert!(b.try_take(1_000_000, clock.now()), "cap=0 always passes");
    }
    assert_eq!(
        b.take(1_000_000_000, clock.now()),
        Duration::ZERO,
        "cap=0 take returns zero sleep"
    );
}

#[test]
fn token_bucket_refills_correctly_under_high_call_frequency() {
    // rate=100/sec, caller polls every 1ms. After 1 sec of polling, the
    // bucket must show 100 earned tokens (was losing them sub-tick before).
    let clock = Arc::new(MockClock::new());
    let mut b = TokenBucket::new(100u64, Duration::from_secs(1));
    // Drain initial capacity (200) so we can observe pure refill.
    for _ in 0..200 {
        assert!(b.try_take(1, clock.now()));
    }
    assert!(!b.try_take(1, clock.now()));

    // Poll at 1ms cadence for 1 sec (1000 polls, each earns 0 tokens
    // individually but together should accumulate 100 tokens of refill).
    for _ in 0..1000 {
        clock.advance(Duration::from_millis(1));
        let _ = b.try_take(0, clock.now()); // trigger refill check
    }

    // Now we should have ~100 tokens (1 sec × 100/sec = 100).
    let mut taken = 0;
    while b.try_take(1, clock.now()) {
        taken += 1;
        if taken > 200 {
            break;
        }
    }
    assert!(
        (95..=105).contains(&taken),
        "after 1 sec of high-frequency polling at 100/sec rate, expected ~100 tokens, got {taken}"
    );
}

// ── RateLimiter (global store) ─────────────────────────────────────────

#[tokio::test]
async fn rate_limiter_tracks_auth_failures_per_ip() {
    let clock = Arc::new(MockClock::new());
    let rl = RateLimiter::with_clock(/* ip_cap */ 4, Arc::clone(&clock));
    rl.set_caps(/* auth */ 3, /* conn_per_ip */ 0).await;

    for i in 0..3 {
        assert!(
            rl.record_auth_failure(ip(1)).await,
            "fail {i} of 3 must be allowed"
        );
    }
    assert!(
        !rl.record_auth_failure(ip(1)).await,
        "4th auth failure on ip(1) must throttle"
    );
    assert!(rl.record_auth_failure(ip(2)).await, "ip(2) starts fresh");
}

#[tokio::test]
async fn rate_limiter_auth_success_resets_counter() {
    let clock = Arc::new(MockClock::new());
    let rl = RateLimiter::with_clock(4, Arc::clone(&clock));
    rl.set_caps(3, 0).await;

    for _ in 0..3 {
        assert!(rl.record_auth_failure(ip(1)).await);
    }
    assert!(!rl.record_auth_failure(ip(1)).await);

    rl.record_auth_success(ip(1)).await;
    assert!(
        rl.record_auth_failure(ip(1)).await,
        "post-success, ip(1) starts a fresh window"
    );
}

#[tokio::test]
async fn rate_limiter_lru_evicts_oldest_when_cap_exceeded() {
    let clock = Arc::new(MockClock::new());
    let rl = RateLimiter::with_clock(/* ip_cap */ 2, Arc::clone(&clock));
    rl.set_caps(3, 0).await;

    assert!(rl.record_auth_failure(ip(1)).await);
    assert!(rl.record_auth_failure(ip(2)).await);
    // 3rd → evicts ip(1) (least recently touched).
    assert!(rl.record_auth_failure(ip(3)).await);
    // ip(1) is now fresh: 3 fresh failures should pass.
    for _ in 0..3 {
        assert!(rl.record_auth_failure(ip(1)).await);
    }
}

#[tokio::test]
async fn rate_limiter_conn_per_ip_cap() {
    let clock = Arc::new(MockClock::new());
    let rl = Arc::new(RateLimiter::with_clock(4, Arc::clone(&clock)));
    rl.set_caps(0, /* conn_per_ip */ 2).await;

    let g1 = rl.try_acquire_connection(ip(1));
    assert!(g1.is_some(), "1st conn must be allowed");
    let g2 = rl.try_acquire_connection(ip(1));
    assert!(g2.is_some(), "2nd conn must be allowed (at cap)");
    let g3 = rl.try_acquire_connection(ip(1));
    assert!(g3.is_none(), "3rd conn must be rejected (over cap)");

    drop(g1);
    let g4 = rl.try_acquire_connection(ip(1));
    assert!(g4.is_some(), "after drop, a new conn must be allowed");
}

#[tokio::test]
async fn rate_limiter_caps_zero_pass_through() {
    let clock = Arc::new(MockClock::new());
    let rl = Arc::new(RateLimiter::with_clock(4, Arc::clone(&clock)));
    rl.set_caps(0, 0).await;

    for _ in 0..1000 {
        assert!(rl.record_auth_failure(ip(1)).await);
    }
    let mut guards = vec![];
    for _ in 0..100 {
        guards.push(rl.try_acquire_connection(ip(1)));
    }
    assert!(
        guards.iter().all(Option::is_some),
        "cap=0 admits all connections"
    );
}

// ── Inspection helpers (used by handle_hello + audit population) ────────

#[tokio::test]
async fn auth_cap_active_reflects_set_caps() {
    let clock = Arc::new(MockClock::new());
    let rl = RateLimiter::with_clock(4, Arc::clone(&clock));

    rl.set_caps(0, 0).await;
    assert!(!rl.auth_cap_active().await, "cap=0 → inactive");

    rl.set_caps(5, 0).await;
    assert!(rl.auth_cap_active().await, "cap>0 → active");
}

#[tokio::test]
async fn is_auth_blocked_returns_true_only_at_cap() {
    let clock = Arc::new(MockClock::new());
    let rl = RateLimiter::with_clock(4, Arc::clone(&clock));
    rl.set_caps(3, 0).await;

    assert!(!rl.is_auth_blocked(ip(1)).await, "unknown IP not blocked");
    for _ in 0..3 {
        let _ = rl.record_auth_failure(ip(1)).await;
    }
    assert!(
        rl.is_auth_blocked(ip(1)).await,
        "ip(1) blocked at cap without consuming a new attempt"
    );
    // Confirm is_auth_blocked is read-only (does not record).
    assert!(
        rl.is_auth_blocked(ip(1)).await,
        "read-only: second call still blocked, didn't reset or burn"
    );
}

#[tokio::test]
async fn is_auth_blocked_passthrough_when_cap_zero() {
    let clock = Arc::new(MockClock::new());
    let rl = RateLimiter::with_clock(4, Arc::clone(&clock));
    rl.set_caps(0, 0).await;
    for _ in 0..1000 {
        let _ = rl.record_auth_failure(ip(1)).await;
    }
    assert!(
        !rl.is_auth_blocked(ip(1)).await,
        "cap=0 → never blocked regardless of failure count"
    );
}

#[tokio::test]
async fn auth_failures_in_window_counts_correctly() {
    let clock = Arc::new(MockClock::new());
    let rl = RateLimiter::with_clock(4, Arc::clone(&clock));
    rl.set_caps(5, 0).await;

    assert_eq!(rl.auth_failures_in_window(ip(1)).await, 0, "unknown IP → 0");
    for _ in 0..3 {
        let _ = rl.record_auth_failure(ip(1)).await;
    }
    assert_eq!(
        rl.auth_failures_in_window(ip(1)).await,
        3,
        "after 3 recorded failures"
    );

    // Slide past the window: count drops to 0.
    clock.advance(Duration::from_secs(61));
    assert_eq!(
        rl.auth_failures_in_window(ip(1)).await,
        0,
        "after window expiry, count is 0"
    );
}

#[tokio::test]
async fn conn_per_ip_cap_returns_current_value() {
    let clock = Arc::new(MockClock::new());
    let rl = RateLimiter::with_clock(4, Arc::clone(&clock));

    rl.set_caps(0, 0).await;
    assert_eq!(rl.conn_per_ip_cap(), 0);

    rl.set_caps(0, 12).await;
    assert_eq!(rl.conn_per_ip_cap(), 12);
}

#[test]
fn production_constructor_seeds_caps() {
    // RateLimiter::new (production) returns an Arc<Self> with caps
    // applied synchronously. Verify by checking the inspection helpers
    // from a fresh tokio runtime (since the constructor is sync but
    // helpers are async).
    let rl = RateLimiter::new(
        /* ip_cap */ 256, /* auth */ 5, /* conn_per_ip */ 16,
    );
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    rt.block_on(async {
        assert!(
            rl.auth_cap_active().await,
            "auth cap 5 must register as active"
        );
        assert_eq!(rl.conn_per_ip_cap(), 16, "conn_per_ip cap 16 must be set");
    });
}

/// The dual-store design (`tokio::RwLock` for auth, `std::Mutex` for
/// conn) exists specifically to make Drop-from-async-task safe without
/// deadlocking. This test spawns N tasks contending for a shared
/// `RateLimiter` and verifies all guards drop cleanly under contention.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rate_limiter_concurrent_acquire_and_drop_no_deadlock() {
    let clock = Arc::new(MockClock::new());
    let rl = Arc::new(RateLimiter::with_clock(64, Arc::clone(&clock)));
    rl.set_caps(0, /* conn_per_ip */ 50).await;

    let mut handles = vec![];
    for _ in 0..100 {
        let rl_c = Arc::clone(&rl);
        handles.push(tokio::spawn(async move {
            let g = rl_c.try_acquire_connection(ip(7));
            tokio::time::sleep(Duration::from_millis(10)).await;
            drop(g);
        }));
    }
    for h in handles {
        let _ = h.await;
    }

    // All guards dropped: a fresh acquire must succeed (no leaked slots,
    // no deadlock between concurrent acquire and the sync Drop path).
    let g_final = rl.try_acquire_connection(ip(7));
    assert!(
        g_final.is_some(),
        "after all tasks complete, a fresh acquire must succeed"
    );
}
