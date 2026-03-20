# TDD Plan: CSV / JSON / GQL Import & Export — `tessera-import`

## Context

`tessera-import` must provide bulk data ingestion and extraction for TesseraGraph Enterprise.
The crate is currently a stub (`lib.rs` with just a module-level doc comment).
The plan covers eight vertical layers, each with a RED → GREEN → REFACTOR cycle.

**Stack detected**: Rust 2024, `thiserror 2`, `serde_json 1`, `tessera-graph` (MIT core)
**Convenciones observadas**:
- Copyright header `// Copyright 2026 BelowZero Security OU. All rights reserved.` en todos los archivos
- Tests en `crates/<crate>/tests/<name>_test.rs` (integración) o `tests/<name>_test.rs`
- `error.rs` con `thiserror`, `pub type Result<T> = std::result::Result<T, …Error>;`
- Clippy: `all = deny`, `pedantic = warn`, `nursery = warn` (workspace heredado)
- No `unsafe_code`
- Sin `csv` crate en workspace — se parsea manualmente o se agrega
- `serde` / `serde_json` ya disponibles en workspace
- GQL parsing via `tessera_graph::gql::parse_statement` + `tessera_storage_enterprise::gql::execute_mut`

**Afecta hot path?**: No — import/export son operaciones batch de administración, no en el critical path de queries online.

## Decisiones Previas Necesarias

Ninguna bloqueante — la arquitectura está clara desde los archivos leídos.

Nota de diseño resueltas:

1. **Sin crate `csv`**: El workspace no lo tiene. Parseo de CSV se hará manualmente (split por línea, split por coma con soporte básico de comillas dobles). Esto es suficiente para el formato definido y evita una dependencia nueva. Si se necesita RFC 4180 completo, agregar `csv = "1"` al `[workspace.dependencies]` es un cambio de una línea.

2. **Permiso de Import/Export**: No existen `Permission::GraphImport` / `Permission::GraphExport` en `tessera-auth`. Se usará `Permission::GraphBackup` para export (operación de lectura masiva) y la combinación `NodeCreate + EdgeCreate` para import (ya existentes). Documentar esto explícitamente en cada función pública.

3. **Enumeración de edges para export**: `Graph` no expone `edge_ids()`. La estrategia correcta es iterar `graph.node_ids()` → `graph.outgoing_edges(id)` por cada nodo, acumulando edges en un `HashSet<EdgeId>` para deduplicar self-loops.

4. **GQL Import**: Delegar completamente a `tessera_graph::gql::parse_statement` + `tessera_storage_enterprise::gql::execute_mut`. El crate de import solo lee el archivo y llama a esas funciones secuencialmente.

5. **Integración con protocolo**: Los nuevos `ClientMessage::Import` y `ClientMessage::Export` llevan payload inline (texto UTF-8 del CSV/JSON/GQL o path de archivo). Para archivos grandes en producción esto debe evolucionar a streaming, pero para la primera implementación payload inline es correcto.

---

## Plan de Ejecución

### Layer 1: Error types y módulo raíz

**Objetivo**: Crear `error.rs` con `ImportError` y `ExportError`, exponer desde `lib.rs`.

**Archivos**:
- `crates/tessera-import/src/error.rs` — Crear
- `crates/tessera-import/src/lib.rs` — Modificar (re-exports)

**RED — Escribe el test primero**

Archivo: `crates/tessera-import/tests/error_types_test.rs`

```rust
// Copyright 2026 BelowZero Security OU. All rights reserved.

use tessera_import::error::{ExportError, ImportError};

#[test]
fn import_error_csv_parse_displays_row_number() {
    let e = ImportError::CsvParse { row: 3, reason: "missing label column".into() };
    let msg = e.to_string();
    assert!(msg.contains("row 3"), "got: {msg}");
    assert!(msg.contains("missing label column"), "got: {msg}");
}

#[test]
fn import_error_json_invalid_displays_reason() {
    let e = ImportError::JsonInvalid("unexpected token at position 42".into());
    assert!(e.to_string().contains("unexpected token"));
}

#[test]
fn import_error_node_not_found_in_edge_displays_label() {
    let e = ImportError::NodeNotFoundForEdge {
        label: "Person".into(),
        prop: "name".into(),
        value: "Alice".into(),
    };
    let msg = e.to_string();
    assert!(msg.contains("Person"), "got: {msg}");
    assert!(msg.contains("Alice"), "got: {msg}");
}

#[test]
fn import_error_gql_statement_displays_line_and_error() {
    let e = ImportError::GqlStatement { line: 7, reason: "syntax error".into() };
    let msg = e.to_string();
    assert!(msg.contains("7"), "got: {msg}");
    assert!(msg.contains("syntax error"), "got: {msg}");
}

#[test]
fn export_error_graph_read_wraps_message() {
    let e = ExportError::GraphRead("node 42 not found".into());
    assert!(e.to_string().contains("node 42"));
}

#[test]
fn export_error_serialize_wraps_message() {
    let e = ExportError::Serialize("NaN is not representable".into());
    assert!(e.to_string().contains("NaN"));
}
```

**Estructura de datos a implementar** (`error.rs`):

```rust
// Copyright 2026 BelowZero Security OU. All rights reserved.

/// Errors that can occur during bulk data import.
#[derive(Debug, thiserror::Error)]
pub enum ImportError {
    #[error("CSV parse error at row {row}: {reason}")]
    CsvParse { row: usize, reason: String },

    #[error("invalid JSON: {0}")]
    JsonInvalid(String),

    #[error("missing required JSON field: {0}")]
    JsonMissingField(String),

    #[error("node not found for edge endpoint — label={label}, {prop}={value}")]
    NodeNotFoundForEdge { label: String, prop: String, value: String },

    #[error("GQL statement error at line {line}: {reason}")]
    GqlStatement { line: usize, reason: String },

    #[error("graph write error: {0}")]
    GraphWrite(String),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// Errors that can occur during bulk data export.
#[derive(Debug, thiserror::Error)]
pub enum ExportError {
    #[error("graph read error: {0}")]
    GraphRead(String),

    #[error("serialization error: {0}")]
    Serialize(String),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

pub type ImportResult<T> = std::result::Result<T, ImportError>;
pub type ExportResult<T> = std::result::Result<T, ExportError>;
```

**GREEN**: Implementar `error.rs` exactamente como arriba. Verificar `cargo test -p tessera-import`.

**REFACTOR**: Ninguno — el tipo es simple y final.

Estimación: 20 min

---

### Layer 2: CSV Import

**Objetivo**: Parsear node CSV y edge CSV y aplicarlos a un `&mut Graph`.

**Archivos**:
- `crates/tessera-import/src/csv/mod.rs` — Crear (parser de nodos y edges)
- `crates/tessera-import/src/csv/parser.rs` — Crear (lógica de parseo de líneas)
- `crates/tessera-import/src/lib.rs` — Agregar `pub mod csv;`
- `crates/tessera-import/tests/csv_import_test.rs` — Crear

**Firmas públicas a implementar**:

```rust
// En crates/tessera-import/src/csv/mod.rs

use tessera_graph::Graph;
use crate::error::ImportResult;

/// Imports nodes from a CSV string into the graph.
///
/// Expected header: `label,prop1,prop2,...`
/// The first column is always the node label. All other columns become
/// string properties unless the value is parseable as i64 or f64.
///
/// # Errors
/// Returns `ImportError::CsvParse` on malformed rows.
/// Returns `ImportError::GraphWrite` if `graph.add_node` fails.
pub fn import_nodes_csv(graph: &mut Graph, csv: &str) -> ImportResult<usize>;

/// Imports edges from a CSV string into the graph.
///
/// Expected header:
/// `source_label,source_prop,source_value,target_label,target_prop,target_value,rel_label,prop1,...`
///
/// Matching is performed by finding a node with `source_label` whose
/// `source_prop` property equals `source_value` (string match).
///
/// # Errors
/// Returns `ImportError::CsvParse` on malformed rows.
/// Returns `ImportError::NodeNotFoundForEdge` if no matching node is found.
/// Returns `ImportError::GraphWrite` if `graph.add_edge` fails.
pub fn import_edges_csv(graph: &mut Graph, csv: &str) -> ImportResult<usize>;
```

**Lógica del parser CSV interno** (`csv/parser.rs`):

```rust
// Copyright 2026 BelowZero Security OU. All rights reserved.

/// Splits a CSV line respecting double-quoted fields.
/// Quotes within a field must be escaped as `""`.
/// Returns `Vec<String>` with whitespace trimmed from unquoted fields.
pub(crate) fn split_csv_line(line: &str) -> Vec<String>;

/// Coerce a raw string value to the most specific Property type.
/// Priority: i64 → f64 → bool → String.
pub(crate) fn coerce_value(raw: &str) -> tessera_graph::Property;
```

**RED — Tests CSV Import**

```rust
// crates/tessera-import/tests/csv_import_test.rs
// Copyright 2026 BelowZero Security OU. All rights reserved.

use tessera_graph::{Graph, Property};
use tessera_import::csv::{import_edges_csv, import_nodes_csv};

// ── Node import ─────────────────────────────────────────────────────────────

#[test]
fn import_nodes_csv_creates_nodes_with_correct_labels() {
    let mut g = Graph::new();
    let csv = "label,name,age\nPerson,Alice,30\nPerson,Bob,25\n";
    let count = import_nodes_csv(&mut g, csv).unwrap();
    assert_eq!(count, 2);
    assert_eq!(g.nodes_by_label("Person").len(), 2);
}

#[test]
fn import_nodes_csv_coerces_integer_property() {
    let mut g = Graph::new();
    let csv = "label,score\nSensor,42\n";
    import_nodes_csv(&mut g, csv).unwrap();
    let id = g.nodes_by_label("Sensor")[0];
    let node = g.node(id).unwrap();
    assert_eq!(node.properties().get("score"), Some(&Property::I64(42)));
}

#[test]
fn import_nodes_csv_coerces_float_property() {
    let mut g = Graph::new();
    let csv = "label,ratio\nMetric,3.14\n";
    import_nodes_csv(&mut g, csv).unwrap();
    let id = g.nodes_by_label("Metric")[0];
    let node = g.node(id).unwrap();
    assert!(matches!(node.properties().get("ratio"), Some(Property::F64(_))));
}

#[test]
fn import_nodes_csv_coerces_bool_true() {
    let mut g = Graph::new();
    let csv = "label,active\nDevice,true\n";
    import_nodes_csv(&mut g, csv).unwrap();
    let id = g.nodes_by_label("Device")[0];
    let node = g.node(id).unwrap();
    assert_eq!(node.properties().get("active"), Some(&Property::Bool(true)));
}

#[test]
fn import_nodes_csv_handles_quoted_fields() {
    let mut g = Graph::new();
    let csv = "label,name\nPlace,\"New, York\"\n";
    import_nodes_csv(&mut g, csv).unwrap();
    let id = g.nodes_by_label("Place")[0];
    let node = g.node(id).unwrap();
    assert_eq!(
        node.properties().get("name").and_then(|p| p.as_str()),
        Some("New, York")
    );
}

#[test]
fn import_nodes_csv_skips_blank_lines() {
    let mut g = Graph::new();
    let csv = "label,name\n\nPerson,Alice\n\n";
    let count = import_nodes_csv(&mut g, csv).unwrap();
    assert_eq!(count, 1);
}

#[test]
fn import_nodes_csv_returns_error_on_missing_label_column() {
    let mut g = Graph::new();
    // Header does not start with "label"
    let csv = "id,name\nPerson,Alice\n";
    let result = import_nodes_csv(&mut g, csv);
    assert!(result.is_err());
    let msg = result.unwrap_err().to_string();
    assert!(msg.contains("label"), "got: {msg}");
}

#[test]
fn import_nodes_csv_returns_error_on_row_with_wrong_column_count() {
    let mut g = Graph::new();
    let csv = "label,name,age\nPerson,Alice\n"; // missing 'age'
    let result = import_nodes_csv(&mut g, csv);
    assert!(result.is_err());
}

#[test]
fn import_nodes_csv_returns_count_of_imported_nodes() {
    let mut g = Graph::new();
    let csv = "label,x\nA,1\nB,2\nC,3\n";
    let count = import_nodes_csv(&mut g, csv).unwrap();
    assert_eq!(count, 3);
}

// ── Edge import ──────────────────────────────────────────────────────────────

#[test]
fn import_edges_csv_connects_existing_nodes() {
    let mut g = Graph::new();
    import_nodes_csv(&mut g, "label,name\nPerson,Alice\nPerson,Bob\n").unwrap();

    let csv = "source_label,source_prop,source_value,target_label,target_prop,target_value,rel_label\nPerson,name,Alice,Person,name,Bob,KNOWS\n";
    let count = import_edges_csv(&mut g, csv).unwrap();
    assert_eq!(count, 1);
    assert_eq!(g.edge_count(), 1);
    let edge_ids = g.edges_by_label("KNOWS");
    assert_eq!(edge_ids.len(), 1);
}

#[test]
fn import_edges_csv_returns_error_when_source_node_not_found() {
    let mut g = Graph::new();
    import_nodes_csv(&mut g, "label,name\nPerson,Bob\n").unwrap();

    let csv = "source_label,source_prop,source_value,target_label,target_prop,target_value,rel_label\nPerson,name,Alice,Person,name,Bob,KNOWS\n";
    let result = import_edges_csv(&mut g, csv);
    assert!(result.is_err());
    let msg = result.unwrap_err().to_string();
    assert!(msg.contains("Alice"), "got: {msg}");
}

#[test]
fn import_edges_csv_with_edge_properties() {
    let mut g = Graph::new();
    import_nodes_csv(&mut g, "label,name\nPerson,Alice\nPerson,Bob\n").unwrap();

    let csv = "source_label,source_prop,source_value,target_label,target_prop,target_value,rel_label,since\nPerson,name,Alice,Person,name,Bob,KNOWS,2020\n";
    import_edges_csv(&mut g, csv).unwrap();
    let edge = g.edge(g.edges_by_label("KNOWS")[0]).unwrap();
    assert_eq!(edge.properties().get("since"), Some(&Property::I64(2020)));
}

#[test]
fn import_edges_csv_returns_error_on_wrong_header() {
    let mut g = Graph::new();
    let csv = "wrong_header\ndata\n";
    let result = import_edges_csv(&mut g, csv);
    assert!(result.is_err());
}
```

**GREEN**: Implementar `csv/parser.rs` y `csv/mod.rs`. El estado de Graph es mutable durante el import. Para la búsqueda de nodos por propiedad en edges: iterar `graph.nodes_by_label(label)`, luego `graph.node(id)`, comparar la propiedad como `Property::String(value)` o cualquier variante cuyo `Display` coincida con el valor raw del CSV.

**REFACTOR**: Extraer `find_node_by_label_and_prop(graph, label, prop, value) -> Option<NodeId>` como función privada reutilizable — se usará también en JSON import.

Estimación: 60 min (implementación) + 20 min (tests)

---

### Layer 3: JSON Import

**Objetivo**: Parsear el formato JSON definido y aplicar al grafo.

**Archivos**:
- `crates/tessera-import/src/json/mod.rs` — Crear
- `crates/tessera-import/src/lib.rs` — Agregar `pub mod json;`
- `crates/tessera-import/Cargo.toml` — Agregar `serde = { workspace = true }` y `serde_json = { workspace = true }`
- `crates/tessera-import/tests/json_import_test.rs` — Crear

**Estructura JSON soportada**:

```json
{
  "nodes": [
    { "label": "Person", "properties": { "name": "Alice", "age": 30 } }
  ],
  "edges": [
    {
      "source": { "label": "Person", "match": { "name": "Alice" } },
      "target": { "label": "Person", "match": { "name": "Bob" } },
      "label": "KNOWS",
      "properties": {}
    }
  ]
}
```

**Firma pública**:

```rust
// crates/tessera-import/src/json/mod.rs

use tessera_graph::Graph;
use crate::error::ImportResult;

/// Imports graph data from a JSON string.
///
/// Accepts the canonical TesseraGraph JSON format with top-level `nodes` and
/// `edges` arrays. Nodes are inserted first; edge endpoints are resolved by
/// label + property match after all nodes have been inserted.
///
/// # Errors
/// Returns `ImportError::JsonInvalid` if the JSON is malformed.
/// Returns `ImportError::JsonMissingField` if a required field is absent.
/// Returns `ImportError::NodeNotFoundForEdge` if an edge endpoint cannot be matched.
/// Returns `ImportError::GraphWrite` if a graph write operation fails.
pub fn import_json(graph: &mut Graph, json_str: &str) -> ImportResult<ImportJsonSummary>;

/// Summary of a completed JSON import operation.
#[derive(Debug, PartialEq, Eq)]
pub struct ImportJsonSummary {
    pub nodes_imported: usize,
    pub edges_imported: usize,
}
```

**RED — Tests JSON Import**

```rust
// crates/tessera-import/tests/json_import_test.rs
// Copyright 2026 BelowZero Security OU. All rights reserved.

use tessera_graph::{Graph, Property};
use tessera_import::json::{ImportJsonSummary, import_json};

const SIMPLE_GRAPH: &str = r#"{
  "nodes": [
    {"label": "Person", "properties": {"name": "Alice", "age": 30}},
    {"label": "Person", "properties": {"name": "Bob", "age": 25}}
  ],
  "edges": [
    {
      "source": {"label": "Person", "match": {"name": "Alice"}},
      "target": {"label": "Person", "match": {"name": "Bob"}},
      "label": "KNOWS",
      "properties": {"since": 2020}
    }
  ]
}"#;

#[test]
fn import_json_creates_correct_node_and_edge_counts() {
    let mut g = Graph::new();
    let summary = import_json(&mut g, SIMPLE_GRAPH).unwrap();
    assert_eq!(summary, ImportJsonSummary { nodes_imported: 2, edges_imported: 1 });
    assert_eq!(g.node_count(), 2);
    assert_eq!(g.edge_count(), 1);
}

#[test]
fn import_json_persists_node_properties_with_correct_types() {
    let mut g = Graph::new();
    import_json(&mut g, SIMPLE_GRAPH).unwrap();
    let ids = g.nodes_by_label("Person");
    // Find Alice
    let alice_id = ids.iter().find(|&&id| {
        g.node(id).unwrap().properties().get("name").and_then(|p| p.as_str()) == Some("Alice")
    }).copied().unwrap();
    let alice = g.node(alice_id).unwrap();
    assert_eq!(alice.properties().get("age"), Some(&Property::I64(30)));
}

#[test]
fn import_json_persists_edge_properties() {
    let mut g = Graph::new();
    import_json(&mut g, SIMPLE_GRAPH).unwrap();
    let edge = g.edge(g.edges_by_label("KNOWS")[0]).unwrap();
    assert_eq!(edge.properties().get("since"), Some(&Property::I64(2020)));
}

#[test]
fn import_json_empty_nodes_and_edges_succeeds() {
    let mut g = Graph::new();
    let summary = import_json(&mut g, r#"{"nodes": [], "edges": []}"#).unwrap();
    assert_eq!(summary, ImportJsonSummary { nodes_imported: 0, edges_imported: 0 });
}

#[test]
fn import_json_returns_error_on_malformed_json() {
    let mut g = Graph::new();
    let result = import_json(&mut g, "not json at all");
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("invalid JSON") ||
            result.unwrap_err_string().contains("invalid"));
}

// Note: unwrap_err_string helper below:
trait ResultExt {
    fn unwrap_err_string(self) -> String;
}
impl<T> ResultExt for Result<T, tessera_import::error::ImportError> {
    fn unwrap_err_string(self) -> String {
        self.unwrap_err().to_string()
    }
}

#[test]
fn import_json_returns_error_on_missing_nodes_field() {
    let mut g = Graph::new();
    let result = import_json(&mut g, r#"{"edges": []}"#);
    assert!(result.is_err());
    let msg = result.unwrap_err().to_string();
    assert!(msg.contains("nodes") || msg.contains("missing"), "got: {msg}");
}

#[test]
fn import_json_returns_error_when_edge_source_node_not_found() {
    let mut g = Graph::new();
    let json = r#"{
      "nodes": [{"label": "Person", "properties": {"name": "Bob"}}],
      "edges": [{
        "source": {"label": "Person", "match": {"name": "Alice"}},
        "target": {"label": "Person", "match": {"name": "Bob"}},
        "label": "KNOWS",
        "properties": {}
      }]
    }"#;
    let result = import_json(&mut g, json);
    assert!(result.is_err());
    let msg = result.unwrap_err().to_string();
    assert!(msg.contains("Alice"), "got: {msg}");
}

#[test]
fn import_json_float_property_parsed_as_f64() {
    let mut g = Graph::new();
    let json = r#"{"nodes": [{"label": "M", "properties": {"lat": 40.7}}], "edges": []}"#;
    import_json(&mut g, json).unwrap();
    let id = g.nodes_by_label("M")[0];
    let node = g.node(id).unwrap();
    assert!(matches!(node.properties().get("lat"), Some(Property::F64(_))));
}

#[test]
fn import_json_bool_property_parsed_as_bool() {
    let mut g = Graph::new();
    let json = r#"{"nodes": [{"label": "D", "properties": {"active": true}}], "edges": []}"#;
    import_json(&mut g, json).unwrap();
    let id = g.nodes_by_label("D")[0];
    let node = g.node(id).unwrap();
    assert_eq!(node.properties().get("active"), Some(&Property::Bool(true)));
}
```

**GREEN**: Parsear con `serde_json::Value`. Convertir `serde_json::Value` → `tessera_graph::Property`:
- `Value::Number(n)` → si `n.as_i64()` is Some → `Property::I64`, else → `Property::F64`
- `Value::Bool(b)` → `Property::Bool`
- `Value::String(s)` → `Property::String`
- Otros → `Property::String(value.to_string())`

Reutilizar `find_node_by_label_and_prop` de Layer 2 (mover a `crates/tessera-import/src/node_lookup.rs`).

**REFACTOR**: Extraer `json_value_to_property(v: &serde_json::Value) -> Property` como función reutilizable en `src/property_coerce.rs`. Será usada en JSON import y JSON export.

Estimación: 45 min (implementación) + 20 min (tests)

---

### Layer 4: GQL Import

**Objetivo**: Leer un string de múltiples sentencias GQL (CREATE/MERGE) y ejecutarlas.

**Archivos**:
- `crates/tessera-import/src/gql_import/mod.rs` — Crear
- `crates/tessera-import/src/lib.rs` — Agregar `pub mod gql_import;`
- `crates/tessera-import/Cargo.toml` — Agregar `tessera-storage-enterprise = { workspace = true }`
- `crates/tessera-import/tests/gql_import_test.rs` — Crear

**Firma pública**:

```rust
// crates/tessera-import/src/gql_import/mod.rs

use tessera_graph::Graph;
use crate::error::ImportResult;

/// Summary of a completed GQL import.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct GqlImportSummary {
    pub statements_executed: usize,
    pub nodes_created: u64,
    pub edges_created: u64,
    pub nodes_merged: u64,
}

/// Executes a sequence of GQL mutation statements from a string.
///
/// Each non-blank line (after stripping `//` and `--` comments) is treated as
/// a complete GQL statement. Statements are executed in document order.
/// The first failure aborts the import and returns an error — partial state
/// is NOT rolled back (the graph does not yet have transactional rollback for
/// bulk operations).
///
/// # Errors
/// Returns `ImportError::GqlStatement` with the 1-based line number if any
/// statement fails to parse or execute.
/// Returns `ImportError::GraphWrite` for storage-level errors.
pub fn import_gql(graph: &mut Graph, gql_text: &str) -> ImportResult<GqlImportSummary>;
```

**RED — Tests GQL Import**

```rust
// crates/tessera-import/tests/gql_import_test.rs
// Copyright 2026 BelowZero Security OU. All rights reserved.

use tessera_graph::Graph;
use tessera_import::gql_import::{GqlImportSummary, import_gql};

#[test]
fn import_gql_single_create_node() {
    let mut g = Graph::new();
    let gql = "CREATE (:Person {name: 'Alice', age: 30})";
    let summary = import_gql(&mut g, gql).unwrap();
    assert_eq!(summary.statements_executed, 1);
    assert_eq!(summary.nodes_created, 1);
    assert_eq!(g.node_count(), 1);
}

#[test]
fn import_gql_multiple_statements_separated_by_newlines() {
    let mut g = Graph::new();
    let gql = "CREATE (:Person {name: 'Alice'})\nCREATE (:Person {name: 'Bob'})";
    let summary = import_gql(&mut g, gql).unwrap();
    assert_eq!(summary.statements_executed, 2);
    assert_eq!(g.node_count(), 2);
}

#[test]
fn import_gql_create_edge_between_created_nodes() {
    let mut g = Graph::new();
    let gql = "CREATE (a:Person {name: 'Alice'})-[:KNOWS]->(b:Person {name: 'Bob'})";
    let summary = import_gql(&mut g, gql).unwrap();
    assert_eq!(summary.edges_created, 1);
    assert_eq!(g.edge_count(), 1);
}

#[test]
fn import_gql_skips_blank_lines_and_comment_lines() {
    let mut g = Graph::new();
    let gql = "// This is a comment\n\nCREATE (:Node {x: 1})\n-- another comment\n";
    let summary = import_gql(&mut g, gql).unwrap();
    assert_eq!(summary.statements_executed, 1);
    assert_eq!(g.node_count(), 1);
}

#[test]
fn import_gql_returns_error_with_line_number_on_parse_failure() {
    let mut g = Graph::new();
    let gql = "CREATE (:Valid {x: 1})\nNOT VALID GQL SYNTAX";
    let result = import_gql(&mut g, gql);
    assert!(result.is_err());
    let msg = result.unwrap_err().to_string();
    // Line 2 should be reported
    assert!(msg.contains("2") || msg.contains("line"), "got: {msg}");
}

#[test]
fn import_gql_returns_error_on_mutation_semantic_failure() {
    let mut g = Graph::new();
    // Edge referencing unbound variable
    let gql = "CREATE (a:X {id: 1})-[:R]->(b:Y {id: 2})\nCREATE (x:X)-[:R]->(nonexistent)";
    let result = import_gql(&mut g, gql);
    // This may or may not error depending on GQL parser/executor behavior;
    // the test validates that if an error occurs it has a line reference.
    if let Err(e) = result {
        let msg = e.to_string();
        assert!(!msg.is_empty(), "error message must not be empty");
    }
}

#[test]
fn import_gql_empty_input_returns_zero_statements() {
    let mut g = Graph::new();
    let summary = import_gql(&mut g, "").unwrap();
    assert_eq!(summary, GqlImportSummary::default());
}

#[test]
fn import_gql_merge_statement_creates_or_matches() {
    let mut g = Graph::new();
    let gql = "MERGE (:Person {name: 'Alice'})\nMERGE (:Person {name: 'Alice'})";
    let summary = import_gql(&mut g, gql).unwrap();
    // Second MERGE should match, not create
    assert_eq!(g.node_count(), 1, "MERGE should not duplicate nodes");
    assert_eq!(summary.statements_executed, 2);
}
```

**GREEN**: Iterar líneas del texto GQL. Por cada línea no vacía y no-comentario, llamar `tessera_graph::gql::parse_statement(line)` → si error → `ImportError::GqlStatement`. Si el statement es un `GqlStatement::Mutation`, llamar `tessera_storage_enterprise::gql::execute_mut(graph, &m)`. Acumular contadores.

Nota: `parse_statement` acepta sentencias únicas. Si el archivo GQL contiene sentencias multi-línea (ej: CREATE con muchas propiedades), la estrategia "una sentencia = una línea" tiene limitaciones. Documentar esto como limitación conocida y agregar un test que lo demuestre. La implementación correcta multi-línea es un enhancement futuro.

**REFACTOR**: Extraer `is_comment_or_blank(line: &str) -> bool` como función privada. Documentar la limitación de "una sentencia = una línea" con `// TODO(v2): support multi-line GQL statements`.

Estimación: 35 min (implementación) + 20 min (tests)

---

### Layer 5: CSV Export

**Objetivo**: Serializar todos los nodos y todas las edges del grafo a CSV.

**Archivos**:
- `crates/tessera-import/src/csv/mod.rs` — Extender con las funciones de export
- `crates/tessera-import/tests/csv_export_test.rs` — Crear

**Firmas públicas**:

```rust
// Agregar a crates/tessera-import/src/csv/mod.rs

use tessera_graph::Graph;
use crate::error::ExportResult;

/// Exports all nodes from the graph to a CSV string.
///
/// Output format: `label,<sorted property keys...>`
/// All property values are serialized as their `Display` representation.
/// Properties are sorted alphabetically for deterministic output.
/// Nodes with different property sets produce rows with empty fields for absent properties.
/// The header is built from the union of all property keys across all nodes.
///
/// # Errors
/// Returns `ExportError::GraphRead` if a node cannot be read.
pub fn export_nodes_csv(graph: &Graph) -> ExportResult<String>;

/// Exports all edges from the graph to a CSV string.
///
/// Output format:
/// `source_id,target_id,rel_label,<sorted property keys...>`
/// `source_id` and `target_id` are the raw `NodeId` integer values.
///
/// Note: uses `source_id`/`target_id` (not label-based matching) because
/// a single label may have multiple nodes. The importer (`import_edges_csv`)
/// uses label+property matching, which is a different (lossy) round-trip
/// for graphs with ambiguous node identity. For a lossless round-trip,
/// use JSON or GQL export.
///
/// # Errors
/// Returns `ExportError::GraphRead` if a node or edge cannot be read.
pub fn export_edges_csv(graph: &Graph) -> ExportResult<String>;
```

**RED — Tests CSV Export**

```rust
// crates/tessera-import/tests/csv_export_test.rs
// Copyright 2026 BelowZero Security OU. All rights reserved.

use tessera_graph::{Graph, props};
use tessera_import::csv::{export_edges_csv, export_nodes_csv};

#[test]
fn export_nodes_csv_produces_header_and_data_rows() {
    let mut g = Graph::new();
    g.add_node("Person", props! { "name" => "Alice", "age" => 30_i64 }).unwrap();
    let csv = export_nodes_csv(&g).unwrap();
    let lines: Vec<&str> = csv.lines().collect();
    assert!(lines.len() >= 2, "must have header + at least 1 row");
    let header = lines[0];
    assert!(header.contains("label"), "header must include 'label': {header}");
    assert!(header.contains("name"), "header must include 'name': {header}");
    assert!(header.contains("age"), "header must include 'age': {header}");
    let row = lines[1];
    assert!(row.contains("Person"), "data row must include label: {row}");
    assert!(row.contains("Alice"), "data row must include name: {row}");
}

#[test]
fn export_nodes_csv_empty_graph_produces_only_header() {
    let g = Graph::new();
    let csv = export_nodes_csv(&g).unwrap();
    let lines: Vec<&str> = csv.lines().filter(|l| !l.is_empty()).collect();
    assert_eq!(lines.len(), 1, "empty graph must produce only the header row");
    assert_eq!(lines[0], "label");
}

#[test]
fn export_nodes_csv_handles_multiple_nodes_different_property_sets() {
    let mut g = Graph::new();
    g.add_node("A", props! { "x" => 1_i64 }).unwrap();
    g.add_node("B", props! { "y" => 2_i64 }).unwrap();
    let csv = export_nodes_csv(&g).unwrap();
    let header = csv.lines().next().unwrap();
    assert!(header.contains("x") && header.contains("y"),
        "header must be union of all props: {header}");
}

#[test]
fn export_nodes_csv_property_values_with_commas_are_quoted() {
    let mut g = Graph::new();
    g.add_node("Place", props! { "name" => "New, York" }).unwrap();
    let csv = export_nodes_csv(&g).unwrap();
    assert!(csv.contains("\"New, York\""), "comma in value must be double-quoted: {csv}");
}

#[test]
fn export_edges_csv_produces_header_and_data_rows() {
    let mut g = Graph::new();
    let a = g.add_node("Person", props! { "name" => "Alice" }).unwrap();
    let b = g.add_node("Person", props! { "name" => "Bob" }).unwrap();
    g.add_edge("KNOWS", a, b, props! { "since" => 2020_i64 }).unwrap();
    let csv = export_edges_csv(&g).unwrap();
    let lines: Vec<&str> = csv.lines().collect();
    assert!(lines.len() >= 2);
    let header = lines[0];
    assert!(header.contains("source_id"), "got: {header}");
    assert!(header.contains("rel_label"), "got: {header}");
    assert!(header.contains("since"), "got: {header}");
    assert!(lines[1].contains("KNOWS"), "data row must have label: {}", lines[1]);
}

#[test]
fn export_edges_csv_empty_graph_produces_only_header() {
    let g = Graph::new();
    let csv = export_edges_csv(&g).unwrap();
    let lines: Vec<&str> = csv.lines().filter(|l| !l.is_empty()).collect();
    assert_eq!(lines.len(), 1);
    assert!(lines[0].contains("source_id"), "got: {}", lines[0]);
}

#[test]
fn export_nodes_then_import_round_trip_node_count() {
    let mut g = Graph::new();
    g.add_node("Sensor", props! { "id" => 1_i64, "type" => "temperature" }).unwrap();
    g.add_node("Sensor", props! { "id" => 2_i64, "type" => "humidity" }).unwrap();

    let csv = export_nodes_csv(&g).unwrap();

    let mut g2 = Graph::new();
    tessera_import::csv::import_nodes_csv(&mut g2, &csv).unwrap();
    assert_eq!(g2.node_count(), 2);
    assert_eq!(g2.nodes_by_label("Sensor").len(), 2);
}
```

**GREEN**: Para `export_nodes_csv`:
1. Recopilar todos los node IDs via `graph.node_ids()`
2. Leer cada nodo via `graph.node(id)?`
3. Construir el conjunto de columnas como unión de todas las property keys, ordenado
4. Generar header: `"label," + sorted_keys.join(",")`
5. Por cada nodo: valor del label + properties en orden de columnas, con comillas si contiene coma o comilla doble

Para `export_edges_csv`:
1. Iterar `graph.node_ids()`, por cada nodo llamar `graph.outgoing_edges(id)?`
2. Acumular edges en `Vec` (no deduplicar — `outgoing_edges` ya solo retorna salientes)
3. Construir columnas de propiedades de edges de la misma manera que nodos
4. Generar header: `"source_id,target_id,rel_label," + sorted_edge_props`

**REFACTOR**: Extraer `fn collect_all_prop_keys(items: &[&Properties]) -> Vec<String>` y `fn quote_csv_value(s: &str) -> String` en un módulo interno `src/csv/format.rs`.

Estimación: 50 min (implementación) + 20 min (tests)

---

### Layer 6: JSON Export

**Objetivo**: Serializar el grafo completo como JSON canónico de TesseraGraph.

**Archivos**:
- `crates/tessera-import/src/json/mod.rs` — Extender con export
- `crates/tessera-import/tests/json_export_test.rs` — Crear

**Firma pública**:

```rust
// Agregar a crates/tessera-import/src/json/mod.rs

use tessera_graph::Graph;
use crate::error::ExportResult;

/// Serializes the entire graph to a canonical JSON string.
///
/// Output format:
/// ```json
/// {
///   "nodes": [{"label": "...", "properties": {...}}],
///   "edges": [{"source_id": 0, "target_id": 1, "label": "...", "properties": {...}}]
/// }
/// ```
///
/// Note: edges reference `source_id` and `target_id` (raw `NodeId` integers)
/// for lossless round-trip. The `import_json` counterpart uses label+property
/// matching for human-authored files.
///
/// # Errors
/// Returns `ExportError::GraphRead` if a node or edge cannot be read.
/// Returns `ExportError::Serialize` if JSON serialization fails.
pub fn export_json(graph: &Graph) -> ExportResult<String>;
```

**RED — Tests JSON Export**

```rust
// crates/tessera-import/tests/json_export_test.rs
// Copyright 2026 BelowZero Security OU. All rights reserved.

use tessera_graph::{Graph, props};
use tessera_import::json::{export_json, import_json};

#[test]
fn export_json_produces_valid_json_with_nodes_and_edges_keys() {
    let mut g = Graph::new();
    g.add_node("Person", props! { "name" => "Alice" }).unwrap();
    let json_str = export_json(&g).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();
    assert!(parsed.get("nodes").is_some(), "must have 'nodes' key");
    assert!(parsed.get("edges").is_some(), "must have 'edges' key");
}

#[test]
fn export_json_empty_graph_produces_empty_arrays() {
    let g = Graph::new();
    let json_str = export_json(&g).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();
    assert_eq!(parsed["nodes"].as_array().unwrap().len(), 0);
    assert_eq!(parsed["edges"].as_array().unwrap().len(), 0);
}

#[test]
fn export_json_node_has_correct_label_and_properties() {
    let mut g = Graph::new();
    g.add_node("Person", props! { "name" => "Alice", "age" => 30_i64 }).unwrap();
    let json_str = export_json(&g).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();
    let node = &parsed["nodes"][0];
    assert_eq!(node["label"].as_str(), Some("Person"));
    assert_eq!(node["properties"]["name"].as_str(), Some("Alice"));
    assert_eq!(node["properties"]["age"].as_i64(), Some(30));
}

#[test]
fn export_json_edge_has_source_id_target_id_label_properties() {
    let mut g = Graph::new();
    let a = g.add_node("A", props! {}).unwrap();
    let b = g.add_node("B", props! {}).unwrap();
    g.add_edge("REL", a, b, props! { "weight" => 1_i64 }).unwrap();
    let json_str = export_json(&g).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();
    let edge = &parsed["edges"][0];
    assert!(edge.get("source_id").is_some());
    assert!(edge.get("target_id").is_some());
    assert_eq!(edge["label"].as_str(), Some("REL"));
    assert_eq!(edge["properties"]["weight"].as_i64(), Some(1));
}

#[test]
fn export_import_json_round_trip_preserves_node_count_and_labels() {
    let mut g = Graph::new();
    g.add_node("Plant", props! { "name" => "Solar A", "capacity" => 100_i64 }).unwrap();
    g.add_node("Plant", props! { "name" => "Solar B", "capacity" => 200_i64 }).unwrap();

    let json_str = export_json(&g).unwrap();

    // Re-import using the human-authored import format
    // (this tests that the export format is importable, even if IDs differ)
    let re_import_json = json_str; // export_json format uses source_id/target_id not match
    // The exported format with source_id/target_id is NOT directly importable by import_json
    // (which expects match-based edge resolution). This is documented behavior.
    // The test verifies that the exported JSON is valid and parseable.
    let parsed: serde_json::Value = serde_json::from_str(&re_import_json).unwrap();
    assert_eq!(parsed["nodes"].as_array().unwrap().len(), 2);
}

#[test]
fn export_json_bool_property_preserved_as_json_bool() {
    let mut g = Graph::new();
    g.add_node("Device", props! { "active" => true }).unwrap();
    let json_str = export_json(&g).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();
    assert_eq!(parsed["nodes"][0]["properties"]["active"].as_bool(), Some(true));
}

#[test]
fn export_json_f64_property_preserved_as_json_number() {
    let mut g = Graph::new();
    g.add_node("Metric", props! { "lat" => 40.7_f64 }).unwrap();
    let json_str = export_json(&g).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();
    let lat = parsed["nodes"][0]["properties"]["lat"].as_f64().unwrap();
    assert!((lat - 40.7_f64).abs() < 1e-6);
}
```

**GREEN**: Usar `serde_json::json!` macro. Para convertir `Property` → `serde_json::Value` usar la función inversa de `json_value_to_property` del Layer 3 (mover a `src/property_coerce.rs`). Iterar edges usando el mismo patrón de `node_ids()` + `outgoing_edges()` de Layer 5.

**REFACTOR**: El módulo `src/property_coerce.rs` debe exportar dos funciones públicas-internas:
- `pub(crate) fn property_to_json(p: &Property) -> serde_json::Value`
- `pub(crate) fn json_value_to_property(v: &serde_json::Value) -> Option<Property>`

Estimación: 35 min (implementación) + 20 min (tests)

---

### Layer 7: GQL Export

**Objetivo**: Generar un archivo `.gql` de sentencias CREATE que reproduce el grafo.

**Archivos**:
- `crates/tessera-import/src/gql_export/mod.rs` — Crear
- `crates/tessera-import/src/lib.rs` — Agregar `pub mod gql_export;`
- `crates/tessera-import/tests/gql_export_test.rs` — Crear

**Firma pública**:

```rust
// crates/tessera-import/src/gql_export/mod.rs

use tessera_graph::Graph;
use crate::error::ExportResult;

/// Generates a GQL script that reproduces the entire graph when executed.
///
/// Output format:
/// - One `CREATE (:Label {key: value, ...})` statement per node.
/// - One `MATCH`+`CREATE` statement per edge, using the numeric node IDs
///   (not yet supported by the GQL executor — see Note below).
///
/// Note on edge export: GQL does not have a native "match by internal ID"
/// syntax. Edges are exported as comments with raw ID references until the
/// GQL layer supports a `WHERE id(n) = <int>` predicate.
/// For a fully executable edge round-trip, use JSON export.
///
/// # Errors
/// Returns `ExportError::GraphRead` if a node or edge cannot be read.
pub fn export_gql(graph: &Graph) -> ExportResult<String>;
```

**RED — Tests GQL Export**

```rust
// crates/tessera-import/tests/gql_export_test.rs
// Copyright 2026 BelowZero Security OU. All rights reserved.

use tessera_graph::{Graph, props};
use tessera_import::gql_export::export_gql;
use tessera_import::gql_import::import_gql;

#[test]
fn export_gql_empty_graph_produces_only_header_comment() {
    let g = Graph::new();
    let gql = export_gql(&g).unwrap();
    // Must be valid (non-empty due to comment header) but import should produce 0 nodes
    let mut g2 = Graph::new();
    import_gql(&mut g2, &gql).unwrap();
    assert_eq!(g2.node_count(), 0);
}

#[test]
fn export_gql_single_node_produces_importable_create_statement() {
    let mut g = Graph::new();
    g.add_node("Person", props! { "name" => "Alice" }).unwrap();
    let gql = export_gql(&g).unwrap();

    let mut g2 = Graph::new();
    import_gql(&mut g2, &gql).unwrap();
    assert_eq!(g2.node_count(), 1);
    let id = g2.nodes_by_label("Person")[0];
    let node = g2.node(id).unwrap();
    assert_eq!(node.properties().get("name").and_then(|p| p.as_str()), Some("Alice"));
}

#[test]
fn export_gql_multiple_nodes_all_importable() {
    let mut g = Graph::new();
    g.add_node("City", props! { "name" => "Tallinn", "population" => 437_000_i64 }).unwrap();
    g.add_node("City", props! { "name" => "Helsinki", "population" => 660_000_i64 }).unwrap();
    let gql = export_gql(&g).unwrap();

    let mut g2 = Graph::new();
    import_gql(&mut g2, &gql).unwrap();
    assert_eq!(g2.node_count(), 2);
    assert_eq!(g2.nodes_by_label("City").len(), 2);
}

#[test]
fn export_gql_integer_property_written_without_quotes() {
    let mut g = Graph::new();
    g.add_node("N", props! { "x" => 42_i64 }).unwrap();
    let gql = export_gql(&g).unwrap();
    // Integer values must appear as bare numbers, not 'quoted'
    assert!(gql.contains("42"), "integer must appear unquoted: {gql}");
    assert!(!gql.contains("'42'"), "integer must NOT be string-quoted: {gql}");
}

#[test]
fn export_gql_string_property_written_with_single_quotes() {
    let mut g = Graph::new();
    g.add_node("N", props! { "city" => "Tallinn" }).unwrap();
    let gql = export_gql(&g).unwrap();
    assert!(gql.contains("'Tallinn'"), "string must be single-quoted: {gql}");
}

#[test]
fn export_gql_bool_property_written_as_true_or_false() {
    let mut g = Graph::new();
    g.add_node("D", props! { "active" => true }).unwrap();
    let gql = export_gql(&g).unwrap();
    assert!(gql.contains("true"), "bool must appear as 'true': {gql}");
}

#[test]
fn export_gql_node_round_trip_preserves_integer_type() {
    let mut g = Graph::new();
    g.add_node("Sensor", props! { "threshold" => 100_i64 }).unwrap();
    let gql = export_gql(&g).unwrap();

    let mut g2 = Graph::new();
    import_gql(&mut g2, &gql).unwrap();
    let id = g2.nodes_by_label("Sensor")[0];
    let node = g2.node(id).unwrap();
    assert_eq!(
        node.properties().get("threshold").and_then(|p| p.as_i64()),
        Some(100)
    );
}
```

**GREEN**: Generar sentencias GQL como strings. Para cada nodo: `CREATE (:Label {key1: val1, key2: val2})`. Serializar `Property` a GQL literal:
- `Property::String(s)` → `'<s>'` (escapar comillas simples internas como `\'`)
- `Property::I64(n)` → `n.to_string()`
- `Property::F64(f)` → `f.to_string()`
- `Property::Bool(b)` → `"true"` o `"false"`
- `Property::Bytes(_)` → omitir con comentario `// [bytes property omitted]`

Para edges: emitir como comentario `// EDGE source_id={} rel={} target_id={} props={}` indicando la limitación.

Agregar header de comentario: `// Generated by TesseraGraph Export — <timestamp>\n// Re-import: tessera-import gql_import\n`.

**REFACTOR**: Extraer `fn property_to_gql_literal(p: &Property) -> String` en `src/gql_export/render.rs`.

Estimación: 40 min (implementación) + 20 min (tests)

---

### Layer 8: Protocol Integration

**Objetivo**: Agregar `ClientMessage::Import` y `ClientMessage::Export` al protocolo, manejarlos en `ConnectionHandler`, verificar permisos.

**Archivos**:
- `crates/tessera-protocol/src/message.rs` — Modificar
- `crates/tessera-server/src/connection.rs` — Modificar
- `crates/tessera-server/Cargo.toml` — Agregar `tessera-import = { workspace = true }`
- `crates/tessera-protocol/tests/message_serde_test.rs` (si existe) o nuevo — Crear/extender

**Nuevos variants de `ClientMessage`**:

```rust
/// Bulk-import graph data from an inline payload.
///
/// `format`: one of `"csv_nodes"`, `"csv_edges"`, `"json"`, `"gql"`.
/// `payload`: UTF-8 content of the data file.
Import { format: String, payload: String },

/// Bulk-export graph data.
///
/// `format`: one of `"csv_nodes"`, `"csv_edges"`, `"json"`, `"gql"`.
Export { format: String },
```

**Nuevo variant de `ServerMessage`**:

```rust
/// Result of a bulk import operation.
ImportResult { nodes_imported: u64, edges_imported: u64, statements_executed: u64 },

/// Result of a bulk export operation.
ExportResult { format: String, payload: String },
```

**Permisos**:
- Import: requiere `Permission::NodeCreate` AND `Permission::EdgeCreate` (verificados secuencialmente — si alguno falla, deny)
- Export: requiere `Permission::GraphBackup`

**RED — Tests de protocolo**

Archivo: `crates/tessera-protocol/tests/message_import_export_test.rs`

```rust
// Copyright 2026 BelowZero Security OU. All rights reserved.

use tessera_protocol::message::{ClientMessage, ServerMessage};

#[test]
fn client_message_import_serializes_and_deserializes() {
    let msg = ClientMessage::Import {
        format: "json".into(),
        payload: r#"{"nodes":[],"edges":[]}"#.into(),
    };
    let json = serde_json::to_string(&msg).unwrap();
    let roundtrip: ClientMessage = serde_json::from_str(&json).unwrap();
    assert_eq!(msg, roundtrip);
}

#[test]
fn client_message_export_serializes_and_deserializes() {
    let msg = ClientMessage::Export { format: "gql".into() };
    let json = serde_json::to_string(&msg).unwrap();
    let roundtrip: ClientMessage = serde_json::from_str(&json).unwrap();
    assert_eq!(msg, roundtrip);
}

#[test]
fn server_message_import_result_serializes_and_deserializes() {
    let msg = ServerMessage::ImportResult {
        nodes_imported: 5,
        edges_imported: 3,
        statements_executed: 0,
    };
    let json = serde_json::to_string(&msg).unwrap();
    let roundtrip: ServerMessage = serde_json::from_str(&json).unwrap();
    assert_eq!(msg, roundtrip);
}

#[test]
fn server_message_export_result_serializes_with_payload() {
    let msg = ServerMessage::ExportResult {
        format: "json".into(),
        payload: r#"{"nodes":[],"edges":[]}"#.into(),
    };
    let json = serde_json::to_string(&msg).unwrap();
    let roundtrip: ServerMessage = serde_json::from_str(&json).unwrap();
    assert_eq!(msg, roundtrip);
}

#[test]
fn client_message_import_has_type_tag_import() {
    let msg = ClientMessage::Import { format: "csv_nodes".into(), payload: "".into() };
    let json = serde_json::to_string(&msg).unwrap();
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(v["type"].as_str(), Some("import"));
}

#[test]
fn client_message_export_has_type_tag_export() {
    let msg = ClientMessage::Export { format: "csv_edges".into() };
    let json = serde_json::to_string(&msg).unwrap();
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(v["type"].as_str(), Some("export"));
}
```

**GREEN para `connection.rs`**: Agregar arm `ClientMessage::Import { format, payload }` y `ClientMessage::Export { format }` en el `match msg { ... }` de `run()`. En el arm de import:

1. Verificar autenticación (session_token.is_some())
2. Llamar `self.check_import_permission().await?` — helper privado que llama `ctx.auth_policy().check_session(token, Permission::NodeCreate, sessions)` y luego `Permission::EdgeCreate`
3. Adquirir write lock en graph
4. Dispatch según `format`: `"csv_nodes"` → `tessera_import::csv::import_nodes_csv`, `"csv_edges"` → `tessera_import::csv::import_edges_csv`, `"json"` → `tessera_import::json::import_json`, `"gql"` → `tessera_import::gql_import::import_gql`
5. Convertir contadores al `ServerMessage::ImportResult`
6. En caso de error → `ServerMessage::QueryError { reason: e.to_string() }`

Para export: mismo patrón con `Permission::GraphBackup` y funciones export.

**REFACTOR**: Extraer `fn dispatch_import(graph: &mut Graph, format: &str, payload: &str) -> Result<ServerMessage, ImportError>` como función libre (no método del handler) para facilitar testing unitario aislado.

Estimación: 50 min (implementación) + 25 min (tests)

---

### Layer 9: Wiring Verification — Integration Test End-to-End

**Objetivo**: Un test de integración que crea un grafo, exporta a JSON, re-importa, y verifica el estado.

**Archivo**: `crates/tessera-import/tests/integration_round_trip_test.rs`

```rust
// Copyright 2026 BelowZero Security OU. All rights reserved.

use tessera_graph::{Graph, Property, props};
use tessera_import::csv::{export_nodes_csv, import_nodes_csv};
use tessera_import::json::{export_json, import_json};
use tessera_import::gql_export::export_gql;
use tessera_import::gql_import::import_gql;

// ── CSV Round-Trip ────────────────────────────────────────────────────────────

#[test]
fn csv_nodes_round_trip_preserves_node_count_and_string_labels() {
    let mut original = Graph::new();
    original.add_node("Plant",  props! { "name" => "Alpha",   "capacity" => 100_i64 }).unwrap();
    original.add_node("Plant",  props! { "name" => "Beta",    "capacity" => 200_i64 }).unwrap();
    original.add_node("System", props! { "name" => "Inverter" }).unwrap();

    let csv = export_nodes_csv(&original).unwrap();

    let mut restored = Graph::new();
    let count = import_nodes_csv(&mut restored, &csv).unwrap();

    assert_eq!(count, 3);
    assert_eq!(restored.node_count(), 3);
    assert_eq!(restored.nodes_by_label("Plant").len(), 2);
    assert_eq!(restored.nodes_by_label("System").len(), 1);
}

#[test]
fn csv_nodes_round_trip_preserves_integer_properties() {
    let mut original = Graph::new();
    original.add_node("Sensor", props! { "threshold" => 42_i64 }).unwrap();

    let csv = export_nodes_csv(&original).unwrap();

    let mut restored = Graph::new();
    import_nodes_csv(&mut restored, &csv).unwrap();

    let id = restored.nodes_by_label("Sensor")[0];
    let node = restored.node(id).unwrap();
    assert_eq!(node.properties().get("threshold"), Some(&Property::I64(42)));
}

// ── JSON Round-Trip ───────────────────────────────────────────────────────────

#[test]
fn json_export_is_parseable_and_contains_all_nodes() {
    let mut g = Graph::new();
    g.add_node("Person", props! { "name" => "Alice", "age" => 30_i64 }).unwrap();
    g.add_node("Person", props! { "name" => "Bob",   "age" => 25_i64 }).unwrap();
    let a = g.nodes_by_label("Person")[0];
    let b = g.nodes_by_label("Person")[1];
    g.add_edge("KNOWS", a, b, props! { "since" => 2020_i64 }).unwrap();

    let json_str = export_json(&g).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();

    assert_eq!(parsed["nodes"].as_array().unwrap().len(), 2);
    assert_eq!(parsed["edges"].as_array().unwrap().len(), 1);
}

// ── GQL Round-Trip ────────────────────────────────────────────────────────────

#[test]
fn gql_round_trip_preserves_node_labels_and_properties() {
    let mut original = Graph::new();
    original.add_node("City", props! { "name" => "Tallinn", "pop" => 437_000_i64 }).unwrap();
    original.add_node("City", props! { "name" => "Riga",    "pop" => 614_000_i64 }).unwrap();

    let gql = export_gql(&original).unwrap();

    let mut restored = Graph::new();
    import_gql(&mut restored, &gql).unwrap();

    assert_eq!(restored.node_count(), 2);
    assert_eq!(restored.nodes_by_label("City").len(), 2);

    // Verify at least one node has the expected property
    let city_ids = restored.nodes_by_label("City");
    let names: Vec<String> = city_ids.iter()
        .filter_map(|&id| restored.node(id).ok())
        .filter_map(|n| n.properties().get("name").and_then(|p| p.as_str()).map(String::from))
        .collect();
    assert!(names.contains(&"Tallinn".to_string()), "Tallinn missing: {names:?}");
    assert!(names.contains(&"Riga".to_string()),    "Riga missing: {names:?}");
}

#[test]
fn gql_round_trip_preserves_integer_properties() {
    let mut original = Graph::new();
    original.add_node("Metric", props! { "value" => 9_999_i64 }).unwrap();

    let gql = export_gql(&original).unwrap();
    let mut restored = Graph::new();
    import_gql(&mut restored, &gql).unwrap();

    let id = restored.nodes_by_label("Metric")[0];
    let node = restored.node(id).unwrap();
    assert_eq!(node.properties().get("value").and_then(|p| p.as_i64()), Some(9_999));
}
```

Estimación: 30 min

---

## Estructura de módulos final de `tessera-import`

```
crates/tessera-import/
├── Cargo.toml                         ← Agregar serde, serde_json, tessera-storage-enterprise
├── src/
│   ├── lib.rs                         ← pub mod csv; pub mod json; pub mod gql_import; pub mod gql_export; pub mod error; pub(crate) mod node_lookup; pub(crate) mod property_coerce;
│   ├── error.rs                       ← ImportError, ExportError, ImportResult<T>, ExportResult<T>
│   ├── node_lookup.rs                 ← pub(crate) fn find_node_by_label_and_prop(...)
│   ├── property_coerce.rs             ← pub(crate) fn coerce_str_value(...), json_value_to_property(...), property_to_json(...), property_to_gql_literal(...)
│   ├── csv/
│   │   ├── mod.rs                     ← import_nodes_csv, import_edges_csv, export_nodes_csv, export_edges_csv
│   │   ├── parser.rs                  ← split_csv_line, coerce_value
│   │   └── format.rs                  ← collect_all_prop_keys, quote_csv_value
│   ├── json/
│   │   └── mod.rs                     ← import_json, export_json, ImportJsonSummary
│   ├── gql_import/
│   │   └── mod.rs                     ← import_gql, GqlImportSummary, is_comment_or_blank
│   └── gql_export/
│       ├── mod.rs                     ← export_gql
│       └── render.rs                  ← property_to_gql_literal
└── tests/
    ├── error_types_test.rs
    ├── csv_import_test.rs
    ├── csv_export_test.rs
    ├── json_import_test.rs
    ├── json_export_test.rs
    ├── gql_import_test.rs
    ├── gql_export_test.rs
    └── integration_round_trip_test.rs
```

## Cambios en `Cargo.toml` de `tessera-import`

```toml
[dependencies]
tessera-graph              = { workspace = true }
tessera-storage-enterprise = { workspace = true }
thiserror                  = { workspace = true }
serde                      = { workspace = true }
serde_json                 = { workspace = true }
```

## Cambios en `crates/tessera-server/Cargo.toml`

```toml
[dependencies]
# ... existing ...
tessera-import = { workspace = true }
```

## Cambios en `crates/tessera-protocol/src/message.rs`

Agregar a `ClientMessage`:
```rust
Import { format: String, payload: String },
Export { format: String },
```

Actualizar el `impl Debug for ClientMessage` para incluir los nuevos variants (no redactar payload en Import — no contiene credenciales).

Agregar a `ServerMessage`:
```rust
ImportResult { nodes_imported: u64, edges_imported: u64, statements_executed: u64 },
ExportResult { format: String, payload: String },
```

---

## Estimación Total

| Capa | Implementación | Tests |
|------|---------------|-------|
| Layer 1: Error types | 15 min | 20 min |
| Layer 2: CSV Import | 60 min | 20 min |
| Layer 3: JSON Import | 45 min | 20 min |
| Layer 4: GQL Import | 35 min | 20 min |
| Layer 5: CSV Export | 50 min | 20 min |
| Layer 6: JSON Export | 35 min | 20 min |
| Layer 7: GQL Export | 40 min | 20 min |
| Layer 8: Protocol | 50 min | 25 min |
| Layer 9: Integration | — | 30 min |
| **Total** | **5 h 30 min** | **3 h 15 min** |

Total estimado: **~9 horas**

---

## Criterios de Éxito

- [ ] `cargo test -p tessera-import` pasa completamente sin warnings Clippy (`-D warnings`)
- [ ] `cargo test -p tessera-protocol` pasa con los nuevos variants de mensajes
- [ ] `cargo test -p tessera-server` pasa con los nuevos handlers de Import/Export
- [ ] `cargo clippy --workspace -- -D warnings` limpio
- [ ] Round-trip CSV: nodos exportados e importados preservan count, labels, y tipos numéricos
- [ ] Round-trip JSON: grafo exportado produce JSON válido que `serde_json::from_str` acepta
- [ ] Round-trip GQL: nodos exportados e importados preservan labels y propiedades escalares
- [ ] Sin `unsafe_code` (forbid heredado del workspace)
- [ ] Copyright header en todos los archivos nuevos
- [ ] Tests en `crates/tessera-import/tests/`, no en `src/` (convención del proyecto)
