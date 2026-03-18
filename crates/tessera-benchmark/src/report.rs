//! Report generation in JSON and CSV formats.

use std::fmt::Write as _;

/// A single benchmark result entry.
#[derive(Debug, Clone)]
#[must_use]
pub struct ReportEntry {
    /// Scenario name (e.g. `"write"`, `"traversal"`).
    pub scenario: String,
    /// Target name (e.g. `"tessera"`, `"memgraph"`).
    pub target: String,
    /// Throughput in operations per second.
    pub throughput_ops_per_sec: u64,
    /// Mean latency in nanoseconds.
    pub mean_latency_ns: u64,
    /// 50th percentile latency in nanoseconds.
    pub p50_ns: u64,
    /// 95th percentile latency in nanoseconds.
    pub p95_ns: u64,
    /// 99th percentile latency in nanoseconds.
    pub p99_ns: u64,
}

impl ReportEntry {
    /// Serialises this entry to a JSON object string (no trailing newline).
    #[must_use]
    pub fn to_json(&self) -> String {
        format!(
            "{{\"scenario\":\"{}\",\"target\":\"{}\",\"throughput_ops_per_sec\":{},\
             \"mean_latency_ns\":{},\"p50_ns\":{},\"p95_ns\":{},\"p99_ns\":{}}}",
            self.scenario,
            self.target,
            self.throughput_ops_per_sec,
            self.mean_latency_ns,
            self.p50_ns,
            self.p95_ns,
            self.p99_ns,
        )
    }
}

/// Accumulates [`ReportEntry`] values and produces JSON or CSV output.
#[derive(Debug, Clone)]
#[must_use]
pub struct Report {
    entries: Vec<ReportEntry>,
}

impl Default for Report {
    fn default() -> Self {
        Self::new()
    }
}

impl Report {
    /// Creates an empty report.
    pub const fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// Appends a result entry.
    pub fn add(&mut self, entry: ReportEntry) {
        self.entries.push(entry);
    }

    /// Returns the number of entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns `true` if the report has no entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Serialises all entries to a JSON array string.
    #[must_use]
    pub fn to_json(&self) -> String {
        let mut buf = String::from("[");
        for (i, entry) in self.entries.iter().enumerate() {
            if i > 0 {
                buf.push(',');
            }
            buf.push_str(&entry.to_json());
        }
        buf.push(']');
        buf
    }

    /// Serialises all entries to CSV with a header row.
    #[must_use]
    pub fn to_csv(&self) -> String {
        let mut buf = String::from(
            "scenario,target,throughput_ops_per_sec,mean_latency_ns,p50_ns,p95_ns,p99_ns\n",
        );
        for entry in &self.entries {
            let _ = writeln!(
                buf,
                "{},{},{},{},{},{},{}",
                entry.scenario,
                entry.target,
                entry.throughput_ops_per_sec,
                entry.mean_latency_ns,
                entry.p50_ns,
                entry.p95_ns,
                entry.p99_ns,
            );
        }
        buf
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_entry(scenario: &str, target: &str, throughput: u64) -> ReportEntry {
        ReportEntry {
            scenario: scenario.into(),
            target: target.into(),
            throughput_ops_per_sec: throughput,
            mean_latency_ns: throughput,
            p50_ns: throughput,
            p95_ns: throughput,
            p99_ns: throughput,
        }
    }

    #[test]
    fn report_entry_json_contains_scenario_name() {
        let entry = sample_entry("write", "tessera", 5000);
        let json = entry.to_json();
        assert!(json.contains("\"scenario\":\"write\""));
        assert!(json.contains("\"throughput_ops_per_sec\":5000"));
    }

    #[test]
    fn report_json_array_has_correct_entry_count() {
        let mut r = Report::new();
        r.add(sample_entry("a", "t", 1));
        r.add(sample_entry("b", "t", 2));
        let json = r.to_json();
        assert!(json.starts_with('['));
        assert!(json.ends_with(']'));
        assert_eq!(json.matches("\"scenario\"").count(), 2);
    }

    #[test]
    fn report_json_empty() {
        let r = Report::new();
        assert_eq!(r.to_json(), "[]");
    }

    #[test]
    fn report_csv_first_line_is_header() {
        let r = Report::new();
        let csv = r.to_csv();
        let first_line = csv.lines().next().unwrap();
        assert_eq!(
            first_line,
            "scenario,target,throughput_ops_per_sec,mean_latency_ns,p50_ns,p95_ns,p99_ns"
        );
    }

    #[test]
    fn report_csv_data_row() {
        let mut r = Report::new();
        r.add(sample_entry("write", "tessera", 100));
        let csv = r.to_csv();
        let lines: Vec<&str> = csv.lines().collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[1], "write,tessera,100,100,100,100,100");
    }
}
