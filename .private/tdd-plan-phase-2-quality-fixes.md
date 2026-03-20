# Phase 2 Quality Fixes: TDD Plan

**Estado**: En progreso
**Rama**: `feature/security-phase2` (continua)
**Estimacion**: 6-8 horas

## Fases

### Fase 1: C3 + C2 + R2 (session.rs — revoke Result, constant_time_eq)
- [ ] 1.1 RED: Tests revoke/revoke_all retornan Result
- [ ] 1.2 GREEN: Cambiar firmas, propagar LockPoisoned
- [ ] 1.3 GREEN: SessionToken::PartialEq constant-time (usa constant_time_eq)
- [ ] 1.4 REFACTOR: clippy + tests

### Fase 2: C1 (mTLS — error explicito)
- [ ] 2.1 RED: Test que ClientAuth::Required falla con error explicito
- [ ] 2.2 GREEN: Eliminar AllowAnyAuthenticatedClient, retornar error
- [ ] 2.3 REFACTOR: clippy + tests, eliminar test viejo tls_config_with_mtls_flag

### Fase 3: C4 (change_password lock)
- [ ] 3.1 RED: Test concurrencia change_password no bloquea reads
- [ ] 3.2 GREEN: Refactorizar change_password (read lock, release, hash, write lock)
- [ ] 3.3 REFACTOR: clippy + tests

### Fase 4: R1 + R5 + R4 (utils, list_usernames Result, indice secundario)
- [ ] 4.1 R1: Extraer unix_timestamp a utils.rs
- [ ] 4.2 RED R5: Test list_usernames retorna Result
- [ ] 4.3 GREEN R5: Cambiar firma
- [ ] 4.4 RED R4: Test get_user_roles con indice secundario
- [ ] 4.5 GREEN R4: Agregar id_to_username HashMap
- [ ] 4.6 REFACTOR: clippy + tests + throughput

### Fase 5: R3 + R6 + R7 (Zeroize, record_error, RoleStoreHandle)
- [ ] 5.1 RED R3: Test PasswordHash implements Zeroize
- [ ] 5.2 GREEN R3: Agregar Zeroize/ZeroizeOnDrop, quitar Clone
- [ ] 5.3 R6: record_error() en AuditLog + actualizar ServerContext
- [ ] 5.4 R7: Crear RoleStoreHandle, actualizar AuthPolicy
- [ ] 5.5 REFACTOR: clippy + tests todos los crates

### Fase 6: Wiring verification
- [ ] 6.1 cargo build --workspace
- [ ] 6.2 cargo test --workspace
- [ ] 6.3 cargo clippy --workspace --tests
- [ ] 6.4 throughput regression guard
- [ ] 6.5 invariantes de seguridad criticas
