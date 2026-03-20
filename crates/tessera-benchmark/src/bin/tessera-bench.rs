// Copyright (c) 2026 BelowZero Security OU. All rights reserved.
// SPDX-License-Identifier: LicenseRef-BelowZero-Enterprise

//! CLI runner for the tessera-benchmark harness.

use tessera_benchmark::dataset::{ChainDataset, Dataset};
use tessera_benchmark::error::{BenchmarkError, Result};
use tessera_benchmark::report::{Report, ReportEntry};
use tessera_benchmark::scenario::{
    ConcurrentScenario, MixedScenario, PathfindingScenario, ReadScenario, Scenario, ScenarioResult,
    TraversalScenario, WriteScenario,
};
use tessera_benchmark::target::BenchmarkTarget;
use tessera_benchmark::tessera_target::TesseraTarget;

/// Supported benchmark targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Tessera,
    #[cfg(feature = "memgraph")]
    Memgraph,
}

/// Supported scenario kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScenarioKind {
    Write,
    Read,
    Traversal,
    Pathfinding,
    Mixed,
    Concurrent,
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
    pub threads: usize,
    pub write_ratio: f64,
    pub output: OutputFormat,
    #[cfg(feature = "memgraph")]
    pub memgraph_uri: String,
    #[cfg(feature = "memgraph")]
    pub memgraph_user: String,
    #[cfg(feature = "memgraph")]
    pub memgraph_pass: String,
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
            threads: 4,
            write_ratio: 0.5,
            output: OutputFormat::Json,
            #[cfg(feature = "memgraph")]
            memgraph_uri: "bolt://localhost:7687".into(),
            #[cfg(feature = "memgraph")]
            memgraph_user: String::new(),
            #[cfg(feature = "memgraph")]
            memgraph_pass: String::new(),
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
                        result.target = match args[i] {
                            #[cfg(feature = "memgraph")]
                            "memgraph" => Target::Memgraph,
                            _ => Target::Tessera,
                        };
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
                            "concurrent" => ScenarioKind::Concurrent,
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
                "--threads" => {
                    i += 1;
                    if i < args.len() {
                        result.threads = args[i].parse().unwrap_or(4);
                    }
                }
                "--write-ratio" => {
                    i += 1;
                    if i < args.len() {
                        result.write_ratio = args[i].parse().unwrap_or(0.5);
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
                #[cfg(feature = "memgraph")]
                "--memgraph-uri" => {
                    i += 1;
                    if i < args.len() {
                        result.memgraph_uri = args[i].to_string();
                    }
                }
                #[cfg(feature = "memgraph")]
                "--memgraph-user" => {
                    i += 1;
                    if i < args.len() {
                        result.memgraph_user = args[i].to_string();
                    }
                }
                #[cfg(feature = "memgraph")]
                "--memgraph-pass" => {
                    i += 1;
                    if i < args.len() {
                        result.memgraph_pass = args[i].to_string();
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

/// Runs the selected scenario(s) against the given target and returns a report.
///
/// # Errors
///
/// Returns [`BenchmarkError`] if any scenario fails during execution.
fn run_scenarios(args: &CliArgs, target: &mut dyn BenchmarkTarget) -> Result<Report> {
    let mut report = Report::new();

    let run_write = matches!(args.scenario, ScenarioKind::Write | ScenarioKind::All);
    let run_read = matches!(args.scenario, ScenarioKind::Read | ScenarioKind::All);
    let run_traversal = matches!(args.scenario, ScenarioKind::Traversal | ScenarioKind::All);
    let run_pathfinding = matches!(args.scenario, ScenarioKind::Pathfinding | ScenarioKind::All);
    let run_mixed = matches!(args.scenario, ScenarioKind::Mixed | ScenarioKind::All);
    let run_concurrent = matches!(args.scenario, ScenarioKind::Concurrent | ScenarioKind::All);

    if run_write {
        let s = WriteScenario {
            node_count: args.nodes,
            edge_count: args.edges,
        };
        let r = s.run(target)?;
        report.add(result_to_entry(&r));
        target.clear();
    }

    if run_read {
        let ds = ChainDataset { length: args.nodes };
        let dataset = ds.build(target)?;
        let s = ReadScenario {
            node_handles: dataset.nodes,
            lookup_iterations: args.iterations,
        };
        let r = s.run(target)?;
        report.add(result_to_entry(&r));
        target.clear();
    }

    if run_traversal {
        let ds = ChainDataset { length: args.nodes };
        let dataset = ds.build(target)?;
        let s = TraversalScenario {
            start: dataset.nodes[0],
            max_depth: args.depth,
            iterations: args.iterations,
        };
        let r = s.run(target)?;
        report.add(result_to_entry(&r));
        target.clear();
    }

    if run_pathfinding {
        let ds = ChainDataset { length: args.nodes };
        let dataset = ds.build(target)?;
        let s = PathfindingScenario {
            from: dataset.nodes[0],
            to: *dataset
                .nodes
                .last()
                .ok_or_else(|| BenchmarkError::scenario("empty dataset for pathfinding"))?,
            iterations: args.iterations,
        };
        let r = s.run(target)?;
        report.add(result_to_entry(&r));
        target.clear();
    }

    if run_mixed {
        let s = MixedScenario {
            write_ratio: args.write_ratio,
            total_ops: args.iterations,
        };
        let r = s.run(target)?;
        report.add(result_to_entry(&r));
        target.clear();
    }

    if run_concurrent {
        let s = ConcurrentScenario {
            thread_count: args.threads,
            ops_per_thread: args.iterations,
            write_ratio: args.write_ratio,
        };
        let r = s.run_with_factory(|| Box::new(TesseraTarget::new()))?;
        report.add(result_to_entry(&r));
    }

    Ok(report)
}

/// Top-level entry point: builds the appropriate target and runs scenarios.
///
/// # Errors
///
/// Returns [`BenchmarkError`] if target creation or scenario execution fails.
pub fn run_scenario(args: &CliArgs) -> Result<Report> {
    match args.target {
        Target::Tessera => {
            let mut target = TesseraTarget::new();
            run_scenarios(args, &mut target)
        }
        #[cfg(feature = "memgraph")]
        Target::Memgraph => {
            let mut target = tessera_benchmark::memgraph_target::MemgraphTarget::connect(
                &args.memgraph_uri,
                &args.memgraph_user,
                &args.memgraph_pass,
            )?;
            run_scenarios(args, &mut target)
        }
    }
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
        assert_eq!(args.threads, 4);
    }

    #[test]
    fn cli_args_parse_concurrent_scenario() {
        let args = CliArgs::parse_from(&[
            "tessera-bench",
            "--scenario",
            "concurrent",
            "--threads",
            "8",
            "--write-ratio",
            "0.3",
        ]);
        assert_eq!(args.scenario, ScenarioKind::Concurrent);
        assert_eq!(args.threads, 8);
        assert!((args.write_ratio - 0.3).abs() < f64::EPSILON);
    }

    #[test]
    fn run_write_scenario_does_not_panic() {
        let args = CliArgs {
            scenario: ScenarioKind::Write,
            nodes: 10,
            edges: 5,
            iterations: 5,
            ..CliArgs::default()
        };
        let report = run_scenario(&args).unwrap();
        assert!(!report.to_json().is_empty());
        assert_eq!(report.len(), 1);
    }

    #[test]
    fn run_all_scenarios_produces_six_entries() {
        let args = CliArgs {
            scenario: ScenarioKind::All,
            nodes: 10,
            edges: 9,
            depth: 3,
            iterations: 5,
            threads: 2,
            ..CliArgs::default()
        };
        let report = run_scenario(&args).unwrap();
        assert_eq!(report.len(), 6); // write, read, traversal, pathfinding, mixed, concurrent
    }

    #[test]
    fn run_concurrent_scenario_only() {
        let args = CliArgs {
            scenario: ScenarioKind::Concurrent,
            threads: 2,
            iterations: 10,
            ..CliArgs::default()
        };
        let report = run_scenario(&args).unwrap();
        assert_eq!(report.len(), 1);
        let json = report.to_json();
        assert!(json.contains("concurrent"));
    }

    #[test]
    fn csv_output_format() {
        let args = CliArgs {
            scenario: ScenarioKind::Write,
            nodes: 10,
            edges: 5,
            iterations: 5,
            output: OutputFormat::Csv,
            ..CliArgs::default()
        };
        let report = run_scenario(&args).unwrap();
        let csv = report.to_csv();
        assert!(csv.contains("write,tessera"));
    }

    #[cfg(feature = "memgraph")]
    #[test]
    fn cli_args_parse_memgraph_target() {
        let args = CliArgs::parse_from(&[
            "tessera-bench",
            "--target",
            "memgraph",
            "--memgraph-uri",
            "bolt://db:7687",
        ]);
        assert_eq!(args.target, Target::Memgraph);
        assert_eq!(args.memgraph_uri, "bolt://db:7687");
    }

    #[cfg(feature = "memgraph")]
    #[test]
    fn cli_args_memgraph_uri_defaults() {
        let args = CliArgs::parse_from(&["tessera-bench", "--target", "memgraph"]);
        assert_eq!(args.memgraph_uri, "bolt://localhost:7687");
        assert!(args.memgraph_user.is_empty());
        assert!(args.memgraph_pass.is_empty());
    }
}
