// SPDX-License-Identifier: BSL-1.1

//! Latency sample aggregation for the lock-contention benchmark.

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Aggregated latency statistics over a set of samples, in seconds.
///
/// Field semantics mirror the Python `LatencyTracker.get_stats()` in the
/// sibling `../tessera/benchmarks` harness so numbers are comparable across
/// both repos.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct LatencyStats {
    pub p50: f64,
    pub p95: f64,
    pub p99: f64,
    pub mean: f64,
    pub min: f64,
    pub max: f64,
    pub count: u64,
}

impl LatencyStats {
    /// The all-zero stats returned when there are no samples.
    const fn zeroed() -> Self {
        Self {
            p50: 0.0,
            p95: 0.0,
            p99: 0.0,
            mean: 0.0,
            min: 0.0,
            max: 0.0,
            count: 0,
        }
    }
}

/// Returns the `p`-th percentile (`p` in `[0, 100]`) of an already-sorted
/// slice of samples, using linear interpolation between closest ranks.
///
/// This matches `numpy.percentile`'s default (`method="linear"`): the rank is
/// `(n - 1) * p / 100`, and the result interpolates linearly between the two
/// samples bracketing that fractional rank.
///
/// # Precondition
/// `sorted_samples_secs` MUST be sorted ascending. An empty slice returns
/// `0.0`.
#[must_use]
pub fn percentile(sorted_samples_secs: &[f64], p: f64) -> f64 {
    if sorted_samples_secs.is_empty() {
        return 0.0;
    }
    let n = sorted_samples_secs.len();
    if n == 1 {
        return sorted_samples_secs[0];
    }
    #[allow(clippy::cast_precision_loss)]
    let rank = (n as f64 - 1.0) * (p / 100.0);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let lo = rank.floor() as usize;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let hi = rank.ceil() as usize;
    let frac = rank - rank.floor();
    sorted_samples_secs[lo] + (sorted_samples_secs[hi] - sorted_samples_secs[lo]) * frac
}

/// Accumulates latency samples and computes percentile statistics.
///
/// Mirrors the Python `LatencyTracker` in `../tessera/benchmarks/src/metrics.py`.
#[derive(Debug, Default)]
pub struct LatencyTracker {
    samples: Vec<f64>,
}

impl LatencyTracker {
    #[must_use]
    pub fn new() -> Self {
        Self {
            samples: Vec::new(),
        }
    }

    /// Records one latency sample.
    pub fn record(&mut self, d: Duration) {
        self.samples.push(d.as_secs_f64());
    }

    /// Computes `{p50, p95, p99, mean, min, max, count}` over the recorded
    /// samples. Returns all-zero stats (with `count == 0`) if none were
    /// recorded. Sorts a local copy; the recorded samples are left untouched.
    #[must_use]
    pub fn stats(&self) -> LatencyStats {
        if self.samples.is_empty() {
            return LatencyStats::zeroed();
        }
        let mut sorted = self.samples.clone();
        sorted.sort_by(f64::total_cmp);
        let sum: f64 = sorted.iter().sum();
        #[allow(clippy::cast_precision_loss)]
        let mean = sum / sorted.len() as f64;
        LatencyStats {
            p50: percentile(&sorted, 50.0),
            p95: percentile(&sorted, 95.0),
            p99: percentile(&sorted, 99.0),
            mean,
            min: sorted[0],
            max: sorted[sorted.len() - 1],
            count: sorted.len() as u64,
        }
    }
}

#[cfg(test)]
// These tests compare against values built to be exactly representable (0.0,
// 42.0, and interpolations checked within a 1e-9 tolerance), so the exact
// float comparisons are intentional, not accidental precision assumptions.
#[allow(clippy::float_cmp)]
mod tests {
    use super::*;

    #[test]
    fn percentile_of_sorted_empty_returns_zero() {
        assert_eq!(percentile(&[], 50.0), 0.0);
    }

    #[test]
    fn percentile_of_single_sample_returns_that_sample() {
        assert_eq!(percentile(&[42.0], 0.0), 42.0);
        assert_eq!(percentile(&[42.0], 99.0), 42.0);
    }

    #[test]
    fn percentile_p50_matches_linear_interpolation_known_vector() {
        let v = [1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0, 10.0];
        assert!((percentile(&v, 50.0) - 5.5).abs() < 1e-9);
    }

    #[test]
    fn percentile_p95_and_p99_match_known_vector() {
        let v = [1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0, 10.0];
        assert!((percentile(&v, 95.0) - 9.55).abs() < 1e-9);
        assert!((percentile(&v, 99.0) - 9.91).abs() < 1e-9);
    }

    #[test]
    fn latency_tracker_record_and_stats_returns_all_fields() {
        let mut t = LatencyTracker::new();
        for ms in 1..=10 {
            t.record(Duration::from_millis(ms));
        }
        let s = t.stats();
        assert_eq!(s.count, 10);
        assert!((s.min - 0.001).abs() < 1e-9);
        assert!((s.max - 0.010).abs() < 1e-9);
        // p50 of 1ms..10ms (in seconds) == 5.5ms.
        assert!((s.p50 - 0.0055).abs() < 1e-9);
    }

    #[test]
    fn latency_tracker_stats_on_empty_returns_zeroed_stats_with_count_zero() {
        let s = LatencyTracker::new().stats();
        assert_eq!(s.count, 0);
        assert_eq!(s.p50, 0.0);
        assert_eq!(s.p95, 0.0);
        assert_eq!(s.p99, 0.0);
        assert_eq!(s.mean, 0.0);
        assert_eq!(s.min, 0.0);
        assert_eq!(s.max, 0.0);
    }
}
