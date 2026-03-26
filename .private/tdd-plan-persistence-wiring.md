# TDD Plan: Server Persistence Wiring

**Created**: 2026-03-23
**Status**: Pending

## Phases
1. Add `Storage` variant to `ServerError`
2. Flush after successful mutation in `handle_query`
3. `PersistenceConfig::from_env()` + env-driven graph init in `main.rs`
4. `flush_on_shutdown()` helper + wire in `main.rs`
5. Throughput canary
6. Wiring verification
