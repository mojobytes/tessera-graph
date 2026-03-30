// Copyright 2026 BelowZero Security OU. All rights reserved.

//! Throughput regression guard for GQL mutation round-trips.
//!
//! BASELINE (before deferred flush): ~115 ops/s — dominated by per-mutation
//! `graph.flush()`.  After deferred flush: expected ≥300 ops/s (debug).

mod common;

use tessera_protocol::bolt_message::{BoltRequest, BoltResponse};

use common::{bolt_recv, bolt_send, spawn_bolt_handler, test_context};

/// Measure GQL mutation throughput: HELLO (once) → N × (RUN CREATE + PULL).
#[tokio::test]
async fn mutation_throughput_guard() {
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

    let n: u64 = 500;
    let start = std::time::Instant::now();

    for i in 0..n {
        bolt_send(
            &mut writer,
            &BoltRequest::Run {
                query: format!("CREATE (n:Bench {{i: {i}}})"),
                params: vec![],
                extra: vec![],
            },
        )
        .await;
        let run_resp = bolt_recv(&mut reader).await;
        assert!(
            matches!(run_resp, BoltResponse::Success { .. }),
            "RUN failed at i={i}: {run_resp:?}"
        );

        bolt_send(&mut writer, &BoltRequest::Pull { extra: vec![] }).await;
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

    // Debug builds are ~10x slower due to missing optimisations, especially
    // GQL parsing and graph mutation.  The guard catches gross regressions,
    // not micro-benchmarking.
    let min_rps: u64 = if cfg!(debug_assertions) { 30 } else { 500 };

    eprintln!("mutation throughput: {rps} ops/s ({elapsed:?} for {n} mutations)");

    assert!(
        rps >= min_rps,
        "mutation throughput regression: {rps} ops/s < {min_rps} minimum"
    );
}
