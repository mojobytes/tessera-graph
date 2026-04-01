// Copyright 2026 BelowZero Security OU. All rights reserved.

use clap::{Parser, Subcommand};

/// `TesseraGraph` Enterprise CLI — admin tool for interacting with a running server.
#[derive(Parser, Debug)]
#[command(name = "tessera-cli", version, about)]
pub struct Cli {
    /// Server host address.
    #[arg(short = 'H', long)]
    pub host: Option<String>,

    /// Server port.
    #[arg(short, long)]
    pub port: Option<u16>,

    /// Username for authentication.
    #[arg(short, long)]
    pub username: Option<String>,

    /// Password (prefer `TESSERA_PASSWORD` env var or interactive prompt).
    #[arg(long)]
    pub password: Option<String>,

    /// PEM CA certificate for self-signed certs.
    #[arg(long)]
    pub ca_cert: Option<String>,

    /// Skip TLS certificate verification (dev only).
    #[arg(long, default_value_t = false)]
    pub tls_skip_verify: bool,

    /// Connection timeout in seconds.
    #[arg(long)]
    pub connect_timeout: Option<u64>,

    /// Output format: table, json, csv.
    #[arg(long)]
    pub format: Option<String>,

    /// Subcommand to execute. If omitted, starts REPL.
    #[command(subcommand)]
    pub command: Option<Command>,
}

/// Available subcommands.
#[derive(Subcommand, Debug)]
pub enum Command {
    /// Execute a single query.
    Query(QueryArgs),

    /// Execute queries from a file.
    Exec(ExecArgs),

    /// Import data from a file.
    Import(ImportArgs),

    /// Export graph data.
    Export(ExportArgs),

    /// Health check — exit 0 on success.
    Ping,

    /// Print version information.
    Version,
}

/// Arguments for the `query` subcommand.
#[derive(Parser, Debug)]
pub struct QueryArgs {
    /// The query string to execute.
    pub query: String,

    /// Query language: gql or cypher.
    #[arg(short, long, default_value = "gql")]
    pub language: String,

    /// Omit headers in table/CSV output.
    #[arg(long, default_value_t = false)]
    pub no_headers: bool,
}

/// Arguments for the `exec` subcommand.
#[derive(Parser, Debug)]
pub struct ExecArgs {
    /// Path to the file containing queries.
    pub file: String,

    /// Query language: gql or cypher.
    #[arg(short, long, default_value = "gql")]
    pub language: String,
}

/// Arguments for the `import` subcommand.
#[derive(Parser, Debug)]
pub struct ImportArgs {
    /// Path to the file to import (use `-` for stdin).
    pub file: String,

    /// Import format: csv-nodes, csv-edges, json, gql (inferred from extension if omitted).
    #[arg(long)]
    pub format: Option<String>,

    /// Print generated queries without executing.
    #[arg(long, default_value_t = false)]
    pub dry_run: bool,

    /// Continue importing after statement errors (log errors to stderr).
    #[arg(long, default_value_t = false)]
    pub continue_on_error: bool,
}

/// Arguments for the `export` subcommand.
#[derive(Parser, Debug)]
pub struct ExportArgs {
    /// Export format: gql, json, csv.
    #[arg(long, default_value = "gql")]
    pub format: String,

    /// Write output to a file instead of stdout.
    #[arg(long)]
    pub output: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_no_args_starts_repl() {
        let cli = Cli::try_parse_from(["tessera-cli"]).unwrap();
        assert!(cli.command.is_none());
        assert!(cli.host.is_none());
    }

    #[test]
    fn parse_host_and_port_flags() {
        let cli = Cli::try_parse_from(["tessera-cli", "-H", "db.prod", "-p", "9000"]).unwrap();
        assert_eq!(cli.host.as_deref(), Some("db.prod"));
        assert_eq!(cli.port, Some(9000));
    }

    #[test]
    fn parse_query_subcommand() {
        let cli = Cli::try_parse_from(["tessera-cli", "query", "MATCH (n) RETURN n"]).unwrap();
        let Some(Command::Query(q)) = cli.command else {
            panic!("expected Query command");
        };
        assert_eq!(q.query, "MATCH (n) RETURN n");
        assert_eq!(q.language, "gql");
    }

    #[test]
    fn parse_query_with_language() {
        let cli =
            Cli::try_parse_from(["tessera-cli", "query", "-l", "cypher", "MATCH (n) RETURN n"])
                .unwrap();
        let Some(Command::Query(q)) = cli.command else {
            panic!("expected Query command");
        };
        assert_eq!(q.language, "cypher");
    }

    #[test]
    fn parse_ping_subcommand() {
        let cli = Cli::try_parse_from(["tessera-cli", "ping"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Ping)));
    }

    #[test]
    fn parse_import_subcommand() {
        let cli = Cli::try_parse_from([
            "tessera-cli",
            "import",
            "data.csv",
            "--format",
            "csv-nodes",
            "--dry-run",
        ])
        .unwrap();
        let Some(Command::Import(i)) = cli.command else {
            panic!("expected Import command");
        };
        assert_eq!(i.file, "data.csv");
        assert_eq!(i.format.as_deref(), Some("csv-nodes"));
        assert!(i.dry_run);
        assert!(!i.continue_on_error);
    }

    #[test]
    fn parse_import_continue_on_error_flag() {
        let cli = Cli::try_parse_from([
            "tessera-cli",
            "import",
            "data.json",
            "--continue-on-error",
        ])
        .unwrap(); // OK: test
        let Some(Command::Import(i)) = cli.command else {
            panic!("expected Import command");
        };
        assert!(i.continue_on_error);
    }

    #[test]
    fn parse_export_subcommand() {
        let cli = Cli::try_parse_from([
            "tessera-cli",
            "export",
            "--format",
            "json",
            "--output",
            "out.json",
        ])
        .unwrap();
        let Some(Command::Export(e)) = cli.command else {
            panic!("expected Export command");
        };
        assert_eq!(e.format, "json");
        assert_eq!(e.output.as_deref(), Some("out.json"));
    }

    #[test]
    fn parse_exec_subcommand() {
        let cli = Cli::try_parse_from(["tessera-cli", "exec", "schema.gql"]).unwrap();
        let Some(Command::Exec(e)) = cli.command else {
            panic!("expected Exec command");
        };
        assert_eq!(e.file, "schema.gql");
    }

    #[test]
    fn parse_tls_skip_verify() {
        let cli = Cli::try_parse_from(["tessera-cli", "--tls-skip-verify", "ping"]).unwrap();
        assert!(cli.tls_skip_verify);
    }

    #[test]
    fn parse_version_subcommand() {
        let cli = Cli::try_parse_from(["tessera-cli", "version"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Version)));
    }
}
