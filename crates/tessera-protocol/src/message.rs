// Copyright 2026 BelowZero Security OU. All rights reserved.

//! Client and server message types for the `TesseraGraph` wire protocol.
//!
//! Messages are serialized as JSON over the framed transport layer.

use serde::{Deserialize, Serialize};

/// Messages sent from the client to the server.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    /// Authenticate with username and password.
    Login { username: String, password: String },
    /// Execute a GQL or Cypher query.
    Query { query: String, language: String },
    /// End the current session.
    Logout,
    /// Liveness check (no authentication required).
    Ping,
}

// Manual Debug impl to redact the password field in Login messages.
impl std::fmt::Debug for ClientMessage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Login { username, .. } => f
                .debug_struct("Login")
                .field("username", username)
                .field("password", &"[REDACTED]")
                .finish(),
            Self::Query { query, language } => f
                .debug_struct("Query")
                .field("query", query)
                .field("language", language)
                .finish(),
            Self::Logout => write!(f, "Logout"),
            Self::Ping => write!(f, "Ping"),
        }
    }
}

/// Messages sent from the server to the client.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
#[allow(clippy::derive_partial_eq_without_eq)] // serde_json::Value does not impl Eq
pub enum ServerMessage {
    /// Authentication succeeded.
    AuthOk { token: String },
    /// Authentication or authorization failed.
    AuthError { reason: String },
    /// Query execution succeeded.
    QueryResult {
        columns: Vec<String>,
        rows: Vec<Vec<serde_json::Value>>,
    },
    /// Query execution failed.
    QueryError { reason: String },
    /// Protocol-level error (malformed frame, unknown message type, etc.).
    ProtocolError { reason: String },
    /// Server at maximum connection capacity.
    CapacityError { reason: String },
    /// Response to a Ping.
    Pong,
    /// Session ended or server shutting down.
    Bye,
}
