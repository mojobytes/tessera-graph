# Phase 2 Quality Fixes Round 2: TDD Plan

**Estado**: En progreso
**Estimacion**: ~4 horas

## Fases

### Fase 1: R-NEW-5 — Harden flaky sleep tests
- [ ] 1.1 session_test: TTL=1 + sleep(2s)
- [ ] 1.2 brute_force_test: lockout=1 + sleep(2s)

### Fase 2: R-NEW-2 — PasswordPolicyBuilder::default() consistency
- [ ] 2.1 RED: Tests builder default matches policy default
- [ ] 2.2 GREEN: Fix builder default to all true

### Fase 3: R-NEW-4 — Document LoginAttemptTracker volatility
- [ ] 3.1 Add doc comment limitation

### Fase 4: R-NEW-1 — validate() read-lock on happy path
- [ ] 4.1 RED: Concurrent validation test
- [ ] 4.2 GREEN: Read lock for happy path, write lock only for expired

### Fase 5: R-NEW-3 — Atomic save_to_file()
- [ ] 5.1 RED: Test no .tmp file remains
- [ ] 5.2 GREEN: Write to .tmp + rename

### Fase 6: C-NEW-1 — TOCTOU in change_password
- [ ] 6.1 RED: Concurrent change_password race test
- [ ] 6.2 GREEN: Re-verify hash in write lock

### Fase 7: C-NEW-2 — Zeroize UserRecord, remove Clone
- [ ] 7.1 RED: Compile-time zeroize assertion
- [ ] 7.2 GREEN: Implement Zeroize+Drop, remove Clone

### Fase 8: Wiring verification
- [ ] 8.1 cargo test -p tessera-auth
- [ ] 8.2 cargo clippy --workspace --tests
- [ ] 8.3 cargo build -p tessera-server
