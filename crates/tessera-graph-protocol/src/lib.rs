// Copyright 2026 BelowZero Security OU. All rights reserved.

//! Wire protocol definitions for tessera-graph-enterprise.

pub mod bolt_client;
pub mod bolt_frame;
pub mod bolt_handshake;
pub mod bolt_message;
pub mod error;
pub mod packstream;
pub mod tls;

pub use bolt_client::{BoltClient, QueryResult, connect};
pub use bolt_frame::{BoltChunkedReader, BoltChunkedWriter, MAX_BOLT_MESSAGE_SIZE, MAX_CHUNK_SIZE};
pub use bolt_handshake::{
    BOLT_MAGIC, BoltVersion, SUPPORTED_VERSION, encode_version_response, negotiate_version,
    parse_version_proposal,
};
pub use bolt_message::{
    BoltDict, BoltRequest, BoltResponse, decode_request, decode_response, encode_request,
    encode_response,
};
pub use error::{ProtocolError, Result};
pub use packstream::{PackStreamValue, decode, encode};
pub use tls::{ClientAuth, TlsConfig, TlsConfigBuilder};
