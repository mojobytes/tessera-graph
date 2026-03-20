// Copyright 2026 BelowZero Security OU. All rights reserved.

//! Wire protocol definitions for tessera-graph-enterprise.

pub mod error;
pub mod frame;
pub mod message;
pub mod tls;

pub use error::{ProtocolError, Result};
pub use frame::{FramedReader, FramedWriter, MAX_FRAME_SIZE};
pub use message::{ClientMessage, ServerMessage};
pub use tls::{ClientAuth, TlsConfig, TlsConfigBuilder};
