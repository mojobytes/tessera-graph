// SPDX-License-Identifier: BSL-1.1

//! Versioned JSON result schema for the lock-contention benchmark.
//!
//! One [`BenchReport`] per matrix point. The `schema_version` field lets a
//! consumer reject results produced by an incompatible harness; the top-level
//! `name` field is the stable, axis-derived key (also present inside `point`)
//! so a run's JSON is comparable at a glance across runs.

use serde::{Deserialize, Serialize};

use crate::bench_support::latency::LatencyStats;
use crate::bench_support::matrix::MatrixPoint;

/// Current result schema version. Bump on any breaking change to the shape.
pub const REPORT_SCHEMA_VERSION: u32 = 1;

/// The benchmark result for a single matrix point.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BenchReport {
    pub schema_version: u32,
    pub name: String,
    pub point: MatrixPoint,
    pub reader_stats: LatencyStats,
    pub writer_stats: LatencyStats,
    pub sample_count_readers: u64,
    pub sample_count_writers: u64,
}

impl BenchReport {
    /// Builds a report for `point`, stamping the current schema version and the
    /// axis-derived stable key.
    #[must_use]
    pub fn new(
        point: MatrixPoint,
        reader_stats: LatencyStats,
        writer_stats: LatencyStats,
        sample_count_readers: u64,
        sample_count_writers: u64,
    ) -> Self {
        Self {
            schema_version: REPORT_SCHEMA_VERSION,
            name: point.name(),
            point,
            reader_stats,
            writer_stats,
            sample_count_readers,
            sample_count_writers,
        }
    }
}

/// Parses a `BenchReport` from JSON, rejecting a mismatched schema version.
///
/// The bare `Deserialize` would happily accept any `schema_version` integer;
/// this checks it explicitly so a consumer never silently misreads results
/// produced by an incompatible harness.
///
/// # Errors
/// Returns `Err` if the JSON is malformed or `schema_version` differs from
/// [`REPORT_SCHEMA_VERSION`].
pub fn from_json_checked(s: &str) -> Result<BenchReport, String> {
    let report: BenchReport = serde_json::from_str(s).map_err(|e| e.to_string())?;
    if report.schema_version != REPORT_SCHEMA_VERSION {
        return Err(format!(
            "unsupported report schema_version {} (this harness produces {REPORT_SCHEMA_VERSION})",
            report.schema_version,
        ));
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bench_support::latency::LatencyStats;
    use crate::bench_support::test_helpers::sample_point;

    fn sample_stats(p50: f64) -> LatencyStats {
        LatencyStats {
            p50,
            p95: p50,
            p99: p50,
            mean: p50,
            min: p50,
            max: p50,
            count: 1,
        }
    }

    fn sample_report() -> BenchReport {
        BenchReport::new(
            sample_point(),
            sample_stats(0.001),
            sample_stats(0.002),
            40,
            10,
        )
    }

    #[test]
    fn report_serializes_stable_keys_as_json_literals() {
        // Both the schema version and the axis-derived name must be written into
        // the JSON as literal top-level fields (not merely survive a roundtrip),
        // so a consumer can read either at a glance without deserializing.
        let report = sample_report();
        let value: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&report).unwrap()).unwrap();
        assert_eq!(
            value["schema_version"],
            serde_json::json!(REPORT_SCHEMA_VERSION),
            "schema_version literal",
        );
        assert_eq!(value["name"], report.point.name(), "stable name literal");
    }

    #[test]
    fn report_roundtrips_through_json_serde_preserving_schema_version() {
        let report = sample_report();
        let json = serde_json::to_string(&report).unwrap();
        let back: BenchReport = serde_json::from_str(&json).unwrap();
        assert_eq!(report, back, "full roundtrip equality");
        assert_eq!(
            back.schema_version, REPORT_SCHEMA_VERSION,
            "version survives roundtrip"
        );
    }

    #[test]
    fn report_rejects_unknown_schema_version_on_deserialize() {
        let mut value: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&sample_report()).unwrap()).unwrap();
        value["schema_version"] = serde_json::json!(999);
        let bad = serde_json::to_string(&value).unwrap();
        assert!(from_json_checked(&bad).is_err());
    }

    #[test]
    fn report_collection_serializes_as_json_array_of_reports() {
        let reports = vec![sample_report(), sample_report()];
        let json = serde_json::to_string(&reports).unwrap();
        let back: Vec<BenchReport> = serde_json::from_str(&json).unwrap();
        assert_eq!(back.len(), 2);
    }
}
