// Copyright 2026 BelowZero Security OU. All rights reserved.

//! Integration tests for LBAC enforcement in the server query path.

mod common;

use std::collections::BTreeSet;
use std::sync::{Arc, RwLock};

use tessera_auth::lbac::{Clearance, SecurityLabel, SecurityPolicy};
use tessera_graph::{props, Graph};
use tessera_protocol::message::{ClientMessage, ServerMessage};

use common::{send_recv, spawn_handler, test_context};

/// Build a context where `admin` has a given clearance level.
fn test_context_with_clearance(
    level: u16,
    compartments: &[&str],
) -> Arc<tessera_server::context::ServerContext> {
    let ctx = test_context();
    let clearance = Clearance::new(
        level,
        compartments
            .iter()
            .map(|s| (*s).to_string())
            .collect::<BTreeSet<_>>(),
    );
    ctx.user_store()
        .set_clearance("admin", clearance)
        .unwrap();
    ctx
}

/// Build a graph containing one node at the given security level.
fn graph_with_classified_node(level: u16, compartments: &[&str]) -> Arc<RwLock<Graph>> {
    let mut g = Graph::new();
    let label = SecurityLabel::new(
        level,
        compartments
            .iter()
            .map(|s| (*s).to_string())
            .collect::<BTreeSet<_>>(),
    );
    let mut p = props! { "name" => "Secret" };
    SecurityPolicy::inject_label(&mut p, &label);
    g.add_node("Thing", p).unwrap();
    Arc::new(RwLock::new(g))
}

async fn login(
    writer: &mut tessera_protocol::frame::FramedWriter<
        tokio::io::WriteHalf<tokio::io::DuplexStream>,
    >,
    reader: &mut tessera_protocol::frame::FramedReader<
        tokio::io::ReadHalf<tokio::io::DuplexStream>,
    >,
) {
    let response = send_recv(
        writer,
        reader,
        &ClientMessage::Login {
            username: "admin".into(),
            password: "Admin@Init1!".into(),
        },
    )
    .await;
    assert!(
        matches!(response, ServerMessage::AuthOk { .. }),
        "login failed: {response:?}"
    );
}

// --- Read path: classified node is hidden from under-cleared user ---

#[tokio::test]
async fn query_read_hides_classified_node_from_under_cleared_user() {
    let ctx = test_context_with_clearance(0, &[]);
    let graph = graph_with_classified_node(5, &[]);
    let (mut writer, mut reader, _shutdown) = spawn_handler(ctx, graph);
    login(&mut writer, &mut reader).await;

    let response = send_recv(
        &mut writer,
        &mut reader,
        &ClientMessage::Query {
            query: "MATCH (n:Thing) RETURN n.name".into(),
            language: "gql".into(),
        },
    )
    .await;

    match response {
        ServerMessage::QueryResult { rows, .. } => {
            assert!(
                rows.is_empty(),
                "under-cleared user must see 0 rows, got {rows:?}"
            );
        }
        other => panic!("expected QueryResult, got {other:?}"),
    }
}

#[tokio::test]
async fn query_read_shows_node_to_fully_cleared_user() {
    let ctx = test_context_with_clearance(10, &[]);
    let graph = graph_with_classified_node(5, &[]);
    let (mut writer, mut reader, _shutdown) = spawn_handler(ctx, graph);
    login(&mut writer, &mut reader).await;

    let response = send_recv(
        &mut writer,
        &mut reader,
        &ClientMessage::Query {
            query: "MATCH (n:Thing) RETURN n.name".into(),
            language: "gql".into(),
        },
    )
    .await;

    match response {
        ServerMessage::QueryResult { rows, .. } => {
            assert!(!rows.is_empty(), "fully-cleared user must see the node");
        }
        other => panic!("expected QueryResult, got {other:?}"),
    }
}

#[tokio::test]
async fn query_read_hides_compartmented_node_when_compartment_missing() {
    let ctx = test_context_with_clearance(5, &["FINANCE"]);
    let graph = graph_with_classified_node(1, &["SECRET_LEGAL"]);
    let (mut writer, mut reader, _shutdown) = spawn_handler(ctx, graph);
    login(&mut writer, &mut reader).await;

    let response = send_recv(
        &mut writer,
        &mut reader,
        &ClientMessage::Query {
            query: "MATCH (n:Thing) RETURN n.name".into(),
            language: "gql".into(),
        },
    )
    .await;

    match response {
        ServerMessage::QueryResult { rows, .. } => {
            assert!(rows.is_empty(), "compartment mismatch must hide node");
        }
        other => panic!("expected QueryResult, got {other:?}"),
    }
}

#[tokio::test]
async fn query_result_never_contains_security_properties() {
    let ctx = test_context_with_clearance(10, &[]);
    let graph = graph_with_classified_node(5, &[]);
    let (mut writer, mut reader, _shutdown) = spawn_handler(ctx, graph);
    login(&mut writer, &mut reader).await;

    let response = send_recv(
        &mut writer,
        &mut reader,
        &ClientMessage::Query {
            query: "MATCH (n:Thing) RETURN n.name".into(),
            language: "gql".into(),
        },
    )
    .await;

    if let ServerMessage::QueryResult { rows, .. } = response {
        for row in &rows {
            for val in row {
                let serialized = serde_json::to_string(val).unwrap();
                assert!(
                    !serialized.contains(SecurityPolicy::LEVEL_KEY),
                    "security level key leaked: {serialized}"
                );
                assert!(
                    !serialized.contains(SecurityPolicy::COMPARTMENTS_KEY),
                    "security compartments key leaked: {serialized}"
                );
            }
        }
    }
}

// --- Mutation path ---

#[tokio::test]
async fn mutation_creates_node_for_cleared_user() {
    let ctx = test_context_with_clearance(0, &[]);
    let graph = Arc::new(RwLock::new(Graph::new()));
    let (mut writer, mut reader, _shutdown) = spawn_handler(ctx, graph);
    login(&mut writer, &mut reader).await;

    let response = send_recv(
        &mut writer,
        &mut reader,
        &ClientMessage::Query {
            query: "CREATE (n:TestNode {name: 'public'})".into(),
            language: "gql".into(),
        },
    )
    .await;

    assert!(
        matches!(response, ServerMessage::QueryResult { .. }),
        "public CREATE must succeed: {response:?}"
    );
}

#[tokio::test]
async fn mutation_through_secure_graph_is_visible_on_subsequent_read() {
    let ctx = test_context_with_clearance(5, &[]);
    let graph = Arc::new(RwLock::new(Graph::new()));
    let (mut writer, mut reader, _shutdown) = spawn_handler(ctx, Arc::clone(&graph));
    login(&mut writer, &mut reader).await;

    let create_resp = send_recv(
        &mut writer,
        &mut reader,
        &ClientMessage::Query {
            query: "CREATE (n:Marker {tag: 'wired'})".into(),
            language: "gql".into(),
        },
    )
    .await;
    assert!(matches!(create_resp, ServerMessage::QueryResult { .. }));

    let read_resp = send_recv(
        &mut writer,
        &mut reader,
        &ClientMessage::Query {
            query: "MATCH (n:Marker) RETURN n.tag".into(),
            language: "gql".into(),
        },
    )
    .await;
    match read_resp {
        ServerMessage::QueryResult { rows, .. } => {
            assert!(!rows.is_empty(), "level-5 user must see created node");
        }
        other => panic!("expected QueryResult, got {other:?}"),
    }
}
