# TDD Plan: Enterprise Adjacency Index

## Contexto

El `resolve_adj_pointer()` del MIT core hace un escaneo lineal O(N) de todas las páginas de adyacencia cuando el `AdjCache` (capacidad 65K) no tiene la entrada. Para grafos con más de 65K nodos, cualquier operación sobre un nodo fuera del cache degenera a O(N). Bulk inserts de 1M aristas se vuelven O(N²).

La solución: un `AdjacencyIndex` en la capa enterprise — `HashMap<NodeId, AdjacencyPointer>` que se mantiene sincronizado en cada mutación. Cada lookup pasa de O(N) páginas a O(1).

## Bloqueante Arquitectónico Detectado

`AdjacencyPointer` contiene `outgoing_page: Option<u32>` e `incoming_page: Option<u32>` — son índices de páginas internos al `Graph`. **No están expuestos por `GraphAccess`**. El enterprise layer solo ve `GraphAccess`.

Esto significa que el plan requiere **3 cambios mínimos en el MIT core** (exposición de API existente, no lógica nueva):

1. Re-exportar `AdjacencyPointer` desde `tessera_graph::lib.rs`
2. Añadir `fn adj_pointer(&self, node: NodeId) -> Option<AdjacencyPointer>` a `GraphAccess` (default `None`, impl concreta en `Graph` delega a `resolve_adj_pointer`)
3. Añadir `fn set_adj_pointer(&self, node: NodeId, ptr: AdjacencyPointer)` a `GraphAccess` (para pre-warm el `AdjCache` desde el índice enterprise)

---

## Plan de Ejecución

### Fase 0 (MIT core) — Pre-requisito obligatorio

#### Paso 1: Re-exportar AdjacencyPointer (10 min)
- Archivo: `tessera-graph/src/lib.rs`
- Acción: `pub use storage::codec::adjacency_codec::AdjacencyPointer;`

#### Paso 2: Añadir `adj_pointer` a GraphAccess (15 min)
- Archivo: `tessera-graph/src/access.rs`
- Acción: método en trait con default `None`:
  ```rust
  fn adj_pointer(&self, _node: NodeId) -> Option<AdjacencyPointer> { None }
  ```

#### Paso 3: Añadir `set_adj_pointer` a GraphAccess (15 min)
- Archivo: `tessera-graph/src/access.rs`
- Acción: método en trait con default no-op:
  ```rust
  fn set_adj_pointer(&self, _node: NodeId, _ptr: AdjacencyPointer) {}
  ```

#### Paso 4: Implementar ambos en `impl GraphAccess for Graph` (20 min)
- Archivo: `tessera-graph/src/access.rs` o `graph.rs`
- `adj_pointer`: `self.resolve_adj_pointer(node.0).ok().flatten()`
- `set_adj_pointer`: `self.adj_cache.insert(node.0, ptr)`

#### Paso 5: Tests unitarios (15 min)
- Test: nodo con edges → `adj_pointer` retorna `Some` con páginas válidas
- Test: nodo sin edges → `adj_pointer` retorna `None`
- Test: `set_adj_pointer` seguido de `adj_pointer` retorna el valor seteado

---

### Fase 1 (Enterprise) — AdjacencyIndex struct

#### Ciclo 1: Struct existe y arranca vacía
- **RED**: Test `test_adj_index_starts_empty` — `AdjacencyIndex::new()`, `get(NodeId::from_raw(0))` retorna `None`
  - Assert: `index.get(NodeId::from_raw(0)).is_none()`
- **GREEN**: `pub struct AdjacencyIndex { inner: HashMap<NodeId, AdjacencyPointer> }` con `new()` y `get()`
- **REFACTOR**: No aplica

#### Ciclo 2: insert y lookup
- **RED**: Test `test_adj_index_insert_and_get` — inserta ptr para nodo 1, lo recupera
  - Assert: `index.get(node) == Some(ptr)` y `index.get(otro_nodo).is_none()`
- **GREEN**: Implementar `pub fn insert(&mut self, node: NodeId, ptr: AdjacencyPointer)`
- **REFACTOR**: No aplica

#### Ciclo 3: remove
- **RED**: Test `test_adj_index_remove_clears_entry` — inserta, remove, get retorna None
  - Assert: `index.get(node).is_none()` tras `index.remove(node)`
- **GREEN**: Implementar `pub fn remove(&mut self, node: NodeId)`
- **REFACTOR**: No aplica

#### Ciclo 4: update parcial sin sobrescribir
- **RED**: Test `test_adj_index_update_preserves_other_page` — inserta ptr con ambas páginas, `update_outgoing(node, Some(99))`, verifica incoming no cambió
  - Assert: `ptr.outgoing_page == Some(99)` y `ptr.incoming_page` igual al original
- **GREEN**: `pub fn update_outgoing(&mut self, node: NodeId, page: Option<u32>)` y `update_incoming`
- **REFACTOR**: Extraer helper privado si hay duplicación

#### Paso: Exponer módulo (5 min)
- Archivo: `tessera-graph-storage/src/lib.rs`
- Acción: `pub mod adj_index;`

---

### Fase 2 (Enterprise) — Integración en NeighborCache

#### Ciclo 5: NeighborCache tiene un AdjacencyIndex
- **RED**: Test `test_neighbor_cache_adj_index_empty_for_new_node` — tras `add_node`, `cache.adj_index().get(node)` retorna `None`
  - Assert: `cache.adj_index().get(new_node_id).is_none()`
- **GREEN**: Añadir campo `adj_index: AdjacencyIndex` a `NeighborCache`, accessor `pub fn adj_index(&self) -> &AdjacencyIndex`
- **REFACTOR**: No aplica

#### Ciclo 6: add_edge popula el índice
- **RED**: Test `test_adj_index_populated_after_add_edge` — añade 2 nodos y 1 arista, `adj_index().get(source)` retorna `Some(ptr)` con `outgoing_page.is_some()`
  - Assert: `cache.adj_index().get(source).unwrap().outgoing_page.is_some()`
- **GREEN**: En `NeighborCache::add_edge`, tras `self.inner.add_edge(...)`, llamar `self.inner.adj_pointer(source)` y `self.inner.adj_pointer(target)` e insertar en `self.adj_index`
- **REFACTOR**: No aplica

#### Ciclo 7: remove_edge y remove_node actualizan el índice
- **RED**: Test `test_adj_index_updated_after_remove_edge` — add edge, remove edge, verificar que el índice refleja el cambio
- **RED**: Test `test_adj_index_cleared_after_remove_node` — remove node, verificar `get(node).is_none()`
- **GREEN**: En `remove_edge`, re-leer `adj_pointer` de source/target y actualizar; en `remove_node`, `adj_index.remove(node)` + actualizar vecinos
- **REFACTOR**: Extraer helper de actualización si hay duplicación

#### Ciclo 8: Pre-warm del AdjCache del MIT core desde el índice
- **RED**: Test `test_adj_index_prewarms_core_cache` — con grafo de N>65K nodos, verificar que `outgoing_edges` no degenera a O(N) si el índice está caliente
  - Assert: latencia de `outgoing_edges` en nodo fuera del AdjCache < threshold (vs O(N) baseline)
- **GREEN**: En `NeighborCache::outgoing_edges` y `incoming_edges`, antes de delegar a `self.inner`:
  ```rust
  if let Some(ptr) = self.adj_index.get(node) {
      self.inner.set_adj_pointer(node, ptr);
  }
  ```
  Esto pre-warm el AdjCache del MIT core, evitando el scan O(N).
- **REFACTOR**: No aplica

---

### Fase 3 — Test de rendimiento

#### Ciclo 9: Throughput de bulk insert
- Test de throughput con 10K edges — medir baseline sin índice y comparar
- Umbral: no degradar más del 10% vs baseline
- Verificar que para N > 65K (capacity del AdjCache), el pre-warm mantiene O(1) por edge

---

### Wiring Verification (grep-based, no new tests)

1. `grep -n "adj_index" cache.rs` — verificar que se usa en `add_edge`, `remove_edge`, `remove_node`, `outgoing_edges`, `incoming_edges`
2. `grep -n "adj_pointer" cache.rs` — verificar que se llama al MIT core para poblar el índice
3. `grep -n "set_adj_pointer" cache.rs` — verificar que se usa para pre-warm en `outgoing_edges`/`incoming_edges`
4. `grep -n "pub mod adj_index" lib.rs` — módulo expuesto

### Checklist de wiring
- [ ] `AdjacencyIndex` tiene ≥1 call site en `NeighborCache` (no solo tests)
- [ ] `adj_pointer()` se llama en `add_edge`, `remove_edge`
- [ ] `set_adj_pointer()` se llama en `outgoing_edges`, `incoming_edges`
- [ ] `adj_index.remove()` se llama en `remove_node`
- [ ] Todos los tests existentes de `NeighborCache` siguen pasando

---

## Dependencias

```
Fase 0 (MIT core: lazy alloc + API exposure)
    ↓
Fase 1 (enterprise: AdjacencyIndex struct)
    ↓
Fase 2 (enterprise: integración en NeighborCache)
    ↓
Fase 3 (enterprise: validación de rendimiento)
```

## Estimación

- Fase 0 MIT core: ~1.5 horas
- Fase 1 enterprise (struct): ~1 hora
- Fase 2 enterprise (integración): ~1.5 horas
- Fase 3 rendimiento: ~30 min
- **Total: ~4.5 horas**

## Criterios de Éxito

- [ ] `AdjacencyPointer` accesible via `tessera_graph::AdjacencyPointer`
- [ ] `NeighborCache.adj_index()` retorna el índice actualizado tras mutaciones
- [ ] Pre-warm elimina el O(N) scan: bulk insert 1K nodos × 2 aristas = O(N) no O(N²)
- [ ] Throughput `add_edge` no degrada más del 10% respecto al baseline
- [ ] Todos los tests existentes de NeighborCache pasan
