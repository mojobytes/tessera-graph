// SPDX-License-Identifier: BSL-1.1

use std::path::Path;

use crate::cli::Cli;
use crate::error::CliError;

/// Connection configuration resolved from CLI flags, env vars, config file, and defaults.
///
/// Passwords are **not** stored here. They are resolved ephemerally and dropped after
/// authentication.
#[derive(Debug, Clone)]
pub struct ConnectionConfig {
    pub host: String,
    pub port: u16,
    pub username: String,
    /// Target database for the session. `None` means single-database
    /// (legacy v0.4 / `NoAuthProvider`) servers; against a v0.5.0+
    /// multi-database server the first RUN will be rejected with
    /// `DatabaseNotFound`. Set via `--database/-d` CLI flag,
    /// `ERMYA_DATABASE` env var, or `database` in `[connection]`
    /// of the TOML config.
    pub database: Option<String>,
    pub connect_timeout_secs: u64,
    pub ca_cert: Option<String>,
    pub tls_skip_verify: bool,
    pub format: String,
    pub language: String,
}

impl Default for ConnectionConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".to_owned(),
            port: 7687,
            username: "admin".to_owned(),
            database: None,
            connect_timeout_secs: 10,
            ca_cert: None,
            tls_skip_verify: false,
            format: "table".to_owned(),
            language: "gql".to_owned(),
        }
    }
}

/// Source for environment variable lookups, abstracted for testability.
///
/// In production, uses `std::env::var`. In tests, uses a closure or map.
pub trait EnvSource {
    fn get(&self, key: &str) -> Option<String>;
}

/// Real environment source — reads from process environment.
pub struct RealEnv;

impl EnvSource for RealEnv {
    fn get(&self, key: &str) -> Option<String> {
        std::env::var(key).ok().filter(|v| !v.is_empty())
    }
}

/// Test environment source — reads from a provided map.
#[cfg(test)]
struct TestEnv {
    vars: std::collections::HashMap<String, String>,
}

#[cfg(test)]
impl EnvSource for TestEnv {
    fn get(&self, key: &str) -> Option<String> {
        self.vars.get(key).cloned()
    }
}

impl ConnectionConfig {
    /// Resolve configuration with full precedence: CLI flags > env vars > config file > defaults.
    ///
    /// Searches for config file at: `./ermya.toml`, `~/.config/ermya/ermya.toml`,
    /// `~/.ermya.toml`.
    #[must_use]
    pub fn resolve_full(cli: &Cli) -> (Self, Option<String>) {
        Self::resolve_full_with_env(cli, &RealEnv)
    }

    /// Resolve configuration without reading any config file.
    ///
    /// Precedence: CLI flags > env vars > defaults.
    /// For most use cases, prefer [`ConnectionConfig::resolve_full`] which also
    /// consults `ermya.toml`.
    #[must_use]
    pub fn resolve_without_file(cli: &Cli) -> (Self, Option<String>) {
        Self::resolve_with_env(cli, &RealEnv)
    }

    /// Resolve without file, using a custom env source (for testability).
    #[must_use]
    pub fn resolve_with_env(cli: &Cli, env: &dyn EnvSource) -> (Self, Option<String>) {
        merge_options(cli, env, None)
    }

    /// Load configuration from a TOML file.
    ///
    /// Returns `None` if the file does not exist.
    ///
    /// # Errors
    ///
    /// Returns `CliError::Config` if:
    /// - The file exists but contains invalid TOML syntax.
    /// - The file contains a `password` key (security policy: passwords must not be stored in config files).
    /// - The file cannot be read (permissions, I/O error).
    pub fn from_toml_file(path: &Path) -> Result<Option<TomlFileConfig>, CliError> {
        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                return Err(CliError::Config(format!(
                    "cannot read {}: {e}",
                    path.display()
                )));
            }
        };

        // Reject files that contain a password key anywhere in [connection]
        let raw: toml::Value = toml::from_str(&content)
            .map_err(|e| CliError::Config(format!("invalid TOML in {}: {e}", path.display())))?;

        if let Some(conn) = raw.get("connection") {
            if conn.get("password").is_some() {
                return Err(CliError::Config(
                    "config file must not contain a password key — use ERMYA_PASSWORD env var or interactive prompt".to_owned(),
                ));
            }
        }

        let file_cfg: TomlFileConfig = toml::from_str(&content)
            .map_err(|e| CliError::Config(format!("invalid TOML in {}: {e}", path.display())))?;

        Ok(Some(file_cfg))
    }

    /// Full resolve with injectable env source (for testability).
    #[must_use]
    pub fn resolve_full_with_env(cli: &Cli, env: &dyn EnvSource) -> (Self, Option<String>) {
        let file_cfg = Self::find_and_load_config_file();
        merge_options(cli, env, file_cfg.as_ref())
    }

    /// Search for a config file in standard locations.
    fn find_and_load_config_file() -> Option<TomlFileConfig> {
        let candidates = config_file_candidates();
        for path in &candidates {
            match Self::from_toml_file(path) {
                Ok(Some(cfg)) => return Some(cfg),
                Ok(None) => {}
                Err(e) => {
                    eprintln!("Warning: {e}");
                }
            }
        }
        None
    }
}

/// Returns the list of candidate config file paths in priority order.
fn config_file_candidates() -> Vec<std::path::PathBuf> {
    let mut candidates = vec![std::path::PathBuf::from("./ermya.toml")];
    if let Some(home) = home_dir() {
        candidates.push(home.join(".config/ermya/ermya.toml"));
        candidates.push(home.join(".ermya.toml"));
    }
    candidates
}

/// Cross-platform home directory.
#[must_use]
pub fn home_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(std::path::PathBuf::from)
}

/// TOML config file structure.
#[derive(Debug, serde::Deserialize)]
pub struct TomlFileConfig {
    pub connection: Option<TomlConnection>,
    pub defaults: Option<TomlDefaults>,
}

/// `[connection]` section in the TOML config file.
#[derive(Debug, serde::Deserialize)]
pub struct TomlConnection {
    pub host: Option<String>,
    pub port: Option<u16>,
    pub username: Option<String>,
    pub database: Option<String>,
    pub ca_cert: Option<String>,
    pub connect_timeout_secs: Option<u64>,
}

/// `[defaults]` section in the TOML config file.
#[derive(Debug, serde::Deserialize)]
pub struct TomlDefaults {
    pub language: Option<String>,
    pub format: Option<String>,
}

/// Single source of truth for configuration precedence: CLI flags > env > file > defaults.
fn merge_options(
    cli: &Cli,
    env: &dyn EnvSource,
    file_cfg: Option<&TomlFileConfig>,
) -> (ConnectionConfig, Option<String>) {
    let defaults = ConnectionConfig::default();

    let host = cli
        .host
        .clone()
        .or_else(|| env.get("ERMYA_HOST"))
        .or_else(|| file_cfg.and_then(|f| f.connection.as_ref()?.host.clone()))
        .unwrap_or(defaults.host);

    let port = cli
        .port
        .or_else(|| env.get("ERMYA_PORT").and_then(|v| v.parse().ok()))
        .or_else(|| file_cfg.and_then(|f| f.connection.as_ref()?.port))
        .unwrap_or(defaults.port);

    let username = cli
        .username
        .clone()
        .or_else(|| env.get("ERMYA_USER"))
        .or_else(|| file_cfg.and_then(|f| f.connection.as_ref()?.username.clone()))
        .unwrap_or(defaults.username);

    let database = cli
        .database
        .clone()
        .or_else(|| env.get("ERMYA_DATABASE"))
        .or_else(|| file_cfg.and_then(|f| f.connection.as_ref()?.database.clone()));

    let ca_cert = cli
        .ca_cert
        .clone()
        .or_else(|| env.get("ERMYA_CA_CERT"))
        .or_else(|| file_cfg.and_then(|f| f.connection.as_ref()?.ca_cert.clone()))
        .or(defaults.ca_cert);

    let connect_timeout_secs = cli
        .connect_timeout
        .or_else(|| {
            env.get("ERMYA_CONNECT_TIMEOUT")
                .and_then(|v| v.parse().ok())
        })
        .or_else(|| file_cfg.and_then(|f| f.connection.as_ref()?.connect_timeout_secs))
        .unwrap_or(defaults.connect_timeout_secs);

    let format = cli
        .format
        .clone()
        .or_else(|| file_cfg.and_then(|f| f.defaults.as_ref()?.format.clone()))
        .unwrap_or(defaults.format);

    let language = file_cfg
        .and_then(|f| f.defaults.as_ref()?.language.clone())
        .unwrap_or(defaults.language);

    let password = cli.password.clone().or_else(|| env.get("ERMYA_PASSWORD"));

    let cfg = ConnectionConfig {
        host,
        port,
        username,
        database,
        connect_timeout_secs,
        ca_cert,
        tls_skip_verify: cli.tls_skip_verify,
        format,
        language,
    };

    (cfg, password)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use std::collections::HashMap;

    fn cli_from(args: &[&str]) -> Cli {
        Cli::try_parse_from(args).unwrap()
    }

    fn empty_env() -> TestEnv {
        TestEnv {
            vars: HashMap::new(),
        }
    }

    fn env_with(pairs: &[(&str, &str)]) -> TestEnv {
        TestEnv {
            vars: pairs
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
        }
    }

    #[test]
    fn defaults_are_correct() {
        let cfg = ConnectionConfig::default();
        assert_eq!(cfg.host, "127.0.0.1");
        assert_eq!(cfg.port, 7687);
        assert_eq!(cfg.username, "admin");
        assert_eq!(cfg.connect_timeout_secs, 10);
        assert!(cfg.ca_cert.is_none());
        assert!(!cfg.tls_skip_verify);
        assert_eq!(cfg.format, "table");
        assert_eq!(cfg.language, "gql");
    }

    #[test]
    fn cli_flags_override_defaults() {
        let cli = cli_from(&["ermya-cli", "-H", "flag-host", "-p", "9000", "-u", "bob"]);
        let (cfg, _) = ConnectionConfig::resolve_with_env(&cli, &empty_env());
        assert_eq!(cfg.host, "flag-host");
        assert_eq!(cfg.port, 9000);
        assert_eq!(cfg.username, "bob");
    }

    #[test]
    fn env_overrides_default() {
        let env = env_with(&[("ERMYA_HOST", "env-host.local")]);
        let cli = cli_from(&["ermya-cli"]);
        let (cfg, _) = ConnectionConfig::resolve_with_env(&cli, &env);
        assert_eq!(cfg.host, "env-host.local");
    }

    #[test]
    fn cli_flag_overrides_env() {
        let env = env_with(&[("ERMYA_HOST", "env-host.local")]);
        let cli = cli_from(&["ermya-cli", "-H", "flag-host"]);
        let (cfg, _) = ConnectionConfig::resolve_with_env(&cli, &env);
        assert_eq!(cfg.host, "flag-host");
    }

    #[test]
    fn env_port_parsed_as_u16() {
        let env = env_with(&[("ERMYA_PORT", "9999")]);
        let cli = cli_from(&["ermya-cli"]);
        let (cfg, _) = ConnectionConfig::resolve_with_env(&cli, &env);
        assert_eq!(cfg.port, 9999);
    }

    #[test]
    fn env_port_invalid_falls_back_to_default() {
        let env = env_with(&[("ERMYA_PORT", "not_a_number")]);
        let cli = cli_from(&["ermya-cli"]);
        let (cfg, _) = ConnectionConfig::resolve_with_env(&cli, &env);
        assert_eq!(cfg.port, 7687);
    }

    #[test]
    fn password_from_flag() {
        let cli = cli_from(&["ermya-cli", "--password", "secret123"]);
        let (_, password) = ConnectionConfig::resolve_with_env(&cli, &empty_env());
        assert_eq!(password.as_deref(), Some("secret123"));
    }

    #[test]
    fn password_from_env() {
        let env = env_with(&[("ERMYA_PASSWORD", "env-pass")]);
        let cli = cli_from(&["ermya-cli"]);
        let (_, password) = ConnectionConfig::resolve_with_env(&cli, &env);
        assert_eq!(password.as_deref(), Some("env-pass"));
    }

    #[test]
    fn password_flag_overrides_env() {
        let env = env_with(&[("ERMYA_PASSWORD", "env-pass")]);
        let cli = cli_from(&["ermya-cli", "--password", "flag-pass"]);
        let (_, password) = ConnectionConfig::resolve_with_env(&cli, &env);
        assert_eq!(password.as_deref(), Some("flag-pass"));
    }

    #[test]
    fn no_password_returns_none() {
        let cli = cli_from(&["ermya-cli"]);
        let (_, password) = ConnectionConfig::resolve_with_env(&cli, &empty_env());
        assert!(password.is_none());
    }

    #[test]
    fn ca_cert_from_flag() {
        let cli = cli_from(&["ermya-cli", "--ca-cert", "/path/ca.pem"]);
        let (cfg, _) = ConnectionConfig::resolve_with_env(&cli, &empty_env());
        assert_eq!(cfg.ca_cert.as_deref(), Some("/path/ca.pem"));
    }

    #[test]
    fn ca_cert_from_env() {
        let env = env_with(&[("ERMYA_CA_CERT", "/env/ca.pem")]);
        let cli = cli_from(&["ermya-cli"]);
        let (cfg, _) = ConnectionConfig::resolve_with_env(&cli, &env);
        assert_eq!(cfg.ca_cert.as_deref(), Some("/env/ca.pem"));
    }

    #[test]
    fn connect_timeout_from_flag() {
        let cli = cli_from(&["ermya-cli", "--connect-timeout", "30"]);
        let (cfg, _) = ConnectionConfig::resolve_with_env(&cli, &empty_env());
        assert_eq!(cfg.connect_timeout_secs, 30);
    }

    #[test]
    fn format_from_flag() {
        let cli = cli_from(&["ermya-cli", "--format", "json"]);
        let (cfg, _) = ConnectionConfig::resolve_with_env(&cli, &empty_env());
        assert_eq!(cfg.format, "json");
    }

    #[test]
    fn username_from_env() {
        let env = env_with(&[("ERMYA_USER", "envuser")]);
        let cli = cli_from(&["ermya-cli"]);
        let (cfg, _) = ConnectionConfig::resolve_with_env(&cli, &env);
        assert_eq!(cfg.username, "envuser");
    }

    #[test]
    fn full_precedence_chain() {
        let env = env_with(&[
            ("ERMYA_HOST", "env-host"),
            ("ERMYA_PORT", "8888"),
            ("ERMYA_USER", "env-user"),
        ]);
        let cli = cli_from(&["ermya-cli", "-H", "flag-host"]);
        let (cfg, _) = ConnectionConfig::resolve_with_env(&cli, &env);
        // flag wins for host
        assert_eq!(cfg.host, "flag-host");
        // env wins for port and user (no flag)
        assert_eq!(cfg.port, 8888);
        assert_eq!(cfg.username, "env-user");
    }

    #[test]
    fn toml_file_sets_host_and_port() {
        let dir = tempfile::tempdir().expect("tempdir"); // OK: test
        let path = dir.path().join("ermya.toml");
        std::fs::write(
            &path,
            "[connection]\nhost = \"file-host.local\"\nport = 9999\n",
        )
        .expect("write"); // OK: test
        let file_cfg = ConnectionConfig::from_toml_file(&path)
            .expect("parse") // OK: test
            .expect("some"); // OK: test
        let conn = file_cfg.connection.expect("connection"); // OK: test
        assert_eq!(conn.host.as_deref(), Some("file-host.local"));
        assert_eq!(conn.port, Some(9999));
    }

    #[test]
    fn toml_missing_file_returns_none() {
        let result = ConnectionConfig::from_toml_file(Path::new("/nonexistent/path/ermya.toml"))
            .expect("no io error"); // OK: test
        assert!(result.is_none());
    }

    #[test]
    fn toml_with_password_key_is_rejected() {
        let dir = tempfile::tempdir().expect("tempdir"); // OK: test
        let path = dir.path().join("ermya.toml");
        std::fs::write(&path, "[connection]\npassword = \"secret\"\n").expect("write"); // OK: test
        let result = ConnectionConfig::from_toml_file(&path);
        assert!(result.is_err());
        let err = result.expect_err("should be error"); // OK: test
        assert!(matches!(err, CliError::Config(_)));
        assert!(err.to_string().contains("password"));
    }

    #[test]
    fn toml_with_defaults_section() {
        let dir = tempfile::tempdir().expect("tempdir"); // OK: test
        let path = dir.path().join("ermya.toml");
        std::fs::write(
            &path,
            "[defaults]\nlanguage = \"cypher\"\nformat = \"json\"\n",
        )
        .expect("write"); // OK: test
        let file_cfg = ConnectionConfig::from_toml_file(&path)
            .expect("parse") // OK: test
            .expect("some"); // OK: test
        let defaults = file_cfg.defaults.expect("defaults"); // OK: test
        assert_eq!(defaults.language.as_deref(), Some("cypher"));
        assert_eq!(defaults.format.as_deref(), Some("json"));
    }

    #[test]
    fn toml_invalid_syntax_returns_error() {
        let dir = tempfile::tempdir().expect("tempdir"); // OK: test
        let path = dir.path().join("ermya.toml");
        std::fs::write(&path, "[connection\n").expect("write"); // OK: test
        let result = ConnectionConfig::from_toml_file(&path);
        assert!(result.is_err());
    }

    #[test]
    fn toml_empty_file_returns_empty_config() {
        let dir = tempfile::tempdir().expect("tempdir"); // OK: test
        let path = dir.path().join("ermya.toml");
        std::fs::write(&path, "").expect("write"); // OK: test
        let file_cfg = ConnectionConfig::from_toml_file(&path)
            .expect("parse") // OK: test
            .expect("some"); // OK: test
        assert!(file_cfg.connection.is_none());
        assert!(file_cfg.defaults.is_none());
    }

    #[test]
    fn resolve_and_resolve_full_agree_when_no_file_config() {
        let cli = cli_from(&["ermya-cli", "-H", "shared-host", "--connect-timeout", "42"]);
        let env = empty_env();
        let (cfg_basic, pwd_basic) = ConnectionConfig::resolve_with_env(&cli, &env);
        let (cfg_full, pwd_full) = ConnectionConfig::resolve_full_with_env(&cli, &env);
        assert_eq!(cfg_basic.host, cfg_full.host);
        assert_eq!(cfg_basic.port, cfg_full.port);
        assert_eq!(cfg_basic.username, cfg_full.username);
        assert_eq!(
            cfg_basic.connect_timeout_secs,
            cfg_full.connect_timeout_secs
        );
        assert_eq!(cfg_basic.format, cfg_full.format);
        assert_eq!(pwd_basic, pwd_full);
    }

    #[test]
    fn toml_file_sets_connect_timeout() {
        let dir = tempfile::tempdir().expect("tempdir"); // OK: test
        let path = dir.path().join("ermya.toml");
        std::fs::write(&path, "[connection]\nconnect_timeout_secs = 42\n").expect("write"); // OK: test
        let file_cfg = ConnectionConfig::from_toml_file(&path)
            .expect("parse") // OK: test
            .expect("some"); // OK: test
        let conn = file_cfg.connection.expect("connection"); // OK: test
        assert_eq!(conn.connect_timeout_secs, Some(42));
    }

    #[test]
    fn merge_options_reads_connect_timeout_from_file() {
        let file_cfg = TomlFileConfig {
            connection: Some(TomlConnection {
                host: None,
                port: None,
                username: None,
                database: None,
                ca_cert: None,
                connect_timeout_secs: Some(99),
            }),
            defaults: None,
        };
        let cli = cli_from(&["ermya-cli"]);
        let (cfg, _) = merge_options(&cli, &empty_env(), Some(&file_cfg));
        assert_eq!(cfg.connect_timeout_secs, 99);
    }

    #[test]
    fn merge_options_file_host_used_when_no_flag_or_env() {
        let file_cfg = TomlFileConfig {
            connection: Some(TomlConnection {
                host: Some("file-host.example".to_owned()),
                port: None,
                username: None,
                database: None,
                ca_cert: None,
                connect_timeout_secs: None,
            }),
            defaults: None,
        };
        let cli = cli_from(&["ermya-cli"]);
        let (cfg, _) = merge_options(&cli, &empty_env(), Some(&file_cfg));
        assert_eq!(cfg.host, "file-host.example");
    }

    #[test]
    fn merge_options_database_precedence_cli_over_env_over_file() {
        // CLI flag wins.
        let cli = cli_from(&["ermya-cli", "--database", "from-cli"]);
        let env = env_with(&[("ERMYA_DATABASE", "from-env")]);
        let file_cfg = TomlFileConfig {
            connection: Some(TomlConnection {
                host: None,
                port: None,
                username: None,
                database: Some("from-file".to_owned()),
                ca_cert: None,
                connect_timeout_secs: None,
            }),
            defaults: None,
        };
        let (cfg, _) = merge_options(&cli, &env, Some(&file_cfg));
        assert_eq!(cfg.database.as_deref(), Some("from-cli"));

        // Env wins when CLI absent.
        let cli = cli_from(&["ermya-cli"]);
        let (cfg, _) = merge_options(&cli, &env, Some(&file_cfg));
        assert_eq!(cfg.database.as_deref(), Some("from-env"));

        // File wins when CLI + env absent.
        let (cfg, _) = merge_options(&cli, &empty_env(), Some(&file_cfg));
        assert_eq!(cfg.database.as_deref(), Some("from-file"));

        // None when nothing set.
        let (cfg, _) = merge_options(&cli, &empty_env(), None);
        assert_eq!(cfg.database, None);
    }
}
