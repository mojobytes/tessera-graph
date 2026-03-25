// Copyright 2026 BelowZero Security OU. All rights reserved.

mod common;

use tessera_protocol::bolt_message::{BoltRequest, BoltResponse};

use common::{bolt_recv, bolt_send, spawn_bolt_handler, test_context};

/// Measure the RUN+PULL round-trip throughput over the Bolt protocol.
///
/// Each iteration: HELLO → RUN → PULL → (loop).  HELLO is done once before
/// the timed section to avoid measuring auth cost.
#[tokio::test]
async fn bolt_run_pull_throughput_guard() {
    let (_dir, ctx) = test_context();
    let (mut writer, mut reader, _shutdown) = spawn_bolt_handler(ctx).await;

    // Authenticate once.
    bolt_send(
        &mut writer,
        &BoltRequest::Hello {
            extra: vec![
                (
                    "principal".to_owned(),
                    tessera_protocol::PackStreamValue::String("admin".to_owned()),
                ),
                (
                    "credentials".to_owned(),
                    tessera_protocol::PackStreamValue::String("Admin@Init1!".to_owned()),
                ),
            ],
        },
    )
    .await;
    let hello_resp = bolt_recv(&mut reader).await;
    assert!(
        matches!(hello_resp, BoltResponse::Success { .. }),
        "HELLO failed: {hello_resp:?}"
    );

    let n: u64 = 1_000;
    let start = std::time::Instant::now();

    for _ in 0..n {
        bolt_send(
            &mut writer,
            &BoltRequest::Run {
                query: "MATCH (n) RETURN n".to_owned(),
                params: vec![],
                extra: vec![],
            },
        )
        .await;
        let run_resp = bolt_recv(&mut reader).await;
        assert!(
            matches!(run_resp, BoltResponse::Success { .. }),
            "RUN failed: {run_resp:?}"
        );

        bolt_send(&mut writer, &BoltRequest::Pull { extra: vec![] }).await;
        // Drain records + final SUCCESS.
        loop {
            let resp = bolt_recv(&mut reader).await;
            if matches!(resp, BoltResponse::Success { .. }) {
                break;
            }
        }
    }

    let elapsed = start.elapsed();
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    let rps = (n as f64 / elapsed.as_secs_f64()) as u64;

    let min_rps: u64 = if cfg!(debug_assertions) { 200 } else { 2_000 };

    assert!(
        rps >= min_rps,
        "bolt RUN+PULL regression: {rps} rps < {min_rps}"
    );
}
