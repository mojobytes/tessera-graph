// SPDX-License-Identifier: BSL-1.1

//! Declarative benchmark matrix: the single source of truth for which
//! lock-contention scenarios exist and which are runnable in-process.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// The Cypher mutation shape exercised by a matrix point.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum Scenario {
    MatchCreate,
    MatchSet,
    Merge,
    Unwind,
}

impl Scenario {
    fn as_str(self) -> &'static str {
        match self {
            Scenario::MatchCreate => "match-create",
            Scenario::MatchSet => "match-set",
            Scenario::Merge => "merge",
            Scenario::Unwind => "unwind",
        }
    }
}

impl fmt::Display for Scenario {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Scenario {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "match-create" => Ok(Scenario::MatchCreate),
            "match-set" => Ok(Scenario::MatchSet),
            "merge" => Ok(Scenario::Merge),
            "unwind" => Ok(Scenario::Unwind),
            other => Err(format!("unknown scenario: {other}")),
        }
    }
}

/// The lock discipline applied to the mutation path.
///
/// - `TwoLockCurrent`: production today — read lock for the MATCH binding
///   phase, released, then a write lock for the write phase.
/// - `SingleLockA`: the candidate Option A — one write lock held across the
///   whole mutation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum Variant {
    TwoLockCurrent,
    SingleLockA,
}

impl Variant {
    fn as_str(self) -> &'static str {
        match self {
            Variant::TwoLockCurrent => "two-lock-current",
            Variant::SingleLockA => "single-lock-a",
        }
    }
}

impl fmt::Display for Variant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Variant {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "two-lock-current" => Ok(Variant::TwoLockCurrent),
            "single-lock-a" => Ok(Variant::SingleLockA),
            other => Err(format!("unknown variant: {other}")),
        }
    }
}

/// How the client reaches the server for a matrix point. `InProcess` is
/// Scenario 1 (direct `Arc<RwLock<Graph>>`); `BoltDockerTls` is Scenario 2
/// (real Bolt driver against a Docker container), declared but not runnable
/// by the in-process harness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum Transport {
    InProcess,
    BoltDockerTls,
}

impl Transport {
    fn as_str(self) -> &'static str {
        match self {
            Transport::InProcess => "in-process",
            Transport::BoltDockerTls => "bolt-docker-tls",
        }
    }
}

impl fmt::Display for Transport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Transport {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "in-process" => Ok(Transport::InProcess),
            "bolt-docker-tls" => Ok(Transport::BoltDockerTls),
            other => Err(format!("unknown transport: {other}")),
        }
    }
}

// `serde(try_from = "String", into = "String")` bridges the kebab-case wire
// form to the enums above without a second spelling of each variant.
macro_rules! string_serde_bridge {
    ($ty:ty) => {
        impl TryFrom<String> for $ty {
            type Error = String;
            fn try_from(s: String) -> Result<Self, Self::Error> {
                s.parse()
            }
        }
        impl From<$ty> for String {
            fn from(v: $ty) -> String {
                v.to_string()
            }
        }
    };
}
string_serde_bridge!(Scenario);
string_serde_bridge!(Variant);
string_serde_bridge!(Transport);

/// One point in the benchmark matrix — a fully specified scenario.
///
/// The stable key is [`MatrixPoint::name`], always computed from the axes; the
/// TOML never carries a `name` field, so there is no way for a hand-written
/// name to drift from the axes it claims to describe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MatrixPoint {
    pub scenario: Scenario,
    pub readers: u32,
    pub writers: u32,
    pub dataset_size: u32,
    pub variant: Variant,
    pub transport: Transport,
    pub runnable: bool,
}

impl MatrixPoint {
    /// The stable, axis-derived key for this point, e.g.
    /// `"match-create-r4-w2-d1000-two-lock-current"`. Transport is omitted
    /// from the in-process name and encoded by the caller for Scenario 2.
    #[must_use]
    pub fn name(&self) -> String {
        let base = format!(
            "{}-r{}-w{}-d{}-{}",
            self.scenario, self.readers, self.writers, self.dataset_size, self.variant,
        );
        match self.transport {
            Transport::InProcess => base,
            // Bolt-Docker points append the transport as a stable suffix so the
            // in-process base key stays a substring of the name.
            Transport::BoltDockerTls => format!("{base}-bolt-docker-tls"),
        }
    }
}

#[derive(Debug, Deserialize)]
struct MatrixFile {
    #[serde(default)]
    point: Vec<MatrixPoint>,
}

/// Parses the declarative matrix TOML into its points. The file is the single
/// source of truth for both Scenario 1 (in-process) and Scenario 2 (declared,
/// not runnable here).
///
/// # Errors
/// Returns `Err` with a human-readable message if the TOML is malformed or a
/// field carries an unknown enum value.
pub fn parse_matrix(toml_str: &str) -> Result<Vec<MatrixPoint>, String> {
    let file: MatrixFile = toml::from_str(toml_str).map_err(|e| e.to_string())?;
    Ok(file.point)
}

/// Splits points into the runnable set (executed by the in-process harness)
/// and an explicit skip report for every non-runnable point — never a silent
/// drop. Each skip message names the point so Scenario-2 declarations are
/// visibly acknowledged, not swallowed.
#[must_use]
pub fn runnable_points_with_skip_report(points: &[MatrixPoint]) -> (Vec<&MatrixPoint>, Vec<String>) {
    let mut runnable = Vec::new();
    let mut skipped = Vec::new();
    for p in points {
        if p.runnable {
            runnable.push(p);
        } else {
            skipped.push(format!(
                "skip (not runnable in-process): {} [transport={}]",
                p.name(),
                p.transport,
            ));
        }
    }
    (runnable, skipped)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bench_support::test_helpers::sample_point;
    use std::collections::HashSet;

    #[test]
    fn matrix_point_name_encodes_all_axes() {
        assert_eq!(sample_point().name(), "match-create-r4-w2-d1000-two-lock-current");
    }

    #[test]
    fn matrix_point_name_changes_with_variant() {
        let mut p = sample_point();
        p.variant = Variant::SingleLockA;
        assert_eq!(p.name(), "match-create-r4-w2-d1000-single-lock-a");
    }

    #[test]
    fn matrix_point_name_is_unique_across_all_four_scenarios() {
        let scenarios =
            [Scenario::MatchCreate, Scenario::MatchSet, Scenario::Merge, Scenario::Unwind];
        let names: HashSet<String> = scenarios
            .iter()
            .map(|&s| {
                let mut p = sample_point();
                p.scenario = s;
                p.name()
            })
            .collect();
        assert_eq!(names.len(), 4);
    }

    #[test]
    fn scenario_from_str_roundtrip() {
        for s in [Scenario::MatchCreate, Scenario::MatchSet, Scenario::Merge, Scenario::Unwind] {
            let text = s.to_string();
            assert_eq!(text.parse::<Scenario>().unwrap(), s);
        }
    }

    #[test]
    fn scenario_from_str_rejects_unknown() {
        assert!("bogus".parse::<Scenario>().is_err());
    }

    const SAMPLE_TOML: &str = r#"
[[point]]
scenario = "match-create"
readers = 4
writers = 2
dataset_size = 1000
variant = "two-lock-current"
transport = "in-process"
runnable = true

[[point]]
scenario = "match-create"
readers = 4
writers = 2
dataset_size = 1000
variant = "single-lock-a"
transport = "in-process"
runnable = true

[[point]]
scenario = "match-create"
readers = 4
writers = 2
dataset_size = 1000
variant = "two-lock-current"
transport = "bolt-docker-tls"
runnable = false
"#;

    #[test]
    fn parse_matrix_toml_returns_all_declared_points() {
        let points = parse_matrix(SAMPLE_TOML).unwrap();
        assert_eq!(points.len(), 3);
    }

    #[test]
    fn parse_matrix_toml_filters_runnable_true_for_execution() {
        let points = parse_matrix(SAMPLE_TOML).unwrap();
        assert_eq!(points.iter().filter(|p| p.runnable).count(), 2);
    }

    #[test]
    fn parse_matrix_toml_skipped_points_report_via_explicit_marker() {
        let points = parse_matrix(SAMPLE_TOML).unwrap();
        let (runnable, skipped) = runnable_points_with_skip_report(&points);
        assert_eq!(runnable.len(), 2);
        assert_eq!(skipped.len(), 1);
        assert!(skipped[0].contains("match-create-r4-w2-d1000-two-lock-current"));
        assert!(skipped[0].to_lowercase().contains("skip"));
    }

    #[test]
    fn parse_matrix_toml_rejects_malformed_toml() {
        assert!(parse_matrix("[[point]]\nscenario = ").is_err());
    }
}
