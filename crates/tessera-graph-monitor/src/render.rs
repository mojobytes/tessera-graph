// Copyright 2026 BelowZero Security OU. All rights reserved.

//! Prometheus text exposition format renderer.

use std::fmt::Write;
use std::sync::atomic::Ordering;

use crate::registry::{HISTOGRAM_BUCKETS, MetricsRegistry};

/// Render all metrics in Prometheus text exposition format (version 0.0.4).
#[must_use]
#[allow(clippy::too_many_lines)] // Single render function — splitting would hurt readability.
pub fn render_prometheus(registry: &MetricsRegistry) -> String {
    let mut buf = String::with_capacity(4096);

    // --- Gauges ---
    write_gauge(
        &mut buf,
        "tessera_connections_active",
        "Current number of active connections",
        registry.connections_active.load(Ordering::Relaxed),
    );
    write_gauge(
        &mut buf,
        "tessera_connections_max",
        "Configured maximum connections",
        registry.connections_max.load(Ordering::Relaxed),
    );

    // --- Counters ---
    write_counter(
        &mut buf,
        "tessera_connections_accepted_total",
        "Total accepted connections",
        registry.connections_accepted.load(Ordering::Relaxed),
    );
    write_counter(
        &mut buf,
        "tessera_connections_rejected_total",
        "Total rejected connections (at capacity)",
        registry.connections_rejected.load(Ordering::Relaxed),
    );

    // Auth attempts with result label
    let _ = writeln!(
        buf,
        "# HELP tessera_graph_auth_attempts_total Total authentication attempts"
    );
    let _ = writeln!(buf, "# TYPE tessera_graph_auth_attempts_total counter");
    write_labeled(
        &mut buf,
        "tessera_graph_auth_attempts_total",
        r#"result="success""#,
        registry.auth_success.load(Ordering::Relaxed),
    );
    write_labeled(
        &mut buf,
        "tessera_graph_auth_attempts_total",
        r#"result="failure""#,
        registry.auth_failure.load(Ordering::Relaxed),
    );

    // Queries with language + type labels
    let _ = writeln!(buf, "# HELP tessera_queries_total Total queries executed");
    let _ = writeln!(buf, "# TYPE tessera_queries_total counter");
    write_labeled(
        &mut buf,
        "tessera_queries_total",
        r#"language="gql",type="read""#,
        registry.queries_gql_read.load(Ordering::Relaxed),
    );
    write_labeled(
        &mut buf,
        "tessera_queries_total",
        r#"language="gql",type="mutation""#,
        registry.queries_gql_mutation.load(Ordering::Relaxed),
    );
    write_labeled(
        &mut buf,
        "tessera_queries_total",
        r#"language="cypher",type="read""#,
        registry.queries_cypher_read.load(Ordering::Relaxed),
    );
    write_labeled(
        &mut buf,
        "tessera_queries_total",
        r#"language="cypher",type="mutation""#,
        registry.queries_cypher_mutation.load(Ordering::Relaxed),
    );

    write_counter(
        &mut buf,
        "tessera_query_errors_total",
        "Total query errors",
        registry.query_errors.load(Ordering::Relaxed),
    );
    write_counter(
        &mut buf,
        "tessera_tls_handshake_failures_total",
        "Total TLS handshake failures",
        registry.tls_handshake_failures.load(Ordering::Relaxed),
    );

    // --- System metrics ---
    write_gauge(
        &mut buf,
        "tessera_process_rss_bytes",
        "Resident Set Size of the server process in bytes",
        registry.process_rss_bytes.load(Ordering::Relaxed),
    );
    write_gauge(
        &mut buf,
        "tessera_open_file_descriptors",
        "Number of open file descriptors",
        registry.open_fds.load(Ordering::Relaxed),
    );
    write_counter(
        &mut buf,
        "tessera_audit_entries_dropped_total",
        "Audit entries dropped due to channel overflow",
        registry.audit_entries_dropped.load(Ordering::Relaxed),
    );
    write_gauge(
        &mut buf,
        "tessera_tenants_loaded",
        "Number of tenant graphs currently loaded in memory",
        registry.tenants_loaded.load(Ordering::Relaxed),
    );

    // --- Histogram: query duration ---
    let _ = writeln!(
        buf,
        "# HELP tessera_query_duration_seconds Query execution duration in seconds"
    );
    let _ = writeln!(buf, "# TYPE tessera_query_duration_seconds histogram");

    for (i, &upper) in HISTOGRAM_BUCKETS.iter().enumerate() {
        let count = registry.query_duration_buckets[i].load(Ordering::Relaxed);
        let le = format_bucket_le(upper);
        let _ = writeln!(
            buf,
            r#"tessera_query_duration_seconds_bucket{{le="{le}"}} {count}"#
        );
    }
    // +Inf bucket = total count
    let total_count = registry.query_duration_count.load(Ordering::Relaxed);
    let _ = writeln!(
        buf,
        r#"tessera_query_duration_seconds_bucket{{le="+Inf"}} {total_count}"#
    );

    let sum = f64::from_bits(registry.query_duration_sum.load(Ordering::Relaxed));
    let _ = writeln!(buf, "tessera_query_duration_seconds_sum {sum}");
    let _ = writeln!(buf, "tessera_query_duration_seconds_count {total_count}");

    buf
}

fn write_gauge(buf: &mut String, name: &str, help: &str, value: u64) {
    let _ = writeln!(buf, "# HELP {name} {help}");
    let _ = writeln!(buf, "# TYPE {name} gauge");
    let _ = writeln!(buf, "{name} {value}");
}

fn write_counter(buf: &mut String, name: &str, help: &str, value: u64) {
    let _ = writeln!(buf, "# HELP {name} {help}");
    let _ = writeln!(buf, "# TYPE {name} counter");
    let _ = writeln!(buf, "{name} {value}");
}

fn write_labeled(buf: &mut String, name: &str, labels: &str, value: u64) {
    let _ = writeln!(buf, "{name}{{{labels}}} {value}");
}

/// Format a histogram bucket upper bound with appropriate precision.
fn format_bucket_le(upper: f64) -> String {
    if upper < 0.1 {
        format!("{upper:.3}")
    } else if upper < 1.0 {
        format!("{upper:.2}")
    } else {
        format!("{upper:.1}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_contains_gauge_metadata_and_value() {
        let r = MetricsRegistry::new(256);
        r.connections_active.store(3, Ordering::Relaxed);
        let output = render_prometheus(&r);
        assert!(output.contains("# HELP tessera_connections_active"));
        assert!(output.contains("# TYPE tessera_connections_active gauge"));
        assert!(output.contains("tessera_connections_active 3\n"));
        assert!(output.contains("tessera_connections_max 256\n"));
    }

    #[test]
    fn render_counter_with_label() {
        let r = MetricsRegistry::new(256);
        r.auth_success.store(10, Ordering::Relaxed);
        r.auth_failure.store(2, Ordering::Relaxed);
        let output = render_prometheus(&r);
        assert!(output.contains(r#"tessera_graph_auth_attempts_total{result="success"} 10"#));
        assert!(output.contains(r#"tessera_graph_auth_attempts_total{result="failure"} 2"#));
    }

    #[test]
    fn render_query_counter_with_two_labels() {
        let r = MetricsRegistry::new(256);
        r.queries_gql_read.store(5, Ordering::Relaxed);
        let output = render_prometheus(&r);
        assert!(output.contains(r#"tessera_queries_total{language="gql",type="read"} 5"#));
    }

    #[test]
    fn render_histogram_buckets_and_sum() {
        let r = MetricsRegistry::new(256);
        r.record_query_duration(0.007);
        let output = render_prometheus(&r);
        assert!(output.contains(r#"tessera_query_duration_seconds_bucket{le="0.001"} 0"#));
        assert!(output.contains(r#"tessera_query_duration_seconds_bucket{le="0.010"} 1"#));
        assert!(output.contains(r#"tessera_query_duration_seconds_bucket{le="+Inf"} 1"#));
        assert!(output.contains("tessera_query_duration_seconds_count 1\n"));
        assert!(output.contains("tessera_query_duration_seconds_sum "));
    }

    #[test]
    fn render_all_zero_metrics_valid() {
        let r = MetricsRegistry::new(0);
        let output = render_prometheus(&r);
        // Should contain all metric families even when zero
        assert!(output.contains("tessera_connections_active 0"));
        assert!(output.contains("tessera_connections_accepted_total 0"));
        assert!(output.contains("tessera_query_duration_seconds_count 0"));
    }

    #[test]
    fn render_histogram_all_buckets_present() {
        let r = MetricsRegistry::new(256);
        let output = render_prometheus(&r);
        // All 12 bucket lines + +Inf
        assert_eq!(
            output
                .matches("tessera_query_duration_seconds_bucket")
                .count(),
            13
        );
    }

    #[test]
    fn format_bucket_le_precision() {
        assert_eq!(format_bucket_le(0.001), "0.001");
        assert_eq!(format_bucket_le(0.005), "0.005");
        assert_eq!(format_bucket_le(0.01), "0.010");
        assert_eq!(format_bucket_le(0.025), "0.025");
        assert_eq!(format_bucket_le(0.05), "0.050");
        assert_eq!(format_bucket_le(0.1), "0.10");
        assert_eq!(format_bucket_le(0.25), "0.25");
        assert_eq!(format_bucket_le(0.5), "0.50");
        assert_eq!(format_bucket_le(1.0), "1.0");
        assert_eq!(format_bucket_le(2.5), "2.5");
        assert_eq!(format_bucket_le(5.0), "5.0");
        assert_eq!(format_bucket_le(10.0), "10.0");
    }
}
