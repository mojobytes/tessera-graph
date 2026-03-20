// Copyright 2026 BelowZero Security OU. All rights reserved.

use std::sync::Arc;

use tessera_auth::credentials::{Password, PasswordPolicy};
use tessera_auth::policy::AuthPolicy;
use tessera_auth::rbac::{Permission, RoleStore, RoleStoreHandle};
use tessera_auth::user::UserStoreHandle;

#[test]
fn permission_check_throughput_regression_guard() {
    let admin_pw = Password::new("Admin@Init1!").unwrap();
    let store = UserStoreHandle::new("admin", &admin_pw, &PasswordPolicy::default()).unwrap();
    store
        .assign_role("admin", RoleStore::ADMIN_ROLE_ID)
        .unwrap();

    let admin_pw2 = Password::new("Admin@Init1!").unwrap();
    let admin_id = store.authenticate("admin", &admin_pw2).unwrap();

    let policy = AuthPolicy::new(Arc::new(store), RoleStoreHandle::with_defaults());

    let iterations = 100_000_u64;
    let start = std::time::Instant::now();
    for _ in 0..iterations {
        policy.check(admin_id, Permission::NodeCreate).unwrap();
    }
    let elapsed = start.elapsed();

    #[allow(clippy::cast_possible_truncation)]
    let elapsed_us = elapsed.as_micros() as u64;
    let ops_per_sec = iterations * 1_000_000 / u64::max(elapsed_us, 1);
    // Debug mode has lock overhead from RwLock on each check.
    // Thresholds account for CI and debug compilation overhead.
    let min_ops = if cfg!(debug_assertions) {
        100_000
    } else {
        5_000_000
    };

    assert!(
        ops_per_sec >= min_ops,
        "Permission check throughput regression: {ops_per_sec} ops/sec (minimum: {min_ops})"
    );
}
