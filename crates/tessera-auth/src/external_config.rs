// Copyright 2026 BelowZero Security OU. All rights reserved.

//! Authentication mode configuration.

use std::fmt;
use std::str::FromStr;

use crate::error::AuthError;

/// Authentication mode selected at server startup via `TESSERA_AUTH_MODE`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthMode {
    /// Local Argon2id password authentication (default).
    Local,
    /// LDAP bind authentication against Active Directory / `OpenLDAP`.
    Ldap,
    /// OIDC JWT validation against an identity provider.
    Oidc,
}

impl FromStr for AuthMode {
    type Err = AuthError;

    fn from_str(s: &str) -> crate::error::Result<Self> {
        match s.to_lowercase().as_str() {
            "local" => Ok(Self::Local),
            "ldap" => Ok(Self::Ldap),
            "oidc" => Ok(Self::Oidc),
            other => Err(AuthError::ConfigError(format!(
                "unknown auth mode: {other} (expected: local, ldap, oidc)"
            ))),
        }
    }
}

impl fmt::Display for AuthMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Local => write!(f, "local"),
            Self::Ldap => write!(f, "ldap"),
            Self::Oidc => write!(f, "oidc"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_mode_from_str_local() {
        let mode: AuthMode = "local".parse().expect("parse"); // OK: test
        assert_eq!(mode, AuthMode::Local);
    }

    #[test]
    fn auth_mode_from_str_ldap() {
        let mode: AuthMode = "ldap".parse().expect("parse"); // OK: test
        assert_eq!(mode, AuthMode::Ldap);
    }

    #[test]
    fn auth_mode_from_str_oidc() {
        let mode: AuthMode = "oidc".parse().expect("parse"); // OK: test
        assert_eq!(mode, AuthMode::Oidc);
    }

    #[test]
    fn auth_mode_case_insensitive() {
        let mode: AuthMode = "LDAP".parse().expect("parse"); // OK: test
        assert_eq!(mode, AuthMode::Ldap);
    }

    #[test]
    fn auth_mode_from_str_invalid() {
        let result: Result<AuthMode, _> = "saml".parse();
        assert!(result.is_err());
        assert!(matches!(
            result.expect_err("err"), // OK: test
            AuthError::ConfigError(_)
        ));
    }

    #[test]
    fn auth_mode_display_roundtrips() {
        for mode in [AuthMode::Local, AuthMode::Ldap, AuthMode::Oidc] {
            let s = mode.to_string();
            let parsed: AuthMode = s.parse().expect("roundtrip"); // OK: test
            assert_eq!(parsed, mode);
        }
    }

    #[test]
    fn provider_unavailable_is_distinct() {
        let e = AuthError::ProviderUnavailable("ldap server timeout".to_owned());
        assert!(!matches!(e, AuthError::InvalidCredentials));
        assert!(e.to_string().contains("provider unavailable"));
    }

    #[test]
    fn config_error_is_distinct() {
        let e = AuthError::ConfigError("bad config".to_owned());
        assert!(!matches!(e, AuthError::InvalidCredentials));
        assert!(e.to_string().contains("configuration error"));
    }
}
