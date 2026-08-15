// SPDX-License-Identifier: BSL-1.1

use clap::{Parser, Subcommand};

/// `ErmyaGraph` Enterprise CLI — admin tool for interacting with a running server.
#[derive(Parser, Debug)]
#[command(name = "ermya-cli", version, about)]
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

    /// Target database name. Required when connecting to a v0.5.0+
    /// multi-database server; ignored against legacy single-database
    /// servers. Sent as `extra["db"]` on every RUN per Bolt 4.x/5.x.
    #[arg(short = 'd', long)]
    pub database: Option<String>,

    /// Password (prefer `ERMYA_PASSWORD` env var or interactive prompt).
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

    /// Offline administration: user management and password hashing.
    ///
    /// These commands operate directly against the on-disk system graph
    /// and do not connect to a running server. Intended for recovery
    /// (lost admin password, bulk bootstrap) with the server stopped —
    /// an exclusive `fs2` advisory lock is acquired so a running
    /// server would block us, and vice versa.
    Admin(AdminArgs),
}

/// Arguments for the `admin` subcommand.
#[derive(Parser, Debug)]
pub struct AdminArgs {
    /// Path to the audit log. Catalog mutations issued by `admin
    /// databases create/drop` and `admin grants grant/revoke` emit
    /// the spec §6.3 events (`database_created`, `database_dropped`,
    /// `grant_changed`) to this file via the synchronous one-shot
    /// sink. Omitting both this flag and the env var disables audit
    /// emission for the offline path (read-only subcommands and
    /// user/hash operations never emit either way).
    #[arg(long, env = "ERMYA_AUDIT_LOG")]
    pub audit_log: Option<std::path::PathBuf>,

    #[command(subcommand)]
    pub action: AdminAction,
}

/// Las subórdenes administrativas con el servidor parado.
///
/// **El corte entre ediciones va por subórden, no por orden.** Las de cuentas
/// locales son públicas —la autenticación básica no se esconde tras el muro de
/// pago, misma decisión que en el servidor— y las de catálogo, permisos y
/// restauración viven en [`crate::cli_enterprise`], que se va con la edición de
/// pago.
///
/// Las de pago llegan aquí por una única variante que envuelve su propio
/// enumerado, y **al copiar el árbol público esa variante se quita entera**,
/// junto con la rama del despacho que la atiende.
///
/// La primera versión de esto dejaba la variante puesta esperando que se
/// "quedara sin contenido posible" al irse el enumerado. No funciona: una
/// variante que nombra un tipo ausente no compila, tenga habitantes o no. Y
/// aunque compilara sería lo contrario de lo que se quiere — un cliente público
/// que anuncia en su ayuda subórdenes que no sabe servir.
#[derive(Subcommand, Debug)]
pub enum AdminAction {
    /// User management (list / add / rm / passwd / enable / disable / promote / demote).
    Users(UsersArgs),
    /// Compute an `argon2id` PHC hash for a password. Useful for scripted bootstrap.
    Hash(HashArgs),
}

#[derive(Parser, Debug)]
pub struct UsersArgs {
    /// Data directory of the server. The system graph is opened at
    /// `{data-dir}/system/` and guarded by an exclusive advisory lock.
    #[arg(long)]
    pub data_dir: String,

    #[command(subcommand)]
    pub action: UsersSub,
}

#[derive(Subcommand, Debug)]
pub enum UsersSub {
    /// List all users. Output is tab-separated: `user\tenabled\tis_admin\tcreated_at`.
    List,
    /// Create a user.
    Add(UserMutArgs),
    /// Remove a user. Exits with code 2 if the user is the last admin.
    Rm(UserRefArgs),
    /// Change a user's password.
    Passwd(UserMutArgs),
    /// Enable a disabled user.
    Enable(UserRefArgs),
    /// Disable a user without removing them.
    Disable(UserRefArgs),
    /// Grant admin privileges.
    Promote(UserRefArgs),
    /// Revoke admin privileges. Exits with code 2 if it would leave no admins.
    Demote(UserRefArgs),
}

#[derive(Parser, Debug)]
pub struct UserRefArgs {
    #[arg(long)]
    pub username: String,
}

#[derive(Parser, Debug)]
pub struct UserMutArgs {
    #[arg(long)]
    pub username: String,
    /// Password as a command-line argument. Mutually exclusive with `--prompt`.
    #[arg(long, conflicts_with = "prompt")]
    pub password: Option<String>,
    /// Read the password interactively (silent). Mutually exclusive with `--password`.
    #[arg(long, default_value_t = false)]
    pub prompt: bool,
    /// Grant admin privileges at creation. Applies to `add` only; ignored
    /// by `passwd` so rotations never silently change the admin flag.
    #[arg(long, default_value_t = false)]
    pub admin: bool,
}

#[derive(Parser, Debug)]
pub struct HashArgs {
    /// Password as a positional argument. Mutually exclusive with `--prompt`.
    #[arg(conflicts_with = "prompt")]
    pub password: Option<String>,
    /// Read the password interactively (silent). Mutually exclusive with the positional argument.
    #[arg(long, default_value_t = false)]
    pub prompt: bool,
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
        let cli = Cli::try_parse_from(["ermya-cli"]).unwrap();
        assert!(cli.command.is_none());
        assert!(cli.host.is_none());
    }

    #[test]
    fn parse_host_and_port_flags() {
        let cli = Cli::try_parse_from(["ermya-cli", "-H", "db.prod", "-p", "9000"]).unwrap();
        assert_eq!(cli.host.as_deref(), Some("db.prod"));
        assert_eq!(cli.port, Some(9000));
    }

    #[test]
    fn parse_query_subcommand() {
        let cli = Cli::try_parse_from(["ermya-cli", "query", "MATCH (n) RETURN n"]).unwrap();
        let Some(Command::Query(q)) = cli.command else {
            panic!("expected Query command");
        };
        assert_eq!(q.query, "MATCH (n) RETURN n");
        assert_eq!(q.language, "gql");
    }

    #[test]
    fn parse_query_with_language() {
        let cli =
            Cli::try_parse_from(["ermya-cli", "query", "-l", "cypher", "MATCH (n) RETURN n"])
                .unwrap();
        let Some(Command::Query(q)) = cli.command else {
            panic!("expected Query command");
        };
        assert_eq!(q.language, "cypher");
    }

    #[test]
    fn parse_ping_subcommand() {
        let cli = Cli::try_parse_from(["ermya-cli", "ping"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Ping)));
    }

    #[test]
    fn parse_import_subcommand() {
        let cli = Cli::try_parse_from([
            "ermya-cli",
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
        let cli =
            Cli::try_parse_from(["ermya-cli", "import", "data.json", "--continue-on-error"])
                .unwrap(); // OK: test
        let Some(Command::Import(i)) = cli.command else {
            panic!("expected Import command");
        };
        assert!(i.continue_on_error);
    }

    #[test]
    fn parse_export_subcommand() {
        let cli = Cli::try_parse_from([
            "ermya-cli",
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
        let cli = Cli::try_parse_from(["ermya-cli", "exec", "schema.gql"]).unwrap();
        let Some(Command::Exec(e)) = cli.command else {
            panic!("expected Exec command");
        };
        assert_eq!(e.file, "schema.gql");
    }

    #[test]
    fn parse_tls_skip_verify() {
        let cli = Cli::try_parse_from(["ermya-cli", "--tls-skip-verify", "ping"]).unwrap();
        assert!(cli.tls_skip_verify);
    }

    #[test]
    fn parse_version_subcommand() {
        let cli = Cli::try_parse_from(["ermya-cli", "version"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Version)));
    }

    #[test]
    fn parse_admin_hash_positional_password() {
        let cli = Cli::try_parse_from(["ermya-cli", "admin", "hash", "hunter22!x"]).unwrap();
        let Some(Command::Admin(a)) = cli.command else {
            panic!("expected Admin command");
        };
        let AdminAction::Hash(h) = a.action else {
            panic!("expected Hash action");
        };
        assert_eq!(h.password.as_deref(), Some("hunter22!x"));
        assert!(!h.prompt);
    }

    #[test]
    fn parse_admin_hash_prompt_flag() {
        let cli = Cli::try_parse_from(["ermya-cli", "admin", "hash", "--prompt"]).unwrap();
        let Some(Command::Admin(a)) = cli.command else {
            panic!("expected Admin");
        };
        let AdminAction::Hash(h) = a.action else {
            panic!("expected Hash");
        };
        assert!(h.prompt);
        assert!(h.password.is_none());
    }

    #[test]
    fn parse_admin_hash_rejects_both_prompt_and_password() {
        let err = Cli::try_parse_from(["ermya-cli", "admin", "hash", "hunter22!x", "--prompt"])
            .expect_err("mutually exclusive args must clash");
        let msg = err.to_string();
        assert!(msg.contains("cannot be used with"), "got: {msg}");
    }

    #[test]
    fn parse_admin_users_add() {
        let cli = Cli::try_parse_from([
            "ermya-cli",
            "admin",
            "users",
            "--data-dir",
            "/srv/ermya",
            "add",
            "--username",
            "alice",
            "--password",
            "hunter22!x",
            "--admin",
        ])
        .unwrap();
        let Some(Command::Admin(a)) = cli.command else {
            panic!("expected Admin");
        };
        let AdminAction::Users(u) = a.action else {
            panic!("expected Users");
        };
        assert_eq!(u.data_dir, "/srv/ermya");
        let UsersSub::Add(args) = u.action else {
            panic!("expected Add");
        };
        assert_eq!(args.username, "alice");
        assert_eq!(args.password.as_deref(), Some("hunter22!x"));
        assert!(args.admin);
    }

    #[test]
    fn parse_admin_users_rm() {
        let cli = Cli::try_parse_from([
            "ermya-cli",
            "admin",
            "users",
            "--data-dir",
            "/d",
            "rm",
            "--username",
            "alice",
        ])
        .unwrap();
        let Some(Command::Admin(a)) = cli.command else {
            panic!();
        };
        let AdminAction::Users(u) = a.action else {
            panic!();
        };
        assert!(matches!(u.action, UsersSub::Rm(ref r) if r.username == "alice"));
    }

    // ─── admin databases ──────────────────────────────────────────────────

    // ─── admin grants ─────────────────────────────────────────────────────
}
