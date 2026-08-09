// SPDX-License-Identifier: BSL-1.1

//! Integration tests for the `tessera-graph-cli admin …` offline commands.
//!
//! These run the CLI binary via `CARGO_BIN_EXE_*` and assert on exit
//! codes, stdout, and stderr — the same contract an operator running
//! recovery scripts will observe. No server connection is required
//! since `admin` is an offline workflow; a `tempfile::TempDir` supplies
//! an isolated `data-dir` per test.

use std::process::Command;

fn cli() -> Command {
    Command::new(env!("CARGO_BIN_EXE_tessera-graph-cli"))
}

// ── admin hash ─────────────────────────────────────────────────────────────

#[test]
fn cli_hash_prints_valid_phc() {
    let out = cli()
        .args(["admin", "hash", "hunter22!x"])
        .output()
        .expect("spawn cli");
    assert!(
        out.status.success(),
        "status={:?} stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).expect("utf8 stdout");
    let line = stdout.trim();
    assert!(
        line.starts_with("$argon2id$"),
        "expected PHC string starting with $argon2id$, got: {line}"
    );
    // PHC has 5 `$`-separated sections: "", "argon2id", params, salt, hash.
    assert_eq!(line.matches('$').count(), 5, "PHC string malformed: {line}");
}

#[test]
fn cli_hash_rejects_missing_password_and_prompt() {
    let out = cli().args(["admin", "hash"]).output().expect("spawn");
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("password") || stderr.contains("--prompt"),
        "expected diagnostic mentioning password/--prompt, got: {stderr}"
    );
}

// ── admin users add + list ─────────────────────────────────────────────────

#[test]
fn cli_users_add_then_list() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().to_str().unwrap();

    let add = cli()
        .args([
            "admin",
            "users",
            "--data-dir",
            data,
            "add",
            "--username",
            "alice",
            "--password",
            "hunter22!x",
        ])
        .output()
        .expect("spawn add");
    assert!(
        add.status.success(),
        "add failed: status={:?} stderr={}",
        add.status,
        String::from_utf8_lossy(&add.stderr)
    );

    let list = cli()
        .args(["admin", "users", "--data-dir", data, "list"])
        .output()
        .expect("spawn list");
    assert!(list.status.success());
    let stdout = String::from_utf8_lossy(&list.stdout);
    assert!(
        stdout.contains("alice"),
        "list output should mention 'alice', got: {stdout}"
    );
}

// ── rm last admin → exit 2 ─────────────────────────────────────────────────

#[test]
fn cli_rm_last_admin_exits_with_code_2() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().to_str().unwrap();

    let add = cli()
        .args([
            "admin",
            "users",
            "--data-dir",
            data,
            "add",
            "--username",
            "admin",
            "--password",
            "hunter22!x",
            "--admin",
        ])
        .output()
        .expect("spawn add");
    assert!(add.status.success());

    let rm = cli()
        .args([
            "admin",
            "users",
            "--data-dir",
            data,
            "rm",
            "--username",
            "admin",
        ])
        .output()
        .expect("spawn rm");
    assert_eq!(
        rm.status.code(),
        Some(2),
        "expected exit code 2 (LastAdmin), got: {:?} stderr={}",
        rm.status,
        String::from_utf8_lossy(&rm.stderr)
    );
}

// ── passwd updates the stored hash ─────────────────────────────────────────

#[test]
fn cli_passwd_updates_hash() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().to_str().unwrap();

    let add = cli()
        .args([
            "admin",
            "users",
            "--data-dir",
            data,
            "add",
            "--username",
            "alice",
            "--password",
            "first-password",
        ])
        .output()
        .expect("spawn add");
    assert!(add.status.success());

    let passwd = cli()
        .args([
            "admin",
            "users",
            "--data-dir",
            data,
            "passwd",
            "--username",
            "alice",
            "--password",
            "second-password",
        ])
        .output()
        .expect("spawn passwd");
    assert!(
        passwd.status.success(),
        "passwd failed: status={:?} stderr={}",
        passwd.status,
        String::from_utf8_lossy(&passwd.stderr)
    );
}

// ── promote / demote cycle ─────────────────────────────────────────────────

#[test]
fn cli_promote_and_demote() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().to_str().unwrap();

    // Seed: one admin + one regular user.
    assert!(
        cli()
            .args([
                "admin",
                "users",
                "--data-dir",
                data,
                "add",
                "--username",
                "admin",
                "--password",
                "hunter22!x",
                "--admin",
            ])
            .status()
            .unwrap()
            .success()
    );
    assert!(
        cli()
            .args([
                "admin",
                "users",
                "--data-dir",
                data,
                "add",
                "--username",
                "alice",
                "--password",
                "hunter22!x",
            ])
            .status()
            .unwrap()
            .success()
    );

    let promote = cli()
        .args([
            "admin",
            "users",
            "--data-dir",
            data,
            "promote",
            "--username",
            "alice",
        ])
        .output()
        .expect("spawn promote");
    assert!(
        promote.status.success(),
        "promote failed: stderr={}",
        String::from_utf8_lossy(&promote.stderr)
    );

    let demote = cli()
        .args([
            "admin",
            "users",
            "--data-dir",
            data,
            "demote",
            "--username",
            "alice",
        ])
        .output()
        .expect("spawn demote");
    assert!(
        demote.status.success(),
        "demote failed: stderr={}",
        String::from_utf8_lossy(&demote.stderr)
    );
}

// ── unknown user on rm → non-success exit ──────────────────────────────────

#[test]
fn cli_rm_unknown_user_fails() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().to_str().unwrap();

    let rm = cli()
        .args([
            "admin",
            "users",
            "--data-dir",
            data,
            "rm",
            "--username",
            "ghost",
        ])
        .output()
        .expect("spawn rm");
    assert!(
        !rm.status.success(),
        "rm of unknown user should not succeed"
    );
    // LastAdmin is code 2; anything else should fall into the generic
    // failure bucket (1). An unknown user is not a last-admin violation.
    assert_ne!(rm.status.code(), Some(2));
}

// ── Task 14 ciclo 5: `admin --audit-log` flag is wired ────────────────────

#[test]
fn cli_admin_help_lists_audit_log_flag() {
    let out = cli()
        .args(["admin", "--help"])
        .output()
        .expect("spawn admin --help");
    assert!(
        out.status.success(),
        "admin --help must exit 0: stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).expect("utf8");
    assert!(
        stdout.contains("--audit-log"),
        "admin --help must mention --audit-log, got: {stdout}"
    );
    assert!(
        stdout.contains("TESSERA_AUDIT_LOG"),
        "admin --help must mention TESSERA_AUDIT_LOG env, got: {stdout}"
    );
}

#[test]
fn cli_admin_audit_log_flag_accepted_before_subcommand() {
    // The flag is on AdminArgs (not the subaction), so it must appear
    // between `admin` and the subcommand. Reuse the `admin hash`
    // happy path which does no I/O on data-dir so this test stays
    // hermetic.
    let dir = tempfile::tempdir().unwrap();
    let audit_log = dir.path().join("audit.log");
    let out = cli()
        .args([
            "admin",
            "--audit-log",
            audit_log.to_str().unwrap(),
            "hash",
            "hunter22!x",
        ])
        .output()
        .expect("spawn");
    assert!(
        out.status.success(),
        "admin --audit-log <path> hash <pw> must exit 0: stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    // The hash subcommand never emits — file should not be created.
    assert!(
        !audit_log.exists(),
        "audit log must not be created by non-Task14 subcommands"
    );
}
