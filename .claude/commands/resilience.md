---
description: "Auditoría de resiliencia operativa para TesseraGraph Enterprise"
argument-hint: "[modulo-o-ruta]"
---

Ejecuta una auditoría de resiliencia operativa sobre tessera-graph-enterprise. Esto NO es un lint de código — es una auditoría de modos de fallo reales que causan incidentes en producción.

Scope: $ARGUMENTS (default: todo el proyecto)

Lanza un agente con `subagent_type="security-expert-advisor"` que ejecute TODOS los checks siguientes. El agente debe ejecutar comandos reales (grep, cargo, lectura de archivos) — no asumir resultados. Reportar hallazgos con archivos y líneas concretos. NO implementar fixes.

---

## A. Memoria y recursos

### A1. Estructuras unbounded en hot paths
```bash
# Buscar Vec::new(), HashMap::new(), BTreeMap::new() en código de producción (no tests)
# que estén en paths de request handling (bolt_handler, query execution, import)
grep -rn "Vec::new\|HashMap::new\|BTreeMap::new\|DashMap::new" crates/ --include="*.rs" | grep -v test | grep -v bench
```
- Verificar si cada estructura tiene un cap o crece sin límite
- CRITICAL si está en hot path sin límite y alimentada por input del cliente

### A2. Canales sin límite
```bash
grep -rn "unbounded_channel\|channel()" crates/ --include="*.rs" | grep -v test
```
- Verificar que los canales bounded tienen capacidad razonable
- HIGH si hay `unbounded_channel` en paths de producción

### A3. Query results materialized en memoria
- Leer el path de ejecución de queries: `bolt_handler.rs` → `gql::execute` → `PendingResult`
- ¿Los resultados se acumulan enteros en `Vec<Vec<PackStreamValue>>` antes de enviar al cliente?
- ¿Hay streaming o paginación, o un MATCH que retorne 1M nodos carga todo en RAM?
- HIGH si no hay límite en el tamaño del result set

### A4. Memory limit configurado pero no enforced
```bash
grep -rn "MEMORY_LIMIT\|memory_limit" crates/ --include="*.rs" | grep -v test
```
- ¿El valor se parsea y se usa, o solo se parsea?
- HIGH si se acepta configuración que da falsa sensación de seguridad

### A5. Session/token storage sin cleanup
- Leer `session.rs`: ¿hay TTL enforcement? ¿cleanup periódico? ¿o crece sin límite?
- Leer `rate_limit.rs`: ¿las entradas de rate limiting se limpian después del cooldown?
- HIGH si HashMap crece sin límite en proceso de larga duración

### A6. Tenant registry sin eviction
- Leer `registry.rs`: ¿hay API para descargar un tenant de memoria?
- ¿Qué pasa con un server que sirve 1000 tenants y cada uno consume memory?
- MEDIUM si no hay mecanismo de unload

---

## B. Concurrencia y race conditions

### B1. Lock ordering
```bash
# Buscar nested locks: funciones que adquieren más de un lock
grep -rn "\.lock()\|\.read()\|\.write()" crates/ --include="*.rs" | grep -v test
```
- ¿Hay funciones que adquieren Lock A y luego Lock B?
- ¿Existe otra función que adquiere Lock B y luego Lock A? (deadlock potential)
- CRITICAL si hay lock inversion

### B2. Read-then-write (TOCTOU)
- Buscar patrones: `if map.contains_key() { ... } map.insert()`
- O `let val = map.get(); ... map.insert(val + 1)`
- Estos son race conditions si otro thread puede modificar entre ambas operaciones
- HIGH si están en paths concurrentes sin lock exclusivo

### B3. Lock poisoning
```bash
grep -rn "\.lock()\|\.read()\|\.write()" crates/ --include="*.rs" | grep -v test | grep "unwrap()"
```
- Un `lock().unwrap()` en producción propaga el panic de otro thread → cascade crash
- CRITICAL si hay `lock().unwrap()` en hot paths
- OK si usa `lock().map_err()` o `lock().ok()`

### B4. Guards held across .await
```bash
# Buscar patrones donde un MutexGuard o RwLockGuard está vivo en un .await
# Esto hace el Future !Send y falla en tokio multi-thread
grep -rn "\.lock()\|\.read()\|\.write()" crates/ --include="*.rs" | grep -v test
```
- Leer las funciones async que usan locks: ¿el guard se droppea antes del .await?
- CRITICAL si un guard cruza un .await en código de producción

---

## C. Durabilidad y corrupción de datos

### C1. WAL habilitado por defecto
```bash
grep -rn "WAL_ENABLED\|wal_enabled" crates/ --include="*.rs"
```
- ¿El default es `true`? ¿Se puede deshabilitar sin advertencia?
- CRITICAL si WAL puede deshabilitarse silenciosamente

### C2. Atomicity gaps
- Leer `txn/manager.rs`: ¿hay un punto donde WAL.sync() ha pasado pero el estado in-memory no se ha actualizado?
- Si el proceso muere en ese punto, ¿la transacción committed se pierde?
- CRITICAL si hay gap documentado o no documentado

### C3. Partial writes
- ¿Qué pasa si el proceso muere durante un `flush()` (page file write)?
- ¿Las páginas se escriben atómicamente o puede quedar una página a medias?
- ¿El WAL puede recuperar de una página parcialmente escrita?
- HIGH si no hay protección contra partial page writes

### C4. CRC/checksum en WAL
```bash
grep -rn "crc\|checksum\|CRC" crates/ --include="*.rs"
# También buscar en el MIT core
grep -rn "crc\|checksum\|CRC" ../tessera-graph/crates/ --include="*.rs"
```
- ¿Cada WAL record tiene checksum? ¿Se valida al leer?
- HIGH si no hay checksums (corrupción silenciosa)

### C5. Index rebuild
- ¿Existe mecanismo para reconstruir índices desde los datos?
- Si el índice de labels se corrompe, ¿cómo se recupera?
- ¿El adjacency index puede reconstruirse?
- MEDIUM si no hay rebuild (requiere re-import de datos)

### C6. Backup integrity
```bash
grep -rn "verify\|checksum\|integrity\|CRC" crates/tessera-graph-storage/ --include="*.rs"
```
- ¿Los backups se validan con checksums?
- ¿Hay un comando de verificación de backup?
- MEDIUM si no hay verificación post-backup

---

## D. Agotamiento de recursos

### D1. Límite de conexiones
```bash
grep -rn "max_connections\|MAX_CONNECTIONS\|Semaphore" crates/ --include="*.rs" | grep -v test
```
- ¿Se enforza? ¿Qué pasa cuando se alcanza? (drop silencioso? error al cliente?)
- HIGH si no hay límite o el overflow no se maneja

### D2. File descriptors
- Contar cuántos archivos abre cada tenant (WAL + page files + index + adj)
- Con N tenants y M conexiones, ¿cuántos FDs se necesitan?
- ¿Se respeta `ulimit -n`?
- MEDIUM si el cálculo muestra que el default puede exceder limits comunes (1024)

### D3. Disk space en runtime
```bash
grep -rn "disk\|space\|statvfs\|available" crates/ --include="*.rs" | grep -v test | grep -v bench
```
- ¿Se verifica espacio antes de escribir en WAL o page files?
- ¿Solo se verifica en backup?
- HIGH si un disco lleno causa crash en vez de error graceful

### D4. Timeouts en todos los I/O paths
- ¿El metrics HTTP server tiene read timeout?
- ¿El Bolt handler tiene timeout en la lectura de mensajes?
- ¿Los flush writes tienen timeout?
- ¿Las conexiones LDAP/OIDC tienen timeout?
- HIGH si algún path de I/O puede bloquear indefinidamente

---

## E. Recuperación de fallos

### E1. WAL replay automático
- ¿`Graph::open()` hace WAL replay automáticamente?
- ¿El replay es idempotente? (¿se puede re-aplicar sin efectos duplicados?)
- CRITICAL si WAL replay no es automático o no es idempotente

### E2. Corrupción parcial de WAL
- ¿Qué pasa si el WAL tiene un record truncado al final? (crash durante write)
- ¿Se detecta? ¿Se salta? ¿Se pierden records posteriores?
- HIGH si la corrupción parcial impide el arranque

### E3. SIGKILL vs SIGTERM
- Identificar qué datos se pierden con SIGKILL:
  - Contenido del `BufWriter` no flusheado
  - Mutations entre último flush y el kill
  - Sessions en memoria
- ¿La ventana de pérdida es configurable? (flush_interval_ms)
- INFORMATIVO — documentar la ventana de pérdida

### E4. Clock skew
```bash
grep -rn "SystemTime\|Instant\|timestamp\|Duration" crates/ --include="*.rs" | grep -v test | head -30
```
- ¿Qué pasa si el reloj del sistema salta hacia atrás? (NTP correction)
- ¿Los TTL de sesiones usan `Instant` (monotónico) o `SystemTime` (wall clock)?
- HIGH si session TTL usa wall clock y puede crear sesiones inmortales

---

## F. Gestión de errores

### F1. Errores swallowed
```bash
grep -rn "let _ =" crates/ --include="*.rs" | grep -v test | grep -v "// OK"
```
- Cada `let _ = expr` descarta un Result. ¿Es intencional?
- HIGH si descarta errores de I/O o de seguridad

### F2. Panics en hot paths
```bash
# unwrap/expect en código de producción (no tests, no startup)
grep -rn "\.unwrap()\|\.expect(" crates/ --include="*.rs" | grep -v test | grep -v bench | grep -v main.rs | grep -v "// OK\|// SAFETY"
```
- CRITICAL si hay unwrap en el handler de queries o mutations
- OK si solo está en startup (main.rs) con mensajes descriptivos

### F3. Error types completos
- ¿Existe un error type para cada modo de fallo?
- ¿Disk full, WAL corruption, lock poisoned, timeout — todos tienen su variante?
- MEDIUM si hay `anyhow::Error` genérico en vez de tipos específicos

---

## G. Seguridad bajo estrés

### G1. Auth bypass
- ¿Hay algún path que ejecute queries sin verificar session token?
- ¿El health endpoint está protegido o es público? (debe ser público)
- ¿El metrics endpoint expone datos sensibles?
- CRITICAL si hay bypass

### G2. Secrets en código
```bash
grep -rni "password\|secret\|token\|key" crates/ --include="*.rs" | grep -v test | grep -v "//\|doc\|///\|pub fn\|struct\|enum\|trait" | head -20
```
- ¿Hay passwords hardcodeados? ¿Keys en strings?
- CRITICAL si hay secrets

### G3. Audit overflow
- ¿Qué pasa si el channel de audit está lleno? ¿Se bloquea? ¿Se pierden eventos?
- ¿El disco de audit logs puede llenarse?
- MEDIUM si audit events se pierden silenciosamente

---

## H. Observabilidad

### H1. Health endpoint accuracy
- ¿El health check refleja el estado real de TODOS los subsistemas?
- ¿O solo dice "alive" sin verificar nada?
- MEDIUM si el health es superficial

### H2. Métricas de sistema
- ¿Hay métricas de disco, memoria RSS, file descriptors?
- ¿O solo métricas de aplicación (connections, queries)?
- MEDIUM si faltan métricas de sistema

---

## I. Configuración segura

### I1. Defaults fail-safe
- ¿Si `TESSERA_ADMIN_PASSWORD` no está configurado, el server arranca?
- ¿Si los certs TLS no existen, el server arranca?
- CRITICAL si arranca sin auth o sin TLS

### I2. Env vars validadas al startup
- ¿Las env vars se validan al inicio o al primer uso?
- ¿Un typo en `TESSERA_FLUSH_INTERVAL_MS=abc` causa un crash tardío?
- HIGH si hay validación lazy que puede crashear en runtime

---

## Formato de salida

```
# Auditoría de Resiliencia — TesseraGraph Enterprise
Fecha: YYYY-MM-DD
Commit: [hash]

## Estado General: [PASS / FAIL / WARNING]

## CRITICAL (pérdida de datos, corrupción, bypass de seguridad)
| # | Check | Hallazgo | Archivo:Línea | Impacto |
|---|-------|----------|---------------|---------|

## HIGH (memory leak, resource exhaustion, crash)
| # | Check | Hallazgo | Archivo:Línea | Impacto |
|---|-------|----------|---------------|---------|

## MEDIUM (degradación, observabilidad)
| # | Check | Hallazgo | Archivo:Línea | Impacto |
|---|-------|----------|---------------|---------|

## LOW (mejoras de robustez)
| # | Check | Hallazgo | Archivo:Línea | Impacto |
|---|-------|----------|---------------|---------|

## PASS (checks superados)
| # | Check | Estado |
|---|-------|--------|

## Resumen
- Checks ejecutados: X/50
- CRITICAL: Y
- HIGH: Z
- MEDIUM: W
- PASS: V
```

Lanza el agente ahora.
