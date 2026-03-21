# TDD Plan: External Authentication (LDAP + OIDC)

## Context

TesseraGraph Enterprise currently supports only local Argon2id authentication via
`UserStoreHandle::authenticate`. Enterprise deployments require integration with
corporate identity providers: Active Directory/OpenLDAP via LDAP bind, and modern
identity platforms (Keycloak, Okta, Azure AD/Entra ID) via OIDC JWT validation.

The work is purely additive. The existing local auth path, `UserStoreHandle`,
`SessionManager`, `RoleStore`, `AuthPolicy`, and `ConnectionHandler` remain
unchanged. External auth replaces only the credential-verification step inside
`handle_login`, selected at startup via `TESSERA_AUTH_MODE`.

**Stack detected**: Rust 2024, edition 2024, `rust-version = "1.85"`, tokio async
runtime, `clippy::all = deny`, `clippy::pedantic = warn`, `clippy::nursery = warn`,
`unsafe_code = forbid`

**Rust version note**: `rust-version = "1.85"` supports RPITIT (return-position
`impl Trait` in traits) for `async fn` in traits natively — no `async_trait` macro
needed. All traits in this plan use native `async fn in trait` syntax.

**Conventions observed**:
- Copyright header: `// Copyright 2026 BelowZero Security OU. All rights reserved.`
- `// OK: test` on `.expect()` in test code
- `// OK: reason` on `.unwrap_or_default()` in production where the default is safe
- `#[must_use]` on all pure-query methods
- Errors in `AuthError` enum via `thiserror`
- Sensitive fields implement `Zeroize` + `Drop { zeroize() }`
- Modules live under `crates/tessera-auth/src/`; tests under a parallel
  `crates/tessera-auth/tests/` tree (one file per module under test)
- `pub(crate)` for internal helpers; `pub` only for public API surface

**Affects hot path**: No. Authentication is a one-shot operation per connection
establishment, not per-query. No throughput benchmarks required.

## Decisions Resolved

1. **No `async_trait` macro** — Rust 1.85 supports `async fn in trait` natively via
   RPITIT. The `Send` bound is enforced by adding `+ Send` on the return type where
   required for tokio. Concrete pattern: `async fn authenticate(...) -> ...` in the
   trait body; the compiler infers the future is `Send` when all captured types are
   `Send`.

2. **LDAP abstraction** — `ldap3` operations are hidden behind a
   `LdapConnection` trait (`search`, `bind`). Tests inject a `MockLdapConnection`.
   No real LDAP server required in unit tests.

3. **OIDC JWT testing** — `jsonwebtoken` is deterministic given a key pair. Tests
   generate RSA-2048 key pairs at test time using `rsa` + `rand`, sign JWTs with
   known claims, and validate them. JWKS fetching is abstracted behind a
   `JwksFetcher` trait; tests inject `MockJwksFetcher`.

4. **Group mapping** — Pure synchronous function
   `map_groups(external_groups: &[String], mapping: &HashMap<String, String>) -> Vec<RoleId>`
   operating on the predefined role name strings (`"admin"`, `"readwrite"`,
   `"readonly"`, `"monitor"`). Fully testable with no I/O.

5. **`ExternalUserId`** — External providers cannot generate a `UserId` (u64
   sequence owned by `UserStore`). The `ConnectionHandler` will synthesize a
   transient `UserId` for the session using a stable hash of the username. This
   avoids modifying `UserStore` or `SessionManager`.

6. **File layout**:
   ```
   crates/tessera-auth/src/
     providers/
       mod.rs          — ExternalAuthProvider trait + ExternalUserInfo
       group_mapping.rs — map_groups pure function
       ldap.rs          — LdapAuthProvider + LdapConfig + LdapConnection trait
       oidc.rs          — OidcAuthProvider + OidcConfig + JwksFetcher trait
     external_config.rs — AuthMode enum + from-env config loader
   crates/tessera-auth/tests/
     providers_group_mapping.rs
     providers_ldap.rs
     providers_oidc.rs
     external_config.rs
   crates/tessera-server/src/
     auth_dispatch.rs   — authenticate_external() helper called by ConnectionHandler
   ```

## Decisions Remaining Before Execution

None. All architectural decisions are resolved above.

---

## Plan de Ejecución

---

### Phase 1: Core Trait + ExternalUserInfo + Group Mapping (pure logic, no I/O)

**Goal**: Define the extensibility contract and the pure group-mapping function.
All tests in this phase are synchronous and have zero dependencies on I/O.

---

#### 1.1 RED — Test: `ExternalUserInfo` field access
- File: `crates/tessera-auth/tests/providers_group_mapping.rs` (create)
- Action: Create
- Test:
  ```rust
  // Copyright 2026 BelowZero Security OU. All rights reserved.
  #[test]
  fn external_user_info_fields_roundtrip() {
      use tessera_auth::providers::ExternalUserInfo;
      let info = ExternalUserInfo {
          username: "alice".to_owned(),
          groups: vec!["admin".to_owned()],
          email: Some("alice@example.com".to_owned()),
          display_name: Some("Alice".to_owned()),
      };
      assert_eq!(info.username, "alice");
      assert_eq!(info.groups, ["admin"]);
      assert_eq!(info.email.as_deref(), Some("alice@example.com"));
  }
  ```
- Output: Compilation failure — `tessera_auth::providers` does not exist.

---

#### 1.2 GREEN — Create `providers/mod.rs` with `ExternalAuthProvider` trait + `ExternalUserInfo`
- File: `crates/tessera-auth/src/providers/mod.rs` (create)
- Action: Create
- Minimal code:
  ```rust
  // Copyright 2026 BelowZero Security OU. All rights reserved.
  pub mod group_mapping;
  pub mod ldap;
  pub mod oidc;

  use crate::error::{AuthError, Result};

  /// Information about a successfully authenticated external user.
  #[derive(Debug, Clone)]
  pub struct ExternalUserInfo {
      pub username: String,
      pub groups: Vec<String>,
      pub email: Option<String>,
      pub display_name: Option<String>,
  }

  /// Abstraction over external identity providers (LDAP, OIDC).
  ///
  /// Implementors must be `Send + Sync` to be stored in `Arc<dyn ExternalAuthProvider>`.
  pub trait ExternalAuthProvider: Send + Sync {
      /// Authenticate a user and return their identity info on success.
      ///
      /// The `credential` field carries a password for LDAP and a JWT for OIDC.
      ///
      /// # Errors
      ///
      /// Returns `AuthError::InvalidCredentials` on any authentication failure.
      /// Never returns provider-specific details to prevent information leakage.
      async fn authenticate(
          &self,
          username: &str,
          credential: &str,
      ) -> Result<ExternalUserInfo>;

      /// Human-readable provider name for logging and configuration display.
      fn provider_name(&self) -> &str;
  }
  ```
- Also add `pub mod providers;` to `crates/tessera-auth/src/lib.rs`.
- Output: Test 1.1 compiles and passes.

---

#### 1.3 RED — Tests: `map_groups` pure function
- File: `crates/tessera-auth/tests/providers_group_mapping.rs` (extend)
- Tests to add:
  ```rust
  use std::collections::HashMap;
  use tessera_auth::providers::group_mapping::map_groups;
  use tessera_auth::rbac::{RoleId, RoleStore};

  fn default_mapping() -> HashMap<String, String> {
      [
          ("admin".to_owned(), "admin".to_owned()),
          ("developers".to_owned(), "readwrite".to_owned()),
          ("viewers".to_owned(), "readonly".to_owned()),
      ]
      .into_iter()
      .collect()
  }

  #[test]
  fn map_groups_returns_admin_role() {
      let mapping = default_mapping();
      let roles = map_groups(&["admin".to_owned()], &mapping);
      assert_eq!(roles, vec![RoleStore::ADMIN_ROLE_ID]);
  }

  #[test]
  fn map_groups_returns_multiple_roles() {
      let mapping = default_mapping();
      let mut roles = map_groups(
          &["admin".to_owned(), "viewers".to_owned()],
          &mapping,
      );
      roles.sort_by_key(RoleId::raw);
      assert_eq!(roles, vec![RoleStore::ADMIN_ROLE_ID, RoleStore::READONLY_ROLE_ID]);
  }

  #[test]
  fn map_groups_unknown_group_is_ignored() {
      let mapping = default_mapping();
      let roles = map_groups(&["unknown_group".to_owned()], &mapping);
      assert!(roles.is_empty());
  }

  #[test]
  fn map_groups_empty_input_returns_empty() {
      let mapping = default_mapping();
      let roles = map_groups(&[], &mapping);
      assert!(roles.is_empty());
  }

  #[test]
  fn map_groups_deduplicates_roles() {
      // Two LDAP groups both mapping to admin — result must contain admin once.
      let mapping: HashMap<String, String> = [
          ("cn=admins".to_owned(), "admin".to_owned()),
          ("cn=superusers".to_owned(), "admin".to_owned()),
      ]
      .into_iter()
      .collect();
      let roles = map_groups(
          &["cn=admins".to_owned(), "cn=superusers".to_owned()],
          &mapping,
      );
      assert_eq!(roles.len(), 1);
      assert_eq!(roles[0], RoleStore::ADMIN_ROLE_ID);
  }
  ```
- Output: Compilation failure — `group_mapping::map_groups` does not exist.

---

#### 1.4 GREEN — Implement `group_mapping.rs`
- File: `crates/tessera-auth/src/providers/group_mapping.rs` (create)
- Action: Create
- Minimal implementation:
  ```rust
  // Copyright 2026 BelowZero Security OU. All rights reserved.
  use std::collections::{HashMap, HashSet};
  use crate::rbac::{RoleId, RoleStore};

  /// Map external group names to internal `RoleId`s using a configurable table.
  ///
  /// Groups that have no entry in `mapping` are silently ignored.
  /// Duplicate role assignments (two groups mapping to the same role) are deduplicated.
  ///
  /// The role name strings must match the predefined role names in `RoleStore`:
  /// `"admin"`, `"readwrite"`, `"readonly"`, `"monitor"`.
  #[must_use]
  pub fn map_groups(
      external_groups: &[String],
      mapping: &HashMap<String, String>,
  ) -> Vec<RoleId> {
      let mut seen: HashSet<RoleId> = HashSet::new();
      let mut roles = Vec::new();

      for group in external_groups {
          let Some(role_name) = mapping.get(group) else {
              continue;
          };
          let Some(role_id) = role_name_to_id(role_name) else {
              continue;
          };
          if seen.insert(role_id) {
              roles.push(role_id);
          }
      }

      roles
  }

  fn role_name_to_id(name: &str) -> Option<RoleId> {
      match name {
          "admin"     => Some(RoleStore::ADMIN_ROLE_ID),
          "readwrite" => Some(RoleStore::READWRITE_ROLE_ID),
          "readonly"  => Some(RoleStore::READONLY_ROLE_ID),
          "monitor"   => Some(RoleStore::MONITOR_ROLE_ID),
          _           => None,
      }
  }
  ```
- Output: All five group-mapping tests pass.

---

#### 1.5 RED — Test: `parse_group_mapping` from environment string
- File: `crates/tessera-auth/tests/providers_group_mapping.rs` (extend)
- Test:
  ```rust
  use tessera_auth::providers::group_mapping::parse_group_mapping;

  #[test]
  fn parse_group_mapping_valid_string() {
      let raw = "admin=admin,developers=readwrite,viewers=readonly";
      let map = parse_group_mapping(raw);
      assert_eq!(map.get("admin").map(String::as_str), Some("admin"));
      assert_eq!(map.get("developers").map(String::as_str), Some("readwrite"));
      assert_eq!(map.get("viewers").map(String::as_str), Some("readonly"));
  }

  #[test]
  fn parse_group_mapping_ignores_malformed_pairs() {
      // Pairs without '=' are silently dropped.
      let raw = "admin=admin,badentry,developers=readwrite";
      let map = parse_group_mapping(raw);
      assert_eq!(map.len(), 2);
      assert!(map.contains_key("admin"));
      assert!(map.contains_key("developers"));
  }

  #[test]
  fn parse_group_mapping_empty_string_returns_empty() {
      let map = parse_group_mapping("");
      assert!(map.is_empty());
  }
  ```
- Output: Compilation failure — `parse_group_mapping` does not exist.

---

#### 1.6 GREEN — Add `parse_group_mapping` to `group_mapping.rs`
- File: `crates/tessera-auth/src/providers/group_mapping.rs` (extend)
- Add:
  ```rust
  /// Parse a comma-separated `"ldap_group=role_name"` string into a `HashMap`.
  ///
  /// Malformed pairs (no `=`) are silently ignored.
  #[must_use]
  pub fn parse_group_mapping(raw: &str) -> HashMap<String, String> {
      if raw.is_empty() {
          return HashMap::new();
      }
      raw.split(',')
          .filter_map(|pair| {
              let (k, v) = pair.split_once('=')?;
              Some((k.trim().to_owned(), v.trim().to_owned()))
          })
          .collect()
  }
  ```
- Output: All group-mapping tests pass.

---

### Phase 2: LDAP Provider

**Goal**: Implement `LdapAuthProvider` with a mockable `LdapConnection` trait.
All tests use a `MockLdapConnection` — no real LDAP server required.

---

#### 2.1 RED — Test: LDAP config parses from env vars
- File: `crates/tessera-auth/tests/providers_ldap.rs` (create)
- Test:
  ```rust
  // Copyright 2026 BelowZero Security OU. All rights reserved.
  use tessera_auth::providers::ldap::LdapConfig;

  #[test]
  fn ldap_config_roundtrip() {
      let cfg = LdapConfig {
          ldap_url: "ldap://localhost:389".to_owned(),
          bind_dn: "cn=svc,dc=example,dc=com".to_owned(),
          bind_password: "secret".to_owned(),
          base_dn: "ou=users,dc=example,dc=com".to_owned(),
          user_filter_template: "(uid={username})".to_owned(),
          group_attribute: "memberOf".to_owned(),
          use_tls: false,
          group_mapping: std::collections::HashMap::new(),
      };
      assert_eq!(cfg.ldap_url, "ldap://localhost:389");
      assert!(!cfg.use_tls);
  }
  ```
- Output: Compilation failure — `tessera_auth::providers::ldap` does not exist.

---

#### 2.2 GREEN — Skeleton `ldap.rs` with `LdapConfig`
- File: `crates/tessera-auth/src/providers/ldap.rs` (create)
- Dependencies to add in `crates/tessera-auth/Cargo.toml`:
  ```toml
  ldap3 = { version = "0.11", default-features = false, features = ["tokio"] }
  tokio = { version = "1", features = ["rt"] }
  ```
- Create `LdapConfig` struct (all public fields), `LdapAuthProvider` struct
  holding `LdapConfig`. Do NOT implement `ExternalAuthProvider` yet.
- Output: Test 2.1 passes.

---

#### 2.3 RED — Test: `LdapConnection` trait + `MockLdapConnection`
- File: `crates/tessera-auth/tests/providers_ldap.rs` (extend)
- Add test scaffolding:
  ```rust
  use tessera_auth::providers::ldap::{LdapAuthProvider, LdapConfig, LdapSearchEntry};
  use tessera_auth::error::AuthError;
  use std::collections::HashMap;

  struct MockLdapConnection {
      // None = simulate server unreachable
      search_result: Option<Vec<LdapSearchEntry>>,
      // true = service bind succeeds; false = fails
      service_bind_ok: bool,
      // true = user re-bind succeeds; false = wrong password
      user_bind_ok: bool,
  }

  // impl LdapConnection for MockLdapConnection — will be implemented after GREEN 2.4
  ```
  This test file is a scaffold. It will not compile until the trait exists.
- Output: Compilation failure — `LdapSearchEntry` and `LdapConnection` do not exist.

---

#### 2.4 GREEN — Define `LdapConnection` trait + `LdapSearchEntry`
- File: `crates/tessera-auth/src/providers/ldap.rs` (extend)
- Add to the module:
  ```rust
  /// A single entry returned by an LDAP search.
  #[derive(Debug, Clone)]
  pub struct LdapSearchEntry {
      /// The full distinguished name of the entry.
      pub dn: String,
      /// Attribute values keyed by attribute name.
      pub attrs: HashMap<String, Vec<String>>,
  }

  /// Abstraction over an LDAP connection, enabling mock injection in tests.
  pub trait LdapConnection: Send + Sync {
      /// Bind as the service account. Returns `Ok(())` on success.
      async fn service_bind(&mut self, bind_dn: &str, bind_password: &str) -> crate::error::Result<()>;

      /// Search for entries matching `filter` under `base_dn`.
      /// Returns at most `size_limit` entries.
      async fn search(
          &mut self,
          base_dn: &str,
          filter: &str,
          attrs: &[&str],
      ) -> crate::error::Result<Vec<LdapSearchEntry>>;

      /// Re-bind as the user to verify their password.
      async fn user_bind(&mut self, user_dn: &str, password: &str) -> crate::error::Result<()>;
  }
  ```
- Output: Scaffold in test file now compiles (trait exists, MockLdapConnection can
  be declared even without `impl` body yet).

---

#### 2.5 RED — Test: successful LDAP authentication
- File: `crates/tessera-auth/tests/providers_ldap.rs` (extend)
- Implement `MockLdapConnection` and test:
  ```rust
  // impl LdapConnection for MockLdapConnection { ... }
  // (service_bind: Ok if service_bind_ok; search: returns search_result; user_bind: Ok if user_bind_ok)

  #[tokio::test]
  async fn ldap_authenticate_success() {
      let mock = MockLdapConnection {
          service_bind_ok: true,
          search_result: Some(vec![LdapSearchEntry {
              dn: "uid=alice,ou=users,dc=example,dc=com".to_owned(),
              attrs: {
                  let mut m = HashMap::new();
                  m.insert("memberOf".to_owned(), vec!["cn=developers,dc=example,dc=com".to_owned()]);
                  m.insert("mail".to_owned(), vec!["alice@example.com".to_owned()]);
                  m.insert("cn".to_owned(), vec!["Alice Liddell".to_owned()]);
                  m
              },
          }]),
          user_bind_ok: true,
      };
      let mapping: HashMap<String, String> =
          [("cn=developers,dc=example,dc=com".to_owned(), "readwrite".to_owned())]
              .into_iter()
              .collect();
      let cfg = ldap_config_with_mapping(mapping);
      let provider = LdapAuthProvider::with_connection(cfg, mock);

      let info = provider.authenticate("alice", "correct-password").await
          .expect("OK: test"); // OK: test
      assert_eq!(info.username, "alice");
      assert_eq!(info.email.as_deref(), Some("alice@example.com"));
      assert_eq!(info.display_name.as_deref(), Some("Alice Liddell"));
      assert!(info.groups.contains(&"cn=developers,dc=example,dc=com".to_owned()));
  }
  ```
- Output: Compilation failure — `LdapAuthProvider::with_connection` does not exist.

---

#### 2.6 GREEN — Implement `LdapAuthProvider` with injectable connection
- File: `crates/tessera-auth/src/providers/ldap.rs` (extend)
- Key design point: `LdapAuthProvider<C: LdapConnection>` is generic over the
  connection type. The production constructor calls real `ldap3`; tests inject mock.
  The `ExternalAuthProvider` impl uses a `Mutex<C>` to drive the connection.
- Flow inside `authenticate`:
  1. Format the user filter by replacing `{username}` with the escaped username.
  2. `service_bind(bind_dn, bind_password)` — on error, log "LDAP service bind
     failed" (NO password in log) → return `AuthError::InvalidCredentials`.
  3. `search(base_dn, filter, &[group_attr, "mail", "cn"])` — on empty result →
     return `AuthError::InvalidCredentials` (no user enumeration).
  4. `user_bind(entry.dn, credential)` — on error → return
     `AuthError::InvalidCredentials`.
  5. Extract groups from `entry.attrs[group_attr]`, email from `mail`, display name
     from `cn`. Build and return `ExternalUserInfo`.
- Security invariants encoded in implementation:
  - `bind_password` is never formatted into log strings.
  - `credential` (user password) is never formatted into log strings.
  - All error paths return `AuthError::InvalidCredentials` regardless of root cause.
- Output: Test 2.5 passes.

---

#### 2.7 RED — Tests: LDAP failure cases
- File: `crates/tessera-auth/tests/providers_ldap.rs` (extend)
- Tests:
  ```rust
  #[tokio::test]
  async fn ldap_service_bind_failure_returns_invalid_credentials() {
      let mock = MockLdapConnection { service_bind_ok: false, search_result: None, user_bind_ok: false };
      let provider = LdapAuthProvider::with_connection(ldap_config_with_mapping(HashMap::new()), mock);
      let err = provider.authenticate("alice", "pw").await.unwrap_err();
      assert!(matches!(err, AuthError::InvalidCredentials));
  }

  #[tokio::test]
  async fn ldap_user_not_found_returns_invalid_credentials() {
      let mock = MockLdapConnection { service_bind_ok: true, search_result: Some(vec![]), user_bind_ok: false };
      let provider = LdapAuthProvider::with_connection(ldap_config_with_mapping(HashMap::new()), mock);
      let err = provider.authenticate("nonexistent", "pw").await.unwrap_err();
      assert!(matches!(err, AuthError::InvalidCredentials));
  }

  #[tokio::test]
  async fn ldap_wrong_password_returns_invalid_credentials() {
      let entry = LdapSearchEntry { dn: "uid=alice,ou=users,dc=example,dc=com".to_owned(), attrs: HashMap::new() };
      let mock = MockLdapConnection { service_bind_ok: true, search_result: Some(vec![entry]), user_bind_ok: false };
      let provider = LdapAuthProvider::with_connection(ldap_config_with_mapping(HashMap::new()), mock);
      let err = provider.authenticate("alice", "wrong-pw").await.unwrap_err();
      assert!(matches!(err, AuthError::InvalidCredentials));
  }

  #[test]
  fn ldap_provider_name() {
      let provider = LdapAuthProvider::with_connection(ldap_config_with_mapping(HashMap::new()), MockLdapConnection::ok());
      assert_eq!(provider.provider_name(), "ldap");
  }
  ```
- Output: Tests compile (types exist) but fail if any error path incorrectly
  returns a different variant.

---

#### 2.8 GREEN — Fix all LDAP error paths to return `InvalidCredentials`
- File: `crates/tessera-auth/src/providers/ldap.rs` (verify/fix)
- Confirm each failure branch in `authenticate` returns `Err(AuthError::InvalidCredentials)`.
- No new types needed — only logic verification.
- Output: All LDAP tests pass.

---

#### 2.9 RED — Test: LDAP username filter escaping
- File: `crates/tessera-auth/tests/providers_ldap.rs` (extend)
- Rationale: LDAP injection via username (e.g., `*)(uid=*)(\0`) must be escaped.
- Test:
  ```rust
  use tessera_auth::providers::ldap::escape_ldap_filter_value;

  #[test]
  fn ldap_escape_prevents_injection() {
      // RFC 4515 special characters must be escaped.
      assert_eq!(escape_ldap_filter_value("alice*(test)"), r"alice\2a\28test\29");
      assert_eq!(escape_ldap_filter_value("normal"), "normal");
      assert_eq!(escape_ldap_filter_value(r"back\slash"), r"back\5cslash");
      assert_eq!(escape_ldap_filter_value("\0null"), r"\00null");
  }
  ```
- Output: Compilation failure — `escape_ldap_filter_value` is not public yet.

---

#### 2.10 GREEN — Implement and expose `escape_ldap_filter_value`
- File: `crates/tessera-auth/src/providers/ldap.rs` (extend)
- Implement per RFC 4515: escape `\`, `*`, `(`, `)`, `\0` as `\5c`, `\2a`, `\28`,
  `\29`, `\00` respectively. All other bytes pass through.
- Make `pub` so integration tests can verify it.
- Ensure `authenticate` calls this function on the username before substituting
  into the filter template.
- Output: Escape tests pass; existing LDAP tests continue to pass.

---

### Phase 3: OIDC Provider

**Goal**: Implement `OidcAuthProvider` with JWT validation. JWKS fetching is
abstracted so unit tests do not make HTTP calls.

---

#### 3.1 RED — Test: OIDC config struct
- File: `crates/tessera-auth/tests/providers_oidc.rs` (create)
- Test:
  ```rust
  // Copyright 2026 BelowZero Security OU. All rights reserved.
  use tessera_auth::providers::oidc::OidcConfig;
  use std::collections::HashMap;

  #[test]
  fn oidc_config_roundtrip() {
      let cfg = OidcConfig {
          issuer: "https://keycloak.example.com/realms/tessera".to_owned(),
          audience: "tessera-graph".to_owned(),
          jwks_url: "https://keycloak.example.com/realms/tessera/protocol/openid-connect/certs".to_owned(),
          group_claim: "groups".to_owned(),
          group_mapping: HashMap::new(),
      };
      assert_eq!(cfg.issuer, "https://keycloak.example.com/realms/tessera");
      assert_eq!(cfg.group_claim, "groups");
  }
  ```
- Output: Compilation failure — `tessera_auth::providers::oidc` does not exist.

---

#### 3.2 GREEN — Skeleton `oidc.rs` with `OidcConfig`
- File: `crates/tessera-auth/src/providers/oidc.rs` (create)
- Dependencies to add in `crates/tessera-auth/Cargo.toml`:
  ```toml
  jsonwebtoken = "9"
  ```
  Note: `jsonwebtoken` 9 brings in `serde` (already in workspace) and handles
  RS256, ES256. No additional HTTP client is needed here — JWKS fetching is
  abstracted.
- Create `OidcConfig` struct. Do not implement `ExternalAuthProvider` yet.
- Output: Test 3.1 passes.

---

#### 3.3 RED — Test: `JwksFetcher` trait + `MockJwksFetcher`
- File: `crates/tessera-auth/tests/providers_oidc.rs` (extend)
- Test scaffolding:
  ```rust
  use tessera_auth::providers::oidc::{JwksFetcher, JwksDocument};

  struct MockJwksFetcher {
      document: JwksDocument,
  }

  // impl JwksFetcher for MockJwksFetcher
  // async fn fetch(&self) -> Result<JwksDocument> { Ok(self.document.clone()) }
  ```
- Output: Compilation failure — `JwksFetcher` and `JwksDocument` do not exist.

---

#### 3.4 GREEN — Define `JwksFetcher` trait + `JwksDocument`
- File: `crates/tessera-auth/src/providers/oidc.rs` (extend)
- `JwksDocument` wraps `jsonwebtoken::jwk::JwkSet` (re-exported or newtype).
- `JwksFetcher` trait:
  ```rust
  pub trait JwksFetcher: Send + Sync {
      async fn fetch(&self) -> crate::error::Result<JwksDocument>;
  }
  ```
- Output: Mock in test file compiles.

---

#### 3.5 RED — Test: successful OIDC authentication with RS256 JWT
- File: `crates/tessera-auth/tests/providers_oidc.rs` (extend)
- Key: Generate an RSA-2048 key pair in the test, sign a JWT with it, construct
  the matching `JwksDocument`, and inject via `MockJwksFetcher`.
- Dependencies for test only (add to `[dev-dependencies]` in `tessera-auth/Cargo.toml`):
  ```toml
  rsa = { version = "0.9", features = ["sha2"] }
  # rand already present
  ```
- Test outline:
  ```rust
  #[tokio::test]
  async fn oidc_authenticate_valid_rs256_jwt() {
      // 1. Generate RSA-2048 key pair
      // 2. Build JwksDocument from the public key
      // 3. Sign a JWT with: iss, aud, sub, exp (now+300), groups claim
      // 4. Construct OidcAuthProvider with MockJwksFetcher
      // 5. Call authenticate("alice", &jwt_string)
      // 6. Assert: username == "alice", groups contains expected group
  }
  ```
- Output: Compilation failure — `OidcAuthProvider::with_fetcher` does not exist.

---

#### 3.6 GREEN — Implement `OidcAuthProvider` core JWT validation
- File: `crates/tessera-auth/src/providers/oidc.rs` (extend)
- `OidcAuthProvider<F: JwksFetcher>` holds config + fetcher + JWKS cache
  (`tokio::sync::RwLock<Option<JwksDocument>>`).
- Validation sequence in `authenticate` (the `credential` field IS the raw JWT):
  1. Decode JWT header to extract `kid` (no validation yet).
  2. Acquire JWKS from cache or fetch via `fetcher.fetch()`.
  3. Locate matching key by `kid` in the JWKS. If not found, force-refresh and
     retry once.
  4. Validate with `jsonwebtoken::decode`:
     - Algorithm: from JWK (RS256 / ES256 — reject HS256 and `none`).
     - Validate `exp` (expired → `InvalidCredentials`).
     - Validate `iss` (must equal `config.issuer`).
     - Validate `aud` (must contain `config.audience`).
  5. Extract `sub` as username.
  6. Extract groups from `config.group_claim` in extra claims.
  7. Build and return `ExternalUserInfo`.
- Security: JWT string is NEVER logged. Validation failures all return
  `AuthError::InvalidCredentials` (no information about why the token failed).
- Output: Test 3.5 passes.

---

#### 3.7 RED — Tests: OIDC validation failure cases
- File: `crates/tessera-auth/tests/providers_oidc.rs` (extend)
- Tests (one per failure mode):
  ```rust
  #[tokio::test]
  async fn oidc_expired_token_returns_invalid_credentials() { ... }

  #[tokio::test]
  async fn oidc_wrong_issuer_returns_invalid_credentials() { ... }

  #[tokio::test]
  async fn oidc_wrong_audience_returns_invalid_credentials() { ... }

  #[tokio::test]
  async fn oidc_invalid_signature_returns_invalid_credentials() { ... }

  #[tokio::test]
  async fn oidc_none_algorithm_rejected() {
      // Construct a JWT with alg=none — must be rejected even with valid claims.
  }
  ```
- Each test generates a JWT that violates exactly one constraint. All must return
  `Err(AuthError::InvalidCredentials)`.
- Output: Tests compile (types exist); some may fail if not all paths are handled.

---

#### 3.8 GREEN — Harden all OIDC validation error paths
- File: `crates/tessera-auth/src/providers/oidc.rs` (verify/fix)
- Ensure:
  - `alg=none` is rejected at the key-lookup stage (JWK must specify an asymmetric
    algorithm; if absent or `none`, return `InvalidCredentials`).
  - Expired token: `jsonwebtoken` returns `ErrorKind::ExpiredSignature` → map to
    `InvalidCredentials`.
  - Wrong issuer: map to `InvalidCredentials`.
  - Wrong audience: map to `InvalidCredentials`.
  - Invalid signature: map to `InvalidCredentials`.
  - No JWT logged in any error branch.
- Output: All OIDC tests pass.

---

#### 3.9 RED — Test: JWKS cache refresh on unknown `kid`
- File: `crates/tessera-auth/tests/providers_oidc.rs` (extend)
- Design: `MockJwksFetcher` with a call counter. First call returns empty JWKS.
  After provider fails to find `kid`, it calls fetch again. Second call returns
  the real JWKS.
- Test:
  ```rust
  #[tokio::test]
  async fn oidc_refreshes_jwks_on_unknown_kid() {
      // MockJwksFetcher returns empty JWKS on first call, real JWKS on second.
      // authenticate() should succeed after the refresh.
      // Assert fetcher.call_count() == 2.
  }
  ```
- Output: Test fails — refresh logic not yet implemented.

---

#### 3.10 GREEN — Implement JWKS cache invalidation and refresh
- File: `crates/tessera-auth/src/providers/oidc.rs` (extend)
- Pattern: on `kid` not found in cached JWKS, clear cache and call `fetcher.fetch()`
  once more. If still not found after second fetch, return `InvalidCredentials`.
- Output: Test 3.9 passes.

---

### Phase 4: Auth Mode Config + `AuthError` Extensions

**Goal**: Define the `AuthMode` enum, the env-var config loader, and add missing
`AuthError` variants needed for provider-level errors that are distinct from
`InvalidCredentials` (e.g., `ProviderUnavailable` for surfacing in audit logs
without leaking to clients).

---

#### 4.1 RED — Test: `AuthMode` parses from env string
- File: `crates/tessera-auth/tests/external_config.rs` (create)
- Tests:
  ```rust
  // Copyright 2026 BelowZero Security OU. All rights reserved.
  use tessera_auth::external_config::AuthMode;

  #[test]
  fn auth_mode_from_str_local() {
      let mode: AuthMode = "local".parse().expect("OK: test");
      assert!(matches!(mode, AuthMode::Local));
  }

  #[test]
  fn auth_mode_from_str_ldap() {
      let mode: AuthMode = "ldap".parse().expect("OK: test");
      assert!(matches!(mode, AuthMode::Ldap));
  }

  #[test]
  fn auth_mode_from_str_oidc() {
      let mode: AuthMode = "oidc".parse().expect("OK: test");
      assert!(matches!(mode, AuthMode::Oidc));
  }

  #[test]
  fn auth_mode_from_str_invalid() {
      let result: Result<AuthMode, _> = "saml".parse();
      assert!(result.is_err());
  }
  ```
- Output: Compilation failure — `tessera_auth::external_config` does not exist.

---

#### 4.2 GREEN — Create `external_config.rs` with `AuthMode`
- File: `crates/tessera-auth/src/external_config.rs` (create)
- Content:
  ```rust
  // Copyright 2026 BelowZero Security OU. All rights reserved.
  use std::str::FromStr;
  use crate::error::AuthError;

  /// Authentication mode selected at server startup.
  #[derive(Debug, Clone, PartialEq, Eq)]
  pub enum AuthMode {
      Local,
      Ldap,
      Oidc,
  }

  impl FromStr for AuthMode {
      type Err = AuthError;
      fn from_str(s: &str) -> crate::error::Result<Self> {
          match s {
              "local" => Ok(Self::Local),
              "ldap"  => Ok(Self::Ldap),
              "oidc"  => Ok(Self::Oidc),
              other   => Err(AuthError::ConfigError(format!("unknown auth mode: {other}"))),
          }
      }
  }
  ```
- Also add `AuthError::ConfigError(String)` to `error.rs`:
  ```rust
  #[error("configuration error: {0}")]
  ConfigError(String),
  ```
- Add `pub mod external_config;` to `lib.rs`.
- Output: All `AuthMode` parse tests pass.

---

#### 4.3 RED — Test: `LdapConfig::from_env` and `OidcConfig::from_env`
- File: `crates/tessera-auth/tests/external_config.rs` (extend)
- Tests use `temp_env` or direct `std::env::set_var` in a serial test (use
  `#[serial_test::serial]` if added, otherwise document the test as environment-
  dependent and run with `-- --test-threads=1`):
  ```rust
  #[test]
  fn ldap_config_from_env_all_vars_set() {
      std::env::set_var("TESSERA_LDAP_URL", "ldap://localhost:389");
      std::env::set_var("TESSERA_LDAP_BIND_DN", "cn=svc,dc=example,dc=com");
      std::env::set_var("TESSERA_LDAP_BIND_PASSWORD", "secret");
      std::env::set_var("TESSERA_LDAP_BASE_DN", "ou=users,dc=example,dc=com");
      std::env::set_var("TESSERA_LDAP_USER_FILTER", "(uid={username})");
      std::env::set_var("TESSERA_LDAP_GROUP_ATTR", "memberOf");
      std::env::set_var("TESSERA_LDAP_USE_TLS", "false");
      std::env::set_var("TESSERA_LDAP_GROUP_MAPPING", "admin=admin");

      let cfg = tessera_auth::providers::ldap::LdapConfig::from_env()
          .expect("OK: test");
      assert_eq!(cfg.ldap_url, "ldap://localhost:389");
      assert!(!cfg.use_tls);
  }

  #[test]
  fn ldap_config_from_env_missing_required_var() {
      std::env::remove_var("TESSERA_LDAP_URL");
      let err = tessera_auth::providers::ldap::LdapConfig::from_env().unwrap_err();
      assert!(matches!(err, tessera_auth::AuthError::ConfigError(_)));
  }
  ```
- Output: Compilation failure — `LdapConfig::from_env` does not exist.

---

#### 4.4 GREEN — Implement `from_env` for `LdapConfig` and `OidcConfig`
- File: `crates/tessera-auth/src/providers/ldap.rs` (extend)
- File: `crates/tessera-auth/src/providers/oidc.rs` (extend)
- Both methods read the environment variables listed in the spec. Missing required
  vars return `AuthError::ConfigError(format!("missing env var: {name}"))`.
- `TESSERA_LDAP_USE_TLS` defaults to `true` if absent (secure by default).
- `TESSERA_OIDC_GROUP_CLAIM` defaults to `"groups"` if absent.
- Group mapping is parsed via `parse_group_mapping`; empty string or absent var
  results in an empty `HashMap`.
- Output: `from_env` tests pass.

---

#### 4.5 RED — Test: `AuthError::ProviderUnavailable` is distinct from `InvalidCredentials`
- File: `crates/tessera-auth/tests/external_config.rs` (extend)
- Test:
  ```rust
  use tessera_auth::AuthError;

  #[test]
  fn provider_unavailable_is_not_invalid_credentials() {
      let e = AuthError::ProviderUnavailable("ldap server timeout".to_owned());
      assert!(!matches!(e, AuthError::InvalidCredentials));
      // The display message must NOT leak the internal detail to a client
      // (the server logs it, but sends AUTH_FAILURE_MSG to the wire).
      assert!(e.to_string().contains("provider unavailable"));
  }
  ```
- Output: Compilation failure — `ProviderUnavailable` variant does not exist.

---

#### 4.6 GREEN — Add `AuthError::ProviderUnavailable`
- File: `crates/tessera-auth/src/error.rs` (extend)
- Add:
  ```rust
  #[error("provider unavailable: {0}")]
  ProviderUnavailable(String),
  ```
- Update LDAP `authenticate`: if `ldap3` returns a connection/TLS error (not an
  auth failure), return `ProviderUnavailable` internally. The server layer maps
  this to `InvalidCredentials` before sending to the wire (see Phase 5).
- Output: Test 4.5 passes.

---

### Phase 5: Server Integration — `auth_dispatch.rs` + `ConnectionHandler` wiring

**Goal**: Add `auth_dispatch.rs` to `tessera-server` as a thin adapter layer.
`ConnectionHandler::handle_login` gains an `ExternalAuthProvider` dispatch path.
`ServerContext` gains an optional `Arc<dyn ExternalAuthProvider>`.

---

#### 5.1 RED — Test: `authenticate_external` returns `UserId` for valid external auth
- File: `crates/tessera-server/tests/auth_dispatch.rs` (create)
- Introduce a `MockExternalAuthProvider` that always succeeds.
- Test:
  ```rust
  // Copyright 2026 BelowZero Security OU. All rights reserved.
  use tessera_auth::providers::{ExternalAuthProvider, ExternalUserInfo};
  use tessera_auth::error::{AuthError, Result};
  use tessera_server::auth_dispatch::authenticate_external;

  struct AlwaysOkProvider;
  impl ExternalAuthProvider for AlwaysOkProvider {
      async fn authenticate(&self, username: &str, _cred: &str) -> Result<ExternalUserInfo> {
          Ok(ExternalUserInfo {
              username: username.to_owned(),
              groups: vec!["admin".to_owned()],
              email: None,
              display_name: None,
          })
      }
      fn provider_name(&self) -> &str { "mock" }
  }

  #[tokio::test]
  async fn authenticate_external_returns_user_id_on_success() {
      let provider: Arc<dyn ExternalAuthProvider> = Arc::new(AlwaysOkProvider);
      let mapping = /* admin → ADMIN_ROLE_ID */;
      let sessions = Arc::new(SessionManager::new(3600));

      let (user_id, token) = authenticate_external("alice", "any-cred", &provider, &mapping, &sessions)
          .await
          .expect("OK: test");
      // user_id is a deterministic hash of "alice"
      let validated = sessions.validate(&token).expect("OK: test");
      assert_eq!(validated, user_id);
  }
  ```
- Output: Compilation failure — `tessera_server::auth_dispatch` does not exist.

---

#### 5.2 GREEN — Create `auth_dispatch.rs`
- File: `crates/tessera-server/src/auth_dispatch.rs` (create)
- Function signature:
  ```rust
  pub async fn authenticate_external(
      username: &str,
      credential: &str,
      provider: &Arc<dyn ExternalAuthProvider>,
      group_mapping: &HashMap<String, String>,
      sessions: &Arc<SessionManager>,
  ) -> crate::error::Result<(UserId, SessionToken)>
  ```
- `UserId` synthesis: `UserId::new(hash_username(username))` where `hash_username`
  computes a stable 64-bit FNV-1a hash. The session is scoped to the connection;
  no entry is written to `UserStore`.
- On `ProviderUnavailable` from the provider: log the detail, return
  `ServerError::Auth(AuthError::InvalidCredentials)` — never leak to wire.
- On `InvalidCredentials`: propagate as-is.
- Output: Test 5.1 passes.

---

#### 5.3 RED — Test: `auth_dispatch` with `ProviderUnavailable` maps to `InvalidCredentials`
- File: `crates/tessera-server/tests/auth_dispatch.rs` (extend)
- Mock provider returns `Err(AuthError::ProviderUnavailable("timeout".to_owned()))`.
- Test asserts the returned error is `InvalidCredentials` (not `ProviderUnavailable`).
- Output: Test compiles but may fail if mapping is not implemented.

---

#### 5.4 GREEN — Add `ProviderUnavailable` → `InvalidCredentials` mapping in `auth_dispatch.rs`
- File: `crates/tessera-server/src/auth_dispatch.rs` (extend)
- Pattern:
  ```rust
  Err(AuthError::ProviderUnavailable(detail)) => {
      tracing::warn!("external auth provider unavailable: {detail}");
      Err(ServerError::Auth(AuthError::InvalidCredentials))
  }
  ```
- Output: Test 5.3 passes.

---

#### 5.5 RED — Test: `ServerContext` carries optional external provider
- File: `crates/tessera-server/tests/auth_dispatch.rs` (extend)
- Test: construct a `ServerContext` with `with_external_provider(provider)` and
  assert `ctx.external_provider()` returns `Some`.
- Output: Compilation failure — `ServerContext` does not have this method.

---

#### 5.6 GREEN — Extend `ServerContext` with optional external provider
- File: `crates/tessera-server/src/context.rs` (extend)
- Add field:
  ```rust
  external_provider: Option<Arc<dyn ExternalAuthProvider>>,
  ```
- Add builder method `with_external_provider(p: Arc<dyn ExternalAuthProvider>) -> Self`
  (builder pattern or setter, keeping `new()` backward-compatible by defaulting to
  `None`).
- Add `pub fn external_provider(&self) -> Option<&Arc<dyn ExternalAuthProvider>>`.
- `tessera-server/Cargo.toml` gains `tessera-auth = { workspace = true }` (already
  present — verify).
- Output: Test 5.5 passes.

---

#### 5.7 GREEN — Wire `ConnectionHandler::handle_login` to dispatch based on auth mode
- File: `crates/tessera-server/src/connection.rs` (extend)
- Logic in `handle_login`:
  ```rust
  if let Some(provider) = self.ctx.external_provider() {
      // External auth path
      match auth_dispatch::authenticate_external(
          username, password, provider,
          &self.ctx.group_mapping(), // new ServerContext accessor
          self.ctx.sessions(),
      ).await {
          Ok((user_id, token)) => { /* send AuthOk */ }
          Err(e) => { /* send_auth_failure */ }
      }
  } else {
      // Existing local auth path (unchanged)
      ...
  }
  ```
- `ServerContext` also gains `group_mapping: Arc<HashMap<String, String>>` and
  accessor `group_mapping()`.
- The `Password::new()` validation guard on the local path MUST NOT be applied to
  external auth (JWTs and LDAP passwords have no Argon2 policy constraints).
- This is the ONLY change to `connection.rs`. The query dispatch, session
  validation, and all other logic are untouched.
- Output: Existing connection tests continue to pass; new external auth path is
  exercised by integration test in 5.8.

---

#### 5.8 RED — Integration test: full login cycle with mock external provider via `ConnectionHandler`
- File: `crates/tessera-server/tests/auth_dispatch.rs` (extend)
- Use the existing `ConnectionHandler` test harness (in-memory duplex stream).
- Construct a server context with `AlwaysOkProvider`.
- Send `ClientMessage::Login { username: "alice", password: "jwt-token-here" }`.
- Assert `ServerMessage::AuthOk { token }` is received.
- Send `ClientMessage::Query { ... }` with the token.
- Assert query response (not `AuthError`).
- Output: Test compiles and passes only if the full wiring is correct.

---

#### 5.9 GREEN — Extend `main.rs` to read `TESSERA_AUTH_MODE` and wire the provider
- File: `crates/tessera-server/src/main.rs` (extend)
- Pattern:
  ```rust
  let auth_mode: AuthMode = std::env::var("TESSERA_AUTH_MODE")
      .unwrap_or_else(|_| "local".to_owned())
      .parse()
      .expect("TESSERA_AUTH_MODE must be local, ldap, or oidc");

  let external_provider: Option<Arc<dyn ExternalAuthProvider>> = match auth_mode {
      AuthMode::Local => None,
      AuthMode::Ldap  => {
          let cfg = LdapConfig::from_env().expect("LDAP config invalid");
          let provider = LdapAuthProvider::new(cfg).await.expect("LDAP provider init");
          Some(Arc::new(provider))
      }
      AuthMode::Oidc  => {
          let cfg = OidcConfig::from_env().expect("OIDC config invalid");
          let provider = OidcAuthProvider::new(cfg);
          Some(Arc::new(provider))
      }
  };

  let group_mapping = Arc::new(
      std::env::var("TESSERA_LDAP_GROUP_MAPPING")
          .or_else(|_| std::env::var("TESSERA_OIDC_GROUP_MAPPING"))
          .map(|s| parse_group_mapping(&s))
          .unwrap_or_default()
  );
  ```
- When `AuthMode::Local`, skip `external_provider` wiring — existing admin password
  path is used exactly as before.
- Output: `cargo build --bin tessera-server` succeeds with zero warnings.

---

### Phase 6: Wiring Verification (Compile + Test Suite)

---

#### 6.1 — Full workspace compile with clippy
- Command: `cargo clippy --workspace --all-targets -- -D warnings`
- Expected: zero errors, zero warnings.
- If any `clippy::pedantic` or `clippy::nursery` warnings fire, fix them before
  proceeding (do not add `#[allow(...)]` without a documented reason in a comment).

---

#### 6.2 — Full test suite
- Command: `cargo test --workspace`
- Expected: all tests pass.
- Pay specific attention to existing tests in `tessera-auth` and `tessera-server`
  that exercise the local auth path — they must be unaffected by this change.

---

#### 6.3 — Security audit checklist (manual, before merging)
- [ ] `grep -r "bind_password" crates/tessera-auth/src/providers/ldap.rs` — ensure
      it never appears in a `tracing::` call or format string.
- [ ] Verify no JWT string is formatted into a `tracing::` call anywhere in
      `crates/tessera-auth/src/providers/oidc.rs`.
- [ ] Verify all `AuthError::ProviderUnavailable` are mapped to `InvalidCredentials`
      before any `ServerMessage` is sent.
- [ ] Verify OIDC `alg=none` test (`oidc_none_algorithm_rejected`) passes.
- [ ] Verify LDAP injection test (`ldap_escape_prevents_injection`) passes.
- [ ] Run `cargo audit` — ensure no new advisory vulnerabilities from `ldap3` or
      `jsonwebtoken`.

---

## New Files Summary

| File | Action |
|------|--------|
| `crates/tessera-auth/src/providers/mod.rs` | Create |
| `crates/tessera-auth/src/providers/group_mapping.rs` | Create |
| `crates/tessera-auth/src/providers/ldap.rs` | Create |
| `crates/tessera-auth/src/providers/oidc.rs` | Create |
| `crates/tessera-auth/src/external_config.rs` | Create |
| `crates/tessera-auth/tests/providers_group_mapping.rs` | Create |
| `crates/tessera-auth/tests/providers_ldap.rs` | Create |
| `crates/tessera-auth/tests/providers_oidc.rs` | Create |
| `crates/tessera-auth/tests/external_config.rs` | Create |
| `crates/tessera-server/src/auth_dispatch.rs` | Create |
| `crates/tessera-server/tests/auth_dispatch.rs` | Create |

## Modified Files Summary

| File | Change |
|------|--------|
| `crates/tessera-auth/src/lib.rs` | Add `pub mod providers; pub mod external_config;` |
| `crates/tessera-auth/src/error.rs` | Add `ConfigError`, `ProviderUnavailable` variants |
| `crates/tessera-auth/Cargo.toml` | Add `ldap3`, `tokio`, `jsonwebtoken`; add `rsa` to dev-dependencies |
| `crates/tessera-server/src/context.rs` | Add `external_provider` + `group_mapping` fields + accessors |
| `crates/tessera-server/src/connection.rs` | Extend `handle_login` with external dispatch |
| `crates/tessera-server/src/main.rs` | Read `TESSERA_AUTH_MODE`, construct provider |

## Estimation

| Phase | Implementation | Tests |
|-------|---------------|-------|
| 1: Trait + group mapping | 30 min | 30 min |
| 2: LDAP provider | 90 min | 60 min |
| 3: OIDC provider | 90 min | 60 min |
| 4: Config + errors | 30 min | 30 min |
| 5: Server wiring | 60 min | 45 min |
| 6: Verification | — | 30 min |
| **Total** | **5 h** | **4 h 15 min** |

## Criteria de Exito

- [ ] `cargo test --workspace` — all tests pass, including all pre-existing tests
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` — zero diagnostics
- [ ] `cargo build --bin tessera-server` — succeeds
- [ ] All six security audit checklist items confirmed
- [ ] `LdapAuthProvider::authenticate` never logs `bind_password` or user password
- [ ] `OidcAuthProvider::authenticate` never logs the JWT string
- [ ] All OIDC validation failures (expired, wrong iss, wrong aud, bad sig, alg=none)
      return `AuthError::InvalidCredentials` — verified by dedicated test per case
- [ ] All LDAP failure cases (server unreachable, user not found, wrong password)
      return `AuthError::InvalidCredentials` — verified by dedicated test per case
- [ ] LDAP username is RFC 4515 escaped before filter substitution — verified by test
- [ ] `AuthMode::Local` path is entirely unaffected — all existing `tessera-server`
      and `tessera-auth` tests continue to pass without modification
