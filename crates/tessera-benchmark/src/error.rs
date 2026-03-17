//! Benchmark error types.

/// Errors produced by the benchmark harness.
#[derive(Debug, thiserror::Error)]
pub enum BenchmarkError {
    /// An error from the underlying graph engine.
    #[error("graph error: {0}")]
    Graph(#[from] tessera_graph::Error),

    /// A scenario-level error (setup, validation, etc.).
    #[error("scenario error: {0}")]
    Scenario(String),

    /// A report generation error.
    #[error("report error: {0}")]
    Report(String),

    /// An error from an external target (e.g. Memgraph via Bolt).
    #[error("external target error: {0}")]
    External(String),
}

impl BenchmarkError {
    /// Creates a scenario error from any string-like value.
    pub fn scenario(msg: impl Into<String>) -> Self {
        Self::Scenario(msg.into())
    }

    /// Creates a report error from any string-like value.
    pub fn report(msg: impl Into<String>) -> Self {
        Self::Report(msg.into())
    }

    /// Creates an external target error from any string-like value.
    pub fn external(msg: impl Into<String>) -> Self {
        Self::External(msg.into())
    }
}

/// Convenience alias used throughout the crate.
pub type Result<T> = std::result::Result<T, BenchmarkError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_display_is_non_empty() {
        let e = BenchmarkError::scenario("something went wrong");
        assert!(!e.to_string().is_empty());
        assert!(e.to_string().contains("something went wrong"));
    }

    #[test]
    fn report_error_display() {
        let e = BenchmarkError::report("write failed");
        assert!(e.to_string().contains("write failed"));
    }

    #[test]
    fn external_error_display() {
        let e = BenchmarkError::external("bolt connection refused");
        assert!(e.to_string().contains("bolt connection refused"));
    }
}
