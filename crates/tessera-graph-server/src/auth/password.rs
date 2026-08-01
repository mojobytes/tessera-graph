// SPDX-License-Identifier: BSL-1.1

//! argon2id password hashing with OWASP 2024 parameters.
//!
//! The hash format is the canonical PHC string
//! (`$argon2id$v=19$m=19456,t=2,p=1$<salt>$<hash>`), so the tuning can be
//! raised in the future without invalidating existing hashes: each stored
//! hash carries its own params.

use argon2::password_hash::{
    PasswordHash, PasswordHasher, PasswordVerifier, SaltString, rand_core::OsRng,
};
use argon2::{Algorithm, Argon2, Params, Version};

use super::SecretString;

/// Minimum accepted plaintext length, in bytes.
pub const MIN_PASSWORD_LEN: usize = 8;

/// Maximum accepted plaintext length, in bytes. Serves as a `DoS` guard:
/// argon2 on a multi-MB input would block the async runtime for seconds.
pub const MAX_PASSWORD_LEN: usize = 1024;

/// Argon2id parameters — OWASP Cheat Sheet 2024, second recommended option.
///
/// `m = 19456 KiB` (~19 MiB memory cost), `t = 2`, `p = 1`. Produces
/// ~50-100 ms per verification on typical server hardware.
fn argon2() -> Argon2<'static> {
    let params = Params::new(19_456, 2, 1, None)
        .expect("compile-time parameters must be valid");
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
}

#[derive(Debug, thiserror::Error)]
pub enum PasswordError {
    #[error("password hash error: {0}")]
    Hash(String),
    #[error("password too long (max {MAX_PASSWORD_LEN} bytes)")]
    TooLong,
}

/// Hash a plaintext password and return its canonical PHC-encoded form.
///
/// # Errors
///
/// Returns [`PasswordError::TooLong`] if the plaintext exceeds
/// [`MAX_PASSWORD_LEN`], or [`PasswordError::Hash`] on any argon2 failure
/// (including `OsRng` failure).
pub fn hash_password(plain: &SecretString) -> Result<String, PasswordError> {
    if plain.len() > MAX_PASSWORD_LEN {
        return Err(PasswordError::TooLong);
    }
    let salt = SaltString::generate(&mut OsRng);
    let hasher = argon2();
    let phc = hasher
        .hash_password(plain.as_bytes(), &salt)
        .map_err(|e| PasswordError::Hash(e.to_string()))?
        .to_string();
    Ok(phc)
}

/// Verify a plaintext password against a stored PHC-encoded hash.
///
/// Runs in constant time with respect to the compared hash bytes. A
/// malformed PHC string is reported as [`PasswordError::Hash`], not as a
/// boolean mismatch — callers that want to mask the distinction at the
/// API boundary should convert on their side.
///
/// # Errors
///
/// Returns [`PasswordError::Hash`] if `phc` is malformed or the algorithm
/// identifier is unknown. A valid hash with a non-matching password
/// returns `Ok(false)`.
pub fn verify_password(plain: &SecretString, phc: &str) -> Result<bool, PasswordError> {
    let parsed = PasswordHash::new(phc).map_err(|e| PasswordError::Hash(e.to_string()))?;
    match argon2().verify_password(plain.as_bytes(), &parsed) {
        Ok(()) => Ok(true),
        Err(argon2::password_hash::Error::Password) => Ok(false),
        Err(e) => Err(PasswordError::Hash(e.to_string())),
    }
}
