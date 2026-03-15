# TDD Plan: Extract GQL Mutations from Core to Enterprise

## Problem
GQL mutations (CREATE, DELETE, SET, MERGE) are in the MIT core but should be enterprise-only per roadmap boundary.

## Strategy
Extract execution code to enterprise FIRST, then revert from core.

---

## Fase 1: Hacer públicos los helpers compartidos en core (35 min)

1. [ ] Cambiar visibilidad de helpers en `src/gql/compiler.rs` del core a `pub fn`:
   - `literal_to_property`
   - `literal_vec_to_properties`
   - `compile_match_for_mutation`
   - `eval_set_value`

2. [ ] Re-exportar desde `src/gql/mod.rs` del core

3. [ ] Re-exportar desde `src/lib.rs` del core

4. [ ] Verificar compilación del core sin warnings

## Fase 2: Tests de integración en enterprise — RED (25 min)

5. [ ] Crear `tests/gql_mutations_integration.rs` con 18 tests:
   - CREATE: `create_node_persists_label_and_properties`, `create_multiple_nodes_separate_statements`
   - CREATE edge: `create_inline_edge_produces_two_nodes_and_one_edge`, `create_edge_with_properties`
   - DELETE: `delete_isolated_node`, `delete_node_with_edges_requires_detach`, `detach_delete_removes_node_and_incident_edges`, `detach_delete_all_nodes_of_label`
   - SET: `set_updates_property_on_matched_node`, `set_adds_new_property_to_node`, `set_applies_to_all_matched_nodes`
   - MERGE: `merge_creates_when_not_found`, `merge_finds_existing_node`, `merge_is_idempotent_repeated_calls`, `merge_different_props_creates_different_nodes`
   - Compat: `read_query_via_parse_statement_still_works`
   - Reject: `parse_statement_rejects_multi_label_node`, `parse_statement_rejects_variable_length_path`

6. [ ] Verificar que no compila (RED)

## Fase 3: Implementar módulo gql en enterprise — GREEN (40 min)

7. [ ] Crear `src/gql/mod.rs` con `execute_mut` + helpers extraídos del core
8. [ ] Exponer `pub mod gql` en `src/lib.rs`
9. [ ] Verificar 18 tests verdes
10. [ ] Verificar workspace enterprise compila

## Fase 4: Revocar mutaciones del core — REVERT (40 min)

11. [ ] Eliminar funciones de ejecución de `src/gql/compiler.rs` del core
12. [ ] Limpiar re-exports en `src/gql/mod.rs` del core
13. [ ] Limpiar re-exports en `src/lib.rs` del core
14. [ ] Eliminar `tests/integration/gql_mutations.rs` del core
15. [ ] Verificar que el core compila y tests pasan
16. [ ] Verificar que enterprise sigue verde

## Fase 5: Git (10 min)

17. [ ] Commit en enterprise
18. [ ] Commit en core

## Criterios de éxito
- `grep -r "execute_mut" .../tessera-graph/src/` → cero resultados
- Core compila y tests pasan sin mutaciones
- Enterprise: 18 tests verdes
- Ningún warning en ninguno de los dos repos
