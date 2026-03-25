# Quality Review: Bolt M6 CLI Migration

**Date:** 2026-03-25
**Status:** Pending fixes (4 critical/high blockers)
**Reviewer:** quality-rust agent
**Build:** clippy clean, 958 tests passing

---

## Hallazgos Críticos (bloquean merge)

### C1: Handshake no valida versión negociada
- **Archivo:** `crates/tessera-protocol/src/bolt_client.rs:65`
- **Qué:** Solo rechaza `[0x00,0x00,0x00,0x00]`. Cualquier otra respuesta (garbage, versión incompatible) se acepta silenciosamente.
- **Impacto:** Confusing PackStream decode errors si el server responde con versión no soportada. MITM device intercepting produce errores crípticos.
- **Fix:** Validar que `resp[1] == 4` (major version) después de descartar all-zeros.

### C2: IGNORED en RUN produce falso éxito (pérdida silenciosa de datos)
- **Archivo:** `crates/tessera-protocol/src/bolt_client.rs:178`
- **Qué:** Si el server está en FAILED state y envía IGNORED al RUN, el catch-all `_ => Vec::new()` continúa al PULL, que también será IGNORED, y devuelve `Ok(QueryResult { columns: [], rows: [] })`.
- **Impacto:** El caller cree que la query ejecutó con 0 resultados. En realidad el server la rechazó.
- **Fix:** Tratar IGNORED y RECORD como error explícito en el match del RUN response.

### H1: PULL sin `n: -1` (fetch all)
- **Archivo:** `crates/tessera-protocol/src/bolt_client.rs:182`
- **Qué:** `BoltRequest::Pull { extra: vec![] }` — sin campo `n`. El servidor tessera ignora el extra, pero un servidor Bolt 4.x estricto devuelve solo 1 row.
- **Impacto:** Truncamiento silencioso contra Neo4j u otros servidores Bolt-compliant.
- **Fix:** Enviar `extra: vec![("n".to_owned(), PackStreamValue::Int(-1))]`.

### H2: Zero tests para BoltClient
- **Archivo:** No existe `crates/tessera-protocol/tests/bolt_client_test.rs`
- **Qué:** El componente nuevo más crítico (handshake + hello + run_query + goodbye) no tiene tests.
- **Impacto:** Los bugs C1, C2, H1 habrían sido detectados con tests básicos.
- **Fix:** Crear tests con `tokio::io::duplex` que verifiquen: handshake correcto, FAILURE→BoltAuthFailure, IGNORED→error, multi-row PULL, reset validates response.

---

## Mejoras Recomendadas (deberían implementarse)

### R1: HELLO sin `scheme: "basic"` ni `user_agent`
- **Archivo:** `crates/tessera-protocol/src/bolt_client.rs:123-136`
- **Impacto:** Incompatible con Neo4j y servidores Bolt estrictos.
- **Fix:** Añadir `scheme: "basic"` y `user_agent: "tessera-cli/VERSION"` al extra dict.

### R2: `reset()` descarta respuesta sin validar
- **Archivo:** `crates/tessera-protocol/src/bolt_client.rs:207-211`
- **Qué:** `let _ = self.recv_response().await?;` — si FAILURE, el caller cree que el reset funcionó.
- **Fix:** Match SUCCESS vs FAILURE y propagar error.

### R3: `BoltInvalidHandshake` reutilizado para errores no-handshake
- **Archivo:** `crates/tessera-protocol/src/bolt_client.rs:146-151`
- **Qué:** IGNORED y RECORD en hello() usan `BoltInvalidHandshake` — semántica incorrecta.
- **Fix:** Crear `BoltUnexpectedResponse { context: &'static str }` variant.

### R4: `Session` wrapper sin encapsulación
- **Archivo:** `crates/tessera-cli/src/connection.rs`
- **Qué:** `pub client: BoltClient<R, W>` — field público, no añade valor sobre usar BoltClient directamente.
- **Fix:** O darle métodos que deleguen (y hacer client private), o eliminar Session.

### R5: Zero tests para error mapping en auth.rs y query.rs
- **Archivos:** `crates/tessera-cli/src/auth.rs`, `crates/tessera-cli/src/query.rs`
- **Qué:** La conversión `BoltAuthFailure → CliError::Auth` (exit code 2) vs `BoltQueryFailure → CliError::Query` (exit code 3) no está testeada.
- **Fix:** Tests unitarios con mock errors para verificar los exit codes.

---

## Mejoras Opcionales (tracked)

### O1: Password no zeroized en buffer PackStream
- **Archivo:** `bolt_client.rs:129`, `main.rs:97-100`
- **Fix:** `Zeroizing<Vec<u8>>` para el encode buffer después del write.

### O2: `connect` free function exportada sin uso
- **Archivo:** `bolt_client.rs:232-237`, `lib.rs:24`
- **Fix:** Eliminar export o documentar como API pública intencional.

### O3: `QueryResult` y `QueryOutput` duplicados
- **Archivos:** `bolt_client.rs:19-24`, `query.rs:13-16`
- **Fix:** Type alias `pub type QueryOutput = tessera_protocol::QueryResult;` o eliminar QueryOutput.

### O4: `_language` param permanentemente unused
- **Archivo:** `query.rs:32`
- **Fix:** Wire through to RUN extra dict o eliminar param.

### O5: `db` in HELLO es extensión tessera no-estándar
- **Archivo:** `bolt_client.rs:133-135`
- **Fix:** Documentar como extensión o mover a RUN extra.

### O6: `format_as_gql` no documenta limitación para Struct/List/Dict values
- **Archivo:** `export.rs:8-46`
- **Fix:** Añadir doc comment sobre non-scalar values.

---

## Métricas

- Archivos revisados: 11
- Tests totales: 958 (todos pasan)
- Clippy: 0 warnings
- Hallazgos totales: 24 (2 critical, 2 high, 7 medium, 5 low, 8 info)
- Hallazgos bloqueantes: 4 (C1, C2, H1, H2)
