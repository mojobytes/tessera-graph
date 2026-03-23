// Copyright 2026 BelowZero Security OU. All rights reserved.

//! Integration tests verifying that mutations are flushed to disk and survive reopen.

mod common;

use std::sync::{Arc, RwLock};
use tessera_graph::{Graph, GraphConfig};
use tessera_protocol::message::{ClientMessage, ServerMessage};

use common::{send_recv, spawn_handler, test_context};

async fn login_and_create(
    writer: &mut tessera_protocol::frame::FramedWriter<
        tokio::io::WriteHalf<tokio::io::DuplexStream>,
    >,
    reader: &mut tessera_protocol::frame::FramedReader<
        tokio::io::ReadHalf<tokio::io::DuplexStream>,
    >,
) {
    let resp = send_recv(
        writer,
        reader,
        &ClientMessage::Login {
            username: "admin".into(),
            password: "Admin@Init1!".into(),
        },
    )
    .await;
    assert!(
        matches!(resp, ServerMessage::AuthOk { .. }),
        "login failed: {resp:?}"
    );

    let resp = send_recv(
        writer,
        reader,
        &ClientMessage::Query {
            query: "CREATE (n:Durable {name: 'persisted'})".into(),
            language: "gql".into(),
        },
    )
    .await;
    assert!(
        matches!(resp, ServerMessage::QueryResult { .. }),
        "mutation failed: {resp:?}"
    );
}

#[tokio::test]
async fn mutation_is_flushed_and_survives_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let data_path = dir.path().to_path_buf();
    let config = GraphConfig::new();

    // --- First session: create a node via the wire protocol ---
    {
        let graph = Arc::new(RwLock::new(
            Graph::open(&data_path, &config).unwrap(),
        ));
        let ctx = test_context();
        let (mut writer, mut reader, shutdown_tx) = spawn_handler(ctx, Arc::clone(&graph));

        login_and_create(&mut writer, &mut reader).await;

        // Shut down the handler cleanly
        let _ = shutdown_tx.send(true);
        drop(writer);
        drop(reader);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    // --- Second session: reopen and verify data survived ---
    {
        let graph = Graph::open(&data_path, &config).unwrap();
        let ids = graph.nodes_by_label("Durable");
        assert_eq!(
            ids.len(),
            1,
            "expected 1 Durable node after reopen, found {}",
            ids.len()
        );
    }
}

#[tokio::test]
async fn flush_is_noop_on_in_memory_graph() {
    // Graph::flush() is a no-op on MemoryBackend — mutations work but don't persist.
    // This verifies no panic or error when flush is called on in-memory graph.
    let graph = Arc::new(RwLock::new(Graph::new()));
    let ctx = test_context();
    let (mut writer, mut reader, _shutdown) = spawn_handler(ctx, graph);

    login_and_create(&mut writer, &mut reader).await;
    // If flush panicked on MemoryBackend, we would not reach this point.
}
