// Copyright 2026 BelowZero Security OU. All rights reserved.

//! Integration tests for the Bolt chunked transport framing layer.

use tessera_graph_protocol::packstream::{decode, encode};
use tessera_graph_protocol::{BoltChunkedReader, BoltChunkedWriter, MAX_CHUNK_SIZE, PackStreamValue};

// ── Helpers ──────────────────────────────────────────────────────────────────

/// Build a duplex pair and return (writer-half, reader-half) with a generous buffer.
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

// ── Writer tests ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn writer_small_message_produces_correct_wire_bytes() {
    let payload = b"0123456789"; // 10 bytes
    let mut wire = Vec::new();
    let mut writer = BoltChunkedWriter::new(&mut wire);
    writer.write_message(payload).await.unwrap();

    // Expected: [0x00, 0x0A][10 bytes payload][0x00, 0x00]
    let mut expected = Vec::new();
    expected.extend_from_slice(&[0x00, 0x0A]);
    expected.extend_from_slice(payload);
    expected.extend_from_slice(&[0x00, 0x00]);

    assert_eq!(wire, expected);
}

#[tokio::test]
async fn writer_empty_message_produces_only_terminator() {
    let mut wire = Vec::new();
    let mut writer = BoltChunkedWriter::new(&mut wire);
    writer.write_message(&[]).await.unwrap();

    assert_eq!(wire, &[0x00, 0x00]);
}

#[tokio::test]
async fn writer_large_message_splits_into_multiple_chunks() {
    let payload = vec![0xABu8; 70_000];
    let mut wire = Vec::new();
    let mut writer = BoltChunkedWriter::new(&mut wire);
    writer.write_message(&payload).await.unwrap();

    // First chunk: 65535 bytes
    let first_size: usize = MAX_CHUNK_SIZE;
    // Second chunk: 70000 - 65535 = 4465 bytes
    let second_size: usize = 70_000 - MAX_CHUNK_SIZE;

    let mut expected = Vec::new();
    // First chunk header + data (both sizes are known to fit in u16)
    #[allow(clippy::cast_possible_truncation)]
    expected.extend_from_slice(&(first_size as u16).to_be_bytes());
    expected.extend_from_slice(&payload[..first_size]);
    // Second chunk header + data
    #[allow(clippy::cast_possible_truncation)]
    expected.extend_from_slice(&(second_size as u16).to_be_bytes());
    expected.extend_from_slice(&payload[first_size..]);
    // Terminator
    expected.extend_from_slice(&[0x00, 0x00]);

    assert_eq!(wire, expected);
}

// ── Reader tests ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn reader_small_message_reassembles_payload() {
    // Wire bytes: [0x00, 0x05] b"hello" [0x00, 0x00]
    let mut wire: &[u8] = &[0x00, 0x05, b'h', b'e', b'l', b'l', b'o', 0x00, 0x00];
    let mut reader = BoltChunkedReader::new(&mut wire);
    let msg = reader.read_message().await.unwrap();
    assert_eq!(msg, Some(b"hello".to_vec()));
}

#[tokio::test]
async fn reader_large_message_multi_chunk_reassembly() {
    let payload = vec![0xCDu8; 70_000];
    let first_size: usize = MAX_CHUNK_SIZE;
    let second_size: usize = 70_000 - MAX_CHUNK_SIZE;

    let mut wire = Vec::new();
    // Both sizes fit in u16; cast is intentional.
    #[allow(clippy::cast_possible_truncation)]
    wire.extend_from_slice(&(first_size as u16).to_be_bytes());
    wire.extend_from_slice(&payload[..first_size]);
    #[allow(clippy::cast_possible_truncation)]
    wire.extend_from_slice(&(second_size as u16).to_be_bytes());
    wire.extend_from_slice(&payload[first_size..]);
    wire.extend_from_slice(&[0x00, 0x00]);

    let mut cursor: &[u8] = &wire;
    let mut reader = BoltChunkedReader::new(&mut cursor);
    let msg = reader.read_message().await.unwrap();
    assert_eq!(msg, Some(payload));
}

#[tokio::test]
async fn reader_returns_none_on_clean_eof() {
    // Empty slice → immediate EOF before any bytes.
    let mut wire: &[u8] = &[];
    let mut reader = BoltChunkedReader::new(&mut wire);
    let msg = reader.read_message().await.unwrap();
    assert_eq!(msg, None);
}

/// A reader configured with a 1 KiB limit must reject a message assembled
/// from two 600-byte chunks (1200 bytes total > 1024 bytes limit).
#[tokio::test]
async fn reader_rejects_message_exceeding_size_limit() {
    // Build wire bytes: chunk1 (600 bytes) + chunk2 (600 bytes) + terminator.
    // Total payload = 1200 bytes, which exceeds the 1024-byte limit.
    let chunk_payload = vec![0xAAu8; 600];
    let mut wire = Vec::new();
    // Chunk 1
    wire.extend_from_slice(&600u16.to_be_bytes());
    wire.extend_from_slice(&chunk_payload);
    // Chunk 2
    wire.extend_from_slice(&600u16.to_be_bytes());
    wire.extend_from_slice(&chunk_payload);
    // Terminator
    wire.extend_from_slice(&[0x00, 0x00]);

    let mut cursor: &[u8] = &wire;
    let mut reader = BoltChunkedReader::new(&mut cursor).with_max_message_size(1024);
    let err = reader.read_message().await.unwrap_err();
    assert_eq!(
        err.kind(),
        std::io::ErrorKind::InvalidData,
        "expected InvalidData for oversized message, got {err:?}"
    );
}

// ── Round-trip tests ─────────────────────────────────────────────────────────

#[tokio::test]
async fn roundtrip_single_small_message() {
    let (mut writer, mut reader) = make_pair(4096);
    let original = b"round-trip test";
    writer.write_message(original).await.unwrap();
    drop(writer); // allow reader to see EOF eventually if needed
    let result = reader.read_message().await.unwrap();
    assert_eq!(result, Some(original.to_vec()));
}

#[tokio::test]
async fn roundtrip_three_messages_in_order() {
    let (mut writer, mut reader) = make_pair(4096);

    writer.write_message(b"alpha").await.unwrap();
    writer.write_message(b"beta").await.unwrap();
    writer.write_message(b"gamma").await.unwrap();

    let a = reader.read_message().await.unwrap();
    let b = reader.read_message().await.unwrap();
    let c = reader.read_message().await.unwrap();

    assert_eq!(a, Some(b"alpha".to_vec()));
    assert_eq!(b, Some(b"beta".to_vec()));
    assert_eq!(c, Some(b"gamma".to_vec()));
}

#[tokio::test]
async fn roundtrip_large_message_100kb() {
    // Use a large duplex buffer so the entire 100 KiB fits without blocking.
    let (mut writer, mut reader) = make_pair(200_000);

    // Deterministic pattern: byte = (index % 251) as u8  (251 is prime)
    // i % 251 is in 0..=250, always non-negative; cast to u8 is safe.
    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
    let original: Vec<u8> = (0_i32..102_400).map(|i| (i % 251) as u8).collect();
    writer.write_message(&original).await.unwrap();

    let result = reader.read_message().await.unwrap();
    assert_eq!(result, Some(original));
}

// ── PackStream integration test ───────────────────────────────────────────────

#[tokio::test]
async fn packstream_value_survives_bolt_framing() {
    // Build a complex PackStreamValue using the actual Dict variant (ordered pairs).
    let original = PackStreamValue::Dict(vec![
        (
            "name".to_owned(),
            PackStreamValue::String("Neo4j Bolt".to_owned()),
        ),
        ("version".to_owned(), PackStreamValue::Int(5)),
        ("active".to_owned(), PackStreamValue::Bool(true)),
        (
            "tags".to_owned(),
            PackStreamValue::List(vec![
                PackStreamValue::String("graph".to_owned()),
                PackStreamValue::String("database".to_owned()),
            ]),
        ),
    ]);

    // encode returns Result<()> — unwrap is correct in test context.
    let mut encoded = Vec::new();
    encode(&original, &mut encoded).unwrap();

    let (mut writer, mut reader) = make_pair(4096);
    writer.write_message(&encoded).await.unwrap();

    let wire_bytes = reader.read_message().await.unwrap().unwrap();
    // decode returns Result<(PackStreamValue, usize)>
    let (decoded, _consumed) = decode(&wire_bytes).unwrap();

    assert_eq!(decoded, original);
}

// ── Wiring test ───────────────────────────────────────────────────────────────

#[test]
fn bolt_frame_is_publicly_accessible() {
    // Verify the constant is exported from the crate root.
    assert_eq!(MAX_CHUNK_SIZE, 65_535);
}
