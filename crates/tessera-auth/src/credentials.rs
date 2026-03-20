// Copyright 2026 BelowZero Security OU. All rights reserved.

use argon2::password_hash::SaltString;
use argon2::{Argon2, PasswordHasher as _, PasswordVerifier as _};
use base64ct::{Base64Unpadded, Encoding};
use rand::RngCore;
use zeroize::Zeroize;

use crate::error::{AuthError, Result};

/// A password value that is zeroized on drop. Never implements `Debug` or `Clone`
/// to prevent accidental leaks in logs or memory.
pub struct Password(String);

impl Password {
    /// Create a new password validated against the default policy.
    ///
    /// # Errors
    ///
    /// Returns `AuthError::PasswordPolicyViolation` if the password does not meet
    /// the default policy requirements.
    pub fn new(raw: &str) -> Result<Self> {
        Self::with_policy(raw, &PasswordPolicy::default())
    }

    /// Create a new password validated against a custom policy.
    ///
    /// # Errors
    ///
    /// Returns `AuthError::PasswordPolicyViolation` if the password does not meet
    /// the given policy requirements.
    pub fn with_policy(raw: &str, policy: &PasswordPolicy) -> Result<Self> {
        policy.validate(raw)?;
        Ok(Self(raw.to_owned()))
    }

    /// Access the raw password bytes. Only used internally for hashing/verification.
    #[must_use]
    pub(crate) fn as_bytes(&self) -> &[u8] {
        self.0.as_bytes()
    }

    /// Access the raw password string. Only used internally for validation.
    #[must_use]
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl Drop for Password {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

/// Opaque wrapper around an Argon2id hash string. The hash includes the algorithm
/// parameters and salt — it is safe to store persistently.
///
/// Does not implement `Clone` — callers that need the hash string should extract
/// it via `.as_str().to_owned()` or `.clone_into()`.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct PasswordHash(String);

impl Zeroize for PasswordHash {
    fn zeroize(&mut self) {
        self.0.zeroize();
    }
}

impl Drop for PasswordHash {
    fn drop(&mut self) {
        self.zeroize();
    }
}

impl PasswordHash {
    /// Reconstruct from a stored hash string (e.g., loaded from JSON).
    #[must_use]
    pub const fn from_stored(hash: String) -> Self {
        Self(hash)
    }

    /// View the hash string. Use only for persistence — never log this value
    /// in production without redaction.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Stateless password hasher using Argon2id with default parameters.
pub struct PasswordHasher {
    _private: (),
}

impl PasswordHasher {
    #[must_use]
    pub const fn new() -> Self {
        Self { _private: () }
    }

    /// Hash a password using Argon2id with a random salt.
    ///
    /// # Errors
    ///
    /// Returns `AuthError::StorageError` if the hashing operation fails internally.
    pub fn hash(&self, password: &Password) -> Result<PasswordHash> {
        let salt = generate_salt();
        let argon2 = Argon2::default();
        let hash = argon2
            .hash_password(password.as_bytes(), &salt)
            .map_err(|e| AuthError::StorageError(format!("hashing failed: {e}")))?;
        Ok(PasswordHash(hash.to_string()))
    }

    /// Verify a password against a stored hash.
    ///
    /// # Errors
    ///
    /// Returns `AuthError::InvalidCredentials` if the password does not match.
    pub fn verify(&self, password: &Password, hash: &PasswordHash) -> Result<()> {
        let parsed = argon2::password_hash::PasswordHash::new(hash.as_str())
            .map_err(|e| AuthError::StorageError(format!("invalid hash format: {e}")))?;
        Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .map_err(|_| AuthError::InvalidCredentials)
    }
}

impl Default for PasswordHasher {
    fn default() -> Self {
        Self::new()
    }
}

/// Policy governing password strength requirements.
#[derive(Debug, Clone)]
pub struct PasswordPolicy {
    min_length: usize,
    require_uppercase: bool,
    require_digit: bool,
    require_symbol: bool,
}

impl PasswordPolicy {
    /// Start building a custom policy.
    #[must_use]
    pub fn builder() -> PasswordPolicyBuilder {
        PasswordPolicyBuilder::default()
    }

    /// Validate a raw string against this policy. Used by `UserStoreHandle`
    /// when it already has the password as a string.
    pub(crate) fn validate_raw_str(&self, raw: &str) -> Result<()> {
        self.validate(raw)
    }

    fn validate(&self, raw: &str) -> Result<()> {
        if raw.len() < self.min_length {
            return Err(AuthError::PasswordPolicyViolation(format!(
                "password must be at least {} characters",
                self.min_length
            )));
        }
        if self.require_uppercase && !raw.chars().any(char::is_uppercase) {
            return Err(AuthError::PasswordPolicyViolation(
                "password must contain at least one uppercase letter".to_owned(),
            ));
        }
        if self.require_digit && !raw.chars().any(|c| c.is_ascii_digit()) {
            return Err(AuthError::PasswordPolicyViolation(
                "password must contain at least one digit".to_owned(),
            ));
        }
        if self.require_symbol && !raw.chars().any(|c| !c.is_alphanumeric()) {
            return Err(AuthError::PasswordPolicyViolation(
                "password must contain at least one symbol".to_owned(),
            ));
        }
        Ok(())
    }
}

impl Default for PasswordPolicy {
    fn default() -> Self {
        Self {
            min_length: 8,
            require_uppercase: true,
            require_digit: true,
            require_symbol: true,
        }
    }
}

/// Builder for constructing a custom `PasswordPolicy`.
#[derive(Debug, Clone)]
pub struct PasswordPolicyBuilder {
    min_length: usize,
    require_uppercase: bool,
    require_digit: bool,
    require_symbol: bool,
}

impl PasswordPolicyBuilder {
    #[must_use]
    pub const fn min_length(mut self, len: usize) -> Self {
        self.min_length = len;
        self
    }

    #[must_use]
    pub const fn require_uppercase(mut self, yes: bool) -> Self {
        self.require_uppercase = yes;
        self
    }

    #[must_use]
    pub const fn require_digit(mut self, yes: bool) -> Self {
        self.require_digit = yes;
        self
    }

    #[must_use]
    pub const fn require_symbol(mut self, yes: bool) -> Self {
        self.require_symbol = yes;
        self
    }

    #[must_use]
    pub const fn build(self) -> PasswordPolicy {
        PasswordPolicy {
            min_length: self.min_length,
            require_uppercase: self.require_uppercase,
            require_digit: self.require_digit,
            require_symbol: self.require_symbol,
        }
    }
}

impl Default for PasswordPolicyBuilder {
    fn default() -> Self {
        Self {
            min_length: 8,
            require_uppercase: true,
            require_digit: true,
            require_symbol: true,
        }
    }
}

/// Generate a random salt compatible with `password-hash` 0.5's `SaltString`.
///
/// `password-hash` depends on `rand_core` 0.6, but we use `rand` 0.9 (which
/// depends on `rand_core` 0.9). To avoid the version conflict we generate 16
/// random bytes with `rand` 0.9, encode them to base64 (the format `SaltString`
/// expects), and parse the result.
fn generate_salt() -> SaltString {
    let mut raw = [0u8; 16];
    rand::rng().fill_bytes(&mut raw);
    let mut buf = [0u8; 32];
    let encoded = Base64Unpadded::encode(&raw, &mut buf).expect("buffer is large enough");
    SaltString::from_b64(encoded).expect("valid base64 salt")
}
