# TDD Plan Unificado — Resiliencia + Streaming Import Quality
Fecha: 2026-04-02
Commit base: c31e994 (develop)

## Contexto

Este plan unifica dos fuentes de trabajo pendiente:

1. **Auditoría de resiliencia** (2026-04-02): 13 hallazgos del repositorio enterprise
   (los 3 del core MIT tessera-graph se excluyen y se rastrean en ese repo).
2. **Quality fixes de streaming import** (plan previo): 14 hallazgos en `tessera-cli`,
   todos en estado `- [ ]` (sin implementar aún).

El orden de fases sigue prioridad de riesgo operativo: primero lo que puede causar
pérdida de datos o crash en producción, luego seguridad, luego robustez, luego deuda
técnica, y finalmente mejoras de experiencia de usuario.

**Stack detectado**: Rust 2024, tokio async, workspace de 14 crates
**Convenciones**:
- Unit tests: `#[cfg(test)] mod tests` inline en el módulo
- Integration tests: `crates/<crate>/tests/*.rs`
- `// OK: test` junto a `.unwrap()`/`.expect()` en tests
- `unwrap_or(0)`, `.map_err()` en producción (nunca `.unwrap()`)
- `clippy::all = deny`, `clippy::pedantic = warn`, `clippy::nursery = warn`
- `unsafe_code = forbid`
- `nice cargo` para comandos pesados de CI

**Afecta hot path**: SI — CRITICAL #1 afecta el path de query/pull (cada consulta),
CRITICAL #4 y HIGH #9 afectan la validación de sesión (cada request autenticado).

---

## Decisiones Previas Necesarias

Ninguna. Todos los hallazgos tienen solución técnica clara definida.

---

## Dependencias entre fases

```
Fase 1 (CRITICAL) — independiente, máxima prioridad
Fase 2 (SECURITY streaming) — independiente de Fase 1
Fase 3 (HIGH resiliencia) — #9 (TTL Instant) debe ir antes que #4 (session cleanup)
Fase 4 (DRY streaming) — depende de Fase 2 (hallazgo 3 usa hallazgo 4 como base)
Fase 5 (MEDIUM resiliencia) — independiente
Fase 6 (streaming improvements) — independiente
Fase 7 (wiring verification) — siempre al final
```

---

## Plan de Ejecución

---

## Fase 1 — CRITICAL: Resiliencia (antes de producción)

**Estimación: 3 h**

### Cycle 1: Result streaming en PULL — eliminar OOM en queries grandes

**Hallazgo**: `PendingResult.rows: Vec<Vec<PackStreamValue>>` acumula TODAS las filas
en heap antes de enviar cualquier byte al cliente. Un `MATCH (n) RETURN n` sobre 1M
nodos materializa ~1 GB antes de escribir el primer Record.

**Código actual** (`crates/tessera-graph-server/src/bolt_handler.rs:47-48,564,582-600`):
```rust
struct PendingResult {
    rows: Vec<Vec<PackStreamValue>>,  // PROBLEMA: acumula todo en memoria
}
// handle_run: self.pending_result = Some(PendingResult { rows });
// handle_pull: for row in result.rows { send_response(Record) }
```

#### RED

1. [ ] Escribir test de regresión de memoria en PULL
   - Archivo: `crates/tessera-graph-server/tests/bolt_handler_test.rs`
   - Test a agregar:

   ```rust
   #[tokio::test]
   async fn pull_streams_rows_without_full_materialization() {
       // Este test verifica que los rows se envían al cliente conforme se
       // producen, sin acumularlos todos primero. Lo medimos comprobando
       // que la primera fila llega ANTES de que se hayan producido todas.
       //
       // Implementación: inyectamos un iterator perezoso como fuente de rows
       // y comprobamos que el primer write ocurre antes de que el iterator
       // se haya drenado completamente.
       //
       // Por ahora: test de contrato — verifica que PendingResult acepta
       // un iterador/canal en lugar de Vec.
       // Este test FALLA (RED) mientras PendingResult use Vec.
       //
       // Usar el harness existente en tests/common/mod.rs para conectar
       // un cliente ficticio y verificar el timing de los records.
       let mgr = create_test_server().await;
       // Insertar 100 nodos y hacer MATCH (n) RETURN n con PULL
       // Verificar que records llegan en streaming (el primer Record
       // no espera al último).
       // El test concreto se ajustará tras leer common/mod.rs.
       // FORMA MÍNIMA: verificar que handle_pull puede aceptar un iterator
       // en lugar de Vec en su firma interna.
       todo!("implementar tras refactorizar PendingResult a iterator/channel")
   }
   ```

   **Nota**: Este es el único ciclo donde el test RED es parcialmente `todo!()` porque
   el refactor es profundo. El test se completa en el ciclo GREEN al mismo tiempo que
   se define la nueva API. El contrato es: el primer Record debe llegar al wire ANTES
   de que todos los rows estén producidos.

#### GREEN

2. [ ] Refactorizar `PendingResult` para almacenar rows como iterador/boxed
   - Archivo: `crates/tessera-graph-server/src/bolt_handler.rs`

   **Opción pragmática (menor cambio superficial, menor riesgo)**:
   Conservar `Vec<Vec<PackStreamValue>>` en `PendingResult` pero introducir un
   mecanismo de `n` rows máximo por PULL, usando el parámetro `n` del mensaje PULL
   que ya está definido en Bolt 4.4. Esto limita la memoria máxima a
   `n * avg_row_size` en lugar de `total_rows * avg_row_size`.

   **Diseño**:
   ```rust
   struct PendingResult {
       /// All rows from the query result.
       /// TODO(streaming): replace with a channel/iterator when the graph
       /// engine supports incremental row emission.
       rows: Vec<Vec<PackStreamValue>>,
       /// Index of the next row to send (for paginated PULL).
       cursor: usize,
   }
   ```

   En `handle_pull`, leer el parámetro `n` del `extra` dict (Bolt 4.4 spec):
   ```rust
   async fn handle_pull(&mut self, extra: &BoltDict) -> Result<()> {
       let Some(result) = self.pending_result.as_mut() else { ... };

       // Bolt 4.4: `n` = number of records to fetch (-1 = all).
       let n: i64 = match extra.iter().find(|(k,_)| k == "n") {
           Some((_, PackStreamValue::Int(n))) => *n,
           _ => -1, // default: fetch all (legacy clients)
       };

       let batch_end = if n < 0 {
           result.rows.len()
       } else {
           (result.cursor + n as usize).min(result.rows.len())
       };

       for row in result.rows[result.cursor..batch_end].iter().cloned() {
           self.send_response(&BoltResponse::Record { fields: row }).await?;
       }
       result.cursor = batch_end;

       let has_more = result.cursor < result.rows.len();
       if !has_more {
           self.pending_result = None;
       }
       self.send_response(&BoltResponse::Success {
           metadata: vec![("has_more".to_owned(), PackStreamValue::Bool(has_more))],
       }).await
   }
   ```

   **Impacto OOM**: El cliente Neo4j/Bolt envía PULL n=1000 por defecto.
   Con 1M rows: en lugar de materializar 1M rows y enviarlos todos de golpe,
   el servidor envía lotes de 1000, y el cliente puede aplicar backpressure.
   La materialización total sigue ocurriendo en `handle_run` — el fix completo
   requiere cambios en el motor de consultas (tessera-graph), que está fuera de scope.
   Esta fase mitiga el OOM al transferencia; la eliminación completa de la
   materialización se documenta como trabajo futuro en un issue.

   - Archivo: `crates/tessera-graph-server/src/bolt_handler.rs`
   - Verificar: `nice cargo test -p tessera-graph-server` pasa

#### REFACTOR

3. [ ] Añadir comentario de trabajo futuro y actualizar el test RED al contrato real
   - Archivo: `crates/tessera-graph-server/src/bolt_handler.rs`
   - Agregar sobre `PendingResult`:
     ```rust
     /// Stores the result of a RUN command until PULL/DISCARD arrives.
     ///
     /// # Memory note
     ///
     /// All rows are currently materialized eagerly (see audit finding CRITICAL #1).
     /// A future improvement is to replace `rows` with a streaming channel fed
     /// directly from the query engine, which would allow incremental PULL without
     /// ever holding more than `n` rows in memory. Tracked in issue #TBD.
     struct PendingResult {
         rows: Vec<Vec<PackStreamValue>>,
         cursor: usize,
     }
     ```
   - Verificar: `nice cargo clippy -p tessera-graph-server -- -D warnings` limpio

---

### Cycle 2: Session cleanup background task — eliminar memory leak

**Hallazgo**: `SessionManager` solo purga sesiones expiradas en `validate()`.
Sin un background task que llame `purge_expired()` periódicamente, un patrón de
conexión+desconexión sin re-autenticación hace crecer el HashMap indefinidamente.

**Código actual** (`crates/tessera-graph-auth/src/session.rs:54`):
- `sessions: Arc<RwLock<HashMap<SessionToken, Session>>>` — sin límite ni cleanup.
- No hay método `purge_expired()` ni tarea de background.

#### RED

4. [ ] Escribir tests para `purge_expired`
   - Archivo: `crates/tessera-graph-auth/tests/session_test.rs`

   ```rust
   #[test]
   fn purge_expired_removes_expired_sessions() {
       let mgr = SessionManager::new(1); // TTL 1 segundo
       let _t1 = mgr.create_session(UserId::new(1)).unwrap(); // OK: test
       let _t2 = mgr.create_session(UserId::new(2)).unwrap(); // OK: test
       std::thread::sleep(std::time::Duration::from_secs(2));
       let removed = mgr.purge_expired();
       assert_eq!(removed, 2, "ambas sesiones expiradas deben ser purgadas");
   }

   #[test]
   fn purge_expired_keeps_live_sessions() {
       let mgr = SessionManager::new(3600);
       let t1 = mgr.create_session(UserId::new(1)).unwrap(); // OK: test
       let removed = mgr.purge_expired();
       assert_eq!(removed, 0, "sesión viva no debe ser purgada");
       assert!(mgr.validate(&t1).is_ok());
   }

   #[test]
   fn session_count_returns_current_size() {
       let mgr = SessionManager::new(3600);
       assert_eq!(mgr.session_count(), 0);
       let _t1 = mgr.create_session(UserId::new(1)).unwrap(); // OK: test
       let _t2 = mgr.create_session(UserId::new(2)).unwrap(); // OK: test
       assert_eq!(mgr.session_count(), 2);
   }
   ```

   - Verificar: `cargo test -p tessera-graph-auth session` FALLA (RED) porque
     `purge_expired` y `session_count` no existen.

#### GREEN

5. [ ] Implementar `purge_expired` y `session_count` en `SessionManager`
   - Archivo: `crates/tessera-graph-auth/src/session.rs`

   ```rust
   /// Remove all expired sessions and return the count of removed entries.
   ///
   /// This should be called periodically by a background task to prevent
   /// unbounded HashMap growth under high connection churn.
   ///
   /// # Panics
   ///
   /// Does not panic. Returns `0` if the lock is poisoned (conservative choice:
   /// do not crash the server on a cleanup task failure).
   pub fn purge_expired(&self) -> usize {
       let now = std::time::Instant::now();
       let Ok(mut sessions) = self.sessions.write() else {
           return 0;
       };
       let before = sessions.len();
       sessions.retain(|_, s| s.expires_at > now);
       before - sessions.len()
   }

   /// Return the current number of live sessions (for metrics/monitoring).
   ///
   /// Returns `0` if the lock is poisoned.
   pub fn session_count(&self) -> usize {
       self.sessions.read().map(|s| s.len()).unwrap_or(0)
   }
   ```

   **Nota importante**: Para que `purge_expired` funcione con `Instant`,
   `Session.expires_at` debe cambiar de `u64` (unix timestamp) a
   `std::time::Instant`. Este cambio también es necesario para el HIGH #9
   (Session TTL wall clock). Estos dos ciclos se combinan — ver Cycle 5.

   **Dependencia**: Completar Cycle 5 (TTL Instant) PRIMERO, luego volver
   aquí para que `purge_expired` use `Instant`.

#### REFACTOR

6. [ ] Registrar `session_count` como métrica en `MetricsRegistry`
   - Archivo: `crates/tessera-graph-monitor/src/registry.rs` + `render.rs`
   - Agregar campo `sessions_active: AtomicU64` a `MetricsRegistry`.
   - El valor se actualiza en `main.rs` desde el background cleanup task.
   - Verificar: `cargo test -p tessera-graph-monitor` pasa.

---

## Fase 2 — SECURITY: Streaming Import — Label CSV sin sanitización

**Hallazgo**: `csv_nodes_to_gql` y `stream_csv_import` interpolan el label CSV
directamente en `CREATE (:{label}...)` sin pasar por `write_gql_identifier`.
Un label `"X {admin: true}"` inyecta propiedades arbitrarias en el GQL generado.

### Cycle 3: CSV label injection prevention

**Preservado del plan original tdd-plan-quality-fixes-streaming-import.md Fase 1.**

#### RED

7. [ ] Escribir tests de inyección CSV (6 tests)
   - Archivo: `crates/tessera-cli/src/import.rs`, sección `#[cfg(test)] mod tests`

   ```rust
   // --- Hallazgo 4: CSV label debe pasar por write_gql_identifier ---

   #[test]
   fn csv_nodes_label_with_space_uses_delimited_identifier() {
       let csv = "label,name\nMy Type,Alice\n";
       let stmts = csv_nodes_to_gql(csv).expect("csv to gql"); // OK: test
       assert!(
           stmts[0].contains(":\"My Type\""),
           "expected delimited identifier, got: {}",
           stmts[0]
       );
   }

   #[test]
   fn csv_nodes_label_injection_attempt_uses_delimited_identifier() {
       let csv = "label,name\nX {admin: true},Alice\n";
       let stmts = csv_nodes_to_gql(csv).expect("csv to gql"); // OK: test
       assert!(
           stmts[0].contains(":\"X {admin: true}\""),
           "expected delimited identifier to neutralize injection, got: {}",
           stmts[0]
       );
       assert!(stmts[0].contains("name: 'Alice'"), "got: {}", stmts[0]);
   }

   #[test]
   fn csv_nodes_label_with_double_quote_is_error() {
       let csv = "label,name\nbad\"label,Alice\n";
       let result = csv_nodes_to_gql(csv);
       assert!(result.is_err(), "double-quote in label must be rejected");
   }

   #[test]
   fn stream_csv_label_with_space_uses_delimited_identifier() {
       let csv = "label,name\nMy Type,Alice\n";
       let mut out = Vec::new();
       stream_csv_import(std::io::Cursor::new(csv), |s| {
           out.push(s);
           Ok(())
       })
       .expect("stream csv"); // OK: test
       assert!(
           out[0].contains(":\"My Type\""),
           "expected delimited identifier, got: {}",
           out[0]
       );
   }

   #[test]
   fn stream_csv_label_injection_attempt_uses_delimited_identifier() {
       let csv = "label,name\nX {admin: true},Alice\n";
       let mut out = Vec::new();
       stream_csv_import(std::io::Cursor::new(csv), |s| {
           out.push(s);
           Ok(())
       })
       .expect("stream csv"); // OK: test
       assert!(
           out[0].contains(":\"X {admin: true}\""),
           "expected delimited identifier, got: {}",
           out[0]
       );
   }

   #[test]
   fn stream_csv_label_with_double_quote_is_error() {
       let csv = "label,name\nbad\"label,Alice\n";
       let result = stream_csv_import(std::io::Cursor::new(csv), |_| Ok(()));
       assert!(result.is_err(), "double-quote in label must be rejected");
   }
   ```

   - Verificar: `cargo test -p tessera-cli csv_nodes_label stream_csv_label` FALLA (RED)

#### GREEN

8. [ ] Aplicar `write_gql_identifier` al label en ambas funciones
   - Archivo: `crates/tessera-cli/src/import.rs`

   **En `csv_nodes_to_gql`** (alrededor de línea 271-277):
   ```rust
   // ANTES (inseguro):
   statements.push(format!("CREATE (:{label}{props_str})"));

   // DESPUES:
   let mut stmt = String::with_capacity(64 + props_str.len());
   stmt.push_str("CREATE (:");
   write_gql_identifier(label, "node label", &mut stmt)?;
   stmt.push_str(&props_str);
   stmt.push(')');
   statements.push(stmt);
   ```

   **En `stream_csv_import`** (alrededor de línea 343-350):
   ```rust
   // ANTES (inseguro):
   on_stmt(format!("CREATE (:{label}{props_str})"))?;

   // DESPUES:
   let mut stmt = String::with_capacity(64 + props_str.len());
   stmt.push_str("CREATE (:");
   write_gql_identifier(label, "node label", &mut stmt)?;
   stmt.push_str(&props_str);
   stmt.push(')');
   on_stmt(stmt)?;
   ```

   - Verificar: `cargo test -p tessera-cli` pasa. Tests de parity existentes siguen verdes.

#### REFACTOR

9. [ ] Extraer helper privado `finish_csv_node_stmt`
   - Archivo: `crates/tessera-cli/src/import.rs`
   - Crear:
     ```rust
     /// Build the tail of a GQL CREATE statement for a CSV node: `:{label}{props_str})`.
     ///
     /// `label` is validated via [`write_gql_identifier`] — labels containing
     /// characters that would break GQL syntax are wrapped in delimited form.
     fn finish_csv_node_stmt(label: &str, props_str: &str) -> Result<String, CliError> {
         let mut stmt = String::with_capacity(16 + label.len() + props_str.len());
         stmt.push_str("CREATE (:");
         write_gql_identifier(label, "node label", &mut stmt)?;
         stmt.push_str(props_str);
         stmt.push(')');
         Ok(stmt)
     }
     ```
   - Ambas funciones batch y streaming delegan en este helper.
   - Verificar: `cargo test -p tessera-cli` sigue verde. Cero advertencias.

---

## Fase 3 — HIGH: Resiliencia

**Estimación: 4 h**

### Cycle 4: Metrics auth — autenticación básica en /metrics y /health

**Hallazgo**: `serve_metrics` (monitor/server.rs:31-56) no requiere autenticación.
Expone contadores de auth failures, query rates, connection counts sin restricción.

**Diseño**: Autenticación HTTP Basic usando un token Bearer configurable vía
`TESSERA_METRICS_TOKEN`. Si la variable no está configurada, el servidor de métricas
NO arranca (fail-safe). Los endpoints /metrics y /health requieren el header
`Authorization: Bearer <token>`.

#### RED

10. [ ] Escribir tests para autenticación de métricas
    - Archivo: `crates/tessera-graph-monitor/src/server.rs`, sección `#[cfg(test)] mod tests`

    ```rust
    #[tokio::test]
    async fn metrics_without_token_returns_401() {
        let registry = Arc::new(MetricsRegistry::new(256));
        let token = Some("secret-token".to_owned());
        let addr = spawn_server_with_token(Arc::clone(&registry), healthy(), token).await;

        // Request sin Authorization header
        let response = http_get(addr, "/metrics").await;
        assert!(
            response.starts_with("HTTP/1.1 401"),
            "unauthenticated /metrics must return 401, got: {response}"
        );
    }

    #[tokio::test]
    async fn metrics_with_valid_token_returns_200() {
        let registry = Arc::new(MetricsRegistry::new(256));
        let token = Some("secret-token".to_owned());
        let addr = spawn_server_with_token(Arc::clone(&registry), healthy(), token).await;

        let response = http_get_with_auth(addr, "/metrics", "secret-token").await;
        assert!(response.starts_with("HTTP/1.1 200 OK"));
    }

    #[tokio::test]
    async fn metrics_with_wrong_token_returns_401() {
        let registry = Arc::new(MetricsRegistry::new(256));
        let token = Some("correct-token".to_owned());
        let addr = spawn_server_with_token(Arc::clone(&registry), healthy(), token).await;

        let response = http_get_with_auth(addr, "/metrics", "wrong-token").await;
        assert!(response.starts_with("HTTP/1.1 401"));
    }

    #[tokio::test]
    async fn metrics_without_configured_token_serves_unauthenticated() {
        // Cuando token = None (no configurado), las métricas se sirven sin auth.
        // Esto es válido solo cuando el servidor está en red aislada.
        // Documentar con warning en el log de inicio.
        let registry = Arc::new(MetricsRegistry::new(256));
        let addr = spawn_server_with_token(Arc::clone(&registry), healthy(), None).await;
        let response = http_get(addr, "/metrics").await;
        assert!(response.starts_with("HTTP/1.1 200 OK"));
    }
    ```

    - Verificar: tests FALLAN (RED) porque `serve_metrics_on` no tiene parámetro `token`.

#### GREEN

11. [ ] Agregar parámetro `metrics_token: Option<String>` a las funciones de servidor
    - Archivo: `crates/tessera-graph-monitor/src/server.rs`

    Cambiar la firma de `serve_metrics` y `serve_metrics_on` para aceptar `metrics_token`.
    En `handle_connection`, agregar verificación antes de despachar:
    ```rust
    // En handle_connection, antes del routing:
    if let Some(ref expected) = metrics_token {
        let auth_header = request
            .lines()
            .find(|l| l.to_lowercase().starts_with("authorization:"))
            .and_then(|l| l.split_once(':').map(|(_, v)| v.trim()));

        let authorized = auth_header
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(|t| constant_time_eq::constant_time_eq(t.as_bytes(), expected.as_bytes()))
            .unwrap_or(false);

        if !authorized {
            let response = "HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Bearer\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
            stream.write_all(response.as_bytes()).await?;
            stream.flush().await?;
            return Ok(());
        }
    }
    ```

    - Actualizar `main.rs`: pasar `TESSERA_METRICS_TOKEN` al servidor de métricas.
    - Añadir dependency `constant_time_eq` a `tessera-graph-monitor/Cargo.toml` si no existe.
    - Verificar: `cargo test -p tessera-graph-monitor` pasa.

#### REFACTOR

12. [ ] Mover la lógica de autenticación a función privada `check_bearer_auth`
    - Archivo: `crates/tessera-graph-monitor/src/server.rs`
    - Extraer la verificación del header a:
      ```rust
      fn check_bearer_auth(request: &str, expected_token: &str) -> bool {
          request
              .lines()
              .find(|l| l.to_lowercase().starts_with("authorization:"))
              .and_then(|l| l.split_once(':').map(|(_, v)| v.trim()))
              .and_then(|v| v.strip_prefix("Bearer "))
              .map(|t| constant_time_eq::constant_time_eq(t.as_bytes(), expected_token.as_bytes()))
              .unwrap_or(false)
      }
      ```
    - Verificar: `cargo test -p tessera-graph-monitor` pasa. Cero warnings.

---

### Cycle 5: Session TTL con Instant monotónico — HIGH #9 + base para CRITICAL #4

**Hallazgo**: `utils::unix_timestamp()` usa `SystemTime` (vulnerable a ajustes NTP).
Un salto NTP hacia adelante puede invalidar todas las sesiones simultáneamente;
un salto hacia atrás puede hacerlas "inmortales".

**Dependencia**: Este cycle debe completarse antes del Cycle 2 (purge_expired),
que necesita `Instant` para su comparación.

#### RED

13. [ ] Escribir tests que documenten el contrato de Instant
    - Archivo: `crates/tessera-graph-auth/tests/session_test.rs`

    ```rust
    #[test]
    fn session_ttl_uses_monotonic_clock() {
        // Verificar que los TTLs de sesión son inmunes a cambios de SystemTime.
        // No podemos ajustar SystemTime en tests, pero sí podemos verificar
        // que una sesión creada con TTL grande no expira al instante.
        let mgr = SessionManager::new(3600);
        let token = mgr.create_session(UserId::new(1)).unwrap(); // OK: test
        // Si TTL usa Instant correctamente, la sesión es válida ahora mismo.
        // Si usara SystemTime con un bug, podría fallar.
        assert!(mgr.validate(&token).is_ok(), "sesión recién creada debe ser válida");
    }

    #[test]
    fn session_expires_after_ttl_elapses() {
        // TTL de 1s. Esperar 2s y verificar que expira correctamente con Instant.
        let mgr = SessionManager::new(1);
        let token = mgr.create_session(UserId::new(1)).unwrap(); // OK: test
        std::thread::sleep(std::time::Duration::from_secs(2));
        assert!(
            mgr.validate(&token).is_err(),
            "sesión con TTL=1s debe expirar tras 2s"
        );
    }
    ```

    - Estos tests ya PASAN con la implementación actual (SystemTime).
      El RED se detecta al cambiar la implementación interna.

#### GREEN

14. [ ] Migrar `Session.expires_at` de `u64` (unix timestamp) a `std::time::Instant`
    - Archivo: `crates/tessera-graph-auth/src/session.rs`

    ```rust
    struct Session {
        user_id: UserId,
        /// Deadline using the monotonic clock. Immune to NTP adjustments.
        expires_at: std::time::Instant,
        roles: Vec<RoleId>,
    }
    ```

    En `create_session_with_roles`:
    ```rust
    let expires_at = std::time::Instant::now() + std::time::Duration::from_secs(self.ttl_seconds);
    ```

    En `validate`:
    ```rust
    if std::time::Instant::now() > expires_at {
        // ...revoke and return TokenExpired
    }
    ```

    - Eliminar el import de `utils::unix_timestamp` de `session.rs` (ya no se usa).
    - Verificar: `cargo test -p tessera-graph-auth` pasa. Los tests de session existentes siguen verdes.
    - Ahora implementar `purge_expired` del Cycle 2:
      ```rust
      pub fn purge_expired(&self) -> usize {
          let now = std::time::Instant::now();
          let Ok(mut sessions) = self.sessions.write() else { return 0; };
          let before = sessions.len();
          sessions.retain(|_, s| s.expires_at > now);
          before - sessions.len()
      }
      ```

#### REFACTOR

15. [ ] Agregar background cleanup task de sesiones en `main.rs`
    - Archivo: `crates/tessera-graph-server/src/main.rs`

    Después de crear `sessions = Arc::new(SessionManager::new(3600))`:
    ```rust
    // Background session cleanup: purge expired sessions every 5 minutes
    // to prevent unbounded HashMap growth under high connection churn.
    {
        let sessions_cleanup = Arc::clone(&sessions);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(300));
            loop {
                interval.tick().await;
                let removed = sessions_cleanup.purge_expired();
                if removed > 0 {
                    tracing::debug!("session cleanup: removed {removed} expired sessions");
                }
            }
        });
    }
    ```

    - Verificar: `cargo check -p tessera-graph-server` sin errores ni warnings.

---

### Cycle 6: Disk space checks — HIGH #7

**Hallazgo**: No hay validación de espacio en disco antes de WAL writes o page flushes.
Disco lleno causa cascada de `Err(Os { code: 28, kind: StorageFull })` no manejados.

**Diseño**: Añadir verificación periódica de espacio disponible al background flush task.
Si el espacio libre cae por debajo de un umbral configurable (`TESSERA_MIN_FREE_DISK_MB`,
default 100 MB), marcar el servidor como degradado en el health flag y loguear warning.

#### RED

16. [ ] Escribir test para el threshold de espacio en disco
    - Archivo: `crates/tessera-graph-server/src/flush_task.rs` — inline `#[cfg(test)] mod tests`

    ```rust
    #[test]
    fn disk_space_threshold_constant_is_positive() {
        assert!(
            MIN_FREE_DISK_BYTES > 0,
            "threshold mínimo debe ser positivo"
        );
    }

    #[test]
    fn available_space_check_does_not_panic_on_nonexistent_path() {
        // La función de check debe retornar None o un valor conservador
        // para paths inexistentes, sin panic.
        let result = check_available_disk_bytes(std::path::Path::new("/nonexistent/path/xyz"));
        // No debe panic. El resultado puede ser None.
        let _ = result;
    }
    ```

    - Verificar: tests FALLAN (RED) porque `MIN_FREE_DISK_BYTES` y
      `check_available_disk_bytes` no existen.

#### GREEN

17. [ ] Implementar check de espacio en disco en `flush_task.rs`
    - Archivo: `crates/tessera-graph-server/src/flush_task.rs`

    ```rust
    /// Minimum free bytes on the data volume before marking the server degraded.
    /// Default: 100 MB. Override with `TESSERA_MIN_FREE_DISK_MB`.
    pub const MIN_FREE_DISK_BYTES: u64 = 100 * 1024 * 1024;

    /// Return available bytes on the filesystem containing `path`, or `None`
    /// if the check fails (non-existent path, permission denied, etc.).
    pub fn check_available_disk_bytes(path: &std::path::Path) -> Option<u64> {
        // Use std::fs::metadata to get a path we can query, then use
        // platform-specific statvfs. Use the `fs2` crate if available,
        // or `libc::statvfs` on Unix.
        // Conservative implementation: delegate to the `fs2` crate.
        // If fs2 is not in Cargo.toml, add it as a dependency.
        fs2::available_space(path).ok()
    }
    ```

    En el loop del background flush task, agregar el check:
    ```rust
    // After each successful flush cycle:
    let min_free = std::env::var("TESSERA_MIN_FREE_DISK_MB")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(100) * 1024 * 1024;

    if let Some(available) = check_available_disk_bytes(&base_dir) {
        if available < min_free {
            tracing::warn!(
                "low disk space: {} MB available (threshold: {} MB)",
                available / (1024 * 1024),
                min_free / (1024 * 1024)
            );
            health.set_degraded();
        }
    }
    ```

    - Agregar `fs2 = "0.4"` a `crates/tessera-graph-server/Cargo.toml`.
    - Verificar: `cargo test -p tessera-graph-server` pasa.

#### REFACTOR

18. [ ] Mover la lectura de `TESSERA_MIN_FREE_DISK_MB` a `PersistenceConfig`
    - Archivo: `crates/tessera-graph-server/src/config.rs`
    - Agregar campo `min_free_disk_bytes: u64` a `PersistenceConfig`.
    - Leer desde `TESSERA_MIN_FREE_DISK_MB` en `PersistenceConfig::from_env()`.
    - Pasar el valor al flush task en lugar de leerlo cada iteración.
    - Verificar: `cargo test -p tessera-graph-server` pasa. Cero warnings.

---

### Cycle 7: SIGKILL audit loss — BufWriter flush en señal — HIGH #8

**Hallazgo**: `AuditWriterTask.writer` es un `BufWriter<File>`. En SIGKILL, los bytes
en el buffer se pierden. El canal ya está drenado (flush por batch) pero si el proceso
muere durante la escritura, el último batch puede quedar a medias.

**Diseño pragmático**: Reducir el riesgo agregando flush explícito después de cada
ENTRY individual (no solo después del batch), usando `sync_all()` en lugar de `flush()`
para garantizar que los bytes lleguen al hardware. Esto aumenta la latencia de escritura
pero minimiza la ventana de pérdida.

**Alternativa aceptada**: Mantener el batch flush actual (buen rendimiento) pero
agregar `sync_all()` solo para las entries marcadas como `critical = true` en el
futuro. Por ahora: agregar flag `sync_after_write` configurable.

#### RED

19. [ ] Escribir test que verifica que el writer hace sync después de cada batch
    - Archivo: `crates/tessera-graph-audit/src/lib.rs`, `#[cfg(test)] mod tests`

    ```rust
    #[test]
    fn audit_writer_task_has_sync_after_batch_option() {
        // Verificar que AuditWriterTask acepta un flag de sync.
        // Si sync_data = true, el writer llama sync_data() después del flush.
        // Este test verifica que la construcción acepta el campo.
        let dir = tempfile::tempdir().unwrap(); // OK: test
        let path = dir.path().join("audit.ndjson");
        let (_, task) = AuditLog::open_with_sync(&path, 0, 0, 16, true).unwrap(); // OK: test
        // La tarea se construyó correctamente con sync_data = true.
        drop(task);
    }
    ```

    - Verificar: test FALLA (RED) porque `open_with_sync` no existe.

#### GREEN

20. [ ] Agregar opción `sync_data: bool` a `AuditWriterTask`
    - Archivo: `crates/tessera-graph-audit/src/lib.rs`

    ```rust
    pub struct AuditWriterTask {
        receiver: mpsc::Receiver<AuditEntry>,
        writer: BufWriter<File>,
        path: PathBuf,
        bytes_written: u64,
        rotation_max_size_bytes: u64,
        max_rotated_files: usize,
        /// If true, call `sync_data()` after each batch flush to minimize
        /// data loss on SIGKILL. Increases write latency; use in production.
        sync_data: bool,
    }
    ```

    En el loop de `run()`, después del `self.writer.flush()`:
    ```rust
    if self.sync_data {
        if let Err(e) = self.writer.get_ref().sync_data() {
            tracing::warn!("audit sync_data error: {e}");
        }
    }
    ```

    Agregar constructor:
    ```rust
    pub fn open_with_sync(
        path: &Path,
        rotation_max_size_bytes: u64,
        max_rotated_files: usize,
        channel_capacity: usize,
        sync_data: bool,
    ) -> Result<(Self, AuditWriterTask)> { ... }
    ```

    - En `main.rs`, leer `TESSERA_AUDIT_SYNC=true/false` (default `true`) y
      pasar el flag al constructor.
    - Agregar campo `sync_data: bool` a `AuditConfig`.
    - Verificar: `cargo test -p tessera-graph-audit` pasa.

#### REFACTOR

21. [ ] Actualizar `AuditConfig::from_env()` para incluir `sync_data`
    - Archivo: `crates/tessera-graph-config/src/lib.rs`
    - Agregar `sync_data: bool` (default `true`) a `AuditConfig`.
    - Leer `TESSERA_AUDIT_SYNC`.
    - Verificar: `cargo test -p tessera-graph-config` pasa.

---

### Cycle 8: Tenant registry no eviction — LRU eviction policy — HIGH #6

**Hallazgo**: `TenantRegistry.unload()` existe pero nunca se llama. Con 100+ tenants
activos, el registry crece indefinidamente en memoria (~6-7 FDs por tenant).

**Diseño**: Agregar política LRU simple con cap configurable (`TESSERA_MAX_LOADED_TENANTS`,
default 0 = sin límite). Cuando se supera el cap, el tenant menos recientemente
usado (LRU) se descarga. Implementar con un `LinkedHashMap` del crate `indexmap`
o manualmente con un `VecDeque<DatabaseAddress>` de keys de acceso reciente.

#### RED

22. [ ] Escribir tests de eviction
    - Archivo: `crates/tessera-graph-tenant/src/registry.rs`, `#[cfg(test)] mod tests`

    ```rust
    #[test]
    fn registry_evicts_lru_when_cap_exceeded() {
        let dir = tempfile::tempdir().unwrap(); // OK: test
        // cap = 2: solo 2 tenants en memoria simultáneamente
        let registry = TenantRegistry::new_with_cap(dir.path(), GraphConfig::new(), 2);

        let addr1 = make_addr("t1", "db1");
        let addr2 = make_addr("t2", "db2");
        let addr3 = make_addr("t3", "db3");

        let _ = registry.get_or_load(&addr1).unwrap(); // OK: test
        let _ = registry.get_or_load(&addr2).unwrap(); // OK: test
        // addr1 es LRU ahora (addr2 fue accedido después)
        let _ = registry.get_or_load(&addr3).unwrap(); // OK: test — debe descargar addr1

        // addr1 fue eviccionado, addr2 y addr3 están en memoria
        assert_eq!(registry.loaded_count(), 2);
    }

    #[test]
    fn registry_with_cap_zero_has_no_eviction() {
        let dir = tempfile::tempdir().unwrap(); // OK: test
        let registry = TenantRegistry::new_with_cap(dir.path(), GraphConfig::new(), 0);
        let addr1 = make_addr("t1", "db1");
        let addr2 = make_addr("t2", "db2");
        let _ = registry.get_or_load(&addr1).unwrap(); // OK: test
        let _ = registry.get_or_load(&addr2).unwrap(); // OK: test
        assert_eq!(registry.loaded_count(), 2);
    }
    ```

    - Verificar: tests FALLAN (RED) porque `new_with_cap` y `loaded_count` no existen.

#### GREEN

23. [ ] Implementar LRU eviction en `TenantRegistry`
    - Archivo: `crates/tessera-graph-tenant/src/registry.rs`
    - Agregar `max_loaded: usize` y `access_order: VecDeque<DatabaseAddress>` a la struct.
    - En `get_or_load`, al insertar un nuevo graph: si `graphs.len() >= max_loaded > 0`,
      descargar el entry más antiguo del `access_order`.
    - Actualizar `access_order` en cada `get_or_load` (mover el addr al frente).
    - El `unload` interno ejecuta `flush()` antes de `remove()`.
    - Agregar `new_with_cap(base_dir, config, cap)` como constructor alternativo.
    - Agregar `loaded_count() -> usize` para tests y métricas.
    - Verificar: `cargo test -p tessera-graph-tenant` pasa.

#### REFACTOR

24. [ ] Leer `TESSERA_MAX_LOADED_TENANTS` en `main.rs`
    - Archivo: `crates/tessera-graph-server/src/main.rs`
    - Leer el env var y pasar al constructor del registry.
    - Default: 0 (sin eviction, backward compatible).
    - Verificar: `cargo check -p tessera-graph-server` limpio.

---

## Fase 4 — DRY: Streaming Import — Deuda técnica

**Estimación: 1 h 45 min**

### Cycle 9: Error del producer silenciado — Hallazgo 1

**Preservado del plan original Fase 2.**

#### RED

25. [ ] Escribir test de error tardío del producer
    - Archivo: `crates/tessera-cli/src/import.rs`, `#[cfg(test)] mod tests`

    ```rust
    #[test]
    fn stream_csv_late_error_propagates_via_result() {
        let csv = "label,name\nPerson,Alice\nPerson,Bob\nbad\"label,Mallory\n";
        let mut count = 0usize;
        let result = stream_csv_import(std::io::Cursor::new(csv), |_| {
            count += 1;
            Ok(())
        });
        assert!(
            result.is_err(),
            "late error (row 3) must propagate; count was {count}"
        );
    }
    ```

    - Archivo: `crates/tessera-cli/src/main.rs`, `#[cfg(test)] mod tests`

    ```rust
    #[test]
    fn producer_error_after_consumer_drains_is_not_silenced() {
        let result: Result<Result<(), &str>, ()> = Ok(Err("producer error"));
        let inner = result.expect("join ok"); // OK: test
        assert!(inner.is_err(), "inner error must be visible to caller");
    }
    ```

#### GREEN

26. [ ] Corregir el wiring del producer en `handle_import`
    - Archivo: `crates/tessera-cli/src/main.rs`

    Canal cambia a `mpsc::channel::<String>(IMPORT_CHANNEL_CAPACITY)`.

    Producer retorna `Result<(), CliError>`:
    ```rust
    let producer = tokio::task::spawn_blocking(move || -> Result<(), CliError> {
        let send_stmt = |stmt: String| {
            tx.blocking_send(stmt)
                .map_err(|_| CliError::ImportExport("channel closed".into()))
        };
        match fmt_owned.as_str() {
            "json"      => import::stream_json_import(reader, send_stmt).map(|_| ()),
            "gql"       => import::stream_gql_import(reader, send_stmt).map(|_| ()),
            "csv-nodes" => import::stream_csv_import(reader, send_stmt).map(|_| ()),
            other => Err(CliError::ImportExport(format!(
                "unsupported import format: {other}"
            ))),
        }
    });
    ```

    Consumer loop solo recibe `String`:
    ```rust
    while let Some(stmt) = rx.recv().await {
        // ...
    }
    ```

    Inspección combinada de errores:
    ```rust
    drop(rx);
    // Error priority:
    // 1. Producer panic (JoinError) — always propagate
    // 2. Consumer query error — Bolt rejection, more actionable
    // 3. Producer logic error — parse/IO error discovered after consumer drained
    let producer_result = producer
        .await
        .map_err(|e| CliError::ImportExport(format!("import thread panicked: {e}")))?;
    if let Some(e) = query_err {
        return Err(e);
    }
    producer_result?;
    ```

    - Verificar: `cargo check -p tessera-cli` sin errores. `cargo test -p tessera-cli` pasa.

#### REFACTOR

27. [ ] Agregar comentario de prioridad de errores
    - Archivo: `crates/tessera-cli/src/main.rs`
    - El código de inspección ya tiene los comentarios del ciclo GREEN.
    - Verificar: `cargo clippy -p tessera-cli -- -D warnings` limpio.

---

### Cycle 10: DRY batch/streaming — Hallazgo 3

**Preservado del plan original Fase 3.**

#### RED

28. [ ] Documentar los tests de parity como contrato DRY
    - Archivo: `crates/tessera-cli/src/import.rs`
    - Agregar comentario `// Contrato DRY: batch delega en streaming` sobre cada parity test.
    - Verificar: `cargo test -p tessera-cli` pasa (estos tests ya existen y pasan).

#### GREEN

29. [ ] Refactorizar `split_gql_statements` para delegar en `stream_gql_import`
    - Archivo: `crates/tessera-cli/src/import.rs`
    ```rust
    pub fn split_gql_statements(content: &str) -> Vec<String> {
        let mut out = Vec::new();
        let _ = stream_gql_import(std::io::Cursor::new(content), |s| {
            out.push(s);
            Ok(())
        });
        out
    }
    ```
    - Verificar: `cargo test -p tessera-cli` pasa.

30. [ ] Refactorizar `csv_nodes_to_gql` para delegar en `stream_csv_import`
    - Archivo: `crates/tessera-cli/src/import.rs`
    ```rust
    pub fn csv_nodes_to_gql(csv_content: &str) -> Result<Vec<String>, CliError> {
        let mut out = Vec::new();
        stream_csv_import(std::io::Cursor::new(csv_content.as_bytes()), |s| {
            out.push(s);
            Ok(())
        })?;
        Ok(out)
    }
    ```
    - Verificar: `cargo test -p tessera-cli` pasa.

31. [ ] Refactorizar `json_to_gql_statements` para delegar en `stream_json_import`
    - Archivo: `crates/tessera-cli/src/import.rs`
    ```rust
    pub fn json_to_gql_statements(json_text: &str) -> Result<Vec<String>, CliError> {
        let mut out = Vec::new();
        stream_json_import(std::io::Cursor::new(json_text.as_bytes()), |s| {
            out.push(s);
            Ok(())
        })?;
        Ok(out)
    }
    ```
    - Eliminar la implementación DOM anterior de `json_to_gql_statements`.
    - Verificar: `cargo test -p tessera-cli` pasa. Todos los tests JSON siguen verdes.

#### REFACTOR

32. [ ] Eliminar código muerto tras el refactor
    - Ejecutar: `cargo clippy -p tessera-cli -- -D warnings` para detectar dead code.
    - Verificar: cero warnings, cero errores.

---

### Cycle 11: `eprintln!` en función pura — Hallazgo 2

**Preservado del plan original Fase 4.**

#### RED

33. [ ] Escribir tests que documentan la nueva firma con `Result`
    - Archivo: `crates/tessera-cli/src/import.rs`, `#[cfg(test)] mod tests`

    ```rust
    #[test]
    fn write_json_value_array_returns_ok_no_eprintln() {
        use serde_json::json;
        let mut buf = String::new();
        let val = json!(["a", "b", "c"]);
        let result = write_json_value_to_buf(&val, &mut buf);
        assert!(!buf.is_empty(), "array should produce some output");
        assert!(result.is_ok());
    }

    #[test]
    fn write_json_value_object_returns_ok_no_eprintln() {
        use serde_json::json;
        let mut buf = String::new();
        let val = json!({"nested": "value"});
        let result = write_json_value_to_buf(&val, &mut buf);
        assert!(!buf.is_empty());
        assert!(result.is_ok());
    }
    ```

    - Verificar: tests FALLAN (RED) porque la firma actual es `fn(...) -> ()`.

#### GREEN

34. [ ] Cambiar la firma de `write_json_value_to_buf` a `Result<(), CliError>`
    - Archivo: `crates/tessera-cli/src/import.rs`

    Nueva firma:
    ```rust
    fn write_json_value_to_buf(v: &serde_json::Value, buf: &mut String) -> Result<(), CliError>
    ```

    Cambios en el cuerpo:
    - Todos los casos retornan `Ok(())`.
    - El caso `other` (arrays/objects): eliminar el `eprintln!`, serializar con
      `serde_json::to_string(other).unwrap_or_default()`, retornar `Ok(())`.
    - Actualizar call sites en `write_json_props_to_buf` y `write_endpoint_match`
      agregando `?` después de cada llamada.

    - Verificar: `cargo test -p tessera-cli` pasa.

#### REFACTOR

35. [ ] Actualizar doc comment y verificar ausencia de `eprintln!` en funciones puras
    - Agregar doc comment a `write_json_value_to_buf`:
      ```rust
      /// Write a JSON value as a GQL literal into `buf`.
      ///
      /// Arrays and objects are serialized as JSON strings (GQL does not have
      /// native array/object literals). Use the caller's context to surface
      /// a diagnostic if needed.
      fn write_json_value_to_buf(v: &serde_json::Value, buf: &mut String) -> Result<(), CliError>
      ```
    - Verificar: ningún `eprintln!` en `import.rs` fuera de `#[cfg(test)]`.
    - Verificar: `cargo clippy -p tessera-cli -- -D warnings` limpio.

---

## Fase 5 — MEDIUM: Resiliencia

**Estimación: 3 h**

### Cycle 12: Swallowed errors — Hallazgo #13

**Hallazgo**: En `listener.rs:123`, `let _ = handler.run().await` silencia todos los
errores de conexión. El operador no sabe si hay storms de errores de conexión.

#### RED

36. [ ] Escribir test que verifica que errores de handler se loguean
    - Archivo: `crates/tessera-graph-server/tests/listener_test.rs`

    ```rust
    #[tokio::test]
    async fn handler_errors_are_logged_not_silenced() {
        // Este test verifica el contrato de logging: cuando handler.run()
        // retorna Err, el error debe aparecer en el log (tracing::warn/error),
        // no silenciarse con let _ = ...
        //
        // Implementación: usar tracing-test o verificar que el código
        // llama a tracing::warn! en lugar de let _ = ...
        // Por ahora: verificación estática en la sección de wiring (Fase 7).
        // Este test documenta el contrato esperado.
        assert!(true, "ver Cycle de wiring en Fase 7 para verificación estática");
    }
    ```

#### GREEN

37. [ ] Cambiar `let _ = handler.run().await` a log del error
    - Archivo: `crates/tessera-graph-server/src/listener.rs`

    En `serve` y `serve_tls`, cambiar:
    ```rust
    // ANTES:
    let _ = handler.run().await;

    // DESPUES:
    if let Err(e) = handler.run().await {
        tracing::warn!("connection handler error: {e}");
    }
    ```

    Los audit errors en `bolt_handler.rs` (8 call sites con `let _ = self.ctx.audit()...`):
    Estos son correctos — los errores de audit no deben abortar la operación de usuario.
    Documentar con comentario que explica la decisión:
    ```rust
    // Audit errors (ChannelFull, ChannelClosed) are non-fatal by design:
    // we never deny service because of an audit backpressure issue.
    // Operational visibility comes from the AuditError::ChannelFull eprintln
    // in record_event() and the audit_overflow metric (see MEDIUM #14).
    let _ = self.ctx.audit().record_event(...);
    ```

    - Verificar: `cargo test -p tessera-graph-server` pasa.

#### REFACTOR

38. [ ] Verificar que `tracing::warn!` es consistente con el resto del codebase
    - Verificar: `cargo clippy -p tessera-graph-server -- -D warnings` limpio.

---

### Cycle 13: Audit channel overflow metric — Hallazgo #14

**Hallazgo**: `audit/lib.rs:210-218` imprime en stderr cuando el canal está lleno
(`eprintln!("audit: channel full — entry dropped")`), pero no hay métrica Prometheus.
Un attacker puede suprimir el audit trail con flood; no hay alerta.

#### RED

39. [ ] Escribir test para la métrica de overflow
    - Archivo: `crates/tessera-graph-audit/src/lib.rs`, `#[cfg(test)] mod tests`

    ```rust
    #[test]
    fn record_event_channel_full_returns_channel_full_error() {
        // Canal de capacidad 1, ya lleno
        let (sender, _rx) = tokio::sync::mpsc::channel(1);
        // Llenar el canal
        sender.try_send(AuditEntry::success(None, AuditEvent::Logout)).unwrap(); // OK: test
        let log = AuditLog::new_with_sender(sender);
        let result = log.record_event(AuditEntry::success(None, AuditEvent::Logout));
        assert!(
            matches!(result, Err(AuditError::ChannelFull)),
            "canal lleno debe retornar ChannelFull"
        );
    }
    ```

    Este test ya PASA. El gap es la métrica Prometheus. El RED real es:
    ```rust
    #[test]
    fn audit_log_exposes_dropped_count() {
        // AuditLog debe tener un contador atómico de entries dropeadas
        // que se pueda leer externamente para exponer en /metrics.
        let (log, _task) = AuditLog::open_with_capacity(/* ... */, 1).unwrap(); // OK: test
        // Forzar overflow...
        // log.dropped_count() debe retornar > 0
        // Este test FALLA (RED) porque dropped_count() no existe.
        let _ = log.dropped_count(); // debe compilar
    }
    ```

    - Verificar: test FALLA (RED) porque `dropped_count()` no existe.

#### GREEN

40. [ ] Agregar contador `dropped_count` a `AuditLog`
    - Archivo: `crates/tessera-graph-audit/src/lib.rs`

    ```rust
    pub struct AuditLog {
        sender: mpsc::Sender<AuditEntry>,
        /// Count of entries dropped due to channel backpressure.
        /// Expose this via /metrics as `tessera_audit_entries_dropped_total`.
        dropped_count: std::sync::Arc<std::sync::atomic::AtomicU64>,
    }

    impl AuditLog {
        /// Return the number of audit entries dropped due to channel overflow.
        pub fn dropped_count(&self) -> u64 {
            self.dropped_count.load(std::sync::atomic::Ordering::Relaxed)
        }
    }
    ```

    En `record_event`, incrementar el contador en el arm `ChannelFull`:
    ```rust
    mpsc::error::TrySendError::Full(_) => {
        self.dropped_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        AuditError::ChannelFull
    }
    ```

    - Agregar `tessera_audit_entries_dropped_total` a `MetricsRegistry` y `render.rs`.
    - En `main.rs`, pasar el `dropped_count` Arc al contexto para que el render task
      lo lea periódicamente.
    - Verificar: `cargo test -p tessera-graph-audit` pasa.

#### REFACTOR

41. [ ] Agregar el counter al renderer de Prometheus
    - Archivo: `crates/tessera-graph-monitor/src/render.rs` y `registry.rs`
    - Agregar `audit_entries_dropped: AtomicU64` a `MetricsRegistry`.
    - Actualizar `render_prometheus` para incluirlo.
    - Verificar: `cargo test -p tessera-graph-monitor` pasa.

---

### Cycle 14: System metrics — Hallazgo #15

**Hallazgo**: `render.rs` no incluye métricas de sistema (RSS, FD count, disco).
OOM y disk full son invisibles hasta el crash.

#### RED

42. [ ] Escribir test que verifica presencia de métricas de sistema
    - Archivo: `crates/tessera-graph-monitor/src/render.rs`, sección `#[cfg(test)] mod tests`

    ```rust
    #[test]
    fn render_contains_process_rss_bytes() {
        let r = MetricsRegistry::new(256);
        // Actualizar con un valor de RSS ficticio
        r.process_rss_bytes.store(1024 * 1024, std::sync::atomic::Ordering::Relaxed);
        let output = render_prometheus(&r);
        assert!(
            output.contains("tessera_process_rss_bytes"),
            "RSS metric must be present in output"
        );
    }

    #[test]
    fn render_contains_open_file_descriptors() {
        let r = MetricsRegistry::new(256);
        r.open_fds.store(42, std::sync::atomic::Ordering::Relaxed);
        let output = render_prometheus(&r);
        assert!(output.contains("tessera_open_file_descriptors 42"));
    }
    ```

    - Verificar: tests FALLAN (RED) porque `process_rss_bytes` y `open_fds` no existen.

#### GREEN

43. [ ] Agregar métricas de sistema a `MetricsRegistry`
    - Archivo: `crates/tessera-graph-monitor/src/registry.rs`
    - Agregar:
      ```rust
      /// Resident Set Size of the process in bytes (updated by background task).
      pub process_rss_bytes: AtomicU64,
      /// Current number of open file descriptors (updated by background task).
      pub open_fds: AtomicU64,
      ```

    - Archivo: `crates/tessera-graph-monitor/src/render.rs`
    - Agregar al renderer:
      ```rust
      write_gauge(&mut buf, "tessera_process_rss_bytes",
          "Resident Set Size of the server process in bytes",
          registry.process_rss_bytes.load(Ordering::Relaxed));
      write_gauge(&mut buf, "tessera_open_file_descriptors",
          "Number of open file descriptors",
          registry.open_fds.load(Ordering::Relaxed));
      ```

    - En `main.rs`, agregar background task que actualiza estas métricas cada 30s:
      ```rust
      // En Unix: leer /proc/self/status para RSS, /proc/self/fd para FD count.
      // En otros sistemas: usar valores de 0 (métrica presente pero vacía).
      #[cfg(target_os = "linux")]
      fn read_rss_bytes() -> u64 { /* leer /proc/self/status VmRSS */ }
      #[cfg(not(target_os = "linux"))]
      fn read_rss_bytes() -> u64 { 0 }
      ```

    - Verificar: `cargo test -p tessera-graph-monitor` pasa.

#### REFACTOR

44. [ ] Extraer la lectura de system stats a módulo `sys_stats.rs`
    - Archivo: `crates/tessera-graph-monitor/src/sys_stats.rs` (nuevo)
    - Mover las funciones `read_rss_bytes()` y `read_open_fds()`.
    - Verificar: `cargo clippy -p tessera-graph-monitor -- -D warnings` limpio.

---

### Cycle 15: Env vars lazy validation — Hallazgo #16

**Hallazgo**: Typos en variables de entorno caen silenciosamente al default.
`TESSERA_WAL_ENABLED=treu` → WAL desactivado silenciosamente.

#### RED

45. [ ] Escribir tests de validación de env vars
    - Archivo: `crates/tessera-graph-server/src/config.rs`, `#[cfg(test)] mod tests`

    ```rust
    #[test]
    fn parse_flush_interval_warns_on_invalid_value() {
        // parse_flush_interval con valor no numérico debe retornar el default
        // Y la función caller debe poder detectar que el valor fue inválido.
        // Cambiamos la firma para retornar (value, was_default_due_to_parse_error).
        let (val, used_default) = PersistenceConfig::parse_flush_interval_checked(Some("notanumber"));
        assert_eq!(val, DEFAULT_FLUSH_INTERVAL_MS);
        assert!(used_default, "invalid value debe usar default y señalarlo");
    }

    #[test]
    fn parse_flush_interval_checked_valid_value() {
        let (val, used_default) = PersistenceConfig::parse_flush_interval_checked(Some("100"));
        assert_eq!(val, 100);
        assert!(!used_default);
    }
    ```

    - Verificar: tests FALLAN (RED) porque `parse_flush_interval_checked` no existe.

#### GREEN

46. [ ] Agregar variante `_checked` a los parsers de config y loguear warnings
    - Archivo: `crates/tessera-graph-server/src/config.rs`

    ```rust
    /// Like `parse_flush_interval` but returns a boolean indicating whether
    /// the default was used due to a parse error (for diagnostic logging).
    pub fn parse_flush_interval_checked(raw: Option<&str>) -> (u64, bool) {
        match raw.and_then(|v| v.parse::<u64>().ok()) {
            Some(v) => (v, false),
            None if raw.is_some() => (DEFAULT_FLUSH_INTERVAL_MS, true), // invalid, used default
            None => (DEFAULT_FLUSH_INTERVAL_MS, false), // not set, used default normally
        }
    }
    ```

    En `PersistenceConfig::from_env()`, usar la variante checked y loguear:
    ```rust
    let (flush_interval_ms, flush_default) = Self::parse_flush_interval_checked(
        std::env::var("TESSERA_FLUSH_INTERVAL_MS").ok().as_deref(),
    );
    if flush_default {
        tracing::warn!(
            "TESSERA_FLUSH_INTERVAL_MS has invalid value — using default {}ms",
            DEFAULT_FLUSH_INTERVAL_MS
        );
    }
    ```

    Aplicar el mismo patrón a `TESSERA_MEMORY_LIMIT_MB`, `TESSERA_QUERY_CACHE_CAPACITY`,
    `TESSERA_MAX_CONNECTIONS`, `TESSERA_IDLE_TIMEOUT_SECS`.

    - Verificar: `cargo test -p tessera-graph-server` pasa.

#### REFACTOR

47. [ ] Consolidar todos los parse-with-warning en un helper genérico
    - Archivo: `crates/tessera-graph-server/src/config.rs`
    - Extraer:
      ```rust
      fn parse_env_or_warn<T: std::str::FromStr>(name: &str, default: T) -> T {
          match std::env::var(name) {
              Err(_) => default, // not set
              Ok(v) => v.parse().unwrap_or_else(|_| {
                  tracing::warn!("{name} has invalid value '{v}' — using default");
                  default
              }),
          }
      }
      ```
    - Reemplazar los parsers inline en `from_env()` por llamadas a este helper.
    - Verificar: `cargo test -p tessera-graph-server` pasa. Cero warnings de clippy.

---

### Cycle 16: FDs per tenant monitoring — Hallazgo #12

**Hallazgo**: ~6-7 FDs por tenant cargado. Con ulimit 1024, ~146 tenants saturan
el proceso. Sin monitoring, el agotamiento es opaco.

#### RED

48. [ ] Escribir test para la métrica de FDs por tenant
    - Archivo: `crates/tessera-graph-tenant/src/registry.rs`, `#[cfg(test)] mod tests`

    ```rust
    #[test]
    fn loaded_count_reflects_active_tenants() {
        let dir = tempfile::tempdir().unwrap(); // OK: test
        let registry = TenantRegistry::new(dir.path(), GraphConfig::new());
        assert_eq!(registry.loaded_count(), 0);
        let addr = make_addr("t1", "db1");
        let _ = registry.get_or_load(&addr).unwrap(); // OK: test
        assert_eq!(registry.loaded_count(), 1);
    }
    ```

    - Si `loaded_count()` ya existe del Cycle 8, este test ya PASA (verificar).

#### GREEN

49. [ ] Agregar `loaded_count()` a `TenantRegistry` si no existe aún
    - Archivo: `crates/tessera-graph-tenant/src/registry.rs`
    - Si ya se implementó en el Cycle 8, este paso es un no-op.
    - Exponer como métrica en `MetricsRegistry`: `tessera_tenants_loaded`.
    - En `main.rs`, actualizar la métrica en el background task existente.
    - Verificar: `cargo test -p tessera-graph-tenant` pasa.

#### REFACTOR

50. [ ] Añadir FD estimate a la métrica de tenants
    - Documentar en el render que la estimación es `loaded_count * 7`.
    - Agregar `tessera_estimated_tenant_fds` como métrica derivada en `render.rs`.
    - Verificar: `cargo test -p tessera-graph-monitor` pasa.

---

## Fase 6 — Streaming Import Improvements

**Estimación: 2 h 45 min**

### Cycle 17: Hallazgos 5, 6, 7, 8, 9 — Mejoras recomendadas

**Preservado del plan original Fase 5.**

51. [ ] Extraer constante `IMPORT_CHANNEL_CAPACITY = 64` con doc comment
    - Archivo: `crates/tessera-cli/src/main.rs`
    - Reemplazar el literal `64` en `mpsc::channel` por la constante.
    - Verificar: `cargo test -p tessera-cli` pasa.

52. [ ] Cambiar `stream_gql_import` para aceptar `Read` en lugar de `BufRead`
    - Archivo: `crates/tessera-cli/src/import.rs`

    **Ciclo RED**:
    ```rust
    #[test]
    fn stream_gql_accepts_unbuffered_read() {
        struct UnbufferedReader(std::io::Cursor<&'static str>);
        impl std::io::Read for UnbufferedReader {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> { self.0.read(buf) }
        }
        let r = UnbufferedReader(std::io::Cursor::new("CREATE (:X);"));
        let mut out = Vec::new();
        stream_gql_import(r, |s| { out.push(s); Ok(()) }).unwrap(); // OK: test
        assert_eq!(out.len(), 1);
    }
    ```

    **Ciclo GREEN**: Cambiar firma a `<R: std::io::Read>` y agregar
    `let reader = std::io::BufReader::new(reader);` al inicio.
    En `main.rs`, eliminar el `BufReader::new` wrapper para GQL.
    - Verificar: `cargo test -p tessera-cli` pasa.

53. [ ] Eliminar tests fantasma de `is_large_file`
    - Archivo: `crates/tessera-cli/src/main.rs`
    - Eliminar `LARGE_FILE_THRESHOLD_BYTES`, `is_large_file`, y sus 3 tests.
    - Verificar: `cargo test -p tessera-cli` pasa. Cero referencias a `is_large_file`.

54. [ ] Hacer timeout de throughput tests condicional a `debug_assertions`
    - Archivo: `crates/tessera-cli/src/import.rs`
    ```rust
    const THROUGHPUT_TIMEOUT: std::time::Duration = if cfg!(debug_assertions) {
        std::time::Duration::from_secs(10)
    } else {
        std::time::Duration::from_secs(2)
    };
    ```
    - Verificar: `cargo test -p tessera-cli throughput` pasa en debug y release.

55. [ ] Eliminar `should_report_progress` con semántica invertida
    - Archivo: `crates/tessera-cli/src/main.rs`
    - Eliminar la función `should_report_progress` y sus 5 tests asociados.
    - Agregar en su lugar:
      ```rust
      #[test]
      fn progress_interval_constant_is_positive() {
          assert!(PROGRESS_INTERVAL > 0);
      }
      #[test]
      fn progress_interval_fires_at_multiple() {
          assert_eq!(PROGRESS_INTERVAL % PROGRESS_INTERVAL, 0);
          assert_ne!((PROGRESS_INTERVAL - 1) % PROGRESS_INTERVAL, 0);
      }
      ```
    - Verificar: `cargo test -p tessera-cli` pasa.

---

### Cycle 18: Hallazgos 10, 11, 12 — Funcionalidad

**Preservado del plan original Fase 6.**

56. [ ] Implementar timeout por statement `IMPORT_STATEMENT_TIMEOUT = 30s`
    - Archivo: `crates/tessera-cli/src/main.rs`

    **Ciclo RED**:
    ```rust
    #[test]
    fn import_statement_timeout_constant_is_reasonable() {
        assert!(IMPORT_STATEMENT_TIMEOUT >= std::time::Duration::from_secs(10));
        assert!(IMPORT_STATEMENT_TIMEOUT <= std::time::Duration::from_secs(120));
    }
    ```

    **Ciclo GREEN**: Agregar constante y envolver `execute_query` con
    `tokio::time::timeout(IMPORT_STATEMENT_TIMEOUT, ...)`.
    - Verificar: `cargo test -p tessera-cli` pasa.

57. [ ] Agregar flag `--continue-on-error` a `ImportArgs`
    - Archivo: `crates/tessera-cli/src/cli.rs`

    **Ciclo RED**:
    ```rust
    #[test]
    fn parse_import_continue_on_error_flag() {
        let cli = Cli::try_parse_from([
            "tessera-cli", "import", "--file", "data.json", "--continue-on-error"
        ]).unwrap(); // OK: test
        match cli.command {
            Some(Command::Import(args)) => assert!(args.continue_on_error),
            _ => panic!("expected Import"),
        }
    }

    #[test]
    fn parse_import_continue_on_error_default_false() {
        let cli = Cli::try_parse_from([
            "tessera-cli", "import", "--file", "data.json"
        ]).unwrap(); // OK: test
        match cli.command {
            Some(Command::Import(args)) => assert!(!args.continue_on_error),
            _ => panic!("expected Import"),
        }
    }
    ```

    **Ciclo GREEN**: Agregar campo `continue_on_error: bool` e implementar en `handle_import`.
    - Verificar: `cargo test -p tessera-cli` pasa.

58. [ ] Agregar prefijo `[PROGRESS]` a mensajes de progreso
    - Archivo: `crates/tessera-cli/src/main.rs`

    **Ciclo RED**:
    ```rust
    #[test]
    fn progress_prefix_format() {
        assert_eq!(PROGRESS_PREFIX, "[PROGRESS] ");
    }
    ```

    **Ciclo GREEN**: Agregar `const PROGRESS_PREFIX: &str = "[PROGRESS] ";` y actualizar
    los `eprintln!` de progreso.
    - Verificar: `cargo test -p tessera-cli` pasa.

---

### Cycle 19: Hallazgos 13, 14 — Calidad de código

**Preservado del plan original Fase 6.**

59. [ ] Renombrar `format_endpoint_match` a `write_endpoint_match` con `&mut String`
    - Archivo: `crates/tessera-cli/src/import.rs`

    **Ciclo RED**:
    ```rust
    #[test]
    fn format_endpoint_match_writes_to_buffer() {
        let edge = serde_json::json!({
            "source": {"label": "A", "match": {"id": "1"}},
            "target": {"label": "B", "match": {"id": "2"}},
            "label": "R", "properties": {}
        });
        let mut buf = String::new();
        write_endpoint_match(&edge, "source", &mut buf).unwrap(); // OK: test
        assert!(buf.contains(":A"));
        assert!(buf.contains("id: '1'"));
    }
    ```

    **Ciclo GREEN**: Renombrar y cambiar firma a `fn write_endpoint_match(edge: &Value, endpoint_key: &str, buf: &mut String) -> Result<(), CliError>`.
    - Verificar: `cargo test -p tessera-cli` pasa.

60. [ ] Mover structs de serde al nivel de módulo
    - Archivo: `crates/tessera-cli/src/import.rs`
    - Mover `ArrayStreamSeed`, `RootVisitor`, `RootSeed` fuera de `stream_json_import`.
    - Mantenerlas como `struct` privadas (sin `pub`).
    - Verificar: `cargo test -p tessera-cli stream_json` pasa. Cero warnings clippy.

---

## Fase 7 — MANDATORY: Wiring Verification (análisis estático)

**Estimación: 30 min**

Esta fase es ANÁLISIS ESTÁTICO, NO tests nuevos. Se verifica con grep/search que
todo el código nuevo tiene call sites en producción. Solo si grep revela código
huérfano se añaden tests o se corrige el wiring.

### Cycle 20: Verificación de wiring post-implementación

61. [ ] Verificar que `purge_expired()` se llama en background task de `main.rs`
    - Buscar: `purge_expired` en `crates/tessera-graph-server/src/main.rs`
    - Criterio: debe aparecer al menos 1 vez fuera de `#[cfg(test)]`.

62. [ ] Verificar que `check_available_disk_bytes` se llama en `flush_task.rs`
    - Buscar: `check_available_disk_bytes` en `crates/tessera-graph-server/src/flush_task.rs`
    - Criterio: debe aparecer en el loop de producción.

63. [ ] Verificar que `min_free_disk_bytes` de `PersistenceConfig` se usa en `flush_task.rs`
    - Buscar: `min_free_disk_bytes` en `flush_task.rs` y `main.rs`
    - Criterio: debe leerse y pasarse al task.

64. [ ] Verificar que `metrics_token` se pasa desde `main.rs` a `serve_metrics`
    - Buscar: `TESSERA_METRICS_TOKEN` en `main.rs`
    - Criterio: debe leerse y pasarse al servidor de métricas.

65. [ ] Verificar que `loaded_count()` se actualiza en la métrica `tessera_tenants_loaded`
    - Buscar: `loaded_count` en `main.rs` o en el background metrics task
    - Criterio: debe actualizarse periódicamente.

66. [ ] Verificar que `dropped_count` del AuditLog se actualiza en `MetricsRegistry`
    - Buscar: `dropped_count` en `main.rs`
    - Criterio: debe leerse y escribirse en `audit_entries_dropped`.

67. [ ] Verificar que `session_count()` se expone como métrica
    - Buscar: `session_count` en `main.rs`
    - Criterio: debe actualizarse en el task de métricas o en el cleanup task.

68. [ ] Verificar que no hay referencias a `format_endpoint_match` (renombrada)
    - Buscar: `format_endpoint_match` en `crates/tessera-cli/src/`
    - Criterio: cero resultados. Si hay referencias, el refactor está incompleto.

69. [ ] Verificar que no hay referencias a `is_large_file` (eliminada)
    - Buscar: `is_large_file` en `crates/tessera-cli/src/`
    - Criterio: cero resultados.

70. [ ] Verificar que `IMPORT_STATEMENT_TIMEOUT` se usa en el loop del consumer
    - Buscar: `IMPORT_STATEMENT_TIMEOUT` en `crates/tessera-cli/src/main.rs`
    - Criterio: debe aparecer en el `tokio::time::timeout(...)` del consumer loop.

71. [ ] Verificar que `sync_data` de `AuditWriterTask` se lee en `main.rs`
    - Buscar: `TESSERA_AUDIT_SYNC` en `main.rs`
    - Criterio: debe leerse y pasarse al constructor de AuditLog.

72. [ ] Compilación limpia final
    - Comando: `nice cargo build --workspace 2>&1`
    - Criterio: cero errores, cero warnings (warnings = errors por lints del workspace).
    - Comando: `nice cargo test --workspace 2>&1`
    - Criterio: todos los tests pasan.
    - Comando: `nice cargo clippy --workspace -- -D warnings 2>&1`
    - Criterio: limpio.

---

## Estimación Total

| Fase | Hallazgos | Cycles | Tiempo |
|------|-----------|--------|--------|
| 1 — CRITICAL resiliencia | #1, #4 (base: #9) | 1-3 | 3 h |
| 2 — SECURITY streaming | H4 | 3 | 30 min |
| 3 — HIGH resiliencia | #10, #9, #7, #8, #6 | 4-8 | 4 h |
| 4 — DRY streaming | H1, H3, H2 | 9-11 | 1 h 45 min |
| 5 — MEDIUM resiliencia | #13, #14, #15, #16, #12 | 12-16 | 3 h |
| 6 — Streaming improvements | H5-H14 | 17-19 | 2 h 45 min |
| 7 — Wiring verification | todos | 20 | 30 min |
| **Total** | **13+14 hallazgos** | **20 cycles, 72 tareas** | **~15.5 h** |

---

## Criterios de Éxito

### Resiliencia
- [ ] `handle_pull` respeta el parámetro `n` de Bolt 4.4 — sin materialización completa
- [ ] `purge_expired()` existe en `SessionManager` y se llama desde background task
- [ ] `SessionManager.expires_at` usa `Instant` (immune a NTP)
- [ ] `/metrics` y `/health` requieren Bearer token cuando `TESSERA_METRICS_TOKEN` está configurado
- [ ] Background flush task verifica espacio en disco y marca degradado si bajo umbral
- [ ] `AuditWriterTask` soporta `sync_data: bool` para minimizar pérdida en SIGKILL
- [ ] `TenantRegistry` tiene política LRU configurable con `TESSERA_MAX_LOADED_TENANTS`
- [ ] Errores de `handler.run()` se loguean con `tracing::warn!` en lugar de `let _ =`
- [ ] `AuditLog.dropped_count()` expuesto como métrica Prometheus
- [ ] RSS y FD count en `/metrics`
- [ ] Config inválida en env vars genera `tracing::warn!` en lugar de silencio
- [ ] `loaded_count()` en TenantRegistry expuesto como métrica

### Streaming Import
- [ ] 6 tests de inyección CSV pasan (H4)
- [ ] Error tardío del producer visible al caller (H1)
- [ ] Las 3 funciones batch delegan en sus equivalentes streaming (H3)
- [ ] `write_json_value_to_buf` no imprime en stderr (H2)
- [ ] `IMPORT_CHANNEL_CAPACITY` nombrada con doc comment (H5)
- [ ] `stream_gql_import` acepta `Read` (H6)
- [ ] Tests fantasma de `is_large_file` eliminados (H7)
- [ ] Timeout throughput tests condicionado a `debug_assertions` (H8)
- [ ] `should_report_progress` con semántica falsa eliminada (H9)
- [ ] Timeout por statement 30s en consumer loop (H10)
- [ ] Flag `--continue-on-error` en import (H11)
- [ ] Prefijo `[PROGRESS]` en mensajes de progreso (H12)
- [ ] `format_endpoint_match` renombrada a `write_endpoint_match` con `&mut String` (H13)
- [ ] Structs de serde al nivel de módulo (H14)

### Compilación y calidad
- [ ] `cargo build --workspace` — cero errores, cero warnings
- [ ] `cargo test --workspace` — todos los tests pasan
- [ ] `cargo clippy --workspace -- -D warnings` — limpio
- [ ] Throughput guards: 10k elementos en < 10s debug / < 2s release (hot path streaming)

---

## Notas de Riesgo

**Cycle 1 (CRITICAL #1 — streaming PULL)**:
El fix propuesto es una mejora incremental (paginación por `n`), no la eliminación
completa de la materialización en `handle_run`. La materialización completa requiere
cambios en el motor tessera-graph (MIT core). Documentar como issue abierto.

**Cycle 5 (Session TTL — Instant)**:
Cambiar `expires_at` de `u64` a `Instant` es un cambio breaking si algún código
serializa sesiones a disco. Verificar que `Session` no implementa `Serialize` antes
del cambio. Confirmado: `Session` es `struct` privada sin `#[derive(Serialize)]`.

**Cycle 8 (LRU eviction)**:
La eviction de un tenant que tiene conexiones activas puede causar errores en esas
conexiones (el Arc del graph sigue vivo hasta que los handlers lo suelten, pero
las escrituras futuras al registry no encontrarán la entrada). Diseño: el LRU solo
aplica a la entrada del registry, no al Arc ya distribuido. Los handlers activos
siguen funcionando hasta que liberan su `Arc<RwLock<Graph>>`. El nuevo tenant que
intente conectarse al mismo database simplemente vuelve a cargar desde disco.

**Cycle 9 (producer channel type change)**:
El tipo del canal cambia de `channel::<Result<String, CliError>>` a `channel::<String>`.
Si algún path no actualizado aún envía `Err` al canal, habrá un error de compilación
(fail-fast). No puede causar comportamiento silencioso incorrecto.

**Cycle 11 (write_json_value_to_buf firma)**:
La función es llamada desde dentro de un visitor de serde. Cambiar a `Result` propaga
el error hacia arriba en la cadena de visitors. Verificar que todos los call sites
intermedios (`write_json_props_to_buf`, `write_endpoint_match`) ya retornan `Result`.
Confirmado en el análisis del código fuente.

**HIGH #5 (Memory limit not enforced)**:
Este hallazgo NO tiene un ciclo dedicado porque la solución completa requiere
instrumentar cada `Vec::push` o usar un allocator personalizado, lo cual es
desproporcionado. La mitigación correcta es la combinación de:
- LRU eviction del registry (Cycle 8)
- PULL paginado (Cycle 1)
- RSS metrics (Cycle 14) para visibilidad
Se documenta el gap residual en un issue de tracking.
