// SPDX-License-Identifier: LicenseRef-TesseraGraph-Proprietary

use std::fmt;

/// CLI error type with associated exit codes.
///
/// Each variant maps to a specific exit code for scripting integration:
/// - `Connection` → 1
/// - `Auth` → 2
/// - `Query` → 3
/// - `ImportExport` → 4
/// - `Config` → 5
#[derive(Debug)]
pub enum CliError {
    Connection(String),
    Auth(String),
    Query(String),
    ImportExport(String),
    Config(String),
}

impl CliError {
    /// Returns the process exit code for this error category.
    #[must_use]
    pub const fn exit_code(&self) -> i32 {
        match self {
            Self::Connection(_) => 1,
            Self::Auth(_) => 2,
            Self::Query(_) => 3,
            Self::ImportExport(_) => 4,
            Self::Config(_) => 5,
        }
    }
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Connection(msg) => write!(f, "Connection error: {msg}"),
            Self::Auth(msg) => write!(f, "Authentication error: {msg}"),
            Self::Query(msg) => write!(f, "Query error: {msg}"),
            Self::ImportExport(msg) => write!(f, "Import/export error: {msg}"),
            Self::Config(msg) => write!(f, "Configuration error: {msg}"),
        }
    }
}

impl std::error::Error for CliError {}

impl From<tessera_graph_protocol::ProtocolError> for CliError {
    fn from(e: tessera_graph_protocol::ProtocolError) -> Self {
        Self::Connection(e.to_string())
    }
}

impl From<std::io::Error> for CliError {
    fn from(e: std::io::Error) -> Self {
        Self::Connection(e.to_string())
    }
}

/// Result alias for CLI operations.
pub type Result<T> = std::result::Result<T, CliError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connection_error_exit_code() {
        let e = CliError::Connection("refused".to_owned());
        assert_eq!(e.exit_code(), 1);
    }

    #[test]
    fn auth_error_exit_code() {
        let e = CliError::Auth("bad password".to_owned());
        assert_eq!(e.exit_code(), 2);
    }

    #[test]
    fn query_error_exit_code() {
        let e = CliError::Query("syntax error".to_owned());
        assert_eq!(e.exit_code(), 3);
    }

    #[test]
    fn import_export_error_exit_code() {
        let e = CliError::ImportExport("file not found".to_owned());
        assert_eq!(e.exit_code(), 4);
    }

    #[test]
    fn config_error_exit_code() {
        let e = CliError::Config("missing host".to_owned());
        assert_eq!(e.exit_code(), 5);
    }

    #[test]
    fn display_contains_message() {
        let e = CliError::Auth("bad password".to_owned());
        let s = e.to_string();
        assert!(s.contains("bad password"));
        assert!(s.contains("Authentication error"));
    }

    #[test]
    fn display_connection_has_prefix() {
        let e = CliError::Connection("timeout".to_owned());
        assert!(e.to_string().starts_with("Connection error:"));
    }

    #[test]
    fn from_protocol_error() {
        let pe = tessera_graph_protocol::ProtocolError::BoltInvalidHandshake { reason: "test" };
        let ce: CliError = pe.into();
        assert_eq!(ce.exit_code(), 1);
        assert!(ce.to_string().contains("Connection error"));
    }

    #[test]
    fn from_io_error() {
        let io_err = std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "refused");
        let ce: CliError = io_err.into();
        assert_eq!(ce.exit_code(), 1);
        assert!(ce.to_string().contains("refused"));
    }
}
