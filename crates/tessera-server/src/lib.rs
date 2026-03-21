// Copyright 2026 BelowZero Security OU. All rights reserved.

//! Library surface for tessera-server integration tests and internal modules.

pub mod auth_dispatch;
pub mod connection;
pub mod context;
pub mod error;
pub mod listener;

pub use connection::ConnectionHandler;
pub use context::ServerContext;
pub use error::{Result, ServerError};
pub use listener::TesseraListener;
