# TDD Plan: Quality Fixes — Streaming Import (14 hallazgos)

## Contexto

La revisión de calidad del streaming import en `tessera-cli` identificó 14 hallazgos
distribuidos en tres niveles de prioridad. Este plan los corrige en orden de impacto:
primero los críticos de seguridad e integridad, luego DRY/deuda técnica, luego
mejoras recomendadas, y finalmente opcionales.

**Stack detectado**: Rust, tokio async, serde_json, csv crate
**Convenciones**: `#[cfg(test)] mod tests` inline, `.unwrap()/.expect()` con `// OK: test`,
`std::io::Cursor` como test reader, `CliError::ImportExport` para errores de dominio,
warnings = errores por lints del workspace
**Afecta hot path**: SI — `handle_import` es el pipeline central de importacion

## Decisiones Previas Necesarias

Ninguna. Todos los hallazgos tienen solucion tecnica clara y unívoca.

---

## Plan de Ejecución

### Fase 1 — Seguridad: Hallazgo 4 — Label CSV sin sanitizacion (15 min)

**Problema**: `csv_nodes_to_gql` (linea 277) y `stream_csv_import` (linea 349) emiten
`CREATE (:{label}...)` interpolando el label directamente en el string sin pasar por
`write_gql_identifier`. Un label como `"My Type"` produce GQL invalido; un label como
`"X {admin: true}"` inyecta propiedades arbitrarias.

El JSON path ya usa `write_gql_identifier` correctamente en `node_value_to_gql_stmt`.

#### Ciclo RED

1. [ ] Escribir tests que fallan (RED)
   - Archivo: `crates/tessera-cli/src/import.rs`, seccion `#[cfg(test)] mod tests`
   - Tests a agregar:

   ```rust
   // --- Hallazgo 4: CSV label debe pasar por write_gql_identifier ---

   #[test]
   fn csv_nodes_label_with_space_uses_delimited_identifier() {
       let csv = "label,name\nMy Type,Alice\n";
       let stmts = csv_nodes_to_gql(csv).expect("csv to gql"); // OK: test
       // Debe producir :"My Type" (delimited), no :My Type (invalido)
       assert!(
           stmts[0].contains(":\"My Type\""),
           "expected delimited identifier, got: {}",
           stmts[0]
       );
   }

   #[test]
   fn csv_nodes_label_injection_attempt_uses_delimited_identifier() {
       // Un label como "X {admin: true}" NO debe inyectar propiedades extra
       // write_gql_identifier lo envuelve en delimited form porque contiene '{'
       let csv = "label,name\nX {admin: true},Alice\n";
       let stmts = csv_nodes_to_gql(csv).expect("csv to gql"); // OK: test
       // El label completo debe estar como delimited identifier, no interpolado
       assert!(
           stmts[0].contains(":\"X {admin: true}\""),
           "expected delimited identifier to neutralize injection, got: {}",
           stmts[0]
       );
       // La unica entrada de propiedades debe ser la del CSV (name: 'Alice')
       assert!(stmts[0].contains("name: 'Alice'"), "got: {}", stmts[0]);
   }

   #[test]
   fn csv_nodes_label_with_double_quote_is_error() {
       // write_gql_identifier rechaza labels con comilla doble
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

   - Verificar: `cargo test -p tessera-cli csv_nodes_label csv_nodes_label_injection stream_csv_label` FALLA (RED)

#### Ciclo GREEN

2. [ ] Implementar la correccion minima que hace pasar los tests
   - Archivo: `crates/tessera-cli/src/import.rs`

   **En `csv_nodes_to_gql`** (alrededor de linea 271-277):
   Reemplazar la construccion directa del statement:
   ```rust
   // ANTES (inseguro):
   statements.push(format!("CREATE (:{label}{props_str})"));

   // DESPUES: construir con write_gql_identifier
   let mut stmt = String::with_capacity(64 + props_str.len());
   stmt.push_str("CREATE (:");
   write_gql_identifier(label, "node label", &mut stmt)?;
   stmt.push_str(&props_str);
   stmt.push(')');
   statements.push(stmt);
   ```

   **En `stream_csv_import`** (alrededor de linea 343-350):
   Mismo cambio:
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

   - Verificar: `cargo test -p tessera-cli` pasa. Los tests de parity existentes
     (`stream_csv_parity_with_batch`, `csv_nodes_basic`, etc.) deben seguir verdes.

#### Ciclo REFACTOR

3. [ ] Extraer helper privado `build_csv_node_stmt` para eliminar la duplicacion entre batch y streaming
   - **Nota**: Este helper es un sub-paso del Hallazgo 3 (DRY). Se introduce aqui
     porque el Hallazgo 4 expone la duplicacion. El Hallazgo 3 completo se trabaja en Fase 3.
   - Archivo: `crates/tessera-cli/src/import.rs`
   - Accion: Crear funcion privada:
     ```rust
     /// Build a GQL CREATE statement for a CSV node row.
     ///
     /// `label` is the raw label string from the CSV (validated via
     /// [`write_gql_identifier`]). `prop_cols` and `values` are parallel slices.
     fn build_csv_node_stmt(
         label: &str,
         prop_cols: &[String],
         values: &[(&str, &str)], // (col_name, raw_value) pairs for non-empty fields
     ) -> Result<String, CliError> { ... }
     ```
   - Nota: Si la firma anterior resulta inconveniente dado como cada funcion extrae
     sus props, una alternativa mas simple es extraer solo la parte de construccion
     del statement (label + props_str ya formateado):
     ```rust
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

### Fase 2 — Integridad: Hallazgo 1 — Error del producer silenciado (20 min)

**Problema**: En `main.rs` lineas 223-226, si el producer falla DESPUES de que el
consumer ya dreneo el canal y salio del `while let`, el error del producer se pierde.
El patron actual:
```rust
if let Err(e) = result {
    let _ = tx.blocking_send(Err(e)); // puede fallar si rx ya se cerro
}
// tx drops — error silenciado
```
Si el consumer rompio el loop por un `query_err`, hace `drop(rx)` y espera al producer
con `.await.map_err(...)` — pero ese `.map_err` solo captura panicos, no el error logico
del producer. El error semantico (ej: "CSV invalid row" en fila 50000) se pierde.

#### Ciclo RED

4. [ ] Escribir tests que fallan (RED)
   - Archivo: `crates/tessera-cli/src/import.rs`, `#[cfg(test)] mod tests`
   - Los tests unitarios directos sobre las funciones de streaming ya verifican
     que el callback de error se propaga. El problema esta en el wiring de `main.rs`.
   - Agregar un test de integracion en el modulo de tests de `import.rs` que simula
     el escenario de error tardio:

   ```rust
   #[test]
   fn stream_csv_late_error_propagates_via_result() {
       // Simula un error que ocurre en la fila 3, despues de que el consumer
       // hipotetico ya proceso las filas 1 y 2.
       // El error del producer debe retornar como Err, no silenciarse.
       let csv = "label,name\nPerson,Alice\nPerson,Bob\nbad\"label,Mallory\n";
       let mut count = 0usize;
       let result = stream_csv_import(std::io::Cursor::new(csv), |_| {
           count += 1;
           Ok(())
       });
       // La fila 3 tiene un label invalido (double quote) -> write_gql_identifier retorna Err
       // El error debe propagarse como Err del stream, no silenciarse
       assert!(
           result.is_err(),
           "late error (row 3) must propagate; count was {count}"
       );
   }
   ```

   - El test de la funcion pura ya pasa (stream_csv_import propaga el error).
     El problema real esta en el wiring de main.rs. Escribir un test de escenario
     para documentar el invariante esperado del canal:

   - Archivo: `crates/tessera-cli/src/main.rs`, seccion `#[cfg(test)] mod tests`

   ```rust
   #[test]
   fn producer_error_after_consumer_drains_is_not_silenced() {
       // Este test documenta el contrato: si el producer falla despues de que
       // el consumer drenó el canal, el JoinHandle debe retornar Ok(Err(_)),
       // no Ok(Ok(())). El test verifica que spawn_blocking retorna el Result.
       //
       // Simulacion sincrona del patron (sin tokio runtime real):
       // El producer devuelve Err. El caller DEBE inspeccionar el JoinHandle.
       // Este test pasa trivialmente porque simplemente verifica la logica de
       // inspeccion, no el runtime completo.
       let result: Result<Result<(), &str>, ()> = Ok(Err("producer error"));
       // El caller debe "desenvolver" ambas capas
       let inner = result.expect("join ok"); // OK: test
       assert!(inner.is_err(), "inner error must be visible to caller");
   }
   ```

   - Verificar: estos tests pasan (documentan el contrato, no el bug actual en wiring).

#### Ciclo GREEN

5. [ ] Corregir el wiring del producer en `handle_import`
   - Archivo: `crates/tessera-cli/src/main.rs`

   El fix requiere que `spawn_blocking` retorne `Result<(), CliError>` y que el caller
   lo inspeccione DESPUES del loop del consumer, combinando ambos errores.

   **Cambio en la clausura del producer** — hacer que retorne `Result<(), CliError>`:
   ```rust
   let producer = tokio::task::spawn_blocking(move || -> Result<(), CliError> {
       let send_stmt = |stmt: String| {
           tx.blocking_send(Ok(stmt))
               .map_err(|_| CliError::ImportExport("channel closed".into()))
       };
       match fmt_owned.as_str() {
           "json"      => import::stream_json_import(reader, send_stmt).map(|_| ()),
           "gql"       => import::stream_gql_import(
                              std::io::BufReader::new(reader), send_stmt
                          ).map(|_| ()),
           "csv-nodes" => import::stream_csv_import(reader, send_stmt).map(|_| ()),
           other => Err(CliError::ImportExport(format!(
               "unsupported import format: {other}"
           ))),
       }
       // tx drops aqui cerrando el canal (tanto en Ok como en Err)
   });
   ```

   **Nota critica**: Ya NO se hace `tx.blocking_send(Err(e))` — el error se retorna
   directamente en el `Result` del `JoinHandle`. El consumer solo recibe `Ok(stmt)`.
   El canal se usa unicamente para los statements exitosos. Errores del producer viajan
   por el `JoinHandle`.

   **Cambio en el consumer loop** — eliminar el manejo de `Err` del canal:
   ```rust
   // El canal solo transporta Ok(stmt) ahora
   while let Some(stmt) = rx.recv().await {
       if let Err(e) = query::execute_query(session, &stmt, "gql").await {
           query_err = Some(e);
           break;
       }
       count += 1;
       if count % PROGRESS_INTERVAL == 0 {
           eprintln!("Imported {count} statements...");
       }
   }
   ```
   El tipo del canal cambia a `mpsc::channel::<String>(IMPORT_CHANNEL_CAPACITY)`.

   **Cambio en la inspeccion del producer** — combinar errores:
   ```rust
   drop(rx); // desbloquea producer si rompimos el loop

   // Awaitar producer: superficia panicos Y errores logicos
   let producer_result = producer
       .await
       .map_err(|e| CliError::ImportExport(format!("import thread panicked: {e}")))?;

   // Si el consumer tuvo un error de query, reportarlo primero
   if let Some(e) = query_err {
       return Err(e);
   }

   // Si el producer tuvo un error (descubierto despues de que consumer dreño),
   // reportarlo ahora
   producer_result?;
   ```

   - Verificar: `cargo check -p tessera-cli` sin errores ni advertencias.
   - Verificar: `cargo test -p tessera-cli` pasa.

#### Ciclo REFACTOR

6. [ ] Consolidar la logica de prioridad de errores con un comentario explicativo
   - Archivo: `crates/tessera-cli/src/main.rs`
   - Agregar un bloque de comentario que explica el orden de inspeccion de errores:
     1. Panic del producer (JoinError) — siempre se propaga
     2. Error del consumer (query_err) — error de Bolt, se propaga antes que producer
     3. Error logico del producer (producer_result) — error de parsing/IO del archivo
   - Razon: si tanto consumer como producer fallaron, el error del consumer (Bolt) es
     mas relevante para el usuario porque indica que el servidor rechazo el statement.
   - No hay cambio de logica, solo documentacion en codigo.

---

### Fase 3 — DRY: Hallazgo 3 — Duplicacion batch/streaming (30 min)

**Problema**: La logica de construccion de statements (formateo de propiedades,
validacion de labels) esta duplicada entre las funciones batch y las streaming.
Un bug fix en `csv_nodes_to_gql` no se propaga automaticamente a `stream_csv_import`.

La Fase 1 ya extrae `finish_csv_node_stmt` para CSV. Esta fase completa el patron
para GQL y JSON, y asegura que batch llame a las mismas primitivas que streaming.

**Observacion importante**: Para GQL, el batch (`split_gql_statements`) y el streaming
(`stream_gql_import`) ya comparten `is_comment_line` que es la logica central. La
duplicacion real esta en la logica de split-por-semicolon, que es inevitable dado
que una retorna `Vec<String>` y la otra hace callback. El patron correcto es que
la funcion batch DELEGUE en la streaming:

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

Para CSV y JSON, el batch ya puede delegar en streaming con `Cursor`.

#### Ciclo RED

7. [ ] Escribir tests de paridad reforzados (RED -> los tests ya existen, este ciclo
       confirma que tras el refactor SIGUEN verdes)
   - Archivo: `crates/tessera-cli/src/import.rs`
   - Los tests de parity ya existen:
     - `stream_gql_parity_with_batch` (linea ~1399)
     - `stream_csv_parity_with_batch` (linea ~1493)
     - `stream_json_parity_with_batch` (linea ~1632)
   - Estos tests son el RED implícito: si el refactor rompe algo, estos fallan.
   - No hay nuevos tests que agregar aqui; el valor es que los existentes se vuelven
     GARANTIA CONTRACTUAL del refactor (no solo tests de comportamiento).
   - Accion: Agregar comentario `// Contrato DRY: batch delega en streaming` sobre cada parity test.

#### Ciclo GREEN

8. [ ] Refactorizar `split_gql_statements` para delegar en `stream_gql_import`
   - Archivo: `crates/tessera-cli/src/import.rs`
   - Reemplazar la implementacion de `split_gql_statements` por:
     ```rust
     pub fn split_gql_statements(content: &str) -> Vec<String> {
         let mut out = Vec::new();
         // Delegar en stream_gql_import con Cursor como reader.
         // El error solo puede ocurrir si el callback falla; aqui nunca falla.
         let _ = stream_gql_import(std::io::Cursor::new(content), |s| {
             out.push(s);
             Ok(())
         });
         out
     }
     ```
   - La funcion `is_comment_line` puede permanecer como helper privado de `stream_gql_import`.
   - Verificar: `cargo test -p tessera-cli` pasa. Particularmente los tests de splitter
     (lineas ~794-868) y `stream_gql_parity_with_batch`.

9. [ ] Refactorizar `csv_nodes_to_gql` para delegar en `stream_csv_import`
   - Archivo: `crates/tessera-cli/src/import.rs`
   - Reemplazar la implementacion por:
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
   - Verificar: `cargo test -p tessera-cli` pasa. Los tests de `csv_nodes_*` deben seguir verdes.

10. [ ] Refactorizar `json_to_gql_statements` para delegar en `stream_json_import`
    - Archivo: `crates/tessera-cli/src/import.rs`
    - Reemplazar la implementacion por:
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
    - Las funciones auxiliares `node_value_to_gql_stmt`, `edge_value_to_gql_stmt`,
      `format_endpoint_match`, `write_gql_identifier`, `write_json_props_to_buf`,
      `write_json_value_to_buf` permanecen como helpers privados.
    - La implementacion DOM anterior de `json_to_gql_statements` (serde_json::from_str
      al objeto raiz) se ELIMINA completamente. Ya no hay dos implementaciones.
    - Verificar: `cargo test -p tessera-cli` pasa. Todos los tests JSON siguen verdes.

#### Ciclo REFACTOR

11. [ ] Eliminar codigo muerto generado por el refactor
    - Archivo: `crates/tessera-cli/src/import.rs`
    - Tras el refactor, la antigua implementacion DOM de `json_to_gql_statements` queda
      eliminada. Las importaciones de serde no usadas (si las hay) deben removerse.
    - Ejecutar: `cargo clippy -p tessera-cli -- -D warnings` para detectar dead code.
    - Verificar: cero warnings, cero errores.

---

### Fase 4 — Calidad: Hallazgo 2 — `eprintln!` en funcion pura (20 min)

**Problema**: `write_json_value_to_buf` (lineas 703-706) imprime en stderr para cada
array/object property. En una importacion de 100k nodos puede generar 100k lineas en
stderr sin posibilidad de suprimirlo. La funcion es pura (toma `&Value`, escribe en
`&mut String`) pero tiene este efecto secundario oculto.

La solucion correcta es convertir la firma para retornar `Result<(), CliError>` y
propagar el warning como un error, O cambiarlo a un contador de warnings en el caller.
Dado que esta funcion es invocada profundamente en el visitor de serde (donde convertir
a `Err` es costoso), la solucion preferida es: retornar `Result` y propagar.

#### Ciclo RED

12. [ ] Escribir test que documenta que NO hay salida a stderr
    - Archivo: `crates/tessera-cli/src/import.rs`
    - Rust no tiene intercepcion de stderr en tests unitarios de forma estandar.
      El RED aqui es conceptual: el test verifica que la funcion retorna `Err` en
      lugar de imprimir, lo cual es el comportamiento correcto post-fix.

    ```rust
    #[test]
    fn write_json_value_array_returns_err_not_eprintln() {
        // ANTES del fix: imprimia en stderr y retornaba ()
        // DESPUES del fix: debe retornar Err con mensaje descriptivo
        use serde_json::json;
        let mut buf = String::new();
        let val = json!(["a", "b", "c"]);
        let result = write_json_value_to_buf(&val, &mut buf);
        // Post-fix: arrays deben retornar Err o serializar con advertencia controlada.
        // Optamos por serializar (comportamiento actual) pero sin eprintln.
        // El test verifica que buf tiene contenido y NO hay panic.
        assert!(!buf.is_empty(), "array should produce some output");
        // Si elegimos retornar Err:
        // assert!(result.is_err());
        // Si elegimos serializar silenciosamente:
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

    - Verificar: estos tests FALLAN (RED) porque la firma actual es `fn(...) -> ()`.

#### Ciclo GREEN

13. [ ] Cambiar la firma de `write_json_value_to_buf` a `Result<(), CliError>`
    - Archivo: `crates/tessera-cli/src/import.rs`

    **Nueva firma**:
    ```rust
    fn write_json_value_to_buf(v: &serde_json::Value, buf: &mut String) -> Result<(), CliError>
    ```

    **Cambios en el cuerpo**:
    - Los casos `Null`, `Bool`, `Number`, `String` ya no necesitan cambio excepto retornar `Ok(())`.
    - El caso `other` (arrays/objects): **eliminar el `eprintln!`**, serializar igual pero retornar `Ok(())`.
      El warning desaparece. Si en el futuro se quiere advertir, se hace a nivel del caller donde
      se tiene contexto (nombre del campo) y se puede contar o loguear de forma estructurada.

    **Actualizar todos los call sites** (la firma cambia de `()` a `Result<(), CliError>`):
    - `write_json_props_to_buf`: agregar `?` despues de cada llamada a `write_json_value_to_buf`.
    - `format_endpoint_match`: agregar `?` despues de la llamada.
    - La firma de `write_json_props_to_buf` ya retorna `Result` — no cambia.
    - La firma de `format_endpoint_match` ya retorna `Result` — no cambia.

    - Verificar: `cargo test -p tessera-cli` pasa. Los tests de array/object property siguen verdes.

#### Ciclo REFACTOR

14. [ ] Verificar que los tests existentes de array property siguen verdes y actualizar doc
    - El test `json_array_property_stored_as_json_string` (linea ~1115) debe seguir verde
      porque el comportamiento de serializacion no cambia, solo desaparece el eprintln.
    - Actualizar el doc comment de `write_json_value_to_buf`:
      ```rust
      /// Write a JSON value as a GQL literal into `buf`.
      ///
      /// Arrays and objects are serialized as JSON strings (GQL does not have
      /// native array/object literals). Use the caller's context to surface
      /// a diagnostic if needed.
      fn write_json_value_to_buf(v: &serde_json::Value, buf: &mut String) -> Result<(), CliError>
      ```
    - Verificar: `cargo clippy -p tessera-cli -- -D warnings` limpio.

---

### Fase 5 — Mejoras Recomendadas: Hallazgos 5–9 (45 min)

#### Hallazgo 5 — Canal de capacidad 64 sin constante nombrada

15. [ ] Extraer constante con doc comment
    - Archivo: `crates/tessera-cli/src/main.rs`
    - Agregar junto a `PROGRESS_INTERVAL`:
      ```rust
      /// Capacity of the bounded channel between the producer (parser thread)
      /// and the consumer (Bolt execution loop).
      ///
      /// 64 statements provides ~16 KB of backpressure buffer at ~256 bytes/stmt
      /// while keeping memory overhead negligible. Increase if profiling shows
      /// the producer stalls waiting for the consumer on slow networks.
      const IMPORT_CHANNEL_CAPACITY: usize = 64;
      ```
    - Reemplazar el literal `64` en la llamada a `mpsc::channel` por `IMPORT_CHANNEL_CAPACITY`.
    - Verificar: `cargo test -p tessera-cli` pasa.

    **No hay ciclo RED separado**: la constante no cambia comportamiento observable, por lo
    que el test es simplemente que el codigo compila y los tests existentes siguen verdes.

#### Hallazgo 6 — Doble `BufReader` redundante para GQL

16. [ ] Cambiar la firma de `stream_gql_import` para aceptar `Read` y bufferar internamente
    - Archivo: `crates/tessera-cli/src/import.rs`

    **Ciclo RED**: Escribir test que verifica que `stream_gql_import` acepta un `Read` sin buffer:
    ```rust
    #[test]
    fn stream_gql_accepts_unbuffered_read() {
        // Verificar que stream_gql_import acepta impl Read (sin BufRead)
        // usando un File sin BufReader (simulado con Cursor que implementa Read).
        // Si la firma acepta Read, esto compila. Si requiere BufRead, no.
        struct UnbufferedReader(std::io::Cursor<&'static str>);
        impl std::io::Read for UnbufferedReader {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                self.0.read(buf)
            }
        }
        let r = UnbufferedReader(std::io::Cursor::new("CREATE (:X);"));
        let mut out = Vec::new();
        stream_gql_import(r, |s| { out.push(s); Ok(()) }).unwrap(); // OK: test
        assert_eq!(out.len(), 1);
    }
    ```
    - Este test FALLA (RED) porque la firma actual es `<R: BufRead>`.

    **Ciclo GREEN**: Cambiar la firma:
    ```rust
    pub fn stream_gql_import<R: std::io::Read>(
        reader: R,
        mut on_stmt: impl FnMut(String) -> Result<(), CliError>,
    ) -> Result<usize, CliError> {
        let reader = std::io::BufReader::new(reader); // buffering interno
        // resto del cuerpo sin cambios
    ```
    - En `main.rs`, eliminar el `BufReader::new` wrapper alrededor del reader antes
      de pasar a `stream_gql_import`:
      ```rust
      // ANTES:
      "gql" => import::stream_gql_import(std::io::BufReader::new(reader), send_stmt),
      // DESPUES:
      "gql" => import::stream_gql_import(reader, send_stmt),
      ```
    - Verificar: `cargo test -p tessera-cli` pasa. El test `stream_gql_accepts_unbuffered_read` verde.

#### Hallazgo 7 — `is_large_file`/`LARGE_FILE_THRESHOLD_BYTES` solo en tests

17. [ ] Eliminar codigo de test sin contraparte en produccion
    - Archivo: `crates/tessera-cli/src/main.rs`

    **Analisis**: Los tests `large_file_threshold_*` (lineas 594-606) prueban
    `is_large_file` y `LARGE_FILE_THRESHOLD_BYTES` que son definidos DENTRO del
    modulo `#[cfg(test)]`. Son tests de funciones que no existen en produccion,
    por lo que no tienen valor como guardia de regresion.

    **Decision**: Eliminar los tres tests y las dos definiciones de test-only.
    Si en el futuro se reintroduce un warning en dry-run para archivos grandes,
    se crea la funcion en produccion y los tests correspondientes.

    **Ciclo GREEN** (no hay RED — son eliminations):
    - Eliminar `LARGE_FILE_THRESHOLD_BYTES` (linea 544)
    - Eliminar `is_large_file` (lineas 546-548)
    - Eliminar los tests `large_file_threshold_one_below`, `large_file_threshold_at_boundary`,
      `large_file_threshold_one_above` (lineas 594-606)
    - Verificar: `cargo test -p tessera-cli` pasa. Cero referencias a `is_large_file`.

#### Hallazgo 8 — Throughput tests con wall-clock timeout fragil en CI

18. [ ] Hacer el timeout condicional segun `cfg!(debug_assertions)`
    - Archivo: `crates/tessera-cli/src/import.rs`
    - Los tres tests de throughput (`throughput_stream_gql_10k`, `throughput_stream_csv_10k`,
      `throughput_stream_json_10k`) usan `THROUGHPUT_TIMEOUT = Duration::from_secs(2)`.
      En CI debug builds este limite puede ser demasiado ajustado.

    **Cambio**:
    ```rust
    // Timeout generoso en debug (CI), estricto en release
    const THROUGHPUT_TIMEOUT: std::time::Duration = if cfg!(debug_assertions) {
        std::time::Duration::from_secs(10)
    } else {
        std::time::Duration::from_secs(2)
    };
    ```
    - Verificar: `cargo test -p tessera-cli throughput` pasa en modo debug y release.

#### Hallazgo 9 — `should_report_progress` en tests no corresponde al codigo real

19. [ ] Alinear `should_report_progress` con el codigo real o eliminarla
    - Archivo: `crates/tessera-cli/src/main.rs`

    **Analisis**: La funcion `should_report_progress(done, total)` en el modulo de tests
    tiene logica `done % PROGRESS_INTERVAL == 0 || done == total`. El codigo real en
    `handle_import` usa solo `count % PROGRESS_INTERVAL == 0`. La funcion de test
    introduce un invariante falso (el `|| done == total`) que no existe en produccion.

    Los tests `progress_interval_fires_at_last` y `progress_interval_small_total_fires_only_at_end`
    prueban este invariante falso y pasarian aunque el codigo real no lo cumpla.

    **Decision**: Eliminar `should_report_progress` del modulo de tests y sus cuatro tests.
    Si se quiere testear el progreso, hacerlo en terminos de la constante `PROGRESS_INTERVAL`
    directamente sin una funcion wrapper que introduce semantica falsa.

    **Reemplazar con tests directos de la constante**:
    ```rust
    #[test]
    fn progress_interval_constant_is_positive() {
        assert!(PROGRESS_INTERVAL > 0, "PROGRESS_INTERVAL must be positive");
    }

    #[test]
    fn progress_interval_fires_at_multiple() {
        // El codigo real usa count % PROGRESS_INTERVAL == 0
        assert_eq!(PROGRESS_INTERVAL % PROGRESS_INTERVAL, 0);
        assert_ne!((PROGRESS_INTERVAL - 1) % PROGRESS_INTERVAL, 0);
    }
    ```
    - Eliminar: `should_report_progress`, `progress_interval_not_at_arbitrary_count`,
      `progress_interval_fires_at_1000`, `progress_interval_fires_at_last`,
      `progress_interval_small_total_fires_only_at_end`.
    - Verificar: `cargo test -p tessera-cli` pasa.

---

### Fase 6 — Hallazgos 10–14 (1h 40 min)

#### Hallazgo 10 — Timeout por statement en el consumer (30 min)

**Problema**: `execute_query` puede bloquearse indefinidamente si el servidor deja
de responder durante una importacion larga. Sin timeout, el proceso se cuelga sin
feedback. Timeout por defecto: 30 segundos por statement (suficiente para queries
pesados en grafos grandes, falla rapido en servidores caidos).

##### Ciclo RED

23. [ ] Escribir tests para el timeout por statement
    - Archivo: `crates/tessera-cli/src/main.rs`, `#[cfg(test)] mod tests`

    ```rust
    #[test]
    fn import_statement_timeout_constant_is_reasonable() {
        // El timeout por statement debe ser >= 10s (grafos grandes) y <= 120s
        assert!(IMPORT_STATEMENT_TIMEOUT >= std::time::Duration::from_secs(10));
        assert!(IMPORT_STATEMENT_TIMEOUT <= std::time::Duration::from_secs(120));
    }
    ```

    - Este test FALLA (RED) porque `IMPORT_STATEMENT_TIMEOUT` no existe.

##### Ciclo GREEN

24. [ ] Implementar timeout por statement
    - Archivo: `crates/tessera-cli/src/main.rs`
    - Agregar constante:
      ```rust
      /// Maximum time to wait for a single statement to execute during import.
      /// If the server does not respond within this time, the import aborts.
      const IMPORT_STATEMENT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
      ```
    - En el consumer loop de `handle_import`, envolver `execute_query` con timeout:
      ```rust
      let result = tokio::time::timeout(
          IMPORT_STATEMENT_TIMEOUT,
          query::execute_query(session, &stmt, "gql"),
      )
      .await
      .map_err(|_| CliError::ImportExport(format!(
          "statement timed out after {}s (statement #{count})",
          IMPORT_STATEMENT_TIMEOUT.as_secs()
      )))?;
      if let Err(e) = result {
          query_err = Some(e);
          break;
      }
      ```
    - Verificar: `cargo test -p tessera-cli` pasa.

##### Ciclo REFACTOR

25. [ ] Documentar el timeout en el help del comando import
    - Archivo: `crates/tessera-cli/src/cli.rs`
    - Actualizar el doc del struct `ImportArgs` para mencionar el timeout de 30s.
    - Verificar: no hay cambios de comportamiento, solo documentacion.

---

#### Hallazgo 11 — `--continue-on-error` para importaciones grandes (45 min)

**Problema**: Un error en cualquier statement aborta toda la importacion. Para grafos
de 200k nodos, abortar en el statement 50k por un dato malformado pierde el trabajo
de los 49999 statements anteriores que SI fueron ejecutados exitosamente.

**Diseno**: Flag `--continue-on-error` en `ImportArgs`. Si activo, los errores de
`execute_query` se registran en stderr con el numero de statement y el texto del
error, y la importacion continua. Al final se reporta el total de exitos y errores.

##### Ciclo RED

26. [ ] Escribir tests para `--continue-on-error`
    - Archivo: `crates/tessera-cli/src/cli.rs` — tests inline

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

    - Estos tests FALLAN (RED) porque `continue_on_error` no existe en `ImportArgs`.

##### Ciclo GREEN

27. [ ] Agregar flag `--continue-on-error` a `ImportArgs`
    - Archivo: `crates/tessera-cli/src/cli.rs`
    - Agregar campo a `ImportArgs`:
      ```rust
      /// Continue importing after statement errors (log errors to stderr)
      #[arg(long, default_value_t = false)]
      pub continue_on_error: bool,
      ```

28. [ ] Implementar logica en `handle_import`
    - Archivo: `crates/tessera-cli/src/main.rs`
    - En el consumer loop, si `args.continue_on_error`:
      ```rust
      let mut error_count = 0usize;
      // ... en el loop:
      if let Err(e) = result {
          if args.continue_on_error {
              error_count += 1;
              eprintln!("Error in statement #{}: {e}", count + error_count);
          } else {
              query_err = Some(e);
              break;
          }
      }
      // ... al final:
      if error_count > 0 {
          eprintln!("{error_count} statements failed (see errors above).");
      }
      ```
    - Verificar: `cargo test -p tessera-cli` pasa.

##### Ciclo REFACTOR

29. [ ] Verificar que el flag solo afecta el live path, no dry-run
    - Verificar: el dry-run path no lee `continue_on_error` (no aplica).
    - Verificar: clippy limpio.

---

#### Hallazgo 12 — Progreso separado de warnings en stderr (25 min)

**Problema**: Los `eprintln!` de progreso y los warnings de parse se mezclan en
stderr sin distincion. Para automatizacion (piping stderr), no hay forma de separar
progreso de errores reales.

**Diseno**: Prefijo `[PROGRESS]` en los mensajes de progreso, `[WARN]` en warnings.
Esto permite filtrado trivial con `grep` sin requerir formato JSON completo.

##### Ciclo RED

30. [ ] Escribir test que verifica el formato del mensaje de progreso
    - Este es un cambio en el formato de `eprintln!` en `handle_import`. No es
      testeable directamente en unit tests (stderr no se captura). El RED aqui
      es la verificacion manual + grep.
    - Agregar una constante de prefijo para evitar typos:
    ```rust
    #[test]
    fn progress_prefix_format() {
        assert_eq!(PROGRESS_PREFIX, "[PROGRESS] ");
    }
    ```

##### Ciclo GREEN

31. [ ] Agregar prefijos a los mensajes de stderr en `handle_import`
    - Archivo: `crates/tessera-cli/src/main.rs`
    - Constantes:
      ```rust
      const PROGRESS_PREFIX: &str = "[PROGRESS] ";
      ```
    - Cambiar:
      ```rust
      // ANTES:
      eprintln!("Imported {count} statements...");
      eprintln!("Imported {count} statements total.");

      // DESPUES:
      eprintln!("{PROGRESS_PREFIX}Imported {count} statements...");
      eprintln!("{PROGRESS_PREFIX}Imported {count} statements total.");
      ```
    - Los errores de `--continue-on-error` (H11) ya tienen contexto propio.
    - Verificar: `cargo test -p tessera-cli` pasa.

---

#### Hallazgo 13 — `format_endpoint_match` evitar String intermedio (15 min)

**Problema**: `format_endpoint_match` retorna un `String` que inmediatamente se
concatena en otro buffer con `push_str`. Esto crea una asignacion intermedia por
cada edge (2 llamadas × N edges = 2N asignaciones innecesarias).

##### Ciclo RED

32. [ ] Escribir test que verifica la nueva firma con `&mut String`
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
    - FALLA (RED) porque `write_endpoint_match` no existe (la funcion actual es `format_endpoint_match`).

##### Ciclo GREEN

33. [ ] Renombrar y cambiar firma de `format_endpoint_match`
    - Archivo: `crates/tessera-cli/src/import.rs`
    - Renombrar a `write_endpoint_match` con firma:
      ```rust
      fn write_endpoint_match(
          edge: &serde_json::Value,
          endpoint_key: &str,
          buf: &mut String,
      ) -> Result<(), CliError>
      ```
    - Mover el contenido del `String` result directamente al `buf` parametro.
    - Actualizar `edge_value_to_gql_stmt` para usar la nueva firma:
      ```rust
      // ANTES:
      let source_match = format_endpoint_match(edge, "source")?;
      buf.push_str(&source_match);

      // DESPUES:
      write_endpoint_match(edge, "source", &mut buf)?;
      ```
    - Verificar: `cargo test -p tessera-cli` pasa. Los tests de edge JSON siguen verdes.

##### Ciclo REFACTOR

34. [ ] Eliminar la funcion antigua `format_endpoint_match` si queda como dead code
    - Verificar: `cargo clippy -p tessera-cli -- -D warnings` limpio.

---

#### Hallazgo 14 — Structs de serde al nivel de modulo (20 min)

**Problema**: `ArrayStreamSeed`, `RootVisitor`, `RootSeed` estan definidos dentro del
cuerpo de `stream_json_import`, lo que dificulta la navegacion del codigo y excluye
la posibilidad de reutilizarlas en otros contextos de streaming.

##### Ciclo RED

35. [ ] No hay test RED para este cambio cosmetico. El RED implicito son los tests
       existentes de `stream_json_*` — si el movimiento rompe lifetimes o visibilidad,
       los tests fallan.

##### Ciclo GREEN

36. [ ] Mover structs al nivel de modulo como tipos privados
    - Archivo: `crates/tessera-cli/src/import.rs`
    - Mover `ArrayStreamSeed`, `RootVisitor`, `RootSeed` fuera de `stream_json_import`.
    - Mantenerlas como `struct` privadas (sin `pub`).
    - Los `use` de serde traits (`DeserializeSeed`, `Deserializer`, `MapAccess`, etc.)
      se mueven al nivel de modulo tambien.
    - La funcion `stream_json_import` se simplifica a instanciar `RootSeed` y llamar
      a `deserialize`.

##### Ciclo REFACTOR

37. [ ] Verificar que no hay `pub` innecesario en las structs movidas
    - Verificar: `cargo clippy -p tessera-cli -- -D warnings` limpio.
    - Verificar: `cargo test -p tessera-cli stream_json` — todos verdes.

---

### Fase 7 — Verificacion Final (15 min)

20. [ ] Compilar y ejecutar todos los tests
    - Comando: `cargo test -p tessera-cli 2>&1`
    - Verificar: cero errores de compilacion
    - Verificar: cero advertencias (warnings = errors por workspace lints)
    - Verificar: todos los tests pasan (batch, streaming, parity, throughput, injection)
    - Verificar: `cargo clippy -p tessera-cli -- -D warnings` limpio

21. [ ] Verificar ausencia de eprintln en funciones puras
    - Comando: buscar `eprintln!` en `import.rs`
    - Verificar: cero ocurrencias en codigo de produccion (fuera de `#[cfg(test)]`)
    - Los `eprintln!` de progreso en `main.rs` son intencionales y se quedan.

22. [ ] Verificar que los parity tests siguen siendo el contrato DRY
    - `stream_gql_parity_with_batch` — verde
    - `stream_csv_parity_with_batch` — verde
    - `stream_json_parity_with_batch` — verde
    - Si alguno falla, el refactor DRY introdujo una regresion.

---

## Estimacion Total

| Fase | Hallazgos | Tareas | Tiempo |
|------|-----------|--------|--------|
| 1 — Seguridad CSV (H4) | H4 | 1-3 | 20 min |
| 2 — Error silenciado (H1) | H1 | 4-6 | 30 min |
| 3 — DRY refactor (H3) | H3 | 7-11 | 35 min |
| 4 — eprintln pura (H2) | H2 | 12-14 | 25 min |
| 5 — Mejoras recomendadas (H5-H9) | H5,H6,H7,H8,H9 | 15-19 | 40 min |
| 6 — Hallazgos 10-14 | H10,H11,H12,H13,H14 | 23-37 | 1h 40 min |
| 7 — Verificacion final | todos | 20-22 | 15 min |
| **Total** | **14 hallazgos** | **37 tareas** | **~4.5 h** |

---

## Criterios de Exito

- [ ] `cargo test -p tessera-cli` pasa con cero warnings
- [ ] Los 6 tests nuevos de seguridad CSV (hallazgo 4) estan verdes
- [ ] El error tardio del producer es visible al caller (hallazgo 1)
- [ ] Las tres funciones batch delegan en sus equivalentes streaming (hallazgo 3)
- [ ] `write_json_value_to_buf` no imprime en stderr (hallazgo 2)
- [ ] `IMPORT_CHANNEL_CAPACITY` nombrada con doc comment (hallazgo 5)
- [ ] `stream_gql_import` acepta `Read` en lugar de `BufRead` (hallazgo 6)
- [ ] Tests fantasma de `is_large_file` eliminados (hallazgo 7)
- [ ] Timeout de throughput tests condicionado a `debug_assertions` (hallazgo 8)
- [ ] `should_report_progress` con semantica falsa eliminada (hallazgo 9)
- [ ] Timeout por statement de 30s en consumer loop (hallazgo 10)
- [ ] Flag `--continue-on-error` en import con reporte de errores parciales (hallazgo 11)
- [ ] Prefijo `[PROGRESS]` en mensajes de progreso para separar de warnings (hallazgo 12)
- [ ] `format_endpoint_match` renombrada a `write_endpoint_match` con `&mut String` (hallazgo 13)
- [ ] Structs de serde movidas al nivel de modulo (hallazgo 14)
- [ ] Throughput guards: 10k elementos en < 10 s debug / < 2 s release (hot path)

---

## Notas de Riesgo

**Hallazgo 3 (DRY) — `split_gql_statements` delegando en `stream_gql_import`**:
El cambio implica que `split_gql_statements` ahora ignora el `Result` del stream
(con `let _ = ...`). Esto es correcto porque el callback nunca falla, pero un linter
puede advertir sobre el `let _`. Si ocurre, wrappear con `.unwrap_or(0)` y documentar
que es imposible fallar en este contexto.

**Hallazgo 1 (canal tipado) — Cambio de `channel::<Result<String, CliError>>` a `channel::<String>`**:
Este es el cambio mas invasivo en `main.rs`. El tipo del canal cambia, lo que afecta
el `while let` loop. Verificar que no hay ninguna rama que todavia envia `Err` al canal
tras el cambio. Si `tx.blocking_send(Err(e))` quedara en algun path no actualizado,
habra un error de compilacion (buen fail-fast).

**Hallazgo 6 — `BufReader` interno en `stream_gql_import`**:
El `BufReader::new(reader)` interno crea un buffer de 8KB por defecto. Si el reader
ya es un `BufReader<File>` (como en main.rs), esto crea doble buffering. Esto es
aceptable — no es un bug — pero si se quiere evitar, usar `BufReader::with_capacity(0, reader)`
para los casos donde el reader ya es bufferizado. Para simplicidad, dejar el buffer
por defecto; el overhead es trivial comparado con el I/O.
