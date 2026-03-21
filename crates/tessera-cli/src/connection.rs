// Copyright 2026 BelowZero Security OU. All rights reserved.

use tokio::io::{AsyncRead, AsyncWrite};

use tessera_protocol::{ClientMessage, FramedReader, FramedWriter, ServerMessage};

use crate::error::CliError;

/// A connected session to a `TesseraGraph` server.
///
/// Generic over the read/write halves so that tests can use `tokio::io::duplex`
/// while production uses `TlsStream<TcpStream>` split halves.
pub struct Session<R, W> {
    reader: FramedReader<R>,
    writer: FramedWriter<W>,
    token: Option<String>,
}

impl<R, W> Session<R, W>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    /// Create a session from pre-split read/write halves.
    pub fn from_split(reader: R, writer: W) -> Self {
        Self {
            reader: FramedReader::new(reader),
            writer: FramedWriter::new(writer),
            token: None,
        }
    }

    /// Send a client message to the server.
    ///
    /// # Errors
    ///
    /// Returns `CliError::Connection` on I/O or serialization failure.
    pub async fn send(&mut self, msg: ClientMessage) -> Result<(), CliError> {
        let payload = serde_json::to_vec(&msg)
            .map_err(|e| CliError::Connection(format!("failed to serialize message: {e}")))?;
        self.writer.write_frame(&payload).await?;
        Ok(())
    }

    /// Receive a server message.
    ///
    /// Returns `ServerMessage::Bye` on clean EOF (server closed connection).
    ///
    /// # Errors
    ///
    /// Returns `CliError::Connection` on I/O or deserialization failure.
    pub async fn recv(&mut self) -> Result<ServerMessage, CliError> {
        let frame = self.reader.read_frame().await?;
        match frame {
            Some(payload) => {
                let msg: ServerMessage = serde_json::from_slice(&payload).map_err(|e| {
                    CliError::Connection(format!("invalid server message: {e}"))
                })?;
                Ok(msg)
            }
            None => Ok(ServerMessage::Bye),
        }
    }

    /// Store the authentication token received from `AuthOk`.
    pub fn set_token(&mut self, token: String) {
        self.token = Some(token);
    }

    /// Return the stored authentication token, if any.
    #[must_use]
    pub fn token(&self) -> Option<&str> {
        self.token.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn send_serializes_ping_frame() {
        let (client_half, server_half) = tokio::io::duplex(4096);
        let (cr, cw) = tokio::io::split(client_half);
        let mut session = Session::from_split(cr, cw);

        session.send(ClientMessage::Ping).await.expect("send"); // OK: test

        let (sr, _sw) = tokio::io::split(server_half);
        let mut reader = FramedReader::new(sr);
        let frame = reader.read_frame().await.expect("read").expect("some"); // OK: test
        let msg: ClientMessage = serde_json::from_slice(&frame).expect("deserialize"); // OK: test
        assert_eq!(msg, ClientMessage::Ping);
    }

    #[tokio::test]
    async fn recv_deserializes_pong() {
        let (client_half, server_half) = tokio::io::duplex(4096);
        let (cr, cw) = tokio::io::split(client_half);
        let mut session = Session::from_split(cr, cw);

        let (_sr, sw) = tokio::io::split(server_half);
        let mut writer = FramedWriter::new(sw);
        let payload = serde_json::to_vec(&ServerMessage::Pong).expect("serialize"); // OK: test
        writer.write_frame(&payload).await.expect("write"); // OK: test

        let msg = session.recv().await.expect("recv"); // OK: test
        assert_eq!(msg, ServerMessage::Pong);
    }

    #[tokio::test]
    async fn recv_eof_returns_bye() {
        let (client_half, server_half) = tokio::io::duplex(4096);
        drop(server_half);
        let (cr, cw) = tokio::io::split(client_half);
        let mut session = Session::from_split(cr, cw);

        let msg = session.recv().await.expect("recv on eof"); // OK: test
        assert_eq!(msg, ServerMessage::Bye);
    }

    #[tokio::test]
    async fn send_recv_roundtrip() {
        let (client_half, server_half) = tokio::io::duplex(4096);
        let (cr, cw) = tokio::io::split(client_half);
        let mut client = Session::from_split(cr, cw);

        let (sr, sw) = tokio::io::split(server_half);
        let mut server_reader = FramedReader::new(sr);
        let mut server_writer = FramedWriter::new(sw);

        // Client sends Login
        client
            .send(ClientMessage::Login {
                username: "admin".to_owned(),
                password: "pass".to_owned(),
            })
            .await
            .expect("send login"); // OK: test

        // Server reads it
        let frame = server_reader.read_frame().await.expect("read").expect("some"); // OK: test
        let _msg: ClientMessage = serde_json::from_slice(&frame).expect("deserialize"); // OK: test

        // Server responds with AuthOk
        let resp = serde_json::to_vec(&ServerMessage::AuthOk {
            token: "tok123".to_owned(),
        })
        .expect("serialize"); // OK: test
        server_writer.write_frame(&resp).await.expect("write"); // OK: test

        // Client reads response
        let msg = client.recv().await.expect("recv"); // OK: test
        assert_eq!(
            msg,
            ServerMessage::AuthOk {
                token: "tok123".to_owned()
            }
        );
    }

    #[test]
    fn token_management() {
        let (client_half, _server_half) = tokio::io::duplex(4096);
        let (cr, cw) = tokio::io::split(client_half);
        let mut session = Session::from_split(cr, cw);

        assert!(session.token().is_none());
        session.set_token("tok123".to_owned());
        assert_eq!(session.token(), Some("tok123"));
    }
}
