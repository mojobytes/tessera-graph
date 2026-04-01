# Auditoría de Resiliencia — TesseraGraph Enterprise
Fecha: 2026-04-02
Commit: f55283d (develop)

## Estado General: WARNING (4 CRITICAL, 6 HIGH, 7 MEDIUM, 13 PASS)

## CRITICAL

| # | Check | Hallazgo | Archivo:Línea | Impacto |
|---|-------|----------|---------------|---------|
| 1 | A3 — Query results materialized | `PendingResult` acumula ALL rows en `Vec<Vec<PackStreamValue>>` antes de enviar. Sin streaming, sin LIMIT server-side. | bolt_handler.rs:47-48,564 | OOM en queries grandes. Un `MATCH (n) RETURN n` sobre 1M nodos crashea el server. |
| 2 | C2 — Atomicity gap en TxnManager | WAL.sync() pasa pero committed set no actualizado. Crash en esa ventana pierde transacciones. Recovery scanning NOT YET IMPLEMENTED. | txn/manager.rs:115-132 | Pérdida silenciosa de datos committed. |
| 3 | E2 — WAL reader stop-on-first-error | WAL reader para en el primer record corrupto. Records válidos posteriores se pierden silenciosamente. | tessera-graph/wal/reader.rs:65-72 | Un bit-flip descarta datos durables. |
| 4 | A5 — Session storage sin cleanup | Sessions solo se purgan lazily en validate(). Sin cleanup periódico, HashMap crece sin límite. | session.rs:54,144-167 | Memory leak proporcional al churn de conexiones. |

## HIGH

| # | Check | Hallazgo | Archivo:Línea | Impacto |
|---|-------|----------|---------------|---------|
| 5 | A4 — Memory limit not enforced | `TESSERA_MEMORY_LIMIT_MB` se parsea pero solo configura BufferPool. Query results, sessions, tenant registry no están limitados. | config.rs:46-51 | Falsa sensación de seguridad. |
| 6 | A6 — Tenant registry no eviction | `unload()` existe pero nadie lo llama. Cada tenant permanece en memoria indefinidamente. | registry.rs:22-23,277 | Unbounded memory growth con multi-tenancy. |
| 7 | D3 — No disk space checks | Zero validación de espacio antes de WAL writes o page flushes. | (todo el codebase) | Disco lleno causa cascada de errores I/O. |
| 8 | E3 — SIGKILL audit loss | BufWriter de audit pierde entries no flusheados. | audit/lib.rs:173 | Pérdida de audit trail en kill forzado. |
| 9 | E4 — Session TTL wall clock | Session TTL usa SystemTime (vulnerable a NTP jumps). Rate limiter usa Instant correctamente. | auth/utils.rs:8-13, session.rs:156 | Clock skew causa sesiones inmortales o expiración masiva. |
| 10 | G1 — Metrics sin auth | /metrics y /health sin autenticación. Expone counters operativos. | monitor/server.rs:31-56 | Leak de inteligencia operativa. |

## MEDIUM

| # | Check | Hallazgo | Archivo:Línea | Impacto |
|---|-------|----------|---------------|---------|
| 11 | B3 — Lock poisoning | PASS — todos usan .map_err(), no .unwrap() en locks. | (múltiples) | Bien diseñado. |
| 12 | D2 — FDs per tenant | ~6-7 FDs por tenant. Con ulimit 1024, ~146 tenants saturan. Sin monitoring. | registry.rs | FD exhaustion opaca a escala. |
| 13 | F1 — Swallowed errors | `let _ = handler.run().await` silencia errores de conexión. Audit errors silenciados en 8 call sites. | listener.rs:123, bolt_handler.rs:341+ | Fallos de conexión invisibles. |
| 14 | G3 — Audit channel overflow | Audit entries dropeados silenciosamente cuando channel lleno. Sin métrica ni alerta. | audit/lib.rs:210-218 | Attacker puede suprimir audit trail con flood. |
| 15 | H2 — No system metrics | Solo métricas de aplicación. Sin disco, RSS, FD count. | render.rs | OOM y disk full invisibles hasta crash. |
| 16 | I2 — Env vars lazy validation | Typos caen silenciosamente al default sin warning. | main.rs:32-39, config.rs:46-67 | Misconfiguration silenciosa. |
| 17 | C4 — No index rebuild command | Rebuild automático al arrancar, pero no hay hot-repair. | tessera-graph/graph.rs:103-106 | Recovery requiere restart. |

## PASS (13)

| # | Check | Estado |
|---|-------|--------|
| A2 | Canales bounded | PASS — audit usa mpsc::channel(capacity) |
| B1 | Lock ordering | PASS — no nested locks en producción |
| B2 | TOCTOU | PASS — double-check locking correcto |
| B4 | Guards across .await | PASS — guards dropeados antes de .await |
| C1 | WAL enabled default | PASS — wal_enabled = true |
| C3 | CRC en WAL | PASS — CRC32 per record, validado al leer |
| C5 | Backup integrity | PASS — CRC32 per file en manifest + verify() |
| D1 | Connection limit | PASS — Semaphore enforced, default 256 |
| E1 | WAL replay | PASS — automático en Graph::open(), idempotente |
| I1-a | Admin password | PASS — .expect() si no está configurado |
| I1-b | TLS required | PASS — .expect() si no hay certs |
| G2 | Secrets en código | PASS — no hardcoded, tokens CSPRNG |
| H1 | Health endpoint | PASS — flush degradation + recovery |

## Prioridad de remediación

### Inmediata (antes de producción)
1. CRITICAL #1 — Streaming de results / LIMIT server-side
2. CRITICAL #2 — WAL recovery scanning para atomicity gap
3. CRITICAL #3 — WAL reader forward scanning past corruption
4. CRITICAL #4 — Session cleanup background task

### Corto plazo (primera semana)
5. HIGH #9 — Session TTL con Instant monotónico
6. HIGH #7 — Disk space check periódico
7. HIGH #10 — Metrics auth o documentar aislamiento de red

### Medio plazo
8. MEDIUM #14 — Audit overflow metric
9. MEDIUM #16 — Env var validation con warnings
10. HIGH #6 — Tenant eviction LRU/TTL
