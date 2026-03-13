# TDD Plan: Quality Fixes — tessera-graph-enterprise Scaffold

## Contexto

Corrección de 3 bugs críticos y 6 mejoras recomendadas en el scaffold del
workspace enterprise. Los issues van desde warnings de Clippy que rompen
`-D warnings` en los 11 crates, hasta crates huérfanos no conectados al
binario y ausencia de configuración de perfil de release.

**Stack detectado**: Rust 2024, MSRV 1.85, workspace de 11 crates
**Convenciones**: `forbid(unsafe_code)`, `deny(clippy::all)`, `thiserror`
para errores de librería, headers `//!` en cada `lib.rs`
**Afecta hot path**: No (scaffold y configuración)

---

## Orden de ejecución

Los issues se agrupan en 4 ciclos:

| Ciclo | Issues | Tipo |
|-------|--------|------|
| C1 | #11 `doc_markdown` en copyright headers | Bug crítico con test de compilación |
| C2 | #12 `tessera-server` sin dep `tessera-graph` | Bug crítico con test de compilación |
| C3 | #13 3 crates huérfanos | Bug crítico — conectar al binario |
| C4 | #14–#19 Configuración y mejoras | Sin tests (configuración pura) |

---

## Ciclo 1 — `doc_markdown` rompe `-D warnings` en los 11 crates (Issue #11)

**Archivos**: `src/lib.rs` (o `src/main.rs`) de los 11 crates, línea del
copyright

### Diagnóstico

Clippy `doc_markdown` detecta `BelowZero` en comentarios `//!` como un
posible link de Markdown no formateado. El lint se activa porque `BelowZero`
parece un identificador que debería ir entre backticks o ser un link.

Los 11 archivos afectados con la línea problemática:
```
//! Copyright 2026 BelowZero Security OU. All rights reserved.
```

### RED — Verificar que el warning existe actualmente

```bash
# Debe producir warnings doc_markdown antes del fix
cargo clippy --workspace 2>&1 | grep -i "doc_markdown\|BelowZero"
```

### GREEN — Cambiar `//!` a `//` en la línea de copyright

El fix es mínimo: la línea de copyright no es documentación de la API pública,
por lo que no necesita `//!`. Cambiar en los 12 archivos (11 `lib.rs` + 1
`main.rs`):

```
// Antes (en cada archivo):
//! Copyright 2026 BelowZero Security OU. All rights reserved.

// Después:
// Copyright 2026 BelowZero Security OU. All rights reserved.
```

Los archivos a modificar son:

1. `crates/tessera-streaming/src/lib.rs`        — línea 3
2. `crates/tessera-replication/src/lib.rs`      — línea 3
3. `crates/tessera-import/src/lib.rs`           — línea 3
4. `crates/tessera-auth/src/lib.rs`             — línea 3
5. `crates/tessera-protocol/src/lib.rs`         — línea 3
6. `crates/tessera-audit/src/lib.rs`            — línea 3
7. `crates/tessera-monitor/src/lib.rs`          — línea 3
8. `crates/tessera-config/src/lib.rs`           — línea 3
9. `crates/tessera-storage-enterprise/src/lib.rs` — línea 3
10. `crates/tessera-tenant/src/lib.rs`          — línea 3
11. `crates/tessera-server/src/main.rs`         — línea 3

### Verificación post-fix

```bash
cargo clippy --workspace -- -D warnings 2>&1 | grep -E "^error"
# Debe devolver 0 líneas
```

### REFACTOR

Ninguno. El cambio de `//!` a `//` es solo para la línea de copyright; el
resto del header `//!` (descripción del crate) permanece como está.

---

## Ciclo 2 — `tessera-server` no depende de `tessera-graph` (Issue #12)

**Archivo**: `crates/tessera-server/Cargo.toml`

### Diagnóstico

`tessera-server` es el binario principal que ejecutará queries GQL. Sin
`tessera-graph` como dependencia, no podrá compilar código de ejecución de
queries cuando se implemente. Añadirla ahora establece la dependencia correcta
antes de que sea bloqueante.

### RED — Test de compilación implícito

Añadir un `use` en `main.rs` que garantice que la dependencia es real:

```rust
// En crates/tessera-server/src/main.rs, después del header:
// Ensure tessera-graph is reachable (will be used for query execution).
use tessera_graph as _; // Used when query execution is implemented.
```

Antes de añadir la dep en `Cargo.toml`, este `use` falla con `error[E0432]`.
Ese es el RED.

### GREEN — Añadir dependencia en Cargo.toml

```toml
// crates/tessera-server/Cargo.toml, sección [dependencies]:
tessera-graph = { workspace = true }
```

Verificar que `tessera-graph` ya está declarada en `[workspace.dependencies]`
del `Cargo.toml` raíz (sí lo está: `tessera-graph = { path = "../tessera-graph" }`).

### Verificación

```bash
cargo build -p tessera-server 2>&1 | grep "^error"
# Debe devolver 0 líneas
```

### REFACTOR

Eliminar el `use tessera_graph as _` y el comentario asociado si se prefiere
no tener imports de placeholder. La dependencia en `Cargo.toml` es suficiente
para que esté disponible cuando se implemente.

> Alternativa aceptable: no añadir el `use` y simplemente añadir la dep en
> Cargo.toml. El test de compilación real será cuando se escriba el primer
> código que use `tessera_graph::*`. Documentar la dep con un comentario:
> `# Required for GQL query execution — will be used in server implementation`.

---

## Ciclo 3 — Crates huérfanos no conectados al binario (Issue #13)

**Crates**: `tessera-streaming`, `tessera-replication`, `tessera-import`
**Archivo principal**: `crates/tessera-server/Cargo.toml`

### Diagnóstico

Los 3 crates están en el workspace pero `tessera-server` no los importa. Esto
significa que:
1. `cargo build` no los compila por defecto al construir el binario.
2. Son inalcanzables desde el punto de entrada del sistema.
3. `cargo test --workspace` sí los ejecuta (tienen su propia compilación),
   pero el binario en producción nunca los usará.

### RED — Test de alcanzabilidad

Añadir `use` statements en `main.rs` que fallan hasta que se añaden las deps:

```rust
// En crates/tessera-server/src/main.rs:
use tessera_streaming as _;   // Streaming connectors (Kafka, Pulsar)
use tessera_replication as _; // HA replication and Raft
use tessera_import as _;      // CSV/JSON/GQL import-export
```

Antes de la corrección estos `use` producen `error[E0432]` (paquete no encontrado
como dependencia directa de `tessera-server`).

### GREEN — Añadir dependencias en tessera-server/Cargo.toml

```toml
[dependencies]
# ...dependencias existentes...
tessera-streaming   = { workspace = true }
tessera-replication = { workspace = true }
tessera-import      = { workspace = true }
```

### Verificación

```bash
cargo build -p tessera-server 2>&1 | grep "^error"
# Debe devolver 0 líneas

# Verificar que los 3 crates se compilan como parte del binario:
cargo build -p tessera-server --message-format=json 2>/dev/null \
  | grep '"target":' | grep -E "streaming|replication|import"
```

### REFACTOR

Igual que en C2: los `use ... as _` de placeholder pueden eliminarse después
de añadir las deps. El objetivo es que las dependencias queden declaradas en
`Cargo.toml` para que el compilador las enlace. Mantener un comentario
explicativo en cada dependencia añadida:

```toml
tessera-streaming   = { workspace = true } # Streaming connectors: Kafka, Pulsar, Redpanda
tessera-replication = { workspace = true } # HA leader-follower replication and Raft
tessera-import      = { workspace = true } # CSV/JSON/GQL import-export and SQL migration
```

---

## Ciclo 4 — Configuración y mejoras sin tests (Issues #14–#19)

Estos issues son cambios de configuración sin comportamiento testeable en
tiempo de ejecución. Se agrupan en un único ciclo de configuración.

### C4.1 — `[profile.release]` con LTO y optimizaciones (Issue #14)

**Archivo**: `Cargo.toml` (raíz del workspace)

Añadir al final del `Cargo.toml` raíz:

```toml
[profile.release]
lto           = true      # Link-Time Optimization: reduce binary size, improve perf
codegen-units = 1         # Single codegen unit: best optimization at cost of compile time
panic         = "abort"   # Abort on panic: smaller binary, no unwinding overhead
strip         = "symbols" # Strip debug symbols from release binary
```

Justificación:
- `lto = true`: mejora inlining cross-crate, crítico para un servidor de base de datos.
- `codegen-units = 1`: permite más oportunidades de optimización en binarios LLVM.
- `panic = "abort"`: correcto para un servidor; el unwinding no aporta recuperabilidad.
- `strip = "symbols"`: reduce el tamaño del binario distribuible.

### C4.2 — Feature flags para modularidad enterprise (Issue #15)

**Archivo**: `crates/tessera-server/Cargo.toml`

Añadir sección `[features]`:

```toml
[features]
default = ["streaming", "replication", "import"]

# Enable Kafka/Pulsar/Redpanda streaming connectors
streaming   = ["tessera-streaming"]
# Enable HA leader-follower replication and Raft consensus
replication = ["tessera-replication"]
# Enable CSV/JSON/GQL import-export and SQL migration tools
import      = ["tessera-import"]
```

Cambiar las 3 deps de C3 a opcionales:

```toml
tessera-streaming   = { workspace = true, optional = true }
tessera-replication = { workspace = true, optional = true }
tessera-import      = { workspace = true, optional = true }
```

Esto permite builds mínimas (`--no-default-features`) para despliegues
que no necesiten todos los módulos.

> Nota: si C3 ya añadió las deps como no-opcionales, este ciclo las convierte
> a opcionales. Ejecutar ambos ciclos en orden (C3 antes que C4.2).

### C4.3 — `.cargo/config.toml` con alias útiles (Issue #16)

**Archivo**: `.cargo/config.toml` (crear, no existe)

```toml
[alias]
# Build and run the server binary
serve    = "run -p tessera-server --"
# Run all tests with output
test-all = "test --workspace -- --nocapture"
# Run clippy with deny warnings (same as CI)
lint     = "clippy --workspace -- -D warnings"
# Check compilation without building
chk      = "check --workspace --all-targets"
```

### C4.4 — Usar `anyhow` en tessera-server (Issue #17)

**Archivos**: `crates/tessera-server/Cargo.toml`, `crates/tessera-server/src/main.rs`

Los binarios no exponen tipos de error a otros crates, por lo que `thiserror`
(pensado para librerías) es excesivo. `anyhow` simplifica el manejo de errores
en el punto de entrada.

Cambios en `Cargo.toml`:
```toml
# Eliminar:
thiserror = { workspace = true }
# Añadir:
anyhow = "1"
```

Añadir `anyhow` a `[workspace.dependencies]` en el `Cargo.toml` raíz:
```toml
anyhow = "1"
```

Actualizar `main.rs` para usar `anyhow::Result`:
```rust
fn main() -> anyhow::Result<()> {
    Ok(())
}
```

> Nota: `thiserror` sigue siendo correcto para todos los crates de librería.
> Solo `tessera-server` (el binario) cambia a `anyhow`.

### C4.5 — `cargo-deny` para auditoría de licencias (Issue #18)

**Archivos**: `deny.toml` (crear en raíz del workspace)

```toml
# cargo-deny configuration for license and dependency auditing
# Run: cargo deny check

[licenses]
# Allow only these SPDX license identifiers
allow = [
    "MIT",
    "Apache-2.0",
    "Apache-2.0 WITH LLVM-exception",
    "BSD-2-Clause",
    "BSD-3-Clause",
    "ISC",
    "Unicode-3.0",
]
# The workspace itself uses a proprietary license — exempt it
exceptions = [
    { allow = ["LicenseRef-Proprietary"], name = "tessera-server" },
    { allow = ["LicenseRef-Proprietary"], name = "tessera-protocol" },
    { allow = ["LicenseRef-Proprietary"], name = "tessera-auth" },
    { allow = ["LicenseRef-Proprietary"], name = "tessera-storage-enterprise" },
    { allow = ["LicenseRef-Proprietary"], name = "tessera-import" },
    { allow = ["LicenseRef-Proprietary"], name = "tessera-streaming" },
    { allow = ["LicenseRef-Proprietary"], name = "tessera-monitor" },
    { allow = ["LicenseRef-Proprietary"], name = "tessera-audit" },
    { allow = ["LicenseRef-Proprietary"], name = "tessera-config" },
    { allow = ["LicenseRef-Proprietary"], name = "tessera-replication" },
    { allow = ["LicenseRef-Proprietary"], name = "tessera-tenant" },
]

[bans]
# Deny duplicate versions of the same crate (forces dependency alignment)
multiple-versions = "warn"

[advisories]
# Deny crates with known security vulnerabilities
vulnerability = "deny"
unmaintained  = "warn"
notice        = "warn"

[sources]
# Only allow crates from crates.io and the local workspace
unknown-registry = "deny"
unknown-git      = "deny"
```

Instrucción de instalación (no automatizable en este plan):
```bash
cargo install cargo-deny --locked
cargo deny check
```

### C4.6 — `#[non_exhaustive]` en enums de error (Issue #19)

**Nota para implementación futura**: cuando se añadan variantes reales a los
enums de error de los crates de librería, añadir `#[non_exhaustive]` al enum:

```rust
// Ejemplo en cualquier src/error.rs de los crates enterprise:
#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum Error {
    // variantes...
}
```

Este issue no tiene un ciclo de implementación ahora porque los enums actuales
de los crates son placeholders vacíos o tienen solo una variante. El atributo
`#[non_exhaustive]` aporta valor cuando hay al menos 2 variantes y existe la
posibilidad de añadir más en versiones futuras. Registrado aquí como recordatorio.

---

## Resumen de archivos modificados

| Ciclo | Archivos |
|-------|----------|
| C1 | 11 archivos `src/lib.rs` + `crates/tessera-server/src/main.rs` |
| C2 | `crates/tessera-server/Cargo.toml` |
| C3 | `crates/tessera-server/Cargo.toml` |
| C4.1 | `Cargo.toml` (raíz) |
| C4.2 | `crates/tessera-server/Cargo.toml` |
| C4.3 | `.cargo/config.toml` (nuevo) |
| C4.4 | `Cargo.toml` (raíz), `crates/tessera-server/Cargo.toml`, `crates/tessera-server/src/main.rs` |
| C4.5 | `deny.toml` (nuevo) |
| C4.6 | N/A (nota futura) |

## Estimación Total

- C1 (copyright headers): 30 min (12 archivos, cambio mecánico)
- C2 (dep tessera-graph): 15 min
- C3 (crates huérfanos): 15 min
- C4.1 (profile.release): 15 min
- C4.2 (feature flags): 30 min
- C4.3 (.cargo/config.toml): 15 min
- C4.4 (anyhow): 20 min
- C4.5 (cargo-deny): 30 min
- C4.6 (non_exhaustive): 0 min (nota futura)

**Total**: ~3 horas

## Criterios de Éxito

- [ ] `cargo clippy --workspace -- -D warnings` sin errores (C1)
- [ ] `cargo build -p tessera-server` sin errores (C2, C3)
- [ ] `cargo build -p tessera-server --release` produce binario optimizado (C4.1)
- [ ] `cargo build -p tessera-server --no-default-features` compila (C4.2)
- [ ] `cargo lint` (alias) funciona (C4.3)
- [ ] `cargo build -p tessera-server` usa `anyhow` sin `thiserror` (C4.4)
- [ ] `cargo deny check` pasa sin errores de licencias (C4.5)
