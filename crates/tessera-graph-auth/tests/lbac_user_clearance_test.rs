// Copyright 2026 BelowZero Security OU. All rights reserved.

use std::collections::BTreeSet;
use tessera_graph_auth::credentials::{Password, PasswordPolicy};
use tessera_graph_auth::lbac::Clearance;
use tessera_graph_auth::user::{UserId, UserStoreHandle};

fn comps(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|s| (*s).to_string()).collect()
}

fn make_store() -> UserStoreHandle {
    let pw = Password::new("Admin@Init1!").unwrap();
    UserStoreHandle::new("admin", &pw, &PasswordPolicy::default()).unwrap()
}

#[test]
fn get_clearance_for_user_without_explicit_clearance_returns_default() {
    let store = make_store();
    let pw = Password::new("Admin@Init1!").unwrap();
    let id = store.authenticate("admin", &pw).unwrap();
    let clearance = store.get_clearance(id).unwrap();
    assert_eq!(clearance.level, 0);
    assert!(clearance.compartments.is_empty());
}

#[test]
fn set_and_get_clearance_roundtrips() {
    let store = make_store();
    let pw = Password::new("Admin@Init1!").unwrap();
    let id = store.authenticate("admin", &pw).unwrap();
    let c = comps(&["FINANCE", "HR"]);
    let clearance = Clearance::new(3, c.clone());
    store.set_clearance("admin", clearance).unwrap();
    let retrieved = store.get_clearance(id).unwrap();
    assert_eq!(retrieved.level, 3);
    assert_eq!(retrieved.compartments, c);
}

#[test]
fn set_clearance_for_nonexistent_user_returns_error() {
    let store = make_store();
    let clearance = Clearance::new(1, BTreeSet::new());
    let result = store.set_clearance("ghost", clearance);
    assert!(result.is_err());
}

#[test]
fn get_clearance_for_nonexistent_user_returns_error() {
    let store = make_store();
    let id = UserId::new(9999);
    let result = store.get_clearance(id);
    assert!(result.is_err());
}

#[test]
fn create_user_with_clearance_and_retrieve() {
    let store = make_store();
    let c = comps(&["LEGAL"]);
    let clearance = Clearance::new(2, c.clone());
    let pw = Password::new("User@Pass1!").unwrap();
    let id = store
        .create_user_with_clearance("alice", &pw, vec![], &PasswordPolicy::default(), clearance)
        .unwrap();
    let retrieved = store.get_clearance(id).unwrap();
    assert_eq!(retrieved.level, 2);
    assert_eq!(retrieved.compartments, c);
}
