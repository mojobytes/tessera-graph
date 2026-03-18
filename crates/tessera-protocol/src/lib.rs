// Copyright 2026 BelowZero Security OU. All rights reserved.

//! Wire protocol definitions for tessera-graph-enterprise.

pub mod error;
pub mod tls;

pub use error::{ProtocolError, Result};
pub use tls::{ClientAuth, TlsConfig, TlsConfigBuilder};
