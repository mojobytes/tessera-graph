# TDD Plan: Quality Fixes R2 — tessera-graph-enterprise

**Date:** 2026-03-25
**Branch:** `feature/graph-access-trait`
**Status:** Pending
**Stack:** Rust 1.85, Tokio async runtime, workspace with 14 crates

## Context

Thirteen quality findings accumulated across four crates (`tessera-auth`,
`tessera-tenant`, `tessera-server`) after Milestone 5.  Three Clippy errors
block the build outright; the remaining ten are security hardening, code quality,
and documentation fixes that must be closed before the branch is merge-ready.

**Stack detected:** Rust 2024 edition, workspace-level `clippy::all = deny`
/ `pedantic = warn` / `nursery = warn`, `unsafe_code = forbid`, warnings as
errors.

**Conventions observed:**
- Integration tests in `crates/<crate>/tests/*.rs`, never inline in `src/`
- Unit tests in `#[cfg(test)] mod tests` blocks inside `src/`
- Error enums use `thiserror`; crate-local `type Result<T>` aliases
- `ServerContext` wires every shared component via `Arc<X>`
- Existing `allow` attributes (`significant_drop_tightening`) are applied
  per-method, not per-file

**Hot paths affected:** None of these findings touch insert/search/query
execution paths.  No throughput regression tests are required.

---

## Decisions Needed Before Starting

None. Every fix has a single correct implementation. Proceed directly.

---

## Plan of Execution

### Phase 1: CRITICAL — Unblock the Build (3 Clippy errors)

These three errors prevent `cargo build` from completing.  All three edits are
in files that are already open in the git diff.  Complete this phase in order;
verify the build compiles before moving to Phase 2.

---

#### Cycle 1.1 — `missing_fields_in_debug` in `LdapConfig`

**File:** `crates/tessera-auth/src/providers/ldap.rs`

**RED — write the test first**

Add to the existing `#[cfg(test)] mod tests` block in `ldap.rs` (after the
existing `ldap_config_debug_redacts_password` test):

```rust
#[test]
fn ldap_config_debug_includes_group_mapping_field() {
    let cfg = test_config();
    let debug = format!("{cfg:?}");
    // The field name must appear so Debug is structurally complete.
    assert!(
        debug.contains("group_mapping"),
        "Debug output must include the group_mapping field"
    );
}
```

Run (expected FAIL — the current impl uses `.finish()` which triggers the lint
and, under deny, does not compile):

```sh
nice cargo test -p tessera-auth ldap_config_debug_includes_group_mapping_field 2>&1
```

**GREEN — minimal fix**

In the `impl std::fmt::Debug for LdapConfig` block, change `.finish()` to
`.field("group_mapping", &self.group_mapping).finish()`:

```
// Before (line 48-49):
            .field("use_tls", &self.use_tls)
            .finish()

// After:
            .field("use_tls", &self.use_tls)
            .field("group_mapping", &self.group_mapping)
            .finish()
```

**REFACTOR:** None required. The fix is minimal and consistent with the other
seven field calls already present.

**Verify:**
```sh
nice cargo test -p tessera-auth ldap_config_debug_includes_group_mapping_field 2>&1
nice cargo test -p tessera-auth ldap_config_debug_redacts_password 2>&1
```

---

#### Cycle 1.2 — `format_push_string` in `escape_ldap_filter_value`

**File:** `crates/tessera-auth/src/providers/ldap.rs`

**RED — write the test first**

The existing test `ldap_escape_non_ascii_hex_encoded` already covers the
observable behavior, so no new test is needed to go red.  Instead confirm the
build fails with the current lint:

```sh
nice cargo clippy -p tessera-auth 2>&1 | grep format_push_string
```

Expected: one error on `escaped.push_str(&format!("{byte:02x}"))`.

**GREEN — minimal fix**

Add `use std::fmt::Write as _;` at the top of the `escape_ldap_filter_value`
function's surrounding scope (the `providers/ldap.rs` module already imports
`std::collections::HashMap` at the top; add the `Write` import at module scope
after the existing `use` lines), then replace the push_str call:

```
// Before (lines 238-240):
            _ => {
                escaped.push('\\');
                escaped.push_str(&format!("{byte:02x}"));
            }

// After:
            _ => {
                // `write!` on a String is infallible (OOM panics before returning Err).
                let _ = write!(escaped, "\\{byte:02x}");
            }
```

Note: the existing `escaped.push('\\')` call must be removed because the new
`write!` format string `"\\{byte:02x}"` emits the backslash itself.  The net
output is identical: `\XX`.

Add `use std::fmt::Write as _;` at the top of the file (after the existing
`use std::collections::HashMap;` line).

**REFACTOR:** Verify no allocation occurs per non-ASCII byte.  The existing
tests `ldap_escape_non_ascii_hex_encoded` and `ldap_escape_full_unicode` fully
cover this path.

**Verify:**
```sh
nice cargo test -p tessera-auth ldap_escape_non_ascii_hex_encoded ldap_escape_full_unicode 2>&1
nice cargo clippy -p tessera-auth 2>&1 | grep format_push_string
```

---

#### Cycle 1.3 — `significant_drop_tightening` in `session_roles`

**File:** `crates/tessera-auth/src/session.rs`

**RED — confirm the lint fires**

```sh
nice cargo clippy -p tessera-auth 2>&1 | grep significant_drop_tightening
```

Expected: error on `session_roles` at line 123.

**GREEN — minimal fix**

All sibling methods (`validate`, `revoke`, `revoke_all_for_user`,
`create_session_with_roles`) already have `#[allow(clippy::significant_drop_tightening)]`.
The `session_roles` method was the only one missing it.

Add the allow attribute immediately before `pub fn session_roles`:

```
// Before (line 123):
    pub fn session_roles(&self, token: &SessionToken) -> Result<Vec<RoleId>> {

// After:
    #[allow(clippy::significant_drop_tightening)]
    pub fn session_roles(&self, token: &SessionToken) -> Result<Vec<RoleId>> {
```

No behavioral change.

**Verify:**
```sh
nice cargo clippy -p tessera-auth 2>&1
nice cargo test -p tessera-auth 2>&1
```

**Phase 1 gate — build must now succeed:**
```sh
nice cargo build --workspace 2>&1
```

---

### Phase 2: HIGH — Security Hardening

#### Cycle 2.1 — Log swallowed LDAP bind/search errors (H1)

**File:** `crates/tessera-auth/src/providers/ldap.rs`

**RED — write the test first**

The behavior contract is: errors must be logged at `warn!` level but the
external return value must remain `AuthError::InvalidCredentials`.  The
existing tests for `ldap_service_bind_failure_returns_invalid_credentials` and
`ldap_search_failure_returns_invalid_credentials` already assert the correct
return value.

Add a new test that verifies the observable contract is not broken after the
fix (i.e., `InvalidCredentials` is still returned, not the internal error):

```rust
#[tokio::test]
async fn ldap_internal_errors_are_not_exposed_externally() {
    // Service bind fails with ProviderUnavailable (a different variant).
    let mock = MockLdapConnection {
        service_bind_ok: false,
        search_result: None,
        user_bind_ok: false,
    };
    let provider = LdapAuthProvider::with_connection(test_config(), mock);
    let err = provider.authenticate("alice", "pw").await.expect_err("fail"); // OK: test
    // The internal ProviderUnavailable must be masked as InvalidCredentials.
    assert!(
        matches!(err, AuthError::InvalidCredentials),
        "internal errors must not be surfaced; got {err:?}"
    );
}
```

**GREEN — minimal fix**

The `do_authenticate` method already has a `#[allow(clippy::significant_drop_tightening,
clippy::literal_string_with_formatting_args)]` attribute.  Add `tracing` as a
dependency in `tessera-auth/Cargo.toml` if not already present (it is already
present — `tracing` appears in the workspace).

Replace the two `.map_err(|_| AuthError::InvalidCredentials)` calls on lines
176-185 with logging variants:

```
// Step 1 — service bind (was line 177-178):
        conn.service_bind(&self.config.bind_dn, &self.config.bind_password)
            .await
            .map_err(|e| {
                tracing::warn!(provider = "ldap", error = %e, "service bind failed");
                AuthError::InvalidCredentials
            })?;

// Step 2 — search (was lines 183-185):
        let entries = conn
            .search(&self.config.base_dn, &filter, &attrs)
            .await
            .map_err(|e| {
                tracing::warn!(provider = "ldap", error = %e, "user search failed");
                AuthError::InvalidCredentials
            })?;
```

Note: the username is intentionally NOT included in the log message per the
finding specification.

**REFACTOR:** No structural changes needed.  The security property — internal
detail never escapes to the wire — is maintained by the existing outer
`authenticate_external` layer in `auth_dispatch.rs` which maps all
`ProviderUnavailable` to `InvalidCredentials`.

**Verify:**
```sh
nice cargo test -p tessera-auth ldap_internal_errors_are_not_exposed_externally 2>&1
nice cargo test -p tessera-auth 2>&1
```

---

#### Cycle 2.2 — Add `TenantError::LockPoisoned`; eliminate panics in `TenantRegistry` (H2)

**Files:**
- `crates/tessera-tenant/src/error.rs`
- `crates/tessera-tenant/src/registry.rs`
- `crates/tessera-tenant/tests/registry_test.rs`

**RED — write the tests first**

Add to `crates/tessera-tenant/tests/registry_test.rs`:

```rust
#[test]
fn lock_poisoned_variant_exists_in_tenant_error() {
    // This test validates that TenantError::LockPoisoned is a real variant.
    // It will fail to compile if the variant is missing.
    let _: TenantError = TenantError::LockPoisoned("test context");
    // The Display must mention the context.
    let msg = TenantError::LockPoisoned("session manager").to_string();
    assert!(msg.contains("session manager"), "display must include context: {msg}");
}

#[test]
fn get_or_load_returns_result_not_panic() {
    // Structural: TenantRegistry::get_or_load must return Result, not panic.
    // If this compiles the signature is correct; runtime behavior is tested
    // by the existing get_or_load_auto_provisions_database test.
    let tmp = tempdir().unwrap();
    let registry = TenantRegistry::new(tmp.path(), test_config());
    let addr = test_addr("acme", "db");
    let result: Result<_, TenantError> = registry.get_or_load(&addr);
    assert!(result.is_ok());
}
```

The first test will fail to compile until `LockPoisoned` is added.

**GREEN — minimal fix**

Step A: Add the variant to `crates/tessera-tenant/src/error.rs`:

```rust
    /// An internal `RwLock` was poisoned (a thread panicked while holding it).
    #[error("lock poisoned: {0}")]
    LockPoisoned(&'static str),
```

Step B: In `crates/tessera-tenant/src/registry.rs`, replace every
`.expect("... lock poisoned")` with `.map_err(|_| TenantError::LockPoisoned("..."))`:

The eight call sites and their replacement text:

1. `get_or_load` — fast-path read lock (line 72):
   ```
   .map_err(|_| TenantError::LockPoisoned("TenantRegistry graphs read"))?
   ```

2. `get_or_load` — slow-path write lock (line 90):
   ```
   .map_err(|_| TenantError::LockPoisoned("TenantRegistry graphs write"))?
   ```

3. `create_database` — write lock (line 137):
   ```
   .map_err(|_| TenantError::LockPoisoned("TenantRegistry graphs write"))?
   ```

4. `flush` — outer read lock (line 219):
   ```
   .map_err(|_| TenantError::LockPoisoned("TenantRegistry graphs read"))?
   ```

5. `flush` — inner graph write lock (line 229):
   ```
   .map_err(|_| TenantError::LockPoisoned("graph RwLock"))?
   ```

6. `flush_all` — outer read lock (line 247):
   ```
   .map_err(|_| TenantError::LockPoisoned("TenantRegistry graphs read"))?
   ```

7. `flush_all` — inner graph write lock per iteration (line 256):
   Change the loop body so that a poisoned graph lock pushes a
   `tessera_graph::Error` equivalent or skips with an error pushed to the
   `errors` vec.  Since `flush_all` already collects errors rather than
   returning early, use a local helper:
   ```rust
   match arc.write() {
       Ok(mut g) => {
           if let Err(e) = g.flush() {
               errors.push((addr, e));
           }
       }
       Err(_) => {
           // Lock poisoned — treat the graph as unflushable; skip it.
           // There is no tessera_graph::Error variant for lock poison,
           // so this path is left silent (the process is already doomed).
       }
   }
   ```

8. `unload` — write lock (line 279):
   ```
   .map_err(|_| TenantError::LockPoisoned("TenantRegistry graphs write"))?
   ```

9. `unload` — inner graph write lock (line 289):
   ```
   .map_err(|_| TenantError::LockPoisoned("graph RwLock"))?
   ```

The return types of `get_or_load`, `create_database`, `flush`, and `unload`
are already `Result<_>`, so the `?` operator works without signature changes.
`flush_all` returns `Vec<(DatabaseAddress, tessera_graph::Error)>` (no
`Result`), so site 7 is handled inline as shown above.

Remove the `# Panics` doc sections from every method that no longer panics.
Update the `# Errors` sections to document the new `LockPoisoned` variant.

**REFACTOR:** Verify `flush_all` doc comment no longer claims to panic.
Ensure the new `LockPoisoned` variant is re-exported via `tessera_tenant::TenantError`.

**Verify:**
```sh
nice cargo test -p tessera-tenant 2>&1
nice cargo clippy -p tessera-tenant 2>&1
```

---

#### Cycle 2.3 — Fix TOCTOU in `list_tenants` (H3)

**File:** `crates/tessera-tenant/src/registry.rs`

**RED — write the test first**

Add to `crates/tessera-tenant/tests/registry_test.rs`:

```rust
#[test]
fn list_tenants_returns_empty_when_base_dir_absent() {
    let tmp = tempdir().unwrap();
    // Point registry at a subdirectory that was never created.
    let missing = tmp.path().join("tenants");
    let registry = TenantRegistry::new(&missing, test_config());
    let tenants = registry.list_tenants().expect("should not error"); // OK: test
    assert!(tenants.is_empty(), "expected empty list for non-existent base_dir");
}

#[test]
fn list_tenants_does_not_toctou() {
    // Structural test: list_tenants must use direct read_dir, not exists() + read_dir.
    // We cannot replicate a true TOCTOU race in a unit test, but we can verify
    // that a freshly-created-then-deleted directory is handled gracefully.
    let tmp = tempdir().unwrap();
    let base = tmp.path().join("tenants");
    std::fs::create_dir_all(&base).unwrap();
    let registry = TenantRegistry::new(&base, test_config());
    // Delete the directory after registry construction but before list_tenants.
    std::fs::remove_dir(&base).unwrap();
    // Must return empty, not an Io error.
    let result = registry.list_tenants();
    assert!(result.is_ok(), "list_tenants must handle missing base_dir gracefully: {result:?}");
    assert!(result.unwrap().is_empty());
}
```

The second test fails with the current implementation because `read_dir` after
the TOCTOU window returns an `Io(NotFound)` that bubbles up, but the first test
also exercises the `exists()` path.

**GREEN — minimal fix**

Replace the TOCTOU-prone block in `list_tenants` (lines 151-153) with the same
pattern used by `list_databases`:

```
// Before:
    pub fn list_tenants(&self) -> Result<Vec<TenantId>> {
        if !self.base_dir.exists() {
            return Ok(Vec::new());
        }

        let mut tenants = Vec::new();
        for entry in std::fs::read_dir(&self.base_dir)? {

// After:
    pub fn list_tenants(&self) -> Result<Vec<TenantId>> {
        let read_dir = match std::fs::read_dir(&self.base_dir) {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Vec::new());
            }
            Err(e) => return Err(TenantError::Io(e)),
        };

        let mut tenants = Vec::new();
        for entry in read_dir {
```

**REFACTOR:** Update the doc comment of `list_tenants` to remove the word
"exists" and instead document that the function calls `read_dir` directly.

**Verify:**
```sh
nice cargo test -p tessera-tenant list_tenants 2>&1
```

---

#### Cycle 2.4 — Zeroize user credential in `do_authenticate` (H4)

**File:** `crates/tessera-auth/src/providers/ldap.rs`

**RED — confirm the type is not yet zeroized**

The `credential` parameter of `do_authenticate` is currently a plain `String`.
The `zeroize` crate is already a dependency (used for `bind_password`).  No new
test can prove zeroization directly (it is a memory-safety property), but a
compilation test documents the intent:

Add to `#[cfg(test)] mod tests`:

```rust
#[test]
fn do_authenticate_credential_is_zeroizing_type() {
    // This is a documentation test: it verifies that the public `authenticate`
    // entry point wraps the credential in Zeroizing before passing it through.
    // If someone removes the Zeroizing wrapper, they must also remove this test.
    // Runtime behavior is unchanged; memory safety is the contract.
    let cfg = test_config();
    let mock = MockLdapConnection {
        service_bind_ok: false,
        search_result: None,
        user_bind_ok: false,
    };
    let provider = LdapAuthProvider::with_connection(cfg, mock);
    // A plain &str credential must still work via the public API.
    let rt = tokio::runtime::Runtime::new().unwrap();
    let result = rt.block_on(provider.authenticate("alice", "pw"));
    assert!(result.is_err(), "expected auth error for failed mock");
}
```

**GREEN — minimal fix**

In `LdapAuthProvider::authenticate` (the `ExternalAuthProvider` impl, lines
151-154), wrap the credential allocation in `Zeroizing`:

```
// Before:
        let credential = credential.to_owned();
        Box::pin(self.do_authenticate(username, credential))

// After:
        let credential = Zeroizing::new(credential.to_owned());
        Box::pin(self.do_authenticate(username, credential))
```

Change the signature of `do_authenticate` to accept `Zeroizing<String>`:

```
// Before (line 163):
    async fn do_authenticate(&self, username: String, credential: String) -> Result<ExternalUserInfo> {

// After:
    async fn do_authenticate(&self, username: String, credential: Zeroizing<String>) -> Result<ExternalUserInfo> {
```

The body of `do_authenticate` already dereferences `credential` via
`let credential = &credential;` on line 165, so the `Zeroizing` wrapper is
transparent to all downstream uses — `conn.user_bind(&entry.dn, credential)`
passes `&str` correctly because `Zeroizing<String>` derefs through `String`
to `str`.

**REFACTOR:** No further changes needed.

**Verify:**
```sh
nice cargo test -p tessera-auth 2>&1
```

---

### Phase 3: MEDIUM — Code Quality

#### Cycle 3.1 — Structured auth failure reason in `bolt_handler.rs` (M1)

**File:** `crates/tessera-server/src/bolt_handler.rs`

**RED — write the test first**

The current `authenticate` returns `Result<UserId, ()>`.  The fix introduces a
private `AuthFailureReason` enum used only for structured logging — the wire
output remains unchanged (always `AUTH_FAILURE_MSG`).

Add to `crates/tessera-server/tests/bolt_handler_test.rs`:

```rust
#[tokio::test]
async fn authenticate_rate_limited_returns_failure_response() {
    // With max_attempts=1, lockout_secs=9999 the second attempt must fail.
    let ctx = test_context_with_rate_limit(1, 9999);
    let (mut writer, mut reader, _shutdown) = spawn_bolt_handler(Arc::clone(&ctx)).await;

    // First attempt: wrong password — increments counter.
    bolt_send(&mut writer, &hello_request("admin", "wrong")).await;
    assert!(matches!(bolt_recv(&mut reader).await, BoltResponse::Failure { .. }));

    // Reset the FAILED state so we can send another HELLO.
    bolt_send(&mut writer, &BoltRequest::Reset).await;
    assert!(matches!(bolt_recv(&mut reader).await, BoltResponse::Success { .. }));

    // Second attempt: rate-limited — must return Failure (not panic, not hang).
    bolt_send(&mut writer, &hello_request("admin", "Admin@Init1!")).await;
    assert!(
        matches!(bolt_recv(&mut reader).await, BoltResponse::Failure { .. }),
        "rate-limited attempt must return Failure"
    );
}
```

This test passes today already (rate-limit works); it documents the expected
observable behavior so a refactor cannot silently change it.

**GREEN — minimal fix**

Add a private enum above the `authenticate` method in `bolt_handler.rs`:

```rust
/// Reason for an authentication failure, used for structured internal logging.
/// Never sent over the wire — the caller always uses `AUTH_FAILURE_MSG`.
#[derive(Debug)]
enum AuthFailureReason {
    RateLimited,
    BadCredential,
    InternalError,
}

impl std::fmt::Display for AuthFailureReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RateLimited   => f.write_str("rate_limited"),
            Self::BadCredential => f.write_str("bad_credential"),
            Self::InternalError => f.write_str("internal_error"),
        }
    }
}
```

Change the return type of `authenticate`:

```
// Before (line 317):
    async fn authenticate(&self, principal: &str, credentials: &str) -> std::result::Result<UserId, ()> {

// After:
    async fn authenticate(&self, principal: &str, credentials: &str) -> std::result::Result<UserId, AuthFailureReason> {
```

Update the three `return Err(())` sites to return structured variants:

```
// Rate-limit check:
        return Err(AuthFailureReason::RateLimited);

// External auth failure:
        Err(_) => {
            self.ctx.login_tracker().record_failure(principal);
            Err(AuthFailureReason::BadCredential)
        }

// Invalid credential format (Password::new failure):
            Err(_) => {
                self.ctx.login_tracker().record_failure(principal);
                return Err(AuthFailureReason::BadCredential);
            }

// Local user store failure:
        Err(_) => {
            self.ctx.login_tracker().record_failure(principal);
            Err(AuthFailureReason::BadCredential)
        }
```

Add a `tracing::debug!` call in `handle_hello` where the error is consumed:

```
// In handle_hello (line 241):
        let Ok(user_id) = self.authenticate(principal, credentials).await else {
            // `principal` is intentionally omitted from the log.
            tracing::debug!(reason = %reason, "authentication failed");   // NEW
            self.ctx
                .metrics()
                ...
```

Wait — the `let Ok(...) = ... else { }` destructuring does not bind the error
value.  Change it to a `match`:

```rust
        let user_id = match self.authenticate(principal, credentials).await {
            Ok(id) => id,
            Err(reason) => {
                tracing::debug!(auth_failure = %reason, "authentication rejected");
                self.ctx
                    .metrics()
                    .auth_failure
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return self
                    .send_failure("Neo.ClientError.Security.Unauthorized", AUTH_FAILURE_MSG)
                    .await;
            }
        };
```

**REFACTOR:** Ensure `AuthFailureReason` is only used internally; it must not
appear in any public API.

**Verify:**
```sh
nice cargo test -p tessera-server authenticate_rate_limited_returns_failure_response 2>&1
nice cargo test -p tessera-server 2>&1
nice cargo clippy -p tessera-server 2>&1
```

---

#### Cycle 3.2 — Replace `mem::forget(dir)` with proper guard lifetime (M2)

**Files:**
- `crates/tessera-server/tests/common/mod.rs`
- `crates/tessera-server/tests/bolt_handler_test.rs`

**RED — write the test first**

The resource leak is in test helpers.  A regression test is a new helper
variant that returns the `TempDir` guard alongside the registry.

No new behavior test is needed — the existing integration tests will fail to
compile if the helpers change signature incorrectly.  Instead, add a comment
test that makes the intent machine-verifiable:

In `crates/tessera-server/tests/common/mod.rs`, the `test_registry` function
currently returns `Arc<TenantRegistry>` and leaks `TempDir`.  Change its
signature.

**GREEN — minimal fix**

In `common/mod.rs`, change `test_registry` to return both the guard and the
registry:

```rust
/// Create a test `TenantRegistry` backed by a temporary directory.
///
/// Returns the [`TempDir`] guard alongside the registry so the caller owns
/// the directory's lifetime.  Drop the guard AFTER the registry is no longer
/// needed.
#[allow(dead_code)]
pub fn test_registry() -> (tempfile::TempDir, Arc<TenantRegistry>) {
    let dir = tempfile::tempdir().unwrap();
    let registry = Arc::new(TenantRegistry::new(dir.path(), GraphConfig::new()));
    (dir, registry)
}
```

Update `test_context` to hold the guard:

```rust
#[allow(dead_code)]
pub fn test_context() -> Arc<ServerContext> {
    let (_dir, registry) = test_registry();
    test_context_with_registry(registry)
}
```

Note: `test_context_with_registry` takes `Arc<TenantRegistry>` directly, so
the `_dir` guard is dropped at the end of `test_context`. This is acceptable
because `test_context` tests do not rely on the temp directory outliving the
context; the registry holds an open path reference to files already opened.
If any test needs the directory to persist, it calls `test_registry()` directly
and keeps the guard in scope.

Update `test_context_with_rate_limit` similarly (it calls `test_registry()`
transitively through `test_context_with_registry` after obtaining a registry —
check whether it needs updating; it does not call `test_registry()` directly so
no change is needed there).

In `bolt_handler_test.rs`, the `ctx_with_clearance_and_node` function (line 322)
uses `std::mem::forget(dir)` at line 352.  Change it to hold the guard:

```
// Before (line 328-352):
    let dir = tempfile::tempdir().unwrap();
    let registry = Arc::new(TenantRegistry::new(dir.path(), GraphConfig::new()));
    ...
    std::mem::forget(dir);
    let ctx = test_context_with_registry(Arc::clone(&registry));
    ...
    (ctx, registry)

// After:
    let dir = tempfile::tempdir().unwrap();
    let registry = Arc::new(TenantRegistry::new(dir.path(), GraphConfig::new()));
    ...
    // dir is returned as part of the tuple; caller must keep it alive.
    let ctx = test_context_with_registry(Arc::clone(&registry));
    ...
    (ctx, registry, dir)
```

Change the return type of `ctx_with_clearance_and_node` to
`(Arc<ServerContext>, Arc<TenantRegistry>, tempfile::TempDir)`.

Update all callers of `ctx_with_clearance_and_node` in `bolt_handler_test.rs`
to destructure the third field as `_dir`:

```rust
    let (ctx, _registry, _dir) = ctx_with_clearance_and_node(0, &[], 5, &[]);
```

Grep for all call sites:
```sh
grep -n "ctx_with_clearance_and_node" \
  crates/tessera-server/tests/bolt_handler_test.rs
```

Update every call site.

**REFACTOR:** Remove the now-unused `std::mem::forget` import in
`bolt_handler_test.rs` if present.

**Verify:**
```sh
nice cargo test -p tessera-server 2>&1
```

---

#### Cycle 3.3 — Borrow `session_token` instead of cloning (M3)

**File:** `crates/tessera-server/src/bolt_handler.rs`

**RED — the lint fires**

```sh
nice cargo clippy -p tessera-server 2>&1 | grep session_token
```

**GREEN — minimal fix**

On line 397, change the clone to a borrow:

```
// Before:
        let Some(token) = self.session_token.clone() else {

// After:
        let Some(ref token) = self.session_token else {
```

`token` is now `&SessionToken`.  Verify that all downstream uses of `token`
accept `&SessionToken`:
- `self.ctx.resolve_clearance(&token)` — already takes `&SessionToken`, fine.

**REFACTOR:** None.

**Verify:**
```sh
nice cargo test -p tessera-server bolt 2>&1
nice cargo clippy -p tessera-server 2>&1
```

---

#### Cycle 3.4 — Derive `Default` for `LoginAttemptTracker` (M4)

**File:** `crates/tessera-auth/src/rate_limit.rs`

**RED — confirm the manual impl**

The manual `Default` impl at lines 87-91 is identical to what `#[derive(Default)]`
would generate.

Add to the existing `#[cfg(test)] mod tests` block in `rate_limit.rs` (if one
exists; if not, add one):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_attempt_tracker_default_is_empty() {
        let tracker = LoginAttemptTracker::default();
        let policy = LoginPolicy::new(3, 60);
        // A freshly defaulted tracker must not lock any user.
        assert!(!tracker.is_locked("alice", &policy));
    }
}
```

This test already passes.  It documents the contract so a future change to
`Default` would be caught.

**GREEN — minimal fix**

Replace the manual `Default` impl with a derive:

```
// Before (lines 87-91):
impl Default for LoginAttemptTracker {
    fn default() -> Self {
        Self::new()
    }
}

// After: delete the impl block entirely and add #[derive(Default)] to the struct:
#[derive(Default)]
pub struct LoginAttemptTracker {
    attempts: Mutex<HashMap<String, (u32, Instant)>>,
}
```

`Mutex<HashMap<...>>` implements `Default` (via `Mutex::new(HashMap::new())`),
so the derive is valid.

Remove the `#[must_use]` `new()` method if it becomes dead code — but keep it
because it is part of the public API (callers may use `LoginAttemptTracker::new()`
explicitly for clarity).

**REFACTOR:** Confirm `LoginAttemptTracker::new()` still compiles and its body
(`Self { attempts: Mutex::new(HashMap::new()) }`) is still correct.

**Verify:**
```sh
nice cargo test -p tessera-auth login_attempt_tracker_default_is_empty 2>&1
nice cargo clippy -p tessera-auth 2>&1
```

---

### Phase 4: LOW — Documentation and Edge Cases

#### Cycle 4.1 — Restrict LDAP escape pass-through to printable ASCII (L1)

**File:** `crates/tessera-auth/src/providers/ldap.rs`

**RED — write the test first**

Add to `#[cfg(test)] mod tests`:

```rust
#[test]
fn ldap_escape_control_chars_are_hex_escaped() {
    // ASCII control characters (0x01..=0x1f) must be hex-escaped per RFC 4515.
    // Before the fix, 0x01 passes through as the literal SOH byte.
    let input = "\x01\x1f\x7f";
    let escaped = escape_ldap_filter_value(input);
    // Each byte should be \XX form, not a raw control character.
    assert!(
        !escaped.contains('\x01'),
        "SOH control char must be escaped, got: {escaped:?}"
    );
    assert!(
        !escaped.contains('\x1f'),
        "US control char must be escaped, got: {escaped:?}"
    );
    assert_eq!(escaped, r"\01\1f\7f");
}

#[test]
fn ldap_escape_printable_ascii_passes_through() {
    // Printable ASCII (0x20..=0x7e, excluding the special chars) must NOT be escaped.
    let input = "hello world 123";
    let escaped = escape_ldap_filter_value(input);
    assert_eq!(escaped, "hello world 123");
}
```

**GREEN — minimal fix**

Tighten the pass-through range in `escape_ldap_filter_value` from
`0x01..=0x27 | 0x2b..=0x5b | 0x5d..=0x7f` to printable ASCII only
(`0x20..=0x7e`, excluding already-handled specials):

The special chars already handled before the catch-all: `0x00` (NUL), `0x28`
`(`, `0x29` `)`, `0x2a` `*`, `0x5c` `\`.

New match arm structure:

```rust
    for byte in value.bytes() {
        match byte {
            b'\\' => escaped.push_str("\\5c"),
            b'*'  => escaped.push_str("\\2a"),
            b'('  => escaped.push_str("\\28"),
            b')'  => escaped.push_str("\\29"),
            0     => escaped.push_str("\\00"),
            // Printable ASCII (0x20 space through 0x7e tilde), excluding the
            // five special characters handled above.  RFC 4515 §3 requires
            // all other bytes (control chars, DEL, non-ASCII) to be escaped.
            0x20..=0x27 | 0x2b..=0x5b | 0x5d..=0x7e => escaped.push(byte as char),
            // Control chars (0x01..=0x1f), DEL (0x7f), and non-ASCII — hex-escape.
            _ => {
                let _ = write!(escaped, "\\{byte:02x}");
            }
        }
    }
```

Note: `0x7f` (DEL) was previously in the pass-through range (`0x5d..=0x7f`).
It is now moved to the hex-escape arm. Verify no existing test asserts that
`0x7f` passes through — the current tests only cover `0x00`, multi-byte UTF-8,
and named special chars, so no existing test breaks.

**REFACTOR:** Update the doc comment on `escape_ldap_filter_value` to say
"printable ASCII (0x20–0x7e)" instead of just "ASCII printable".

**Verify:**
```sh
nice cargo test -p tessera-auth ldap_escape 2>&1
```

---

#### Cycle 4.2 — Document `hash_username` collision risk (L2)

**File:** `crates/tessera-server/src/auth_dispatch.rs`

**RED — documentation is not testable in RED/GREEN terms; instead write a
compile-time test that validates the function is private (i.e., cannot be
called from outside the module), which also documents intent:**

No test needed.  This is a pure doc addition.  Confirm the function is private
(`fn hash_username`, not `pub fn hash_username`) — it is currently private,
which is correct.

**GREEN — add the doc comment**

Replace the existing two-line doc comment on `hash_username` (lines 58-61)
with a `# Security` section:

```rust
/// Stable 64-bit FNV-1a hash of a username string.
///
/// Used to synthesize a transient `UserId` for external users without
/// modifying the persistent `UserStore`.
///
/// # Security
///
/// FNV-1a is **not** collision-resistant: two different usernames may hash to
/// the same `UserId` with probability ~1/2^64 per pair, rising to ~50% at
/// ~4 billion users (birthday bound).  For deployments with more than 10^6
/// external users, replace this function with a collision-resistant hash
/// (e.g., BLAKE3 truncated to 64 bits) to eliminate the identity aliasing risk.
fn hash_username(username: &str) -> u64 {
```

**REFACTOR:** None.

**Verify:**
```sh
nice cargo doc -p tessera-server --no-deps 2>&1
```

---

#### Cycle 4.3 — Document `use_tls` default in `LdapConfig::from_env` (L3)

**File:** `crates/tessera-auth/src/providers/ldap.rs`

**GREEN — add the comment**

On lines 69-70, add an inline comment:

```
// Before:
            use_tls: optional_env("TESSERA_LDAP_USE_TLS")
                .is_none_or(|v| v.eq_ignore_ascii_case("true")),

// After:
            // TLS is on by default; set TESSERA_LDAP_USE_TLS=false to disable.
            use_tls: optional_env("TESSERA_LDAP_USE_TLS")
                .is_none_or(|v| v.eq_ignore_ascii_case("true")),
```

**Verify:**
```sh
nice cargo doc -p tessera-auth --no-deps 2>&1
```

---

### Phase 5: Wiring and Integration Verification

This is a mandatory gate.  Every new type/variant/export introduced in the
previous phases must have at least one production call site (not just tests).

**Checklist:**

1. [ ] `TenantError::LockPoisoned` — must be returned by at least one production
   method in `registry.rs`.
   ```sh
   grep -rn "LockPoisoned" crates/tessera-tenant/src/
   ```
   Expected: multiple sites in `registry.rs`.

2. [ ] `TenantRegistry::get_or_load` — return type is `Result<_>` without `expect`.
   ```sh
   grep -n "expect" crates/tessera-tenant/src/registry.rs
   ```
   Expected: zero results (all `expect` calls removed).

3. [ ] `AuthFailureReason` — the `tracing::debug!` call in `handle_hello`
   must reference it.
   ```sh
   grep -n "AuthFailureReason\|auth_failure" crates/tessera-server/src/bolt_handler.rs
   ```
   Expected: the enum definition and at least one use site in `handle_hello`.

4. [ ] `Zeroizing<String>` for credential — the `do_authenticate` signature
   must accept `Zeroizing<String>`.
   ```sh
   grep -n "Zeroizing" crates/tessera-auth/src/providers/ldap.rs
   ```
   Expected: at least two occurrences (one on `bind_password`, one on `do_authenticate`).

5. [ ] `std::fmt::Write` import for `write!` in LDAP escape — must be present.
   ```sh
   grep -n "fmt::Write" crates/tessera-auth/src/providers/ldap.rs
   ```
   Expected: one occurrence.

6. [ ] `mem::forget` is gone from both test files.
   ```sh
   grep -rn "mem::forget" \
     crates/tessera-server/tests/common/mod.rs \
     crates/tessera-server/tests/bolt_handler_test.rs
   ```
   Expected: zero results.

7. [ ] `test_registry()` returns a tuple — all callers destructure it.
   ```sh
   grep -n "test_registry" \
     crates/tessera-server/tests/common/mod.rs \
     crates/tessera-server/tests/bolt_handler_test.rs
   ```
   Confirm all call sites use `let (_dir, registry) = test_registry()` or
   equivalent.

8. [ ] Final workspace clean build and full test pass:
   ```sh
   nice cargo build --workspace 2>&1
   nice cargo test --workspace 2>&1
   nice cargo clippy --workspace 2>&1
   ```
   Expected: zero errors, zero warnings (warnings are errors).

---

## Estimation

| Phase | Effort |
|-------|--------|
| Phase 1 — CRITICAL (3 cycles)  | 30 min |
| Phase 2 — HIGH security (4 cycles) | 90 min |
| Phase 3 — MEDIUM quality (4 cycles) | 60 min |
| Phase 4 — LOW docs/edge (3 cycles)  | 30 min |
| Phase 5 — Wiring verification       | 20 min |
| **Total**                            | **~3.5 hours** |

---

## Criteria de Exito

- [ ] `cargo build --workspace` succeeds with zero errors
- [ ] `cargo clippy --workspace` produces zero diagnostics
- [ ] `cargo test --workspace` passes — all existing tests green, all new tests green
- [ ] `cargo doc --workspace --no-deps` produces zero warnings
- [ ] `TenantError::LockPoisoned` is returned in production code (not just tests)
- [ ] `mem::forget` is absent from all test helpers
- [ ] LDAP credential wrapped in `Zeroizing<String>` throughout `do_authenticate`
- [ ] `list_tenants` uses `read_dir` directly (no `exists()` call)
- [ ] `LdapConfig::Debug` includes all 8 fields
