//! Benchmark scenario runners.
//!
//! Each scenario exercises a specific workload pattern against a
//! [`BenchmarkTarget`](crate::target::BenchmarkTarget), collecting timing
//! samples and returning a [`ScenarioResult`].

use tessera_graph::Properties;

use crate::error::Result;
use crate::measure::Measurement;
use crate::target::{BenchmarkTarget, NodeHandle};

/// The result of running a single scenario against a single target.
#[derive(Debug, Clone)]
#[must_use]
pub struct ScenarioResult {
    /// Scenario name (e.g. `"write"`, `"read"`).
    pub scenario_name: String,
    /// Target name (e.g. `"tessera"`, `"memgraph"`).
    pub target_name: String,
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
    /// Number of timing samples collected.
    pub sample_count: usize,
}

impl ScenarioResult {
    /// Builds a result from a measurement and scenario/target names.
    fn from_measurement(measurement: &Measurement, scenario: &str, target: &str) -> Self {
        Self {
            scenario_name: scenario.into(),
            target_name: target.into(),
            throughput_ops_per_sec: measurement.throughput_ops_per_sec(),
            mean_latency_ns: measurement.mean_ns(),
            p50_ns: measurement.p50_ns(),
            p95_ns: measurement.p95_ns(),
            p99_ns: measurement.p99_ns(),
            sample_count: measurement.sample_count(),
        }
    }
}

/// Trait for runnable benchmark scenarios.
#[allow(clippy::missing_errors_doc)]
pub trait Scenario {
    /// Runs the scenario against the given target and returns results.
    fn run(&self, target: &mut dyn BenchmarkTarget) -> Result<ScenarioResult>;
}

// ---------------------------------------------------------------------------
// WriteScenario
// ---------------------------------------------------------------------------

/// Measures write throughput: bulk node and edge creation.
pub struct WriteScenario {
    /// Number of nodes to create.
    pub node_count: usize,
    /// Number of edges to create (wired between adjacent node pairs).
    pub edge_count: usize,
}

impl Scenario for WriteScenario {
    #[allow(clippy::cast_possible_truncation)]
    fn run(&self, target: &mut dyn BenchmarkTarget) -> Result<ScenarioResult> {
        let name = target.name().to_string();
        let total_ops = self.node_count + self.edge_count;

        let mut samples = Vec::with_capacity(total_ops);
        let mut node_handles: Vec<NodeHandle> = Vec::with_capacity(self.node_count);

        // Time each node creation
        for _ in 0..self.node_count {
            let start = std::time::Instant::now();
            let h = target.create_node("N", Properties::new())?;
            samples.push(start.elapsed().as_nanos() as u64);
            node_handles.push(h);
        }

        // Time each edge creation (chain adjacent nodes)
        let edge_count = self.edge_count.min(self.node_count.saturating_sub(1));
        for i in 0..edge_count {
            let start = std::time::Instant::now();
            target.create_edge("E", node_handles[i], node_handles[i + 1], Properties::new())?;
            samples.push(start.elapsed().as_nanos() as u64);
        }

        let measurement = Measurement::from_nanos(&samples);
        Ok(ScenarioResult::from_measurement(&measurement, "write", &name))
    }
}

// ---------------------------------------------------------------------------
// ReadScenario
// ---------------------------------------------------------------------------

/// Measures point-lookup latency: repeated `get_node` calls.
pub struct ReadScenario {
    /// Node handles to look up (cycled through).
    pub node_handles: Vec<NodeHandle>,
    /// Total number of lookup iterations.
    pub lookup_iterations: usize,
}

impl Scenario for ReadScenario {
    #[allow(clippy::cast_possible_truncation)]
    fn run(&self, target: &mut dyn BenchmarkTarget) -> Result<ScenarioResult> {
        let name = target.name().to_string();

        let handles = &self.node_handles;
        if handles.is_empty() {
            return Err(crate::error::BenchmarkError::scenario(
                "ReadScenario requires at least one node handle",
            ));
        }

        let mut samples = Vec::with_capacity(self.lookup_iterations);
        for idx in 0..self.lookup_iterations {
            let h = handles[idx % handles.len()];
            let start = std::time::Instant::now();
            let _ = target.get_node(h);
            samples.push(start.elapsed().as_nanos() as u64);
        }

        let m = Measurement::from_nanos(&samples);
        Ok(ScenarioResult::from_measurement(&m, "read", &name))
    }
}

// ---------------------------------------------------------------------------
// TraversalScenario
// ---------------------------------------------------------------------------

/// Measures BFS traversal throughput from a given start node.
pub struct TraversalScenario {
    /// Starting node for traversal.
    pub start: NodeHandle,
    /// Maximum traversal depth.
    pub max_depth: u32,
    /// Number of traversal iterations.
    pub iterations: usize,
}

impl Scenario for TraversalScenario {
    #[allow(clippy::cast_possible_truncation)]
    fn run(&self, target: &mut dyn BenchmarkTarget) -> Result<ScenarioResult> {
        let name = target.name().to_string();
        let start = self.start;
        let depth = self.max_depth;

        let mut samples = Vec::with_capacity(self.iterations);
        for _ in 0..self.iterations {
            let t0 = std::time::Instant::now();
            let _ = target.traverse_bfs(start, depth);
            samples.push(t0.elapsed().as_nanos() as u64);
        }

        let m = Measurement::from_nanos(&samples);
        Ok(ScenarioResult::from_measurement(&m, "traversal", &name))
    }
}

// ---------------------------------------------------------------------------
// PathfindingScenario
// ---------------------------------------------------------------------------

/// Measures shortest-path latency between two nodes.
pub struct PathfindingScenario {
    /// Source node.
    pub from: NodeHandle,
    /// Destination node.
    pub to: NodeHandle,
    /// Number of pathfinding iterations.
    pub iterations: usize,
}

impl Scenario for PathfindingScenario {
    #[allow(clippy::cast_possible_truncation)]
    fn run(&self, target: &mut dyn BenchmarkTarget) -> Result<ScenarioResult> {
        let name = target.name().to_string();
        let from = self.from;
        let to = self.to;

        let mut samples = Vec::with_capacity(self.iterations);
        for _ in 0..self.iterations {
            let t0 = std::time::Instant::now();
            let _ = target.shortest_path(from, to);
            samples.push(t0.elapsed().as_nanos() as u64);
        }

        let m = Measurement::from_nanos(&samples);
        Ok(ScenarioResult::from_measurement(&m, "pathfinding", &name))
    }
}

// ---------------------------------------------------------------------------
// MixedScenario
// ---------------------------------------------------------------------------

/// Interleaves writes and reads deterministically based on `write_ratio`.
pub struct MixedScenario {
    /// Fraction of operations that are writes (0.0–1.0).
    pub write_ratio: f64,
    /// Total number of operations.
    pub total_ops: usize,
}

impl Scenario for MixedScenario {
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    fn run(&self, target: &mut dyn BenchmarkTarget) -> Result<ScenarioResult> {
        let name = target.name().to_string();
        let mut created: Vec<NodeHandle> = Vec::new();
        let write_every = if self.write_ratio > 0.0 {
            (1.0 / self.write_ratio).round() as usize
        } else {
            usize::MAX
        };

        let mut samples = Vec::with_capacity(self.total_ops);
        for op_idx in 0..self.total_ops {
            let is_write = created.is_empty() || op_idx % write_every == 0;
            let t0 = std::time::Instant::now();
            if is_write {
                if let Ok(h) = target.create_node("N", Properties::new()) {
                    created.push(h);
                }
            } else {
                let h = created[op_idx % created.len()];
                let _ = target.get_node(h);
            }
            samples.push(t0.elapsed().as_nanos() as u64);
        }

        let m = Measurement::from_nanos(&samples);
        Ok(ScenarioResult::from_measurement(&m, "mixed", &name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tessera_target::TesseraTarget;

    #[test]
    fn write_scenario_produces_positive_throughput() {
        let mut t = TesseraTarget::new();
        let s = WriteScenario {
            node_count: 100,
            edge_count: 50,
        };
        let r = s.run(&mut t).unwrap();
        assert!(r.throughput_ops_per_sec > 0);
        assert_eq!(r.scenario_name, "write");
        assert_eq!(r.target_name, "tessera");
    }

    #[test]
    fn read_scenario_sample_count_equals_lookup_count() {
        let mut t = TesseraTarget::new();
        let h = t.create_node("N", Properties::new()).unwrap();
        let s = ReadScenario {
            node_handles: vec![h],
            lookup_iterations: 50,
        };
        let r = s.run(&mut t).unwrap();
        assert_eq!(r.sample_count, 50);
        assert_eq!(r.scenario_name, "read");
    }

    #[test]
    fn read_scenario_empty_handles_errors() {
        let mut t = TesseraTarget::new();
        let s = ReadScenario {
            node_handles: vec![],
            lookup_iterations: 10,
        };
        assert!(s.run(&mut t).is_err());
    }

    #[test]
    fn traversal_scenario_visits_at_least_start_node() {
        let mut t = TesseraTarget::new();
        let start = t.create_node("N", Properties::new()).unwrap();
        let s = TraversalScenario {
            start,
            max_depth: 3,
            iterations: 20,
        };
        let r = s.run(&mut t).unwrap();
        assert!(r.throughput_ops_per_sec > 0);
        assert_eq!(r.scenario_name, "traversal");
    }

    #[test]
    fn pathfinding_scenario_unreachable_does_not_error() {
        let mut target = TesseraTarget::new();
        let node_a = target.create_node("N", Properties::new()).unwrap();
        let node_b = target.create_node("N", Properties::new()).unwrap();
        let scenario = PathfindingScenario {
            from: node_a,
            to: node_b,
            iterations: 10,
        };
        let result = scenario.run(&mut target).unwrap();
        assert_eq!(result.scenario_name, "pathfinding");
        assert_eq!(result.sample_count, 10);
    }

    #[test]
    fn mixed_scenario_produces_result_with_name_mixed() {
        let mut t = TesseraTarget::new();
        let s = MixedScenario {
            write_ratio: 0.5,
            total_ops: 100,
        };
        let r = s.run(&mut t).unwrap();
        assert_eq!(r.scenario_name, "mixed");
        assert_eq!(r.sample_count, 100);
    }
}
