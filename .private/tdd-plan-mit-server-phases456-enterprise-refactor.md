# TDD Plan: MIT Server Fases 4-6 + Enterprise Refactor

## Contexto

El MIT server tiene handler, auth, y graph_accessor implementados y testeados (25 tests).
Falta: listener, config, startup, main. Una vez completo, el enterprise server debe usar
la infraestructura MIT (listener, config) sin duplicar código.

**Repos**:
- MIT: `/Volumes/WD_BLACK/repos/MojoBytes/tessera-ecosystem/tessera-graph/crates/tessera-graph-server/`
- Enterprise: `/Volumes/WD_BLACK/repos/MojoBytes/tessera-ecosystem/tessera-graph-enterprise/crates/tessera-graph-server/`

## Decisión Arquitectural Clave

El enterprise `BoltConnectionHandler<S>` es fundamentalmente distinto del MIT `BoltHandler<S,A,G>`:
- Enterprise maneja `ServerContext` (RBAC, LBAC, audit, sessions, tenancy, rate-limiting, batch state, paginated PULL)
- MIT usa generics `AuthProvider` + `GraphAccessor`
- Los traits MIT NO cubren lo que enterprise necesita (sessions, clearances, audit, etc.)

**Estrategia**: Compartir **listener** y **config parsing**. El handler enterprise se mantiene independiente.
El listener MIT expone `serve_with<F>` genérico; enterprise lo llama con su propio closure.

---

## Fase 4: `config.rs` — ServerConfig

### Ciclo 4.1: ServerConfig con defaults

- **RED** — Test `config_test::server_config_default_has_expected_values`
  - Assert: `bind_addr == "127.0.0.1:7687"`, `tls_cert/key == None`, `password == None`, `data_dir == None`, `max_connections == 256`, `idle_timeout_secs == 300`
- **GREEN** — `src/config.rs`: struct `ServerConfig` + `Default` impl
- **REFACTOR** — N/A

### Ciclo 4.2: ServerConfig::from_map (testable sin env vars)

- **RED** — Test `config_test::from_map_parses_all_fields`
  - Insert `TESSERA_BIND`, `TESSERA_PASSWORD`, `TESSERA_TLS_CERT`, `TESSERA_TLS_KEY`, `TESSERA_DATA_DIR`, `TESSERA_MAX_CONNECTIONS`, `TESSERA_IDLE_TIMEOUT` en HashMap
  - Assert: cada campo se parsea correctamente
- **RED** — Test `config_test::from_map_uses_defaults_for_missing_keys`
  - HashMap vacío → defaults
- **GREEN** — `ServerConfig::from_map(&HashMap<String,String>)` + `ServerConfig::from_env()` que delega a `from_map`
- **REFACTOR** — N/A

---

## Fase 5: `listener.rs` — TesseraListener

### Ciclo 5.1: bind + local_addr

- **RED** — Test `listener_test::listener_binds_to_ephemeral_port`
  - `TesseraListener::bind("127.0.0.1:0")` → `local_addr().port() > 0`
- **GREEN** — `src/listener.rs`: struct `TesseraListener` con `inner: TcpListener`, métodos `bind`, `local_addr`
- **REFACTOR** — N/A

### Ciclo 5.2: serve_with — accept loop genérico

El punto de extensión clave. `serve_with` acepta un closure que maneja cada `TcpStream`.

- **RED** — Test `listener_test::serve_with_accepts_connection` (feature `plain-tcp`)
  - Spawn `listener.serve_with(handler_fn, shutdown_rx, 10)` donde `handler_fn` es un closure que solo dropea el stream
  - Conectar con `TcpStream::connect(addr)` → no error
  - Enviar shutdown → serve_with retorna
- **GREEN** — `pub async fn serve_with<F, Fut>(self, handler: F, shutdown: Receiver<bool>, max_connections: usize) -> Result<()>` donde `F: Fn(TcpStream) -> Fut + Send + Sync + 'static, Fut: Future<Output = ()> + Send + 'static`
  - `Arc<Semaphore>` para max_connections
  - `tokio::select!` en shutdown + accept
  - Spawn task por conexión con semaphore permit
- **REFACTOR** — N/A

### Ciclo 5.3: Semaphore limita conexiones

- **RED** — Test `listener_test::serve_with_respects_max_connections`
  - `max_connections = 1`
  - Conectar c1 (handshake Bolt para ocupar el permit)
  - Conectar c2 → server acepta TCP pero no puede adquirir permit → cierra stream
  - Assert: c2 recibe EOF o timeout rápido
- **GREEN** — Ya implementado en serve_with: `semaphore.try_acquire_owned()` falla → drop stream
- **REFACTOR** — N/A

### Ciclo 5.4: Shutdown graceful

- **RED** — Test `listener_test::serve_with_stops_on_shutdown`
  - Spawn serve_with → enviar shutdown → `timeout(5s, handle)` retorna Ok
- **GREEN** — Branch shutdown en `tokio::select!` retorna `Ok(())`
- **REFACTOR** — N/A

### Ciclo 5.5: serve_plain — wrapper para community (feature-gated)

- **RED** — Test `listener_test::serve_plain_handles_bolt_hello`
  - Spawn `listener.serve_plain(auth, graph, shutdown_rx, max, idle_timeout)` 
  - Conectar → Bolt handshake → HELLO → SUCCESS
  - GOODBYE → shutdown
- **GREEN** — `#[cfg(feature = "plain-tcp")] pub async fn serve_plain<A, G>(self, auth: Arc<A>, graph: Arc<G>, shutdown: Receiver<bool>, max: usize, idle: Duration) -> Result<()>` que llama a `serve_with` con closure que construye `BoltHandler`
- **REFACTOR** — N/A

### Ciclo 5.6: serve_tls — producción

- **RED** — Test `listener_test::serve_tls_rejects_plain_client`
  - Generar self-signed cert (rcgen)
  - Spawn `serve_tls` → conectar con plain TCP (no TLS) → enviar basura → EOF/error
- **GREEN** — `pub async fn serve_tls<A, G>(self, auth: Arc<A>, graph: Arc<G>, tls: TlsConfig, shutdown: Receiver<bool>, max: usize, idle: Duration) -> Result<()>` que llama a `serve_with` con closure que hace TLS accept + `BoltHandler`
- **REFACTOR** — Añadir `rcgen` a dev-deps. Añadir `test_tls_config()` a `tests/common/mod.rs`

---

## Fase 6: `startup.rs` + `main.rs`

### Ciclo 6.1: start_server con plain-tcp

- **RED** — Test `startup_test::start_server_binds_and_shuts_down`
  - Config con `bind_addr = "127.0.0.1:0"`, sin TLS, sin password
  - `start_server(config, shutdown_rx)` → spawn → shutdown → retorna Ok en <5s
- **GREEN** — `src/startup.rs`: `pub async fn start_server(config: ServerConfig, shutdown: Receiver<bool>) -> Result<()>`
  - Selecciona NoAuth/PasswordAuth según `config.password`
  - Crea `DefaultGraphAccessor` con graph in-memory o `data_dir`
  - Si no hay TLS certs → `serve_plain` (solo con feature plain-tcp, error sin feature)
  - Si hay TLS certs → `serve_tls`
- **REFACTOR** — N/A

### Ciclo 6.2: start_server con password requiere auth

- **RED** — Test `startup_test::start_server_with_password_rejects_bad_auth`
  - Config con password = "test-pw", plain-tcp
  - Conectar → HELLO con credentials incorrectas → FAILURE
- **GREEN** — Ya cubierto por la selección de auth provider en start_server
- **REFACTOR** — N/A

### Ciclo 6.3: main.rs funcional

- **GREEN** — Reemplazar stub:
  ```rust
  #[tokio::main]
  async fn main() {
      tracing_subscriber::fmt::init();
      let config = ServerConfig::from_env();
      let (shutdown_tx, shutdown_rx) = watch::channel(false);
      tokio::spawn(async move { /* signal handler */ });
      if let Err(e) = start_server(config, shutdown_rx).await { ... }
  }
  ```
- **REFACTOR** — N/A (verificación manual: `cargo build --release --bin tessera-graph-server`)

### Ciclo 6.4: Actualizar lib.rs exports

- **GREEN** — Añadir `pub mod config; pub mod listener; pub mod startup;` + re-exports en `lib.rs`
- **REFACTOR** — N/A

---

## Fase 7: Enterprise Refactor — Depend on MIT Server

### Ciclo 7.1: Añadir MIT server como dependencia

- **RED** — `cargo check` en enterprise
- **GREEN** — Enterprise `Cargo.toml`:
  ```toml
  [dependencies]
  tessera-server-core = { path = "../../../tessera-graph/crates/tessera-graph-server", package = "tessera-graph-server" }
  ```
- **REFACTOR** — Verificar `cargo tree` MIT no tiene deps enterprise

### Ciclo 7.2: Enterprise listener usa MIT TesseraListener

- **RED** — Tests enterprise de listener deben seguir pasando
- **GREEN** — Reemplazar enterprise `listener.rs`:
  - Re-exportar `pub use tessera_server_core::TesseraListener;`
  - Añadir `serve_enterprise(listener, ctx, shutdown, max, idle, default_tenant) -> Result<()>`
    que llama `listener.serve_with(...)` con closure enterprise que construye `BoltConnectionHandler`
  - Añadir `serve_enterprise_tls(...)` que hace TLS wrap + mismo patrón
- **REFACTOR** — Eliminar código duplicado de accept loop del enterprise

### Ciclo 7.3: Conversión de errores

- **RED** — Compile check
- **GREEN** — `impl From<tessera_server_core::ServerError> for ServerError` en enterprise `error.rs`
- **REFACTOR** — N/A

### Ciclo 7.4: Enterprise config comparte helpers MIT

- **RED** — Compile check
- **GREEN** — Enterprise `config.rs` importa `parse_env_or_warn` de MIT si existe, o mantiene su propia impl (evitar dependencia forzada si la API MIT no encaja perfectamente)
- **REFACTOR** — Evaluar si vale la pena compartir o duplicar (si es <10 LOC, duplicar es mejor)

### Ciclo 7.5: Tests enterprise completos

- **RED** — `cargo test -p tessera-graph-server` en enterprise workspace
- **GREEN** — Actualizar imports en `tests/common/mod.rs` y `tests/listener_test.rs` si el tipo `TesseraListener` ahora viene de MIT
- **REFACTOR** — Clean up imports redundantes

---

## Fase 8: Wiring & Verificación

### Ciclo 8.1: MIT no tiene deps enterprise

```bash
cargo tree -p tessera-graph-server  # MIT workspace
# MUST NOT contain: tessera-graph-auth, tessera-graph-audit, tessera-graph-tenant, tessera-graph-monitor
```

### Ciclo 8.2: Enterprise importa MIT

```bash
cargo tree -p tessera-graph-server  # Enterprise workspace
# MUST contain: tessera-graph-server (MIT) como dependencia
```

### Ciclo 8.3: E2E por TCP listener MIT

- **RED** — Test `e2e_test::bolt_roundtrip_through_listener`
  - Spawn `serve_plain` → `TcpStream::connect` → Bolt handshake → HELLO → RUN "CREATE (:T {x:1})" → PULL → RUN "MATCH (n:T) RETURN n.x" → PULL (expect RECORD) → GOODBYE
- **GREEN** — Si fases 4-6 están correctas, pasa directamente
- **REFACTOR** — N/A

### Ciclo 8.4: Binario MIT compila

```bash
cargo build --release --bin tessera-graph-server  # MIT workspace
```

### Ciclo 8.5: Clippy limpio ambos repos

```bash
cargo clippy -p tessera-graph-server --all-targets -- -D warnings  # MIT
cargo clippy -p tessera-graph-server --all-targets -- -D warnings  # Enterprise
```

### Wiring Checklist

- [ ] `ServerConfig` — tiene call site en `startup.rs` y `main.rs`
- [ ] `ServerConfig::from_env()` — llamado en `main.rs`
- [ ] `TesseraListener::bind` — llamado en `startup.rs`
- [ ] `TesseraListener::serve_with` — llamado por `serve_plain`, `serve_tls`, y enterprise `serve_enterprise`
- [ ] `TesseraListener::serve_plain` — llamado en `startup.rs` (con feature plain-tcp)
- [ ] `TesseraListener::serve_tls` — llamado en `startup.rs`
- [ ] `start_server` — llamado en `main.rs`
- [ ] Enterprise `serve_enterprise` / `serve_enterprise_tls` — llamado en enterprise `startup.rs`
- [ ] `From<tessera_server_core::ServerError>` — usado en conversiones enterprise
- [ ] Background tasks (signal handler) — spawned en `main.rs`
- [ ] No queda código dead/unreachable
- [ ] Enterprise `BoltConnectionHandler` no fue tocado (solo listener refactorizado)

---

## Estimación: ~6 horas

- Fase 4 (config): ~30 min
- Fase 5 (listener): ~2h (serve_with es lo más complejo)
- Fase 6 (startup+main): ~1h
- Fase 7 (enterprise refactor): ~1.5h
- Fase 8 (wiring): ~1h

## Orden de Ejecución

1. Fase 4 → 2. Fase 5 → 3. Fase 6 → 4. Fase 7 → 5. Fase 8
