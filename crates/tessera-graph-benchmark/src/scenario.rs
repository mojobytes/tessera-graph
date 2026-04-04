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
        Ok(ScenarioResult::from_measurement(
            &measurement,
            "write",
            &name,
        ))
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
            target.traverse_bfs(start, depth)?;
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
            target.shortest_path(from, to)?;
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

// ---------------------------------------------------------------------------
// ConcurrentScenario
// ---------------------------------------------------------------------------

/// Measures throughput under concurrent load: N threads each performing a
/// mix of writes and reads against independent target instances.
///
/// Each thread gets its own target (via a factory closure) to avoid
/// artificial mutex contention — this mirrors real multi-client workloads
/// where each connection has its own session.
pub struct ConcurrentScenario {
    /// Number of concurrent threads.
    pub thread_count: usize,
    /// Number of operations each thread performs.
    pub ops_per_thread: usize,
    /// Fraction of operations that are writes (0.0–1.0).
    pub write_ratio: f64,
}

impl ConcurrentScenario {
    /// Runs the concurrent scenario using `factory` to create one target per thread.
    ///
    /// # Errors
    ///
    /// Returns [`crate::error::BenchmarkError`] if any thread encounters an error.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    pub fn run_with_factory<F>(&self, factory: F) -> Result<ScenarioResult>
    where
        F: Fn() -> Box<dyn BenchmarkTarget + Send> + Send + Sync,
    {
        let target_name = factory().name().to_string();
        let thread_count = self.thread_count;
        let ops_per_thread = self.ops_per_thread;
        let write_ratio = self.write_ratio;

        let write_every = if write_ratio > 0.0 {
            (1.0 / write_ratio).round() as usize
        } else {
            usize::MAX
        };

        let wall_start = std::time::Instant::now();

        let handles: Vec<_> = (0..thread_count)
            .map(|_| {
                let f = &factory;
                // SAFETY: factory is Sync, we only call it from the spawning thread
                // to build each target before the thread starts its work loop.
                let mut target = f();
                std::thread::spawn(move || {
                    let mut samples = Vec::with_capacity(ops_per_thread);
                    let mut created: Vec<NodeHandle> = Vec::new();

                    for op_idx in 0..ops_per_thread {
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
                    samples
                })
            })
            .collect();

        let mut all_samples = Vec::with_capacity(thread_count * ops_per_thread);
        for h in handles {
            let thread_samples = h.join().map_err(|_| {
                crate::error::BenchmarkError::scenario("concurrent thread panicked")
            })?;
            all_samples.extend(thread_samples);
        }

        let wall_elapsed = wall_start.elapsed();
        let total_ops = all_samples.len() as u64;

        // Throughput = total ops across all threads / wall-clock time
        let throughput = if wall_elapsed.as_nanos() > 0 {
            total_ops * 1_000_000_000 / wall_elapsed.as_nanos() as u64
        } else {
            0
        };

        let measurement = Measurement::from_nanos(&all_samples);
        let mut result = ScenarioResult::from_measurement(&measurement, "concurrent", &target_name);
        // Override throughput with wall-clock-based aggregate (more meaningful for concurrent)
        result.throughput_ops_per_sec = throughput;
        Ok(result)
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

    #[test]
    fn concurrent_scenario_aggregates_all_thread_samples() {
        let s = ConcurrentScenario {
            thread_count: 4,
            ops_per_thread: 50,
            write_ratio: 0.5,
        };
        let r = s
            .run_with_factory(|| Box::new(TesseraTarget::new()))
            .unwrap();
        assert_eq!(r.sample_count, 200); // 4 * 50
    }

    #[test]
    fn concurrent_scenario_throughput_positive() {
        let s = ConcurrentScenario {
            thread_count: 2,
            ops_per_thread: 50,
            write_ratio: 0.5,
        };
        let r = s
            .run_with_factory(|| Box::new(TesseraTarget::new()))
            .unwrap();
        assert!(r.throughput_ops_per_sec > 0);
    }

    #[test]
    fn concurrent_scenario_name_is_concurrent() {
        let s = ConcurrentScenario {
            thread_count: 2,
            ops_per_thread: 10,
            write_ratio: 1.0,
        };
        let r = s
            .run_with_factory(|| Box::new(TesseraTarget::new()))
            .unwrap();
        assert_eq!(r.scenario_name, "concurrent");
        assert_eq!(r.target_name, "tessera");
    }

    /// Target that fails on traversal/pathfinding (mimics Bolt targets
    /// that cannot execute these operations via query language).
    use crate::target::{EdgeData, EdgeHandle, NodeData};

    struct UnsupportedTraversalTarget(TesseraTarget);

    impl BenchmarkTarget for UnsupportedTraversalTarget {
        fn name(&self) -> &str { self.0.name() }
        fn create_node(&mut self, l: &str, p: Properties) -> Result<NodeHandle> { self.0.create_node(l, p) }
        fn create_edge(&mut self, l: &str, f: NodeHandle, t: NodeHandle, p: Properties) -> Result<EdgeHandle> { self.0.create_edge(l, f, t, p) }
        fn get_node(&self, h: NodeHandle) -> Result<NodeData> { self.0.get_node(h) }
        fn get_edge(&self, h: EdgeHandle) -> Result<EdgeData> { self.0.get_edge(h) }
        fn traverse_bfs(&self, _s: NodeHandle, _d: u32) -> Result<Vec<NodeHandle>> {
            Err(crate::error::BenchmarkError::scenario("not supported"))
        }
        fn traverse_dfs(&self, _s: NodeHandle, _d: u32) -> Result<Vec<NodeHandle>> {
            Err(crate::error::BenchmarkError::scenario("not supported"))
        }
        fn shortest_path(&self, _f: NodeHandle, _t: NodeHandle) -> Result<Option<Vec<NodeHandle>>> {
            Err(crate::error::BenchmarkError::scenario("not supported"))
        }
        fn clear(&mut self) { self.0.clear(); }
    }

    #[test]
    fn traversal_scenario_propagates_error_from_target() {
        let mut t = UnsupportedTraversalTarget(TesseraTarget::new());
        let start = t.create_node("N", Properties::new()).unwrap(); // OK: test
        let s = TraversalScenario {
            start,
            max_depth: 3,
            iterations: 5,
        };
        assert!(
            s.run(&mut t).is_err(),
            "traversal must propagate target error, not silently measure Err time"
        );
    }

    #[test]
    fn pathfinding_scenario_propagates_error_from_target() {
        let mut t = UnsupportedTraversalTarget(TesseraTarget::new());
        let a = t.create_node("N", Properties::new()).unwrap(); // OK: test
        let b = t.create_node("N", Properties::new()).unwrap(); // OK: test
        let s = PathfindingScenario {
            from: a,
            to: b,
            iterations: 5,
        };
        assert!(
            s.run(&mut t).is_err(),
            "pathfinding must propagate target error, not silently measure Err time"
        );
    }
}
