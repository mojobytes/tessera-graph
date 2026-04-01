// Copyright 2026 BelowZero Security OU. All rights reserved.

//! Prometheus metrics and monitoring for tessera-graph-enterprise.

pub mod health;
pub mod registry;
pub mod render;
pub mod server;

pub use health::{AtomicHealthFlag, HealthProvider, StaticHealth};
pub use registry::MetricsRegistry;
pub use render::render_prometheus;
pub use server::serve_metrics;
