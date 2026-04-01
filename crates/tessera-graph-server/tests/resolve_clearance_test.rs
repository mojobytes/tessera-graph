// Copyright 2026 BelowZero Security OU. All rights reserved.

mod common;

use tessera_graph_auth::lbac::Clearance;
use tessera_graph_auth::session::SessionToken;

#[test]
fn resolve_clearance_returns_default_for_valid_session() {
    let (_dir, ctx) = common::test_context();
    let user_id = ctx
        .user_store()
        .authenticate(
            "admin",
            &tessera_graph_auth::credentials::Password::new("Admin@Init1!").unwrap(),
        )
        .unwrap();
    let token = ctx.sessions().create_session(user_id).unwrap();
    let clearance = ctx.resolve_clearance(&token).unwrap();
    assert_eq!(clearance, Clearance::default());
}

#[test]
fn resolve_clearance_returns_custom_clearance_after_set() {
    let (_dir, ctx) = common::test_context();
    let comps = ["FINANCE", "HR"].iter().map(|s| (*s).to_string()).collect();
    let custom = Clearance::new(5, comps);
    ctx.user_store()
        .set_clearance("admin", custom.clone())
        .unwrap();
    let user_id = ctx
        .user_store()
        .authenticate(
            "admin",
            &tessera_graph_auth::credentials::Password::new("Admin@Init1!").unwrap(),
        )
        .unwrap();
    let token = ctx.sessions().create_session(user_id).unwrap();
    let clearance = ctx.resolve_clearance(&token).unwrap();
    assert_eq!(clearance.level, 5);
    assert_eq!(clearance.compartments, custom.compartments);
}

#[test]
fn resolve_clearance_fails_for_invalid_token() {
    let (_dir, ctx) = common::test_context();
    let bad_token = SessionToken::from_raw("totally-invalid-token".to_string());
    let result = ctx.resolve_clearance(&bad_token);
    assert!(result.is_err(), "must deny invalid tokens");
}
