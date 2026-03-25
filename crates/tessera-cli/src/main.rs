// Copyright 2026 BelowZero Security OU. All rights reserved.

use std::net::ToSocketAddrs;
use std::sync::Arc;
use std::time::Instant;

use clap::Parser;
use tokio::io::{AsyncRead, AsyncWrite, split};
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_rustls::TlsConnector;

use tessera_cli_lib::auth;
use tessera_cli_lib::cli::{Cli, Command};
use tessera_cli_lib::config::ConnectionConfig;
use tessera_cli_lib::connection::Session;
use tessera_cli_lib::error::CliError;
use tessera_cli_lib::export;
use tessera_cli_lib::import::{self, ImportPlan};
use tessera_cli_lib::output::OutputFormat;
use tessera_cli_lib::query;
use tessera_cli_lib::repl::{self, MetaCommand, QueryAccumulator};

use tessera_protocol::BoltClient;

#[tokio::main]
async fn main() {
    let exit_code = match run().await {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("Error: {e}");
            e.exit_code()
        }
    };
    std::process::exit(exit_code);
}

async fn run() -> Result<(), CliError> {
    let cli = Cli::parse();
    let (config, password) = ConnectionConfig::resolve_full(&cli);

    // Validate output format early (fail-fast before connecting)
    let _: OutputFormat = config.format.parse()?;

    // Version subcommand — no connection needed
    if matches!(cli.command, Some(Command::Version)) {
        println!("tessera-cli {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    // Establish TLS connection
    let tls_config = build_tls_config(&config)?;
    let connector = TlsConnector::from(Arc::new(tls_config));

    let addr = format!("{}:{}", config.host, config.port);
    let socket_addr = addr
        .to_socket_addrs()
        .map_err(|e| CliError::Connection(format!("cannot resolve {addr}: {e}")))?
        .next()
        .ok_or_else(|| CliError::Connection(format!("no addresses found for {addr}")))?;

    let tcp = timeout(
        std::time::Duration::from_secs(config.connect_timeout_secs),
        TcpStream::connect(socket_addr),
    )
    .await
    .map_err(|_| CliError::Connection(format!("connection to {addr} timed out")))?
    .map_err(|e| CliError::Connection(format!("cannot connect to {addr}: {e}")))?;

    let server_name = rustls::pki_types::ServerName::try_from(config.host.clone())
        .map_err(|e| CliError::Connection(format!("invalid server name '{}': {e}", config.host)))?;

    let tls_stream = connector
        .connect(server_name, tcp)
        .await
        .map_err(|e| CliError::Connection(format!("TLS handshake failed: {e}")))?;

    let (reader, writer) = split(tls_stream);

    // Bolt 4.4 handshake
    let client = BoltClient::connect_split(reader, writer)
        .await
        .map_err(|e| CliError::Connection(format!("Bolt handshake failed: {e}")))?;

    let mut session = Session::from_client(client);

    // Ping subcommand — performs HELLO with credentials to verify connectivity
    if matches!(cli.command, Some(Command::Ping)) {
        let password = password.unwrap_or_default();
        auth::login(&mut session, &config.username, &password, None).await?;
        println!("OK");
        let _ = session.client.goodbye().await;
        return Ok(());
    }

    // Authenticate
    let password = password.unwrap_or_else(|| {
        rpassword::prompt_password("Password: ").unwrap_or_default() // OK: fallback to empty if terminal fails
    });
    auth::login(&mut session, &config.username, &password, None).await?;

    // Dispatch command
    dispatch_command(cli.command, &mut session, &config).await?;

    // Graceful disconnect
    let _ = session.client.goodbye().await;
    Ok(())
}

async fn dispatch_command<R, W>(
    command: Option<Command>,
    session: &mut Session<R, W>,
    config: &ConnectionConfig,
) -> Result<(), CliError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    match command {
        Some(Command::Query(args)) => {
            let format: OutputFormat = config.format.parse()?;
            let start = Instant::now();
            let output = query::execute_query(session, &args.query, &args.language).await?;
            let elapsed = start.elapsed();
            let rendered = tessera_cli_lib::output::render(
                format,
                &output.columns,
                &output.rows,
                Some(elapsed),
                !args.no_headers,
            )?;
            print!("{rendered}");
        }
        Some(Command::Exec(args)) => handle_exec(session, &args).await?,
        Some(Command::Import(args)) => handle_import(session, &args).await?,
        Some(Command::Export(args)) => handle_export(session, &args).await?,
        Some(Command::Ping | Command::Version) => unreachable!(),
        None => run_repl(session, config).await?,
    }
    Ok(())
}

async fn handle_exec<R, W>(
    session: &mut Session<R, W>,
    args: &tessera_cli_lib::cli::ExecArgs,
) -> Result<(), CliError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let content = std::fs::read_to_string(&args.file)
        .map_err(|e| CliError::ImportExport(format!("cannot read {}: {e}", args.file)))?;
    let plan = ImportPlan::from_gql_content(&content, 100)?;
    for stmt in plan.statements() {
        let output = query::execute_query(session, stmt, &args.language).await?;
        let rendered = tessera_cli_lib::output::render(
            OutputFormat::Table,
            &output.columns,
            &output.rows,
            None,
            true,
        )?;
        println!("{rendered}");
    }
    Ok(())
}

async fn handle_import<R, W>(
    session: &mut Session<R, W>,
    args: &tessera_cli_lib::cli::ImportArgs,
) -> Result<(), CliError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let content = if args.file == "-" {
        use std::io::Read;
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .map_err(|e| CliError::ImportExport(format!("cannot read stdin: {e}")))?;
        buf
    } else {
        std::fs::read_to_string(&args.file)
            .map_err(|e| CliError::ImportExport(format!("cannot read {}: {e}", args.file)))?
    };

    let fmt = args
        .format
        .as_deref()
        .unwrap_or_else(|| infer_import_format(&args.file));

    let statements = match fmt {
        "gql" => import::split_gql_statements(&content),
        "csv-nodes" => import::csv_nodes_to_gql(&content)?,
        other => {
            return Err(CliError::ImportExport(format!(
                "unsupported import format: {other}"
            )));
        }
    };

    if args.dry_run {
        let plan = ImportPlan::new(statements, 100);
        print!("{}", plan.dry_run_summary());
    } else {
        let total = statements.len();
        for (i, stmt) in statements.iter().enumerate() {
            query::execute_query(session, stmt, "gql").await?;
            if (i + 1) % 100 == 0 || i + 1 == total {
                eprintln!("Imported {}/{total} statements", i + 1);
            }
        }
    }
    Ok(())
}

async fn handle_export<R, W>(
    session: &mut Session<R, W>,
    args: &tessera_cli_lib::cli::ExportArgs,
) -> Result<(), CliError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let output = query::execute_query(session, "MATCH (n) RETURN n", "gql").await?;
    let rendered = export::format_export(&args.format, &output.columns, &output.rows)?;
    if let Some(path) = &args.output {
        std::fs::write(path, &rendered)
            .map_err(|e| CliError::ImportExport(format!("cannot write {path}: {e}")))?;
        eprintln!("Exported to {path}");
    } else {
        print!("{rendered}");
    }
    Ok(())
}

async fn run_repl<R, W>(session: &mut Session<R, W>, config: &ConnectionConfig) -> Result<(), CliError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut rl = rustyline::DefaultEditor::new()
        .map_err(|e| CliError::Config(format!("cannot initialize readline: {e}")))?;

    // Load history
    let history_path = dirs_history_path();
    if let Some(ref path) = history_path {
        let _ = rl.load_history(path);
    }

    let mut accumulator = QueryAccumulator::new();
    let mut format: OutputFormat = config.format.parse()?;
    let mut language = config.language.clone();
    let mut show_timing = false;

    println!(
        "tessera-cli {} — Connected to {}:{}",
        env!("CARGO_PKG_VERSION"),
        config.host,
        config.port,
    );
    println!("Type \\h for help, \\q to quit.\n");

    loop {
        let prompt = repl::format_prompt(&config.username, &config.host, config.port, accumulator.is_pending());
        let line = match rl.readline(&prompt) {
            Ok(line) => line,
            Err(rustyline::error::ReadlineError::Interrupted | rustyline::error::ReadlineError::Eof) => {
                break;
            }
            Err(e) => {
                eprintln!("Error: {e}");
                break;
            }
        };

        let trimmed = line.trim();

        // Check for meta-commands
        if let Some(cmd) = repl::parse_meta_command(trimmed) {
            match cmd {
                MetaCommand::Quit => break,
                MetaCommand::Help => print_help(),
                MetaCommand::SetFormat(f) => {
                    format = f;
                    eprintln!("Output format: {format}");
                }
                MetaCommand::SetLanguage(l) => {
                    language.clone_from(&l);
                    eprintln!("Language: {l}");
                }
                MetaCommand::SetTiming(on) => {
                    show_timing = on;
                    eprintln!("Timing: {}", if on { "on" } else { "off" });
                }
                MetaCommand::Clear => {
                    let _ = rl.clear_screen();
                }
                MetaCommand::Unknown(msg) => {
                    eprintln!("{msg}");
                }
            }
            continue;
        }

        // Accumulate query lines
        if let Some(completed_query) = accumulator.push(trimmed) {
            let _ = rl.add_history_entry(&completed_query);

            let start = Instant::now();
            match query::execute_query(session, &completed_query, &language).await {
                Ok(output) => {
                    let elapsed = if show_timing { Some(start.elapsed()) } else { None };
                    match tessera_cli_lib::output::render(format, &output.columns, &output.rows, elapsed, true) {
                        Ok(rendered) => print!("{rendered}"),
                        Err(e) => eprintln!("{e}"),
                    }
                }
                Err(CliError::Auth(reason)) => {
                    eprintln!("Session expired: {reason}");
                    break;
                }
                Err(e) => eprintln!("{e}"),
            }
        }
    }

    // Save history
    if let Some(ref path) = history_path {
        let _ = rl.save_history(path);
    }

    Ok(())
}

fn print_help() {
    eprintln!("Meta-commands:");
    eprintln!("  \\q              Quit");
    eprintln!("  \\h or \\?        Help");
    eprintln!("  \\format <fmt>   Change output format (table, json, csv)");
    eprintln!("  \\l <lang>       Change query language (gql, cypher)");
    eprintln!("  \\timing on|off  Show query execution time");
    eprintln!("  \\clear          Clear screen");
    eprintln!();
    eprintln!("End a query with ; or press Enter on an empty line to execute.");
}

fn build_tls_config(config: &ConnectionConfig) -> Result<rustls::ClientConfig, CliError> {
    if config.tls_skip_verify {
        eprintln!("WARNING: TLS certificate verification is disabled. Do NOT use in production.");
        let cfg = rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoCertVerifier))
            .with_no_client_auth();
        return Ok(cfg);
    }

    let mut root_store = rustls::RootCertStore::empty();

    if let Some(ca_path) = &config.ca_cert {
        let pem_data = std::fs::read(ca_path)
            .map_err(|e| CliError::Config(format!("cannot read CA cert {ca_path}: {e}")))?;
        let certs = rustls_pemfile::certs(&mut &pem_data[..])
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| CliError::Config(format!("invalid PEM in {ca_path}: {e}")))?;
        for cert in certs {
            root_store
                .add(cert)
                .map_err(|e| CliError::Config(format!("cannot add CA cert: {e}")))?;
        }
    } else {
        let native_certs = rustls_native_certs::load_native_certs();
        for err in &native_certs.errors {
            eprintln!("Warning: failed to load native certificate: {err}");
        }
        for cert in native_certs.certs {
            let _ = root_store.add(cert);
        }
    }

    let cfg = rustls::ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_no_client_auth();

    Ok(cfg)
}

/// No-op certificate verifier for `--tls-skip-verify` (dev only).
#[derive(Debug)]
struct NoCertVerifier;

impl rustls::client::danger::ServerCertVerifier for NoCertVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::aws_lc_rs::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// Path to the history file: `~/.tessera_history`.
fn dirs_history_path() -> Option<std::path::PathBuf> {
    tessera_cli_lib::config::home_dir().map(|h| h.join(".tessera_history"))
}

/// Infer import format from file extension.
fn infer_import_format(file: &str) -> &str {
    let path = std::path::Path::new(file);
    match path.extension().and_then(|e| e.to_str()) {
        Some(ext) if ext.eq_ignore_ascii_case("gql") || ext.eq_ignore_ascii_case("cypher") => "gql",
        Some(ext) if ext.eq_ignore_ascii_case("csv") => "csv-nodes",
        _ => "gql",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn infer_format_gql_extension() {
        assert_eq!(infer_import_format("schema.gql"), "gql");
        assert_eq!(infer_import_format("schema.GQL"), "gql");
    }

    #[test]
    fn infer_format_cypher_extension() {
        assert_eq!(infer_import_format("data.cypher"), "gql");
    }

    #[test]
    fn infer_format_csv_extension() {
        assert_eq!(infer_import_format("nodes.csv"), "csv-nodes");
    }

    #[test]
    fn infer_format_json_falls_back_to_gql() {
        assert_ne!(infer_import_format("data.json"), "json");
        assert_eq!(infer_import_format("data.json"), "gql");
    }

    #[test]
    fn infer_format_unknown_defaults_to_gql() {
        assert_eq!(infer_import_format("data.txt"), "gql");
        assert_eq!(infer_import_format("data"), "gql");
    }
}
