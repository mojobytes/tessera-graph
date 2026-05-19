# Migration Plan — MIT Core 0.5.0 Sync

**Created:** 2026-05-19
**Status:** Phase 1 in progress — blocked on upstream bug in `mojobytes/tessera-graph` 0.5.0
**Target branch:** `feature/resilience-streaming-quality` (Phase 1 was done here in-place; `feature/mit-core-0.5.0-sync` was never created — see Phase 1 execution log below)
**Estimated total effort:** 17–33 h across 7 sessions

---

## Execution log

### 2026-05-19 — Phase 1 partially executed (BLOCKED)

Phase 1 of the plan (delete 3 duplicated crates + adapt API + reach `cargo check`/`clippy` green) was executed in-place on `feature/resilience-streaming-quality` because that branch already held the prerequisites (`adj_index.rs` commit, `.gitignore` update, this plan document). 7 commits, all local, none pushed:

- `341c8c4` `fix(storage): commit AdjacencyIndex implementation referenced by lib.rs`
- `74b7ed8` `chore(gitignore): ignore local agent state and graph export artefacts`
- `71726d2` `docs(plan): MIT core 0.5.0 sync — 7-phase migration plan` (this file)
- `c83f177` `refactor(workspace): delete tessera-graph-config — MIT core is functionally identical`
- `fe5e8ab` `refactor(workspace): delete tessera-graph-cypher — MIT core is superset`
- `1f14395` `refactor(workspace): delete tessera-graph-cli — MIT core is superset`
- `6dfa0dd` `refactor(storage,server,import): adapt to MIT core 0.5.0 API`

**Green:**
- `cargo check --workspace` ✅
- `cargo clippy --workspace --tests -- -D warnings` ✅
- `cargo test --workspace --no-run` ✅

**Red — 5 test failures, all caused by an upstream MIT core 0.5.0 bug:**
- `cypher_compat_starts_with_filters_correctly`
- `cypher_compat_starts_with_case_insensitive_keyword`
- `cypher_compat_ends_with_filters_correctly`
- `cypher_compat_in_with_starts_with`
- `cypher_compat_string_ops_combined_with_and`

**Root cause** (in `mojobytes/tessera-graph` 0.5.0): the parser in `crates/tessera-graph/src/gql/parser.rs:1220-1248` has explicit `STARTS WITH` / `ENDS WITH` handling but the `peek_ahead(1) == Token::Ident("WITH")` check never matches because the lexer tokenises `WITH` as `Token::With` (reserved keyword for `WITH x AS y` pipelines). The `STARTS WITH` / `ENDS WITH` parsing is **dead code** in 0.5.0. See `.private/error-log.md` entry `[2026-05-19]` for the full diagnosis and proposed fix.

**Blocker resolution path:**

1. Open an issue in `mojobytes/tessera-graph` with the bug report drafted at session end (title: `fix(gql): STARTS WITH / ENDS WITH unreachable in parser — peek_ahead checks Ident("WITH") but lexer emits Token::With`).
2. Apply the proposed fix in the MIT core: change the condition to accept both `Token::With` and `Token::Ident("WITH")`. Add the 5 missing tests upstream (`STARTS WITH`, `ENDS WITH`, `CONTAINS`, `IN [...]`, combined-with-AND).
3. Once the MIT core fix lands, rerun `cargo test --workspace` from this enterprise repo — should be green without touching any enterprise code.
4. Push the 7 commits → reopen the work on PR #1 → run Phase 1 §3.5 (fix CI workflow) → merge.

**Remaining Phase 1 work (not blocked by the upstream bug):**

- Step 9 of the original §3.1 list: fix `.github/workflows/ci.yml` (`actions/checkout` cannot use `path: ../tessera-graph` — see Phase 1 §3.5 below).
- Step 10: push, validate green CI, merge PR #1.

## Phase 1 — §3.5: Fix CI workflow (deferred from Phase 1, blocked by upstream)

The current `.github/workflows/ci.yml` clones the MIT core via `actions/checkout@v4` with `path: ../tessera-graph`. This fails at runtime with `"Repository path is not under '/home/runner/work/.../tessera-graph-enterprise'"` because `actions/checkout` only accepts paths inside `$GITHUB_WORKSPACE`.

**Two viable fixes:**

A. **Shell-step clone** — replace the second `actions/checkout` with a `run:` step that does `git clone https://x-access-token:${{ secrets.GH_PAT }}@github.com/mojobytes/tessera-graph.git ../tessera-graph`. Requires a PAT with read access to the MIT core repo (or `GITHUB_TOKEN` if the MIT core is public). Preserves the current `../tessera-graph` layout, no Cargo.toml changes needed.

B. **Restructure layout** — move the MIT core into a subdirectory of the enterprise repo (e.g. `vendor/tessera-graph/`) and update the 3 `path =` entries in `Cargo.toml`. `actions/checkout` can then clone with `path: vendor/tessera-graph`. Breaks every local developer who has the current `../tessera-graph` layout.

**Recommendation:** A. Less disruptive to local dev. The PAT (if needed) can be the same one already used by `tessera-bench` for Bolt benchmarks.

---

---

## 1. Contexto y motivación

El workspace enterprise está **roto desde una checkout limpia** desde aproximadamente 2026-04-21. Diagnóstico:

- El enterprise declara `tessera-graph = { ..., features = ["extended-gql"] }` en `Cargo.toml:27` y en `crates/tessera-graph-storage/Cargo.toml:20`.
- El MIT core eliminó la feature `extended-gql` en el commit `1a5b40b` del 2026-04-21 con justificación explícita: *"Memgraph places WITH, UNWIND, GROUP BY, variable-length paths, list literals, shortestPath() and COLLECT in its open-source community edition. A compilation feature flag does not enforce licensing."*
- El MIT core saltó a **v0.5.0** el 2026-05-18 introduciendo multi-database real (`DatabaseRegistry`, per-RUN database binding, on-disk layout v2, migration tool obligatoria, `:Database` catalogue, `GRANT ACCESS ON DATABASE`).
- El MIT core cambió de **MIT → BSD-3-Clause → BSL-1.1** (Business Source License). Cambio intencional confirmado por el usuario en sesión del 2026-05-18.

**Resultado:** 157 commits de divergencia entre lo último que el enterprise vio y el HEAD actual del MIT core. El enterprise asume APIs/features/layout que ya no existen.

**Objetivo del plan:** restaurar la compilación, adoptar lo que el core 0.5.0 ya hace (multi-database, admin, audit, migration), y delimitar con precisión qué sigue siendo valor enterprise propietario.

## 2. Estado actual auditado

### 2.1 Crates duplicados nombre-a-nombre entre ambos repos

| Crate | Enterprise (LOC src) | MIT core (LOC src) | Diff funcional | Acción |
|---|---|---|---|---|
| `tessera-graph-config` | 208 | 208 | 1 línea (copyright header) | **Eliminar enterprise** |
| `tessera-graph-cypher` | 1.470 | ~2.300 (con `admin.rs`) | Core es superset estricto: añade `admin.rs` (`CREATE USER`, `DROP USER`, `ALTER USER`, `SHOW USERS`, `GRANT`), `try_parse_admin()` previo al pipeline GQL, `QueryCache` con clave `(query, params_signature)` (más correcto que el del enterprise) | **Eliminar enterprise** |
| `tessera-graph-cli` | 3.997 | ~4.200 (+ `admin/` + `migrate.rs`) | Core es superset estricto: añade `admin/` (subcomandos `users`, `databases`, `grants`, `hash`), `migrate.rs` (rewrite `{data-dir}/data/` → `{data-dir}/databases/<name>/`), `cli.rs` 25KB vs 6KB enterprise. `repl.rs` idéntico (4 líneas de diff de header) | **Eliminar enterprise** |
| `tessera-graph-server` | 2.501 | ~7.500 | **NO es superset** — ambos tienen funcionalidad propia | **Refactorizar enterprise** para apoyarse en core; mantener overlay |

### 2.2 Crates exclusivamente enterprise (no tocar en fase 1)

- `tessera-graph-auth` — Argon2id + RBAC roles + LBAC Bell-LaPadula. Coexiste con auth básico del core.
- `tessera-graph-storage` — LBAC + `NeighborCache` + `SharedNeighborCache` + `AdjacencyIndex`.
- `tessera-graph-import` — Streaming import multi-formato.
- `tessera-graph-streaming` — Pipeline de import streaming.
- `tessera-graph-monitor` — Prometheus metrics + `/metrics` endpoint.
- `tessera-graph-audit` — Eventos NDJSON propios (potencialmente código muerto post-sync, ver fase 4).
- `tessera-graph-replication` — Replication (estado: stub, no operativo).
- `tessera-graph-tenant` — Multi-tenancy server→tenant→database (potencialmente código muerto post-sync, ver fase 4).
- `tessera-graph-benchmark` — Benchmark harness vs Memgraph.

### 2.3 Capacidades nuevas del core 0.5.0 ausentes en enterprise

- **`DatabaseRegistry`** — in-memory orchestrator de databases con `max_connections`, idle-TTL sweeper, lock-free fast path.
- **`Graph::open_with_hook`** — commit hook pre-WAL para enforcement de quota; el enterprise tendrá que adoptarlo para wiring de límites por tenant.
- **`Error::QuotaExceeded { path, limit_bytes, current_bytes }`** — surface como `Neo.ClientError.General.StorageExhausted` con path stripped.
- **HELLO authentication-only + per-RUN database binding** — contrato Bolt 5.x (`session(database=...)`). El handler enterprise espera modelo viejo de HELLO con `extras["db"]` fijando la sesión.
- **Catálogo del sistema** — `:Database`, `:User`, `:Wildcard` nodes en un system graph separado. Queries de usuario no pueden alcanzarlo.
- **Audit log con `event_type` top-level** — `database_created`, `database_dropped`, `grant_changed`, `auth_success_with_database`. Campo `database` obligatorio en todo evento post-HELLO. `access_level` en lowercase.
- **`tessera-graph-cli migrate`** — comando obligatorio para migrar layout v1 → v2 antes de arrancar el servidor.

### 2.4 Capacidades enterprise diferenciadas (preservar)

- **LBAC Bell-LaPadula** con compartments + `SecureGraph` / `SecureGraphRef` wrappers + clearance-scoped neighbor cache (`SharedNeighborCache` con `ClearanceKey` partitioning).
- **RBAC** roles + policies con throughput guard.
- **`AdjacencyIndex`** — `HashMap<NodeId, AdjacencyPointer>` unbounded; resuelve el cliff de 65K nodos del `AdjCache` del core.
- **`flush_task.rs`** — deferred WAL flush background task (50ms default).
- **`auth_dispatch.rs`** — multi-provider dispatch entre RBAC nativo + LDAP + OIDC.
- **`ServerContext`** con `neighbor_caches: HashMap<DatabaseAddress, Arc<SharedNeighborCache>>`.
- **`BoltConnectionHandler`** con awareness de RBAC + LBAC + tenancy + batch state.

## 3. Fase 1 — Sesión 2: Eliminación de 3 crates duplicados

**Rama:** `feature/mit-core-0.5.0-sync` desde `develop`.

**Pre-requisito:** branch limpio, exports en raíz ignorados, `adj_index.rs` commiteado (✅ cerrado en sesión 1).

### 3.1 Pasos ejecutables

1. Crear rama `feature/mit-core-0.5.0-sync` desde `develop` (no desde `feature/resilience-streaming-quality` para evitar arrastrar deuda en progreso).
2. Eliminar directorio `crates/tessera-graph-config/` enterprise.
3. Eliminar directorio `crates/tessera-graph-cypher/` enterprise.
4. Eliminar directorio `crates/tessera-graph-cli/` enterprise.
5. Actualizar `Cargo.toml` raíz:
   - Remover los 3 paths eliminados de `[workspace] members`.
   - Mover los 3 `[workspace.dependencies]` para que apunten a `../tessera-graph/crates/tessera-graph-{config,cypher,cli}`.
   - Eliminar `features = ["extended-gql"]` de la entry de `tessera-graph`.
6. Actualizar `crates/tessera-graph-storage/Cargo.toml`:
   - Eliminar la sección `[features]` con `extended-gql` (líneas 19-20).
7. Eliminar todos los `#[cfg(feature = "extended-gql")]` / `#[cfg(not(feature = "extended-gql"))]` en el código (12 ocurrencias en `crates/tessera-graph-storage/src/gql/mod.rs`).
8. Ejecutar `nice cargo check --workspace` y corregir errores de imports rotos uno a uno.
9. Ejecutar `nice cargo test --workspace --no-run` para validar que la suite compila.

### 3.2 Criterio de éxito

- `cargo check --workspace` retorna 0 sin warnings.
- `cargo test --workspace --no-run` retorna 0 sin warnings.
- Los 3 crates enterprise eliminados ya no aparecen en `cargo metadata`.
- El servidor enterprise sigue compilando y arrancando (puede fallar en runtime contra layout v1 — eso es fase 5).

### 3.3 Riesgos conocidos

- **Imports profundos:** algún módulo enterprise puede importar tipos de los 3 crates con paths concretos (`tessera_graph_cypher::cache::QueryCache`). El cambio de signature de `QueryCache` (clave compuesta `(query, params_signature)`) puede romper call sites — habrá que adaptarlos.
- **Tests con fixtures referencing CLI binary:** los tests E2E pueden invocar el CLI como subprocess. Hay que verificar que el path al binario sigue siendo `target/debug/tessera-graph-cli` o si el rename del paquete cambia.

### 3.4 Esfuerzo estimado

3–6 h.

## 4. Fase 2 — Sesión 3: Refactor `tessera-graph-server` enterprise

**Objetivo:** que el server enterprise use `tessera-server-core` 0.5.0 como base, absorbiendo del core todo lo que ahora está duplicado o ausente, y manteniendo como overlay sólo lo genuinamente enterprise.

### 4.1 Decisiones de diseño previas necesarias

- **Multi-database del core vs `tessera-graph-tenant` enterprise** (también afecta fase 4). El core hace 1 server → N databases. El enterprise asume server → tenant → N databases. Si tenancy multi-nivel es un requisito de producto, hay que envolver el `DatabaseRegistry` del core con un `TenantRegistry` enterprise que namespacing los nombres de database por tenant. Si no, eliminar `tessera-graph-tenant` (ver fase 4).
- **HELLO + per-RUN binding** — el `BoltConnectionHandler` enterprise debe adoptar el contrato del core 0.5.0: HELLO sólo autentica, RUN bindea database vía `extras["db"]`. Rebind permitido cuando un RUN siguiente trae un `extras["db"]` distinto.

### 4.2 Pasos ejecutables

1. Cambiar el `tessera-graph-server` enterprise a un crate más fino que dependa de `tessera-server-core`:
   - Adoptar `DatabaseRegistry` del core dentro de `ServerContext`.
   - Pasar el ciclo de vida de `DbHandle` por el handler enterprise.
2. Refactorizar `bolt_handler.rs` enterprise:
   - Quitar lógica de HELLO con database fija; mover binding a RUN.
   - Wire al `admin_handler` del core para statements `CREATE DATABASE`, `GRANT`, `SHOW DATABASES`.
   - Mantener LBAC/RBAC/tenancy interceptors como capa por encima.
3. Adoptar `Graph::open_with_hook` para enforcement de quota por tenant.
4. Actualizar `flush_task.rs` para funcionar contra `DatabaseRegistry` (un flush task por database, no global).
5. Actualizar `context.rs` para mantener `neighbor_caches: HashMap<DatabaseAddress, Arc<SharedNeighborCache>>` con `DatabaseAddress` derivado del registry.

### 4.3 Criterio de éxito

- Servidor enterprise arranca contra layout v2.
- Bolt drivers oficiales (neo4j-driver Python, neo4j-java-driver) se conectan con `session(database="<name>")` y RUN bindea correctamente.
- Tests de `tessera-graph-server` enterprise verdes.
- `tessera-bench --target tessera-bolt` ejecuta sin regresiones de throughput.

### 4.4 Esfuerzo estimado

8–12 h.

## 5. Fase 3 — Sesión 4: Decisión arquitectónica sobre auth

**Tipo de sesión:** diseño + decisión, **no** implementación.

### 5.1 Problema

- El core 0.5.0 tiene `auth/` con grants `READ`/`READ_WRITE` por database.
- El enterprise tiene `tessera-graph-auth` con Argon2id + RBAC roles + LBAC Bell-LaPadula clearance.
- Hay solapamiento: ambos hashean passwords, ambos autorizan operaciones, ambos exponen el concepto de usuario.

### 5.2 Opciones a evaluar

| Opción | Descripción | Pros | Contras |
|---|---|---|---|
| **A. Enterprise extiende vía traits** | El core define `AuthProvider`/`AuthorizationProvider` traits que el enterprise implementa con LBAC/RBAC | Reusa toda la infra del core (`:User` nodes, password hashing, audit hooks). Menos código duplicado. | Requiere que los traits del core sean lo suficientemente flexibles para LBAC compartments — verificar antes de comprometerse |
| **B. Enterprise reemplaza al dispatcher de auth** | El enterprise sobrescribe el handler de HELLO y todas las decisiones de auth, ignorando el sistema del core | Control total. Menos riesgo de regression al evolucionar el core. | Duplica `:User` storage, duplica password hashing, queda fuera del audit log del core |
| **C. Híbrido: core autentica, enterprise autoriza** | El core hace AuthN (verificar password contra `:User`); el enterprise hace AuthZ (LBAC/RBAC sobre las operaciones) | Separación limpia de responsabilidades. Aprovecha el catálogo del core. | Requiere wiring fino entre HELLO (core) y todas las operaciones siguientes (enterprise) |

### 5.3 Acción de sesión

1. Leer en profundidad el `auth/` del core 0.5.0 — interfaces, traits, extension points existentes.
2. Mapear las operaciones LBAC enterprise a puntos de extensión del core.
3. Producir documento de decisión en `.private/auth-architecture-decision-2026-XX-XX.md`.
4. **No tocar código.** Sólo decidir.

### 5.4 Esfuerzo estimado

3–5 h.

## 6. Fase 4 — Sesión 5: Destino de `tessera-graph-tenant` y `tessera-graph-audit`

### 6.1 `tessera-graph-tenant`

**Hipótesis:** código muerto post-sync. El core 0.5.0 ya hace multi-database.

**Verificación necesaria:**
1. ¿Algún call site del enterprise consume `TenantRegistry`?
2. ¿La feature de tenancy multi-nivel (server → tenant → N databases) es un requisito comercial diferenciador, o era una respuesta al hecho de que el core no soportaba multi-database?

**Resoluciones posibles:**
- **Eliminar el crate** si todos los consumers se pueden migrar al `DatabaseRegistry` del core.
- **Refactorizar como wrapper de namespacing** si tenancy multi-nivel es valor de producto: `TenantRegistry` se convierte en un thin layer que namespacing-prefija nombres de database por tenant antes de delegar al `DatabaseRegistry` del core.

### 6.2 `tessera-graph-audit`

**Problema:** el core 0.5.0 audita con `event_type` top-level (`database_created`, `grant_changed`, etc.) en su propio NDJSON. El enterprise tiene su NDJSON paralelo.

**Resoluciones posibles:**
- **Eliminar** y plug-in al audit del core vía algún extension point (verificar si existe).
- **Conservar como overlay** si hay eventos enterprise que el core no captura (e.g. `lbac_clearance_violation`, `rbac_role_assigned`).

### 6.3 Esfuerzo estimado

2–4 h investigación + decisión documentada en `.private/`.

## 7. Fase 5 — Sesión 6: On-disk migration

### 7.1 Problema

El core 0.5.0 rechaza arrancar contra layout v1 (`{data-dir}/data/`). Requiere layout v2 (`{data-dir}/databases/<name>/`) y archivo `.tessera-version` con `disk_layout: 2`.

### 7.2 Casos a cubrir

- **Entorno dev del usuario:** posiblemente seguro `rm -rf` y empezar limpio.
- **Datos del cliente exportados** (los 564 MB ahora en `nodes.csv`, etc., ignorados por `.gitignore`): re-importar tras la migración usando el CLI 0.5.0.
- **Docker Compose:** actualizar volúmenes y verificar que el entrypoint ejecuta `tessera-graph-cli migrate` antes de arrancar el servidor.
- **CI:** asegurar que los integration tests E2E crean datos en layout v2 fresh.

### 7.3 Pasos ejecutables

1. Documentar workflow de migración en `docs/` (existente o nuevo).
2. Adaptar `Dockerfile` y `docker-compose.yml` si es necesario.
3. Actualizar fixtures de integration tests que asuman layout v1.

### 7.4 Esfuerzo estimado

2–4 h.

## 8. Fase 6 — Sesión 7: Integrar `SharedNeighborCache` con `AdjacencyIndex`

### 8.1 Problema detectado en auditoría de sesión 1

El `AdjacencyIndex` está integrado en `NeighborCache` (single-threaded, RefCell) pero **no** en `SharedNeighborCache` (thread-safe, RwLock). El servidor Bolt enterprise usa `SharedNeighborCache`, por lo que en producción los misses del `AdjCache` del core siguen cayendo al O(N) page scan que el índice estaba diseñado para evitar.

### 8.2 Pasos ejecutables (plan TDD)

1. **RED:** test que demuestre que para grafo > 65K nodos, un BFS contra `SharedNeighborCache` mide miss rates (esto requiere instrumentación o un harness con un counter de page-scan).
2. **GREEN:** añadir `adj_index: RwLock<AdjacencyIndex>` al `SharedNeighborCache`. Replicar el patrón de pre-warm en `outgoing_neighbor_ids` y mutation hooks.
3. **REFACTOR:** consolidar la lógica común entre `NeighborCache` y `SharedNeighborCache` si emerge duplicación significativa.
4. **Benchmark de validación:** rerun de la batería contra 1M nodos. Memoria del proyecto registra 26 min CPU + 8 GB RAM sin índice — meta es <2 min CPU + RAM lineal.

### 8.3 Criterio de éxito

- Tests de integración cubren miss path con índice poblado.
- Benchmark 1M nodos completa en tiempo razonable (<2 min CPU).
- No regression en throughput de mutación (`add_edge` con índice maintenance).

### 8.4 Esfuerzo estimado

4–6 h.

## 9. Fase 7 — Sesión 8: Tests E2E + benchmarks + cierre

### 9.1 Cobertura

1. Suite completa enterprise + comm core.
2. Benchmark vs Memgraph (insert + BFS + shortest path).
3. Test E2E con neo4j-driver Python contra el servidor enterprise:
   - HELLO auth-only
   - `session(database="test")` → RUN bindea
   - `session(database="test2")` → rebind funcional
4. Test de carga: `tessera-bench --target tessera-bolt --scenario import` con dataset realista.

### 9.2 Documentación a actualizar

- `CHANGELOG.md` enterprise — entrada para esta release post-sync.
- `docs/multi-database.md` (si existe) — actualizar contra el modelo final adoptado.
- Memoria del proyecto en `.claude/projects/.../memory/` — refrescar `project_overview.md`, `project_neighbor_cache_status.md`, eliminar memorias obsoletas.

### 9.3 Esfuerzo estimado

3–5 h.

## 10. Riesgos transversales

### 10.1 Licencia BSL-1.1 del MIT core

- BSL-1.1 típicamente prohíbe ofrecer el software como servicio comercial durante un periodo de gracia (típicamente 4 años) antes de convertirse en licencia abierta.
- El crate enterprise hereda esa restricción si depende vía path.
- **Acción:** revisar términos exactos del BSL-1.1 elegido (puede ser variante con additional grants).

### 10.2 Regressions silenciosas en LBAC

- El refactor de auth puede degradar enforcement de Bell-LaPadula sin que los tests existentes lo detecten (los tests verifican "deny", pero rara vez verifican "deny por la razón correcta").
- **Acción:** en fase 3 + fase 6, añadir tests específicos que distingan entre "rejected por LBAC compartment" vs "rejected por core grant".

### 10.3 Drift de datos durante la migración

- Si hay datos productivos del cliente en layout v1, la migración v1 → v2 debe ser idempotente y reversible.
- El `migrate` command del core dice ser idempotente (re-run = no-op).
- **Acción:** en fase 5, verificar con backup + restore que la migración es segura. Probar `--dry-run` antes de aplicar.

### 10.4 Branches en paralelo

- La rama actual `feature/resilience-streaming-quality` está 2 commits por delante de origin.
- Si el sync se trabaja en `feature/mit-core-0.5.0-sync` desde `develop`, hay riesgo de merge conflict cuando se rebase.
- **Acción:** mergear primero `feature/resilience-streaming-quality` a `develop` (o cherry-pick los commits relevantes) antes de empezar fase 1.

## 11. Métricas de éxito globales

| Métrica | Antes del sync | Objetivo post-sync |
|---|---|---|
| `cargo check --workspace` | ❌ Falla en resolver feature `extended-gql` | ✅ 0 errores, 0 warnings |
| `cargo test --workspace` | ❌ No compila | ✅ Todos los tests pasan |
| Insert throughput vía Bolt | 466 ops/s (memoria) | ≥1.600 ops/s (paridad Memgraph) — depende de fase 6 + bolt pipelining |
| BFS contra 1M nodos | 26 min CPU, 8 GB RAM | <2 min CPU, RAM lineal |
| Neo4j drivers oficiales | ❌ No compatibles con HELLO viejo | ✅ Compatibilidad `session(database=...)` |
| Crates enterprise duplicados | 4 (`config`, `cypher`, `cli`, `server` parcial) | 0 duplicados; `server` refactorizado como overlay |
| Crates enterprise código muerto | Pendiente confirmar (`tenant`, `audit`) | Decidido + ejecutado |

## 12. Apéndice — Decisiones diferidas

Decisiones que aparecerán durante la ejecución y **deben** documentarse antes de aplicarse:

1. ¿Se conserva tenancy multi-nivel como diferenciador enterprise, o el multi-database del core es suficiente?
2. ¿Cómo se enchufa LBAC al sistema de grants del core?
3. ¿`tessera-graph-audit` se elimina, se mantiene como overlay, o se reescribe como plugin del core?
4. ¿`tessera-graph-replication` (stub) se elimina o se reactiva sobre el modelo de multi-database del core?
5. ¿El `BoltConnectionHandler` enterprise sigue siendo un fork del handler del core, o se refactoriza como interceptor chain?

Cada una de estas decisiones tendrá su propio archivo `.private/decision-XXX-YYYY-MM-DD.md`.
