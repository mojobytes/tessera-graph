// SPDX-License-Identifier: BSL-1.1

//! Admin-statement parser tests. Verifies the top-level prefix detection
//! for `CREATE USER` / `DROP USER` / `ALTER USER ...` / `SHOW USERS`,
//! the single-quoted string-literal tokenisation (with `''` escape for
//! a literal single quote), and the `CREATE (` -> regular mutation
//! regression guard.

use ermya_graph::gql::{AdminStatement, GqlStatement};
use ermya_graph_config::QueryLanguage;
use ermya_graph_cypher::parse_with_mode;

fn parse(q: &str) -> GqlStatement {
    parse_with_mode(q, QueryLanguage::CypherCompat).expect("parse")
}

#[test]
fn create_user_parses() {
    let stmt = parse("CREATE USER alice SET PASSWORD 'hunter22!x'");
    match stmt {
        GqlStatement::Admin(AdminStatement::CreateUser { username, password }) => {
            assert_eq!(username, "alice");
            assert_eq!(password.as_bytes(), b"hunter22!x");
        }
        other => panic!("expected CreateUser, got {other:?}"),
    }
}

#[test]
fn drop_user_parses() {
    let stmt = parse("DROP USER alice");
    assert!(matches!(
        stmt,
        GqlStatement::Admin(AdminStatement::DropUser { .. })
    ));
}

#[test]
fn alter_user_set_password_parses() {
    let stmt = parse("ALTER USER alice SET PASSWORD 'newsecret!!'");
    assert!(matches!(
        stmt,
        GqlStatement::Admin(AdminStatement::AlterUserPassword { .. })
    ));
}

#[test]
fn alter_user_set_status_active_parses() {
    let stmt = parse("ALTER USER alice SET STATUS ACTIVE");
    match stmt {
        GqlStatement::Admin(AdminStatement::AlterUserStatus { enabled, .. }) => assert!(enabled),
        other => panic!("expected AlterUserStatus, got {other:?}"),
    }
}

#[test]
fn alter_user_set_status_suspended_parses() {
    let stmt = parse("ALTER USER alice SET STATUS SUSPENDED");
    match stmt {
        GqlStatement::Admin(AdminStatement::AlterUserStatus { enabled, .. }) => assert!(!enabled),
        other => panic!("expected AlterUserStatus, got {other:?}"),
    }
}

#[test]
fn alter_user_set_admin_true_parses() {
    let stmt = parse("ALTER USER alice SET ADMIN TRUE");
    match stmt {
        GqlStatement::Admin(AdminStatement::AlterUserAdmin { is_admin, .. }) => assert!(is_admin),
        other => panic!("expected AlterUserAdmin, got {other:?}"),
    }
}

#[test]
fn show_users_parses() {
    let stmt = parse("SHOW USERS");
    assert!(matches!(
        stmt,
        GqlStatement::Admin(AdminStatement::ShowUsers)
    ));
}

#[test]
fn create_paren_still_parses_as_mutation() {
    // Regression: `CREATE (n:Foo)` must NOT be routed through the admin
    // parser.
    let stmt = parse("CREATE (n:Foo)");
    assert!(matches!(stmt, GqlStatement::Mutation(_)));
}

#[test]
fn password_with_special_chars_parses() {
    let stmt = parse("CREATE USER alice SET PASSWORD 'p@ss w0rd!'");
    match stmt {
        GqlStatement::Admin(AdminStatement::CreateUser { password, .. }) => {
            assert_eq!(password.as_bytes(), b"p@ss w0rd!");
        }
        other => panic!("expected CreateUser, got {other:?}"),
    }
}

#[test]
fn password_with_escaped_quote_parses() {
    // Single-quoted strings escape a literal single quote by doubling it
    // (SQL / Neo4j convention).
    let stmt = parse("CREATE USER alice SET PASSWORD 'foo''bar'");
    match stmt {
        GqlStatement::Admin(AdminStatement::CreateUser { password, .. }) => {
            assert_eq!(password.as_bytes(), b"foo'bar");
        }
        other => panic!("expected CreateUser, got {other:?}"),
    }
}

// ─────────────────────────────────────────────────────────────────────
// Task 6 — CREATE/DROP/SHOW DATABASE admin statements
// ─────────────────────────────────────────────────────────────────────

use ermya_graph::gql::DatabaseOptions;

fn parse_err(q: &str) -> ermya_graph::Error {
    parse_with_mode(q, QueryLanguage::CypherCompat).expect_err("expected parse error")
}

#[test]
fn parse_create_database_minimal() {
    let stmt = parse("CREATE DATABASE plantA");
    match stmt {
        GqlStatement::Admin(AdminStatement::CreateDatabase {
            name,
            if_not_exists,
            options,
        }) => {
            assert_eq!(name, "plantA");
            assert!(!if_not_exists);
            assert_eq!(options, DatabaseOptions::default());
        }
        other => panic!("expected CreateDatabase, got {other:?}"),
    }
}

#[test]
fn parse_create_database_if_not_exists() {
    let stmt = parse("CREATE DATABASE plantA IF NOT EXISTS");
    match stmt {
        GqlStatement::Admin(AdminStatement::CreateDatabase {
            name,
            if_not_exists,
            options,
        }) => {
            assert_eq!(name, "plantA");
            assert!(if_not_exists);
            assert_eq!(options, DatabaseOptions::default());
        }
        other => panic!("expected CreateDatabase, got {other:?}"),
    }
}

#[test]
fn parse_create_database_with_options() {
    let stmt = parse(
        "CREATE DATABASE plantA WITH OPTIONS { max_size_bytes: 1073741824, max_connections: 50 }",
    );
    match stmt {
        GqlStatement::Admin(AdminStatement::CreateDatabase { name, options, .. }) => {
            assert_eq!(name, "plantA");
            assert_eq!(options.max_size_bytes, Some(1_073_741_824));
            assert_eq!(options.max_connections, Some(50));
        }
        other => panic!("expected CreateDatabase, got {other:?}"),
    }
}

#[test]
fn parse_create_database_if_not_exists_with_options() {
    let stmt = parse("CREATE DATABASE plantA IF NOT EXISTS WITH OPTIONS { max_connections: 8 }");
    match stmt {
        GqlStatement::Admin(AdminStatement::CreateDatabase {
            name,
            if_not_exists,
            options,
        }) => {
            assert_eq!(name, "plantA");
            assert!(if_not_exists);
            assert_eq!(options.max_size_bytes, None);
            assert_eq!(options.max_connections, Some(8));
        }
        other => panic!("expected CreateDatabase, got {other:?}"),
    }
}

#[test]
fn parse_drop_database() {
    let stmt = parse("DROP DATABASE plantA");
    match stmt {
        GqlStatement::Admin(AdminStatement::DropDatabase { name, if_exists }) => {
            assert_eq!(name, "plantA");
            assert!(!if_exists);
        }
        other => panic!("expected DropDatabase, got {other:?}"),
    }
}

#[test]
fn parse_drop_database_if_exists() {
    let stmt = parse("DROP DATABASE plantA IF EXISTS");
    match stmt {
        GqlStatement::Admin(AdminStatement::DropDatabase { name, if_exists }) => {
            assert_eq!(name, "plantA");
            assert!(if_exists);
        }
        other => panic!("expected DropDatabase, got {other:?}"),
    }
}

#[test]
fn parse_show_databases() {
    let stmt = parse("SHOW DATABASES");
    assert!(matches!(
        stmt,
        GqlStatement::Admin(AdminStatement::ShowDatabases)
    ));
}

#[test]
fn parse_rejects_reserved_name_system() {
    let err = parse_err("CREATE DATABASE system");
    let msg = format!("{err}");
    assert!(
        msg.contains("reserved"),
        "expected 'reserved' in error, got: {msg}"
    );
}

#[test]
fn parse_rejects_invalid_name_starts_with_digit() {
    let err = parse_err("CREATE DATABASE 1plant");
    let msg = format!("{err}");
    assert!(
        msg.contains("invalid"),
        "expected 'invalid' in error, got: {msg}"
    );
}

#[test]
fn parse_rejects_name_with_special_chars() {
    let err = parse_err("CREATE DATABASE plant.A");
    let msg = format!("{err}");
    assert!(
        msg.contains("invalid"),
        "expected 'invalid' in error, got: {msg}"
    );
}

#[test]
fn parse_drop_database_rejects_trailing_garbage() {
    let err = parse_err("DROP DATABASE plantA FOO BAR");
    let msg = format!("{err}");
    assert!(
        msg.contains("unexpected") || msg.contains("trailing"),
        "expected unexpected/trailing in error, got: {msg}"
    );
}

#[test]
fn parse_create_database_rejects_unknown_option() {
    let err = parse_err("CREATE DATABASE plantA WITH OPTIONS { weird_key: 1 }");
    let msg = format!("{err}");
    assert!(
        msg.contains("unknown option") || msg.contains("weird_key"),
        "expected 'unknown option' in error, got: {msg}"
    );
}

// ─────────────────────────────────────────────────────────────────────
// Task 7 — GRANT/REVOKE/SHOW GRANTS admin statements
// ─────────────────────────────────────────────────────────────────────

use ermya_graph::gql::{AccessLevelAst, GrantTargetAst};

#[test]
fn parse_grant_access_named() {
    let stmt = parse("GRANT ACCESS ON DATABASE plantA TO alice");
    match stmt {
        GqlStatement::Admin(AdminStatement::Grant {
            username,
            target,
            level,
        }) => {
            assert_eq!(username, "alice");
            assert!(
                matches!(target, GrantTargetAst::Named(ref n) if n == "plantA"),
                "expected Named(plantA), got {target:?}"
            );
            assert_eq!(level, AccessLevelAst::Read);
        }
        other => panic!("expected Grant, got {other:?}"),
    }
}

#[test]
fn parse_grant_access_wildcard() {
    let stmt = parse("GRANT ACCESS ON DATABASE * TO alice");
    match stmt {
        GqlStatement::Admin(AdminStatement::Grant {
            username,
            target,
            level,
        }) => {
            assert_eq!(username, "alice");
            assert!(matches!(target, GrantTargetAst::Wildcard));
            assert_eq!(level, AccessLevelAst::Read);
        }
        other => panic!("expected Grant, got {other:?}"),
    }
}

#[test]
fn parse_grant_write() {
    let stmt = parse("GRANT WRITE ON DATABASE plantA TO alice");
    match stmt {
        GqlStatement::Admin(AdminStatement::Grant { level, .. }) => {
            assert_eq!(level, AccessLevelAst::ReadWrite);
        }
        other => panic!("expected Grant, got {other:?}"),
    }
}

#[test]
fn parse_grant_write_wildcard() {
    let stmt = parse("GRANT WRITE ON DATABASE * TO alice");
    match stmt {
        GqlStatement::Admin(AdminStatement::Grant { target, level, .. }) => {
            assert!(matches!(target, GrantTargetAst::Wildcard));
            assert_eq!(level, AccessLevelAst::ReadWrite);
        }
        other => panic!("expected Grant, got {other:?}"),
    }
}

#[test]
fn parse_revoke_named() {
    let stmt = parse("REVOKE ACCESS ON DATABASE plantA FROM alice");
    match stmt {
        GqlStatement::Admin(AdminStatement::Revoke { username, target }) => {
            assert_eq!(username, "alice");
            assert!(
                matches!(target, GrantTargetAst::Named(ref n) if n == "plantA"),
                "expected Named(plantA), got {target:?}"
            );
        }
        other => panic!("expected Revoke, got {other:?}"),
    }
}

#[test]
fn parse_revoke_wildcard() {
    let stmt = parse("REVOKE ACCESS ON DATABASE * FROM alice");
    match stmt {
        GqlStatement::Admin(AdminStatement::Revoke { target, .. }) => {
            assert!(matches!(target, GrantTargetAst::Wildcard));
        }
        other => panic!("expected Revoke, got {other:?}"),
    }
}

#[test]
fn parse_show_grants() {
    let stmt = parse("SHOW GRANTS");
    assert!(matches!(
        stmt,
        GqlStatement::Admin(AdminStatement::ShowGrants { filter_user: None })
    ));
}

#[test]
fn parse_show_grants_for_user() {
    let stmt = parse("SHOW GRANTS FOR alice");
    match stmt {
        GqlStatement::Admin(AdminStatement::ShowGrants { filter_user }) => {
            assert_eq!(filter_user.as_deref(), Some("alice"));
        }
        other => panic!("expected ShowGrants, got {other:?}"),
    }
}

#[test]
fn parse_grant_rejects_invalid_level() {
    let err = parse_err("GRANT ADMIN ON DATABASE plantA TO alice");
    let msg = format!("{err}");
    assert!(
        msg.to_ascii_lowercase().contains("access") || msg.to_ascii_lowercase().contains("write"),
        "expected guidance about ACCESS/WRITE, got: {msg}"
    );
}

#[test]
fn parse_grant_rejects_missing_database_keyword() {
    let err = parse_err("GRANT ACCESS ON plantA TO alice");
    let msg = format!("{err}");
    assert!(
        msg.to_ascii_uppercase().contains("DATABASE"),
        "expected DATABASE in error, got: {msg}"
    );
}

#[test]
fn parse_grant_rejects_missing_to_clause() {
    let err = parse_err("GRANT ACCESS ON DATABASE plantA");
    let msg = format!("{err}");
    assert!(
        msg.to_ascii_uppercase().contains("TO"),
        "expected TO in error, got: {msg}"
    );
}

#[test]
fn parse_revoke_rejects_missing_from_clause() {
    let err = parse_err("REVOKE ACCESS ON DATABASE plantA alice");
    let msg = format!("{err}");
    assert!(
        msg.to_ascii_uppercase().contains("FROM"),
        "expected FROM in error, got: {msg}"
    );
}

#[test]
fn parse_grant_rejects_reserved_target_name() {
    let err = parse_err("GRANT ACCESS ON DATABASE system TO alice");
    let msg = format!("{err}");
    assert!(
        msg.contains("reserved"),
        "expected 'reserved' in error, got: {msg}"
    );
}

#[test]
fn parse_show_grants_rejects_trailing_garbage() {
    let err = parse_err("SHOW GRANTS FOR alice and bob");
    let msg = format!("{err}");
    assert!(
        msg.to_ascii_lowercase().contains("trailing")
            || msg.to_ascii_lowercase().contains("unexpected"),
        "expected trailing/unexpected in error, got: {msg}"
    );
}
