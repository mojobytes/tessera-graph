# TDD Plan: Lazy Adjacency Allocation (MIT Core)

## Contexto

`add_node_str()` en el MIT core pre-aloca 2 páginas de adjacency vacías (outgoing + incoming) por cada nodo nuevo, consumiendo 8KB/nodo. Para 1M nodos = 8GB solo en páginas vacías.

El fix: no crear adjacency pages en `add_node()`, sino lazy-alocar en `add_edge()` cuando se necesita por primera vez.

### Archivos clave

Todos en `/crates/tessera-graph/src/`:
- `graph.rs` — `add_node_str` (líneas 365-403), `resolve_adj_pointer` (1195-1237), `edges_for_direction` (1239-1261), `add_edge_to_adjacency`
- `adj_cache.rs` — `AdjCache` con capacidad 65,536
- `storage/codec/adjacency_codec.rs` — `AdjacencyPointer`, `AdjacencyRecord`, `write_adjacency`

### Qué cambiar

1. **`add_node_str()`**: Eliminar el bloque completo de "Initialize empty adjacency records" (líneas 378-399): las 2 `AdjacencyRecord`, los 2 `write_adjacency`, los 2 `wal_log_adj_page`, y el `adj_cache.insert`
2. **`resolve_adj_pointer()`**: Ya maneja `None` correctamente — retorna `None` cuando no encuentra páginas. Sin cambios.
3. **`edges_for_direction()`**: Ya maneja `None` page_id (retorna vec vacío). Sin cambios.
4. **`add_edge_to_adjacency()`**: Ya tiene path on-demand (líneas 1127-1134) que crea `AdjacencyRecord` cuando `page_id` es `None`. Solo necesita ser alcanzable.
5. **WAL**: Los `wal_log_adj_page` se eliminan de `add_node_str`. Ya existen en los paths de mutación de edges.
6. **`Graph::open()` / `rebuild_adj_cache`**: Es page-driven (no pages = no cache entries). Sin cambios.

### Resultado esperado

- `add_node()` aloca 0 páginas de adjacency (antes: 2)
- Memoria por nodo sin edges: ~128 bytes (node slot) vs ~8,320 bytes antes
- 1M nodos aislados: ~128MB vs ~8GB
- Primer `add_edge()` a un nodo aloca adjacency pages on demand

---

## Plan de Ejecución

### Ciclo 1: RED — Probar que add_node no aloca adj pages

- **RED**: Test `add_node_allocates_zero_adj_pages` en `graph.rs`, `mod tests`
  ```rust
  #[test]
  fn add_node_allocates_zero_adj_pages() {
      let mut g = Graph::new();
      g.add_node("Person", Properties::default()).unwrap();
      let adj_pages = g.storage.page_count(DataFile::Adjacency);
      assert_eq!(adj_pages, 0, "add_node must not pre-allocate adjacency pages");
  }
  ```
  - Assert: `page_count(DataFile::Adjacency) == 0` tras un `add_node` sin edges
  - Falla hoy (valor actual: 2)
- **GREEN**: Nada — solo establece la baseline roja
- **REFACTOR**: Nada

### Ciclo 2: RED — Probar que add_edge funciona sin adj pages pre-existentes

- **RED**: Test `add_edge_creates_adj_pages_on_demand` en `graph.rs`, `mod tests`
  ```rust
  #[test]
  fn add_edge_creates_adj_pages_on_demand() {
      let mut g = Graph::new();
      let a = g.add_node("A", Properties::default()).unwrap();
      let b = g.add_node("B", Properties::default()).unwrap();
      g.add_edge("rel", a, b, Properties::default()).unwrap();
      let adj_pages = g.storage.page_count(DataFile::Adjacency);
      assert_eq!(adj_pages, 2,
          "one add_edge between two distinct nodes must create exactly 2 adj pages");
      let out = g.outgoing_edges(a).unwrap();
      assert_eq!(out.len(), 1);
      let inc = g.incoming_edges(b).unwrap();
      assert_eq!(inc.len(), 1);
  }
  ```
  - Falla hoy (valor actual tras add_edge: 6 páginas, no 2)
- **GREEN**: Nada — se resuelve en Ciclo 3
- **REFACTOR**: Nada

### Ciclo 3: GREEN — Eliminar pre-alocación de add_node_str

- **GREEN**: En `add_node_str` (graph.rs), eliminar las líneas 378-399 (el bloque "Initialize empty adjacency records"). El método queda:
  ```rust
  pub(crate) fn add_node_str(&mut self, label: &str, properties: Properties) -> Result<NodeId> {
      let id_val = self.storage.meta().next_node_id;
      let id = NodeId(id_val);
      self.storage.meta_mut().next_node_id = id_val + 1;
      let node = Node::new(id, label, properties);
      self.write_node_slot(&node)?;
      self.node_exists.insert(id_val);
      self.node_label_index.insert(node.label(), id_val);
      self.storage.meta_mut().node_count += 1;
      self.wal_sync()?;
      Ok(id)
  }
  ```
  - Correr `cargo test -p tessera-graph` — Ciclos 1 y 2 pasan, cero regresiones
- **REFACTOR**: Nada — la eliminación ya es minimal

### Ciclo 4: Nodo aislado tras reopen no tiene cache entry

- **RED/GREEN**: Test `reopen_graph_isolated_node_has_no_adj_cache_entry`
  ```rust
  #[test]
  fn reopen_graph_isolated_node_has_no_adj_cache_entry() {
      use tempfile::TempDir;
      let dir = TempDir::new().unwrap();
      let cfg = GraphConfig { create_if_missing: true, ..Default::default() };
      let id = {
          let mut g = Graph::open(dir.path(), &cfg).unwrap();
          g.add_node("X", Properties::default()).unwrap()
      };
      let g = Graph::open(dir.path(), &cfg).unwrap();
      assert!(g.node(id).is_ok());
      assert!(g.adj_cache.get(id.0).is_none(),
          "isolated node must not occupy an adj cache entry after reopen");
      assert_eq!(g.outgoing_edges(id).unwrap().len(), 0);
      assert_eq!(g.incoming_edges(id).unwrap().len(), 0);
  }
  ```
  - Debería pasar directamente tras Ciclo 3 (`rebuild_adj_cache` es page-driven)
- **REFACTOR**: Nada

### Ciclo 5: Self-loop en nodo fresco crea exactamente 2 páginas

- **RED/GREEN**: Test `self_loop_on_fresh_node_creates_two_adj_pages`
  ```rust
  #[test]
  fn self_loop_on_fresh_node_creates_two_adj_pages() {
      let mut g = Graph::new();
      let a = g.add_node("A", Properties::default()).unwrap();
      g.add_edge("self", a, a, Properties::default()).unwrap();
      let adj_pages = g.storage.page_count(DataFile::Adjacency);
      assert_eq!(adj_pages, 2);
      assert_eq!(g.outgoing_edges(a).unwrap().len(), 1);
      assert_eq!(g.incoming_edges(a).unwrap().len(), 1);
  }
  ```
  - Pasa directamente tras Ciclo 3
- **REFACTOR**: Nada

### Ciclo 6: Regression guard de throughput

- Correr el test ignorado existente:
  ```
  cargo test -p tessera-graph -- --ignored shared_graph_add_node_throughput_floor
  ```
  Debe pasar (el throughput mejora al eliminar 2 `write_adjacency` + 2 `wal_log_adj_page` por nodo).

- Nuevo test `add_node_adj_page_count_scales_zero`:
  ```rust
  #[test]
  #[ignore = "throughput gate"]
  fn add_node_adj_page_count_scales_zero() {
      let mut g = Graph::new();
      for _ in 0..1_000 {
          g.add_node("N", Properties::default()).unwrap();
      }
      assert_eq!(g.storage.page_count(DataFile::Adjacency), 0,
          "lazy allocation: 1000 isolated nodes must produce 0 adj pages");
  }
  ```

### Ciclo Final: Wiring Verification

Verificar con grep (sin tests nuevos):

1. `grep -n "write_adjacency" graph.rs` — NO aparece dentro de `add_node_str`
2. `grep -n "adj_cache.insert" graph.rs` — NO aparece dentro de `add_node_str`, solo en `add_edge_to_adjacency`, `remove_edge_from_adjacency`, `rebuild_adj_cache`
3. `grep -n "wal_log_adj_page" graph.rs` — solo en paths de mutación de edges

### Checklist de wiring
- [ ] `write_adjacency` eliminado de `add_node_str`
- [ ] `adj_cache.insert` eliminado de `add_node_str`
- [ ] `wal_log_adj_page` solo en paths de mutación de edges
- [ ] `resolve_adj_pointer` retorna `None` para nodos sin adj pages (ya correcto)
- [ ] Todos los tests previos pasan sin regresión

---

## Estimación: ~70 min
