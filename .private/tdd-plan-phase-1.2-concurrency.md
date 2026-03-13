# TDD Plan: Phase 1.2 — Concurrency & Transactions

## Contexto

`tessera-graph` es un motor de grafos embebible con storage paginado (5 data files, 4096 bytes/page),
buffer pool LRU con eviction, WAL con LSN autoincremental y checkpoints. El `Graph` es actualmente
**single-threaded por diseño**: toda mutación toma `&mut self` y los dos componentes internos de
mayor actividad (`BufferPool` y `AdjCache`) usan `RefCell<T>` para interior mutability, haciendo al
tipo explícitamente `!Send + !Sync`.

Phase 1.2 requiere tres cosas de naturaleza diferente:

1. **Hacer `Graph` `Send + Sync`** con granular locking — va en `tessera-graph` MIT porque beneficia
   a todo usuario que quiera compartir un grafo entre threads sin Enterprise.
2. **Transaction manager (Begin/Commit/Rollback + WAL)** — va en `tessera-storage-enterprise` porque
   es funcionalidad de valor diferenciador que no tiene sentido distribuir bajo MIT.
3. **MVCC / Snapshot Isolation** — va en `tessera-storage-enterprise`. Requiere version chains en
   slots, visibility maps y un gestor de snapshots globales.

**Stack detectado**: Rust 2024, MSRV 1.85, `forbid(unsafe_code)`, `deny(clippy::all)`, `thiserror 2`.

**Convenciones observadas**:
- Tests unitarios `#[cfg(test)]` dentro de cada módulo.
- Tests de integración en `tests/integration/` (tessera-graph) o `tests/` en crates enterprise.
- `Box<dyn Trait>` para backends polimórficos.
- Sin `unsafe`. Sin `Rc`. `RefCell` solo aceptable en estructuras single-threaded.
- Errores vía `thiserror`, tipo `Error` centralizado en `error.rs`.
- Benchmarks con `criterion` en `benches/`.

**Afecta hot path**: Si. `add_node`, `add_edge`, cualquier escritura pasan por `write_page` /
`wal_append` / mutación de `AdjCache`. El locking granular impacta directamente el throughput de
insert. Los tests de rendimiento son obligatorios.

---

## Decisiones Previas Necesarias

Ninguna bloqueante. Las decisiones arquitectónicas ya están definidas en el roadmap y reforzadas por
el análisis del código:

- **RwLock elegido sobre Mutex** para permitir lecturas concurrentes sin contención. Válido porque
  `node()`, `edge()`, traversal y queries GQL son read-heavy.
- **Locking por segmento lógico** (no por página individual): un `RwLock` que agrupa `storage +
  adj_cache + string_heap + label_indexes` es correcto para Phase 1.2. El locking por página
  individual es Phase 2 (optimización de throughput multi-writer). En Phase 1.2 el objetivo es
  correctness de `Send + Sync`, no máximo paralelismo de escrituras.
- **`parking_lot::RwLock`** es aceptable. Justificación: no requiere `unsafe`, no añade dependencias
  de sistema, tiene semántica de fairness superior a `std::sync::RwLock` (no writer starvation), y
  es ampliamente usada en el ecosistema Rust para infraestructura de bases de datos. Si se prefiere
  quedarse en `std`, el plan sigue siendo idéntico — solo cambia el import.
- **WAL transaction boundaries**: cada `WalRecord` ya lleva LSN. Un `BEGIN` y `COMMIT` se
  representan como nuevos variantes del enum `WalRecord` existente. El `Rollback` no necesita
  escribir al WAL si las páginas no fueron flusheadas todavía (las descartamos del buffer pool).
- **TransactionId** es un `u64` generado atómicamente con `AtomicU64`. No depende del LSN.
- **MVCC slot versioning**: cada slot de 128 bytes tiene espacio en los bytes reservados (actualmente
  `0x00`) para almacenar un `xmin`/`xmax` de 4 bytes cada uno. Ver `node_codec` y `edge_codec` para
  confirmar layout. Esto requiere análisis de los codecs antes de implementar.

---

## Bloqueantes Identificados para `Send + Sync`

Tipos concretos que impiden `Send + Sync` en `Graph` hoy:

| Campo | Tipo | Problema |
|---|---|---|
| `storage` | `Box<dyn StorageBackend>` | `StorageBackend` no tiene bound `Send + Sync` |
| `adj_cache` | `AdjCache` | Contiene `RefCell<CacheInner>` → `!Send` |
| `string_heap` | `StringHeap` | Requiere revisión (probable `RefCell`) |
| `node_label_index` / `edge_label_index` | `LabelIndex` | Requiere revisión |
| `BufferPool` | `BufferPool` | Contiene `RefCell<PoolInner>` → `!Send` |

Todos los `RefCell` deben convertirse a `RwLock` (o eliminarse) en las estructuras que formarán
parte del estado compartido. `BufferPool` vive dentro de `FileBackend` que implementa
`StorageBackend`, así que la transición es por capas.

---

## Plan de Ejecución

### Fase A: `AdjCache` thread-safe (tessera-graph MIT)

**Objetivo**: Convertir `AdjCache` de `RefCell` a `RwLock` sin cambiar la API pública.
Esta es la unidad más pequeña y más fácilmente aislable.

---

**Ciclo A.1 — RED: `AdjCache` es `Send + Sync`**

- Archivo: `/Volumes/WD_BLACK/repos/MojoBytes/tessera-ecosystem/tessera-graph/src/adj_cache.rs`
- Acción: Agregar test al bloque `#[cfg(test)]` existente

```rust
#[test]
fn adj_cache_is_send_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<AdjCache>();
}
```

El test no compila porque `AdjCache` contiene `RefCell<CacheInner>`.

Estimación: 5 min

---

**GREEN A.1: Reemplazar `RefCell` por `RwLock` en `AdjCache`**

- Archivo: `/Volumes/WD_BLACK/repos/MojoBytes/tessera-ecosystem/tessera-graph/src/adj_cache.rs`
- Acción: Modificar

Cambios:
1. Reemplazar `use std::cell::RefCell` por `use std::sync::RwLock` (o `parking_lot::RwLock`).
2. Cambiar `inner: RefCell<CacheInner>` a `inner: RwLock<CacheInner>`.
3. En `get()`: `self.inner.borrow_mut()` → `self.inner.write().expect("adj_cache lock poisoned")`.
4. En `insert()`, `remove()`, `clear()`, `len()`, `is_empty()`: misma sustitución.
5. Los métodos que solo leen (`len`, `is_empty`) pueden usar `read()` en lugar de `write()`.

**REFACTOR A.1**: Extraer macro o helper `fn write_inner` para evitar repetir el `.expect()` en cada
método. Alternativamente, dado el `deny(clippy::all)`, simplemente usar `.unwrap()` con comentario
explicando que el lock poisoning es irrecuperable en este contexto.

Estimación: 20 min

---

### Fase B: `BufferPool` thread-safe (tessera-graph MIT)

**Objetivo**: `BufferPool` es más complejo porque `PageRef<'a>` devuelve un guard que contiene un
`Ref<'a, PoolInner>`. Al migrar a `RwLock`, `PageRef` debe contener un `RwLockReadGuard`.

---

**Ciclo B.1 — RED: `BufferPool` es `Send + Sync`**

- Archivo: `/Volumes/WD_BLACK/repos/MojoBytes/tessera-ecosystem/tessera-graph/src/storage/buffer_pool.rs`
- Acción: Agregar test al bloque `#[cfg(test)]` existente

```rust
#[test]
fn buffer_pool_is_send_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<BufferPool>();
}
```

Estimación: 5 min

---

**GREEN B.1: Reemplazar `RefCell` por `RwLock` en `BufferPool`**

- Archivo: `/Volumes/WD_BLACK/repos/MojoBytes/tessera-ecosystem/tessera-graph/src/storage/buffer_pool.rs`
- Acción: Modificar

Cambios:
1. `inner: RefCell<PoolInner>` → `inner: RwLock<PoolInner>`.
2. `PageRef<'a>` cambia: `borrow: Ref<'a, PoolInner>` → `borrow: RwLockReadGuard<'a, PoolInner>`.
   El `Deref` impl no cambia porque `RwLockReadGuard` también dereferencia al `PoolInner`.
3. `get_page`: el bloque `borrow_mut()` se convierte en `write()`. El borrow final `borrow()` se
   convierte en `read()`.
4. `put_page`, `flush_file`, `register_file`, `invalidate`: `borrow_mut()` → `write()`.
5. `cached_count`, `is_dirty`: `borrow()` → `read()`.
6. El método de test `pool_with_file` accede a `pool.inner.borrow()` directamente — cambiarlo a
   `pool.inner.read().unwrap()`.

**REFACTOR B.1**: Verificar que `PageRef` sigue siendo `!Send` intencionalmente (un guard que apunta
a un `RwLock` compartido sí puede ser `Send` si el contenido es `Send`). Añadir comentario
explicativo en la definición de `PageRef`.

Estimación: 30 min

---

**Ciclo B.2 — RED: Dos threads pueden leer páginas concurrentemente sin deadlock**

- Archivo: `/Volumes/WD_BLACK/repos/MojoBytes/tessera-ecosystem/tessera-graph/src/storage/buffer_pool.rs`
- Acción: Agregar test

```rust
#[test]
fn concurrent_reads_do_not_deadlock() {
    use std::sync::Arc;
    use std::thread;

    let pool = Arc::new(pool_with_file_arc(16, 4));
    // Pre-load page 0 into cache
    pool.get_page(DataFile::Nodes, 0).unwrap();
    drop(pool.get_page(DataFile::Nodes, 0).unwrap()); // drop guard before spawning

    let pool2 = Arc::clone(&pool);
    let handle = thread::spawn(move || {
        pool2.get_page(DataFile::Nodes, 0).unwrap();
    });
    pool.get_page(DataFile::Nodes, 0).unwrap();
    handle.join().unwrap();
}
```

Nota: este test requiere que `PageRef` sea `Send`. Si no lo es, el test no compilará — eso es
información útil. `PageRef` retiene un `RwLockReadGuard`; si `PoolInner: Send`, entonces
`RwLockReadGuard<PoolInner>: Send`. Verificar.

Estimación: 15 min

---

### Fase C: `StorageBackend` trait thread-safe (tessera-graph MIT)

**Objetivo**: El trait `StorageBackend` debe requerir `Send + Sync` para que `Box<dyn StorageBackend>`
sea `Send + Sync`.

---

**Ciclo C.1 — RED: `Box<dyn StorageBackend>` es `Send + Sync`**

- Archivo: `/Volumes/WD_BLACK/repos/MojoBytes/tessera-ecosystem/tessera-graph/src/storage/backend.rs`
- Acción: Agregar test en un módulo `#[cfg(test)]` nuevo al final del archivo

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn storage_backend_dyn_is_send_sync() {
        fn assert_send_sync<T: Send + Sync + ?Sized>() {}
        assert_send_sync::<dyn StorageBackend>();
    }
}
```

Estimación: 5 min

---

**GREEN C.1: Agregar bounds `Send + Sync` al trait `StorageBackend`**

- Archivo: `/Volumes/WD_BLACK/repos/MojoBytes/tessera-ecosystem/tessera-graph/src/storage/backend.rs`
- Acción: Modificar la declaración del trait

```rust
pub trait StorageBackend: Send + Sync {
    // ... métodos sin cambios
}
```

Esto causará errores de compilación en `MemoryBackend` y `FileBackend` si algún campo suyo es
`!Send` o `!Sync`. Esos errores guían la Fase D.

Estimación: 10 min

---

### Fase D: `MemoryBackend` y `FileBackend` implementan `Send + Sync` (tessera-graph MIT)

**Objetivo**: Después de Fase B y C, `FileBackend` debería ser `Send + Sync` automáticamente
(contiene `BufferPool` ya thread-safe y `WalWriter` que contiene `File` que es `Send`). Verificar y
corregir lo que falte.

---

**Ciclo D.1 — RED: `FileBackend` y `MemoryBackend` son `Send + Sync`**

- Archivo: `/Volumes/WD_BLACK/repos/MojoBytes/tessera-ecosystem/tessera-graph/src/storage/file.rs`
- Acción: Agregar test en `#[cfg(test)]`

```rust
#[test]
fn file_backend_is_send_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<FileBackend>();
}
```

- Archivo: `/Volumes/WD_BLACK/repos/MojoBytes/tessera-ecosystem/tessera-graph/src/storage/memory.rs`
- Acción: Agregar test similar para `MemoryBackend`.

Estimación: 5 min

---

**GREEN D.1: Corregir cualquier tipo `!Send`/`!Sync` restante en backends**

- Archivos: `file.rs`, `memory.rs`
- Acción: Modificar si el compilador reporta errores

`MemoryBackend` solo contiene `HashMap`, `GraphMeta` (primitivos) → ya es `Send + Sync`.
`FileBackend` contiene `BufferPool` (ya corregido), `GraphMeta`, `Option<WalWriter>`. `WalWriter`
contiene `File` que es `Send` en Rust standard. Probablemente compile limpio.

Estimación: 15 min (incluyendo debugging si hay sorpresas)

---

### Fase E: `Graph` envuelto en `RwLock` — API `SharedGraph` (tessera-graph MIT)

**Objetivo**: `Graph` en sí mismo no necesita ser `Send + Sync` si todas sus dependencias internas
lo son. La forma correcta de exponer un grafo thread-safe es wrappear `Graph` en `Arc<RwLock<Graph>>`
y ofrecer un tipo `SharedGraph` que implementa una API paralela.

Esta es una decisión de diseño deliberada: no convertir `Graph` en algo que internamente gestiona
su propio locking (que limitaría la composabilidad), sino ofrecer `SharedGraph` como un thin wrapper
que expone exactamente la misma API pero tomando locks de forma transparente.

---

**Ciclo E.1 — RED: `SharedGraph::new()` existe y es `Send + Sync + Clone`**

- Archivo: `/Volumes/WD_BLACK/repos/MojoBytes/tessera-ecosystem/tessera-graph/src/graph.rs`
  (o nuevo módulo `src/shared_graph.rs` si la complejidad lo justifica)
- Acción: Agregar test

```rust
#[test]
fn shared_graph_is_send_sync_clone() {
    fn assert_send_sync_clone<T: Send + Sync + Clone>() {}
    assert_send_sync_clone::<SharedGraph>();
}
```

Estimación: 5 min

---

**GREEN E.1: Implementar `SharedGraph`**

- Archivo: `/Volumes/WD_BLACK/repos/MojoBytes/tessera-ecosystem/tessera-graph/src/graph.rs`
  (añadir al final, o `src/shared_graph.rs` pub re-exportado desde `lib.rs`)
- Acción: Crear

```rust
/// Thread-safe handle to a `Graph`, backed by `Arc<RwLock<Graph>>`.
///
/// Clone is cheap (increments refcount). All methods acquire the appropriate
/// read or write lock internally. For batch operations, use `read()` / `write()`
/// directly to hold the lock across multiple calls.
#[derive(Clone)]
pub struct SharedGraph {
    inner: Arc<RwLock<Graph>>,
}

impl SharedGraph {
    pub fn new(graph: Graph) -> Self { ... }
    pub fn read(&self) -> RwLockReadGuard<'_, Graph> { ... }
    pub fn write(&self) -> RwLockWriteGuard<'_, Graph> { ... }

    // Delegating convenience methods (add_node, add_edge, node, edge, node_count, ...)
    pub fn add_node(&self, label: &str, props: Properties) -> Result<NodeId> {
        self.inner.write().unwrap().add_node(label, props)
    }
    // etc.
}
```

Re-exportar desde `lib.rs`:
```rust
pub use graph::SharedGraph;
```

**REFACTOR E.1**: Evaluar si los métodos delegados deben estar en `SharedGraph` directamente o si
basta con exponer `read()` / `write()`. Para evitar código repetido, inicialmente solo exponer
`read()` y `write()` y documentar el patrón. Los métodos delegados pueden añadirse en ciclos
posteriores según necesidad.

Estimación: 30 min

---

**Ciclo E.2 — RED: Dos threads pueden escribir en `SharedGraph` sin data races**

- Archivo: Tests de integración en
  `/Volumes/WD_BLACK/repos/MojoBytes/tessera-ecosystem/tessera-graph/tests/integration/`
- Acción: Crear nuevo archivo `tests/integration/concurrency.rs`

```rust
use std::sync::Arc;
use std::thread;
use tessera_graph::{SharedGraph, Graph, props};

#[test]
fn two_writers_do_not_corrupt_graph() {
    let graph = SharedGraph::new(Graph::new());
    let n_threads = 4;
    let ops_per_thread = 100;

    let handles: Vec<_> = (0..n_threads).map(|_| {
        let g = graph.clone();
        thread::spawn(move || {
            for i in 0..ops_per_thread {
                g.write().unwrap()
                    .add_node("Worker", props! { "i" => i as i64 })
                    .unwrap();
            }
        })
    }).collect();

    for h in handles { h.join().unwrap(); }

    assert_eq!(graph.read().unwrap().node_count(), n_threads * ops_per_thread);
}
```

Estimación: 20 min

---

**Ciclo E.3 — RED: Un reader no bloquea a otro reader**

- Archivo: `tests/integration/concurrency.rs`
- Acción: Añadir test

```rust
#[test]
fn concurrent_readers_do_not_block_each_other() {
    use std::time::{Duration, Instant};

    let graph = SharedGraph::new(Graph::new());
    {
        let mut g = graph.write().unwrap();
        for i in 0..1000_u64 {
            g.add_node("N", props! { "v" => i as i64 }).unwrap();
        }
    }

    let start = Instant::now();
    let handles: Vec<_> = (0..8).map(|_| {
        let g = graph.clone();
        thread::spawn(move || {
            let _count = g.read().unwrap().node_count();
        })
    }).collect();
    for h in handles { h.join().unwrap(); }

    // 8 concurrent readers should complete in well under 100ms
    assert!(start.elapsed() < Duration::from_millis(100));
}
```

Estimación: 15 min

---

### Fase F: Benchmark de regresión de throughput (tessera-graph MIT)

**OBLIGATORIO — hot path afectado.**

---

**Ciclo F.1 — RED: Baseline de throughput documentado antes del locking**

- Archivo: `/Volumes/WD_BLACK/repos/MojoBytes/tessera-ecosystem/tessera-graph/benches/graph_write.rs`
  (ya existe — añadir benchmark específico para `SharedGraph`)
- Acción: Agregar grupo de benchmark

```rust
// En graph_write.rs, añadir:
fn bench_shared_graph_add_node(c: &mut Criterion) {
    use tessera_graph::SharedGraph;

    let mut group = c.benchmark_group("shared_graph_write");
    group.throughput(Throughput::Elements(1));

    group.bench_function("add_node_single_thread", |b| {
        b.iter_batched(
            || SharedGraph::new(Graph::new()),
            |g| {
                g.write().unwrap()
                    .add_node("N", props! { "x" => 1_i64 })
                    .unwrap()
            },
            BatchSize::SmallInput,
        );
    });

    group.finish();
}
```

**Criterio de aceptación**: El throughput de `SharedGraph::add_node` single-thread NO debe degradar
más de un 15% respecto al throughput de `Graph::add_node` single-thread medido en el mismo
benchmark group. El overhead del `RwLock::write()` es determinista y bajo (~10 ns en x86_64).

---

**Ciclo F.2 — RED: Test de regresión de throughput como test de compilación**

- Archivo: `/Volumes/WD_BLACK/repos/MojoBytes/tessera-ecosystem/tessera-graph/src/graph.rs`
- Acción: Añadir test de regresión con medición de tiempo mínimo

```rust
#[cfg(test)]
mod throughput_regression {
    use super::*;
    use std::time::Instant;

    /// Regresión de throughput: SharedGraph debe procesar al menos 50.000
    /// add_node/s en single-thread (conservador, el target real es > 200k/s
    /// según benchmarks existentes de Graph::add_node en memoria).
    ///
    /// Umbral elegido para ser robusto en CI (máquinas lentas), no para medir
    /// rendimiento peak. Los benchmarks de criterion son la fuente de verdad.
    #[test]
    fn shared_graph_add_node_throughput_floor() {
        use crate::SharedGraph;
        let g = SharedGraph::new(Graph::new());
        let n = 10_000_u64;
        let start = Instant::now();
        for i in 0..n {
            g.write().unwrap()
                .add_node("N", crate::property::Properties::default())
                .unwrap();
        }
        let elapsed = start.elapsed();
        let ops_per_sec = n as f64 / elapsed.as_secs_f64();
        assert!(
            ops_per_sec > 50_000.0,
            "throughput regression: {ops_per_sec:.0} ops/s < 50_000 ops/s floor"
        );
    }
}
```

Estimación: 15 min

---

### Fase G: WAL — nuevos record types para transacciones (tessera-graph MIT)

**Objetivo**: Extender `WalRecord` con `Begin`, `Commit`, `Rollback` para que el Transaction Manager
de Enterprise pueda escribir boundaries en el WAL. Estos tipos viven en el WAL MIT porque el WAL
recovery debe entenderlos para poder ignorar/deshacer transacciones incompletas en crash recovery.

---

**Ciclo G.1 — RED: `WalRecord` tiene variantes `Begin`, `Commit`, `Rollback`**

- Archivo: `/Volumes/WD_BLACK/repos/MojoBytes/tessera-ecosystem/tessera-graph/src/wal/record.rs`
- Acción: Agregar test

```rust
#[test]
fn encode_decode_begin_commit_rollback() {
    let begin = WalRecord::Begin { lsn: 0, txn_id: 42 };
    let bytes = encode(&begin);
    let (decoded, _) = decode(&bytes).unwrap();
    assert!(matches!(decoded, WalRecord::Begin { txn_id: 42, .. }));

    let commit = WalRecord::Commit { lsn: 0, txn_id: 42 };
    let bytes = encode(&commit);
    let (decoded, _) = decode(&bytes).unwrap();
    assert!(matches!(decoded, WalRecord::Commit { txn_id: 42, .. }));

    let rollback = WalRecord::Rollback { lsn: 0, txn_id: 42 };
    let bytes = encode(&rollback);
    let (decoded, _) = decode(&bytes).unwrap();
    assert!(matches!(decoded, WalRecord::Rollback { txn_id: 42, .. }));
}
```

Estimación: 10 min

---

**GREEN G.1: Agregar variantes al enum `WalRecord`**

- Archivo: `/Volumes/WD_BLACK/repos/MojoBytes/tessera-ecosystem/tessera-graph/src/wal/record.rs`
- Acción: Modificar

```rust
// Nuevas constantes de tag
const TAG_BEGIN: u8    = 0x09;
const TAG_COMMIT: u8   = 0x0A;
const TAG_ROLLBACK: u8 = 0x0B;

// Nuevas variantes en WalRecord
pub enum WalRecord {
    // ... variantes existentes ...
    /// Transaction begin marker.
    Begin    { lsn: u64, txn_id: u64 },
    /// Transaction commit marker.
    Commit   { lsn: u64, txn_id: u64 },
    /// Transaction rollback marker.
    Rollback { lsn: u64, txn_id: u64 },
}
```

Formato binario para las nuevas variantes: `[tag][lsn:u64][txn_id:u64]` — reutilizar `encode_id`
con un payload de 16 bytes (lsn + txn_id) o añadir `encode_txn` helper siguiendo el patrón
existente.

Actualizar también:
- `impl WalRecord { fn lsn() }` — añadir los tres arms nuevos al match.
- `encode()` — añadir los tres arms.
- `decode()` — añadir los tres tags.
- `set_lsn()` en `writer.rs` — añadir los tres arms.

**REFACTOR G.1**: Los tres nuevos variantes tienen el mismo shape (`lsn: u64, txn_id: u64`).
Evaluar si `encode_txn` / `decode_txn` reducen duplicación sin sacrificar claridad.

Estimación: 25 min

---

### Fase H: `TransactionManager` (tessera-storage-enterprise)

**Objetivo**: Implementar el gestor de transacciones que usa `SharedGraph` + WAL para ofrecer
Begin/Commit/Rollback con isolation level configurable.

---

**Ciclo H.1 — RED: `TransactionManager::begin()` devuelve un `TransactionHandle`**

- Archivo: `/Volumes/WD_BLACK/repos/MojoBytes/tessera-ecosystem/tessera-graph-enterprise/crates/tessera-storage-enterprise/src/lib.rs`
  (o nuevo módulo `src/txn/mod.rs`)
- Acción: Crear módulo `src/txn/` con `mod.rs`, `manager.rs`, `handle.rs`

```rust
// En tests del módulo manager.rs
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn begin_returns_handle_with_unique_id() {
        let mgr = TransactionManager::new();
        let t1 = mgr.begin(IsolationLevel::ReadCommitted);
        let t2 = mgr.begin(IsolationLevel::ReadCommitted);
        assert_ne!(t1.txn_id(), t2.txn_id());
    }
}
```

Estimación: 10 min

---

**GREEN H.1: Implementar `TransactionManager`, `TransactionHandle`, `IsolationLevel`**

- Archivo: `/Volumes/WD_BLACK/repos/MojoBytes/tessera-ecosystem/tessera-graph-enterprise/crates/tessera-storage-enterprise/src/txn/manager.rs`
- Acción: Crear

```rust
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

pub struct TransactionManager {
    next_txn_id: AtomicU64,
    // Fase H usa solo ID generation; gestión de estado activo viene en H.3
}

impl TransactionManager {
    pub fn new() -> Self {
        Self { next_txn_id: AtomicU64::new(1) }
    }

    pub fn begin(&self, isolation: IsolationLevel) -> TransactionHandle {
        let txn_id = self.next_txn_id.fetch_add(1, Ordering::Relaxed);
        TransactionHandle { txn_id, isolation, state: TxnState::Active }
    }
}
```

```rust
// handle.rs
pub struct TransactionHandle {
    pub(crate) txn_id: u64,
    pub(crate) isolation: IsolationLevel,
    pub(crate) state: TxnState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IsolationLevel {
    ReadUncommitted,
    ReadCommitted,
    SnapshotIsolation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxnState {
    Active,
    Committed,
    RolledBack,
}

impl TransactionHandle {
    pub fn txn_id(&self) -> u64 { self.txn_id }
    pub fn state(&self) -> TxnState { self.state }
}
```

Estimación: 25 min

---

**Ciclo H.2 — RED: `commit()` escribe `WalRecord::Commit` y marca el handle**

- Archivo: `src/txn/manager.rs`
- Acción: Agregar test

```rust
#[test]
fn commit_writes_wal_record_and_transitions_state() {
    // Este test requiere un mock de WAL o uso de tessera-graph's WalWriter
    // sobre un archivo temporal.
    use tempfile::NamedTempFile;
    use tessera_graph::Graph;

    let tmp = NamedTempFile::new().unwrap();
    let mgr = TransactionManager::new();
    let mut handle = mgr.begin(IsolationLevel::ReadCommitted);

    mgr.commit(&mut handle, tmp.path()).unwrap();

    assert_eq!(handle.state(), TxnState::Committed);

    // Verificar que el WAL contiene el record Commit
    use tessera_graph::wal::reader::WalReader;
    let reader = WalReader::open(tmp.path()).unwrap();
    let records: Vec<_> = reader.records().collect();
    assert!(records.iter().any(|r| matches!(r,
        tessera_graph::wal::record::WalRecord::Commit { txn_id, .. } if *txn_id == handle.txn_id()
    )));
}
```

Nota: Este test requiere que `tessera_graph::wal` sea pub (actualmente es `mod wal` en lib.rs —
revisar visibilidad). Si no está pub, hay que añadir `pub use wal::...` o exponer lo necesario.

Estimación: 15 min

---

**GREEN H.2: Implementar `commit()` y `rollback()` en `TransactionManager`**

- Archivo: `src/txn/manager.rs`
- Acción: Modificar

```rust
use tessera_graph::wal::{record::WalRecord, writer::WalWriter};
use crate::error::EnterpriseError;

impl TransactionManager {
    pub fn commit(
        &self,
        handle: &mut TransactionHandle,
        wal_path: &std::path::Path,
    ) -> Result<(), EnterpriseError> {
        if handle.state != TxnState::Active {
            return Err(EnterpriseError::TransactionNotActive(handle.txn_id));
        }
        let mut wal = WalWriter::open(wal_path)?;
        wal.append(WalRecord::Commit { lsn: 0, txn_id: handle.txn_id })?;
        wal.sync()?;
        handle.state = TxnState::Committed;
        Ok(())
    }

    pub fn rollback(
        &self,
        handle: &mut TransactionHandle,
        wal_path: &std::path::Path,
    ) -> Result<(), EnterpriseError> {
        if handle.state != TxnState::Active {
            return Err(EnterpriseError::TransactionNotActive(handle.txn_id));
        }
        let mut wal = WalWriter::open(wal_path)?;
        wal.append(WalRecord::Rollback { lsn: 0, txn_id: handle.txn_id })?;
        wal.sync()?;
        handle.state = TxnState::RolledBack;
        Ok(())
    }
}
```

Crear `src/error.rs` en `tessera-storage-enterprise` con:

```rust
#[derive(Debug, thiserror::Error)]
pub enum EnterpriseError {
    #[error("transaction {0} is not active")]
    TransactionNotActive(u64),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("graph error: {0}")]
    Graph(#[from] tessera_graph::Error),
}
```

Estimación: 30 min

---

**Ciclo H.3 — RED: Commit en transacción ya committed devuelve error**

- Archivo: `src/txn/manager.rs`
- Acción: Agregar test

```rust
#[test]
fn double_commit_returns_error() {
    use tempfile::NamedTempFile;
    let tmp = NamedTempFile::new().unwrap();
    let mgr = TransactionManager::new();
    let mut handle = mgr.begin(IsolationLevel::ReadCommitted);
    mgr.commit(&mut handle, tmp.path()).unwrap();
    assert!(mgr.commit(&mut handle, tmp.path()).is_err());
}
```

Estimación: 5 min (ya cubierto por la implementación de H.2 — solo verificar que el test pasa)

---

### Fase I: `ReadCommitted` — visibilidad de datos committados (tessera-storage-enterprise)

**Objetivo**: Implementar la semántica de Read Committed: una operación de lectura solo ve datos
cuyo `COMMIT` record está presente en el WAL o en la base committed. Esto requiere un `CommitLog`
en memoria que el `TransactionManager` mantiene.

---

**Ciclo I.1 — RED: Datos escritos por una transacción no committada no son visibles**

- Archivo: `tests/` en `tessera-storage-enterprise` (test de integración)
  o directamente en `src/txn/manager.rs` con un test aislado
- Acción: Crear test que ilustra el contract de Read Committed

```rust
#[test]
fn read_committed_does_not_see_uncommitted_data() {
    // T1 comienza y escribe. T2 lee antes de que T1 haga commit.
    // T2 NO debe ver los datos de T1.
    //
    // Implementación Phase I: el TransactionManager mantiene un
    // CommitSet (HashSet<u64> de txn_ids commiteados) y expone
    // fn is_committed(txn_id: u64) -> bool.
    // El caller es responsable de chequear visibilidad antes de leer.
    // MVCC completo (visibility en nivel de slot) es Fase MVCC.

    let mgr = TransactionManager::new();
    let t1 = mgr.begin(IsolationLevel::ReadCommitted);
    let t2 = mgr.begin(IsolationLevel::ReadCommitted);

    // T1 no ha hecho commit
    assert!(!mgr.is_committed(t1.txn_id()));
    // T2 tampoco
    assert!(!mgr.is_committed(t2.txn_id()));
}
```

Estimación: 10 min

---

**GREEN I.1: Implementar `CommitLog` en `TransactionManager`**

- Archivo: `src/txn/manager.rs`
- Acción: Modificar

```rust
use std::sync::RwLock;
use std::collections::HashSet;

pub struct TransactionManager {
    next_txn_id: AtomicU64,
    committed: RwLock<HashSet<u64>>,
}

impl TransactionManager {
    pub fn is_committed(&self, txn_id: u64) -> bool {
        self.committed.read()
            .expect("commit log lock poisoned")
            .contains(&txn_id)
    }

    // En commit(): después de wal.sync(), insertar en committed
    // self.committed.write()?.insert(handle.txn_id);
}
```

Estimación: 15 min

---

**Ciclo I.2 — RED: Después de commit, `is_committed` retorna true**

```rust
#[test]
fn after_commit_txn_is_visible() {
    use tempfile::NamedTempFile;
    let tmp = NamedTempFile::new().unwrap();
    let mgr = TransactionManager::new();
    let mut handle = mgr.begin(IsolationLevel::ReadCommitted);
    let id = handle.txn_id();
    mgr.commit(&mut handle, tmp.path()).unwrap();
    assert!(mgr.is_committed(id));
}
```

Estimación: 5 min

---

### Fase J: Snapshot Isolation — `SnapshotId` y visibility (tessera-storage-enterprise)

**Objetivo**: Implementar Snapshot Isolation via MVCC. Este es el ciclo más complejo y requiere
decisions sobre el layout de versiones en slots. La implementación completa de version chains en
slots es un trabajo de semanas; Phase J implementa la infraestructura de snapshots (el "cuándo se
tomó la foto") como precondición para el trabajo de slot versioning posterior.

---

**Ciclo J.1 — RED: `SnapshotId` capture el estado committed en el momento de `begin`**

- Archivo: `src/txn/snapshot.rs` (nuevo)
- Acción: Crear y agregar test

```rust
#[test]
fn snapshot_captures_committed_set_at_begin_time() {
    use tempfile::NamedTempFile;
    let tmp = NamedTempFile::new().unwrap();
    let mgr = TransactionManager::new();

    // T1 commitea antes de que T2 comience
    let mut t1 = mgr.begin(IsolationLevel::SnapshotIsolation);
    let t1_id = t1.txn_id();
    mgr.commit(&mut t1, tmp.path()).unwrap();

    // T2 comienza — su snapshot DEBE incluir T1
    let t2 = mgr.begin(IsolationLevel::SnapshotIsolation);
    let snap = t2.snapshot().expect("SI handle must have snapshot");
    assert!(snap.is_visible(t1_id));

    // T3 comienza después de T2 — T3 NO debe ser visible en el snapshot de T2
    let t3 = mgr.begin(IsolationLevel::SnapshotIsolation);
    assert!(!snap.is_visible(t3.txn_id()));
}
```

Estimación: 15 min

---

**GREEN J.1: Implementar `Snapshot` y lógica de captura en `begin`**

- Archivo: `src/txn/snapshot.rs`
- Acción: Crear

```rust
/// Immutable snapshot of the committed transaction set at a point in time.
///
/// A transaction with SnapshotIsolation sees exactly the transactions that
/// were committed when its snapshot was taken, plus its own writes.
pub struct Snapshot {
    committed_at_begin: HashSet<u64>,
    /// The txn_id of the transaction that owns this snapshot.
    owner_txn_id: u64,
}

impl Snapshot {
    /// Returns true if the data written by `writer_txn_id` is visible
    /// to this snapshot.
    pub fn is_visible(&self, writer_txn_id: u64) -> bool {
        writer_txn_id == self.owner_txn_id
            || self.committed_at_begin.contains(&writer_txn_id)
    }
}
```

En `TransactionManager::begin()`, para `IsolationLevel::SnapshotIsolation`:
```rust
let snapshot = Snapshot {
    committed_at_begin: self.committed.read().unwrap().clone(),
    owner_txn_id: txn_id,
};
TransactionHandle { ..., snapshot: Some(snapshot) }
```

Estimación: 25 min

---

**REFACTOR J.1**: `HashSet::clone()` en cada `begin()` es O(n) donde n = transacciones
committadas. Para Phase 1.2 esto es aceptable (el número de txns concurrentes es bajo en
arranque de producto). En Phase 2 (optimización) se reemplazará por una estructura de versiones
más eficiente (e.g., epoch-based o MVCC timestamp).

---

### Fase K: Tests de integración end-to-end (tessera-storage-enterprise)

**Objetivo**: Un test que ejercite el flujo completo: `SharedGraph` + `TransactionManager` +
`ReadCommitted` isolation.

---

**Ciclo K.1 — RED: Flujo completo Begin → write → Commit → visible**

- Archivo: `crates/tessera-storage-enterprise/tests/txn_integration.rs`
- Acción: Crear

```rust
use tessera_storage_enterprise::txn::{TransactionManager, IsolationLevel};
use tessera_graph::{SharedGraph, Graph, props};
use tempfile::NamedTempFile;

#[test]
fn committed_write_is_visible_via_read_committed() {
    let wal_tmp = NamedTempFile::new().unwrap();
    let graph = SharedGraph::new(Graph::new());
    let mgr = TransactionManager::new();

    let mut txn = mgr.begin(IsolationLevel::ReadCommitted);

    // Escritura dentro de la transacción
    let node_id = graph.write().unwrap()
        .add_node("Person", props! { "name" => "Alice" })
        .unwrap();

    // Antes del commit, la txn no está en el commit log
    assert!(!mgr.is_committed(txn.txn_id()));

    mgr.commit(&mut txn, wal_tmp.path()).unwrap();

    // Después del commit, la txn está en el commit log
    assert!(mgr.is_committed(txn.txn_id()));

    // El nodo existe en el grafo (grafo no tiene isolation a nivel de slot todavía)
    let node = graph.read().unwrap().node(node_id).unwrap();
    assert_eq!(node.label(), "Person");
}
```

Estimación: 20 min

---

**Ciclo K.2 — RED: Rollback no hace commit visible**

- Archivo: `tests/txn_integration.rs`
- Acción: Añadir test

```rust
#[test]
fn rolled_back_transaction_is_not_committed() {
    let wal_tmp = NamedTempFile::new().unwrap();
    let mgr = TransactionManager::new();
    let mut txn = mgr.begin(IsolationLevel::ReadCommitted);
    let id = txn.txn_id();
    mgr.rollback(&mut txn, wal_tmp.path()).unwrap();
    assert!(!mgr.is_committed(id));
}
```

Estimación: 5 min

---

## Estimación Total

| Fase | Descripción | Estimación |
|---|---|---|
| A | `AdjCache` thread-safe | 25 min |
| B | `BufferPool` thread-safe | 50 min |
| C | `StorageBackend` trait bounds | 15 min |
| D | `MemoryBackend` / `FileBackend` Send+Sync | 20 min |
| E | `SharedGraph` wrapper | 70 min |
| F | Benchmark de regresión | 30 min |
| G | WAL record types Begin/Commit/Rollback | 35 min |
| H | `TransactionManager` + `TransactionHandle` | 85 min |
| I | Read Committed semantics | 30 min |
| J | Snapshot Isolation + `Snapshot` struct | 40 min |
| K | Tests de integración end-to-end | 25 min |
| **Total** | | **~7.5 horas** |

---

## Criterios de Éxito

- [ ] `Graph` (y `SharedGraph`) satisfacen los bounds `Send + Sync` verificados por tests de
      compilación.
- [ ] `BufferPool` y `AdjCache` usan `RwLock` — cero `RefCell` en código visible a múltiples threads.
- [ ] Ningún test existente en `tessera-graph` falla (regresión cero).
- [ ] `cargo clippy --all-targets` pasa sin warnings (tratados como errores).
- [ ] Throughput de `SharedGraph::add_node` single-thread >= 50.000 ops/s (piso de regresión en CI).
- [ ] Throughput medido con criterion no degrada mas de 15% vs `Graph::add_node`.
- [ ] `WalRecord::Begin`, `::Commit`, `::Rollback` se encode/decode correctamente con CRC.
- [ ] `TransactionManager` genera IDs únicos, monotónicos (AtomicU64).
- [ ] `commit()` y `rollback()` en txn no activa retornan `EnterpriseError::TransactionNotActive`.
- [ ] `is_committed()` retorna true solo después de commit exitoso.
- [ ] Snapshot de SI captura exactly el committed set en el momento del `begin`.
- [ ] Tests de integración K.1 y K.2 pasan.
- [ ] `cargo test --workspace` completo pasa en ambos repos (tessera-graph y tessera-graph-enterprise).

---

## Notas de Implementación

### Orden de ejecución obligatorio

Las fases tienen dependencias lineales: A → B → C → D → E → F (puede ser paralelo a G) → G → H → I → J → K.

No saltar de D a H sin completar E y G: `TransactionManager` depende de `SharedGraph` (E) y de los
nuevos `WalRecord` types (G).

### Visibilidad de módulos WAL

`tessera-graph/src/lib.rs` declara `mod wal` como privado. Para que `tessera-storage-enterprise`
use `WalWriter` directamente, hay dos opciones:
1. Exponer `pub use wal::writer::WalWriter` desde `lib.rs` (recomendado — limpio).
2. Hacer `pub mod wal` en `lib.rs` (expone todo el módulo, menos preciso).

La opción 1 es preferible. Añadir en `lib.rs`:
```rust
pub use wal::writer::WalWriter;
pub use wal::reader::WalReader;
pub use wal::record::WalRecord;
```

### `parking_lot` vs `std::sync`

Si se decide usar `parking_lot`:
- Añadir `parking_lot = "0.12"` a `[dependencies]` en `tessera-graph/Cargo.toml`.
- Ventaja: `RwLock::read()` / `write()` retornan guards directamente sin `Result` (no lock
  poisoning), eliminando todos los `.unwrap()` / `.expect()`.
- Desventaja: dependencia externa adicional en el crate MIT público.
- Decisión: dejar a discreción del implementador. El plan es válido con ambas opciones.

### Slot versioning (MVCC profundo) — fuera de scope de Phase 1.2

El plan no incluye modificación del layout de slots de 128 bytes para almacenar `xmin`/`xmax`.
Eso es Phase 1.3 o P1 según el roadmap. Phase 1.2 implementa la infraestructura de control
(locking, transaction IDs, commit log, snapshots) sin modificar el storage físico de datos.
La visibilidad a nivel de slot vendrá cuando se modifiquen `node_codec` y `edge_codec`.
