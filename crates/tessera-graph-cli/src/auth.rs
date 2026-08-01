// SPDX-License-Identifier: LicenseRef-TesseraGraph-Proprietary

use tokio::io::{AsyncRead, AsyncWrite};

use tessera_graph_protocol::ProtocolError;

use crate::connection::Session;
use crate::error::CliError;

/// Authenticate with the server via Bolt HELLO.
///
/// Sends HELLO with `principal`, `credentials`, and optionally `db`.
///
/// # Errors
///
/// - `CliError::Auth` if the server responds with FAILURE (authentication error).
/// - `CliError::Connection` on I/O or protocol errors.
pub async fn login<R, W>(
    session: &mut Session<R, W>,
    username: &str,
    password: &str,
    db: Option<&str>,
) -> Result<(), CliError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    session
        .client
        .hello(username, password, db)
        .await
        .map_err(|e| match e {
            ProtocolError::BoltAuthFailure { message } => CliError::Auth(message),
            other => CliError::Connection(other.to_string()),
        })
}
