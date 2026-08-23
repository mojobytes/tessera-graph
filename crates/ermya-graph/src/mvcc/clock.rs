// SPDX-License-Identifier: MIT

//! The transaction clock: a monotonic source of visibility timestamps.

use std::sync::atomic::{AtomicU64, Ordering};

/// A monotonic clock handing out strictly increasing transaction timestamps.
///
/// A transaction takes a `start_ts` on begin (its snapshot: it sees everything
/// committed strictly before that value) and a `commit_ts` on commit (the point
/// at which its writes become visible to newer transactions). Both come from
/// [`TxnClock::next`].
///
/// Timestamp `0` is never issued: it is reserved as "no transaction" / the empty
/// initial snapshot, mirroring how `NodeId`/`EdgeId` start at 1.
#[derive(Debug)]
pub struct TxnClock(AtomicU64);

impl TxnClock {
    /// Creates a clock whose first [`next`](Self::next) returns 1.
    pub const fn new() -> Self {
        Self(AtomicU64::new(1))
    }

    /// Returns the next timestamp and advances the clock.
    ///
    /// Strictly increasing across concurrent callers (`SeqCst` `fetch_add`).
    pub fn next(&self) -> u64 {
        self.0.fetch_add(1, Ordering::SeqCst)
    }

    /// Returns the timestamp the next [`next`](Self::next) would issue, without
    /// advancing. Used by auto-commit reads to snapshot the current instant.
    pub fn current(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::TxnClock;

    #[test]
    fn txn_clock_issues_strictly_increasing_timestamps() {
        let clock = TxnClock::new();
        let t1 = clock.next();
        let t2 = clock.next();
        assert!(t2 > t1, "expected {t2} > {t1}");
    }

    #[test]
    fn txn_clock_starts_above_zero() {
        let clock = TxnClock::new();
        assert!(
            clock.next() > 0,
            "timestamp 0 is reserved for 'no transaction'"
        );
    }

    #[test]
    fn txn_clock_current_does_not_advance() {
        let clock = TxnClock::new();
        let before = clock.current();
        let after = clock.current();
        assert_eq!(before, after, "current() must not advance the clock");

        let t1 = clock.next();
        assert!(
            clock.current() >= t1,
            "current() must reflect issued timestamps"
        );
    }
}
