// Copyright 2026 BelowZero Security OU. All rights reserved.

use bytes::BytesMut;
use tessera_protocol::frame;

#[test]
fn frame_codec_throughput_guard() {
    let payload = vec![0xABu8; 256];
    let n: u64 = 100_000;
    let start = std::time::Instant::now();

    for _ in 0..n {
        let encoded = frame::encode(&payload);
        let mut buf = BytesMut::from(encoded.as_slice());
        let _ = frame::decode(&mut buf).unwrap();
    }

    let elapsed = start.elapsed();
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    let ops_per_sec = (n as f64 / elapsed.as_secs_f64()) as u64;

    let min_ops: u64 = if cfg!(debug_assertions) {
        200_000
    } else {
        1_000_000
    };

    assert!(
        ops_per_sec >= min_ops,
        "frame codec regression: {ops_per_sec} ops/s < {min_ops}"
    );
}
