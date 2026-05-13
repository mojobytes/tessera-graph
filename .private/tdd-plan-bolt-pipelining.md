# TDD Plan: Bolt RUN+PULL Pipelining

## Contexto

La CLI de importación ejecuta 466 CREATE ops/s vía Bolt frente a 8,017 ops/s en proceso (overhead 17x) y 1,600 ops/s en Memgraph (3.4x más lento que Memgraph). La causa raíz es que cada sentencia CREATE necesita 4 round-trips TCP+TLS: flush-y-espera tras RUN, flush-y-espera tras PULL. El protocolo Bolt garantiza que el servidor procesa los mensajes en el orden de llegada y responde en el mismo orden, por lo que es válido enviar RUN+PULL sin esperar la respuesta intermedia — esto reduce los round-trips de 4 a 2 y debe aproximar o superar los 1,600 ops/s de Memgraph.

La implementación toca exactamente tres capas: `BoltChunkedWriter` (MIT core, capa de framing), `BoltClient` (MIT core, capa de protocolo), y el loop de importación de la CLI (Enterprise). El servidor no necesita ningún cambio.

**Stack detectado**: Rust / Tokio async
**Convenciones**: `async fn`, generics `<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>`, tests en `tests/` separados del src, errores via `ProtocolError` tipado, doc-comments `///` en toda API pública, `#[must_use]` en constructores
**Afecta hot path**: SÍ — la ruta `execute_query` → `run_query` es el cuello de botella directo de insert throughput

---

## Decisiones Previas Necesarias

Ninguna. La arquitectura está clara: pipelining es válido en Bolt 4.x por especificación, el servidor ya procesa mensajes en orden, y la capa de framing ya expone `flush()` separado.

---

## Plan de Ejecución

### Fase 1: Feature branch en MIT core

1. [ ] Crear rama `feature/bolt-run-pull-pipelining` desde `develop` en el repo MIT core (20 min)
   - Directorio: `/Volumes/WD_BLACK/repos/MojoBytes/tessera-ecosystem/tessera-graph/`
   - Acción: `git checkout develop && git pull && git checkout -b feature/bolt-run-pull-pipelining`
   - Output: rama activa en el repo MIT

### Fase 2: Layer 1 — `write_message_no_flush` en BoltChunkedWriter (TDD)

2. [ ] RED — escribir test que falla: `write_message_no_flush` escribe bytes correctos pero NO hace flush hasta llamar `flush()` manualmente (20 min)
   - Archivo: `crates/tessera-graph-protocol/tests/bolt_frame_test.rs`
   - Acción: Agregar al final del archivo dos tests:
     - `writer_no_flush_produces_correct_wire_bytes` — escribe sobre `Vec<u8>` (que ignora flush), verifica bytes idénticos a `write_message`
     - `writer_two_messages_no_flush_then_flush_roundtrip` — usa `make_pair`, llama `write_message_no_flush` dos veces seguidas (sin flush intermedio) y después `flush()`, lector recibe ambos mensajes en orden
   - Output: `cargo test -p tessera-graph-protocol -- writer_no_flush` falla con "method not found"

3. [ ] GREEN — implementar `write_message_no_flush` en `BoltChunkedWriter` (15 min)
   - Archivo: `crates/tessera-graph-protocol/src/bolt_frame.rs`
   - Acción: Agregar método público `write_message_no_flush` justo después de `write_message`. La implementación es idéntica a `write_message` pero omite la llamada `self.inner.flush().await?`.
   - Output: `cargo test -p tessera-graph-protocol -- writer_no_flush` pasa, `cargo test -p tessera-graph-protocol` pasa completo sin warnings

### Fase 3: Layer 2 — `send_request_no_flush` y `run_query_pipelined` en BoltClient (TDD)

4. [ ] RED — escribir tests para `run_query_pipelined` (20 min)
   - Archivo: `crates/tessera-graph-protocol/tests/bolt_client_test.rs`
   - Acción: Agregar función `mock_server_pipelined` que:
     - Completa handshake + HELLO
     - Lee **dos** mensajes consecutivos del wire (drain_one_message x2) **antes** de escribir cualquier respuesta — esto verifica que cliente envió ambos sin esperar
     - Escribe SUCCESS{fields:["x"]} (respuesta RUN), luego Record{[Int(1)]}, luego SUCCESS{} (respuestas PULL)
   - Agregar tests:
     - `run_query_pipelined_success_collects_rows` — verifica `columns == ["x"]`, `rows.len() == 1`
     - `run_query_pipelined_run_failure_returns_error` — mock responde FAILURE a RUN
     - `run_query_pipelined_run_ignored_returns_connection_ignored` — mock responde IGNORED a RUN
   - Output: todos fallan con "method not found"

5. [ ] GREEN — implementar `send_request_no_flush` y `run_query_pipelined` en `BoltClient` (25 min)
   - Archivo: `crates/tessera-graph-protocol/src/bolt_client.rs`
   - Acción:
     - Agregar método privado `send_request_no_flush` que llama `write_message_no_flush`
     - Agregar método público `run_query_pipelined` con misma firma que `run_query`:
       1. `send_request_no_flush(Run{...})`
       2. `send_request(Pull{...})` — flush empuja ambos mensajes
       3. Lee respuesta de RUN
       4. Lee respuestas de PULL
   - Output: `cargo test -p tessera-graph-protocol` pasa completo

6. [ ] REFACTOR — eliminar duplicación entre `run_query` y `run_query_pipelined` (15 min)
   - Extraer lógica de lectura a funciones privadas `read_run_response` y `read_pull_responses`
   - Output: tests siguen pasando, sin warnings

### Fase 4: Feature branch en Enterprise

7. [ ] Crear rama `feature/bolt-run-pull-pipelining` desde la rama activa en Enterprise (10 min)

### Fase 5: Layer 3 — `execute_query_pipelined` en CLI (TDD)

8. [ ] RED — test para `execute_query_pipelined` (20 min)
   - Archivo: `crates/tessera-graph-cli/tests/query_pipelined_test.rs`
   - Acción: test con Session sobre duplex + mock server, llama `execute_query_pipelined`, verifica Ok

9. [ ] GREEN — implementar `execute_query_pipelined` en `query.rs` (15 min)
   - Llama `session.client.run_query_pipelined(query)` en lugar de `run_query`

### Fase 6: Wiring en el loop de importación

10. [ ] Modificar import loop para usar pipelining (20 min)
    - En `handle_import`, cambiar `execute_query` por `execute_query_pipelined`
    - Output: compila sin warnings

11. [ ] Verificar compilación completa del workspace Enterprise (5 min)

### Fase 7: Tests de rendimiento

12. [ ] Medir baseline ANTES del pipelining (20 min)
13. [ ] Agregar test de regresión de throughput (25 min)
    - Test `pipelined_create_throughput_regression_guard` con N=500 CREATEs
    - Assert: `ops_per_sec > 800.0`
14. [ ] Medir throughput POST pipelining contra servidor real (10 min)
    - Criterio: throughput >= 900 ops/s (objetivo: duplicar los 466 actuales)

---

## Estimación Total: ~4 horas

## Criterios de Éxito

- [ ] `write_message_no_flush` escribe bytes idénticos sin flush
- [ ] `run_query_pipelined` envía RUN y PULL en el mismo flush antes de leer respuesta
- [ ] `run_query` existente sin modificar — todos sus tests previos pasan
- [ ] Loop de importación usa pipelining sin cambiar timeout, error recovery ni RESET
- [ ] `cargo test --workspace` (Enterprise) sin errores
- [ ] `cargo test -p tessera-graph-protocol` sin errores
- [ ] Throughput de CREATE via Bolt >= 900 ops/s
- [ ] Test de regresión de throughput en la suite

### Wiring Checklist

- [ ] `write_message_no_flush` tiene call site en `send_request_no_flush`
- [ ] `send_request_no_flush` tiene call site en `run_query_pipelined`
- [ ] `run_query_pipelined` tiene call site en `execute_query_pipelined`
- [ ] `execute_query_pipelined` tiene call site en `handle_import`
- [ ] No quedan referencias stale a `execute_query` en el import path
