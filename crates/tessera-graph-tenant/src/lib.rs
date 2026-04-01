// Copyright 2026 BelowZero Security OU. All rights reserved.

//! Multi-tenant database management for tessera-graph-enterprise.

pub mod error;
pub mod registry;
pub mod types;

pub use error::{Result, TenantError};
pub use registry::TenantRegistry;
pub use types::{DatabaseAddress, DatabaseName, TenantId};
