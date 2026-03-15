# Error Log — TesseraGraph Enterprise

### [2026-03-15] Mismatch de tipos en helper `run_mutation` al llamar `execute_mut`
- **Qué hice mal:** El helper `run_mutation` en el test de enterprise pasaba `ms` (valor de tipo `MutationStatement`) en lugar de `&ms` a `execute_mut`. La función requiere `&MutationStatement`.
- **Causa raíz:** El método `as_mutation()` retorna un valor owned (no una referencia), entonces al escribir `execute_mut(graph, ms)` sin el `&`, el tipo no coincidía. El test original del core usaba `&stmt.as_mutation().unwrap()` aplicando el `&` directamente al resultado de la llamada.
- **Cómo lo solucioné:** Cambié `execute_mut(graph, ms)` a `execute_mut(graph, &ms)`.
- **Regla para evitarlo:** Cuando se copia código entre repos y se cambia el call site de un método, verificar que todos los `&` de borrows se preservan. El patrón `method().unwrap()` vs `let x = method().unwrap(); use(&x)` son equivalentes, pero el segundo necesita explícitamente el `&` en el uso.

### [2026-03-14] Commit enterprise Phase 1.2 directamente en `develop` sin rama feature
- **Qué hice mal:** Comité los cambios de Phase 1.2 (TransactionManager, MVCC snapshots) directamente en la rama `develop` del repo enterprise, sin crear una rama `feature/concurrency-transactions` primero.
- **Causa raíz:** Seguí el flujo del repo tessera-graph (que sí tenía rama feature) pero no repliqué el mismo patrón en el repo enterprise. Asumí que como era el primer commit sustancial del enterprise, podía ir directo a develop.
- **Cómo lo solucioné:** El commit ya está en `develop`. En la próxima sesión se puede crear la rama feature retroactivamente o aceptar este commit en develop y aplicar Git Flow correctamente de aquí en adelante.
- **Regla para evitarlo:** SIEMPRE crear rama `feature/*` o `fix/*` desde `develop` antes de commitear, sin excepciones. Aplica a TODOS los repos, incluso en commits iniciales.

### [2026-03-14] Push a remote sin verificar que el remote existía
- **Qué hice mal:** Ejecuté `git push origin develop` en el repo enterprise sin verificar primero que hubiera un remote configurado. El comando falló con "origin does not appear to be a git repository".
- **Causa raíz:** Asumí que el remote estaba configurado porque el otro repo (tessera-graph) sí lo tenía. No verifiqué con `git remote -v` antes de intentar el push.
- **Cómo lo solucioné:** Verifiqué con `git remote -v` (vacío) y reporté al usuario que no hay remote configurado.
- **Regla para evitarlo:** SIEMPRE ejecutar `git remote -v` antes del primer push en cualquier repo. No asumir que un repo tiene remote configurado solo porque otro repo del mismo ecosistema lo tiene.

### [2026-03-14] Múltiples errores de clippy no anticipados tras migración RefCell→RwLock
- **Qué hice mal:** La migración generó 6+ errores de clippy (`significant_drop_tightening`, `missing_panics_doc`, `doc_markdown`, `redundant_closure`, `must_use` redundante) que no anticipé y tuve que corregir iterativamente.
- **Causa raíz:** No ejecuté `cargo clippy` tras cada cambio incremental. Acumulé cambios y descubrí los errores al final.
- **Cómo lo solucioné:** Corregí cada warning uno a uno: `drop(page)` para drops tempranos, `#[allow(clippy::significant_drop_tightening)]` donde era necesario, docs de `# Panics`, etc.
- **Regla para evitarlo:** Ejecutar `cargo clippy` después de cada ciclo TDD (no solo al final). Cuando se migra de RefCell a RwLock, anticipar: (1) `significant_drop_tightening` por guards, (2) `missing_panics_doc` por `.expect()`, (3) `must_use` en guards que ya lo tienen.

### [2026-03-14] `cargo test name1 name2` no funciona como se esperaba
- **Qué hice mal:** Intenté ejecutar `cargo test test_name1 test_name2` esperando que corriera ambos tests. Cargo test solo acepta un patrón, no múltiples nombres separados por espacio.
- **Causa raíz:** Desconocimiento de la API de `cargo test`. Los argumentos después del primero se pasan al test binary, no son patrones adicionales.
- **Cómo lo solucioné:** Usé pattern matching con `|` o ejecuté tests por separado.
- **Regla para evitarlo:** Para múltiples tests en cargo: usar `cargo test 'pattern1\|pattern2'` o ejecutar cada uno por separado. Un solo argumento de filtro por invocación.

### [2026-03-14] Throughput test falló en modo debug (17k < 50k threshold)
- **Qué hice mal:** Escribí un test de throughput con threshold de 50k ops/sec sin considerar que en modo debug (sin optimizaciones) el rendimiento sería mucho menor.
- **Causa raíz:** No consideré que `cargo test` compila en debug por defecto y que los benchmarks/throughput tests necesitan thresholds diferentes para debug vs release.
- **Cómo lo solucioné:** Añadí `cfg!(debug_assertions)` para usar 10k en debug y 50k en release.
- **Regla para evitarlo:** SIEMPRE usar thresholds condicionales en tests de rendimiento: `if cfg!(debug_assertions) { threshold_debug } else { threshold_release }`. Los tests de throughput en debug son ~3-5x más lentos.
