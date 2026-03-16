//! Timing and measurement utilities for benchmark scenarios.

use std::time::Instant;

/// A collection of raw timing samples in nanoseconds.
#[derive(Debug, Clone)]
pub struct Measurement {
    samples_ns: Vec<u64>,
}

impl Measurement {
    /// Creates a measurement from pre-collected nanosecond samples.
    #[must_use]
    pub fn from_nanos(samples: &[u64]) -> Self {
        Self {
            samples_ns: samples.to_vec(),
        }
    }

    /// Returns the number of collected samples.
    #[must_use]
    pub fn sample_count(&self) -> usize {
        self.samples_ns.len()
    }

    /// Arithmetic mean of all samples in nanoseconds.
    #[must_use]
    pub fn mean_ns(&self) -> u64 {
        if self.samples_ns.is_empty() {
            return 0;
        }
        let sum: u64 = self.samples_ns.iter().sum();
        sum / self.samples_ns.len() as u64
    }

    /// 50th percentile (median) in nanoseconds.
    #[must_use]
    pub fn p50_ns(&self) -> u64 {
        self.percentile_ns(0.50)
    }

    /// 95th percentile in nanoseconds.
    #[must_use]
    pub fn p95_ns(&self) -> u64 {
        self.percentile_ns(0.95)
    }

    /// 99th percentile in nanoseconds.
    #[must_use]
    pub fn p99_ns(&self) -> u64 {
        self.percentile_ns(0.99)
    }

    /// Throughput as operations per second, derived from `mean_ns`.
    ///
    /// Returns 0 if the mean is zero (no samples or all zero-duration).
    #[must_use]
    pub fn throughput_ops_per_sec(&self) -> u64 {
        let mean = self.mean_ns();
        if mean == 0 {
            return 0;
        }
        1_000_000_000 / mean
    }

    /// Nearest-rank percentile on a sorted clone.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    fn percentile_ns(&self, pct: f64) -> u64 {
        if self.samples_ns.is_empty() {
            return 0;
        }
        let mut sorted = self.samples_ns.clone();
        sorted.sort_unstable();
        let rank = (pct * sorted.len() as f64).ceil() as usize;
        let idx = rank.min(sorted.len()).saturating_sub(1);
        sorted[idx]
    }
}

/// A simple wall-clock timer that runs a closure `iters` times and
/// collects per-iteration timings.
pub struct Timer;

impl Timer {
    /// Runs `f` exactly `iters` times, collecting a nanosecond sample
    /// for each invocation.
    #[allow(clippy::cast_possible_truncation)]
    pub fn run(iters: usize, mut f: impl FnMut()) -> Measurement {
        let mut samples = Vec::with_capacity(iters);
        for _ in 0..iters {
            let start = Instant::now();
            f();
            samples.push(start.elapsed().as_nanos() as u64);
        }
        Measurement::from_nanos(&samples)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mean_of_known_samples() {
        let m = Measurement::from_nanos(&[100, 200, 300]);
        assert_eq!(m.mean_ns(), 200);
    }

    #[test]
    fn mean_of_empty_is_zero() {
        let m = Measurement::from_nanos(&[]);
        assert_eq!(m.mean_ns(), 0);
    }

    #[test]
    fn percentiles_on_sorted_100_samples() {
        let samples: Vec<u64> = (1..=100).collect();
        let m = Measurement::from_nanos(&samples);
        assert_eq!(m.p50_ns(), 50);
        assert_eq!(m.p95_ns(), 95);
        assert_eq!(m.p99_ns(), 99);
    }

    #[test]
    fn percentiles_on_empty() {
        let m = Measurement::from_nanos(&[]);
        assert_eq!(m.p50_ns(), 0);
        assert_eq!(m.p95_ns(), 0);
        assert_eq!(m.p99_ns(), 0);
    }

    #[test]
    fn throughput_1000_ops_in_1s() {
        // 1000 ops, each taking 1_000_000 ns (1 ms) → 1000 ops/s
        let samples = vec![1_000_000u64; 1000];
        let m = Measurement::from_nanos(&samples);
        assert_eq!(m.throughput_ops_per_sec(), 1_000);
    }

    #[test]
    fn throughput_zero_mean_returns_zero() {
        let m = Measurement::from_nanos(&[]);
        assert_eq!(m.throughput_ops_per_sec(), 0);
    }

    #[test]
    fn timer_collects_correct_sample_count() {
        let m = Timer::run(10, || {
            // minimal work
            std::hint::black_box(42);
        });
        assert_eq!(m.sample_count(), 10);
    }

    #[test]
    fn timer_samples_are_non_zero() {
        let m = Timer::run(5, || {
            std::thread::sleep(std::time::Duration::from_micros(10));
        });
        assert!(m.mean_ns() > 0);
    }
}
