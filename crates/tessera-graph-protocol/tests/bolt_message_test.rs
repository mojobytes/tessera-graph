// Copyright 2026 BelowZero Security OU. All rights reserved.

//! Integration tests for Bolt 4.4 message encode/decode.

use tessera_graph_protocol::packstream::decode as ps_decode;
use tessera_graph_protocol::{
    BoltChunkedReader, BoltChunkedWriter, BoltDict, BoltRequest, BoltResponse, PackStreamValue,
    ProtocolError, decode_request, decode_response, encode_request, encode_response,
};

// ── Helpers ───────────────────────────────────────────────────────────────────

fn make_pair(
    buf: usize,
) -> (
    BoltChunkedWriter<tokio::io::WriteHalf<tokio::io::DuplexStream>>,
    BoltChunkedReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
) {
    let (client, server) = tokio::io::duplex(buf);
    let (_client_read, client_write) = tokio::io::split(client);
    let (server_read, _server_write) = tokio::io::split(server);
    (
        BoltChunkedWriter::new(client_write),
        BoltChunkedReader::new(server_read),
    )
}

const fn empty_dict() -> BoltDict {
    vec![]
}

fn sample_dict() -> BoltDict {
    vec![
        (
            "user_agent".to_owned(),
            PackStreamValue::String("tessera/1.0".to_owned()),
        ),
        (
            "scheme".to_owned(),
            PackStreamValue::String("basic".to_owned()),
        ),
    ]
}

// ── encode_request: verify struct tag and field count ─────────────────────────

#[test]
fn encode_hello_produces_correct_tag() {
    let req = BoltRequest::Hello {
        extra: sample_dict(),
    };
    let bytes = encode_request(&req).unwrap();
    let (val, _) = ps_decode(&bytes).unwrap();
    let PackStreamValue::Struct { tag, fields } = val else {
        panic!("expected Struct");
    };
    assert_eq!(tag, 0x01, "HELLO tag");
    assert_eq!(fields.len(), 1, "HELLO has 1 field (extra dict)");
}

#[test]
fn encode_logon_produces_correct_tag() {
    let req = BoltRequest::Logon {
        auth: sample_dict(),
    };
    let bytes = encode_request(&req).unwrap();
    let (val, _) = ps_decode(&bytes).unwrap();
    let PackStreamValue::Struct { tag, fields } = val else {
        panic!("expected Struct");
    };
    assert_eq!(tag, 0x6A, "LOGON tag");
    assert_eq!(fields.len(), 1, "LOGON has 1 field (auth dict)");
}

#[test]
fn encode_run_produces_correct_tag_and_three_fields() {
    let req = BoltRequest::Run {
        query: "MATCH (n) RETURN n".to_owned(),
        params: empty_dict(),
        extra: empty_dict(),
    };
    let bytes = encode_request(&req).unwrap();
    let (val, _) = ps_decode(&bytes).unwrap();
    let PackStreamValue::Struct { tag, fields } = val else {
        panic!("expected Struct");
    };
    assert_eq!(tag, 0x10, "RUN tag");
    assert_eq!(fields.len(), 3, "RUN has 3 fields (query, params, extra)");
    // First field must be the query string
    assert_eq!(
        fields[0],
        PackStreamValue::String("MATCH (n) RETURN n".to_owned())
    );
}

#[test]
fn encode_pull_produces_correct_tag() {
    let req = BoltRequest::Pull {
        extra: vec![("n".to_owned(), PackStreamValue::Int(-1))],
    };
    let bytes = encode_request(&req).unwrap();
    let (val, _) = ps_decode(&bytes).unwrap();
    let PackStreamValue::Struct { tag, fields } = val else {
        panic!("expected Struct");
    };
    assert_eq!(tag, 0x3F, "PULL tag");
    assert_eq!(fields.len(), 1);
}

#[test]
fn encode_discard_produces_correct_tag() {
    let req = BoltRequest::Discard {
        extra: vec![("n".to_owned(), PackStreamValue::Int(-1))],
    };
    let bytes = encode_request(&req).unwrap();
    let (val, _) = ps_decode(&bytes).unwrap();
    let PackStreamValue::Struct { tag, .. } = val else {
        panic!("expected Struct");
    };
    assert_eq!(tag, 0x2F, "DISCARD tag");
}

#[test]
fn encode_begin_produces_correct_tag() {
    let req = BoltRequest::Begin {
        extra: empty_dict(),
    };
    let bytes = encode_request(&req).unwrap();
    let (val, _) = ps_decode(&bytes).unwrap();
    let PackStreamValue::Struct { tag, fields } = val else {
        panic!("expected Struct");
    };
    assert_eq!(tag, 0x11, "BEGIN tag");
    assert_eq!(fields.len(), 1);
}

#[test]
fn encode_commit_has_zero_fields() {
    let bytes = encode_request(&BoltRequest::Commit).unwrap();
    let (val, _) = ps_decode(&bytes).unwrap();
    let PackStreamValue::Struct { tag, fields } = val else {
        panic!("expected Struct");
    };
    assert_eq!(tag, 0x12, "COMMIT tag");
    assert_eq!(fields.len(), 0);
}

#[test]
fn encode_rollback_has_zero_fields() {
    let bytes = encode_request(&BoltRequest::Rollback).unwrap();
    let (val, _) = ps_decode(&bytes).unwrap();
    let PackStreamValue::Struct { tag, fields } = val else {
        panic!("expected Struct");
    };
    assert_eq!(tag, 0x13, "ROLLBACK tag");
    assert_eq!(fields.len(), 0);
}

#[test]
fn encode_reset_has_zero_fields() {
    let bytes = encode_request(&BoltRequest::Reset).unwrap();
    let (val, _) = ps_decode(&bytes).unwrap();
    let PackStreamValue::Struct { tag, fields } = val else {
        panic!("expected Struct");
    };
    assert_eq!(tag, 0x0F, "RESET tag");
    assert_eq!(fields.len(), 0);
}

#[test]
fn encode_goodbye_has_zero_fields() {
    let bytes = encode_request(&BoltRequest::Goodbye).unwrap();
    let (val, _) = ps_decode(&bytes).unwrap();
    let PackStreamValue::Struct { tag, fields } = val else {
        panic!("expected Struct");
    };
    assert_eq!(tag, 0x02, "GOODBYE tag");
    assert_eq!(fields.len(), 0);
}

// ── encode_response: verify struct tag and field count ────────────────────────

#[test]
fn encode_success_produces_correct_tag() {
    let resp = BoltResponse::Success {
        metadata: vec![("fields".to_owned(), PackStreamValue::List(vec![]))],
    };
    let bytes = encode_response(&resp).unwrap();
    let (val, _) = ps_decode(&bytes).unwrap();
    let PackStreamValue::Struct { tag, fields } = val else {
        panic!("expected Struct");
    };
    assert_eq!(tag, 0x70, "SUCCESS tag");
    assert_eq!(fields.len(), 1);
}

#[test]
fn encode_failure_produces_correct_tag() {
    let resp = BoltResponse::Failure {
        metadata: vec![
            (
                "code".to_owned(),
                PackStreamValue::String("Neo.ClientError.Statement.SyntaxError".to_owned()),
            ),
            (
                "message".to_owned(),
                PackStreamValue::String("Invalid syntax".to_owned()),
            ),
        ],
    };
    let bytes = encode_response(&resp).unwrap();
    let (val, _) = ps_decode(&bytes).unwrap();
    let PackStreamValue::Struct { tag, fields } = val else {
        panic!("expected Struct");
    };
    assert_eq!(tag, 0x7F, "FAILURE tag");
    assert_eq!(fields.len(), 1);
}

#[test]
fn encode_record_wraps_fields_in_list() {
    let resp = BoltResponse::Record {
        fields: vec![
            PackStreamValue::Int(42),
            PackStreamValue::String("Alice".to_owned()),
        ],
    };
    let bytes = encode_response(&resp).unwrap();
    let (val, _) = ps_decode(&bytes).unwrap();
    let PackStreamValue::Struct { tag, fields } = val else {
        panic!("expected Struct");
    };
    assert_eq!(tag, 0x71, "RECORD tag");
    assert_eq!(fields.len(), 1, "RECORD wraps fields in a List");
    assert!(
        matches!(&fields[0], PackStreamValue::List(l) if l.len() == 2),
        "inner list has 2 elements"
    );
}

#[test]
fn encode_ignored_has_zero_fields() {
    let bytes = encode_response(&BoltResponse::Ignored).unwrap();
    let (val, _) = ps_decode(&bytes).unwrap();
    let PackStreamValue::Struct { tag, fields } = val else {
        panic!("expected Struct");
    };
    assert_eq!(tag, 0x7E, "IGNORED tag");
    assert_eq!(fields.len(), 0);
}

// ── Decode roundtrip: all 10 request types ────────────────────────────────────

#[allow(clippy::needless_pass_by_value)]
fn assert_request_roundtrip(req: BoltRequest) {
    let encoded = encode_request(&req).unwrap();
    let decoded = decode_request(&encoded).unwrap();
    assert_eq!(decoded, req);
}

#[test]
fn roundtrip_hello() {
    assert_request_roundtrip(BoltRequest::Hello {
        extra: sample_dict(),
    });
}

#[test]
fn roundtrip_logon() {
    assert_request_roundtrip(BoltRequest::Logon {
        auth: sample_dict(),
    });
}

#[test]
fn roundtrip_run() {
    assert_request_roundtrip(BoltRequest::Run {
        query: "MATCH (n:Person) RETURN n.name".to_owned(),
        params: vec![("limit".to_owned(), PackStreamValue::Int(10))],
        extra: empty_dict(),
    });
}

#[test]
fn roundtrip_pull() {
    assert_request_roundtrip(BoltRequest::Pull {
        extra: vec![("n".to_owned(), PackStreamValue::Int(100))],
    });
}

#[test]
fn roundtrip_discard() {
    assert_request_roundtrip(BoltRequest::Discard {
        extra: vec![("n".to_owned(), PackStreamValue::Int(-1))],
    });
}

#[test]
fn roundtrip_begin() {
    assert_request_roundtrip(BoltRequest::Begin {
        extra: vec![(
            "bookmarks".to_owned(),
            PackStreamValue::List(vec![PackStreamValue::String("bm:1234".to_owned())]),
        )],
    });
}

#[test]
fn roundtrip_commit() {
    assert_request_roundtrip(BoltRequest::Commit);
}

#[test]
fn roundtrip_rollback() {
    assert_request_roundtrip(BoltRequest::Rollback);
}

#[test]
fn roundtrip_reset() {
    assert_request_roundtrip(BoltRequest::Reset);
}

#[test]
fn roundtrip_goodbye() {
    assert_request_roundtrip(BoltRequest::Goodbye);
}

// ── Decode roundtrip: all 4 response types ────────────────────────────────────

#[allow(clippy::needless_pass_by_value)]
fn assert_response_roundtrip(resp: BoltResponse) {
    let encoded = encode_response(&resp).unwrap();
    let decoded = decode_response(&encoded).unwrap();
    assert_eq!(decoded, resp);
}

#[test]
fn roundtrip_success() {
    assert_response_roundtrip(BoltResponse::Success {
        metadata: vec![
            (
                "fields".to_owned(),
                PackStreamValue::List(vec![PackStreamValue::String("n".to_owned())]),
            ),
            ("t_first".to_owned(), PackStreamValue::Int(1)),
        ],
    });
}

#[test]
fn roundtrip_failure() {
    assert_response_roundtrip(BoltResponse::Failure {
        metadata: vec![
            (
                "code".to_owned(),
                PackStreamValue::String("Neo.ClientError.Statement.SyntaxError".to_owned()),
            ),
            (
                "message".to_owned(),
                PackStreamValue::String("Expected an expression".to_owned()),
            ),
        ],
    });
}

#[test]
fn roundtrip_record() {
    assert_response_roundtrip(BoltResponse::Record {
        fields: vec![
            PackStreamValue::Int(1),
            PackStreamValue::String("Alice".to_owned()),
            PackStreamValue::Bool(true),
        ],
    });
}

#[test]
fn roundtrip_ignored() {
    assert_response_roundtrip(BoltResponse::Ignored);
}

// ── Error cases ───────────────────────────────────────────────────────────────

#[test]
fn decode_request_unknown_tag_returns_unexpected_tag_error() {
    // Encode a struct with a tag that has no mapping in decode_request
    use tessera_graph_protocol::packstream::encode as ps_encode;
    let unknown_struct = PackStreamValue::Struct {
        tag: 0xAB, // not a valid Bolt request tag
        fields: vec![],
    };
    let mut buf = Vec::new();
    ps_encode(&unknown_struct, &mut buf).unwrap();

    let err = decode_request(&buf).unwrap_err();
    assert!(
        matches!(err, ProtocolError::BoltUnexpectedTag { got: 0xAB, .. }),
        "expected BoltUnexpectedTag with got=0xAB, got: {err:?}"
    );
}

#[test]
fn decode_request_hello_zero_fields_returns_missing_field_error() {
    // HELLO struct with no fields — extra dict is missing
    use tessera_graph_protocol::packstream::encode as ps_encode;
    let hello_empty = PackStreamValue::Struct {
        tag: 0x01,
        fields: vec![],
    };
    let mut buf = Vec::new();
    ps_encode(&hello_empty, &mut buf).unwrap();

    let err = decode_request(&buf).unwrap_err();
    assert!(
        matches!(
            err,
            ProtocolError::BoltMissingField {
                message: "Hello",
                field: "extra"
            }
        ),
        "expected BoltMissingField for Hello.extra, got: {err:?}"
    );
}

#[test]
fn decode_request_run_one_field_returns_missing_field_error() {
    // RUN struct with only 1 field — params and extra are missing
    use tessera_graph_protocol::packstream::encode as ps_encode;
    let run_incomplete = PackStreamValue::Struct {
        tag: 0x10,
        fields: vec![PackStreamValue::String("MATCH (n) RETURN n".to_owned())],
    };
    let mut buf = Vec::new();
    ps_encode(&run_incomplete, &mut buf).unwrap();

    let err = decode_request(&buf).unwrap_err();
    assert!(
        matches!(
            err,
            ProtocolError::BoltMissingField {
                message: "Run",
                field: "params"
            }
        ),
        "expected BoltMissingField for Run.params, got: {err:?}"
    );
}

#[test]
fn decode_request_non_struct_returns_unexpected_tag_error() {
    // Encode a plain string — not a struct at all
    use tessera_graph_protocol::packstream::encode as ps_encode;
    let mut buf = Vec::new();
    ps_encode(
        &PackStreamValue::String("not a struct".to_owned()),
        &mut buf,
    )
    .unwrap();

    let err = decode_request(&buf).unwrap_err();
    assert!(
        matches!(err, ProtocolError::BoltUnexpectedTag { .. }),
        "expected BoltUnexpectedTag for non-struct input, got: {err:?}"
    );
}

#[test]
fn decode_response_unknown_tag_returns_unexpected_tag_error() {
    use tessera_graph_protocol::packstream::encode as ps_encode;
    let unknown_struct = PackStreamValue::Struct {
        tag: 0xCC,
        fields: vec![],
    };
    let mut buf = Vec::new();
    ps_encode(&unknown_struct, &mut buf).unwrap();

    let err = decode_response(&buf).unwrap_err();
    assert!(
        matches!(err, ProtocolError::BoltUnexpectedTag { got: 0xCC, .. }),
        "expected BoltUnexpectedTag with got=0xCC, got: {err:?}"
    );
}

// ── Integration: full stack encode → BoltChunked → decode ─────────────────────

#[tokio::test]
async fn full_stack_request_hello_survives_chunked_transport() {
    let original = BoltRequest::Hello {
        extra: vec![(
            "user_agent".to_owned(),
            PackStreamValue::String("tessera/1.0".to_owned()),
        )],
    };

    let encoded = encode_request(&original).unwrap();

    let (mut writer, mut reader) = make_pair(4096);
    writer.write_message(&encoded).await.unwrap();

    let wire_bytes = reader.read_message().await.unwrap().unwrap();
    let decoded = decode_request(&wire_bytes).unwrap();

    assert_eq!(decoded, original);
}

#[tokio::test]
async fn full_stack_request_run_survives_chunked_transport() {
    let original = BoltRequest::Run {
        query: "MATCH (n:Person {name: $name}) RETURN n".to_owned(),
        params: vec![(
            "name".to_owned(),
            PackStreamValue::String("Alice".to_owned()),
        )],
        extra: empty_dict(),
    };

    let encoded = encode_request(&original).unwrap();

    let (mut writer, mut reader) = make_pair(4096);
    writer.write_message(&encoded).await.unwrap();

    let wire_bytes = reader.read_message().await.unwrap().unwrap();
    let decoded = decode_request(&wire_bytes).unwrap();

    assert_eq!(decoded, original);
}

#[tokio::test]
async fn full_stack_response_success_survives_chunked_transport() {
    let original = BoltResponse::Success {
        metadata: vec![
            (
                "fields".to_owned(),
                PackStreamValue::List(vec![PackStreamValue::String("n".to_owned())]),
            ),
            ("t_first".to_owned(), PackStreamValue::Int(12)),
        ],
    };

    let encoded = encode_response(&original).unwrap();

    let (mut writer, mut reader) = make_pair(4096);
    writer.write_message(&encoded).await.unwrap();

    let wire_bytes = reader.read_message().await.unwrap().unwrap();
    let decoded = decode_response(&wire_bytes).unwrap();

    assert_eq!(decoded, original);
}

#[tokio::test]
async fn full_stack_response_record_survives_chunked_transport() {
    let original = BoltResponse::Record {
        fields: vec![
            PackStreamValue::Int(99),
            PackStreamValue::String("Bob".to_owned()),
            PackStreamValue::Bool(false),
            PackStreamValue::Null,
        ],
    };

    let encoded = encode_response(&original).unwrap();

    let (mut writer, mut reader) = make_pair(4096);
    writer.write_message(&encoded).await.unwrap();

    let wire_bytes = reader.read_message().await.unwrap().unwrap();
    let decoded = decode_response(&wire_bytes).unwrap();

    assert_eq!(decoded, original);
}

#[tokio::test]
async fn full_stack_multiple_messages_in_sequence() {
    let hello = BoltRequest::Hello {
        extra: empty_dict(),
    };
    let run = BoltRequest::Run {
        query: "RETURN 1".to_owned(),
        params: empty_dict(),
        extra: empty_dict(),
    };
    let pull = BoltRequest::Pull {
        extra: vec![("n".to_owned(), PackStreamValue::Int(-1))],
    };

    let (mut writer, mut reader) = make_pair(8192);

    writer
        .write_message(&encode_request(&hello).unwrap())
        .await
        .unwrap();
    writer
        .write_message(&encode_request(&run).unwrap())
        .await
        .unwrap();
    writer
        .write_message(&encode_request(&pull).unwrap())
        .await
        .unwrap();

    let m1 = decode_request(&reader.read_message().await.unwrap().unwrap()).unwrap();
    let m2 = decode_request(&reader.read_message().await.unwrap().unwrap()).unwrap();
    let m3 = decode_request(&reader.read_message().await.unwrap().unwrap()).unwrap();

    assert_eq!(m1, hello);
    assert_eq!(m2, run);
    assert_eq!(m3, pull);
}
