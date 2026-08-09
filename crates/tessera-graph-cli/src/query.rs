// SPDX-License-Identifier: BSL-1.1

use tokio::io::{AsyncRead, AsyncWrite};

use tessera_graph_protocol::ProtocolError;
use tessera_graph_protocol::packstream::PackStreamValue;

use crate::connection::Session;
use crate::error::CliError;

/// Result of a successful query execution.
#[derive(Debug)]
pub struct QueryOutput {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<PackStreamValue>>,
}

/// Execute a query against the server via an authenticated session.
///
/// The `_language` parameter is currently unused: the server determines the
/// query language via `tessera-cypher` which auto-detects GQL vs Cypher syntax.
/// It is kept for future parametrised language selection.
///
/// # Errors
///
/// - `CliError::Query` if the server reports a query error.
/// - `CliError::Auth` if the server reports an authentication error.
/// - `CliError::Connection` on I/O or protocol errors.
pub async fn execute_query<R, W>(
    session: &mut Session<R, W>,
    query: &str,
    _language: &str,
) -> Result<QueryOutput, CliError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let result = session.client.run_query(query).await.map_err(|e| match e {
        ProtocolError::BoltQueryFailure { message } => CliError::Query(message),
        ProtocolError::BoltAuthFailure { message } => CliError::Auth(message),
        other => CliError::Connection(other.to_string()),
    })?;

    Ok(QueryOutput {
        columns: result.columns,
        rows: result.rows,
    })
}
