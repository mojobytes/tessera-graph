//! CLI runner for the tessera-benchmark harness.

use tessera_benchmark::dataset::{ChainDataset, Dataset};
use tessera_benchmark::error::{BenchmarkError, Result};
use tessera_benchmark::target::BenchmarkTarget;
use tessera_benchmark::report::{Report, ReportEntry};
use tessera_benchmark::scenario::{
    MixedScenario, PathfindingScenario, ReadScenario, Scenario, ScenarioResult, TraversalScenario,
    WriteScenario,
};
use tessera_benchmark::tessera_target::TesseraTarget;

/// Supported benchmark targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Tessera,
}

/// Supported scenario kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScenarioKind {
    Write,
    Read,
    Traversal,
    Pathfinding,
    Mixed,
    All,
}

/// Output format for the report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    Json,
    Csv,
}

/// Parsed CLI arguments.
#[derive(Debug)]
pub struct CliArgs {
    pub target: Target,
    pub scenario: ScenarioKind,
    pub nodes: usize,
    pub edges: usize,
    pub depth: u32,
    pub iterations: usize,
    pub output: OutputFormat,
}

impl Default for CliArgs {
    fn default() -> Self {
        Self {
            target: Target::Tessera,
            scenario: ScenarioKind::All,
            nodes: 1_000,
            edges: 999,
            depth: 5,
            iterations: 100,
            output: OutputFormat::Json,
        }
    }
}

impl CliArgs {
    /// Parses arguments from an iterator of string slices (for testability).
    #[must_use]
    pub fn parse_from(args: &[&str]) -> Self {
        let mut result = Self::default();
        let mut i = 1; // skip binary name
        while i < args.len() {
            match args[i] {
                "--target" => {
                    i += 1;
                    if i < args.len() {
                        // Currently only Tessera target supported; Memgraph
                        // will be added behind the `memgraph` feature flag.
                        result.target = Target::Tessera;
                    }
                }
                "--scenario" => {
                    i += 1;
                    if i < args.len() {
                        result.scenario = match args[i] {
                            "write" => ScenarioKind::Write,
                            "read" => ScenarioKind::Read,
                            "traversal" => ScenarioKind::Traversal,
                            "pathfinding" => ScenarioKind::Pathfinding,
                            "mixed" => ScenarioKind::Mixed,
                            _ => ScenarioKind::All,
                        };
                    }
                }
                "--nodes" => {
                    i += 1;
                    if i < args.len() {
                        result.nodes = args[i].parse().unwrap_or(1_000);
                    }
                }
                "--edges" => {
                    i += 1;
                    if i < args.len() {
                        result.edges = args[i].parse().unwrap_or(999);
                    }
                }
                "--depth" => {
                    i += 1;
                    if i < args.len() {
                        result.depth = args[i].parse().unwrap_or(5);
                    }
                }
                "--iterations" => {
                    i += 1;
                    if i < args.len() {
                        result.iterations = args[i].parse().unwrap_or(100);
                    }
                }
                "--output" => {
                    i += 1;
                    if i < args.len() {
                        result.output = match args[i] {
                            "csv" => OutputFormat::Csv,
                            _ => OutputFormat::Json,
                        };
                    }
                }
                _ => {}
            }
            i += 1;
        }
        result
    }
}

fn result_to_entry(r: &ScenarioResult) -> ReportEntry {
    ReportEntry {
        scenario: r.scenario_name.clone(),
        target: r.target_name.clone(),
        throughput_ops_per_sec: r.throughput_ops_per_sec,
        mean_latency_ns: r.mean_latency_ns,
        p50_ns: r.p50_ns,
        p95_ns: r.p95_ns,
        p99_ns: r.p99_ns,
    }
}

/// Runs the selected scenario(s) and returns a report.
///
/// # Errors
///
/// Returns [`BenchmarkError`] if any scenario fails during execution.
pub fn run_scenario(args: &CliArgs) -> Result<Report> {
    let mut report = Report::new();
    let mut target = match args.target {
        Target::Tessera => TesseraTarget::new(),
    };

    let run_write = matches!(args.scenario, ScenarioKind::Write | ScenarioKind::All);
    let run_read = matches!(args.scenario, ScenarioKind::Read | ScenarioKind::All);
    let run_traversal = matches!(args.scenario, ScenarioKind::Traversal | ScenarioKind::All);
    let run_pathfinding = matches!(args.scenario, ScenarioKind::Pathfinding | ScenarioKind::All);
    let run_mixed = matches!(args.scenario, ScenarioKind::Mixed | ScenarioKind::All);

    if run_write {
        let s = WriteScenario {
            node_count: args.nodes,
            edge_count: args.edges,
        };
        let r = s.run(&mut target)?;
        report.add(result_to_entry(&r));
        target.clear();
    }

    if run_read {
        // Build a chain to have nodes to look up
        let ds = ChainDataset {
            length: args.nodes,
        };
        let dataset = ds.build(&mut target)?;
        let s = ReadScenario {
            node_handles: dataset.nodes,
            lookup_iterations: args.iterations,
        };
        let r = s.run(&mut target)?;
        report.add(result_to_entry(&r));
        target.clear();
    }

    if run_traversal {
        let ds = ChainDataset {
            length: args.nodes,
        };
        let dataset = ds.build(&mut target)?;
        let s = TraversalScenario {
            start: dataset.nodes[0],
            max_depth: args.depth,
            iterations: args.iterations,
        };
        let r = s.run(&mut target)?;
        report.add(result_to_entry(&r));
        target.clear();
    }

    if run_pathfinding {
        let ds = ChainDataset {
            length: args.nodes,
        };
        let dataset = ds.build(&mut target)?;
        let s = PathfindingScenario {
            from: dataset.nodes[0],
            to: *dataset.nodes.last().ok_or_else(|| {
                BenchmarkError::scenario("empty dataset for pathfinding")
            })?,
            iterations: args.iterations,
        };
        let r = s.run(&mut target)?;
        report.add(result_to_entry(&r));
        target.clear();
    }

    if run_mixed {
        let s = MixedScenario {
            write_ratio: 0.5,
            total_ops: args.iterations,
        };
        let r = s.run(&mut target)?;
        report.add(result_to_entry(&r));
        target.clear();
    }

    Ok(report)
}

fn main() {
    let raw_args: Vec<String> = std::env::args().collect();
    let args_refs: Vec<&str> = raw_args.iter().map(String::as_str).collect();
    let args = CliArgs::parse_from(&args_refs);

    match run_scenario(&args) {
        Ok(report) => {
            let output = match args.output {
                OutputFormat::Json => report.to_json(),
                OutputFormat::Csv => report.to_csv(),
            };
            println!("{output}");
        }
        Err(e) => {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_args_parse_tessera_write() {
        let args = CliArgs::parse_from(&[
            "tessera-bench",
            "--target",
            "tessera",
            "--scenario",
            "write",
            "--nodes",
            "1000",
        ]);
        assert_eq!(args.target, Target::Tessera);
        assert_eq!(args.scenario, ScenarioKind::Write);
        assert_eq!(args.nodes, 1000);
    }

    #[test]
    fn cli_args_defaults() {
        let args = CliArgs::parse_from(&["tessera-bench"]);
        assert_eq!(args.target, Target::Tessera);
        assert_eq!(args.scenario, ScenarioKind::All);
        assert_eq!(args.nodes, 1_000);
    }

    #[test]
    fn run_write_scenario_does_not_panic() {
        let args = CliArgs {
            target: Target::Tessera,
            scenario: ScenarioKind::Write,
            nodes: 10,
            edges: 5,
            depth: 3,
            iterations: 5,
            output: OutputFormat::Json,
        };
        let report = run_scenario(&args).unwrap();
        assert!(!report.to_json().is_empty());
        assert_eq!(report.len(), 1);
    }

    #[test]
    fn run_all_scenarios_produces_five_entries() {
        let args = CliArgs {
            target: Target::Tessera,
            scenario: ScenarioKind::All,
            nodes: 10,
            edges: 9,
            depth: 3,
            iterations: 5,
            output: OutputFormat::Json,
        };
        let report = run_scenario(&args).unwrap();
        assert_eq!(report.len(), 5);
    }

    #[test]
    fn csv_output_format() {
        let args = CliArgs {
            target: Target::Tessera,
            scenario: ScenarioKind::Write,
            nodes: 10,
            edges: 5,
            depth: 3,
            iterations: 5,
            output: OutputFormat::Csv,
        };
        let report = run_scenario(&args).unwrap();
        let csv = report.to_csv();
        assert!(csv.contains("write,tessera"));
    }
}
