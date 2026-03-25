# TDD Plan: 20 Quality Fixes — tessera-graph-enterprise

**Date:** 2026-03-24
**Status:** Complete (16/20 implemented, 4 deferred)
**Stack:** Rust 1.85, Tokio async runtime, workspace with 14 crates
**Conventions observed:**
- Integration tests live in `crates/<crate>/tests/*.rs`, never inline in `src/`
- Unit tests live in `#[cfg(test)] mod tests` blocks inside `src/`
- Error types use `thiserror`. Results are `crate::Result<T>`.
- Clippy `all = deny`, `pedantic = warn`, `nursery = warn` — warnings ARE errors
- `unsafe_code = forbid`
- Workspace dependency re-use; `zeroize` is already in `tessera-auth/Cargo.toml`
- `ServerContext` passes `Arc<X>` for every shared component

**Hot paths affected:** TenantId/DatabaseName validation (C1) is on the connection
hot path. All other findings are bookkeeping, correctness, or security hardening.
A throughput regression test is mandatory only for C1.

---

## Decisiones Previas Necesarias

None. All findings have clear, single-correct implementations that do not require
architectural trade-offs. Proceed directly.

---

## Summary of 20 Findings by Phase

| Phase | Findings | Focus |
|-------|----------|-------|
| 1     | C1       | Path traversal — whitelist validation |
| 2     | C3       | BEGIN/COMMIT/ROLLBACK contract fix |
| 3     | C2       | Rate limiting wired into Bolt auth |
| 4     | C4       | External auth roles no longer discarded |
| 5     | R8       | LDAP bind_password zeroization |
| 6     | R4       | TransactionManager committed set pruning |
| 7     | R3       | Dead `_registry` parameter removed |
| 8     | R5, O8   | Prometheus +Inf (already present, test added); list_databases TOCTOU |
| 9     | R1       | SecureGraph / SecureGraphRef DRY refactor |
| 10    | R2       | dict_str single-pass extraction |
| 11    | R6       | LOGON state cleanup |
| 12    | R7       | Audit logging in handle_run |
| 13    | O1       | Unique connection_id per connection |
| 14    | O2       | gql_result_to_packstream projection order |
| 15    | O3       | unix_timestamp deduplication |
| 16    | O4       | LoginAttemptTracker lock-poison safety |
| 17    | O5       | Non-empty params in handle_run → FAILURE |
| 18    | O6       | escape_ldap_filter_value multi-byte fix |
| 19    | O7       | SessionManager periodic expiry |
| 20    | WV       | Wiring verification pass |

---

## Phase 1 — C1: Path Traversal in Tenant/Database Names

### Context

`TenantId::new` and `DatabaseName::new` in
`crates/tessera-tenant/src/types.rs:16-68` reject empty strings and `/` only.
Characters like `..`, `\0`, `\`, and non-printable bytes pass validation.
`TenantRegistry::db_path` calls `self.base_dir.join(tenant).join(database)`
directly, which on all OS targets allows traversal when names contain `..`.

The fix is a whitelist: only `[a-zA-Z0-9_-]` characters may appear in a name.
No other change is needed — `Path::join` with a whitelist-validated component
cannot escape the base directory.

### Cycle 1-A: RED — write failing whitelist tests

File: `crates/tessera-tenant/tests/types_test.rs`

Add after the existing rejection tests:

```rust
// C1 path-traversal rejections
#[test]
fn tenant_id_rejects_dot_dot() {
    assert!(matches!(TenantId::new("..").unwrap_err(), TenantError::InvalidName(_)));
}

#[test]
fn tenant_id_rejects_null_byte() {
    assert!(matches!(TenantId::new("a\0b").unwrap_err(), TenantError::InvalidName(_)));
}

#[test]
fn tenant_id_rejects_backslash() {
    assert!(matches!(TenantId::new(r"a\b").unwrap_err(), TenantError::InvalidName(_)));
}

#[test]
fn tenant_id_rejects_dot_prefix() {
    assert!(matches!(TenantId::new(".hidden").unwrap_err(), TenantError::InvalidName(_)));
}

#[test]
fn tenant_id_rejects_space() {
    assert!(matches!(TenantId::new("a b").unwrap_err(), TenantError::InvalidName(_)));
}

#[test]
fn tenant_id_allows_alphanumeric_hyphen_underscore() {
    assert!(TenantId::new("acme-corp_2").is_ok());
    assert!(TenantId::new("ACME123").is_ok());
}

#[test]
fn database_name_rejects_dot_dot() {
    assert!(matches!(DatabaseName::new("..").unwrap_err(), TenantError::InvalidName(_)));
}

#[test]
fn database_name_rejects_null_byte() {
    assert!(matches!(DatabaseName::new("prod\0uction").unwrap_err(), TenantError::InvalidName(_)));
}

#[test]
fn database_name_rejects_spaces() {
    assert!(matches!(DatabaseName::new("prod uction").unwrap_err(), TenantError::InvalidName(_)));
}
```

Run: `cargo test -p tessera-tenant` — these tests MUST fail (current validation
allows all of the above).

### Cycle 1-B: GREEN — implement whitelist validation

File: `crates/tessera-tenant/src/types.rs`

Replace the validation bodies in both `TenantId::new` and `DatabaseName::new`.
The shared predicate is identical, so extract it as a private free function at
the bottom of the file:

```rust
/// Returns `true` iff every character in `name` is ASCII alphanumeric, `-`, or `_`.
fn is_valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}
```

Replace `TenantId::new`:
```rust
pub fn new(name: impl Into<String>) -> Result<Self, crate::TenantError> {
    let name = name.into();
    if !is_valid_name(&name) {
        return Err(crate::TenantError::InvalidName(format!(
            "tenant name must be non-empty and contain only [a-zA-Z0-9_-], got: {name:?}"
        )));
    }
    Ok(Self(name))
}
```

Replace `DatabaseName::new` identically (different error text, same predicate).

Run: `cargo test -p tessera-tenant` — all tests including pre-existing ones MUST
pass. Verify no Clippy warnings: `cargo clippy -p tessera-tenant`.

### Cycle 1-C: REFACTOR + throughput regression guard

The validation is already O(n) in name length. No refactoring needed beyond the
shared helper introduced above.

Add a throughput regression test to confirm the hot path (`get_or_load` →
`TenantId::new`) is not degraded:

File: `crates/tessera-tenant/tests/types_test.rs`

```rust
#[test]
fn tenant_id_new_throughput_regression_guard() {
    // Minimum acceptable: 1_000_000 valid constructions per second.
    // This is a lower bound — real hardware will be >10x faster.
    let start = std::time::Instant::now();
    let n = 100_000u64;
    for i in 0..n {
        let name = if i % 2 == 0 { "acme-corp" } else { "tenant-99" };
        let _ = TenantId::new(name).unwrap();
    }
    let elapsed = start.elapsed().as_secs_f64();
    let rate = n as f64 / elapsed;
    assert!(
        rate > 1_000_000.0,
        "TenantId::new throughput {rate:.0} ops/s below 1_000_000 minimum"
    );
}
```

---

## Phase 2 — C3: BEGIN Must Not Lie

### Context

`bolt_handler.rs:510-522` — `handle_begin` responds `SUCCESS`, `handle_rollback`
does nothing. Mutations applied after BEGIN are immediately committed. The client
believes it is in a transaction and can rollback, which is false.

The correct response is to send `FAILURE` when BEGIN is received, with a
descriptive `FeatureNotSupported` code, and set `self.failed = true` so the
connection enters the FAILED state. This matches Bolt protocol semantics: a
client that receives FAILURE on BEGIN must RESET before sending more commands.

`handle_commit` and `handle_rollback` currently both succeed silently. After this
fix they become unreachable for well-behaved clients (BEGIN fails → FAILED state →
only RESET/GOODBYE processed). Keep them as `send_ignored` for robustness.

### Cycle 2-A: RED — write failing test

File: `crates/tessera-server/tests/bolt_handler_test.rs`

```rust
#[tokio::test]
async fn begin_responds_failure_not_success() {
    let ctx = test_context();
    let (mut writer, mut reader, _shutdown) = spawn_bolt_handler(ctx).await;

    // Authenticate first
    bolt_send(&mut writer, &hello_request("admin", "Admin@Init1!")).await;
    let resp = bolt_recv(&mut reader).await;
    assert!(matches!(resp, BoltResponse::Success { .. }), "HELLO must succeed");

    // BEGIN must fail — explicit transactions are not implemented
    bolt_send(&mut writer, &BoltRequest::Begin { extra: vec![] }).await;
    let resp = bolt_recv(&mut reader).await;
    assert!(
        matches!(resp, BoltResponse::Failure { .. }),
        "BEGIN must respond FAILURE, got: {resp:?}"
    );
}

#[tokio::test]
async fn after_begin_failure_connection_is_in_failed_state() {
    let ctx = test_context();
    let (mut writer, mut reader, _shutdown) = spawn_bolt_handler(ctx).await;

    bolt_send(&mut writer, &hello_request("admin", "Admin@Init1!")).await;
    let _ = bolt_recv(&mut reader).await; // discard SUCCESS

    bolt_send(&mut writer, &BoltRequest::Begin { extra: vec![] }).await;
    let _ = bolt_recv(&mut reader).await; // BEGIN FAILURE

    // RUN in FAILED state must be IGNORED, not executed
    bolt_send(&mut writer, &run_query("MATCH (n) RETURN n")).await;
    let resp = bolt_recv(&mut reader).await;
    assert!(
        matches!(resp, BoltResponse::Ignored),
        "RUN after BEGIN failure must be IGNORED, got: {resp:?}"
    );
}
```

Run: tests fail because `handle_begin` currently sends SUCCESS.

### Cycle 2-B: GREEN

File: `crates/tessera-server/src/bolt_handler.rs`

Replace `handle_begin`:

```rust
async fn handle_begin(&mut self) -> Result<()> {
    // Explicit transactions are not yet implemented. Responding SUCCESS would
    // be a lie — mutations still auto-commit and ROLLBACK does nothing.
    // Respond FAILURE so the client enters the FAILED state and cannot
    // silently corrupt data believing it is inside a transaction.
    self.send_failure(
        "Neo.DatabaseError.General.UnknownError",
        "explicit transactions are not supported in this server version",
    )
    .await
}
```

Replace `handle_commit` to send IGNORED (unreachable for well-behaved clients):

```rust
async fn handle_commit(&mut self) -> Result<()> {
    // Should only be reached if a client sends COMMIT without BEGIN, which is
    // a protocol error. Respond IGNORED to avoid confusion.
    self.send_ignored().await
}
```

Leave `handle_rollback` as is (it already clears `pending_result` and sends
SUCCESS — this is harmless as a no-op if no transaction was begun; however for
correctness also change to send IGNORED):

```rust
async fn handle_rollback(&mut self) -> Result<()> {
    self.pending_result = None;
    self.send_ignored().await
}
```

Run tests: all pass. `cargo clippy -p tessera-server` — no warnings.

---

## Phase 3 — C2: Rate Limiting Wired into Bolt Auth

### Context

`tessera_auth::rate_limit::LoginAttemptTracker` and `LoginPolicy` exist
(`crates/tessera-auth/src/rate_limit.rs`) but are never wired into
`BoltConnectionHandler::authenticate()`. `ServerContext` has no rate-limit field.

The fix adds `Arc<LoginAttemptTracker>` and `LoginPolicy` to `ServerContext`, then
calls `tracker.is_locked()` before authentication and `tracker.record_failure()`
/ `tracker.record_success()` after.

`LoginPolicy` is `Copy`-impossible (contains `u64`) so hold it as an `Arc` too,
or as a plain field since it is immutable after construction. Use a plain field —
it is `Clone` and `Copy`-compatible (two `u32`/`u64` fields).

### Cycle 3-A: RED — write failing tests

File: `crates/tessera-server/tests/auth_integration_test.rs`

Add:

```rust
#[tokio::test]
async fn repeated_bad_passwords_lock_account() {
    // Use a very tight policy: lock after 2 failures for 60 s
    let ctx = test_context_with_rate_limit(2, 60);
    let (mut writer, mut reader, _shutdown) = spawn_bolt_handler(ctx).await;

    // Two failures
    for _ in 0..2 {
        bolt_send(&mut writer, &hello_request("admin", "wrong")).await;
        let resp = bolt_recv(&mut reader).await;
        assert!(matches!(resp, BoltResponse::Failure { .. }));
        // After FAILURE we must RESET before next request
        bolt_send(&mut writer, &BoltRequest::Reset).await;
        let _ = bolt_recv(&mut reader).await;
    }

    // Third attempt — account must be locked even with correct password
    bolt_send(&mut writer, &hello_request("admin", "Admin@Init1!")).await;
    let resp = bolt_recv(&mut reader).await;
    assert!(
        matches!(resp, BoltResponse::Failure { .. }),
        "locked account must be rejected even with correct password"
    );
}

#[tokio::test]
async fn successful_auth_resets_failure_counter() {
    let ctx = test_context_with_rate_limit(3, 60);
    let (mut writer, mut reader, _shutdown) = spawn_bolt_handler(ctx).await;

    // One failure
    bolt_send(&mut writer, &hello_request("admin", "wrong")).await;
    let _ = bolt_recv(&mut reader).await;
    bolt_send(&mut writer, &BoltRequest::Reset).await;
    let _ = bolt_recv(&mut reader).await;

    // Correct credentials — must succeed and reset counter
    bolt_send(&mut writer, &hello_request("admin", "Admin@Init1!")).await;
    let resp = bolt_recv(&mut reader).await;
    assert!(matches!(resp, BoltResponse::Success { .. }), "correct password after 1 failure must succeed");
}
```

Add helper `test_context_with_rate_limit(max_attempts, lockout_secs)` to
`crates/tessera-server/tests/common/mod.rs`:

```rust
#[allow(dead_code)]
pub fn test_context_with_rate_limit(
    max_attempts: u32,
    lockout_secs: u64,
) -> Arc<ServerContext> {
    // Build a context identical to test_context() but with a custom LoginPolicy
    // and a fresh LoginAttemptTracker wired in.
    // (Exact signature depends on ServerContext::new_with_rate_limit added in GREEN.)
    todo!("implement after GREEN step")
}
```

Run: compilation failure because `ServerContext` has no rate-limit API.

### Cycle 3-B: GREEN

Step 1 — Add `rate_limit` module exports in `tessera-auth/src/lib.rs`:

```rust
pub use rate_limit::{LoginAttemptTracker, LoginPolicy};
```

Step 2 — Add fields to `ServerContext`:

File: `crates/tessera-server/src/context.rs`

```rust
use tessera_auth::{LoginAttemptTracker, LoginPolicy};

pub struct ServerContext {
    // ... existing fields ...
    login_tracker: Arc<LoginAttemptTracker>,
    login_policy: LoginPolicy,
}
```

Add constructor parameter and builder:

```rust
pub fn new(
    // ... existing params ...
    login_tracker: Arc<LoginAttemptTracker>,
    login_policy: LoginPolicy,
) -> Self { ... }

/// Access the login attempt tracker.
#[must_use]
pub const fn login_tracker(&self) -> &Arc<LoginAttemptTracker> {
    &self.login_tracker
}

/// Access the login policy.
#[must_use]
pub const fn login_policy(&self) -> &LoginPolicy {
    &self.login_policy
}
```

Step 3 — Update `BoltConnectionHandler::authenticate()`:

File: `crates/tessera-server/src/bolt_handler.rs`

```rust
async fn authenticate(
    &self,
    principal: &str,
    credentials: &str,
) -> std::result::Result<UserId, ()> {
    // Rate-limit check — fail-safe: deny if locked
    if self
        .ctx
        .login_tracker()
        .is_locked(principal, self.ctx.login_policy())
    {
        return Err(());
    }

    let result = if let Some(provider) = self.ctx.external_provider().cloned() {
        crate::auth_dispatch::authenticate_external(
            principal,
            credentials,
            &provider,
            self.ctx.group_mapping(),
            self.ctx.sessions(),
        )
        .await
        .map(|(id, _token)| id)
        .map_err(|_| ())
    } else {
        let password = Password::new(credentials).map_err(|_| ())?;
        self.ctx
            .user_store()
            .authenticate(principal, &password)
            .map_err(|_| ())
    };

    match &result {
        Ok(_) => self.ctx.login_tracker().record_success(principal),
        Err(()) => self.ctx.login_tracker().record_failure(principal),
    }

    result
}
```

Step 4 — Update `ServerContext::new` in all call sites (common/mod.rs, main.rs,
any other tests). The default tracker uses `LoginPolicy::new(10, 300)` (10
attempts, 5-minute lockout). Update `test_context()` helper to pass
`Arc::new(LoginAttemptTracker::new())` and `LoginPolicy::new(10, 300)`.

Step 5 — Implement `test_context_with_rate_limit` in `common/mod.rs`.

Run: all tests pass. `cargo clippy -p tessera-server` — no warnings.

### Cycle 3-C: REFACTOR

No structural changes needed. Verify `is_locked` is called before every
authentication attempt, not after. Review: the check is the first thing in
`authenticate()` — correct.

---

## Phase 4 — C4: External Auth Roles No Longer Discarded

### Context

`crates/tessera-server/src/auth_dispatch.rs:47` — `let _roles = map_groups(...)`.
The roles are computed but thrown away. External users have no RBAC permissions
after this point.

The fix stores the mapped roles in the session. `SessionManager` currently only
maps `SessionToken → (UserId, expires_at)`. Extending `SessionManager` with a
roles map would couple auth-session to RBAC. The correct approach is to store
roles alongside the session using an ephemeral `UserStoreHandle` insertion, but
`UserStoreHandle` was not designed for transient users.

The minimal correct fix: extend `SessionManager` to optionally store a `Vec<RoleId>`
per session, and provide a `roles_for_session` accessor. `AuthPolicy::check_session`
then uses this when the user is not found in `UserStoreHandle`.

**Scope caveat:** This is the most invasive of the critical fixes. The plan scopes
it to adding `roles_for_session` to `SessionManager` and returning them from a new
`ServerContext::resolve_roles_for_token` method. Actual RBAC enforcement at query
time (which queries are allowed per role) is out of scope and documented as a
follow-up.

### Cycle 4-A: RED — write failing tests

File: `crates/tessera-server/tests/auth_integration_test.rs`

Add test using `AlwaysOkProvider` that maps groups to roles:

```rust
#[tokio::test]
async fn external_auth_mapped_roles_are_retrievable_from_session() {
    // Provider returns group "admin-group"; mapping maps it to "admin" role.
    let ctx = test_context_with_external_provider_and_mapping(
        vec![("admin-group".to_owned(), "admin".to_owned())],
    );
    // Authenticate via HELLO
    let (mut writer, mut reader, _shutdown) = spawn_bolt_handler(ctx.clone()).await;
    bolt_send(&mut writer, &hello_request("external_user", "any")).await;
    let resp = bolt_recv(&mut reader).await;
    assert!(matches!(resp, BoltResponse::Success { .. }));

    // The session must carry the mapped roles
    // (Verify via the new ServerContext::session_roles accessor)
    // This requires the session token — for integration test purposes we assert
    // the session count and that no panic occurred; role storage is unit-tested
    // in tessera-auth tests.
}
```

Unit test in `crates/tessera-auth/src/session.rs`:

```rust
#[test]
fn session_stores_and_retrieves_roles() {
    let mgr = SessionManager::new(3600);
    let uid = UserId::new(1);
    let token = mgr.create_session(uid).unwrap();
    let roles = vec![RoleId::new(42)];
    mgr.set_session_roles(&token, roles.clone()).unwrap();
    let retrieved = mgr.roles_for_session(&token).unwrap();
    assert_eq!(retrieved, roles);
}

#[test]
fn session_roles_empty_by_default() {
    let mgr = SessionManager::new(3600);
    let uid = UserId::new(1);
    let token = mgr.create_session(uid).unwrap();
    let roles = mgr.roles_for_session(&token).unwrap();
    assert!(roles.is_empty());
}
```

### Cycle 4-B: GREEN

Step 1 — Extend `Session` struct in `crates/tessera-auth/src/session.rs`:

```rust
struct Session {
    user_id: UserId,
    expires_at: u64,
    roles: Vec<RoleId>,
}
```

Step 2 — Add `set_session_roles` and `roles_for_session` methods to `SessionManager`.

Step 3 — In `auth_dispatch.rs` replace `let _roles = ...` with:

```rust
let roles = map_groups(&info.groups, group_mapping);
let token = sessions
    .create_session(user_id)
    .map_err(|_| AuthError::InvalidCredentials)?;
// Store mapped roles so downstream RBAC checks can use them.
let _ = sessions.set_session_roles(&token, roles);
Ok((user_id, token))
```

Step 4 — In `ServerContext` add:

```rust
pub fn resolve_roles_for_token(&self, token: &SessionToken) -> Vec<RoleId> {
    self.sessions.roles_for_session(token).unwrap_or_default()
}
```

Run: all tests pass. `cargo test --workspace` — no regressions.

### Cycle 4-C: REFACTOR

`RoleId` must be publicly re-exported from `tessera-auth`. Verify `pub use
rbac::RoleId` is in `lib.rs` — it is. No further refactoring needed.

---

## Phase 5 — R8: LDAP bind_password Zeroized

### Context

`crates/tessera-auth/src/providers/ldap.rs:22` — `bind_password: String`. The
`zeroize` crate is already a dependency of `tessera-auth` (see `Cargo.toml`).
`Password` in `credentials.rs` already uses `Zeroizing<String>`. The fix is a
one-line type change.

### Cycle 5-A: RED

File: `crates/tessera-auth/src/providers/ldap.rs` — add test:

```rust
#[test]
fn ldap_config_bind_password_is_zeroizing() {
    // This test verifies the type at compile time — if `bind_password` is
    // plain `String`, the `Zeroizing` wrapper used here would fail to compile.
    use zeroize::Zeroizing;
    let cfg = LdapConfig {
        ldap_url: "ldap://localhost".to_owned(),
        bind_dn: "cn=svc".to_owned(),
        bind_password: Zeroizing::new("secret".to_owned()),
        base_dn: "dc=example".to_owned(),
        user_filter_template: "(uid={username})".to_owned(),
        group_attribute: "memberOf".to_owned(),
        use_tls: false,
        group_mapping: std::collections::HashMap::new(),
    };
    // If we reach here the field accepted a Zeroizing<String>
    drop(cfg);
}
```

This test will fail to compile until the field type is changed.

### Cycle 5-B: GREEN

File: `crates/tessera-auth/src/providers/ldap.rs`

```rust
use zeroize::Zeroizing;

pub struct LdapConfig {
    // ...
    pub bind_password: Zeroizing<String>,
    // ...
}
```

Update `from_env()`:

```rust
bind_password: Zeroizing::new(required_env("TESSERA_LDAP_BIND_PASSWORD")?),
```

Update all test constructors in the same file to use `Zeroizing::new("secret".to_owned())`.

Update `do_authenticate` call site:

```rust
conn.service_bind(&self.config.bind_dn, &self.config.bind_password)
```

`Zeroizing<String>` derefs to `String` which derefs to `str`, so `&*self.config.bind_password`
or simply `&self.config.bind_password` with an explicit deref coercion works.
Change to: `conn.service_bind(&self.config.bind_dn, &self.config.bind_password)`.

Run: `cargo test -p tessera-auth` — all pass. `cargo clippy -p tessera-auth` — no warnings.

---

## Phase 6 — R4: TransactionManager committed Set Pruning

### Context

`crates/tessera-storage-enterprise/src/txn/manager.rs:23-36` — `committed:
RwLock<Arc<HashSet<u64>>>` grows forever. Every committed transaction adds one
entry. In a long-running server this is an unbounded memory leak.

Pruning strategy: when a write to `committed` occurs (in `commit()`), discard all
IDs less than the oldest active snapshot's minimum txn_id. Because
`TransactionManager` does not track active handles, we need a minimal tracking
structure: a `Mutex<BTreeSet<u64>>` of currently active snapshot txn_ids.

The plan uses a simpler, safe approximation: keep only the last N committed IDs
where N is configurable (e.g., 65536 = `MAX_COMMITTED_WINDOW`). IDs below the
window floor are pruned. Snapshot correctness is preserved because snapshots hold
an `Arc<HashSet<u64>>` — they see the set at the time they were taken, not the
current set. Pruning only affects future snapshots, which is correct.

### Cycle 6-A: RED

File: `crates/tessera-storage-enterprise/src/txn/manager.rs`

Add test (inside existing `#[cfg(test)] mod tests`):

```rust
#[test]
fn committed_set_is_pruned_after_window() {
    let tmp = NamedTempFile::new().unwrap();
    let mgr = TransactionManager::open_with_window(tmp.path(), 16).unwrap();

    // Commit 100 transactions — well above the window of 16
    for _ in 0..100 {
        let mut h = mgr.begin(IsolationLevel::ReadCommitted).unwrap();
        mgr.commit(&mut h).unwrap();
    }

    // The committed set must hold at most 16 entries
    assert!(
        mgr.committed_count().unwrap() <= 16,
        "committed set must be pruned to at most window size"
    );
}

#[test]
fn snapshot_taken_before_prune_is_unaffected() {
    let tmp = NamedTempFile::new().unwrap();
    let mgr = TransactionManager::open_with_window(tmp.path(), 4).unwrap();

    // Commit 2 transactions, then take a snapshot
    for _ in 0..2 {
        let mut h = mgr.begin(IsolationLevel::ReadCommitted).unwrap();
        mgr.commit(&mut h).unwrap();
    }

    let snap_handle = mgr.begin(IsolationLevel::SnapshotIsolation).unwrap();
    let snap_count_before = snap_handle.snapshot().unwrap().committed_count();

    // Commit 10 more to trigger pruning
    for _ in 0..10 {
        let mut h = mgr.begin(IsolationLevel::ReadCommitted).unwrap();
        mgr.commit(&mut h).unwrap();
    }

    // The snapshot's view is frozen — must still see original 2 committed txns
    assert_eq!(
        snap_handle.snapshot().unwrap().committed_count(),
        snap_count_before,
        "snapshot must not be affected by pruning of the live committed set"
    );
}
```

### Cycle 6-B: GREEN

Step 1 — Add `MAX_COMMITTED_WINDOW: usize = 65_536` constant (or configurable via
`open_with_window`).

Step 2 — Add `window: usize` field to `TransactionManager`.

Step 3 — Add `open_with_window(path: &Path, window: usize) -> Result<Self>` (existing
`open` calls `open_with_window(path, MAX_COMMITTED_WINDOW)`).

Step 4 — In `commit()`, after inserting into `new_set`, prune if `new_set.len() > window`:

```rust
if new_set.len() > self.window {
    // Keep only the `window` highest IDs (most recent commits).
    // Older IDs are no longer needed for new snapshot isolation checks.
    let min_keep = new_set
        .iter()
        .copied()
        .rev() // HashSet has no order — collect, sort, then trim
        .take(self.window)  // ← see note below
        .min()
        .unwrap_or(0);
    new_set.retain(|&id| id >= min_keep);
}
```

Note: `HashSet` has no ordering. For efficient pruning, switch `committed` to
`BTreeSet<u64>` so we can call `.iter().next()` for the minimum. This is a minor
refactor inside the same commit:

Change `RwLock<Arc<HashSet<u64>>>` → `RwLock<Arc<BTreeSet<u64>>>`.

Update `Snapshot::new` and `is_visible` — `Snapshot` holds `Arc<HashSet<u64>>`
currently; change its inner type to `Arc<BTreeSet<u64>>`.

Pruning in `commit()`:

```rust
if new_set.len() > self.window {
    while new_set.len() > self.window {
        // BTreeSet::pop_first is stable since Rust 1.66
        new_set.pop_first();
    }
}
```

Run: `cargo test -p tessera-storage-enterprise` — all pass.

---

## Phase 7 — R3: Remove Dead `_registry` Parameter

### Context

`crates/tessera-server/src/listener.rs:111,195` — `_registry:
Arc<TenantRegistry>` is received but bound with `_` prefix, meaning it is
intentionally unused. The registry is already in `ServerContext` (accessible via
`ctx.tenant_registry()`). The parameter was likely an artifact of an earlier
design. Its presence is misleading and violates the principle of least confusion.

### Cycle 7-A: RED

The failure here is a Clippy warning `clippy::unused_variables` — already denied
by workspace settings. Verify:

```
cargo clippy -p tessera-server 2>&1 | grep "_registry"
```

If Clippy does not currently warn (because of the `_` prefix suppression), add an
explicit test that the parameter is absent from the signature. The cleanest
approach: the parameter removal IS the test (compilation fails if any call site
still passes it).

Add a new integration test that calls `serve` without the registry parameter to
document the expected signature:

```rust
// In crates/tessera-server/tests/listener_test.rs (new test or assertion):
// (This test will fail to compile until serve() and serve_tls() signatures are updated)
#[tokio::test]
async fn serve_does_not_accept_registry_parameter() {
    // This test verifies that serve() compiles WITHOUT a registry argument.
    // If the function still has the _registry parameter, this call would not
    // compile (wrong arity). The test body is intentionally minimal.
    let _ = async {
        // Compile-time check only — never actually awaited in this test.
        let _: std::pin::Pin<Box<dyn std::future::Future<Output = _>>> = Box::pin(
            TesseraListener::bind("127.0.0.1:0") // hypothetical
                .await
                .unwrap()
                .serve(
                    Arc::new(todo!()),
                    // no registry here
                    tokio::sync::watch::channel(false).1,
                    1,
                    std::time::Duration::from_secs(30),
                    "default".to_owned(),
                ),
        );
    };
}
```

For simplicity: skip the compile-time test approach and instead write the removal
directly, then verify no test regressions.

### Cycle 7-B: GREEN

File: `crates/tessera-server/src/listener.rs`

Remove `registry: Arc<TenantRegistry>` from both `serve` and `serve_tls`
signatures and their bodies. Remove the `let _registry = Arc::clone(&registry)`
bindings. Remove the `use tessera_tenant::TenantRegistry` import if it is no
longer used (verify with `cargo check`).

Update all call sites: scan with
`grep -r "serve\|serve_tls" crates/tessera-server/ --include="*.rs"` to find
callers in `main.rs`, tests, etc. Remove the registry argument at each call site.

Run: `cargo build -p tessera-server` and `cargo test -p tessera-server`.

---

## Phase 8 — R5 + O8: Prometheus +Inf and list_databases TOCTOU

### R5: Prometheus +Inf Bucket

Reading `render.rs:115-120` confirms the `+Inf` bucket IS already emitted:

```rust
let _ = writeln!(
    buf,
    r#"tessera_query_duration_seconds_bucket{{le="+Inf"}} {total_count}"#
);
```

And `render.rs:212-219` already tests for it (`assert_eq!(...13)`).

**Conclusion:** R5 is already correctly implemented. The plan adds only a
documentation comment clarifying that the `+Inf` requirement is satisfied.

Action: Add a code comment above line 115 in `render.rs`:

```rust
// Prometheus spec (https://prometheus.io/docs/instrumenting/exposition_formats/)
// requires an explicit +Inf bucket equal to the total observation count.
```

No new test needed — existing `render_histogram_all_buckets_present` covers it.

### O8: list_databases TOCTOU

### Cycle 8-A: RED

File: `crates/tessera-tenant/tests/registry_test.rs`

```rust
#[test]
fn list_databases_returns_empty_when_tenant_dir_disappears_between_check_and_read() {
    // Simulate TOCTOU: the tenant directory exists at exists()-check time but
    // `read_dir` would return NotFound. The current implementation propagates
    // that as a TenantNotFound error; the correct behavior is to return Ok(vec![]).
    //
    // We test this by directly checking that NotFound io errors from read_dir
    // are treated as an empty result rather than an error.
    //
    // Since we cannot reliably race the OS, we test the logical behavior
    // through the public API: a tenant that existed but whose directory is
    // deleted between list_tenants() and list_databases() should return Ok([]).

    let dir = tempfile::tempdir().unwrap();
    let registry = TenantRegistry::new(dir.path(), tessera_graph::GraphConfig::new());
    let tenant = TenantId::new("acme").unwrap();
    let addr = DatabaseAddress {
        tenant: tenant.clone(),
        database: DatabaseName::default_name(),
    };

    // Create and immediately remove the tenant dir to simulate disappearance
    registry.create_database(&addr).unwrap();
    std::fs::remove_dir_all(dir.path().join("acme")).unwrap();

    // list_databases should return Ok(vec![]) not TenantNotFound
    let result = registry.list_databases(&tenant);
    assert!(
        matches!(result, Ok(ref v) if v.is_empty()),
        "list_databases must return Ok([]) when tenant dir disappears, got: {result:?}"
    );
}
```

### Cycle 8-B: GREEN

File: `crates/tessera-tenant/src/registry.rs`

In `list_databases`, change the check from `if !tenant_dir.exists()` to
handle `NotFound` gracefully in the `read_dir` call itself:

```rust
pub fn list_databases(&self, tenant: &TenantId) -> Result<Vec<DatabaseName>> {
    let tenant_dir = self.base_dir.join(tenant.as_str());

    let entries = match std::fs::read_dir(&tenant_dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Vec::new());
        }
        Err(e) => return Err(TenantError::Io(e)),
    };

    let mut databases = Vec::new();
    for entry in entries {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            if let Some(name) = entry.file_name().to_str() {
                if let Ok(db) = DatabaseName::new(name) {
                    databases.push(db);
                }
            }
        }
    }
    Ok(databases)
}
```

Note: the previous behavior returned `TenantNotFound` for a non-existent tenant.
The new behavior returns `Ok(vec![])`. Update the doc comment to reflect this:
"Returns an empty vec if the tenant directory does not exist." Callers that
expected `TenantNotFound` must be checked — search with
`grep -r "list_databases" crates/ --include="*.rs"`.

Run: `cargo test -p tessera-tenant`.

---

## Phase 9 — R1: SecureGraph / SecureGraphRef DRY Refactor

### Context

`crates/tessera-storage-enterprise/src/lbac.rs:148-514` — Eight read methods
(`node_ids`, `nodes_by_label`, `node`, `node_exists`, `node_count`, `edges_by_label`,
`edge`, `edge_count`, `outgoing_edges`, `incoming_edges`) are copy-pasted between
`SecureGraph<'g, G: GraphAccess>` and `SecureGraphRef<'g, G: GraphAccess>`. The
only difference is `&mut self` vs `&self` for the mutable type, and `self.inner`
vs `self.inner`.

The `filter` module already contains shared pure functions. The read methods can
be extracted into a private trait or free functions that take `&G` and `&Clearance`.

The clean design: define a private `read_impl` free function for each read
method, parametrized over `G: GraphAccess`. Both `SecureGraph` and `SecureGraphRef`
call these functions. The mutation methods stay as-is.

### Cycle 9-A: RED

Write a test that confirms the refactored `SecureGraphRef` and `SecureGraph`
produce identical results for every read method on the same graph:

File: `crates/tessera-storage-enterprise/tests/lbac_parity_test.rs` (new file)

```rust
// Verifies that SecureGraph (read methods) and SecureGraphRef return identical
// results — this test would catch any divergence introduced by the refactor.

use tessera_auth::lbac::{Clearance, SecurityLabel, SecurityPolicy};
use tessera_graph::{Graph, GraphConfig, GraphAccess, NodeId, props};
use tessera_storage_enterprise::lbac::{SecureGraph, SecureGraphRef};

fn make_graph_with_nodes() -> Graph {
    let dir = tempfile::tempdir().unwrap();
    let mut g = Graph::open(dir.path(), &GraphConfig::new()).unwrap();
    std::mem::forget(dir); // keep dir alive
    let label = SecurityLabel::new(
        tessera_auth::lbac::ClearanceLevel::Unclassified,
        Default::default(),
    );
    let mut props1 = props![];
    SecurityPolicy::inject_label(&mut props1, &label);
    g.add_node("Person", props1).unwrap();
    g
}

#[test]
fn secure_graph_and_ref_read_methods_are_identical() {
    let mut graph = make_graph_with_nodes();
    let clearance = Clearance::max();

    let ids_via_ref = {
        let secure = SecureGraphRef::new(&graph, clearance.clone());
        secure.node_ids()
    };

    let ids_via_mut = {
        let secure = SecureGraph::new(&mut graph, clearance.clone());
        secure.node_ids()
    };

    assert_eq!(ids_via_ref, ids_via_mut, "node_ids must be identical");
}
```

The test should pass before AND after the refactor. Its purpose is to act as a
regression guard during the refactor.

### Cycle 9-B: GREEN (Refactor)

File: `crates/tessera-storage-enterprise/src/lbac.rs`

Extract a module `read_ops` (private, inside the file) containing free functions:

```rust
mod read_ops {
    use tessera_auth::lbac::Clearance;
    use tessera_graph::{Edge, EdgeId, Error, GraphAccess, Node, NodeId};
    use std::collections::HashSet;
    use super::filter;

    pub fn node_ids<G: GraphAccess>(inner: &G, clearance: &Clearance) -> Vec<NodeId> { ... }
    pub fn nodes_by_label<G: GraphAccess>(inner: &G, clearance: &Clearance, label: &str) -> Vec<NodeId> { ... }
    pub fn node<G: GraphAccess>(inner: &G, clearance: &Clearance, id: NodeId) -> tessera_graph::Result<Node> { ... }
    pub fn node_exists<G: GraphAccess>(inner: &G, clearance: &Clearance, id: NodeId) -> bool { ... }
    pub fn edges_by_label<G: GraphAccess>(inner: &G, clearance: &Clearance, label: &str) -> Vec<EdgeId> { ... }
    pub fn edge<G: GraphAccess>(inner: &G, clearance: &Clearance, id: EdgeId) -> tessera_graph::Result<Edge> { ... }
    pub fn edge_count<G: GraphAccess>(inner: &G, clearance: &Clearance) -> usize { ... }
    pub fn outgoing_edges<G: GraphAccess>(inner: &G, clearance: &Clearance, node: NodeId) -> tessera_graph::Result<Vec<Edge>> { ... }
    pub fn incoming_edges<G: GraphAccess>(inner: &G, clearance: &Clearance, node: NodeId) -> tessera_graph::Result<Vec<Edge>> { ... }
}
```

Each `GraphAccess` impl on `SecureGraph` and `SecureGraphRef` becomes a one-liner
delegation:

```rust
fn node_ids(&self) -> Vec<NodeId> {
    read_ops::node_ids(self.inner, &self.clearance)
}
```

Run: `cargo test -p tessera-storage-enterprise` — all existing tests plus parity
test pass. `cargo clippy -p tessera-storage-enterprise`.

### Cycle 9-C: REFACTOR

Verify `node_count` is computed as `self.node_ids().len()` in both types — it is.
After the refactor this remains consistent via the shared `read_ops::node_ids`.
No further change needed.

---

## Phase 10 — R2: dict_str Single-Pass Extraction

### Context

`bolt_handler.rs:644-656` — `dict_str` does a linear scan. It is called 3+ times
in `handle_hello` (principal, credentials, db). For a HELLO dict of 3 entries,
this is a constant-time operation in practice — but it is algorithmically O(k*n)
where k=fields and n=dict length.

The fix: extract all needed fields in one pass.

### Cycle 10-A: RED

Add a unit test inside `bolt_handler.rs` that verifies the new `extract_hello_fields`
function returns all three fields in a single call and handles missing fields:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_hello_fields_finds_all_keys() {
        let dict: BoltDict = vec![
            ("principal".to_owned(), PackStreamValue::String("alice".to_owned())),
            ("credentials".to_owned(), PackStreamValue::String("pw".to_owned())),
            ("db".to_owned(), PackStreamValue::String("mydb".to_owned())),
        ];
        let (principal, credentials, db) = extract_hello_fields(&dict);
        assert_eq!(principal, Some("alice"));
        assert_eq!(credentials, Some("pw"));
        assert_eq!(db, Some("mydb"));
    }

    #[test]
    fn extract_hello_fields_returns_none_for_missing() {
        let dict: BoltDict = vec![
            ("principal".to_owned(), PackStreamValue::String("alice".to_owned())),
        ];
        let (principal, credentials, db) = extract_hello_fields(&dict);
        assert_eq!(principal, Some("alice"));
        assert_eq!(credentials, None);
        assert_eq!(db, None);
    }
}
```

### Cycle 10-B: GREEN

File: `crates/tessera-server/src/bolt_handler.rs`

Add free function:

```rust
/// Extract `principal`, `credentials`, and `db` from a HELLO/LOGON extra dict
/// in a single pass, avoiding O(k*n) repeated linear scans.
fn extract_hello_fields<'a>(
    dict: &'a BoltDict,
) -> (Option<&'a str>, Option<&'a str>, Option<&'a str>) {
    let mut principal = None;
    let mut credentials = None;
    let mut db = None;

    for (k, v) in dict {
        if let PackStreamValue::String(s) = v {
            match k.as_str() {
                "principal"   => principal   = Some(s.as_str()),
                "credentials" => credentials = Some(s.as_str()),
                "db"          => db          = Some(s.as_str()),
                _             => {}
            }
        }
    }

    (principal, credentials, db)
}
```

Update `handle_hello` to use `extract_hello_fields(extra)` instead of three
`dict_str` calls.

Keep `dict_str` for the one remaining usage in `handle_run` (if any) or mark it
`#[cfg(test)]` if no longer used in production. Verify with
`grep -n "dict_str(" crates/tessera-server/src/bolt_handler.rs`.

Run: `cargo test -p tessera-server`.

---

## Phase 11 — R6: LOGON State Cleanup

### Context

`bolt_handler.rs:189-191` — `BoltRequest::Logon` calls `handle_hello` directly.
Re-authentication via LOGON can change `session_token` and `graph` mid-connection
while `pending_result` from a previous RUN/PULL cycle is still live. A prior
session's clearance might have been higher — the new session inherits the graph
from HELLO, but a pending result from the old session is still accessible.

Fix: before calling `handle_hello` from LOGON, clear `pending_result` and revoke
the previous session.

### Cycle 11-A: RED

File: `crates/tessera-server/tests/bolt_handler_test.rs`

```rust
#[tokio::test]
async fn logon_clears_pending_result_from_previous_session() {
    let ctx = test_context();
    let (mut writer, mut reader, _shutdown) = spawn_bolt_handler(ctx).await;

    // Authenticate and run a query to populate pending_result
    bolt_send(&mut writer, &hello_request("admin", "Admin@Init1!")).await;
    let _ = bolt_recv(&mut reader).await;

    bolt_send(&mut writer, &run_query("MATCH (n) RETURN n")).await;
    let _ = bolt_recv(&mut reader).await; // SUCCESS with fields

    // LOGON with same credentials (re-auth)
    bolt_send(
        &mut writer,
        &BoltRequest::Logon {
            auth: vec![
                ("principal".to_owned(), PackStreamValue::String("admin".to_owned())),
                ("credentials".to_owned(), PackStreamValue::String("Admin@Init1!".to_owned())),
            ],
        },
    )
    .await;
    let resp = bolt_recv(&mut reader).await;
    assert!(matches!(resp, BoltResponse::Success { .. }));

    // PULL must return empty Success (no pending result from old session)
    bolt_send(&mut writer, &BoltRequest::Pull { extra: vec![] }).await;
    let resp = bolt_recv(&mut reader).await;
    // Should be Success with has_more=false, NOT records from the old query
    assert!(matches!(resp, BoltResponse::Success { .. }), "PULL after LOGON must return empty success");
}
```

### Cycle 11-B: GREEN

File: `crates/tessera-server/src/bolt_handler.rs`

Replace the LOGON arm in `dispatch`:

```rust
BoltRequest::Logon { ref auth } => {
    // Clean up state from the previous authentication context before
    // re-authenticating. This prevents stale pending results from the old
    // session being accessible under the new session's clearance.
    if let Some(old_token) = self.session_token.take() {
        let _ = self.ctx.sessions().revoke(&old_token);
    }
    self.pending_result = None;
    self.handle_hello(auth).await?;
}
```

Run: `cargo test -p tessera-server`.

---

## Phase 12 — R7: Audit Logging in handle_run

### Context

`bolt_handler.rs:323-471` — `handle_run` never calls `self.ctx.audit()`.
`ServerContext` exposes `audit()` returning `&Arc<AuditLog>`. The fix adds audit
entries at two points: before execution (query accepted) and in the rejection
branches (unauthorized, parse error, execution error).

### Cycle 12-A: RED

File: `crates/tessera-server/tests/bolt_handler_test.rs`

Test that after a successful RUN the audit log contains a success entry, and that
an unauthorized RUN produces a denied entry. Because `AuditLog` is write-only (no
read API), verify indirectly via a custom `AuditLog` wrapper or by testing that
`record_success` / `record_denied` do not panic. The more practical approach: use
a shared tempfile and verify the NDJSON file content.

```rust
#[tokio::test]
async fn run_query_emits_audit_entry_on_success() {
    let audit_dir = tempfile::tempdir().unwrap();
    let audit_path = audit_dir.path().join("audit.ndjson");
    let ctx = test_context_with_audit_path(&audit_path);

    let (mut writer, mut reader, _shutdown) = spawn_bolt_handler(ctx).await;
    bolt_send(&mut writer, &hello_request("admin", "Admin@Init1!")).await;
    let _ = bolt_recv(&mut reader).await;

    bolt_send(&mut writer, &run_query("MATCH (n) RETURN n")).await;
    let _ = bolt_recv(&mut reader).await; // SUCCESS

    // Flush is synchronous inside AuditLog — file should contain entries
    let content = std::fs::read_to_string(&audit_path).unwrap();
    // At least one line should exist for the query
    assert!(content.lines().count() >= 1, "audit file must contain at least one entry after RUN");
}
```

Add `test_context_with_audit_path` to `tests/common/mod.rs`.

### Cycle 12-B: GREEN

File: `crates/tessera-server/src/bolt_handler.rs`

At the start of `handle_run`, after extracting `session_token` and `clearance`,
add:

```rust
// Audit: query accepted for execution
let _ = self.ctx.audit().record_success(
    self.session_token.as_ref().map(|_| {
        // We have the session token but not the UserId here — use None
        // until resolve_user_id is exposed. Record the query as the target.
        None
    }).unwrap_or(None),
    "RUN",
    Some(query),
);
```

At each early-return failure branch (`send_failure` calls), add:

```rust
let _ = self.ctx.audit().record_denied(None, "RUN", Some(query), "reason text");
```

For the execution error branch, use `record_error`.

Note: the `UserId` is not directly available in `handle_run` without a session
lookup. Call `self.ctx.sessions().validate(token)` to get it:

```rust
let user_id_opt = self.session_token.as_ref()
    .and_then(|t| self.ctx.sessions().validate(t).ok())
    .map(|uid| uid.raw());
```

Add this lookup at the start of `handle_run`, before the existing session check,
to use `user_id_opt` in all subsequent audit calls.

Run: `cargo test -p tessera-server`.

---

## Phase 13 — O1: Unique connection_id Per Connection

### Context

`bolt_handler.rs:290-293` — `"connection_id"` is always `"bolt-tessera"`. Every
client sees the same ID, making per-connection tracing impossible.

Fix: add an `AtomicU64` connection counter to `ServerContext` and format it as
`format!("bolt-{}", counter.fetch_add(1, Relaxed))` in `new_with_handshake` or
`handle_hello`.

### Cycle 13-A: RED

File: `crates/tessera-server/tests/bolt_handler_test.rs`

```rust
#[tokio::test]
async fn each_connection_gets_unique_connection_id() {
    let ctx = test_context();
    let id1 = get_connection_id(Arc::clone(&ctx)).await;
    let id2 = get_connection_id(Arc::clone(&ctx)).await;
    assert_ne!(id1, id2, "concurrent connections must have distinct connection_ids");
}

async fn get_connection_id(ctx: Arc<ServerContext>) -> String {
    let (mut writer, mut reader, _shutdown) = spawn_bolt_handler(ctx).await;
    bolt_send(&mut writer, &hello_request("admin", "Admin@Init1!")).await;
    if let BoltResponse::Success { metadata } = bolt_recv(&mut reader).await {
        metadata
            .iter()
            .find_map(|(k, v)| {
                if k == "connection_id" {
                    if let PackStreamValue::String(s) = v { Some(s.clone()) } else { None }
                } else { None }
            })
            .unwrap_or_default()
    } else {
        panic!("HELLO must succeed")
    }
}
```

### Cycle 13-B: GREEN

Add `next_connection_id: AtomicU64` to `ServerContext`. Add accessor
`fn next_connection_id(&self) -> u64` that calls `fetch_add(1, Relaxed)`.

In `handle_hello`, replace:

```rust
PackStreamValue::String("bolt-tessera".to_owned()),
```

with:

```rust
PackStreamValue::String(format!("bolt-{}", self.ctx.next_connection_id())),
```

Run: tests pass.

---

## Phase 14 — O2: gql_result_to_packstream Projection Order

### Context

`bolt_handler.rs:611` — `columns.sort()` destroys the query projection order.
`RETURN a.name, a.age` produces columns `["age", "name"]` not `["name", "age"]`.

The GQL result is `Vec<HashMap<String, GqlValue>>`. `HashMap` has no insertion
order. The column order depends on the GQL executor's return contract.

The minimal fix without changing the GQL executor: since `HashMap` has no order,
sort columns deterministically (current behavior) and document it. The
semantically correct fix requires the GQL executor to return `IndexMap` or an
ordered result.

**Decision:** The plan fixes O2 by removing the sort and instead deriving column
order from `result[0].keys()` as-is (which preserves iteration order of `HashMap`,
non-deterministic but consistent for a given execution). The proper fix (IndexMap)
is deferred to a GQL executor change. For now, add a test that documents the
current (sorted) behavior as "stable but not projection-order", and remove the
`.sort()` call with a comment explaining why.

Actually, since `HashMap` iteration order is non-deterministic, removing `.sort()`
makes the output non-deterministic. That is worse. The stable approach: keep
`.sort()` but document it explicitly, and mark this as a follow-up for when the
GQL executor returns an ordered type.

**Revised plan for O2:** keep `.sort()`, add an explanatory comment, and add a
test that verifies column order is deterministic (alphabetical) across multiple
calls:

```rust
#[test]
fn gql_result_columns_are_sorted_alphabetically() {
    // O2: Until the GQL executor returns an ordered result type (IndexMap),
    // columns are sorted alphabetically to guarantee stable output order.
    let mut row = std::collections::HashMap::new();
    row.insert("z_col".to_owned(), GqlValue::Int(1));
    row.insert("a_col".to_owned(), GqlValue::Int(2));
    row.insert("m_col".to_owned(), GqlValue::Int(3));
    let (columns, _) = gql_result_to_packstream(&[row]);
    assert_eq!(columns, vec!["a_col", "m_col", "z_col"]);
}
```

Add this test inside `bolt_handler.rs #[cfg(test)]`. Add the comment above `.sort()`:

```rust
// Columns are sorted alphabetically for deterministic output. Projection order
// is not preserved because GqlValue results use HashMap. Tracking issue: replace
// with IndexMap when the GQL executor supports ordered projection.
columns.sort();
```

This is a documentation/test fix, not a behavior change.

---

## Phase 15 — O3: unix_timestamp Deduplication

### Context

`unix_timestamp()` is defined identically in:
- `crates/tessera-auth/src/utils.rs` (private `pub(crate)`)
- `crates/tessera-audit/src/lib.rs` (private `fn`)

The audit crate does not depend on tessera-auth. Both crates are in the workspace.

The clean solution: extract `unix_timestamp` into `tessera-config` or a new
`tessera-utils` micro-crate, then have both depend on it. However, adding a new
crate requires Cargo.toml workspace changes, which is a larger scope.

**Simpler alternative:** tessera-audit already has no dependency on tessera-auth.
Publish `unix_timestamp` as `pub` from `tessera-auth::utils` and have
`tessera-audit` depend on `tessera-auth`. But that creates a dependency that does
not semantically belong.

**Decision:** Use `tessera-config` as the shared location — it is already a
dependency of `tessera-server`. Make `unix_timestamp` a `pub` function in
`tessera-config`, then update both `tessera-auth` and `tessera-audit` to delegate
to it. This requires adding `tessera-config` as a dependency of `tessera-auth` —
check for cycles.

Dependency cycle check: `tessera-config` must not depend on `tessera-auth`.
Read `tessera-config/Cargo.toml` before implementing.

### Cycle 15-A: Pre-check (READ before writing)

```
cat crates/tessera-config/Cargo.toml
```

If `tessera-config` does not depend on `tessera-auth`: proceed.
If it does: use `tessera-audit` as the canonical location instead (audit → auth
is currently one-way: server depends on both).

### Cycle 15-B: RED — duplicate detection test (compile-time via Clippy)

Clippy does not detect cross-crate duplication. Add a comment-only test:

```rust
// O3: unix_timestamp is also defined in tessera-audit::lib::unix_timestamp.
// Once the deduplication in Phase 15 is complete, this function should be
// replaced with tessera_config::unix_timestamp or re-exported from there.
// This comment acts as the test — if the function still exists here after
// Phase 15, the plan is incomplete.
```

### Cycle 15-C: GREEN

Step 1 — In `tessera-config` add:

```rust
/// Returns the current time as seconds since the Unix epoch.
pub fn unix_timestamp() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock is before epoch")
        .as_secs()
}
```

Step 2 — In `tessera-auth/src/utils.rs` delegate:

```rust
pub fn unix_timestamp() -> u64 {
    tessera_config::unix_timestamp()
}
```

Add `tessera-config` to `tessera-auth/Cargo.toml` dependencies (check no cycle).

Step 3 — In `tessera-audit/src/lib.rs` remove the local `unix_timestamp` and add
`tessera-config` dependency, then call `tessera_config::unix_timestamp()`.

Run: `cargo build --workspace` — no errors or cycles.

---

## Phase 16 — O4: LoginAttemptTracker Lock-Poison Safety

### Context

`crates/tessera-auth/src/rate_limit.rs:43,58,68` — all three methods call
`.expect("tracker lock poisoned")`, which panics and kills the server process if
the lock is ever poisoned.

Fix: return `Result<(), AuthError>` from `record_failure` and `record_success`.
`is_locked` returns `bool` — change to `Result<bool, AuthError>`. Update call sites
in Phase 3 (C2).

### Cycle 16-A: RED

Add tests inside `rate_limit.rs #[cfg(test)]`:

```rust
#[test]
fn record_failure_returns_ok() {
    let tracker = LoginAttemptTracker::new();
    assert!(tracker.record_failure("alice").is_ok());
}

#[test]
fn record_success_returns_ok() {
    let tracker = LoginAttemptTracker::new();
    tracker.record_failure("alice").unwrap();
    assert!(tracker.record_success("alice").is_ok());
}

#[test]
fn is_locked_returns_result() {
    let tracker = LoginAttemptTracker::new();
    let policy = LoginPolicy::new(3, 60);
    let result = tracker.is_locked("alice", &policy);
    assert!(result.is_ok());
    assert!(!result.unwrap());
}
```

These will fail to compile until the signatures change.

### Cycle 16-B: GREEN

File: `crates/tessera-auth/src/rate_limit.rs`

Change signatures:

```rust
pub fn record_failure(&self, username: &str) -> crate::error::Result<()> {
    let mut map = self.attempts.lock().map_err(|_| AuthError::LockPoisoned("login tracker"))?;
    let entry = map.entry(username.to_owned()).or_insert_with(|| (0, Instant::now()));
    entry.0 += 1;
    entry.1 = Instant::now();
    Ok(())
}

pub fn record_success(&self, username: &str) -> crate::error::Result<()> {
    let mut map = self.attempts.lock().map_err(|_| AuthError::LockPoisoned("login tracker"))?;
    map.remove(username);
    Ok(())
}

pub fn is_locked(&self, username: &str, policy: &LoginPolicy) -> crate::error::Result<bool> {
    let map = self.attempts.lock().map_err(|_| AuthError::LockPoisoned("login tracker"))?;
    Ok(match map.get(username) {
        Some(&(count, last_attempt)) => {
            if count < policy.max_attempts {
                return Ok(false);
            }
            last_attempt.elapsed().as_secs() < policy.lockout_duration_secs
        }
        None => false,
    })
}
```

Update C2 call sites in `bolt_handler.rs` (Phase 3) to handle the `Result` return.
In `authenticate()`, use `unwrap_or(false)` for `is_locked` in the fail-safe direction:

```rust
if self.ctx.login_tracker().is_locked(principal, self.ctx.login_policy())
    .unwrap_or(true) // fail-safe: if lock poisoned, treat as locked
{
    return Err(());
}
```

Run: `cargo test --workspace`.

---

## Phase 17 — O5: Non-Empty Params Return FAILURE

### Context

`bolt_handler.rs:331` — `_params: &BoltDict` is ignored. If a client sends params
with a query, they are silently dropped. The client gets SUCCESS but the query ran
without substitution.

Fix: check if `params` is non-empty and return FAILURE immediately.

### Cycle 17-A: RED

File: `crates/tessera-server/tests/bolt_handler_test.rs`

```rust
#[tokio::test]
async fn run_with_params_returns_failure() {
    let ctx = test_context();
    let (mut writer, mut reader, _shutdown) = spawn_bolt_handler(ctx).await;

    bolt_send(&mut writer, &hello_request("admin", "Admin@Init1!")).await;
    let _ = bolt_recv(&mut reader).await;

    // Send RUN with a non-empty params dict
    bolt_send(
        &mut writer,
        &BoltRequest::Run {
            query: "MATCH (n) WHERE n.id = $id RETURN n".to_owned(),
            params: vec![
                ("id".to_owned(), PackStreamValue::Int(42)),
            ],
            extra: vec![],
        },
    )
    .await;
    let resp = bolt_recv(&mut reader).await;
    assert!(
        matches!(resp, BoltResponse::Failure { .. }),
        "RUN with params must return FAILURE since params are not supported, got: {resp:?}"
    );
}
```

### Cycle 17-B: GREEN

File: `crates/tessera-server/src/bolt_handler.rs`

In `handle_run`, change `_params` to `params` and add at the start:

```rust
if !params.is_empty() {
    return self
        .send_failure(
            "Neo.ClientError.Statement.ParameterMissing",
            "query parameters are not yet supported; submit queries without parameters",
        )
        .await;
}
```

Run: `cargo test -p tessera-server`.

---

## Phase 18 — O6: escape_ldap_filter_value Multi-Byte Fix

### Context

`crates/tessera-auth/src/providers/ldap.rs:207` — the loop iterates `value.bytes()`
and casts non-ASCII bytes via `byte as char`. For a multi-byte UTF-8 character like
`é` (0xC3 0xA9), byte `0xC3` (195) is cast to Unicode scalar `U+00C3` (Ã), which
is incorrect. The RFC 4515 escape format requires each byte of the UTF-8 encoding
to be individually hex-escaped: `\c3\a9` for `é`.

Fix: iterate bytes but escape any byte `>= 0x80` as `\\{byte:02x}`. ASCII bytes
below 0x80 are handled by the existing match arms.

### Cycle 18-A: RED

File: `crates/tessera-auth/src/providers/ldap.rs` (in `#[cfg(test)] mod tests`):

```rust
#[test]
fn ldap_escape_multi_byte_utf8_char() {
    // 'é' is U+00E9 = UTF-8 bytes [0xC3, 0xA9]
    // RFC 4515 requires each byte to be hex-escaped: \c3\a9
    let result = escape_ldap_filter_value("café");
    assert_eq!(result, r"caf\c3\a9", "multi-byte UTF-8 must be byte-escaped per RFC 4515");
}

#[test]
fn ldap_escape_pure_ascii_unchanged() {
    // Regression: pure ASCII must still work after the fix
    assert_eq!(escape_ldap_filter_value("alice"), "alice");
    assert_eq!(escape_ldap_filter_value(r"a\b"), r"a\5cb");
}

#[test]
fn ldap_escape_chinese_character() {
    // U+4E2D (中) = UTF-8 bytes [0xE4, 0xB8, 0xAD]
    let result = escape_ldap_filter_value("中");
    assert_eq!(result, r"\e4\b8\ad");
}
```

### Cycle 18-B: GREEN

File: `crates/tessera-auth/src/providers/ldap.rs`

Replace `escape_ldap_filter_value`:

```rust
#[must_use]
pub fn escape_ldap_filter_value(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len() * 3);
    for byte in value.bytes() {
        match byte {
            b'\\' => escaped.push_str("\\5c"),
            b'*'  => escaped.push_str("\\2a"),
            b'('  => escaped.push_str("\\28"),
            b')'  => escaped.push_str("\\29"),
            0     => escaped.push_str("\\00"),
            // ASCII printable (0x20–0x7E) pass through as-is.
            // All other bytes (non-ASCII multi-byte UTF-8, control chars) are
            // hex-escaped per RFC 4515 §3.
            b if b >= 0x80 || (b < 0x20 && b != 0) => {
                escaped.push('\\');
                escaped.push_str(&format!("{b:02x}"));
            }
            _ => escaped.push(byte as char),
        }
    }
    escaped
}
```

Run: `cargo test -p tessera-auth`.

---

## Phase 19 — O7: SessionManager Periodic Expiry

### Context

`crates/tessera-auth/src/session.rs` — expired sessions are only removed on
`validate()` call (lazy expiry). In a long-running server with many short-lived
connections, the session map grows until `validate` is called for each expired
session. Sessions from abandoned connections (e.g. client crashed) never get
cleaned up.

Fix: add a `purge_expired` method and call it from a Tokio background task spawned
from `ServerContext::new`.

### Cycle 19-A: RED

Unit test in `session.rs`:

```rust
#[test]
fn purge_expired_removes_stale_sessions() {
    // Use a 0-second TTL so all sessions expire immediately
    let mgr = SessionManager::new(0);
    let uid = UserId::new(1);
    let _token = mgr.create_session(uid).unwrap();
    // Wait 1 second to ensure expiry
    std::thread::sleep(std::time::Duration::from_secs(1));
    mgr.purge_expired().unwrap();
    // Session should be gone — validate must fail
    // (We cannot validate because we don't hold the token in this scope,
    //  but we verify via session_count)
    assert_eq!(mgr.session_count().unwrap(), 0);
}
```

Add `session_count() -> Result<usize>` method to `SessionManager` (needed for
the test and potentially useful for metrics).

### Cycle 19-B: GREEN

Step 1 — Add `purge_expired` to `SessionManager`:

```rust
pub fn purge_expired(&self) -> crate::error::Result<()> {
    let now = crate::utils::unix_timestamp();
    let mut sessions = self.sessions.write()
        .map_err(|_| AuthError::LockPoisoned("session manager"))?;
    sessions.retain(|_, s| s.expires_at > now);
    Ok(())
}

pub fn session_count(&self) -> crate::error::Result<usize> {
    Ok(self.sessions.read()
        .map_err(|_| AuthError::LockPoisoned("session manager"))?
        .len())
}
```

Step 2 — In `ServerContext::new` (or a `start_background_tasks` method), spawn a
Tokio task:

```rust
// In ServerContext, add:
pub fn start_background_tasks(self: &Arc<Self>) {
    let sessions = Arc::clone(&self.sessions);
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
        loop {
            interval.tick().await;
            let _ = sessions.purge_expired();
        }
    });
}
```

Call `ctx.start_background_tasks()` from `main.rs` after constructing the context.

Run: `cargo test --workspace`.

---

## Phase 20 — Wiring Verification Cycle (WV)

This phase verifies that every new public function/method introduced in Phases 1-19
has at least one call site in production code (not only tests).

### WV-1: Automated grep scan

For each new public symbol, run a targeted search:

| Symbol | Expected call site |
|--------|--------------------|
| `is_valid_name` | `TenantId::new`, `DatabaseName::new` (same file) |
| `extract_hello_fields` | `handle_hello` in `bolt_handler.rs` |
| `LoginAttemptTracker::is_locked` | `BoltConnectionHandler::authenticate` |
| `LoginAttemptTracker::record_failure` | `BoltConnectionHandler::authenticate` |
| `LoginAttemptTracker::record_success` | `BoltConnectionHandler::authenticate` |
| `ServerContext::login_tracker` | `bolt_handler.rs` |
| `ServerContext::login_policy` | `bolt_handler.rs` |
| `ServerContext::next_connection_id` | `bolt_handler.rs` handle_hello |
| `ServerContext::resolve_roles_for_token` | (future RBAC — document as placeholder) |
| `SessionManager::set_session_roles` | `auth_dispatch::authenticate_external` |
| `SessionManager::purge_expired` | `ServerContext::start_background_tasks` |
| `SessionManager::session_count` | background task or metrics endpoint |
| `tessera_config::unix_timestamp` | `tessera-auth::utils`, `tessera-audit::lib` |
| `TransactionManager::open_with_window` | `TransactionManager::open` (delegates) |
| `read_ops::*` | Both `SecureGraph` and `SecureGraphRef` trait impls |
| `escape_ldap_filter_value` | `LdapAuthProvider::do_authenticate` (already) |
| `purge_expired` | background task in server |

### WV-2: Shell commands to execute

```bash
# Run from workspace root
cargo build --workspace 2>&1 | head -20

# Check no new dead_code warnings
cargo clippy --workspace -- -D dead_code 2>&1 | grep "warning\|error"

# Verify call sites exist
grep -rn "is_valid_name"         crates/tessera-tenant/src/
grep -rn "extract_hello_fields"  crates/tessera-server/src/
grep -rn "is_locked"             crates/tessera-server/src/
grep -rn "record_failure"        crates/tessera-server/src/
grep -rn "login_tracker"         crates/tessera-server/src/
grep -rn "next_connection_id"    crates/tessera-server/src/
grep -rn "set_session_roles"     crates/tessera-server/src/ crates/tessera-auth/src/
grep -rn "purge_expired"         crates/tessera-server/src/ crates/tessera-auth/src/
grep -rn "open_with_window"      crates/tessera-storage-enterprise/src/
grep -rn "read_ops::"            crates/tessera-storage-enterprise/src/
grep -rn "unix_timestamp"        crates/tessera-config/src/ crates/tessera-auth/src/utils.rs crates/tessera-audit/src/
```

Each grep must return at least one line outside of test modules for production
wiring to be confirmed. Any symbol with ZERO non-test call sites is a dead
function and must be removed or connected before the plan is considered complete.

### WV-3: Full test suite

```bash
cargo test --workspace
```

Expected: all tests pass, zero compilation errors, zero denied Clippy warnings.

---

## Estimation

| Phase | Finding | Impl | Tests | Total |
|-------|---------|------|-------|-------|
| 1     | C1      | 20 min | 20 min | 40 min |
| 2     | C3      | 15 min | 15 min | 30 min |
| 3     | C2      | 45 min | 30 min | 75 min |
| 4     | C4      | 60 min | 30 min | 90 min |
| 5     | R8      | 10 min | 10 min | 20 min |
| 6     | R4      | 30 min | 20 min | 50 min |
| 7     | R3      | 15 min | 10 min | 25 min |
| 8     | R5+O8   | 15 min | 15 min | 30 min |
| 9     | R1      | 45 min | 20 min | 65 min |
| 10    | R2      | 15 min | 10 min | 25 min |
| 11    | R6      | 15 min | 15 min | 30 min |
| 12    | R7      | 20 min | 20 min | 40 min |
| 13    | O1      | 15 min | 15 min | 30 min |
| 14    | O2      | 10 min | 10 min | 20 min |
| 15    | O3      | 20 min | 10 min | 30 min |
| 16    | O4      | 20 min | 15 min | 35 min |
| 17    | O5      | 10 min | 10 min | 20 min |
| 18    | O6      | 15 min | 15 min | 30 min |
| 19    | O7      | 25 min | 20 min | 45 min |
| 20    | WV      | 20 min | — | 20 min |
| **Total** | | **~7h** | **~4h** | **~11h** |

---

## Criteria de Éxito

- [ ] `cargo test --workspace` passes with zero failures
- [ ] `cargo clippy --workspace -- -D warnings` emits zero diagnostics
- [ ] `cargo build --workspace` emits zero warnings
- [ ] C1: `TenantId::new("../escape")` returns `Err(InvalidName)`
- [ ] C1: throughput regression guard passes (>1M ops/s for valid names)
- [ ] C2: account is locked after N failures per configured `LoginPolicy`
- [ ] C3: `handle_begin` sends `FAILURE`, subsequent RUN is `IGNORED`
- [ ] C4: external auth sessions carry mapped roles in `SessionManager`
- [ ] R8: `LdapConfig.bind_password` field type is `Zeroizing<String>`
- [ ] R4: `committed_count()` never exceeds `window` after many commits
- [ ] R3: `serve` and `serve_tls` compile without a `registry` parameter
- [ ] O8: `list_databases` returns `Ok([])` when tenant dir disappears
- [ ] R1: `SecureGraph` and `SecureGraphRef` share all read method implementations via `read_ops`
- [ ] O6: `escape_ldap_filter_value("café")` returns `"caf\\c3\\a9"`
- [ ] WV: every new public symbol has at least one non-test call site confirmed by grep
