// Copyright 2026 BelowZero Security OU. All rights reserved.

use tokio::io::{AsyncRead, AsyncWrite};

use tessera_protocol::{ClientMessage, ServerMessage};

use crate::connection::Session;
use crate::error::CliError;

/// Perform the login handshake with the server.
///
/// Sends a `Login` message and waits for `AuthOk` (storing the token) or `AuthError`.
///
/// # Errors
///
/// - `CliError::Auth` if the server responds with `AuthError`.
/// - `CliError::Connection` if the server responds with an unexpected message or I/O fails.
pub async fn login<R, W>(
    session: &mut Session<R, W>,
    username: &str,
    password: &str,
) -> Result<(), CliError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    session
        .send(ClientMessage::Login {
            username: username.to_owned(),
            password: password.to_owned(),
        })
        .await?;

    match session.recv().await? {
        ServerMessage::AuthOk { token } => {
            session.set_token(token);
            Ok(())
        }
        ServerMessage::AuthError { reason } => Err(CliError::Auth(reason)),
        other => Err(CliError::Connection(format!(
            "unexpected server response during login: {other:?}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tessera_protocol::{FramedReader, FramedWriter};

    /// Create a duplex where the server reads one message, then writes `response`.
    /// Returns the client-side `Session`.
    fn mock_session_with_response(
        response: ServerMessage,
    ) -> Session<
        tokio::io::ReadHalf<tokio::io::DuplexStream>,
        tokio::io::WriteHalf<tokio::io::DuplexStream>,
    > {
        let (client_half, server_half) = tokio::io::duplex(4096);
        let (sr, sw) = tokio::io::split(server_half);

        // Spawn a task that reads one message from the client, then responds.
        tokio::spawn(async move {
            let mut reader = FramedReader::new(sr);
            let mut writer = FramedWriter::new(sw);
            // Read client's Login message (consume it)
            let _frame = reader.read_frame().await;
            // Respond with the preconfigured response
            let payload = serde_json::to_vec(&response).expect("serialize"); // OK: test
            let _ = writer.write_frame(&payload).await;
            // Drop both to close the connection
        });

        let (cr, cw) = tokio::io::split(client_half);
        Session::from_split(cr, cw)
    }

    #[tokio::test]
    async fn auth_ok_stores_token() {
        let mut session = mock_session_with_response(ServerMessage::AuthOk {
            token: "tok123".to_owned(),
        });
        login(&mut session, "admin", "pass")
            .await
            .expect("login ok"); // OK: test
        assert_eq!(session.token(), Some("tok123"));
    }

    #[tokio::test]
    async fn auth_error_returns_cli_error() {
        let mut session = mock_session_with_response(ServerMessage::AuthError {
            reason: "invalid credentials".to_owned(),
        });
        let err = login(&mut session, "admin", "wrong")
            .await
            .expect_err("should fail"); // OK: test
        assert!(matches!(err, CliError::Auth(_)));
        assert!(err.to_string().contains("invalid credentials"));
    }

    #[tokio::test]
    async fn unexpected_message_returns_connection_error() {
        let mut session = mock_session_with_response(ServerMessage::Pong);
        let err = login(&mut session, "admin", "pass")
            .await
            .expect_err("should fail"); // OK: test
        assert!(matches!(err, CliError::Connection(_)));
        assert!(err.to_string().contains("unexpected"));
    }

    #[tokio::test]
    async fn capacity_error_returns_connection_error() {
        let mut session = mock_session_with_response(ServerMessage::CapacityError {
            reason: "too many connections".to_owned(),
        });
        let err = login(&mut session, "admin", "pass")
            .await
            .expect_err("should fail"); // OK: test
        assert!(matches!(err, CliError::Connection(_)));
    }
}
