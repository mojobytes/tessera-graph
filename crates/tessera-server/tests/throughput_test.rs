// Copyright 2026 BelowZero Security OU. All rights reserved.

mod common;

use std::sync::{Arc, RwLock};
use std::time::Duration;

use tessera_graph::Graph;
use tessera_protocol::frame::{FramedReader, FramedWriter};
use tessera_protocol::message::{ClientMessage, ServerMessage};
use tessera_server::TesseraListener;

use common::test_context;

#[tokio::test]
async fn ping_pong_throughput_guard() {
    let ctx = test_context();
    let graph = Arc::new(RwLock::new(Graph::new()));
    // Plain TCP throughput test — TLS throughput is a separate benchmark concern.
    let listener = TesseraListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    tokio::spawn(async move {
        let _ = listener
            .serve(ctx, graph, shutdown_rx, 10, Duration::from_secs(30))
            .await;
    });

    tokio::time::sleep(Duration::from_millis(50)).await;

    let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let (read_half, write_half) = tokio::io::split(stream);
    let mut writer = FramedWriter::new(write_half);
    let mut reader = FramedReader::new(read_half);

    let ping_json = serde_json::to_vec(&ClientMessage::Ping).unwrap();

    let n: u64 = 10_000;
    let start = std::time::Instant::now();

    for _ in 0..n {
        writer.write_frame(&ping_json).await.unwrap();
        let frame = reader.read_frame().await.unwrap().unwrap();
        let response: ServerMessage = serde_json::from_slice(&frame).unwrap();
        assert_eq!(response, ServerMessage::Pong);
    }

    let elapsed = start.elapsed();
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    let rps = (n as f64 / elapsed.as_secs_f64()) as u64;

    let min_rps: u64 = if cfg!(debug_assertions) {
        2_000
    } else {
        20_000
    };

    assert!(
        rps >= min_rps,
        "ping-pong regression: {rps} rps < {min_rps}"
    );

    let _ = shutdown_tx.send(true);
}
