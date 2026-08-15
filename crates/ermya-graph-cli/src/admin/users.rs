// SPDX-License-Identifier: BSL-1.1

//! `ermya-graph-cli admin users …` — offline user management.
//!
//! Opens the on-disk system graph at `{data-dir}/system/`, acquires an
//! exclusive `fs2` advisory lock at `system.lock` (same contract as the
//! server's [`startup::open_system_graph`]), and dispatches the
//! requested `UsersSub` action against [`SystemGraphAuthStore`].
//!
//! Exit code contract:
//! * `0` — success
//! * `1` — generic failure (I/O, parsing, unknown user, store error)
//! * `2` — [`AuthStoreError::LastAdmin`]: removing / demoting would
//!   leave the system without an admin account. Operators scripting
//!   recovery rely on this exit code to detect the condition without
//!   parsing stderr.
//! * `3` — lock contended: another process (usually the server) holds
//!   the system-graph lock. The operator must stop the server before
//!   running offline admin commands.

use ermya_graph_server::auth::{AuthStoreError, SecretString, SystemGraphAuthStore, UserStore};

use crate::admin::{AdminResult, open_locked_store};
use crate::cli::{UserMutArgs, UsersArgs, UsersSub};

/// Dispatch the requested `users` action. All variants acquire the
/// system-graph lock before opening the store, so running this while
/// the server is live will fail fast with exit code `3`.
///
/// # Errors
///
/// Returns `Err((exit_code, message))` following the module-level
/// contract: `1` for generic failures, `2` for last-admin protection,
/// `3` for lock contention.
pub async fn run(args: UsersArgs) -> AdminResult {
    let locked = open_locked_store("admin users", &args.data_dir)?;
    let store = &locked.store;

    match args.action {
        UsersSub::List => list(store).await,
        UsersSub::Add(a) => add(store, a).await,
        UsersSub::Rm(r) => {
            store
                .drop_user(&r.username)
                .await
                .map_err(|e| map_store_error(&e))?;
            println!("user '{}' removed", r.username);
            Ok(())
        }
        UsersSub::Passwd(a) => passwd(store, a).await,
        UsersSub::Enable(r) => {
            store
                .set_enabled(&r.username, true)
                .await
                .map_err(|e| map_store_error(&e))?;
            println!("user '{}' enabled", r.username);
            Ok(())
        }
        UsersSub::Disable(r) => {
            store
                .set_enabled(&r.username, false)
                .await
                .map_err(|e| map_store_error(&e))?;
            println!("user '{}' disabled", r.username);
            Ok(())
        }
        UsersSub::Promote(r) => {
            store
                .set_admin(&r.username, true)
                .await
                .map_err(|e| map_store_error(&e))?;
            println!("user '{}' promoted to admin", r.username);
            Ok(())
        }
        UsersSub::Demote(r) => {
            store
                .set_admin(&r.username, false)
                .await
                .map_err(|e| map_store_error(&e))?;
            println!("user '{}' demoted from admin", r.username);
            Ok(())
        }
    }
}

async fn list(store: &SystemGraphAuthStore) -> AdminResult {
    let users = store.list_users().await.map_err(|e| map_store_error(&e))?;
    for u in users {
        println!(
            "{}\t{}\t{}\t{}",
            u.username, u.enabled, u.is_admin, u.created_at
        );
    }
    Ok(())
}

async fn add(store: &SystemGraphAuthStore, args: UserMutArgs) -> AdminResult {
    let password = read_password(&args)?;
    store
        .create_user(&args.username, &password, args.admin)
        .await
        .map_err(|e| map_store_error(&e))?;
    println!(
        "user '{}' created{}",
        args.username,
        if args.admin { " (admin)" } else { "" }
    );
    Ok(())
}

async fn passwd(store: &SystemGraphAuthStore, args: UserMutArgs) -> AdminResult {
    if args.admin {
        // `--admin` on `passwd` is meaningless and silently ignoring it
        // would hide operator mistakes; fail fast instead.
        return Err((
            1,
            "`--admin` is not valid for `passwd`; use `promote` instead".to_owned(),
        ));
    }
    let password = read_password(&args)?;
    store
        .set_password(&args.username, &password)
        .await
        .map_err(|e| map_store_error(&e))?;
    println!("password updated for '{}'", args.username);
    Ok(())
}

fn read_password(args: &UserMutArgs) -> Result<SecretString, (i32, String)> {
    let raw = match (args.password.as_ref(), args.prompt) {
        (Some(p), _) => p.clone(),
        (None, true) => rpassword::prompt_password("Password: ")
            .map_err(|e| (1, format!("failed to read password: {e}")))?,
        (None, false) => {
            return Err((1, "specify --password or --prompt".to_owned()));
        }
    };
    Ok(SecretString::new(raw))
}

/// Local override of [`crate::admin::map_store_error`]: keeps the
/// generic exit-1 mapping for everything except [`AuthStoreError::LastAdmin`],
/// which maps to exit-2 per the module's exit-code contract.
fn map_store_error(e: &AuthStoreError) -> (i32, String) {
    let code = match e {
        AuthStoreError::LastAdmin => 2,
        _ => 1,
    };
    (code, e.to_string())
}
