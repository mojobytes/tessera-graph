// Copyright 2026 BelowZero Security OU. All rights reserved.

//! Client and server message types for the `TesseraGraph` wire protocol.
//!
//! Messages are serialized as JSON over the framed transport layer.

use serde::{Deserialize, Serialize};

/// Messages sent from the client to the server.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
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
    /// Response to a Ping.
    Pong,
    /// Session ended or server shutting down.
    Bye,
}
