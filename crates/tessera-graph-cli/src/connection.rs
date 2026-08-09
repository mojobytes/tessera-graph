// SPDX-License-Identifier: BSL-1.1

use tokio::io::{AsyncRead, AsyncWrite};

use tessera_graph_protocol::BoltClient;

/// A connected session to a `TesseraGraph` server over the Bolt 4.4 protocol.
///
/// Generic over the read/write halves so that tests can use `tokio::io::duplex`
/// while production uses `TlsStream<TcpStream>` split halves.
pub struct Session<R: AsyncRead + Unpin, W: AsyncWrite + Unpin> {
    /// The underlying Bolt client. Public so `main.rs` can call `goodbye()`.
    pub client: BoltClient<R, W>,
}

impl<R, W> Session<R, W>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    /// Create a session from a connected `BoltClient`.
    pub const fn from_client(client: BoltClient<R, W>) -> Self {
        Self { client }
    }
}
