// Copyright 2026 BelowZero Security OU. All rights reserved.

use tokio::io::{AsyncRead, AsyncWrite};

use tessera_protocol::{ClientMessage, ServerMessage};

use crate::connection::Session;
use crate::error::CliError;

/// Result of a successful query execution.
#[derive(Debug)]
pub struct QueryOutput {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<serde_json::Value>>,
}

/// Execute a query against the server via an authenticated session.
///
/// # Errors
///
/// - `CliError::Auth` if the server reports an auth/session error.
/// - `CliError::Query` if the server reports a query error.
/// - `CliError::Connection` on unexpected responses or I/O failure.
pub async fn execute_query<R, W>(
    session: &mut Session<R, W>,
    query: &str,
    language: &str,
) -> Result<QueryOutput, CliError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    session
        .send(ClientMessage::Query {
            query: query.to_owned(),
            language: language.to_owned(),
        })
        .await?;

    match session.recv().await? {
        ServerMessage::QueryResult { columns, rows } => Ok(QueryOutput { columns, rows }),
        ServerMessage::QueryError { reason } => Err(CliError::Query(reason)),
        ServerMessage::AuthError { reason } => Err(CliError::Auth(reason)),
        ServerMessage::Bye => Err(CliError::Connection("server closed connection".to_owned())),
        other => Err(CliError::Connection(format!(
            "unexpected server response during query: {other:?}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tessera_protocol::{FramedReader, FramedWriter};

    fn mock_session_with_response(
        response: ServerMessage,
    ) -> Session<
        tokio::io::ReadHalf<tokio::io::DuplexStream>,
        tokio::io::WriteHalf<tokio::io::DuplexStream>,
    > {
        let (client_half, server_half) = tokio::io::duplex(4096);
        let (sr, sw) = tokio::io::split(server_half);

        tokio::spawn(async move {
            let mut reader = FramedReader::new(sr);
            let mut writer = FramedWriter::new(sw);
            let _frame = reader.read_frame().await;
            let payload = serde_json::to_vec(&response).expect("serialize"); // OK: test
            let _ = writer.write_frame(&payload).await;
        });

        let (cr, cw) = tokio::io::split(client_half);
        Session::from_split(cr, cw)
    }

    #[tokio::test]
    async fn query_result_returns_output() {
        let mut session = mock_session_with_response(ServerMessage::QueryResult {
            columns: vec!["name".to_owned()],
            rows: vec![vec![serde_json::json!("Alice")]],
        });

        let output = execute_query(&mut session, "MATCH (n) RETURN n.name", "gql")
            .await
            .expect("query ok"); // OK: test
        assert_eq!(output.columns, vec!["name"]);
        assert_eq!(output.rows.len(), 1);
        assert_eq!(output.rows[0][0], serde_json::json!("Alice"));
    }

    #[tokio::test]
    async fn query_error_returns_cli_error() {
        let mut session = mock_session_with_response(ServerMessage::QueryError {
            reason: "syntax error at position 5".to_owned(),
        });

        let err = execute_query(&mut session, "INVALID QUERY", "gql")
            .await
            .expect_err("should fail"); // OK: test
        assert!(matches!(err, CliError::Query(_)));
        assert!(err.to_string().contains("syntax error"));
    }

    #[tokio::test]
    async fn auth_error_on_expired_token() {
        let mut session = mock_session_with_response(ServerMessage::AuthError {
            reason: "session expired".to_owned(),
        });

        let err = execute_query(&mut session, "MATCH (n) RETURN n", "gql")
            .await
            .expect_err("should fail"); // OK: test
        assert!(matches!(err, CliError::Auth(_)));
        assert!(err.to_string().contains("session expired"));
    }

    #[tokio::test]
    async fn bye_returns_connection_error() {
        let mut session = mock_session_with_response(ServerMessage::Bye);

        let err = execute_query(&mut session, "MATCH (n) RETURN n", "gql")
            .await
            .expect_err("should fail"); // OK: test
        assert!(matches!(err, CliError::Connection(_)));
        assert!(err.to_string().contains("server closed"));
    }

    #[tokio::test]
    async fn unexpected_response() {
        let mut session = mock_session_with_response(ServerMessage::Pong);

        let err = execute_query(&mut session, "MATCH (n) RETURN n", "gql")
            .await
            .expect_err("should fail"); // OK: test
        assert!(matches!(err, CliError::Connection(_)));
        assert!(err.to_string().contains("unexpected"));
    }

    #[tokio::test]
    async fn empty_result_set() {
        let mut session = mock_session_with_response(ServerMessage::QueryResult {
            columns: vec!["n".to_owned()],
            rows: vec![],
        });

        let output = execute_query(&mut session, "MATCH (n:Nonexistent) RETURN n", "gql")
            .await
            .expect("query ok"); // OK: test
        assert_eq!(output.columns, vec!["n"]);
        assert!(output.rows.is_empty());
    }
}
