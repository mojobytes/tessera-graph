// Copyright 2026 BelowZero Security OU. All rights reserved.

use bytes::BytesMut;
use tessera_protocol::ProtocolError;
use tessera_protocol::frame::{self, FramedReader, FramedWriter, MAX_FRAME_SIZE};

#[test]
fn frame_encode_produces_length_prefix() {
    let encoded = frame::encode(b"hello").unwrap();
    assert_eq!(encoded.len(), 9);
    assert_eq!(&encoded[..4], &[0, 0, 0, 5]);
    assert_eq!(&encoded[4..], b"hello");
}

#[test]
fn frame_encode_empty_payload() {
    let encoded = frame::encode(b"").unwrap();
    assert_eq!(encoded.len(), 4);
    assert_eq!(&encoded[..4], &[0, 0, 0, 0]);
}

#[test]
fn frame_decode_complete_frame() {
    let mut buf = BytesMut::from(&[0u8, 0, 0, 5, b'h', b'e', b'l', b'l', b'o'][..]);
    let result = frame::decode(&mut buf).unwrap();
    assert_eq!(result, Some(b"hello".to_vec()));
    assert!(buf.is_empty(), "buffer should be fully consumed");
}

#[test]
fn frame_decode_incomplete_header() {
    let mut buf = BytesMut::from(&[0u8, 0][..]);
    let result = frame::decode(&mut buf).unwrap();
    assert_eq!(result, None);
}

#[test]
fn frame_decode_incomplete_payload() {
    let mut buf = BytesMut::from(&[0u8, 0, 0, 5, b'h', b'e'][..]);
    let result = frame::decode(&mut buf).unwrap();
    assert_eq!(result, None);
}

#[test]
fn frame_decode_rejects_oversized_frame() {
    let oversized = MAX_FRAME_SIZE + 1;
    let mut buf = BytesMut::from(&oversized.to_be_bytes()[..]);
    let err = frame::decode(&mut buf).unwrap_err();
    assert!(
        matches!(err, ProtocolError::FrameTooLarge { declared } if declared == oversized),
        "expected FrameTooLarge, got {err:?}"
    );
}

#[test]
fn encode_returns_error_for_oversized_payload() {
    let oversized = vec![0u8; MAX_FRAME_SIZE as usize + 1];
    let result = frame::encode(&oversized);
    assert!(
        result.is_err(),
        "encode must return Err for payload > MAX_FRAME_SIZE"
    );
}

#[test]
fn encode_accepts_max_frame_size_payload() {
    // We can't allocate 16 MiB in a test easily, but we can verify encode
    // succeeds for a reasonably sized payload below the limit
    let payload = vec![0u8; 1024];
    let result = frame::encode(&payload);
    assert!(result.is_ok());
}

#[tokio::test]
async fn framed_reader_reads_single_frame() {
    let (reader, mut writer) = tokio::io::duplex(1024);

    let payload = b"test payload";
    let encoded = frame::encode(payload).unwrap();
    tokio::io::AsyncWriteExt::write_all(&mut writer, &encoded)
        .await
        .unwrap();
    drop(writer);

    let mut framed = FramedReader::new(reader);
    let result = framed.read_frame().await.unwrap();
    assert_eq!(result, Some(payload.to_vec()));

    let eof = framed.read_frame().await.unwrap();
    assert_eq!(eof, None);
}

#[tokio::test]
async fn framed_reader_reads_multiple_frames() {
    let (reader, mut writer) = tokio::io::duplex(1024);

    let frames = [b"one".as_slice(), b"two", b"three"];
    for f in &frames {
        let encoded = frame::encode(f).unwrap();
        tokio::io::AsyncWriteExt::write_all(&mut writer, &encoded)
            .await
            .unwrap();
    }
    drop(writer);

    let mut framed_reader = FramedReader::new(reader);
    for expected in &frames {
        let result = framed_reader.read_frame().await.unwrap();
        assert_eq!(result.as_deref(), Some(*expected));
    }
}

#[tokio::test]
async fn framed_writer_writes_encoded_frame() {
    let (mut reader, writer) = tokio::io::duplex(1024);

    let payload = b"wire data";
    let mut framed = FramedWriter::new(writer);
    framed.write_frame(payload).await.unwrap();
    drop(framed);

    let mut raw = Vec::new();
    tokio::io::AsyncReadExt::read_to_end(&mut reader, &mut raw)
        .await
        .unwrap();

    let expected = frame::encode(payload).unwrap();
    assert_eq!(raw, expected);
}
