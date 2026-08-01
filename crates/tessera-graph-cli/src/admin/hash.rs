// SPDX-License-Identifier: LicenseRef-TesseraGraph-Proprietary

//! `tessera-graph-cli admin hash` — compute an argon2id PHC hash.
//!
//! Useful when scripting a bootstrap admin password (for example to
//! pre-populate the system graph with a known hash during testing, or
//! to hash out-of-band and commit the result to a secrets manager).

use tessera_graph_server::auth::{SecretString, hash_password};

/// Run the `admin hash` command.
///
/// # Errors
///
/// Returns `Err(message)` when:
/// * neither a positional password nor `--prompt` is supplied;
/// * reading the interactive password prompt fails (typically a
///   closed stdin on non-interactive terminals);
/// * hashing itself fails (allocation or RNG error — rare).
pub fn run(password: Option<String>, prompt: bool) -> Result<(), String> {
    let plain = resolve_password(password, prompt)?;
    let secret = SecretString::new(plain);
    let phc = hash_password(&secret).map_err(|e| e.to_string())?;
    println!("{phc}");
    Ok(())
}

fn resolve_password(password: Option<String>, prompt: bool) -> Result<String, String> {
    match (password, prompt) {
        (Some(p), _) => Ok(p),
        (None, true) => rpassword::prompt_password("Password: ")
            .map_err(|e| format!("failed to read password: {e}")),
        (None, false) => {
            Err("specify a password as a positional argument or use --prompt".to_owned())
        }
    }
}
