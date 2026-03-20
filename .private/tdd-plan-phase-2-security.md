# Phase 2 — Security: TDD Implementation Plan

**Estado**: Completado
**Estimacion**: 9-10 horas
**Rama**: `feature/security-phase2` (desde `develop`)

## Decisiones Arquitectonicas

- **Fail-safe en type system**: `ServerContext` no compila sin `AuthPolicy` + `TlsConfig`
- **Argon2id** para hashing, **rustls** para TLS (pure Rust, no OpenSSL)
- **Tokens opacos** (32 random bytes, base64ct) — sin JWT, sesion validada contra store
- **`Password` y `SessionToken`** no implementan `Debug` ni `Clone` — prevencion de leaks
- **zeroize** para limpieza de secrets en memoria
- **Permission check O(1)** via `HashSet` — threshold >=500K ops/s debug, >=5M release

## Fases

### Fase 1: Dependencias y estructura base de `tessera-auth`

- [ ] 1. Actualizar `crates/tessera-auth/Cargo.toml` con dependencias de seguridad
  - argon2 0.5, rand 0.9, zeroize 1 (derive), base64ct 1 (std), constant_time_eq 0.3
  - serde 1 (derive), serde_json 1, thiserror (workspace)
- [ ] 2. Actualizar `crates/tessera-protocol/Cargo.toml` con dependencias TLS
  - rustls 0.23, rustls-pemfile 2, thiserror (workspace)
- [ ] 3. Actualizar workspace `Cargo.toml` — serde, serde_json en workspace.dependencies
- [ ] 4. Crear estructura de modulos en `tessera-auth/src/lib.rs` y error central
  - Modulos: credentials, user, session, rbac, policy, rate_limit
  - AuthError enum con variantes: InvalidCredentials, UserNotFound, UserAlreadyExists, PermissionDenied, TokenExpired, TokenInvalid, PasswordPolicyViolation, StorageError, LockPoisoned

### Fase 2: Credential hashing (TDD ciclo 1)

- [ ] 5. RED — tests de hashing (credentials_test.rs)
  - hash_password_produces_valid_argon2id_hash
  - verify_correct_password_returns_ok
  - verify_wrong_password_returns_err
  - two_hashes_of_same_password_differ
  - empty_password_is_rejected_by_policy
  - password_too_short_is_rejected (min 8 chars)
  - zeroized_password_cannot_be_read_after_hash
- [ ] 6. GREEN — implementar credentials module
  - Password(String) con Zeroize+Drop, no Clone, no Debug
  - PasswordHash(String) opaque wrapper
  - PasswordHasher con Argon2id
  - PasswordPolicy (min_length, require_uppercase, require_digit, require_symbol)
- [ ] 7. REFACTOR — tipos coherentes, clippy limpio

### Fase 3: User store (TDD ciclo 2)

- [ ] 8. RED — tests del user store (user_store_test.rs)
  - create_user_stores_argon2id_hash_not_plaintext
  - create_duplicate_user_returns_error
  - authenticate_valid_credentials_returns_user
  - authenticate_invalid_password_returns_invalid_credentials
  - authenticate_nonexistent_user_returns_invalid_credentials (timing-safe)
  - delete_user_then_authenticate_returns_error
  - change_password_invalidates_old_hash
  - list_users_does_not_expose_hashes
  - user_store_survives_roundtrip_json_serialization
  - builtin_admin_user_exists_on_new_store
- [ ] 9. GREEN — implementar user module
  - UserId(u64), UserRecord, UserStore, UserStoreHandle(Arc<RwLock>)
  - create_user, authenticate, delete_user, change_password, list_usernames
  - save_to_file, load_from_file (JSON)
- [ ] 10. REFACTOR — seguridad en serializacion

### Fase 4: Session tokens (TDD ciclo 3)

- [ ] 11. RED — tests de sesiones (session_test.rs)
  - create_session_returns_opaque_token
  - validate_valid_token_returns_user_id
  - validate_expired_token_returns_token_expired
  - validate_unknown_token_returns_token_invalid
  - revoke_session_then_validate_returns_token_invalid
  - two_sessions_for_same_user_are_independent
  - session_manager_is_send_sync
  - token_is_url_safe_base64
- [ ] 12. GREEN — implementar session module
  - SessionToken(String) opaque, no Debug
  - Session { user_id, created_at, expires_at }
  - SessionManager { sessions: Arc<RwLock<HashMap>>, ttl_seconds }
- [ ] 13. REFACTOR

### Fase 5: RBAC types (TDD ciclo 4)

- [ ] 14. RED — tests de tipos RBAC (rbac_types_test.rs)
  - admin_role_has_all_permissions
  - readonly_role_cannot_create_nodes
  - readwrite_role_can_create_and_delete
  - monitor_role_has_only_monitor_permission
  - custom_role_with_single_permission
  - permission_display_roundtrip
  - role_is_serializable_json
- [ ] 15. GREEN — implementar rbac module
  - Permission enum (NodeCreate/Read/Update/Delete, Edge*, GraphFlush/Backup/Config, Admin*, Monitor)
  - RoleId(u64), Role { id, name, permissions: HashSet }
  - RoleStore con predefined: admin, readwrite, readonly, monitor
- [ ] 16. REFACTOR

### Fase 6: Policy enforcement (TDD ciclo 5)

- [ ] 17. RED — tests de policy (policy_test.rs)
  - admin_user_can_perform_any_operation
  - readonly_user_cannot_create_node
  - readonly_user_can_read_node
  - user_with_no_roles_is_denied_everything
  - user_with_multiple_roles_union_of_permissions
  - unknown_user_id_is_denied (fail-safe)
  - permission_denied_error_contains_required_permission
- [ ] 18. GREEN — implementar policy module
  - AuthPolicy { user_store: Arc<UserStoreHandle>, role_store: Arc<RwLock<RoleStore>> }
  - check(user_id, Permission) -> Result<()>
  - check_session(token, Permission, SessionManager) -> Result<UserId>
- [ ] 19. REFACTOR — superficie publica de tessera-auth

### Fase 7: Brute-force protection (TDD ciclo 6)

- [ ] 20. RED — tests brute force (brute_force_test.rs)
  - five_failed_attempts_trigger_lockout
  - locked_account_rejects_correct_password
  - lockout_expires_after_configured_duration
  - successful_login_resets_failure_counter
  - different_users_have_independent_counters
- [ ] 21. GREEN — implementar rate_limit module
  - LoginAttemptTracker, LoginPolicy { max_attempts, lockout_duration_secs }
  - Integrar en UserStoreHandle::authenticate()

### Fase 8: SSL/TLS (TDD ciclo 7)

- [ ] 22. RED — tests TLS (tls_test.rs)
  - tls_config_loads_valid_cert_and_key
  - tls_config_rejects_mismatched_cert_and_key
  - tls_config_rejects_expired_cert
  - tls_config_rejects_missing_cert_file
  - tls_config_without_client_auth_accepts_connection
  - tls_config_with_mtls_requires_client_cert
  - tls_config_enforces_minimum_tls_version_1_3
- [ ] 23. GREEN — implementar tls module en tessera-protocol
  - TlsConfig wraps Arc<rustls::ServerConfig>
  - ClientAuth { None, Optional, Required }
  - TlsConfigBuilder (cert_file, key_file, client_auth, build)
  - Enforce TLS 1.3 only

### Fase 9: Activity auditing (TDD ciclo 8)

- [ ] 24. RED — tests audit (audit_test.rs)
  - audit_log_records_successful_operation
  - audit_log_records_denied_operation
  - audit_log_preserves_user_timestamp_operation_and_result
  - audit_log_is_append_only
  - audit_log_survives_roundtrip_to_file
  - audit_entry_serializes_to_json_lines_format (NDJSON)
- [ ] 25. GREEN — implementar tessera-audit
  - AuditEntry, AuditResult { Success, Denied, Error }
  - AuditLog wraps Arc<Mutex<BufWriter<File>>> — append-only NDJSON

### Fase 10: Server integration (TDD ciclo 9)

- [ ] 26. RED — tests integracion server (auth_integration_test.rs)
  - server_context_requires_auth_policy_to_construct
  - server_context_without_tls_config_fails
  - server_context_is_send_sync
  - permission_check_propagates_through_context
  - unauthenticated_request_is_denied
- [ ] 27. GREEN — implementar ServerContext en tessera-server
  - ServerContext { auth_policy, sessions, audit, tls }
  - check_permission(token, Permission) -> Result<UserId>
- [ ] 28. REFACTOR — validacion workspace completo

### Fase 11: Tests adversariales

- [ ] 29. Tests de seguridad (security_test.rs)
  - privilege_escalation_readonly_cannot_call_admin_operation
  - privilege_escalation_readwrite_cannot_manage_users
  - token_reuse_after_logout_is_rejected
  - concurrent_session_creation_all_tokens_unique (100 threads)
  - authenticate_timing_same_for_valid_and_invalid_user
  - password_hash_never_appears_in_list_users_output
  - expired_token_even_one_second_over_is_rejected

### Fase 12: Throughput regression guard

- [ ] 30. Test throughput (throughput_test.rs)
  - permission_check_throughput_regression_guard
  - Threshold: >=500K ops/s debug, >=5M ops/s release

## Criterios de Exito

- `cargo test --workspace` — todos pasan (pre-existentes + nuevos)
- `cargo clippy --workspace --tests -- -D warnings` — cero warnings
- `ServerContext` no compila sin `AuthPolicy` + `TlsConfig`
- Password/SessionToken no implementan Debug
- Permission check >= 500K ops/s debug, >= 5M ops/s release
