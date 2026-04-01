// Copyright 2026 BelowZero Security OU. All rights reserved.

//! Metrics registry — lock-free atomic counters, gauges, and histogram for Prometheus.

use std::sync::atomic::{AtomicU64, Ordering};

/// Standard Prometheus histogram bucket upper bounds (in seconds).
pub const HISTOGRAM_BUCKETS: [f64; 12] = [
    0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

/// Lock-free metrics registry.
///
/// All fields are `AtomicU64` for zero-overhead instrumentation on the hot path.
/// Fields are public so instrumentation points can write directly without method
/// dispatch overhead.
#[derive(Debug)]
pub struct MetricsRegistry {
    // --- Gauges ---
    /// Current number of active connections.
    pub connections_active: AtomicU64,
    /// Configured maximum connections.
    pub connections_max: AtomicU64,

    // --- Counters ---
    /// Total accepted connections.
    pub connections_accepted: AtomicU64,
    /// Total rejected connections (at capacity).
    pub connections_rejected: AtomicU64,
    /// Total successful authentication attempts.
    pub auth_success: AtomicU64,
    /// Total failed authentication attempts.
    pub auth_failure: AtomicU64,
    /// Total GQL read queries.
    pub queries_gql_read: AtomicU64,
    /// Total GQL mutation queries.
    pub queries_gql_mutation: AtomicU64,
    /// Total Cypher read queries.
    pub queries_cypher_read: AtomicU64,
    /// Total Cypher mutation queries.
    pub queries_cypher_mutation: AtomicU64,
    /// Total query errors.
    pub query_errors: AtomicU64,
    /// Total TLS handshake failures.
    pub tls_handshake_failures: AtomicU64,

    // --- Histogram: query duration ---
    /// Cumulative bucket counts for query duration histogram.
    pub query_duration_buckets: [AtomicU64; 12],
    /// Sum of all observed query durations (stored as `f64::to_bits()`).
    pub query_duration_sum: AtomicU64,
    /// Total number of observed query durations.
    pub query_duration_count: AtomicU64,
}

impl MetricsRegistry {
    /// Create a new registry with all counters at zero.
    #[must_use]
    pub fn new(max_connections: u64) -> Self {
        Self {
            connections_active: AtomicU64::new(0),
            connections_max: AtomicU64::new(max_connections),
            connections_accepted: AtomicU64::new(0),
            connections_rejected: AtomicU64::new(0),
            auth_success: AtomicU64::new(0),
            auth_failure: AtomicU64::new(0),
            queries_gql_read: AtomicU64::new(0),
            queries_gql_mutation: AtomicU64::new(0),
            queries_cypher_read: AtomicU64::new(0),
            queries_cypher_mutation: AtomicU64::new(0),
            query_errors: AtomicU64::new(0),
            tls_handshake_failures: AtomicU64::new(0),
            query_duration_buckets: std::array::from_fn(|_| AtomicU64::new(0)),
            query_duration_sum: AtomicU64::new(0),
            query_duration_count: AtomicU64::new(0),
        }
    }

    /// Record a query duration observation (in seconds).
    ///
    /// Updates the histogram buckets (cumulative), sum, and count atomically.
    pub fn record_query_duration(&self, seconds: f64) {
        // Increment cumulative buckets: once the value fits, all larger buckets count it
        let mut found = false;
        for (i, &upper) in HISTOGRAM_BUCKETS.iter().enumerate() {
            if !found && seconds <= upper {
                found = true;
            }
            if found {
                self.query_duration_buckets[i].fetch_add(1, Ordering::Relaxed);
            }
        }

        // Add to sum via CAS loop (f64 stored as bits in AtomicU64)
        cas_add_f64(&self.query_duration_sum, seconds);

        // Increment count
        self.query_duration_count.fetch_add(1, Ordering::Relaxed);
    }
}

/// Atomically add a `f64` delta to an `AtomicU64` that stores `f64::to_bits()`.
///
/// Uses a compare-exchange loop to handle concurrent updates safely.
fn cas_add_f64(atom: &AtomicU64, delta: f64) {
    loop {
        let current_bits = atom.load(Ordering::Relaxed);
        let current = f64::from_bits(current_bits);
        let new = current + delta;
        let new_bits = new.to_bits();

        if atom
            .compare_exchange_weak(current_bits, new_bits, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counter_starts_at_zero_and_increments() {
        let r = MetricsRegistry::new(256);
        assert_eq!(r.connections_accepted.load(Ordering::Relaxed), 0);
        r.connections_accepted.fetch_add(1, Ordering::Relaxed);
        assert_eq!(r.connections_accepted.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn connections_max_set_from_constructor() {
        let r = MetricsRegistry::new(128);
        assert_eq!(r.connections_max.load(Ordering::Relaxed), 128);
    }

    #[test]
    fn all_counters_start_at_zero() {
        let r = MetricsRegistry::new(256);
        assert_eq!(r.connections_active.load(Ordering::Relaxed), 0);
        assert_eq!(r.connections_rejected.load(Ordering::Relaxed), 0);
        assert_eq!(r.auth_success.load(Ordering::Relaxed), 0);
        assert_eq!(r.auth_failure.load(Ordering::Relaxed), 0);
        assert_eq!(r.queries_gql_read.load(Ordering::Relaxed), 0);
        assert_eq!(r.query_errors.load(Ordering::Relaxed), 0);
        assert_eq!(r.tls_handshake_failures.load(Ordering::Relaxed), 0);
        assert_eq!(r.query_duration_count.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn histogram_record_duration_increments_correct_bucket() {
        let r = MetricsRegistry::new(256);
        r.record_query_duration(0.007); // 7 ms — fits in ≤0.01 bucket (index 2)

        assert_eq!(r.query_duration_count.load(Ordering::Relaxed), 1);
        // Buckets 0 (0.001) and 1 (0.005): 0.007 > upper → NOT incremented
        assert_eq!(r.query_duration_buckets[0].load(Ordering::Relaxed), 0);
        assert_eq!(r.query_duration_buckets[1].load(Ordering::Relaxed), 0);
        // Bucket 2 (0.01) and all higher: incremented (cumulative)
        assert_eq!(r.query_duration_buckets[2].load(Ordering::Relaxed), 1);
        assert_eq!(r.query_duration_buckets[3].load(Ordering::Relaxed), 1);
        assert_eq!(r.query_duration_buckets[11].load(Ordering::Relaxed), 1);
    }

    #[test]
    fn histogram_sum_accumulates_as_f64_bits() {
        let r = MetricsRegistry::new(256);
        r.record_query_duration(0.050);
        r.record_query_duration(0.100);
        let sum = f64::from_bits(r.query_duration_sum.load(Ordering::Relaxed));
        assert!((sum - 0.150).abs() < 1e-10);
    }

    #[test]
    fn histogram_very_fast_query_lands_in_first_bucket() {
        let r = MetricsRegistry::new(256);
        r.record_query_duration(0.0005); // 0.5 ms — fits in ≤0.001
        assert_eq!(r.query_duration_buckets[0].load(Ordering::Relaxed), 1);
    }

    #[test]
    fn histogram_very_slow_query_only_counted_beyond_last_bucket() {
        let r = MetricsRegistry::new(256);
        r.record_query_duration(99.0); // 99s — exceeds all buckets
        // All buckets should be 0 (none of them have upper >= 99)
        for bucket in &r.query_duration_buckets {
            assert_eq!(bucket.load(Ordering::Relaxed), 0);
        }
        // But count and sum should still be updated
        assert_eq!(r.query_duration_count.load(Ordering::Relaxed), 1);
        let sum = f64::from_bits(r.query_duration_sum.load(Ordering::Relaxed));
        assert!((sum - 99.0).abs() < 1e-10);
    }

    #[test]
    fn gauge_can_increment_and_decrement() {
        let r = MetricsRegistry::new(256);
        r.connections_active.fetch_add(1, Ordering::Relaxed);
        r.connections_active.fetch_add(1, Ordering::Relaxed);
        r.connections_active.fetch_sub(1, Ordering::Relaxed);
        assert_eq!(r.connections_active.load(Ordering::Relaxed), 1);
    }
}
