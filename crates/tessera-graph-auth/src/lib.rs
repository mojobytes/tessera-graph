// Copyright 2026 BelowZero Security OU. All rights reserved.

//! Authentication, RBAC and LBAC for tessera-graph-enterprise.

pub mod credentials;
pub mod error;
pub mod external_config;
pub mod lbac;
pub mod policy;
pub mod providers;
pub mod rate_limit;
pub mod rbac;
pub mod session;
pub mod user;
pub(crate) mod utils;

pub use error::{AuthError, Result};
pub use lbac::{Clearance, SecurityLabel, SecurityPolicy};
pub use policy::AuthPolicy;
pub use rbac::{Permission, Role, RoleId, RoleStore, RoleStoreHandle};
pub use session::{SessionManager, SessionToken};
pub use user::{UserId, UserStoreHandle};
