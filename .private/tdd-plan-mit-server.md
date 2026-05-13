# TDD Plan: Servidor Bolt Básico en MIT Core

## Contexto

El MIT core no es usable standalone — no tiene servidor. Memgraph da servidor + CLI gratis en community. Necesitamos un servidor Bolt mínimo en el MIT core para que sea un producto funcional. El enterprise server se refactorizará para extender/wrappear este servidor.

**Stack**: Rust / Tokio, Bolt 4.4
**Repo**: `/Volumes/WD_BLACK/repos/MojoBytes/tessera-ecosystem/tessera-graph/`
**Rama**: `feature/community-edition`

## Estructura del crate

```
crates/tessera-graph-server/
├── Cargo.toml
├── src/
│   ├── lib.rs            — re-exports
│   ├── error.rs          — ServerError
│   ├── auth.rs           — AuthProvider trait + NoAuth/PasswordAuth
│   ├── graph_accessor.rs — GraphAccessor trait + DefaultGraphAccessor
│   ├── handler.rs        — BoltHandler<S, A, G> state machine
│   ├── util.rs           — PendingResult, gql_result_to_packstream helpers
│   ├── listener.rs       — TesseraListener (TCP + TLS accept loop)
│   ├── config.rs         — ServerConfig from env vars
│   ├── startup.rs        — start_server() orchestration
│   └── main.rs           — binary entry point + signal handling
└── tests/
    ├── handler_test.rs   — unit tests over DuplexStream
    └── e2e_test.rs       — TCP listener + BoltClient roundtrip
```

## Diseño clave: Extension points para Enterprise

```rust
// Enterprise implementa estos traits con RBAC/LBAC/audit
pub trait AuthProvider: Send + Sync + 'static {
    fn authenticate(&self, principal: &str, credentials: &str) -> bool;
}

pub trait GraphAccessor: Send + Sync + 'static {
    fn execute_query(&self, stmt: &GqlQuery) -> Result<GqlResult>;
    fn execute_mutation(&self, stmt: &MutationStatement) -> Result<GqlMutationResult>;
}
```

## Plan de Ejecución

### Fase 1: Scaffolding

1. [ ] Crear `Cargo.toml` — deps: tokio, tokio-rustls, thiserror, tracing, tessera-graph, tessera-graph-protocol, tessera-graph-config, tessera-graph-cypher. Feature `plain-tcp` para tests sin TLS.
2. [ ] Registrar en workspace `Cargo.toml`
3. [ ] Crear `src/lib.rs` + `src/error.rs` — `ServerError` con thiserror

### Fase 2: Extension traits (TDD)

4. [ ] RED — tests `NoAuthProvider` acepta todo, `PasswordAuthProvider` rechaza credentials incorrectas
5. [ ] GREEN — `src/auth.rs` con trait `AuthProvider` + 2 implementaciones
6. [ ] RED — tests `DefaultGraphAccessor` ejecuta query y mutation sobre `Arc<RwLock<Graph>>`
7. [ ] GREEN — `src/graph_accessor.rs` con trait `GraphAccessor` + `DefaultGraphAccessor`

### Fase 3: Handler state machine (TDD)

8. [ ] RED — tests del handler sobre DuplexStream:
   - HELLO sin auth → SUCCESS con `server` + `connection_id`
   - HELLO con password incorrecto → FAILURE
   - RUN antes de HELLO → FAILURE
   - RUN "MATCH (n) RETURN n" → SUCCESS con fields
   - PULL → RECORDs + SUCCESS
   - RESET tras fallo → SUCCESS, estado limpio
   - GOODBYE → cierra conexión
9. [ ] GREEN — `src/handler.rs` — `BoltHandler<S, A, G>` genérico:
   - `new_with_handshake` — handshake + split stream
   - `run()` — select loop: shutdown / timeout / read_message
   - `dispatch()` — decode → route a handle_hello/run/pull/reset/goodbye
   - No deps enterprise
10. [ ] REFACTOR — extraer `PendingResult`, helpers de conversión a `src/util.rs`

### Fase 4: TCP Listener (TDD)

11. [ ] RED — test E2E: spawn listener plain TCP, conectar BoltClient, HELLO→RUN→PULL→GOODBYE
12. [ ] GREEN — `src/listener.rs` — `TesseraListener`:
    - `bind(addr)` → `TcpListener`
    - `serve()` (plain TCP, feature-gated) + `serve_tls()` (producción)
    - Semaphore para max_connections
    - JoinSet para tareas de conexión

### Fase 5: Config + Binary

13. [ ] RED — tests de `ServerConfig::from_env()` defaults y overrides
14. [ ] GREEN — `src/config.rs` — parseo de env vars (TESSERA_BIND, TESSERA_TLS_CERT/KEY, TESSERA_PASSWORD, TESSERA_DATA_DIR, etc.)
15. [ ] GREEN — `src/startup.rs` — `start_server(config, shutdown)`:
    - Abre Graph desde data_dir o in-memory
    - Selecciona NoAuth o PasswordAuth según config
    - Construye TLS, bind, serve_tls
16. [ ] GREEN — `src/main.rs` — `#[tokio::main]`, tracing, señales SIGTERM/Ctrl-C

### Fase 6: Wiring Verification

17. [ ] `cargo test --workspace` sin fallos
18. [ ] `cargo clippy --all-targets -- -D warnings` limpio
19. [ ] `cargo build --release --bin tessera-graph-server` produce binario
20. [ ] `cargo tree -p tessera-graph-server` NO contiene crates enterprise (auth, audit, tenant, monitor)
21. [ ] Verificar: `AuthProvider` y `GraphAccessor` son `pub trait` — enterprise puede implementarlos

## Estimación: ~5.5 horas

## Criterios de Éxito

- [ ] Handler NO importa ningún crate enterprise
- [ ] `BoltHandler` genérico sobre `AuthProvider` + `GraphAccessor`
- [ ] Tests pasan sobre DuplexStream (sin TCP real para handler tests)
- [ ] E2E test con TCP real + BoltClient
- [ ] Binary compila y arranca
- [ ] Clippy limpio, 0 warnings
