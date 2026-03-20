// Copyright 2026 BelowZero Security OU. All rights reserved.

mod common;

use std::sync::{Arc, RwLock};

use tessera_graph::Graph;
use tessera_protocol::message::{ClientMessage, ServerMessage};

use common::{send_recv, spawn_handler, test_context};

#[tokio::test]
async fn connection_handler_rejects_query_without_login() {
    let ctx = test_context();
    let graph = Arc::new(RwLock::new(Graph::new()));
    let (mut writer, mut reader, _shutdown) = spawn_handler(ctx, graph);

    let response = send_recv(
        &mut writer,
        &mut reader,
        &ClientMessage::Query {
            query: "MATCH (n) RETURN n".into(),
            language: "gql".into(),
        },
    )
    .await;

    assert!(
        matches!(response, ServerMessage::AuthError { .. }),
        "expected AuthError, got {response:?}"
    );
}

#[tokio::test]
async fn connection_handler_login_returns_auth_ok() {
    let ctx = test_context();
    let graph = Arc::new(RwLock::new(Graph::new()));
    let (mut writer, mut reader, _shutdown) = spawn_handler(ctx, graph);

    let response = send_recv(
        &mut writer,
        &mut reader,
        &ClientMessage::Login {
            username: "admin".into(),
            password: "Admin@Init1!".into(),
        },
    )
    .await;

    match response {
        ServerMessage::AuthOk { ref token } => {
            assert!(!token.is_empty(), "token should not be empty");
        }
        other => panic!("expected AuthOk, got {other:?}"),
    }
}

#[tokio::test]
async fn connection_handler_wrong_password_returns_auth_error() {
    let ctx = test_context();
    let graph = Arc::new(RwLock::new(Graph::new()));
    let (mut writer, mut reader, _shutdown) = spawn_handler(ctx, graph);

    let response = send_recv(
        &mut writer,
        &mut reader,
        &ClientMessage::Login {
            username: "admin".into(),
            password: "WrongPassword1!".into(),
        },
    )
    .await;

    assert!(
        matches!(response, ServerMessage::AuthError { .. }),
        "expected AuthError, got {response:?}"
    );
}

#[tokio::test]
async fn connection_handler_ping_returns_pong() {
    let ctx = test_context();
    let graph = Arc::new(RwLock::new(Graph::new()));
    let (mut writer, mut reader, _shutdown) = spawn_handler(ctx, graph);

    let response = send_recv(&mut writer, &mut reader, &ClientMessage::Ping).await;
    assert_eq!(response, ServerMessage::Pong);
}

#[tokio::test]
async fn connection_handler_logout_invalidates_session() {
    let ctx = test_context();
    let graph = Arc::new(RwLock::new(Graph::new()));
    let (mut writer, mut reader, _shutdown) = spawn_handler(ctx, graph);

    // Login first
    let response = send_recv(
        &mut writer,
        &mut reader,
        &ClientMessage::Login {
            username: "admin".into(),
            password: "Admin@Init1!".into(),
        },
    )
    .await;
    assert!(matches!(response, ServerMessage::AuthOk { .. }));

    // Logout
    let json = serde_json::to_vec(&ClientMessage::Logout).unwrap();
    writer.write_frame(&json).await.unwrap();

    let frame = reader.read_frame().await.unwrap().expect("expected Bye");
    let response: ServerMessage = serde_json::from_slice(&frame).unwrap();
    assert_eq!(response, ServerMessage::Bye);

    // Connection should be closed after logout (EOF)
    let eof = reader.read_frame().await.unwrap();
    assert_eq!(eof, None, "expected EOF after logout");
}

#[tokio::test]
async fn connection_handler_query_returns_result() {
    let ctx = test_context();
    let graph = Arc::new(RwLock::new(Graph::new()));

    {
        let mut g = graph.write().unwrap();
        g.add_node(
            "Person".to_owned(),
            std::iter::once(("name".into(), "Alice".into())).collect(),
        )
        .unwrap();
    }

    let (mut writer, mut reader, _shutdown) = spawn_handler(ctx, graph);

    // Login
    let response = send_recv(
        &mut writer,
        &mut reader,
        &ClientMessage::Login {
            username: "admin".into(),
            password: "Admin@Init1!".into(),
        },
    )
    .await;
    assert!(matches!(response, ServerMessage::AuthOk { .. }));

    // Query
    let response = send_recv(
        &mut writer,
        &mut reader,
        &ClientMessage::Query {
            query: "MATCH (n:Person) RETURN n.name".into(),
            language: "gql".into(),
        },
    )
    .await;

    match response {
        ServerMessage::QueryResult { columns, rows } => {
            assert!(!columns.is_empty(), "columns should not be empty");
            assert!(!rows.is_empty(), "rows should not be empty");
        }
        other => panic!("expected QueryResult, got {other:?}"),
    }
}

// --- Quality fix tests ---

#[tokio::test]
async fn connection_handler_malformed_frame_returns_protocol_error() {
    let ctx = test_context();
    let graph = Arc::new(RwLock::new(Graph::new()));
    let (mut writer, mut reader, _shutdown) = spawn_handler(ctx, graph);

    // Send invalid JSON in a valid frame
    let bad_json = b"{ this is not json }";
    writer.write_frame(bad_json).await.unwrap();

    let frame = reader.read_frame().await.unwrap().expect("expected frame");
    let response: ServerMessage = serde_json::from_slice(&frame).unwrap();
    assert!(
        matches!(response, ServerMessage::ProtocolError { .. }),
        "expected ProtocolError for malformed JSON, got {response:?}"
    );
}

#[tokio::test]
async fn connection_handler_auth_error_reason_is_generic() {
    let ctx = test_context();
    let graph = Arc::new(RwLock::new(Graph::new()));
    let (mut writer, mut reader, _shutdown) = spawn_handler(ctx, graph);

    let response = send_recv(
        &mut writer,
        &mut reader,
        &ClientMessage::Login {
            username: "admin".into(),
            password: "WrongPassword1!".into(),
        },
    )
    .await;

    match response {
        ServerMessage::AuthError { ref reason } => {
            assert_eq!(
                reason, "authentication failed",
                "auth error must not leak internal detail, got: {reason:?}"
            );
        }
        other => panic!("expected AuthError, got {other:?}"),
    }
}

#[tokio::test]
async fn connection_handler_short_password_auth_error_is_generic() {
    let ctx = test_context();
    let graph = Arc::new(RwLock::new(Graph::new()));
    let (mut writer, mut reader, _shutdown) = spawn_handler(ctx, graph);

    let response = send_recv(
        &mut writer,
        &mut reader,
        &ClientMessage::Login {
            username: "admin".into(),
            password: "x".into(),
        },
    )
    .await;

    match response {
        ServerMessage::AuthError { ref reason } => {
            assert_eq!(
                reason, "authentication failed",
                "auth error must not leak password validation detail, got: {reason:?}"
            );
        }
        other => panic!("expected AuthError, got {other:?}"),
    }
}
