// Copyright 2026 BelowZero Security OU. All rights reserved.

//! Wire protocol definitions for tessera-graph-enterprise.

pub mod bolt_client;
pub mod bolt_frame;
pub mod bolt_handshake;
pub mod bolt_message;
pub mod error;
pub mod frame;
pub mod message;
pub mod packstream;
pub mod tls;

pub use bolt_frame::{BoltChunkedReader, BoltChunkedWriter, MAX_BOLT_MESSAGE_SIZE, MAX_CHUNK_SIZE};
pub use bolt_handshake::{
    encode_version_response, negotiate_version, parse_version_proposal, BoltVersion, BOLT_MAGIC,
    SUPPORTED_VERSION,
};
pub use bolt_message::{
    decode_request, decode_response, encode_request, encode_response, BoltDict, BoltRequest,
    BoltResponse,
};
pub use bolt_client::{connect, BoltClient, QueryResult};
pub use error::{ProtocolError, Result};
pub use frame::{FramedReader, FramedWriter, MAX_FRAME_SIZE};
pub use message::{ClientMessage, ServerMessage};
pub use packstream::{decode, encode, PackStreamValue};
pub use tls::{ClientAuth, TlsConfig, TlsConfigBuilder};
