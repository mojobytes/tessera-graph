# TDD Plan: Quality Fixes for GQL Mutation Extraction (Findings 1-11)

## Resumen
11 hallazgos del code review. 4 fases, ~4 horas.

## Fase 1: Críticos — Comportamiento incorrecto (Findings 1, 2, 4)
- 1.1: Guard para set_clause no implementado → error explícito
- 1.2: Mover #[allow(clippy::cast_possible_truncation)] al cast específico
- 1.3: Unificar node_vars antes del match (Merge usa el mismo map que Create)

## Fase 2: Recomendados de código (Findings 7, 8, 9, 10)
- 2.1: Mejorar mensaje de error en DELETE (incluir node ID y edge counts)
- 2.2: Agrupar execute_set por NodeId + corregir conteo properties_set
- 2.3: TODO(perf) en MERGE para nodes_by_label O(n)

## Fase 3: Tests faltantes (Findings 5, 6, 11)
- 5 cubierto en 1.1
- 6 cubierto en 1.3
- 3.1: Tests de error paths para unbound variable en DELETE y SET

## Fase 4: Boundary — Feature flag enterprise-helpers (Finding 3)
- Feature flag en core Cargo.toml
- cfg-gate compile_match_for_mutation, eval_set_value, literal_vec_to_properties
- #[doc(hidden)] en literal_to_property
- Activar feature en enterprise Cargo.toml

## Fase 5: Wiring verification
