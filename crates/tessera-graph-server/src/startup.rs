// Copyright 2026 BelowZero Security OU. All rights reserved.

//! Startup configuration validation.
//!
//! Extracts environment-driven configuration into a testable function that
//! returns actionable error messages instead of panicking.

use tessera_graph_auth::credentials::Password;

/// Validate the admin password from an environment variable value.
///
/// Returns an actionable error message (not a stack trace) if the password
/// is missing or violates the policy.
///
/// # Errors
///
/// - `Err` with a message mentioning `TESSERA_ADMIN_PASSWORD` if the value is `None`
/// - `Err` with the policy violation details if the password is invalid
pub fn validate_admin_password(raw: Option<String>) -> Result<Password, String> {
    let raw = raw.ok_or_else(|| {
        "TESSERA_ADMIN_PASSWORD must be set. \
         Set it via environment variable or Docker secret."
            .to_owned()
    })?;
    Password::new(&raw).map_err(|e| {
        format!(
            "TESSERA_ADMIN_PASSWORD is invalid: {e}. \
             The password must be at least 8 characters with mixed case, a digit, and a symbol."
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_password_returns_err() {
        let Err(msg) = validate_admin_password(None) else {
            panic!("expected Err for missing password");
        };
        assert!(
            msg.contains("TESSERA_ADMIN_PASSWORD"),
            "error must mention the env var name, got: {msg}"
        );
    }

    #[test]
    fn weak_password_returns_err_with_policy() {
        let Err(msg) = validate_admin_password(Some("abc".to_owned())) else {
            panic!("expected Err for weak password");
        };
        assert!(
            msg.contains("invalid"),
            "error must describe the violation, got: {msg}"
        );
    }

    #[test]
    fn valid_password_returns_ok() {
        let result = validate_admin_password(Some("T3st@Secure!".to_owned()));
        assert!(result.is_ok());
    }
}
