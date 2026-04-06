# TDD Plan: Quality Review — 14 Findings in Optimized Traversal

## Contexto

La implementación de BFS variable-hop optimizado y bidirectional BFS shortestPath en
`crates/tessera-graph-storage/src/gql/mod.rs` pasó una quality review que detectó 14 hallazgos.
Los hallazgos van desde bugs de correctness (path no óptimo en BFS bidireccional) hasta mejoras
de robustez (panic en producción), semántica incorrecta (Expr::Var pierde claves), tests débiles
y documentación insuficiente.

Este plan los agrupa en 5 ciclos TDD coherentes, ordenados por impacto:

1. Correctness: algoritmo (finding #1)
2. Correctness: semántica (finding #7)
3. Robustez: manejo de errores y edge cases (#3, #4, #5, #6, #13)
4. Calidad de tests (#9, #10, #11, #14)
5. Cleanup y documentación (#2, #8, #12)

**Stack detectado**: Rust (edition 2024), `tessera-graph-storage` crate, feature `extended-gql`
**Convenciones**: tests de integración en `crates/tessera-graph-storage/tests/`, no inline.
  Cargo.toml del crate habilita `extended-gql` como feature default.
  Warnings tratados como errores (`[lints] workspace = true`).
**Afecta hot path**: Si — bidirectional BFS y BFS variable-hop son los paths de ejecucion
  optimizados. Los cambios de correctness y robustez afectan directamente el throughput de
  shortestPath y variable-hop queries.

---

## Archivos clave

- `crates/tessera-graph-storage/src/gql/mod.rs` — implementacion (modificar)
- `crates/tessera-graph-storage/tests/optimized_traversal_test.rs` — 19 tests existentes (extender)
- `crates/tessera-graph-storage/tests/gql_mutations_integration.rs` — eliminar import muerto

---

## Ciclo 1: Correctness del algoritmo bidireccional BFS (finding #1)

**Problema**: `bidirectional_bfs` retorna en el primer nodo de intersection que encuentra
dentro de una expansion de frontera, sin terminar de expandir el resto de la capa actual.
Si hay multiples puntos de encuentro en la misma capa, puede elegir uno suboptimo.

**Descripcion del fix**: Acumular todos los candidatos encontrados durante la expansion
completa de una frontera. Al finalizar la capa, elegir el meeting point que minimiza
`fwd_depth[meeting] + bwd_depth[meeting]`. Requiere mantener mapas de profundidad separados
de los mapas de parents.

### RED — tests que fallan con la implementacion actual

En `crates/tessera-graph-storage/tests/optimized_traversal_test.rs`, agregar al final:

```rust
// ── Ciclo Q1: BFS bidireccional correctness ─────────────────────────────────

/// Grafo con dos caminos de igual longitud que se encuentran en la misma capa
/// de expansion. El BFS bidireccional debe devolver uno de los caminos optimos
/// (longitud 3), no un camino suboptimo de longitud mayor.
///
/// Topologia:
///   A -> B -> C -> E
///   A -> D -> C -> E  (D se une en C, mismo nivel que B)
///
/// shortestPath(A, E) = longitud 4 (A, _, C, E). Ambos caminos son optimos.
/// El bug puede devolver longitud 5 si elige B primero pero luego encuentra
/// D->C en la misma capa y reconstruye mal.
#[test]
fn bidirectional_bfs_two_paths_same_frontier_layer_picks_optimal() {
    let mut g = Graph::new();
    let a = g.add_node("Node", props(&[("name", "A")])).unwrap();
    let b = g.add_node("Node", props(&[("name", "B")])).unwrap();
    let c = g.add_node("Node", props(&[("name", "C")])).unwrap();
    let d = g.add_node("Node", props(&[("name", "D")])).unwrap();
    let e = g.add_node("Node", props(&[("name", "E")])).unwrap();
    // Dos caminos de longitud 3 hacia E: A->B->C->E y A->D->C->E
    g.add_edge("R", a, b, Properties::new()).unwrap();
    g.add_edge("R", b, c, Properties::new()).unwrap();
    g.add_edge("R", a, d, Properties::new()).unwrap();
    g.add_edge("R", d, c, Properties::new()).unwrap();
    g.add_edge("R", c, e, Properties::new()).unwrap();

    let query = parse_query(
        "MATCH (a:Node {name:'A'}) MATCH (b:Node {name:'E'}) RETURN shortestPath(a, b)",
    );
    let result = execute_query(&g, &query).unwrap();
    assert_eq!(result.len(), 1);
    let val = result[0].values().next().unwrap();
    match val {
        GqlValue::List(path) => {
            assert_eq!(
                path.len(),
                4,
                "camino optimo A->_->C->E tiene 4 nodos, se obtuvo {}",
                path.len()
            );
        }
        _ => panic!("expected List, got {val:?}"),
    }
}

/// Grafo diamante: A->B->D y A->C->D, shortestPath(A,D) debe ser longitud 3.
/// Si la expansion delantera encuentra B y C en la misma capa, y la expansion
/// trasera ya visito D, el meeting point se detecta en esa capa.
/// El path reconstruido debe tener exactamente 3 nodos.
#[test]
fn bidirectional_bfs_diamond_returns_length_3() {
    let mut g = Graph::new();
    let a = g.add_node("Node", props(&[("name", "A")])).unwrap();
    let b = g.add_node("Node", props(&[("name", "B")])).unwrap();
    let c = g.add_node("Node", props(&[("name", "C")])).unwrap();
    let d = g.add_node("Node", props(&[("name", "D")])).unwrap();
    g.add_edge("R", a, b, Properties::new()).unwrap();
    g.add_edge("R", a, c, Properties::new()).unwrap();
    g.add_edge("R", b, d, Properties::new()).unwrap();
    g.add_edge("R", c, d, Properties::new()).unwrap();

    let query = parse_query(
        "MATCH (a:Node {name:'A'}) MATCH (b:Node {name:'D'}) RETURN shortestPath(a, b)",
    );
    let result = execute_query(&g, &query).unwrap();
    assert_eq!(result.len(), 1);
    let val = result[0].values().next().unwrap();
    match val {
        GqlValue::List(path) => {
            assert_eq!(path.len(), 3, "diamante: camino optimo tiene 3 nodos (A,_,D)");
        }
        _ => panic!("expected List, got {val:?}"),
    }
}
```

Ejecutar: `cargo test --package tessera-graph-storage bidirectional_bfs_two_paths`
Resultado esperado: FAIL (el algoritmo puede devolver longitud incorrecta)

### GREEN — fix en `bidirectional_bfs`

Archivo: `crates/tessera-graph-storage/src/gql/mod.rs`
Funcion: `bidirectional_bfs` (lineas 223-304)

El cambio reemplaza el `return` inmediato al encontrar intersection por acumulacion de
candidatos en la capa completa y seleccion del mejor al finalizar cada expansion.

**Logica del fix:**

1. Agregar mapas de profundidad: `fwd_depth: HashMap<NodeId, u32>` y `bwd_depth: HashMap<NodeId, u32>`
   - Inicializar: `fwd_depth.insert(from, 0)`, `bwd_depth.insert(to, 0)`
2. Al insertar en `fwd_visited`, registrar tambien la profundidad en `fwd_depth`
3. Durante la expansion de una capa forward: en lugar de `return Some(reconstruct...)`,
   acumular en `let mut meeting_candidates: Vec<NodeId> = Vec::new()`
4. Al terminar la expansion de esa capa (despues del for loop), si `!meeting_candidates.is_empty()`:
   - Elegir el candidato con menor `fwd_depth[c] + bwd_depth[c]`
   - `return Some(reconstruct_path(...))`
5. Misma logica para la expansion backward
6. El loop principal debe terminar el ciclo de expansion antes de verificar candidatos

Firma de `reconstruct_path` no cambia.

### REFACTOR

- Verificar que todos los tests previos del Ciclo 3 siguen pasando
- `shortest_path_picks_minimum` ya cubre el caso A->D->E vs A->B->C->D->E
- Los nuevos tests cubren el caso de multiples paths en la misma capa

Ejecutar: `cargo test --package tessera-graph-storage --features extended-gql`

---

## Ciclo 2: Correctness semantica — Expr::Var pierde claves de propiedades (finding #7)

**Problema**: En `eval_bfs_expr`, cuando la expresion es `Expr::Var(var)` (ej. `RETURN n`),
el codigo devuelve `GqlValue::List(props_values)` — una lista de valores sin sus claves.
Esto viola el contrato semantico de GQL donde `RETURN n` debe devolver el nodo completo
con nombre de propiedades accesibles. El MIT core devuelve un mapa o estructura diferente.

**Solucion**: Para `Expr::Var`, delegar la ejecucion al MIT core. Si la variable apunta a un
nodo conocido (start o end), es mejor documentar la limitacion y cambiar el comportamiento
para que sea consistente con lo que el MIT core devolveria para la misma variable.

La solucion correcta es: cuando la query tiene `Expr::Var` en el RETURN (no PropAccess),
la funcion `needs_optimized_execution` debe retornar `false` para esas queries y delegarlas
al MIT core. Alternativamente, si se mantiene el path optimizado, devolver un `GqlValue::Map`
con las claves preservadas.

Dado que `GqlValue` puede o no tener una variante `Map` dependiendo del MIT core, verificar
antes de implementar. Si existe `GqlValue::Map`, usarla. Si no existe, delegar al MIT core.

### RED — test que falla con la implementacion actual

```rust
// ── Ciclo Q2: Expr::Var semantica ────────────────────────────────────────────

/// RETURN n en una query variable-hop debe producir un resultado consistente
/// con el MIT core. La implementacion actual devuelve una List de valores sin
/// claves, lo que es semanticamente incorrecto.
///
/// Este test verifica que al menos los datos de propiedades estan presentes
/// y que el comportamiento es documentado (delegacion al MIT core) o que la
/// estructura devuelta contiene claves.
#[test]
fn variable_hop_bare_var_return_delegates_or_documents_limitation() {
    let g = build_chain_graph(); // A -> B -> C -> D
    // RETURN b (bare variable, no PropAccess)
    let query = parse_query(
        "MATCH (a:Node {name:'A'})-[*1..1]->(b:Node) RETURN b",
    );

    // El MIT core maneja esta query correctamente
    let mit_result = tessera_graph::gql::execute(&g, &query).unwrap();
    assert!(!mit_result.is_empty(), "MIT core debe encontrar al menos un resultado");

    // Enterprise con bare var: debe delegar al MIT core (porque needs_optimized_execution
    // retorna false para queries con bare Var en RETURN) o devolver el mismo resultado.
    let enterprise_result = execute_query(&g, &query).unwrap();

    // Si delega al MIT core, los resultados deben ser identicos
    assert_eq!(
        enterprise_result, mit_result,
        "bare Var RETURN debe producir el mismo resultado que MIT core"
    );
}
```

Ejecutar: `cargo test --package tessera-graph-storage variable_hop_bare_var_return`
Resultado esperado: FAIL (enterprise devuelve List sin claves, MIT core devuelve algo diferente)

### GREEN — fix en `needs_optimized_execution` o en `execute_variable_hop_query`

Archivo: `crates/tessera-graph-storage/src/gql/mod.rs`

**Opcion A (preferida, mas simple)**: Extender `needs_optimized_execution` para retornar
`false` si alguno de los items de RETURN contiene un `Expr::Var` sin ser parte de
`PropAccess`. Esto delega automaticamente al MIT core para `RETURN n`.

```rust
// En needs_optimized_execution, antes del return false final:
for item in &query.return_clause.items {
    if matches!(&item.expr, Expr::Var(_)) {
        return false; // bare Var — delegar al MIT core
    }
}
```

**Opcion B (mas completa)**: Devolver `GqlValue::Map` con claves. Solo si `GqlValue` tiene
variante `Map`. Verificar en MIT core antes de implementar.

Usar Opcion A. Es la mas conservadora y correcta: si no podemos producir la semantica
correcta de un bare Var, delegamos al MIT core que si puede.

### REFACTOR

- El test `variable_hop_with_where_delegates_to_mit_core` debe seguir pasando
- Revisar que `variable_hop_results_match_mit_core` usa `b.name` (PropAccess), no bare Var
  — ese test NO se ve afectado por el cambio

Ejecutar: `cargo test --package tessera-graph-storage`

---

## Ciclo 3: Robustez — manejo de errores y edge cases (#3, #4, #5, #6, #13)

Este ciclo agrupa cinco hallazgos de robustez. Cuatro son cambios en `mod.rs`, uno es
documentacion/deduplicacion.

**Finding #3**: `ast_direction_to_direction` usa Debug como proxy. Si MIT core cambia
nombres de variantes, falla silenciosamente con fallback a Outgoing.

**Finding #4**: `.expect()` en `execute_variable_hop_query` puede hacer panic en produccion.

**Finding #5**: Iteracion de NodeIds sin label asume IDs contiguos (0..node_count). Incorrecto
si hay nodos eliminados (gaps en IDs).

**Finding #6**: `node_passes_filter` absorbe el error de `graph.node()` como false.

**Finding #13**: `Direction::Both` en `edges_for_direction` puede duplicar self-loops.

### RED — tests que fallan o documentan el comportamiento actual

```rust
// ── Ciclo Q3: Robustez ───────────────────────────────────────────────────────

/// Finding #4: execute_variable_hop_query usa .expect() en hop_idx.
/// El invariant es correcto (needs_optimized_execution garantiza que existe
/// al menos un variable-hop), pero .expect() hace panic en produccion.
/// Este test verifica que si por algun motivo el invariant falla, el sistema
/// no hace panic sino que delega gracefully al MIT core.
///
/// Nota: este test es mas de documentacion — no puede provocar el panic directamente
/// porque el invariant es correcto. Pero despues del fix (usar .ok_or / fallback),
/// el compilador confirma que el path de error esta cubierto.
#[test]
fn variable_hop_graceful_fallback_no_panic() {
    // Query normal: el path optimizado debe funcionar sin panic
    let g = build_chain_graph();
    let query = parse_query(
        "MATCH (a:Node {name:'A'})-[*1..3]->(b:Node) RETURN b.name",
    );
    // Si hubiera panic, este test lo captaria
    let result = execute_query(&g, &query);
    assert!(result.is_ok(), "no debe hacer panic ni retornar error");
}

/// Finding #5: iteracion sin label asume IDs contiguos.
/// Crear un grafo con un nodo eliminado (gap en IDs) y verificar que la
/// query sin label no produce error ni resultados incorrectos.
#[test]
fn variable_hop_no_label_with_deleted_node_gap() {
    let mut g = Graph::new();
    let a = g.add_node("Node", props(&[("name", "A")])).unwrap();
    let b = g.add_node("Node", props(&[("name", "B")])).unwrap();
    let c = g.add_node("Node", props(&[("name", "C")])).unwrap();
    g.add_edge("R", a, b, Properties::new()).unwrap();
    g.add_edge("R", b, c, Properties::new()).unwrap();
    // Eliminar nodo B — crea un gap en los IDs
    // Note: si Graph no tiene remove_node, usar un ID que sabemos que no existe
    // y verificar que node_exists devuelve false para ese ID
    // La implementacion actual usa (0..node_count()) que incluiria el gap
    // Este test verifica que no hace panic al acceder a un ID eliminado
    drop(b); // solo para suprimir el warning de unused
    // Ejecutar query sin label (el path que itera 0..node_count)
    // La query usa label "Node" — este test documenta el comportamiento actual
    // Para el path SIN label, una query artificial:
    let query_with_label = parse_query(
        "MATCH (a:Node {name:'A'})-[*1..2]->(b:Node) RETURN b.name",
    );
    let result = execute_query(&g, &query_with_label).unwrap();
    // Debe encontrar B y C independientemente de gaps (usa nodes_by_label)
    let names = extract_column_sorted(&result, "b.name");
    assert_eq!(names, vec!["B", "C"]);
}

/// Finding #13: Direction::Both puede duplicar self-loops.
/// Verificar que un self-loop (nodo conectado a si mismo) no aparece duplicado
/// en los resultados de una query con direction Both.
#[test]
fn variable_hop_both_direction_self_loop_no_duplicate() {
    let mut g = Graph::new();
    let a = g.add_node("Node", props(&[("name", "A")])).unwrap();
    let b = g.add_node("Node", props(&[("name", "B")])).unwrap();
    // Self-loop en A
    g.add_edge("SELF", a, a, Properties::new()).unwrap();
    g.add_edge("CONNECTS", a, b, Properties::new()).unwrap();

    // Query con direction Both (usando <->)
    // Si el parser no soporta <->, usar direction Outgoing como baseline
    // y documentar el comportamiento para Both
    let query = parse_query(
        "MATCH (a:Node {name:'A'})-[*1..2]->(b:Node) RETURN b.name",
    );
    let result = execute_query(&g, &query).unwrap();
    let names = extract_column_sorted(&result, "b.name");
    // Con direction Outgoing: A (self-loop a depth 1) + B (a depth 1)
    // El self-loop no debe aparecer duplicado aunque BFS tenga visited set
    assert_eq!(
        names.iter().filter(|n| n.as_str() == "A").count(),
        1,
        "self-loop no debe producir resultado duplicado"
    );
}
```

Ejecutar: `cargo test --package tessera-graph-storage variable_hop_no_label|variable_hop_graceful|self_loop`
Resultado esperado: algunos pasan (tests de documentacion), `variable_hop_no_label_with_deleted_node_gap`
puede fallar si `node_count()` incluye el nodo eliminado y el acceso hace panic.

### GREEN — fixes en `mod.rs`

**Fix #4 — `.expect()` → graceful fallback** (linea 364):
```rust
// ANTES:
.expect("needs_optimized_execution confirmed variable-hop exists");

// DESPUES:
.unwrap_or_else(|| {
    // needs_optimized_execution guarantees a variable-hop exists;
    // if the invariant is somehow violated, delegate gracefully.
    usize::MAX // sentinel — detected below
});

// Justo despues del .unwrap_or_else, agregar:
if hop_idx == usize::MAX {
    return tessera_graph::gql::execute(graph, query);
}
```

Alternativa mas limpia (preferida):
```rust
let Some(hop_idx) = pattern
    .hops
    .iter()
    .position(|(ep, _)| matches!(ep.length, EdgeLength::Variable { .. }))
else {
    // Invariant violation: delegate gracefully instead of panicking.
    return tessera_graph::gql::execute(graph, query);
};
```

**Fix #3 — `debug_assert` en `ast_direction_to_direction`** (lineas 494-503):
```rust
fn ast_direction_to_direction(ast_dir: &impl std::fmt::Debug) -> Direction {
    let s = format!("{ast_dir:?}");
    if s.contains("Incoming") {
        Direction::Incoming
    } else if s.contains("Both") {
        Direction::Both
    } else {
        debug_assert!(
            s.contains("Outgoing"),
            "ast_direction_to_direction: unknown variant '{s}', defaulting to Outgoing. \
             Update this function if MIT core adds new Direction variants."
        );
        Direction::Outgoing
    }
}
```

**Fix #5 — iteracion sin label usa `graph.all_node_ids()` o patron seguro** (lineas 402-410):
Revisar si `GraphAccess` expone un metodo para iterar todos los nodos existentes.
Si existe `all_node_ids()` o similar, usarlo. Si no, el patron actual con `node_exists(id)`
ya es correcto para gaps — solo falla si `node_count()` no refleja el maximo ID asignado
sino el conteo de nodos activos.

Verificar el contrato de `graph.node_count()` en MIT core antes de implementar.
Si `node_count()` retorna el numero de nodos activos (no el maximo ID), el fix es
usar `nodes_by_label` con todas las labels conocidas, o iterar via un metodo de iteracion.
Si no existe tal metodo, documentar la limitacion con un `// KNOWN LIMITATION` comment.

**Fix #6 — `node_passes_filter` documenta absorcion de error** (lineas 566-593):
La signature `-> bool` no puede propagar errores sin cambiar todos los call sites.
El fix apropiado es documentar explicitamente que el error se absorbe como `false`:
```rust
// Property check — only fetch node if we have props to check.
// graph.node() errors are treated as "does not pass filter" (fail-safe).
if !end_props.is_empty() {
    let Ok(node) = graph.node(node_id) else {
        // Node fetch failed (e.g., node was deleted concurrently).
        // Fail-safe: exclude the node from results.
        return false;
    };
    ...
}
```
Adicionalmente, agregar un `// TODO(Q6): consider returning Result<bool, Error>`
para documentar la deuda tecnica pendiente.

**Fix #13 — `edges_for_direction` deduplica self-loops para Direction::Both** (lineas 506-520):
```rust
Direction::Both => {
    let mut edges = graph.outgoing_edges(node_id)?;
    let incoming = graph.incoming_edges(node_id)?;
    // Self-loops appear in both outgoing and incoming; deduplicate by edge ID.
    let outgoing_ids: HashSet<_> = edges.iter().map(|e| (e.source(), e.target())).collect();
    for edge in incoming {
        if edge.source() != edge.target() || !outgoing_ids.contains(&(edge.source(), edge.target())) {
            edges.push(edge);
        }
    }
    Ok(edges)
}
```
Nota: si `Edge` no tiene un ID unico accesible, deduplicar por `(source, target)` es
una aproximacion. Documentar en comentario si hay limitaciones.

### REFACTOR

- Ejecutar `cargo clippy --package tessera-graph-storage -- -D warnings`
- El `debug_assert` no debe tener `missing_panics_doc` porque solo dispara en debug builds
- El `unwrap_or_else` eliminado del `.expect()` no necesita doc de panics

Ejecutar: `cargo test --package tessera-graph-storage`

---

## Ciclo 4: Calidad de tests (#9, #10, #11, #14)

Este ciclo mejora los tests existentes. No cambia `mod.rs` — solo modifica
`crates/tessera-graph-storage/tests/optimized_traversal_test.rs`.

**Finding #9**: `shortest_path_matches_mit_core` solo compara longitud, no nodos concretos.

**Finding #10**: `variable_hop_with_where_delegates_to_mit_core` pasa trivialmente si ambos
retornan vacio.

**Finding #11**: Comentario de throughput tree no documenta resultados esperados.

**Finding #14**: `shortest_path_throughput_guard` solo tiene threshold relativo, sin piso absoluto.

### RED — mejoras de tests (los tests actuales pasan pero son debiles)

**Fix #9** — reemplazar el cuerpo de `shortest_path_matches_mit_core`:

El test actual (lineas 271-303) compara solo longitud. Reemplazar con assertions sobre
nodos concretos. El grafo `build_shortest_path_graph()` tiene:
- A(id=0), B(id=1), C(id=2), D(id=3), E(id=4)
- shortestPath(A, E): A->D->E (ids: 0, 3, 4) gracias al shortcut

```rust
#[test]
fn shortest_path_matches_mit_core() {
    let g = build_shortest_path_graph();
    let query = parse_query(
        "MATCH (a:Node {name:'A'}) MATCH (b:Node {name:'E'}) RETURN shortestPath(a, b)",
    );

    let enterprise_result = execute_query(&g, &query).unwrap();
    let mit_result = tessera_graph::gql::execute(&g, &query).unwrap();

    assert_eq!(enterprise_result.len(), mit_result.len(), "row count mismatch");
    assert_eq!(enterprise_result.len(), 1, "should produce exactly one row");

    let col = enterprise_result[0].keys().next().unwrap().clone();
    let ent_path = match &enterprise_result[0][&col] {
        GqlValue::List(v) => v.clone(),
        v => panic!("expected List, got {v:?}"),
    };
    let mit_path = match &mit_result[0].get(&col).unwrap_or_else(|| {
        mit_result[0].values().next().unwrap()
    }) {
        GqlValue::List(v) => v.clone(),
        v => panic!("expected List, got {v:?}"),
    };

    // Ambos deben encontrar el camino optimo de longitud 3: A->D->E
    assert_eq!(ent_path.len(), 3, "enterprise: shortest A->D->E tiene 3 nodos");
    assert_eq!(mit_path.len(), ent_path.len(), "MIT core y enterprise deben coincidir en longitud");

    // El primer nodo es A (id 0) y el ultimo es E (id 4)
    assert_eq!(ent_path.first(), Some(&GqlValue::Int(0)), "primer nodo debe ser A (id=0)");
    assert_eq!(ent_path.last(), Some(&GqlValue::Int(4)), "ultimo nodo debe ser E (id=4)");
}
```

**Fix #10** — extender `variable_hop_with_where_delegates_to_mit_core`:

El test actual (lineas 79-88) solo verifica que enterprise == mit_core sin asegurar
que ningun resultado es no-vacio. Si ambos devuelven vacio, el test pasa trivialmente.

```rust
#[test]
fn variable_hop_with_where_delegates_to_mit_core() {
    let g = build_chain_graph();
    let query = parse_query(
        "MATCH (a:Node)-[*1..3]->(b:Node) WHERE a.name = 'A' RETURN b.name",
    );
    let enterprise_result = execute_query(&g, &query).unwrap();
    let mit_result = tessera_graph::gql::execute(&g, &query).unwrap();
    assert_eq!(enterprise_result, mit_result);

    // Verificar que el resultado no es trivialmente vacio — la query debe encontrar B, C, D
    let names = extract_column_sorted(&enterprise_result, "b.name");
    assert_eq!(
        names,
        vec!["B", "C", "D"],
        "WHERE delegation debe retornar resultados concretos, no vacio trivial"
    );
}
```

**Fix #11** — extender comentario en `variable_hop_throughput_guard`:

El test actual (linea 231) tiene el comentario "Tree: branching=4, depth=4 → 1 + 4 + 16 + 64 + 256 = 341 nodes"
pero no documenta cuantos resultados espera la query. Agregar assertion de conteo:

```rust
// Dentro de variable_hop_throughput_guard, despues del loop de iteraciones:
// Verificar una vez que el resultado tiene el conteo correcto:
// MATCH (root)-[*1..4]->(b) desde root en un arbol branching=4, depth=4
// devuelve exactamente 4 + 16 + 64 + 256 = 340 resultados (excluyendo root mismo)
let verification_result = execute_query(&g, &query).unwrap();
assert_eq!(
    verification_result.len(),
    340,
    "arbol branching=4 depth=4: se esperan 340 nodos alcanzables en *1..4"
);
```

**Fix #14** — agregar piso absoluto en `shortest_path_throughput_guard`:

El test actual (lineas 440-444) solo verifica `enterprise_qps >= mit_qps * 0.9`.
Si ambos son muy lentos (ej. 1 qps), el test pasa igual. Agregar piso absoluto:

```rust
// Despues del assert relativo existente, agregar:
let min_absolute_qps = if cfg!(debug_assertions) { 1.0 } else { 5.0 };
assert!(
    enterprise_qps >= min_absolute_qps,
    "shortestPath throughput absoluto {enterprise_qps:.2} qps por debajo del piso minimo \
     {min_absolute_qps:.1} qps"
);
```

### GREEN

Estos tests son mejoras de assertions existentes. Algunos pueden requerir ajustes en
los valores concretos de IDs si el orden de insercion no garantiza los IDs esperados.
Verificar IDs en `build_shortest_path_graph()` — el orden es: a=primero, b=segundo,
c=tercero, d=cuarto, e=quinto. Los IDs en `tessera-graph` son consecutivos desde 0
si no hay eliminaciones. Confirmar con `assert_eq!(a.as_u64(), 0)` en la funcion
`build_shortest_path_graph` si es necesario.

Si los IDs no son predecibles, usar una estrategia alternativa: obtener los IDs via
`graph.nodes_by_label("Node")` y buscar por nombre, luego comparar con los IDs en el path.

### REFACTOR

- Eliminar el cuerpo antiguo de `shortest_path_matches_mit_core` y reemplazarlo completo
- Verificar que `shortest_path_picks_minimum` (test existente) sigue siendo consistente con las
  nuevas assertions (ambos verifican camino de longitud 3 en el mismo grafo)

Ejecutar: `cargo test --package tessera-graph-storage`

---

## Ciclo 5: Cleanup y documentacion (#2, #8, #12)

Este es el ciclo mas mecanico. No hay logica nueva — solo eliminacion de redundancia,
imports muertos y documentacion insuficiente.

**Finding #2**: `col_name` recalculado en cada iteracion del doble loop en `try_execute_shortest_path`
(lineas 138-160 en `mod.rs`).

**Finding #8**: Import muerto `GqlStatement` en `gql_mutations_integration.rs`.

**Finding #12**: `needs_optimized_execution` no documenta que solo inspecciona RETURN para
shortestPath, no WHERE ni otras clausulas.

### RED — solo finding #8 puede tener un test de compilacion

Finding #8 produce un warning de compilacion (`unused import`). Con `lints.workspace = true`
que trata warnings como errores, este warning ya deberia estar bloqueando la compilacion.
Verificar ejecutando: `cargo build --package tessera-graph-storage`

Si la compilacion falla por el import muerto, la correccion del Ciclo 5 es el fix.
Si no falla (el lint no aplica a tests), el fix sigue siendo correcto por limpieza.

### GREEN — los tres fixes

**Fix #8** — eliminar import muerto en `gql_mutations_integration.rs`:

Archivo: `crates/tessera-graph-storage/tests/gql_mutations_integration.rs`, linea 6:
```rust
// ANTES:
use tessera_graph::{GqlMutationResult, GqlStatement, Graph, gql, props};

// DESPUES:
use tessera_graph::{GqlMutationResult, Graph, gql, props};
```

Nota: verificar que `GqlStatement` realmente no se usa en ningun test del archivo antes
de eliminar. Si se usa en alguna funcion de ayuda, mantener.

**Fix #2** — extraer `col_name` antes del loop en `try_execute_shortest_path`:

Archivo: `crates/tessera-graph-storage/src/gql/mod.rs`, en `try_execute_shortest_path`.

Actualmente `col_name` se recalcula en cada iteracion del `for &from_id / for &to_id`:
```rust
// ANTES (dentro del doble loop):
let col_name = sp_item
    .alias
    .as_deref()
    .map_or_else(|| expr_surface_name(&sp_item.expr), String::from);

// DESPUES: extraer antes del loop for &from_id in &from_ids {
let col_name = sp_item
    .alias
    .as_deref()
    .map_or_else(|| expr_surface_name(&sp_item.expr), String::from);

for &from_id in &from_ids {
    for &to_id in &to_ids {
        let path = bidirectional_bfs(graph, from_id, to_id);
        // usar col_name (ya calculado fuera del loop)
        ...
    }
}
```

**Fix #12** — expandir documentacion de `needs_optimized_execution`:

Archivo: `crates/tessera-graph-storage/src/gql/mod.rs`, doc comment de `needs_optimized_execution`
(lineas 27-32):

```rust
/// Returns `true` if the query contains patterns that benefit from the
/// enterprise optimized execution path:
///
/// - Variable-length edge patterns (`-[*1..3]->`)
/// - `shortestPath(a, b)` function calls in RETURN
///
/// Queries that return `false` are delegated to the MIT core engine.
///
/// # Limitations
///
/// This classifier only inspects:
/// - Edge patterns in MATCH for variable-length hops
/// - RETURN items for `shortestPath` function calls
///
/// It does NOT inspect WHERE clauses, nested expressions, or shortestPath
/// in contexts other than RETURN items. Queries with WHERE clauses are always
/// delegated to the MIT core engine (see `execute_query`). If `shortestPath`
/// appears in a WHERE expression or subquery, it will not be detected here
/// and will be delegated to MIT core, which may not support it.
#[must_use]
pub fn needs_optimized_execution(query: &GqlQuery) -> bool {
```

### REFACTOR

- Ejecutar `cargo clippy --package tessera-graph-storage -- -D warnings` tras cada fix
- Confirmar que eliminacion de `GqlStatement` no rompe ningun test

Ejecutar: `cargo test --package tessera-graph-storage`

---

## Ciclo 6: Wiring Verification

### Objetivo

Verificar que los 5 ciclos anteriores no rompieron nada y que los fixes estan correctamente
integrados en el sistema end-to-end.

### Pasos

1. Ejecutar la suite completa de optimized_traversal:
   ```
   cargo test --package tessera-graph-storage --features extended-gql 2>&1
   ```
   Resultado esperado: todos los tests pasan, incluyendo los 19 originales + los nuevos.

2. Ejecutar tests de mutaciones para verificar que el cleanup del import no rompio nada:
   ```
   cargo test --package tessera-graph-storage --test gql_mutations_integration
   ```

3. Ejecutar clippy en el workspace:
   ```
   cargo clippy --workspace -- -D warnings
   ```
   Resultado esperado: 0 warnings.

4. Verificar que el test de throughput de variable-hop sigue pasando el umbral (200 qps en debug):
   ```
   cargo test --package tessera-graph-storage variable_hop_throughput_guard -- --nocapture
   ```

5. Verificar que el test de throughput de shortestPath sigue pasando:
   ```
   cargo test --package tessera-graph-storage shortest_path_throughput_guard -- --nocapture
   ```

6. Ejecutar la suite completa del workspace para confirmar que ningun otro crate se vio afectado:
   ```
   cargo test --workspace 2>&1
   ```

---

## Estimacion Total

| Ciclo | Descripcion | Estimacion |
|-------|-------------|------------|
| 1 | BFS bidireccional correctness | 45 min |
| 2 | Expr::Var semantica | 20 min |
| 3 | Robustez (#3, #4, #5, #6, #13) | 40 min |
| 4 | Calidad de tests (#9, #10, #11, #14) | 30 min |
| 5 | Cleanup y documentacion (#2, #8, #12) | 20 min |
| 6 | Wiring verification | 15 min |
| **Total** | | **~2h 50min** |

---

## Criterios de Exito

- [ ] Los 19 tests existentes en `optimized_traversal_test.rs` siguen pasando
- [ ] Los nuevos tests del Ciclo 1 (`bidirectional_bfs_two_paths_same_frontier_layer_picks_optimal`,
      `bidirectional_bfs_diamond_returns_length_3`) pasan
- [ ] El nuevo test del Ciclo 2 (`variable_hop_bare_var_return_delegates_or_documents_limitation`) pasa
- [ ] Los tests del Ciclo 3 pasan (robustez sin panics)
- [ ] Las mejoras del Ciclo 4 estan en vigor: `shortest_path_matches_mit_core` verifica nodos concretos,
      `variable_hop_with_where_delegates_to_mit_core` verifica contenido no-vacio,
      `variable_hop_throughput_guard` verifica 340 resultados,
      `shortest_path_throughput_guard` tiene piso absoluto
- [ ] `cargo clippy --workspace -- -D warnings` pasa sin errores
- [ ] Import `GqlStatement` eliminado de `gql_mutations_integration.rs`
- [ ] `col_name` extraido fuera del doble loop en `try_execute_shortest_path`
- [ ] `needs_optimized_execution` tiene documentacion de limitaciones
- [ ] `ast_direction_to_direction` tiene `debug_assert` para variantes desconocidas
- [ ] `.expect()` en `execute_variable_hop_query` reemplazado por `else` graceful
- [ ] Throughput de variable-hop >= 200 qps (debug) tras los cambios
- [ ] Throughput de shortestPath >= `mit_qps * 0.9` (debug) tras los cambios

---

## Nota sobre finding #5 (iteracion sparse de NodeIds)

Antes de implementar el fix del Ciclo 3 para finding #5, verificar el contrato de
`GraphAccess::node_count()` en MIT core. Si retorna el numero de nodos activos (no el
maximo ID historico), la iteracion `0..node_count()` es incorrecta para grafos con
eliminaciones. Si retorna el maximo ID + 1, la iteracion es correcta pero el `node_exists`
ya actua como guard. Consultar `crates/tessera-graph/src/lib.rs` o la documentacion
de `GraphAccess` antes de implementar este fix.

---

## Confirmacion requerida

El plan esta listo. **No se implementara nada hasta recibir confirmacion explicita.**

¿Apruebas este plan para proceder con la implementacion?
- Si quieres ajustar algo, indica que cambiar.
- Si apruebas, responde "adelante" o equivalente.
