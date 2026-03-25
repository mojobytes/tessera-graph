// Copyright 2026 BelowZero Security OU. All rights reserved.

//! LDAP authentication provider for Active Directory / `OpenLDAP` integration.
//!
//! The provider binds as a service account, searches for the user, re-binds as the user
//! to verify credentials, and extracts group membership for RBAC mapping.

use std::collections::HashMap;
use std::fmt::Write as _;

use zeroize::Zeroizing;

use crate::error::{AuthError, Result};
use crate::providers::{ExternalAuthProvider, ExternalUserInfo};

/// Configuration for the LDAP authentication provider.
///
/// `bind_password` is wrapped in [`Zeroizing`] to ensure the service account
/// password is cleared from memory on drop. The `Debug` impl redacts it.
#[derive(Clone)]
pub struct LdapConfig {
    /// LDAP server URL (e.g., `ldap://ldap.example.com:389`).
    pub ldap_url: String,
    /// Distinguished name of the service account for searching users.
    pub bind_dn: String,
    /// Password of the service account. Zeroized on drop; redacted in `Debug`.
    pub bind_password: Zeroizing<String>,
    /// Base DN for user searches (e.g., `ou=users,dc=example,dc=com`).
    pub base_dn: String,
    /// LDAP filter template with `{username}` placeholder (e.g., `(uid={username})`).
    pub user_filter_template: String,
    /// Attribute name for group membership (e.g., `memberOf`).
    pub group_attribute: String,
    /// Whether to use TLS (LDAPS or `StartTLS`).
    pub use_tls: bool,
    /// Mapping of LDAP group DNs to internal role names.
    pub group_mapping: HashMap<String, String>,
}

impl std::fmt::Debug for LdapConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LdapConfig")
            .field("ldap_url", &self.ldap_url)
            .field("bind_dn", &self.bind_dn)
            .field("bind_password", &"[REDACTED]")
            .field("base_dn", &self.base_dn)
            .field("user_filter_template", &self.user_filter_template)
            .field("group_attribute", &self.group_attribute)
            .field("use_tls", &self.use_tls)
            .field("group_mapping", &self.group_mapping)
            .finish()
    }
}

impl LdapConfig {
    /// Load LDAP configuration from environment variables.
    ///
    /// # Errors
    ///
    /// Returns `AuthError::ConfigError` if any required variable is missing.
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            ldap_url: required_env("TESSERA_LDAP_URL")?,
            bind_dn: required_env("TESSERA_LDAP_BIND_DN")?,
            bind_password: Zeroizing::new(required_env("TESSERA_LDAP_BIND_PASSWORD")?),
            base_dn: required_env("TESSERA_LDAP_BASE_DN")?,
            user_filter_template: optional_env("TESSERA_LDAP_USER_FILTER")
                .unwrap_or_else(|| "(uid={username})".to_owned()),
            group_attribute: optional_env("TESSERA_LDAP_GROUP_ATTR")
                .unwrap_or_else(|| "memberOf".to_owned()),
            // TLS is on by default; set TESSERA_LDAP_USE_TLS=false to disable.
            use_tls: optional_env("TESSERA_LDAP_USE_TLS")
                .is_none_or(|v| v.eq_ignore_ascii_case("true")),
            group_mapping: optional_env("TESSERA_LDAP_GROUP_MAPPING")
                .map_or_else(HashMap::new, |s| {
                    crate::providers::group_mapping::parse_group_mapping(&s)
                }),
        })
    }
}

/// A single entry returned by an LDAP search.
#[derive(Debug, Clone)]
pub struct LdapSearchEntry {
    /// The full distinguished name of the entry.
    pub dn: String,
    /// Attribute values keyed by attribute name.
    pub attrs: HashMap<String, Vec<String>>,
}

/// Abstraction over an LDAP connection, enabling mock injection in tests.
///
/// All methods are async to support `ldap3`'s async API.
pub trait LdapConnection: Send + Sync {
    /// Bind as the service account.
    ///
    /// # Errors
    ///
    /// Returns `AuthError::ProviderUnavailable` if the server is unreachable.
    fn service_bind(
        &mut self,
        bind_dn: &str,
        bind_password: &str,
    ) -> impl std::future::Future<Output = Result<()>> + Send;

    /// Search for entries matching `filter` under `base_dn`.
    ///
    /// # Errors
    ///
    /// Returns `AuthError::ProviderUnavailable` on search failure.
    fn search(
        &mut self,
        base_dn: &str,
        filter: &str,
        attrs: &[&str],
    ) -> impl std::future::Future<Output = Result<Vec<LdapSearchEntry>>> + Send;

    /// Re-bind as the user to verify their password.
    ///
    /// # Errors
    ///
    /// Returns `AuthError::InvalidCredentials` if the password is wrong.
    fn user_bind(
        &mut self,
        user_dn: &str,
        password: &str,
    ) -> impl std::future::Future<Output = Result<()>> + Send;
}

/// LDAP authentication provider, generic over the connection type for testability.
pub struct LdapAuthProvider<C: LdapConnection> {
    config: LdapConfig,
    connection: tokio::sync::Mutex<C>,
}

impl<C: LdapConnection> LdapAuthProvider<C> {
    /// Create a provider with an injectable connection (for testing).
    pub fn with_connection(config: LdapConfig, connection: C) -> Self {
        Self {
            config,
            connection: tokio::sync::Mutex::new(connection),
        }
    }
}

impl<C: LdapConnection + 'static> ExternalAuthProvider for LdapAuthProvider<C> {
    fn authenticate(
        &self,
        username: &str,
        credential: &str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<ExternalUserInfo>> + Send + '_>,
    > {
        let username = username.to_owned();
        let credential = Zeroizing::new(credential.to_owned());
        Box::pin(self.do_authenticate(username, credential))
    }

    fn provider_name(&self) -> &'static str {
        "ldap"
    }
}

impl<C: LdapConnection> LdapAuthProvider<C> {
    #[allow(clippy::significant_drop_tightening, clippy::literal_string_with_formatting_args)]
    async fn do_authenticate(&self, username: String, credential: Zeroizing<String>) -> Result<ExternalUserInfo> {
        let username = &username;
        let credential = &credential;
        let escaped_username = escape_ldap_filter_value(username);
        // Template substitution, not a format macro
        let filter = self
            .config
            .user_filter_template
            .replace("{username}", &escaped_username);

        let mut conn = self.connection.lock().await;

        // Step 1: Bind as service account
        conn.service_bind(&self.config.bind_dn, &self.config.bind_password)
            .await
            .map_err(|e| {
                tracing::warn!("LDAP service bind failed: {e}");
                AuthError::InvalidCredentials
            })?;

        // Step 2: Search for user
        let attrs = [self.config.group_attribute.as_str(), "mail", "cn"];
        let entries = conn
            .search(&self.config.base_dn, &filter, &attrs)
            .await
            .map_err(|e| {
                tracing::warn!("LDAP search failed: {e}");
                AuthError::InvalidCredentials
            })?;

        let entry = entries.into_iter().next().ok_or(AuthError::InvalidCredentials)?;

        // Step 3: Re-bind as user to verify password
        conn.user_bind(&entry.dn, credential)
            .await
            .map_err(|_| AuthError::InvalidCredentials)?;

        // Step 4: Extract user info
        let groups = entry
            .attrs
            .get(&self.config.group_attribute)
            .cloned()
            .unwrap_or_default();

        let email = entry
            .attrs
            .get("mail")
            .and_then(|v| v.first().cloned());

        let display_name = entry
            .attrs
            .get("cn")
            .and_then(|v| v.first().cloned());

        Ok(ExternalUserInfo {
            username: username.to_owned(),
            groups,
            email,
            display_name,
        })
    }
}

/// Escape special characters in an LDAP filter value per RFC 4515.
///
/// Prevents LDAP injection via usernames containing `*`, `(`, `)`, `\`, or NUL.
/// Non-ASCII bytes are hex-escaped (`\XX`) per RFC 4515 §3.
#[must_use]
pub fn escape_ldap_filter_value(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'\\' => escaped.push_str("\\5c"),
            b'*' => escaped.push_str("\\2a"),
            b'(' => escaped.push_str("\\28"),
            b')' => escaped.push_str("\\29"),
            0 => escaped.push_str("\\00"),
            // ASCII printable (0x20..=0x7e) minus already-handled specials — safe to pass through.
            // Control chars (0x01..=0x1f) and DEL (0x7f) are hex-escaped per RFC 4515.
            0x20..=0x27 | 0x2b..=0x5b | 0x5d..=0x7e => escaped.push(byte as char),
            // Non-ASCII and remaining control bytes — hex-escape per RFC 4515.
            _ => {
                write!(escaped, "\\{byte:02x}").expect("write to String is infallible");
            }
        }
    }
    escaped
}

/// Read a required environment variable.
fn required_env(key: &str) -> Result<String> {
    std::env::var(key)
        .map_err(|_| AuthError::ConfigError(format!("missing required env var: {key}")))
}

/// Read an optional environment variable.
fn optional_env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> LdapConfig {
        LdapConfig {
            ldap_url: "ldap://localhost:389".to_owned(),
            bind_dn: "cn=svc,dc=example,dc=com".to_owned(),
            bind_password: Zeroizing::new("secret".to_owned()),
            base_dn: "ou=users,dc=example,dc=com".to_owned(),
            user_filter_template: "(uid={username})".to_owned(),
            group_attribute: "memberOf".to_owned(),
            use_tls: false,
            group_mapping: HashMap::new(),
        }
    }

    struct MockLdapConnection {
        service_bind_ok: bool,
        search_result: Option<Vec<LdapSearchEntry>>,
        user_bind_ok: bool,
    }

    impl MockLdapConnection {
        fn ok_with_entry(entry: LdapSearchEntry) -> Self {
            Self {
                service_bind_ok: true,
                search_result: Some(vec![entry]),
                user_bind_ok: true,
            }
        }
    }

    impl LdapConnection for MockLdapConnection {
        async fn service_bind(&mut self, _bind_dn: &str, _bind_password: &str) -> Result<()> {
            if self.service_bind_ok {
                Ok(())
            } else {
                Err(AuthError::ProviderUnavailable(
                    "mock service bind failed".to_owned(),
                ))
            }
        }

        async fn search(
            &mut self,
            _base_dn: &str,
            _filter: &str,
            _attrs: &[&str],
        ) -> Result<Vec<LdapSearchEntry>> {
            self.search_result
                .clone()
                .ok_or_else(|| AuthError::ProviderUnavailable("mock search failed".to_owned()))
        }

        async fn user_bind(&mut self, _user_dn: &str, _password: &str) -> Result<()> {
            if self.user_bind_ok {
                Ok(())
            } else {
                Err(AuthError::InvalidCredentials)
            }
        }
    }

    fn alice_entry() -> LdapSearchEntry {
        let mut attrs = HashMap::new();
        attrs.insert(
            "memberOf".to_owned(),
            vec!["cn=developers,dc=example,dc=com".to_owned()],
        );
        attrs.insert("mail".to_owned(), vec!["alice@example.com".to_owned()]);
        attrs.insert("cn".to_owned(), vec!["Alice Liddell".to_owned()]);
        LdapSearchEntry {
            dn: "uid=alice,ou=users,dc=example,dc=com".to_owned(),
            attrs,
        }
    }

    #[test]
    fn ldap_config_roundtrip() {
        let cfg = test_config();
        assert_eq!(cfg.ldap_url, "ldap://localhost:389");
        assert!(!cfg.use_tls);
    }

    #[test]
    fn ldap_config_debug_redacts_password() {
        let cfg = test_config();
        let debug = format!("{cfg:?}");
        assert!(
            debug.contains("[REDACTED]"),
            "Debug output must redact bind_password"
        );
        assert!(
            !debug.contains("secret"),
            "Debug output must NOT contain the actual password"
        );
    }

    #[tokio::test]
    async fn ldap_authenticate_success() {
        let mock = MockLdapConnection::ok_with_entry(alice_entry());
        let provider = LdapAuthProvider::with_connection(test_config(), mock);

        let info = provider
            .authenticate("alice", "correct-password")
            .await
            .expect("auth ok"); // OK: test
        assert_eq!(info.username, "alice");
        assert_eq!(info.email.as_deref(), Some("alice@example.com"));
        assert_eq!(info.display_name.as_deref(), Some("Alice Liddell"));
        assert!(info
            .groups
            .contains(&"cn=developers,dc=example,dc=com".to_owned()));
    }

    #[tokio::test]
    async fn ldap_service_bind_failure_returns_invalid_credentials() {
        let mock = MockLdapConnection {
            service_bind_ok: false,
            search_result: None,
            user_bind_ok: false,
        };
        let provider = LdapAuthProvider::with_connection(test_config(), mock);
        let err = provider.authenticate("alice", "pw").await.expect_err("fail"); // OK: test
        assert!(matches!(err, AuthError::InvalidCredentials));
    }

    #[tokio::test]
    async fn ldap_user_not_found_returns_invalid_credentials() {
        let mock = MockLdapConnection {
            service_bind_ok: true,
            search_result: Some(vec![]),
            user_bind_ok: false,
        };
        let provider = LdapAuthProvider::with_connection(test_config(), mock);
        let err = provider
            .authenticate("nonexistent", "pw")
            .await
            .expect_err("fail"); // OK: test
        assert!(matches!(err, AuthError::InvalidCredentials));
    }

    #[tokio::test]
    async fn ldap_wrong_password_returns_invalid_credentials() {
        let entry = LdapSearchEntry {
            dn: "uid=alice,ou=users,dc=example,dc=com".to_owned(),
            attrs: HashMap::new(),
        };
        let mock = MockLdapConnection {
            service_bind_ok: true,
            search_result: Some(vec![entry]),
            user_bind_ok: false,
        };
        let provider = LdapAuthProvider::with_connection(test_config(), mock);
        let err = provider
            .authenticate("alice", "wrong-pw")
            .await
            .expect_err("fail"); // OK: test
        assert!(matches!(err, AuthError::InvalidCredentials));
    }

    #[tokio::test]
    async fn ldap_provider_name() {
        let mock = MockLdapConnection::ok_with_entry(alice_entry());
        let provider = LdapAuthProvider::with_connection(test_config(), mock);
        assert_eq!(provider.provider_name(), "ldap");
    }

    #[test]
    fn ldap_escape_prevents_injection() {
        assert_eq!(
            escape_ldap_filter_value("alice*(test)"),
            r"alice\2a\28test\29"
        );
        assert_eq!(escape_ldap_filter_value("normal"), "normal");
        assert_eq!(escape_ldap_filter_value(r"back\slash"), r"back\5cslash");
        assert_eq!(escape_ldap_filter_value("\0null"), r"\00null");
    }

    #[test]
    fn ldap_escape_empty_string() {
        assert_eq!(escape_ldap_filter_value(""), "");
    }

    #[test]
    fn ldap_escape_non_ascii_hex_encoded() {
        // "café" → UTF-8 bytes: c=0x63, a=0x61, f=0x66, é=0xc3 0xa9
        let escaped = escape_ldap_filter_value("café");
        assert_eq!(escaped, r"caf\c3\a9");
    }

    #[test]
    fn ldap_escape_full_unicode() {
        // "日本" → UTF-8: 0xe6 0x97 0xa5 0xe6 0x9c 0xac
        let escaped = escape_ldap_filter_value("日本");
        assert_eq!(escaped, r"\e6\97\a5\e6\9c\ac");
    }

    #[tokio::test]
    async fn ldap_search_failure_returns_invalid_credentials() {
        let mock = MockLdapConnection {
            service_bind_ok: true,
            search_result: None,
            user_bind_ok: false,
        };
        let provider = LdapAuthProvider::with_connection(test_config(), mock);
        let err = provider.authenticate("alice", "pw").await.expect_err("fail"); // OK: test
        assert!(matches!(err, AuthError::InvalidCredentials));
    }

    #[tokio::test]
    async fn ldap_extracts_groups_from_configured_attribute() {
        let mut attrs = HashMap::new();
        attrs.insert(
            "memberOf".to_owned(),
            vec![
                "cn=admins,dc=example,dc=com".to_owned(),
                "cn=dev,dc=example,dc=com".to_owned(),
            ],
        );
        let entry = LdapSearchEntry {
            dn: "uid=bob,ou=users,dc=example,dc=com".to_owned(),
            attrs,
        };
        let mock = MockLdapConnection::ok_with_entry(entry);
        let provider = LdapAuthProvider::with_connection(test_config(), mock);

        let info = provider.authenticate("bob", "pw").await.expect("ok"); // OK: test
        assert_eq!(info.groups.len(), 2);
    }
}
