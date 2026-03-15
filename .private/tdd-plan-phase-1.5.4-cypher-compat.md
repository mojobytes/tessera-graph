# TDD Plan: Phase 1.5.4 — Cypher Compatibility Mode

## Arquitectura
Option C — Token-stream pre-processor en enterprise (tessera-cypher crate).
Zero cambios al parser del core. Enterprise reescribe tokens Cypher → GQL.

## Scope
- Config: QueryLanguage enum (gql | cypher-compat | strict-gql) en tessera-config
- Tier 1: Backtick idents, block comments
- Tier 2: STARTS WITH, ENDS WITH, CONTAINS, IN [list], REMOVE n.prop
- Functions: id(n), type(r), labels(n)
- Deferred: OPTIONAL MATCH, WITH, UNWIND, CASE WHEN → 1.5.5

## Fases (12 ciclos TDD)
1. QueryLanguage enum + tessera-cypher crate stub + wiring
2. RED: tests config + strict-gql rejection
3. GREEN: QueryLanguage impl + parse_with_mode skeleton
4. RED: tests backtick idents + block comments
5. GREEN: CypherLexer wrapping core lexer
6. RED: tests STARTS WITH, ENDS WITH, CONTAINS
7. GREEN: token rewriter + BinOp variants + string op evaluation
8. RED: tests IN operator + REMOVE
9. GREEN: IN operator + REMOVE support
10. RED: tests scalar functions (id, type, labels)
11. GREEN: CypherFunc + enterprise executor for functions
12. Integration wiring verification

## Estimación: ~7.5 horas
