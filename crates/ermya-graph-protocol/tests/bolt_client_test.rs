// SPDX-License-Identifier: BSL-1.1

//! Integration tests for `BoltClient::run_query` error handling.

use ermya_graph_protocol::{
    BoltResponse, PackStreamValue, ProtocolError, SUPPORTED_VERSION, encode_response,
    encode_version_response,
};

use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};

// ── Helpers ─────────────────────────────────────────────────────────────────

/// Accepted Bolt 4.4 version response derived from the library's own encoder.
const BOLT_44_VERSION: [u8; 4] = encode_version_response(Some(SUPPORTED_VERSION));

/// Encode a `BoltResponse` into a chunked-framed message (length-prefixed
/// chunks + zero terminator).
fn frame_response(resp: &BoltResponse) -> Vec<u8> {
    let payload = encode_response(resp).expect("encode_response");
    let mut buf = Vec::new();
    // Single chunk: u16-BE length + payload + 0x0000 terminator.
    let len = u16::try_from(payload.len())
        .expect("test payload must fit in a single Bolt chunk (≤ 65535 bytes)");
    buf.extend_from_slice(&len.to_be_bytes());
    buf.extend_from_slice(&payload);
    buf.extend_from_slice(&[0x00, 0x00]);
    buf
}

/// Perform handshake + HELLO on `stream`, returning the stream for further use.
async fn handshake_and_hello(stream: &mut DuplexStream) {
    // Handshake
    let mut handshake = [0u8; 20];
    stream.read_exact(&mut handshake).await.unwrap();
    stream.write_all(&BOLT_44_VERSION).await.unwrap();
    stream.flush().await.unwrap();

    // HELLO → SUCCESS
    drain_one_message(stream).await;
    let success = frame_response(&BoltResponse::Success { metadata: vec![] });
    stream.write_all(&success).await.unwrap();
    stream.flush().await.unwrap();
}

/// Run a mock Bolt server for `run_query` tests.
///
/// 1. Handshake + HELLO → SUCCESS.
/// 2. RUN → `run_response`.
/// 3. If `pull_responses` is non-empty, drains PULL and writes each response.
async fn mock_server(
    mut stream: DuplexStream,
    run_response: BoltResponse,
    pull_responses: Vec<BoltResponse>,
) {
    handshake_and_hello(&mut stream).await;

    // RUN → run_response
    drain_one_message(&mut stream).await;
    let run_bytes = frame_response(&run_response);
    stream.write_all(&run_bytes).await.unwrap();
    stream.flush().await.unwrap();

    // PULL → pull_responses
    if !pull_responses.is_empty() {
        drain_one_message(&mut stream).await;
        for resp in &pull_responses {
            let bytes = frame_response(resp);
            stream.write_all(&bytes).await.unwrap();
        }
        stream.flush().await.unwrap();
    }
}

/// Drain a single chunked Bolt message from the stream (reads chunks until the
/// zero terminator).
async fn drain_one_message(stream: &mut DuplexStream) {
    loop {
        let mut header = [0u8; 2];
        stream.read_exact(&mut header).await.unwrap();
        let len = u16::from_be_bytes(header) as usize;
        if len == 0 {
            break;
        }
        let mut buf = vec![0u8; len];
        stream.read_exact(&mut buf).await.unwrap();
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn run_query_returns_error_when_run_is_ignored() {
    let (client_stream, server_stream) = tokio::io::duplex(8192);

    let server = tokio::spawn(mock_server(
        server_stream,
        BoltResponse::Ignored,
        vec![], // no PULL expected — client should bail after RUN IGNORED
    ));

    let mut client = ermya_graph_protocol::connect(client_stream).await.unwrap();
    client.hello("neo4j", "test", None).await.unwrap();

    let result = client.run_query("RETURN 1").await;

    assert!(result.is_err(), "run_query should fail when RUN is IGNORED");
    assert!(
        matches!(result.unwrap_err(), ProtocolError::BoltConnectionIgnored),
        "expected BoltConnectionIgnored"
    );

    server.await.unwrap();
}

#[tokio::test]
async fn run_query_returns_error_when_pull_is_ignored() {
    let (client_stream, server_stream) = tokio::io::duplex(8192);

    // RUN succeeds, but PULL returns IGNORED (simulates FAILED state entered
    // between RUN and PULL, or a server that accepted RUN but then failed).
    let server = tokio::spawn(mock_server(
        server_stream,
        BoltResponse::Success {
            metadata: vec![(
                "fields".to_owned(),
                PackStreamValue::List(vec![PackStreamValue::String("x".to_owned())]),
            )],
        },
        vec![BoltResponse::Ignored],
    ));

    let mut client = ermya_graph_protocol::connect(client_stream).await.unwrap();
    client.hello("neo4j", "test", None).await.unwrap();

    let result = client.run_query("RETURN 1 AS x").await;

    assert!(
        result.is_err(),
        "run_query should fail when PULL is IGNORED"
    );
    assert!(
        matches!(result.unwrap_err(), ProtocolError::BoltConnectionIgnored),
        "expected BoltConnectionIgnored"
    );

    server.await.unwrap();
}

#[tokio::test]
async fn run_query_returns_error_when_run_gets_unexpected_record() {
    let (client_stream, server_stream) = tokio::io::duplex(8192);

    let server = tokio::spawn(mock_server(
        server_stream,
        BoltResponse::Record {
            fields: vec![PackStreamValue::Int(42)],
        },
        vec![],
    ));

    let mut client = ermya_graph_protocol::connect(client_stream).await.unwrap();
    client.hello("neo4j", "test", None).await.unwrap();

    let result = client.run_query("RETURN 1").await;

    assert!(
        result.is_err(),
        "run_query should fail on unexpected RECORD to RUN"
    );
    let err = result.unwrap_err();
    match &err {
        ProtocolError::BoltQueryFailure { message } => {
            assert!(
                message.contains("unexpected RECORD"),
                "error message should mention unexpected RECORD, got: {message}"
            );
        }
        other => panic!("expected BoltQueryFailure, got: {other:?}"),
    }

    server.await.unwrap();
}

#[tokio::test]
async fn run_query_success_still_works() {
    let (client_stream, server_stream) = tokio::io::duplex(8192);

    let server = tokio::spawn(mock_server(
        server_stream,
        BoltResponse::Success {
            metadata: vec![(
                "fields".to_owned(),
                PackStreamValue::List(vec![PackStreamValue::String("n".to_owned())]),
            )],
        },
        vec![
            BoltResponse::Record {
                fields: vec![PackStreamValue::Int(1)],
            },
            BoltResponse::Success { metadata: vec![] },
        ],
    ));

    let mut client = ermya_graph_protocol::connect(client_stream).await.unwrap();
    client.hello("neo4j", "test", None).await.unwrap();

    let result = client.run_query("RETURN 1 AS n").await.unwrap();

    assert_eq!(result.columns, vec!["n"]);
    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0], vec![PackStreamValue::Int(1)]);

    server.await.unwrap();
}

// ── reset() tests ───────────────────────────────────────────────────────────

/// Mock server that completes handshake + HELLO, then responds to RESET with
/// the given `BoltResponse`.
async fn mock_server_reset(mut stream: DuplexStream, reset_response: BoltResponse) {
    handshake_and_hello(&mut stream).await;

    // RESET → reset_response
    drain_one_message(&mut stream).await;
    let resp = frame_response(&reset_response);
    stream.write_all(&resp).await.unwrap();
    stream.flush().await.unwrap();
}

#[tokio::test]
async fn reset_returns_ok_on_success() {
    let (client_stream, server_stream) = tokio::io::duplex(8192);

    let server = tokio::spawn(mock_server_reset(
        server_stream,
        BoltResponse::Success { metadata: vec![] },
    ));

    let mut client = ermya_graph_protocol::connect(client_stream).await.unwrap();
    client.hello("neo4j", "test", None).await.unwrap();

    assert!(client.reset().await.is_ok());

    server.await.unwrap();
}

#[tokio::test]
async fn reset_returns_error_on_failure() {
    let (client_stream, server_stream) = tokio::io::duplex(8192);

    let server = tokio::spawn(mock_server_reset(
        server_stream,
        BoltResponse::Failure {
            metadata: vec![(
                "message".to_owned(),
                PackStreamValue::String("reset denied".to_owned()),
            )],
        },
    ));

    let mut client = ermya_graph_protocol::connect(client_stream).await.unwrap();
    client.hello("neo4j", "test", None).await.unwrap();

    let result = client.reset().await;
    assert!(result.is_err());
    match result.unwrap_err() {
        ProtocolError::BoltResetFailure { message } => {
            assert_eq!(message, "reset denied");
        }
        other => panic!("expected BoltResetFailure, got: {other:?}"),
    }

    server.await.unwrap();
}

#[tokio::test]
async fn reset_returns_error_on_ignored() {
    let (client_stream, server_stream) = tokio::io::duplex(8192);

    let server = tokio::spawn(mock_server_reset(server_stream, BoltResponse::Ignored));

    let mut client = ermya_graph_protocol::connect(client_stream).await.unwrap();
    client.hello("neo4j", "test", None).await.unwrap();

    let result = client.reset().await;
    assert!(result.is_err());
    assert!(
        matches!(result.unwrap_err(), ProtocolError::BoltConnectionIgnored),
        "expected BoltConnectionIgnored"
    );

    server.await.unwrap();
}

// ── no-PULL assertion ──────────────────────────────────────────────────────

#[tokio::test]
async fn run_ignored_does_not_send_pull() {
    let (client_stream, server_stream) = tokio::io::duplex(8192);

    let server = tokio::spawn(async move {
        let mut stream = server_stream;
        handshake_and_hello(&mut stream).await;

        // RUN → IGNORED
        drain_one_message(&mut stream).await;
        let ignored = frame_response(&BoltResponse::Ignored);
        stream.write_all(&ignored).await.unwrap();
        stream.flush().await.unwrap();

        // The client should NOT send PULL. Try to read another message —
        // if the client correctly bails, the stream will close (EOF) or no
        // data arrives within the timeout.
        let mut header = [0u8; 2];
        let read = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            stream.read_exact(&mut header),
        )
        .await;

        // Timeout or EOF is acceptable. A successful read proves that the
        // client sent another Bolt message after RUN was ignored.
        assert!(
            !matches!(read, Ok(Ok(_))),
            "client sent unexpected data after RUN IGNORED (likely a PULL)"
        );
    });

    let mut client = ermya_graph_protocol::connect(client_stream).await.unwrap();
    client.hello("neo4j", "test", None).await.unwrap();

    let result = client.run_query("RETURN 1").await;
    assert!(matches!(
        result.unwrap_err(),
        ProtocolError::BoltConnectionIgnored
    ));

    // Drop client to close the stream so the server task can finish.
    drop(client);
    server.await.unwrap();
}

// ── Handshake version validation ───────────────────────────────────────────

#[tokio::test]
async fn connect_rejects_unsupported_version_from_server() {
    let (client_stream, server_stream) = tokio::io::duplex(8192);

    let server = tokio::spawn(async move {
        let mut stream = server_stream;
        let mut handshake = [0u8; 20];
        stream.read_exact(&mut handshake).await.unwrap();
        // Respond with Bolt 5.0 — not what the client supports.
        // Wire format: [0x00, 0x00, minor=0, major=5]
        let fake_version = [0x00, 0x00, 0x00, 0x05];
        stream.write_all(&fake_version).await.unwrap();
        stream.flush().await.unwrap();
    });

    let result = ermya_graph_protocol::connect(client_stream).await;
    let err = result
        .err()
        .expect("connect should fail with unsupported version");
    match err {
        ProtocolError::BoltInvalidHandshake { reason } => {
            assert!(
                reason.contains("unsupported"),
                "expected 'unsupported' in reason, got: {reason}"
            );
        }
        other => panic!("expected BoltInvalidHandshake, got: {other:?}"),
    }

    server.await.unwrap();
}

#[tokio::test]
async fn connect_accepts_exact_supported_version() {
    let (client_stream, server_stream) = tokio::io::duplex(8192);

    let server = tokio::spawn(async move {
        let mut stream = server_stream;
        let mut handshake = [0u8; 20];
        stream.read_exact(&mut handshake).await.unwrap();
        stream.write_all(&BOLT_44_VERSION).await.unwrap();
        stream.flush().await.unwrap();
        // Client will try HELLO next — just drop the connection.
    });

    let result = ermya_graph_protocol::connect(client_stream).await;
    assert!(
        result.is_ok(),
        "connect should succeed with matching version"
    );

    server.await.unwrap();
}

#[tokio::test]
async fn handshake_round_trip_negotiate_then_connect() {
    use ermya_graph_protocol::negotiate_version;

    let (client_stream, server_stream) = tokio::io::duplex(8192);

    // Server side: use the real negotiate_version + encode_version_response
    let server = tokio::spawn(async move {
        let mut stream = server_stream;
        let mut handshake = [0u8; 20];
        stream.read_exact(&mut handshake).await.unwrap();

        let negotiated =
            negotiate_version(&handshake).expect("server should accept the client's proposal");
        assert_eq!(negotiated, SUPPORTED_VERSION);

        let response = encode_version_response(Some(negotiated));
        stream.write_all(&response).await.unwrap();
        stream.flush().await.unwrap();
    });

    // Client side: use connect_split via the public connect() helper
    let result = ermya_graph_protocol::connect(client_stream).await;
    assert!(result.is_ok(), "round-trip handshake should succeed");

    server.await.unwrap();
}
