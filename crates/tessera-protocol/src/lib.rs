// Copyright 2026 BelowZero Security OU. All rights reserved.

//! Wire protocol definitions for tessera-graph-enterprise.

pub mod bolt_frame;
pub mod error;
pub mod frame;
pub mod message;
pub mod packstream;
pub mod tls;

pub use bolt_frame::{BoltChunkedReader, BoltChunkedWriter, MAX_CHUNK_SIZE};
pub use error::{ProtocolError, Result};
pub use frame::{FramedReader, FramedWriter, MAX_FRAME_SIZE};
pub use message::{ClientMessage, ServerMessage};
pub use packstream::PackStreamValue;
pub use tls::{ClientAuth, TlsConfig, TlsConfigBuilder};
