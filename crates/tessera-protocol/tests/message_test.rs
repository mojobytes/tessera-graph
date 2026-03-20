// Copyright 2026 BelowZero Security OU. All rights reserved.

use tessera_protocol::message::{ClientMessage, ServerMessage};

#[test]
fn client_message_login_roundtrip() {
    let msg = ClientMessage::Login {
        username: "admin".into(),
        password: "pw".into(),
    };
    let json = serde_json::to_string(&msg).unwrap();
    let decoded: ClientMessage = serde_json::from_str(&json).unwrap();
    assert_eq!(decoded, msg);
}

#[test]
fn client_message_query_roundtrip() {
    let msg = ClientMessage::Query {
        query: "MATCH (n) RETURN n".into(),
        language: "gql".into(),
    };
    let json = serde_json::to_string(&msg).unwrap();
    let decoded: ClientMessage = serde_json::from_str(&json).unwrap();
    assert_eq!(decoded, msg);
}

#[test]
fn client_message_logout_roundtrip() {
    let msg = ClientMessage::Logout;
    let json = serde_json::to_string(&msg).unwrap();
    let decoded: ClientMessage = serde_json::from_str(&json).unwrap();
    assert_eq!(decoded, msg);
}

#[test]
fn client_message_ping_roundtrip() {
    let msg = ClientMessage::Ping;
    let json = serde_json::to_string(&msg).unwrap();
    let decoded: ClientMessage = serde_json::from_str(&json).unwrap();
    assert_eq!(decoded, msg);
}

#[test]
fn server_message_auth_ok_roundtrip() {
    let msg = ServerMessage::AuthOk {
        token: "tok123".into(),
    };
    let json = serde_json::to_string(&msg).unwrap();
    let decoded: ServerMessage = serde_json::from_str(&json).unwrap();
    assert_eq!(decoded, msg);
}

#[test]
fn server_message_query_result_roundtrip() {
    let msg = ServerMessage::QueryResult {
        columns: vec!["n".into()],
        rows: vec![vec![serde_json::json!({"label": "Person"})]],
    };
    let json = serde_json::to_string(&msg).unwrap();
    let decoded: ServerMessage = serde_json::from_str(&json).unwrap();
    assert_eq!(decoded, msg);
}

#[test]
fn server_message_pong_roundtrip() {
    let msg = ServerMessage::Pong;
    let json = serde_json::to_string(&msg).unwrap();
    let decoded: ServerMessage = serde_json::from_str(&json).unwrap();
    assert_eq!(decoded, msg);
}

#[test]
fn server_message_bye_roundtrip() {
    let msg = ServerMessage::Bye;
    let json = serde_json::to_string(&msg).unwrap();
    let decoded: ServerMessage = serde_json::from_str(&json).unwrap();
    assert_eq!(decoded, msg);
}

#[test]
fn unknown_message_type_returns_err() {
    let result = serde_json::from_str::<ClientMessage>(r#"{"type":"unknown"}"#);
    assert!(result.is_err(), "unknown type should fail to deserialize");
}

#[test]
fn client_message_login_json_has_type_tag() {
    let msg = ClientMessage::Login {
        username: "u".into(),
        password: "p".into(),
    };
    let json = serde_json::to_string(&msg).unwrap();
    assert!(json.contains(r#""type":"login""#));
}

#[test]
fn client_message_login_debug_redacts_password() {
    let msg = ClientMessage::Login {
        username: "admin".into(),
        password: "super-secret-123".into(),
    };
    let debug = format!("{msg:?}");
    assert!(
        !debug.contains("super-secret-123"),
        "Debug output must not contain the password: {debug}"
    );
    assert!(
        debug.contains("[REDACTED]"),
        "Debug output must show a redaction marker: {debug}"
    );
}

#[test]
fn client_message_query_debug_contains_query_text() {
    let msg = ClientMessage::Query {
        query: "MATCH (n) RETURN n".into(),
        language: "gql".into(),
    };
    let debug = format!("{msg:?}");
    assert!(
        debug.contains("MATCH"),
        "Query debug should contain query text: {debug}"
    );
}

#[test]
fn client_message_ping_debug() {
    let debug = format!("{:?}", ClientMessage::Ping);
    assert_eq!(debug, "Ping");
}

#[test]
fn server_message_protocol_error_roundtrip() {
    let msg = ServerMessage::ProtocolError {
        reason: "bad format".into(),
    };
    let json = serde_json::to_string(&msg).unwrap();
    let decoded: ServerMessage = serde_json::from_str(&json).unwrap();
    assert_eq!(decoded, msg);
}

#[test]
fn server_message_capacity_error_roundtrip() {
    let msg = ServerMessage::CapacityError {
        reason: "server at capacity".into(),
    };
    let json = serde_json::to_string(&msg).unwrap();
    let decoded: ServerMessage = serde_json::from_str(&json).unwrap();
    assert_eq!(decoded, msg);
}
