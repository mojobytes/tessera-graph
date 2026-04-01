// Copyright 2026 BelowZero Security OU. All rights reserved.

//! Bolt 4.4 message types — request/response encoding over `PackStream`.

use crate::packstream::PackStreamValue;

/// Convenience alias for `PackStream` dicts used in Bolt messages.
pub type BoltDict = Vec<(String, PackStreamValue)>;

// ── Tag constants ─────────────────────────────────────────────────────────────

/// Struct tag for HELLO request.
pub const TAG_HELLO: u8 = 0x01;
/// Struct tag for GOODBYE request.
pub const TAG_GOODBYE: u8 = 0x02;
/// Struct tag for RESET request.
pub const TAG_RESET: u8 = 0x0F;
/// Struct tag for RUN request.
pub const TAG_RUN: u8 = 0x10;
/// Struct tag for BEGIN request.
pub const TAG_BEGIN: u8 = 0x11;
/// Struct tag for COMMIT request.
pub const TAG_COMMIT: u8 = 0x12;
/// Struct tag for ROLLBACK request.
pub const TAG_ROLLBACK: u8 = 0x13;
/// Struct tag for DISCARD request.
pub const TAG_DISCARD: u8 = 0x2F;
/// Struct tag for PULL request.
pub const TAG_PULL: u8 = 0x3F;
/// Struct tag for LOGON request.
pub const TAG_LOGON: u8 = 0x6A;
/// Struct tag for SUCCESS response.
pub const TAG_SUCCESS: u8 = 0x70;
/// Struct tag for RECORD response.
pub const TAG_RECORD: u8 = 0x71;
/// Struct tag for IGNORED response.
pub const TAG_IGNORED: u8 = 0x7E;
/// Struct tag for FAILURE response.
pub const TAG_FAILURE: u8 = 0x7F;

// ── Message types ─────────────────────────────────────────────────────────────

/// All Bolt client request types.
#[derive(Debug, Clone, PartialEq)]
pub enum BoltRequest {
    /// HELLO — opens a new session and negotiates features.
    Hello {
        /// Extra metadata dict (e.g., `user_agent`, `routing`).
        extra: BoltDict,
    },
    /// LOGON — authenticates an existing session.
    Logon {
        /// Authentication parameters.
        auth: BoltDict,
    },
    /// RUN — submit a Cypher/GQL query.
    Run {
        /// The query string.
        query: String,
        /// Query parameters.
        params: BoltDict,
        /// Extra metadata (e.g., `db`, `mode`).
        extra: BoltDict,
    },
    /// PULL — retrieve records from the result stream.
    Pull {
        /// Extra metadata (e.g., `n` for batch size, `qid` for query ID).
        extra: BoltDict,
    },
    /// DISCARD — discard records from the result stream.
    Discard {
        /// Extra metadata (e.g., `n`, `qid`).
        extra: BoltDict,
    },
    /// BEGIN — start an explicit transaction.
    Begin {
        /// Extra metadata (e.g., `bookmarks`, `mode`, `db`).
        extra: BoltDict,
    },
    /// COMMIT — commit the current explicit transaction.
    Commit,
    /// ROLLBACK — roll back the current explicit transaction.
    Rollback,
    /// RESET — interrupt any ongoing operation and reset the session.
    Reset,
    /// GOODBYE — gracefully close the connection.
    Goodbye,
}

/// All Bolt server response types.
#[derive(Debug, Clone, PartialEq)]
pub enum BoltResponse {
    /// SUCCESS — the preceding request completed successfully.
    Success {
        /// Response metadata.
        metadata: BoltDict,
    },
    /// FAILURE — the preceding request failed.
    Failure {
        /// Error metadata (e.g., `code`, `message`).
        metadata: BoltDict,
    },
    /// RECORD — a single row in a result stream.
    Record {
        /// The record fields in column order.
        fields: Vec<PackStreamValue>,
    },
    /// IGNORED — the request was ignored (e.g., after a FAILURE).
    Ignored,
}

// ── Encoding ──────────────────────────────────────────────────────────────────

/// Encode a [`BoltRequest`] to `PackStream` bytes.
///
/// # Errors
///
/// Returns [`crate::ProtocolError::PackStreamInvalidFloat`] if any nested
/// `PackStreamValue::Float` is NaN or infinite.
pub fn encode_request(req: &BoltRequest) -> crate::Result<Vec<u8>> {
    let struct_val = match req {
        BoltRequest::Hello { extra } => PackStreamValue::Struct {
            tag: TAG_HELLO,
            fields: vec![PackStreamValue::Dict(extra.clone())],
        },
        BoltRequest::Logon { auth } => PackStreamValue::Struct {
            tag: TAG_LOGON,
            fields: vec![PackStreamValue::Dict(auth.clone())],
        },
        BoltRequest::Run {
            query,
            params,
            extra,
        } => PackStreamValue::Struct {
            tag: TAG_RUN,
            fields: vec![
                PackStreamValue::String(query.clone()),
                PackStreamValue::Dict(params.clone()),
                PackStreamValue::Dict(extra.clone()),
            ],
        },
        BoltRequest::Pull { extra } => PackStreamValue::Struct {
            tag: TAG_PULL,
            fields: vec![PackStreamValue::Dict(extra.clone())],
        },
        BoltRequest::Discard { extra } => PackStreamValue::Struct {
            tag: TAG_DISCARD,
            fields: vec![PackStreamValue::Dict(extra.clone())],
        },
        BoltRequest::Begin { extra } => PackStreamValue::Struct {
            tag: TAG_BEGIN,
            fields: vec![PackStreamValue::Dict(extra.clone())],
        },
        BoltRequest::Commit => PackStreamValue::Struct {
            tag: TAG_COMMIT,
            fields: vec![],
        },
        BoltRequest::Rollback => PackStreamValue::Struct {
            tag: TAG_ROLLBACK,
            fields: vec![],
        },
        BoltRequest::Reset => PackStreamValue::Struct {
            tag: TAG_RESET,
            fields: vec![],
        },
        BoltRequest::Goodbye => PackStreamValue::Struct {
            tag: TAG_GOODBYE,
            fields: vec![],
        },
    };
    let mut buf = Vec::new();
    crate::packstream::encode(&struct_val, &mut buf)?;
    Ok(buf)
}

/// Decode a [`BoltRequest`] from `PackStream` bytes.
///
/// # Errors
///
/// Returns:
/// - [`crate::ProtocolError::BoltUnexpectedTag`] if the decoded value is not a
///   struct, or carries an unrecognised tag byte.
/// - [`crate::ProtocolError::BoltMissingField`] if a required field is absent
///   or has the wrong type.
/// - Any `PackStream` decode error propagated from [`crate::packstream::decode`].
pub fn decode_request(data: &[u8]) -> crate::Result<BoltRequest> {
    let (value, _consumed) = crate::packstream::decode(data)?;
    let PackStreamValue::Struct { tag, fields } = value else {
        return Err(crate::ProtocolError::BoltUnexpectedTag {
            expected: 0,
            got: 0,
        });
    };
    match tag {
        TAG_HELLO => {
            let extra = extract_dict(&fields, 0, "Hello", "extra")?;
            Ok(BoltRequest::Hello { extra })
        }
        TAG_LOGON => {
            let auth = extract_dict(&fields, 0, "Logon", "auth")?;
            Ok(BoltRequest::Logon { auth })
        }
        TAG_RUN => {
            let query = extract_string(&fields, 0, "Run", "query")?;
            let params = extract_dict(&fields, 1, "Run", "params")?;
            let extra = extract_dict(&fields, 2, "Run", "extra")?;
            Ok(BoltRequest::Run {
                query,
                params,
                extra,
            })
        }
        TAG_PULL => {
            let extra = extract_dict(&fields, 0, "Pull", "extra")?;
            Ok(BoltRequest::Pull { extra })
        }
        TAG_DISCARD => {
            let extra = extract_dict(&fields, 0, "Discard", "extra")?;
            Ok(BoltRequest::Discard { extra })
        }
        TAG_BEGIN => {
            let extra = extract_dict(&fields, 0, "Begin", "extra")?;
            Ok(BoltRequest::Begin { extra })
        }
        TAG_COMMIT => Ok(BoltRequest::Commit),
        TAG_ROLLBACK => Ok(BoltRequest::Rollback),
        TAG_RESET => Ok(BoltRequest::Reset),
        TAG_GOODBYE => Ok(BoltRequest::Goodbye),
        other => Err(crate::ProtocolError::BoltUnexpectedTag {
            expected: 0,
            got: other,
        }),
    }
}

/// Encode a [`BoltResponse`] to `PackStream` bytes.
///
/// # Errors
///
/// Returns [`crate::ProtocolError::PackStreamInvalidFloat`] if any nested
/// `PackStreamValue::Float` is NaN or infinite.
pub fn encode_response(resp: &BoltResponse) -> crate::Result<Vec<u8>> {
    let struct_val = match resp {
        BoltResponse::Success { metadata } => PackStreamValue::Struct {
            tag: TAG_SUCCESS,
            fields: vec![PackStreamValue::Dict(metadata.clone())],
        },
        BoltResponse::Failure { metadata } => PackStreamValue::Struct {
            tag: TAG_FAILURE,
            fields: vec![PackStreamValue::Dict(metadata.clone())],
        },
        BoltResponse::Record { fields } => PackStreamValue::Struct {
            tag: TAG_RECORD,
            fields: vec![PackStreamValue::List(fields.clone())],
        },
        BoltResponse::Ignored => PackStreamValue::Struct {
            tag: TAG_IGNORED,
            fields: vec![],
        },
    };
    let mut buf = Vec::new();
    crate::packstream::encode(&struct_val, &mut buf)?;
    Ok(buf)
}

/// Decode a [`BoltResponse`] from `PackStream` bytes.
///
/// # Errors
///
/// Returns:
/// - [`crate::ProtocolError::BoltUnexpectedTag`] if the decoded value is not a
///   struct, or carries an unrecognised tag byte.
/// - [`crate::ProtocolError::BoltMissingField`] if a required field is absent
///   or has the wrong type.
/// - Any `PackStream` decode error propagated from [`crate::packstream::decode`].
pub fn decode_response(data: &[u8]) -> crate::Result<BoltResponse> {
    let (value, _consumed) = crate::packstream::decode(data)?;
    let PackStreamValue::Struct { tag, fields } = value else {
        return Err(crate::ProtocolError::BoltUnexpectedTag {
            expected: 0,
            got: 0,
        });
    };
    match tag {
        TAG_SUCCESS => {
            let metadata = extract_dict(&fields, 0, "Success", "metadata")?;
            Ok(BoltResponse::Success { metadata })
        }
        TAG_FAILURE => {
            let metadata = extract_dict(&fields, 0, "Failure", "metadata")?;
            Ok(BoltResponse::Failure { metadata })
        }
        TAG_RECORD => {
            let record_fields = extract_list(&fields, 0, "Record", "fields")?;
            Ok(BoltResponse::Record {
                fields: record_fields,
            })
        }
        TAG_IGNORED => Ok(BoltResponse::Ignored),
        other => Err(crate::ProtocolError::BoltUnexpectedTag {
            expected: 0,
            got: other,
        }),
    }
}

// ── Private field helpers ─────────────────────────────────────────────────────

fn extract_dict(
    fields: &[PackStreamValue],
    index: usize,
    message: &'static str,
    field: &'static str,
) -> crate::Result<BoltDict> {
    match fields.get(index) {
        Some(PackStreamValue::Dict(d)) => Ok(d.clone()),
        Some(_) | None => Err(crate::ProtocolError::BoltMissingField { message, field }),
    }
}

fn extract_string(
    fields: &[PackStreamValue],
    index: usize,
    message: &'static str,
    field: &'static str,
) -> crate::Result<String> {
    match fields.get(index) {
        Some(PackStreamValue::String(s)) => Ok(s.clone()),
        Some(_) | None => Err(crate::ProtocolError::BoltMissingField { message, field }),
    }
}

fn extract_list(
    fields: &[PackStreamValue],
    index: usize,
    message: &'static str,
    field: &'static str,
) -> crate::Result<Vec<PackStreamValue>> {
    match fields.get(index) {
        Some(PackStreamValue::List(l)) => Ok(l.clone()),
        Some(_) | None => Err(crate::ProtocolError::BoltMissingField { message, field }),
    }
}
