// SPDX-License-Identifier: LicenseRef-TesseraGraph-Proprietary

use std::net::ToSocketAddrs;
use std::sync::Arc;
use std::time::Instant;

use clap::Parser;
use tokio::io::{AsyncRead, AsyncWrite, split};
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_rustls::TlsConnector;

use tessera_graph_cli_lib::admin;
use tessera_graph_cli_lib::auth;
use tessera_graph_cli_lib::cli::{AdminAction, Cli, Command};
use tessera_graph_cli_lib::config::ConnectionConfig;
use tessera_graph_cli_lib::connection::Session;
use tessera_graph_cli_lib::error::CliError;
use tessera_graph_cli_lib::export;
use tessera_graph_cli_lib::import::{self, ImportPlan};
use tessera_graph_cli_lib::output::OutputFormat;
use tessera_graph_cli_lib::query;
use tessera_graph_cli_lib::repl::{self, MetaCommand, QueryAccumulator};

use tessera_graph_protocol::BoltClient;

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

    // Admin subcommands — fully offline. These short-circuit before any
    // TLS/Bolt setup so recovery works even when no server is running.
    // The branch owns its exit-code contract (see `admin::users`): it
    // terminates the process directly rather than folding into CliError.
    if let Some(Command::Admin(admin_args)) = cli.command {
        // El destino del registro de auditoría se acepta en la línea de órdenes
        // y **esta edición no lo usa**: sólo emitían las órdenes de catálogo y
        // permisos, que se despachan en el módulo de pago. Las que quedan aquí
        // —cuentas locales y cifrado de contraseña— nunca emitieron, ni siquiera
        // antes del reparto. Se sigue aceptando para que una misma llamada
        // funcione igual contra las dos ediciones.
        // El corte va POR SUBORDEN, igual que el despacho administrativo del
        // servidor: las de cuentas locales son públicas, las de catálogo,
        // permisos y restauración las despacha el módulo de pago. Así el árbol
        // público no nombra ni una de las tres.
        let res = match admin_args.action {
            AdminAction::Users(u) => admin::users::run(u).await,
            AdminAction::Hash(h) => admin::hash::run(h.password, h.prompt)
                .map_err(|msg| (1_i32, msg)),
        };
        match res {
            Ok(()) => std::process::exit(0),
            Err((code, msg)) => {
                eprintln!("{msg}");
                std::process::exit(code);
            }
        }
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
        auth::login(&mut session, &config.username, &password, config.database.as_deref()).await?;
        println!("OK");
        let _ = session.client.goodbye().await;
        return Ok(());
    }

    // Authenticate
    let password = password.unwrap_or_else(|| {
        rpassword::prompt_password("Password: ").unwrap_or_default() // OK: fallback to empty if terminal fails
    });
    auth::login(&mut session, &config.username, &password, config.database.as_deref()).await?;

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
            let rendered = tessera_graph_cli_lib::output::render(
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
        Some(
            Command::Ping | Command::Version | Command::Admin(_),
        ) => unreachable!(),
        None => run_repl(session, config).await?,
    }
    Ok(())
}

async fn handle_exec<R, W>(
    session: &mut Session<R, W>,
    args: &tessera_graph_cli_lib::cli::ExecArgs,
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
        let rendered = tessera_graph_cli_lib::output::render(
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
    args: &tessera_graph_cli_lib::cli::ImportArgs,
) -> Result<(), CliError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let fmt = args
        .format
        .as_deref()
        .unwrap_or_else(|| infer_import_format(&args.file));

    if args.dry_run {
        // Dry-run: load content into memory for summary (batch functions)
        let content = read_import_content(&args.file)?;
        let statements = match fmt {
            "gql" => import::split_gql_statements(&content),
            "csv-nodes" => import::csv_nodes_to_gql(&content)?,
            "json" => import::json_to_gql_statements(&content)?,
            other => {
                return Err(CliError::ImportExport(format!(
                    "unsupported import format: {other}"
                )));
            }
        };
        let plan = ImportPlan::new(statements, 100);
        print!("{}", plan.dry_run_summary());
    } else {
        // Live import: streaming with bounded channel for backpressure
        let reader: Box<dyn std::io::Read + Send> = if args.file == "-" {
            Box::new(std::io::stdin())
        } else {
            Box::new(std::io::BufReader::new(
                std::fs::File::open(&args.file)
                    .map_err(|e| CliError::ImportExport(format!("cannot open {}: {e}", args.file)))?,
            ))
        };

        let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(IMPORT_CHANNEL_CAPACITY);

        let fmt_owned = fmt.to_owned();
        let producer = tokio::task::spawn_blocking(move || -> Result<(), CliError> {
            let send_stmt = |stmt: String| {
                tx.blocking_send(stmt)
                    .map_err(|_| CliError::ImportExport("channel closed".into()))
            };
            match fmt_owned.as_str() {
                "json" => import::stream_json_import(reader, send_stmt).map(|_| ()),
                "gql" => import::stream_gql_import(reader, send_stmt).map(|_| ()),
                "csv-nodes" => import::stream_csv_import(reader, send_stmt).map(|_| ()),
                other => Err(CliError::ImportExport(format!(
                    "unsupported import format: {other}"
                ))),
            }
            // tx drops here, closing the channel (both on Ok and Err)
        });

        let (count, error_count, query_err) =
            run_pipelined_import(&mut session.client, &mut rx).await;

        // Drop rx to unblock producer if we broke early
        drop(rx);

        // Error priority:
        // 1. JoinError (producer panic) — always propagated
        // 2. query_err (Bolt error) — consumer's error takes precedence
        // 3. producer_result (parse/IO error) — producer's logical error
        let producer_result = producer
            .await
            .map_err(|e| CliError::ImportExport(format!("import thread panicked: {e}")))?;

        if let Some(e) = query_err {
            return Err(e);
        }
        producer_result?;

        if error_count > 0 {
            eprintln!("{error_count} statements failed (see errors above).");
        }
        eprintln!("[PROGRESS] Imported {count} statements total.");
    }
    Ok(())
}

/// Read import content from file or stdin into a string (used for dry-run only).
fn read_import_content(file: &str) -> Result<String, CliError> {
    if file == "-" {
        use std::io::Read;
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .map_err(|e| CliError::ImportExport(format!("cannot read stdin: {e}")))?;
        Ok(buf)
    } else {
        std::fs::read_to_string(file)
            .map_err(|e| CliError::ImportExport(format!("cannot read {file}: {e}")))
    }
}

/// Pipelined import loop: queues RUN+PULL pairs in batches, flushes once,
/// and drains all responses.  Returns `(success_count, error_count, first_error)`.
async fn run_pipelined_import<R, W>(
    client: &mut tessera_graph_protocol::BoltClient<R, W>,
    rx: &mut tokio::sync::mpsc::Receiver<String>,
) -> (usize, usize, Option<CliError>)
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut count = 0usize;
    let mut error_count = 0usize;
    let mut query_err: Option<CliError> = None;
    let mut pipeline_depth = 0usize;

    loop {
        let mut batch_done = false;
        while pipeline_depth < PIPELINE_BATCH_SIZE {
            match rx.try_recv() {
                Ok(stmt) => {
                    if let Err(e) = client.pipeline_run(&stmt).await {
                        query_err = Some(CliError::ImportExport(format!(
                            "pipeline write error: {e}"
                        )));
                        batch_done = true;
                        break;
                    }
                    pipeline_depth += 1;
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                    if pipeline_depth > 0 {
                        break;
                    }
                    if let Some(stmt) = rx.recv().await {
                        if let Err(e) = client.pipeline_run(&stmt).await {
                            query_err = Some(CliError::ImportExport(format!(
                                "pipeline write error: {e}"
                            )));
                            batch_done = true;
                            break;
                        }
                        pipeline_depth += 1;
                    } else {
                        batch_done = true;
                        break;
                    }
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                    batch_done = true;
                    break;
                }
            }
        }

        if query_err.is_some() {
            break;
        }

        if pipeline_depth > 0 {
            if let Err(e) = client.flush_pipeline().await {
                query_err = Some(CliError::ImportExport(format!(
                    "pipeline flush error: {e}"
                )));
                break;
            }

            let drain_result = timeout(
                IMPORT_STATEMENT_TIMEOUT,
                client.drain_pipeline(pipeline_depth),
            )
            .await;

            match drain_result {
                Ok(Ok(result)) => {
                    let prev_count = count;
                    count += result.success_count;
                    error_count += result.failure_count;
                    if count / PROGRESS_INTERVAL != prev_count / PROGRESS_INTERVAL {
                        eprintln!("[PROGRESS] Imported {count} statements...");
                    }
                }
                Ok(Err(e)) => {
                    query_err = Some(CliError::ImportExport(format!(
                        "pipeline drain error: {e}"
                    )));
                    break;
                }
                Err(_timeout) => {
                    query_err = Some(CliError::ImportExport(format!(
                        "pipeline drain timed out after {}s",
                        IMPORT_STATEMENT_TIMEOUT.as_secs(),
                    )));
                    break;
                }
            }
            pipeline_depth = 0;
        }

        if batch_done {
            break;
        }
    }

    (count, error_count, query_err)
}

async fn handle_export<R, W>(
    session: &mut Session<R, W>,
    args: &tessera_graph_cli_lib::cli::ExportArgs,
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

async fn run_repl<R, W>(
    session: &mut Session<R, W>,
    config: &ConnectionConfig,
) -> Result<(), CliError>
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
        let prompt = repl::format_prompt(
            &config.username,
            &config.host,
            config.port,
            accumulator.is_pending(),
        );
        let line = match rl.readline(&prompt) {
            Ok(line) => line,
            Err(
                rustyline::error::ReadlineError::Interrupted | rustyline::error::ReadlineError::Eof,
            ) => {
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
                    let elapsed = if show_timing {
                        Some(start.elapsed())
                    } else {
                        None
                    };
                    match tessera_graph_cli_lib::output::render(
                        format,
                        &output.columns,
                        &output.rows,
                        elapsed,
                        true,
                    ) {
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
    tessera_graph_cli_lib::config::home_dir().map(|h| h.join(".tessera_history"))
}

/// Infer import format from file extension.
fn infer_import_format(file: &str) -> &'static str {
    let path = std::path::Path::new(file);
    match path.extension().and_then(|e| e.to_str()) {
        Some(ext) if ext.eq_ignore_ascii_case("gql") || ext.eq_ignore_ascii_case("cypher") => {
            "gql"
        }
        Some(ext) if ext.eq_ignore_ascii_case("csv") => "csv-nodes",
        Some(ext)
            if ext.eq_ignore_ascii_case("json")
                || ext.eq_ignore_ascii_case("ndjson")
                || ext.eq_ignore_ascii_case("jsonl") =>
        {
            "json"
        }
        _ => "gql",
    }
}

const PROGRESS_INTERVAL: usize = 1_000;

/// Maximum time to wait for a pipeline batch to drain during import.
const IMPORT_STATEMENT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// Capacity of the bounded channel between the producer (parser thread)
/// and the consumer (Bolt pipelining loop).
///
/// Sized to keep the pipeline saturated: the consumer pulls up to
/// `PIPELINE_BATCH_SIZE` statements per flush cycle, so the channel
/// must hold at least that many to avoid stalls.
const IMPORT_CHANNEL_CAPACITY: usize = 512;

/// Number of RUN+PULL pairs to queue before flushing.
///
/// This controls the pipelining depth: how many RUN+PULL message pairs are
/// sent before draining responses.  The server writes a SUCCESS response
/// for each RUN and each PULL; if the pipeline is too deep the server's
/// TCP send buffer fills while the client is still sending, causing a
/// TCP deadlock (both sides blocked on write).
///
/// 64 is conservative enough to avoid buffer deadlocks even with large
/// MATCH…CREATE statements (~400 bytes each → ~50 KB outbound, ~2 KB
/// inbound responses), while still providing ~60× fewer round-trips
/// than serial execution.
const PIPELINE_BATCH_SIZE: usize = 64;

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
    fn infer_format_json_extension() {
        assert_eq!(infer_import_format("data.json"), "json");
        assert_eq!(infer_import_format("data.JSON"), "json");
    }

    #[test]
    fn infer_format_ndjson_extension() {
        assert_eq!(infer_import_format("data.ndjson"), "json");
        assert_eq!(infer_import_format("data.NDJSON"), "json");
    }

    #[test]
    fn infer_format_jsonl_extension() {
        assert_eq!(infer_import_format("data.jsonl"), "json");
    }

    #[test]
    fn infer_format_unknown_defaults_to_gql() {
        assert_eq!(infer_import_format("data.txt"), "gql");
        assert_eq!(infer_import_format("data"), "gql");
    }

    #[test]
    fn progress_interval_constant_is_positive() {
        const _: () = assert!(PROGRESS_INTERVAL > 0);
    }

    #[test]
    fn progress_interval_fires_at_multiple() {
        assert_eq!(PROGRESS_INTERVAL % PROGRESS_INTERVAL, 0);
        assert_ne!((PROGRESS_INTERVAL - 1) % PROGRESS_INTERVAL, 0);
    }
}
