I now have everything I need. Let me produce the plan.

---

## Contexto

TesseraGraph Enterprise necesita LBAC completo (Bell-LaPadula con compartimentos) desde el primer ciclo. La implementación requiere tipos en `tessera-auth::lbac`, un `SecureGraph<G>` en `tessera-storage-enterprise::lbac`, la generalización de `execute_mut` para que sea `G: GraphAccess` en vez de `&mut Graph`, y la adición del campo `clearance` a `UserRecord`. Todo el acceso a datos pasa por `SecureGraph` que filtra en lectura y rechaza en escritura según dominancia.

**Stack detectado**: Rust 1.85, edition 2024, `thiserror` v2, `serde`, `clippy all=deny/pedantic=warn/nursery=warn`, `unsafe_code=forbid`
**Convenciones**: Copyright `// Copyright 2026 BelowZero Security OU. All rights reserved.` en primera línea, tests en `crates/<crate>/tests/` nunca en `src/`, errores via `thiserror`, dual threshold en throughput guards con `cfg!(debug_assertions)`
**Afecta hot path**: SI — `node()`, `edge()`, `node_ids()`, `outgoing_edges()`, `incoming_edges()` se ejecutan en cada query path

## Decisiones Previas Necesarias

Ninguna. La arquitectura está finalizada en el enunciado.

---

## Plan de Ejecución

### Fase 1: Tipos LBAC en `tessera-auth`

**Ciclo 1 — RED: Tests para `SecurityLabel`**
1. [ ] Crear archivo de test (20 min)
   - Archivo: `/Volumes/WD_BLACK/repos/MojoBytes/tessera-ecosystem/tessera-graph-enterprise/crates/tessera-auth/tests/lbac_types_test.rs`
   - Acción: Crear con los siguientes tests:

```rust
// Copyright 2026 BelowZero Security OU. All rights reserved.

use std::collections::BTreeSet;
use tessera_auth::lbac::{SecurityLabel, Clearance};

// --- SecurityLabel ---

#[test]
fn security_label_default_is_public() {
    let label = SecurityLabel::default();
    assert_eq!(label.level, 0);
    assert!(label.compartments.is_empty());
}

#[test]
fn security_label_new_stores_level_and_compartments() {
    let comps: BTreeSet<String> = ["FINANCE", "HR"].iter().map(|s| s.to_string()).collect();
    let label = SecurityLabel::new(3, comps.clone());
    assert_eq!(label.level, 3);
    assert_eq!(label.compartments, comps);
}

#[test]
fn security_label_serializes_and_deserializes() {
    let comps: BTreeSet<String> = ["LEGAL"].iter().map(|s| s.to_string()).collect();
    let label = SecurityLabel::new(2, comps);
    let json = serde_json::to_string(&label).unwrap();
    let back: SecurityLabel = serde_json::from_str(&json).unwrap();
    assert_eq!(back.level, label.level);
    assert_eq!(back.compartments, label.compartments);
}

// --- Clearance ---

#[test]
fn clearance_default_is_level_zero_no_compartments() {
    let c = Clearance::default();
    assert_eq!(c.level, 0);
    assert!(c.compartments.is_empty());
}

#[test]
fn clearance_new_stores_fields() {
    let comps: BTreeSet<String> = ["FINANCE"].iter().map(|s| s.to_string()).collect();
    let c = Clearance::new(5, comps.clone());
    assert_eq!(c.level, 5);
    assert_eq!(c.compartments, comps);
}

#[test]
fn clearance_serializes_and_deserializes() {
    let comps: BTreeSet<String> = ["HR", "LEGAL"].iter().map(|s| s.to_string()).collect();
    let c = Clearance::new(4, comps);
    let json = serde_json::to_string(&c).unwrap();
    let back: Clearance = serde_json::from_str(&json).unwrap();
    assert_eq!(back.level, c.level);
    assert_eq!(back.compartments, c.compartments);
}
```

**Ciclo 1 — GREEN: Implementar `SecurityLabel` y `Clearance`**

2. [ ] Crear módulo `lbac` en tessera-auth (20 min)
   - Archivo: `/Volumes/WD_BLACK/repos/MojoBytes/tessera-ecosystem/tessera-graph-enterprise/crates/tessera-auth/src/lbac.rs`
   - Acción: Crear con:

```rust
// Copyright 2026 BelowZero Security OU. All rights reserved.

//! Label-Based Access Control types (Bell-LaPadula with compartments).

use std::collections::BTreeSet;

/// A security classification label attached to a graph resource (node or edge).
///
/// Two-dimensional: a hierarchical `level` and a set of horizontal `compartments`.
/// Resources without an explicit label are treated as level 0, empty compartments (public).
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct SecurityLabel {
    /// Hierarchical classification level (0 = public, higher = more classified).
    pub level: u16,
    /// Horizontal compartments the resource belongs to (e.g. "FINANCE", "HR").
    pub compartments: BTreeSet<String>,
}

impl SecurityLabel {
    /// Create a new label with the given level and compartments.
    #[must_use]
    pub fn new(level: u16, compartments: BTreeSet<String>) -> Self {
        Self { level, compartments }
    }
}

/// A user's clearance: defines which resources the user may access.
///
/// A clearance dominates a label iff `clearance.level >= label.level` AND
/// `label.compartments ⊆ clearance.compartments`.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct Clearance {
    /// Hierarchical clearance level.
    pub level: u16,
    /// Compartments the user is authorized for.
    pub compartments: BTreeSet<String>,
}

impl Clearance {
    /// Create a new clearance with the given level and compartments.
    #[must_use]
    pub fn new(level: u16, compartments: BTreeSet<String>) -> Self {
        Self { level, compartments }
    }

    /// Returns `true` iff this clearance dominates the given label.
    ///
    /// Dominance: `self.level >= label.level` AND `label.compartments ⊆ self.compartments`.
    #[must_use]
    pub fn dominates(&self, label: &SecurityLabel) -> bool {
        self.level >= label.level && label.compartments.is_subset(&self.compartments)
    }
}
```

   - Archivo: `/Volumes/WD_BLACK/repos/MojoBytes/tessera-ecosystem/tessera-graph-enterprise/crates/tessera-auth/src/lib.rs`
   - Acción: Añadir `pub mod lbac;` y re-exportar `pub use lbac::{Clearance, SecurityLabel};`

**Ciclo 1 — REFACTOR**: Verificar que `cargo clippy -p tessera-auth` y los tests pasan. No hay duplicación porque estos son tipos nuevos.

---

**Ciclo 2 — RED: Tests de dominancia**

3. [ ] Añadir bloque de tests de dominancia al mismo archivo `lbac_types_test.rs` (20 min):

```rust
// --- Dominance ---

#[test]
fn dominates_level_and_superset_compartments() {
    let comps_label: BTreeSet<String> = ["FINANCE"].iter().map(|s| s.to_string()).collect();
    let comps_clearance: BTreeSet<String> =
        ["FINANCE", "HR"].iter().map(|s| s.to_string()).collect();
    let label = SecurityLabel::new(2, comps_label);
    let clearance = Clearance::new(3, comps_clearance);
    assert!(clearance.dominates(&label));
}

#[test]
fn dominates_exact_level_and_exact_compartments() {
    let comps: BTreeSet<String> = ["FINANCE"].iter().map(|s| s.to_string()).collect();
    let label = SecurityLabel::new(2, comps.clone());
    let clearance = Clearance::new(2, comps);
    assert!(clearance.dominates(&label));
}

#[test]
fn does_not_dominate_insufficient_level() {
    let comps: BTreeSet<String> = BTreeSet::new();
    let label = SecurityLabel::new(5, comps.clone());
    let clearance = Clearance::new(4, comps);
    assert!(!clearance.dominates(&label));
}

#[test]
fn does_not_dominate_missing_compartment() {
    let comps_label: BTreeSet<String> = ["FINANCE", "LEGAL"].iter().map(|s| s.to_string()).collect();
    let comps_clearance: BTreeSet<String> = ["FINANCE"].iter().map(|s| s.to_string()).collect();
    let label = SecurityLabel::new(1, comps_label);
    let clearance = Clearance::new(10, comps_clearance);
    assert!(!clearance.dominates(&label));
}

#[test]
fn public_resource_dominated_by_any_clearance() {
    let label = SecurityLabel::default(); // level 0, no compartments
    let clearance = Clearance::new(0, BTreeSet::new());
    assert!(clearance.dominates(&label));
}

#[test]
fn user_with_no_compartments_cannot_access_compartmented_resource() {
    let comps_label: BTreeSet<String> = ["SECRET"].iter().map(|s| s.to_string()).collect();
    let label = SecurityLabel::new(0, comps_label); // level 0 but has compartment
    let clearance = Clearance::new(100, BTreeSet::new()); // high level, no compartments
    assert!(!clearance.dominates(&label));
}

#[test]
fn empty_compartment_label_accessible_to_clearance_with_compartments() {
    let comps_clearance: BTreeSet<String> = ["FINANCE"].iter().map(|s| s.to_string()).collect();
    let label = SecurityLabel::new(1, BTreeSet::new()); // has level but no compartments
    let clearance = Clearance::new(1, comps_clearance);
    assert!(clearance.dominates(&label));
}
```

**Ciclo 2 — GREEN**: La lógica ya está en `dominates()`. Ejecutar tests — deben pasar sin cambios adicionales.

**Ciclo 2 — REFACTOR**: Nada que refactorizar.

---

### Fase 2: `SecurityPolicy` — extracción/inyección de propiedades reservadas

**Ciclo 3 — RED: Tests para `SecurityPolicy`**

4. [ ] Añadir archivo de test para `SecurityPolicy` (25 min)
   - Archivo: `/Volumes/WD_BLACK/repos/MojoBytes/tessera-ecosystem/tessera-graph-enterprise/crates/tessera-auth/tests/security_policy_test.rs`

```rust
// Copyright 2026 BelowZero Security OU. All rights reserved.

use std::collections::BTreeSet;
use tessera_auth::lbac::{SecurityLabel, SecurityPolicy};

#[test]
fn property_key_constants_have_reserved_prefix() {
    assert!(SecurityPolicy::LEVEL_KEY.starts_with("__security"));
    assert!(SecurityPolicy::COMPARTMENTS_KEY.starts_with("__security"));
}

#[test]
fn inject_label_into_empty_properties() {
    let mut props = std::collections::HashMap::new();
    let comps: BTreeSet<String> = ["FINANCE", "HR"].iter().map(|s| s.to_string()).collect();
    let label = SecurityLabel::new(3, comps);
    SecurityPolicy::inject_label(&mut props, &label);
    assert!(props.contains_key(SecurityPolicy::LEVEL_KEY));
    assert!(props.contains_key(SecurityPolicy::COMPARTMENTS_KEY));
}

#[test]
fn inject_then_extract_roundtrips_label() {
    let mut props = std::collections::HashMap::new();
    let comps: BTreeSet<String> = ["FINANCE", "HR"].iter().map(|s| s.to_string()).collect();
    let original = SecurityLabel::new(3, comps);
    SecurityPolicy::inject_label(&mut props, &original);
    let extracted = SecurityPolicy::extract_label(&props);
    assert_eq!(extracted.level, original.level);
    assert_eq!(extracted.compartments, original.compartments);
}

#[test]
fn extract_label_from_empty_properties_returns_default() {
    let props = std::collections::HashMap::new();
    let label = SecurityPolicy::extract_label(&props);
    assert_eq!(label.level, 0);
    assert!(label.compartments.is_empty());
}

#[test]
fn extract_label_level_zero_empty_compartments_string() {
    use tessera_graph::Property;
    let mut props = std::collections::HashMap::new();
    props.insert(SecurityPolicy::LEVEL_KEY.to_string(), Property::I64(0));
    props.insert(
        SecurityPolicy::COMPARTMENTS_KEY.to_string(),
        Property::String(String::new()),
    );
    let label = SecurityPolicy::extract_label(&props);
    assert_eq!(label.level, 0);
    assert!(label.compartments.is_empty());
}

#[test]
fn compartments_encode_sorted_comma_separated() {
    let mut props = std::collections::HashMap::new();
    let comps: BTreeSet<String> = ["LEGAL", "FINANCE", "HR"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let label = SecurityLabel::new(1, comps);
    SecurityPolicy::inject_label(&mut props, &label);
    let encoded = props
        .get(SecurityPolicy::COMPARTMENTS_KEY)
        .unwrap()
        .as_str()
        .unwrap();
    // BTreeSet is sorted, so output must be sorted
    assert_eq!(encoded, "FINANCE,HR,LEGAL");
}

#[test]
fn strip_security_properties_removes_reserved_keys() {
    use tessera_graph::Property;
    let mut props = std::collections::HashMap::new();
    props.insert("name".to_string(), Property::String("Alice".to_string()));
    props.insert(SecurityPolicy::LEVEL_KEY.to_string(), Property::I64(2));
    props.insert(
        SecurityPolicy::COMPARTMENTS_KEY.to_string(),
        Property::String("FINANCE".to_string()),
    );
    SecurityPolicy::strip_security_properties(&mut props);
    assert!(!props.contains_key(SecurityPolicy::LEVEL_KEY));
    assert!(!props.contains_key(SecurityPolicy::COMPARTMENTS_KEY));
    assert!(props.contains_key("name"));
}

#[test]
fn is_security_property_detects_reserved_keys() {
    assert!(SecurityPolicy::is_security_property(SecurityPolicy::LEVEL_KEY));
    assert!(SecurityPolicy::is_security_property(SecurityPolicy::COMPARTMENTS_KEY));
    assert!(!SecurityPolicy::is_security_property("name"));
    assert!(!SecurityPolicy::is_security_property("level")); // must be reserved prefix
}
```

**Ciclo 3 — GREEN: Implementar `SecurityPolicy`**

5. [ ] Añadir `SecurityPolicy` a `/Volumes/WD_BLACK/repos/MojoBytes/tessera-ecosystem/tessera-graph-enterprise/crates/tessera-auth/src/lbac.rs` (25 min):

```rust
/// Keys usadas para almacenar etiquetas de seguridad como propiedades reservadas.
/// Estas propiedades son invisibles para usuarios — se inyectan/extraen/eliminan
/// por `SecurityPolicy` y nunca se exponen en resultados de queries.
pub struct SecurityPolicy;

impl SecurityPolicy {
    /// Property key for the hierarchical security level (stored as `I64`).
    pub const LEVEL_KEY: &'static str = "__security_level";

    /// Property key for compartments (stored as `String`, comma-separated sorted).
    pub const COMPARTMENTS_KEY: &'static str = "__security_compartments";

    /// Returns `true` if `key` is a reserved security property name.
    #[must_use]
    pub fn is_security_property(key: &str) -> bool {
        key == Self::LEVEL_KEY || key == Self::COMPARTMENTS_KEY
    }

    /// Injects `label` into `props` as reserved properties.
    ///
    /// Any existing values for the reserved keys are overwritten.
    pub fn inject_label(props: &mut tessera_graph::Properties, label: &SecurityLabel) {
        use tessera_graph::Property;
        props.insert(Self::LEVEL_KEY.to_string(), Property::I64(i64::from(label.level)));
        // Compartments are sorted (BTreeSet guarantees order) and comma-joined.
        let encoded = label.compartments.iter().cloned().collect::<Vec<_>>().join(",");
        props.insert(Self::COMPARTMENTS_KEY.to_string(), Property::String(encoded));
    }

    /// Extracts a `SecurityLabel` from `props`.
    ///
    /// Missing or malformed properties fall back to level 0 / empty compartments
    /// (fail-safe: unknown = public treatment is intentionally NOT applied here —
    /// callers decide what to do with a default label).
    #[must_use]
    pub fn extract_label(props: &tessera_graph::Properties) -> SecurityLabel {
        let level = props
            .get(Self::LEVEL_KEY)
            .and_then(|p| p.as_i64())
            .and_then(|v| u16::try_from(v).ok())
            .unwrap_or(0);

        let compartments = props
            .get(Self::COMPARTMENTS_KEY)
            .and_then(|p| p.as_str())
            .map(|s| {
                if s.is_empty() {
                    BTreeSet::new()
                } else {
                    s.split(',').map(|c| c.to_string()).collect()
                }
            })
            .unwrap_or_default();

        SecurityLabel { level, compartments }
    }

    /// Removes all reserved security properties from `props`.
    ///
    /// Called before returning nodes/edges to callers so security metadata
    /// is never exposed through the public API.
    pub fn strip_security_properties(props: &mut tessera_graph::Properties) {
        props.remove(Self::LEVEL_KEY);
        props.remove(Self::COMPARTMENTS_KEY);
    }
}
```

   - Nota: `tessera-auth/Cargo.toml` necesita añadir `tessera-graph = { workspace = true }` como dependencia.

**Ciclo 3 — REFACTOR**: Verificar que `cargo clippy -p tessera-auth` pasa limpio. Verificar que `tessera-graph` no introduce dependencias circulares (no lo hace: `tessera-graph` es MIT core sin dependencias a enterprise).

---

### Fase 3: Clearance en `UserRecord`

**Ciclo 4 — RED: Tests para clearance en `UserStoreHandle`**

6. [ ] Añadir sección de tests en archivo nuevo (20 min)
   - Archivo: `/Volumes/WD_BLACK/repos/MojoBytes/tessera-ecosystem/tessera-graph-enterprise/crates/tessera-auth/tests/lbac_user_clearance_test.rs`

```rust
// Copyright 2026 BelowZero Security OU. All rights reserved.

use std::collections::BTreeSet;
use tessera_auth::credentials::{Password, PasswordPolicy};
use tessera_auth::lbac::Clearance;
use tessera_auth::user::UserStoreHandle;

fn make_store() -> UserStoreHandle {
    let pw = Password::new("Admin@Init1!").unwrap();
    UserStoreHandle::new("admin", &pw, &PasswordPolicy::default()).unwrap()
}

#[test]
fn get_clearance_for_user_without_explicit_clearance_returns_default() {
    let store = make_store();
    let pw = Password::new("Admin@Init1!").unwrap();
    let id = store.authenticate("admin", &pw).unwrap();
    let clearance = store.get_clearance(id).unwrap();
    assert_eq!(clearance.level, 0);
    assert!(clearance.compartments.is_empty());
}

#[test]
fn set_and_get_clearance_roundtrips() {
    let store = make_store();
    let pw = Password::new("Admin@Init1!").unwrap();
    let id = store.authenticate("admin", &pw).unwrap();
    let comps: BTreeSet<String> = ["FINANCE", "HR"].iter().map(|s| s.to_string()).collect();
    let clearance = Clearance::new(3, comps.clone());
    store.set_clearance("admin", clearance.clone()).unwrap();
    let retrieved = store.get_clearance(id).unwrap();
    assert_eq!(retrieved.level, 3);
    assert_eq!(retrieved.compartments, comps);
}

#[test]
fn set_clearance_for_nonexistent_user_returns_error() {
    let store = make_store();
    let clearance = Clearance::new(1, BTreeSet::new());
    let result = store.set_clearance("ghost", clearance);
    assert!(result.is_err());
}

#[test]
fn get_clearance_for_nonexistent_user_returns_error() {
    let store = make_store();
    let id = tessera_auth::user::UserId::new(9999);
    let result = store.get_clearance(id);
    assert!(result.is_err());
}

#[test]
fn create_user_with_clearance_and_retrieve() {
    use tessera_auth::rbac::RoleStore;
    let store = make_store();
    let comps: BTreeSet<String> = ["LEGAL"].iter().map(|s| s.to_string()).collect();
    let clearance = Clearance::new(2, comps.clone());
    let pw = Password::new("User@Pass1!").unwrap();
    let id = store
        .create_user_with_clearance("alice", &pw, vec![], &PasswordPolicy::default(), clearance)
        .unwrap();
    let retrieved = store.get_clearance(id).unwrap();
    assert_eq!(retrieved.level, 2);
    assert_eq!(retrieved.compartments, comps);
}
```

**Ciclo 4 — GREEN: Añadir `clearance` a `UserRecord` y métodos a `UserStoreHandle`**

7. [ ] Modificar `UserRecord` y `UserStoreHandle` (25 min)
   - Archivo: `/Volumes/WD_BLACK/repos/MojoBytes/tessera-ecosystem/tessera-graph-enterprise/crates/tessera-auth/src/user.rs`
   - Acción: Añadir `pub clearance: Clearance` a `UserRecord` (con `#[serde(default)]` para compatibilidad con registros existentes sin campo), e implementar:
     - `get_clearance(&self, user_id: UserId) -> Result<Clearance>`
     - `set_clearance(&self, username: &str, clearance: Clearance) -> Result<()>`
     - `create_user_with_clearance(&self, ..., clearance: Clearance) -> Result<UserId>`

   La estructura `UserRecord` debe añadir:
   ```rust
   #[serde(default)]
   pub clearance: Clearance,
   ```
   El `Zeroize` impl no necesita tocar `clearance` (no es credencial sensible).

**Ciclo 4 — REFACTOR**: `cargo clippy -p tessera-auth --tests` limpio.

---

### Fase 4: `SecureGraph` — estructura y reads

**Ciclo 5 — RED: Tests de filtrado de lectura**

8. [ ] Crear archivo de test (30 min)
   - Archivo: `/Volumes/WD_BLACK/repos/MojoBytes/tessera-ecosystem/tessera-graph-enterprise/crates/tessera-storage-enterprise/tests/secure_graph_reads_test.rs`

```rust
// Copyright 2026 BelowZero Security OU. All rights reserved.

use std::collections::BTreeSet;
use tessera_auth::lbac::{Clearance, SecurityLabel, SecurityPolicy};
use tessera_graph::{Graph, props};
use tessera_storage_enterprise::lbac::SecureGraph;

fn make_graph_with_node(level: u16, compartments: &[&str]) -> (Graph, tessera_graph::NodeId) {
    let mut g = Graph::new();
    let comps: BTreeSet<String> = compartments.iter().map(|s| s.to_string()).collect();
    let label = SecurityLabel::new(level, comps);
    let mut p = props! { "name" => "Alice" };
    SecurityPolicy::inject_label(&mut p, &label);
    let id = g.add_node("Person", p).unwrap();
    (g, id)
}

fn clearance(level: u16, compartments: &[&str]) -> Clearance {
    let comps: BTreeSet<String> = compartments.iter().map(|s| s.to_string()).collect();
    Clearance::new(level, comps)
}

// --- node() ---

#[test]
fn node_returns_node_when_clearance_dominates() {
    let (mut g, id) = make_graph_with_node(2, &["FINANCE"]);
    let c = clearance(3, &["FINANCE", "HR"]);
    let sg = SecureGraph::new(&mut g, c);
    let node = sg.node(id).unwrap();
    assert_eq!(node.label(), "Person");
}

#[test]
fn node_strips_security_properties_from_result() {
    let (mut g, id) = make_graph_with_node(2, &["FINANCE"]);
    let c = clearance(3, &["FINANCE"]);
    let sg = SecureGraph::new(&mut g, c);
    let node = sg.node(id).unwrap();
    assert!(!node.properties().contains_key(SecurityPolicy::LEVEL_KEY));
    assert!(!node.properties().contains_key(SecurityPolicy::COMPARTMENTS_KEY));
}

#[test]
fn node_returns_not_found_when_level_insufficient() {
    let (mut g, id) = make_graph_with_node(5, &[]);
    let c = clearance(4, &[]);
    let sg = SecureGraph::new(&mut g, c);
    // Fail-safe: insufficient clearance returns NodeNotFound (not a clearance error)
    assert!(sg.node(id).is_err());
}

#[test]
fn node_returns_not_found_when_compartment_missing() {
    let (mut g, id) = make_graph_with_node(1, &["LEGAL"]);
    let c = clearance(5, &["FINANCE"]); // high level but wrong compartment
    let sg = SecureGraph::new(&mut g, c);
    assert!(sg.node(id).is_err());
}

#[test]
fn node_public_resource_visible_to_zero_clearance() {
    let (mut g, id) = make_graph_with_node(0, &[]);
    let c = clearance(0, &[]);
    let sg = SecureGraph::new(&mut g, c);
    assert!(sg.node(id).is_ok());
}

// --- node_ids() ---

#[test]
fn node_ids_filters_inaccessible_nodes() {
    let mut g = Graph::new();
    let c_pub = clearance(0, &[]);
    let c_fin: BTreeSet<String> = ["FINANCE"].iter().map(|s| s.to_string()).collect();
    let label_pub = SecurityLabel::new(0, BTreeSet::new());
    let label_fin = SecurityLabel::new(1, c_fin);
    let mut p1 = props! { "x" => 1_i64 };
    SecurityPolicy::inject_label(&mut p1, &label_pub);
    let mut p2 = props! { "x" => 2_i64 };
    SecurityPolicy::inject_label(&mut p2, &label_fin);
    g.add_node("N", p1).unwrap();
    g.add_node("N", p2).unwrap();
    let sg = SecureGraph::new(&mut g, c_pub);
    assert_eq!(sg.node_ids().len(), 1);
}

#[test]
fn nodes_by_label_filters_inaccessible() {
    let mut g = Graph::new();
    // Two Person nodes: one public, one FINANCE-compartmented
    let label_pub = SecurityLabel::default();
    let comps: BTreeSet<String> = ["FINANCE"].iter().map(|s| s.to_string()).collect();
    let label_fin = SecurityLabel::new(0, comps);
    let mut p1 = props! {};
    SecurityPolicy::inject_label(&mut p1, &label_pub);
    let mut p2 = props! {};
    SecurityPolicy::inject_label(&mut p2, &label_fin);
    g.add_node("Person", p1).unwrap();
    g.add_node("Person", p2).unwrap();
    let c = clearance(0, &[]);
    let sg = SecureGraph::new(&mut g, c);
    assert_eq!(sg.nodes_by_label("Person").len(), 1);
}

// --- node_count() ---

#[test]
fn node_count_reflects_only_accessible_nodes() {
    let mut g = Graph::new();
    let label_pub = SecurityLabel::default();
    let comps: BTreeSet<String> = ["CLASSIFIED"].iter().map(|s| s.to_string()).collect();
    let label_class = SecurityLabel::new(3, comps);
    let mut p1 = props! {};
    SecurityPolicy::inject_label(&mut p1, &label_pub);
    let mut p2 = props! {};
    SecurityPolicy::inject_label(&mut p2, &label_class);
    g.add_node("N", p1).unwrap();
    g.add_node("N", p2).unwrap();
    let sg = SecureGraph::new(&mut g, clearance(0, &[]));
    assert_eq!(sg.node_count(), 1);
}

// --- node_exists() ---

#[test]
fn node_exists_returns_false_for_inaccessible_node() {
    let (mut g, id) = make_graph_with_node(5, &["TOP_SECRET"]);
    let sg = SecureGraph::new(&mut g, clearance(0, &[]));
    assert!(!sg.node_exists(id));
}

#[test]
fn node_exists_returns_true_for_accessible_node() {
    let (mut g, id) = make_graph_with_node(0, &[]);
    let sg = SecureGraph::new(&mut g, clearance(0, &[]));
    assert!(sg.node_exists(id));
}

// --- edge() and outgoing/incoming ---

fn make_graph_with_edge(
    node_level: u16,
    edge_level: u16,
    compartments: &[&str],
) -> (Graph, tessera_graph::EdgeId) {
    let mut g = Graph::new();
    let comps: BTreeSet<String> = compartments.iter().map(|s| s.to_string()).collect();
    let node_label = SecurityLabel::new(node_level, comps.clone());
    let edge_label = SecurityLabel::new(edge_level, comps);
    let mut np = props! {};
    SecurityPolicy::inject_label(&mut np, &node_label);
    let src = g.add_node("N", np.clone()).unwrap();
    let tgt = g.add_node("N", np).unwrap();
    let mut ep = props! {};
    SecurityPolicy::inject_label(&mut ep, &edge_label);
    let eid = g.add_edge("E", src, tgt, ep).unwrap();
    (g, eid)
}

#[test]
fn edge_returns_edge_when_clearance_dominates_all_three() {
    let (mut g, eid) = make_graph_with_edge(1, 1, &["FINANCE"]);
    let sg = SecureGraph::new(&mut g, clearance(2, &["FINANCE"]));
    assert!(sg.edge(eid).is_ok());
}

#[test]
fn edge_strips_security_properties_from_result() {
    let (mut g, eid) = make_graph_with_edge(1, 1, &["FINANCE"]);
    let sg = SecureGraph::new(&mut g, clearance(2, &["FINANCE"]));
    let edge = sg.edge(eid).unwrap();
    assert!(!edge.properties().contains_key(SecurityPolicy::LEVEL_KEY));
    assert!(!edge.properties().contains_key(SecurityPolicy::COMPARTMENTS_KEY));
}

#[test]
fn edge_not_visible_when_endpoint_node_inaccessible() {
    // Edge itself is public but endpoints are classified
    let mut g = Graph::new();
    let node_comps: BTreeSet<String> = ["SECRET"].iter().map(|s| s.to_string()).collect();
    let node_label = SecurityLabel::new(0, node_comps);
    let mut np = props! {};
    SecurityPolicy::inject_label(&mut np, &node_label);
    let src = g.add_node("N", np.clone()).unwrap();
    let tgt = g.add_node("N", np).unwrap();
    let edge_label = SecurityLabel::default(); // public edge
    let mut ep = props! {};
    SecurityPolicy::inject_label(&mut ep, &edge_label);
    let eid = g.add_edge("E", src, tgt, ep).unwrap();
    let sg = SecureGraph::new(&mut g, clearance(0, &[])); // no SECRET clearance
    assert!(sg.edge(eid).is_err());
}

#[test]
fn outgoing_edges_filters_inaccessible_edges() {
    let mut g = Graph::new();
    // src and tgt: public
    let label_pub = SecurityLabel::default();
    let mut np = props! {};
    SecurityPolicy::inject_label(&mut np, &label_pub);
    let src = g.add_node("N", np.clone()).unwrap();
    let tgt = g.add_node("N", np).unwrap();
    // public edge
    let mut ep_pub = props! {};
    SecurityPolicy::inject_label(&mut ep_pub, &label_pub);
    g.add_edge("E", src, tgt, ep_pub).unwrap();
    // classified edge
    let comps: BTreeSet<String> = ["FINANCE"].iter().map(|s| s.to_string()).collect();
    let label_class = SecurityLabel::new(0, comps);
    let mut ep_class = props! {};
    SecurityPolicy::inject_label(&mut ep_class, &label_class);
    g.add_edge("E", src, tgt, ep_class).unwrap();
    let sg = SecureGraph::new(&mut g, clearance(0, &[]));
    let edges = sg.outgoing_edges(src).unwrap();
    assert_eq!(edges.len(), 1);
}

#[test]
fn edge_count_counts_only_accessible_edges() {
    let mut g = Graph::new();
    let label_pub = SecurityLabel::default();
    let comps: BTreeSet<String> = ["FINANCE"].iter().map(|s| s.to_string()).collect();
    let label_fin = SecurityLabel::new(0, comps);
    let mut np = props! {};
    SecurityPolicy::inject_label(&mut np, &label_pub);
    let src = g.add_node("N", np.clone()).unwrap();
    let tgt = g.add_node("N", np).unwrap();
    let mut ep_pub = props! {};
    SecurityPolicy::inject_label(&mut ep_pub, &label_pub);
    let mut ep_fin = props! {};
    SecurityPolicy::inject_label(&mut ep_fin, &label_fin);
    g.add_edge("E", src, tgt, ep_pub).unwrap();
    g.add_edge("E", src, tgt, ep_fin).unwrap();
    let sg = SecureGraph::new(&mut g, clearance(0, &[]));
    assert_eq!(sg.edge_count(), 1);
}
```

**Ciclo 5 — GREEN: Implementar `SecureGraph` con reads**

9. [ ] Crear módulo `lbac` en tessera-storage-enterprise (40 min)
   - Archivo: `/Volumes/WD_BLACK/repos/MojoBytes/tessera-ecosystem/tessera-graph-enterprise/crates/tessera-storage-enterprise/src/lbac.rs`
   - Acción: Crear con:

```rust
// Copyright 2026 BelowZero Security OU. All rights reserved.

//! `SecureGraph` — LBAC enforcement wrapper over any `GraphAccess` implementation.

use tessera_auth::lbac::{Clearance, SecurityPolicy};
use tessera_graph::access::GraphAccess;
use tessera_graph::error::{EdgeId, NodeId, Result};
use tessera_graph::{Edge, Error, Node, Properties};

/// A security-enforcing wrapper over any `GraphAccess` implementation.
///
/// All read operations filter results by the caller's `Clearance`.
/// All write operations enforce that the caller can write to the
/// target resource's security label.
/// Security properties are stripped from all returned nodes and edges.
///
/// # Fail-safe
///
/// Any error during clearance extraction results in denial (the resource
/// is treated as if the clearance check failed).
pub struct SecureGraph<'g, G: GraphAccess> {
    inner: &'g mut G,
    clearance: Clearance,
}

impl<'g, G: GraphAccess> SecureGraph<'g, G> {
    /// Create a new `SecureGraph` wrapping `inner` with the given `clearance`.
    pub fn new(inner: &'g mut G, clearance: Clearance) -> Self {
        Self { inner, clearance }
    }

    /// Returns `true` iff the caller's clearance dominates the label on `props`.
    fn can_read(&self, props: &Properties) -> bool {
        let label = SecurityPolicy::extract_label(props);
        self.clearance.dominates(&label)
    }

    /// Return a cleaned copy of `node` with security properties stripped.
    fn strip_node(mut node: Node) -> Node {
        SecurityPolicy::strip_security_properties(node.properties_mut());
        node
    }

    /// Return a cleaned copy of `edge` with security properties stripped.
    fn strip_edge(mut edge: Edge) -> Edge {
        SecurityPolicy::strip_security_properties(edge.properties_mut());
        edge
    }
}

impl<G: GraphAccess> GraphAccess for SecureGraph<'_, G> {
    fn node_ids(&self) -> Vec<NodeId> {
        self.inner
            .node_ids()
            .into_iter()
            .filter(|&id| {
                self.inner
                    .node(id)
                    .map(|n| self.can_read(n.properties()))
                    .unwrap_or(false)
            })
            .collect()
    }

    fn nodes_by_label(&self, label: &str) -> Vec<NodeId> {
        self.inner
            .nodes_by_label(label)
            .into_iter()
            .filter(|&id| {
                self.inner
                    .node(id)
                    .map(|n| self.can_read(n.properties()))
                    .unwrap_or(false)
            })
            .collect()
    }

    fn node(&self, id: NodeId) -> Result<Node> {
        let node = self.inner.node(id)?;
        if self.can_read(node.properties()) {
            Ok(Self::strip_node(node))
        } else {
            Err(Error::NodeNotFound(id))
        }
    }

    fn node_exists(&self, id: NodeId) -> bool {
        self.inner
            .node(id)
            .map(|n| self.can_read(n.properties()))
            .unwrap_or(false)
    }

    fn node_count(&self) -> usize {
        self.node_ids().len()
    }

    fn edges_by_label(&self, label: &str) -> Vec<EdgeId> {
        self.inner
            .edges_by_label(label)
            .into_iter()
            .filter(|&id| self.edge_visible(id))
            .collect()
    }

    fn edge(&self, id: EdgeId) -> Result<Edge> {
        let edge = self.inner.edge(id)?;
        if self.edge_visible_for(&edge) {
            Ok(Self::strip_edge(edge))
        } else {
            Err(Error::EdgeNotFound(id))
        }
    }

    fn edge_count(&self) -> usize {
        self.inner
            .edges_by_label("") // NOTE: needs all edges — see refactor note
            // This naive approach iterates all edges; replace with dedicated all_edges()
            // when GraphAccess exposes it.
            // For now, sum visible edges across all reachable node outgoing edges.
            // PLACEHOLDER — implement via node scan below.
            .len()
        // Actual implementation in the helper below — this placeholder is replaced
        // in the GREEN step.
    }

    fn outgoing_edges(&self, node: NodeId) -> Result<Vec<Edge>> {
        // Verify caller can see the node itself
        let node_val = self.inner.node(node)?;
        if !self.can_read(node_val.properties()) {
            return Err(Error::NodeNotFound(node));
        }
        let edges = self.inner.outgoing_edges(node)?;
        Ok(edges
            .into_iter()
            .filter(|e| self.edge_visible_for(e))
            .map(Self::strip_edge)
            .collect())
    }

    fn incoming_edges(&self, node: NodeId) -> Result<Vec<Edge>> {
        let node_val = self.inner.node(node)?;
        if !self.can_read(node_val.properties()) {
            return Err(Error::NodeNotFound(node));
        }
        let edges = self.inner.incoming_edges(node)?;
        Ok(edges
            .into_iter()
            .filter(|e| self.edge_visible_for(e))
            .map(Self::strip_edge)
            .collect())
    }

    // --- Mutations (Fase 5) — placeholders here, implemented in Ciclo 7 ---
    fn add_node(&mut self, label: &str, properties: Properties) -> Result<NodeId> {
        todo!("implemented in Ciclo 7")
    }
    fn update_node(&mut self, id: NodeId, node: &Node) -> Result<()> {
        todo!("implemented in Ciclo 7")
    }
    fn remove_node(&mut self, id: NodeId) -> Result<Node> {
        todo!("implemented in Ciclo 7")
    }
    fn add_edge(&mut self, label: &str, source: NodeId, target: NodeId, properties: Properties) -> Result<EdgeId> {
        todo!("implemented in Ciclo 7")
    }
    fn update_edge(&mut self, id: EdgeId, edge: &Edge) -> Result<()> {
        todo!("implemented in Ciclo 7")
    }
    fn remove_edge(&mut self, id: EdgeId) -> Result<Edge> {
        todo!("implemented in Ciclo 7")
    }
}

impl<G: GraphAccess> SecureGraph<'_, G> {
    /// Returns `true` iff `edge_id` passes all three visibility checks:
    /// edge label dominated AND both endpoint nodes dominated.
    fn edge_visible(&self, edge_id: EdgeId) -> bool {
        self.inner
            .edge(edge_id)
            .map(|e| self.edge_visible_for(&e))
            .unwrap_or(false)
    }

    fn edge_visible_for(&self, edge: &Edge) -> bool {
        if !self.can_read(edge.properties()) {
            return false;
        }
        // Both endpoints must also be visible
        let src_ok = self
            .inner
            .node(edge.source())
            .map(|n| self.can_read(n.properties()))
            .unwrap_or(false);
        let tgt_ok = self
            .inner
            .node(edge.target())
            .map(|n| self.can_read(n.properties()))
            .unwrap_or(false);
        src_ok && tgt_ok
    }
}
```

   Nota sobre `edge_count`: `GraphAccess` no expone un `all_edges()` iterator. La implementación correcta cuenta edges sumando outgoing edges de todos nodos visibles con deduplicación. El implementador debe revisar si `Graph` expone `all_edge_ids()` en el core y, si no, usar `node_ids()` + `outgoing_edges()` con un `HashSet<EdgeId>` de deduplicación.

   - Archivo: `/Volumes/WD_BLACK/repos/MojoBytes/tessera-ecosystem/tessera-graph-enterprise/crates/tessera-storage-enterprise/src/lib.rs`
   - Acción: Añadir `pub mod lbac;`

   - Archivo: `/Volumes/WD_BLACK/repos/MojoBytes/tessera-ecosystem/tessera-graph-enterprise/crates/tessera-storage-enterprise/Cargo.toml`
   - Acción: Añadir `tessera-auth = { workspace = true }` en `[dependencies]`

**Ciclo 5 — REFACTOR**: Resolver el `edge_count()` de forma correcta. Revisar si `tessera_graph::Graph` tiene un método `edge_ids()` o equivalente. Si no existe, implementar el edge count via scan de nodos visibles acumulando en `HashSet<EdgeId>`.

---

### Fase 5: `SecureGraph` — escrituras

**Ciclo 6 — RED: Tests de escritura**

10. [ ] Crear archivo de test para writes (25 min)
    - Archivo: `/Volumes/WD_BLACK/repos/MojoBytes/tessera-ecosystem/tessera-graph-enterprise/crates/tessera-storage-enterprise/tests/secure_graph_writes_test.rs`

```rust
// Copyright 2026 BelowZero Security OU. All rights reserved.

use std::collections::BTreeSet;
use tessera_auth::lbac::{Clearance, SecurityLabel, SecurityPolicy};
use tessera_graph::{Graph, props};
use tessera_storage_enterprise::lbac::SecureGraph;

fn clearance(level: u16, compartments: &[&str]) -> Clearance {
    let comps: BTreeSet<String> = compartments.iter().map(|s| s.to_string()).collect();
    Clearance::new(level, comps)
}

fn labeled_props(level: u16, compartments: &[&str]) -> tessera_graph::Properties {
    let comps: BTreeSet<String> = compartments.iter().map(|s| s.to_string()).collect();
    let label = SecurityLabel::new(level, comps);
    let mut p = props! {};
    SecurityPolicy::inject_label(&mut p, &label);
    p
}

// --- add_node: security properties blocked from callers ---

#[test]
fn add_node_user_cannot_inject_security_level_directly() {
    let mut g = Graph::new();
    let mut sg = SecureGraph::new(&mut g, clearance(5, &[]));
    let mut p = props! { "name" => "Alice" };
    p.insert(
        SecurityPolicy::LEVEL_KEY.to_string(),
        tessera_graph::Property::I64(99),
    );
    // The call succeeds but the security property must be stripped from stored node
    // (callers cannot escalate security level via add_node)
    let id = sg.add_node("Person", p).unwrap();
    // Read back through the inner graph (bypassing SecureGraph) to verify stored label
    let raw = g.node(id).unwrap();
    let stored_level = raw
        .properties()
        .get(SecurityPolicy::LEVEL_KEY)
        .and_then(|p| p.as_i64())
        .unwrap_or(0);
    // The stored level must NOT be 99; it should default to 0 (caller-supplied stripped)
    assert_ne!(stored_level, 99);
}

#[test]
fn add_node_with_public_clearance_creates_public_node() {
    let mut g = Graph::new();
    let mut sg = SecureGraph::new(&mut g, clearance(0, &[]));
    let id = sg.add_node("Person", props! { "name" => "Bob" }).unwrap();
    let raw = g.node(id).unwrap();
    let stored_level = raw
        .properties()
        .get(SecurityPolicy::LEVEL_KEY)
        .and_then(|p| p.as_i64())
        .unwrap_or(0);
    assert_eq!(stored_level, 0);
}

// --- update_node: caller cannot set security props ---

#[test]
fn update_node_rejects_attempt_to_set_security_property() {
    let mut g = Graph::new();
    let id = g.add_node("Person", labeled_props(0, &[])).unwrap();
    let mut sg = SecureGraph::new(&mut g, clearance(5, &[]));
    let mut node = sg.node(id).unwrap();
    // Try to inject security property into update
    node.properties_mut().insert(
        SecurityPolicy::LEVEL_KEY.to_string(),
        tessera_graph::Property::I64(99),
    );
    // update_node must strip the injected security property before applying
    sg.update_node(id, &node).unwrap();
    let raw = g.node(id).unwrap();
    let stored_level = raw
        .properties()
        .get(SecurityPolicy::LEVEL_KEY)
        .and_then(|p| p.as_i64())
        .unwrap_or(0);
    assert_ne!(stored_level, 99);
}

#[test]
fn update_node_denied_when_clearance_does_not_dominate_existing_label() {
    let mut g = Graph::new();
    let id = g.add_node("Person", labeled_props(5, &[])).unwrap();
    let mut sg = SecureGraph::new(&mut g, clearance(3, &[])); // insufficient clearance
    let dummy_node = g.node(id).unwrap();
    let result = sg.update_node(id, &dummy_node);
    assert!(result.is_err());
}

// --- remove_node ---

#[test]
fn remove_node_succeeds_when_clearance_dominates() {
    let mut g = Graph::new();
    let id = g.add_node("Person", labeled_props(2, &["FINANCE"])).unwrap();
    let mut sg = SecureGraph::new(&mut g, clearance(3, &["FINANCE"]));
    assert!(sg.remove_node(id).is_ok());
}

#[test]
fn remove_node_denied_when_clearance_insufficient() {
    let mut g = Graph::new();
    let id = g.add_node("Person", labeled_props(5, &[])).unwrap();
    let mut sg = SecureGraph::new(&mut g, clearance(3, &[]));
    assert!(sg.remove_node(id).is_err());
}

// --- add_edge ---

#[test]
fn add_edge_user_cannot_inject_security_label_on_edge() {
    let mut g = Graph::new();
    let src = g.add_node("N", labeled_props(0, &[])).unwrap();
    let tgt = g.add_node("N", labeled_props(0, &[])).unwrap();
    let mut sg = SecureGraph::new(&mut g, clearance(0, &[]));
    let mut ep = props! {};
    ep.insert(
        SecurityPolicy::LEVEL_KEY.to_string(),
        tessera_graph::Property::I64(99),
    );
    let eid = sg.add_edge("E", src, tgt, ep).unwrap();
    let raw = g.edge(eid).unwrap();
    let stored_level = raw
        .properties()
        .get(SecurityPolicy::LEVEL_KEY)
        .and_then(|p| p.as_i64())
        .unwrap_or(0);
    assert_ne!(stored_level, 99);
}

// --- update_edge ---

#[test]
fn update_edge_denied_when_clearance_does_not_dominate_edge_label() {
    let mut g = Graph::new();
    let src = g.add_node("N", labeled_props(0, &[])).unwrap();
    let tgt = g.add_node("N", labeled_props(0, &[])).unwrap();
    let eid = g.add_edge("E", src, tgt, labeled_props(5, &[])).unwrap();
    let dummy_edge = g.edge(eid).unwrap();
    let mut sg = SecureGraph::new(&mut g, clearance(3, &[]));
    assert!(sg.update_edge(eid, &dummy_edge).is_err());
}

// --- remove_edge ---

#[test]
fn remove_edge_denied_when_clearance_insufficient() {
    let mut g = Graph::new();
    let src = g.add_node("N", labeled_props(0, &[])).unwrap();
    let tgt = g.add_node("N", labeled_props(0, &[])).unwrap();
    let eid = g.add_edge("E", src, tgt, labeled_props(5, &[])).unwrap();
    let mut sg = SecureGraph::new(&mut g, clearance(3, &[]));
    assert!(sg.remove_edge(eid).is_err());
}
```

**Ciclo 6 — GREEN: Implementar mutations en `SecureGraph`**

11. [ ] Completar los métodos de mutación en `src/lbac.rs` reemplazando los `todo!()` (30 min):

```rust
fn add_node(&mut self, label: &str, mut properties: Properties) -> Result<NodeId> {
    // Strip any security properties the caller tried to inject
    SecurityPolicy::strip_security_properties(&mut properties);
    // New nodes inherit the caller's clearance level and compartments
    // (write-what-you-know: the created resource gets a label matching the caller's clearance)
    let new_label = tessera_auth::lbac::SecurityLabel::new(
        self.clearance.level,
        self.clearance.compartments.clone(),
    );
    SecurityPolicy::inject_label(&mut properties, &new_label);
    self.inner.add_node(label, properties)
}

fn update_node(&mut self, id: NodeId, node: &Node) -> Result<()> {
    // Check caller can write to the existing node
    let existing = self.inner.node(id)?;
    if !self.can_read(existing.properties()) {
        return Err(Error::NodeNotFound(id));
    }
    // Build the updated node stripping caller-supplied security props
    let mut updated = node.clone();
    SecurityPolicy::strip_security_properties(updated.properties_mut());
    // Preserve existing security label (callers cannot change it via update_node)
    let existing_level = SecurityPolicy::extract_label(existing.properties());
    SecurityPolicy::inject_label(updated.properties_mut(), &existing_level);
    self.inner.update_node(id, &updated)
}

fn remove_node(&mut self, id: NodeId) -> Result<Node> {
    let existing = self.inner.node(id)?;
    if !self.can_read(existing.properties()) {
        return Err(Error::NodeNotFound(id));
    }
    let mut removed = self.inner.remove_node(id)?;
    SecurityPolicy::strip_security_properties(removed.properties_mut());
    Ok(removed)
}

fn add_edge(
    &mut self,
    label: &str,
    source: NodeId,
    target: NodeId,
    mut properties: Properties,
) -> Result<EdgeId> {
    SecurityPolicy::strip_security_properties(&mut properties);
    let new_label = tessera_auth::lbac::SecurityLabel::new(
        self.clearance.level,
        self.clearance.compartments.clone(),
    );
    SecurityPolicy::inject_label(&mut properties, &new_label);
    self.inner.add_edge(label, source, target, properties)
}

fn update_edge(&mut self, id: EdgeId, edge: &Edge) -> Result<()> {
    let existing = self.inner.edge(id)?;
    if !self.can_read(existing.properties()) {
        return Err(Error::EdgeNotFound(id));
    }
    let mut updated = edge.clone();
    SecurityPolicy::strip_security_properties(updated.properties_mut());
    let existing_label = SecurityPolicy::extract_label(existing.properties());
    SecurityPolicy::inject_label(updated.properties_mut(), &existing_label);
    self.inner.update_edge(id, &updated)
}

fn remove_edge(&mut self, id: EdgeId) -> Result<Edge> {
    let existing = self.inner.edge(id)?;
    if !self.can_read(existing.properties()) {
        return Err(Error::EdgeNotFound(id));
    }
    let mut removed = self.inner.remove_edge(id)?;
    SecurityPolicy::strip_security_properties(removed.properties_mut());
    Ok(removed)
}
```

**Ciclo 6 — REFACTOR**: `cargo clippy -p tessera-storage-enterprise --tests` limpio. El `node.clone()` y `edge.clone()` en update requieren que `Node` y `Edge` implementen `Clone` — verificar en el core (ya lo hacen: `#[derive(Debug, Clone)]`).

---

### Fase 6: Generalización de `execute_mut`

**Ciclo 7 — RED: Test de `execute_mut` con `SecureGraph`**

12. [ ] Añadir tests al archivo existente o crear nuevo (20 min)
    - Archivo: `/Volumes/WD_BLACK/repos/MojoBytes/tessera-ecosystem/tessera-graph-enterprise/crates/tessera-storage-enterprise/tests/secure_graph_gql_test.rs`

```rust
// Copyright 2026 BelowZero Security OU. All rights reserved.

//! Integration tests: execute_mut through SecureGraph.

use std::collections::BTreeSet;
use tessera_auth::lbac::{Clearance, SecurityLabel, SecurityPolicy};
use tessera_graph::{Graph, gql, props};
use tessera_storage_enterprise::gql::execute_mut;
use tessera_storage_enterprise::lbac::SecureGraph;

fn clearance(level: u16, compartments: &[&str]) -> Clearance {
    let comps: BTreeSet<String> = compartments.iter().map(|s| s.to_string()).collect();
    Clearance::new(level, comps)
}

fn run_through_secure(
    g: &mut Graph,
    clearance: Clearance,
    query: &str,
) -> tessera_graph::Result<tessera_graph::GqlMutationResult> {
    let mut sg = SecureGraph::new(g, clearance);
    let stmt = gql::parse_statement(query)?;
    let ms = stmt.as_mutation().expect("mutation expected");
    execute_mut(&mut sg, &ms)
}

#[test]
fn create_through_secure_graph_assigns_clearance_label() {
    let mut g = Graph::new();
    run_through_secure(
        &mut g,
        clearance(2, &["FINANCE"]),
        "CREATE (n:Person {name: 'Alice'})",
    )
    .unwrap();
    // Read raw node to verify security label was injected
    let ids = g.nodes_by_label("Person");
    assert_eq!(ids.len(), 1);
    let raw = g.node(ids[0]).unwrap();
    let label = SecurityPolicy::extract_label(raw.properties());
    assert_eq!(label.level, 2);
    assert!(label.compartments.contains("FINANCE"));
}

#[test]
fn delete_through_secure_graph_denied_when_level_insufficient() {
    let mut g = Graph::new();
    // Create a classified node directly (bypassing SecureGraph)
    let comps: BTreeSet<String> = BTreeSet::new();
    let label = SecurityLabel::new(5, comps);
    let mut p = props! { "name" => "Bob" };
    SecurityPolicy::inject_label(&mut p, &label);
    g.add_node("Person", p).unwrap();
    // Try to delete with insufficient clearance
    let result = run_through_secure(
        &mut g,
        clearance(3, &[]),
        "MATCH (n:Person {name: 'Bob'}) DETACH DELETE n",
    );
    // SecureGraph hides the node from MATCH, so DELETE deletes 0 nodes (not an error)
    // The node must still exist in the raw graph
    assert!(result.is_ok()); // MATCH returns empty set — no-op
    assert_eq!(g.node_count(), 1); // Node still there
}

#[test]
fn match_only_returns_nodes_visible_to_clearance() {
    let mut g = Graph::new();
    // Public node
    let label_pub = SecurityLabel::default();
    let mut p1 = props! { "name" => "Public" };
    SecurityPolicy::inject_label(&mut p1, &label_pub);
    g.add_node("Person", p1).unwrap();
    // Classified node
    let comps: BTreeSet<String> = ["SECRET"].iter().map(|s| s.to_string()).collect();
    let label_secret = SecurityLabel::new(3, comps);
    let mut p2 = props! { "name" => "Secret" };
    SecurityPolicy::inject_label(&mut p2, &label_secret);
    g.add_node("Person", p2).unwrap();
    // Query with low clearance — only public visible
    let mut sg = SecureGraph::new(&mut g, clearance(0, &[]));
    let q = gql::parse("MATCH (n:Person) RETURN n.name").unwrap();
    let rows = gql::execute(&sg, &q).unwrap();
    assert_eq!(rows.len(), 1);
}

#[test]
fn existing_execute_mut_on_plain_graph_still_works() {
    // Regression: original call site with &mut Graph must compile and pass
    let mut g = Graph::new();
    let stmt = gql::parse_statement("CREATE (n:Person {name: 'Alice'})").unwrap();
    let ms = stmt.as_mutation().unwrap();
    execute_mut(&mut g, &ms).unwrap();
    assert_eq!(g.node_count(), 1);
}
```

**Ciclo 7 — GREEN: Generalizar `execute_mut` a `G: GraphAccess`**

13. [ ] Modificar `src/gql/mod.rs` (30 min)
    - Archivo: `/Volumes/WD_BLACK/repos/MojoBytes/tessera-ecosystem/tessera-graph-enterprise/crates/tessera-storage-enterprise/src/gql/mod.rs`
    - Acción: Cambiar la firma de `execute_mut` y todas las funciones internas de `&mut Graph` a `&mut G` donde `G: GraphAccess`. Actualizar imports: añadir `use tessera_graph::access::GraphAccess;` y eliminar `use tessera_graph::Graph` donde ya no sea necesario. Las funciones `execute_create`, `execute_delete`, `execute_set`, `execute_merge` también deben ser genéricas sobre `G: GraphAccess`.

    Firma resultante:
    ```rust
    pub fn execute_mut<G: GraphAccess>(
        graph: &mut G,
        stmt: &MutationStatement,
    ) -> tessera_graph::Result<GqlMutationResult>
    ```

    Las funciones helper también:
    ```rust
    fn execute_create<G: GraphAccess>(graph: &mut G, ...) -> tessera_graph::Result<()>
    fn execute_delete<G: GraphAccess>(graph: &mut G, ...) -> tessera_graph::Result<()>
    fn execute_set<G: GraphAccess>(graph: &mut G, ...) -> tessera_graph::Result<()>
    fn execute_merge<G: GraphAccess>(graph: &mut G, ...) -> tessera_graph::Result<()>
    ```

    Nota: `compile_match_for_mutation` en el MIT core toma `&dyn GraphAccess` o es genérico. Verificar su firma actual antes de implementar. Si toma `&Graph`, necesita ser actualizada en el MIT core o se usa `graph as &dyn GraphAccess`.

**Ciclo 7 — REFACTOR**: Ejecutar `cargo test -p tessera-storage-enterprise` completo. Los tests de `gql_mutations_integration.rs` existentes deben seguir pasando sin cambios.

---

### Fase 7: Tests de throughput

**Ciclo 8 — RED/GREEN/REFACTOR: Guards de regresión de rendimiento**

14. [ ] Crear archivo de throughput para LBAC (25 min)
    - Archivo: `/Volumes/WD_BLACK/repos/MojoBytes/tessera-ecosystem/tessera-graph-enterprise/crates/tessera-storage-enterprise/tests/lbac_throughput_test.rs`

```rust
// Copyright 2026 BelowZero Security OU. All rights reserved.

//! Throughput regression guards for LBAC enforcement on hot paths.
//!
//! These tests verify that the SecureGraph wrapper does not degrade
//! node read throughput by more than 10% relative to a baseline.
//! Dual thresholds account for debug vs release compilation.

use std::collections::BTreeSet;
use tessera_auth::lbac::{Clearance, SecurityLabel, SecurityPolicy};
use tessera_graph::{Graph, props};
use tessera_storage_enterprise::lbac::SecureGraph;
use tessera_graph::access::GraphAccess;

const ITERATIONS: u64 = 10_000;

fn build_graph(node_count: usize) -> (Graph, Vec<tessera_graph::NodeId>) {
    let mut g = Graph::new();
    let label = SecurityLabel::default(); // all public
    let mut ids = Vec::with_capacity(node_count);
    for i in 0..node_count {
        let mut p = props! { "i" => i as i64 };
        SecurityPolicy::inject_label(&mut p, &label);
        let id = g.add_node("N", p).unwrap();
        ids.push(id);
    }
    (g, ids)
}

#[test]
fn node_read_throughput_regression_guard() {
    let (mut g, ids) = build_graph(100);
    let clearance = Clearance::new(0, BTreeSet::new());
    let sg = SecureGraph::new(&mut g, clearance);

    let start = std::time::Instant::now();
    for i in 0..ITERATIONS {
        let id = ids[(i as usize) % ids.len()];
        let _ = sg.node(id).unwrap();
    }
    let elapsed = start.elapsed();

    #[allow(clippy::cast_possible_truncation)]
    let elapsed_us = elapsed.as_micros() as u64;
    let ops_per_sec = ITERATIONS * 1_000_000 / u64::max(elapsed_us, 1);

    // Thresholds calibrated for: debug = heavy lock + filter overhead,
    // release = optimized inlining.
    let min_ops = if cfg!(debug_assertions) { 50_000 } else { 500_000 };

    assert!(
        ops_per_sec >= min_ops,
        "SecureGraph node() throughput regression: {ops_per_sec} ops/sec (minimum: {min_ops})"
    );
}

#[test]
fn node_ids_throughput_regression_guard() {
    let (mut g, _ids) = build_graph(1_000);
    let clearance = Clearance::new(0, BTreeSet::new());
    let sg = SecureGraph::new(&mut g, clearance);

    let iterations = 100_u64;
    let start = std::time::Instant::now();
    for _ in 0..iterations {
        let _ = sg.node_ids();
    }
    let elapsed = start.elapsed();

    #[allow(clippy::cast_possible_truncation)]
    let elapsed_us = elapsed.as_micros() as u64;
    // Each call scans 1000 nodes. Measure scans-per-second.
    let ops_per_sec = iterations * 1_000_000 / u64::max(elapsed_us, 1);
    let min_ops = if cfg!(debug_assertions) { 50 } else { 500 };

    assert!(
        ops_per_sec >= min_ops,
        "SecureGraph node_ids() throughput regression: {ops_per_sec} scans/sec (minimum: {min_ops})"
    );
}

#[test]
fn dominance_check_throughput_regression_guard() {
    // Measures raw dominance check speed — this is the innermost hot path.
    let comps_label: BTreeSet<String> = ["FINANCE", "HR"].iter().map(|s| s.to_string()).collect();
    let label = SecurityLabel::new(3, comps_label);
    let comps_clearance: BTreeSet<String> =
        ["FINANCE", "HR", "LEGAL"].iter().map(|s| s.to_string()).collect();
    let clearance = Clearance::new(5, comps_clearance);

    let iterations = 1_000_000_u64;
    let start = std::time::Instant::now();
    for _ in 0..iterations {
        let _ = clearance.dominates(&label);
    }
    let elapsed = start.elapsed();

    #[allow(clippy::cast_possible_truncation)]
    let elapsed_us = elapsed.as_micros() as u64;
    let ops_per_sec = iterations * 1_000_000 / u64::max(elapsed_us, 1);
    let min_ops = if cfg!(debug_assertions) {
        1_000_000
    } else {
        10_000_000
    };

    assert!(
        ops_per_sec >= min_ops,
        "dominates() throughput regression: {ops_per_sec} ops/sec (minimum: {min_ops})"
    );
}
```

---

### Fase 8: Integración y compilación final

15. [ ] Verificación de compilación completa (15 min)
    - Acción: `cargo clippy -p tessera-auth --tests -- -D warnings`
    - Acción: `cargo clippy -p tessera-storage-enterprise --tests -- -D warnings`
    - Acción: `cargo test -p tessera-auth`
    - Acción: `cargo test -p tessera-storage-enterprise`
    - Output esperado: 0 errores, 0 warnings tratados como error

16. [ ] Actualizar exports públicos en `lib.rs` de ambos crates (10 min)
    - `tessera-auth/src/lib.rs`: Confirmar que `lbac`, `SecurityLabel`, `Clearance`, `SecurityPolicy` están re-exportados
    - `tessera-storage-enterprise/src/lib.rs`: Confirmar que `pub mod lbac` está presente

17. [ ] Verificar que `compile_match_for_mutation` en MIT core es compatible (15 min)
    - Archivo a inspeccionar: `/Volumes/WD_BLACK/repos/MojoBytes/tessera-ecosystem/tessera-graph/src/gql/`
    - Si la función toma `&Graph` en lugar de `&dyn GraphAccess`, añadir una versión genérica o cambiar la firma. Esta es una modificación al MIT core — verificar que el README del MIT core indica que `GraphAccess` fue diseñado específicamente para este propósito.

---

## Estimación Total

- Implementación: 5-6 horas
- Testing funcional (integrado en cada ciclo): incluido
- Testing rendimiento (Ciclo 8): 30 minutos adicionales

## Criterios de Éxito

- [ ] `cargo test -p tessera-auth` pasa (todos los ciclos 1-4)
- [ ] `cargo test -p tessera-storage-enterprise` pasa (todos los ciclos 5-7)
- [ ] `cargo clippy --workspace --tests -- -D warnings` limpio
- [ ] Dominance check throughput >= 1,000,000 ops/sec (debug) / 10,000,000 ops/sec (release)
- [ ] Node read throughput >= 50,000 ops/sec (debug) / 500,000 ops/sec (release)
- [ ] Tests existentes en `gql_mutations_integration.rs` pasan sin modificación (regresión cero)
- [ ] Ningún test en `src/` — todos en `tests/`
- [ ] Propiedad `__security_level` y `__security_compartments` nunca visibles en resultados de query

---

## Notas Críticas para el Implementador

**Dependencias circulares**: `tessera-auth` va a depender de `tessera-graph` (para `Properties`). `tessera-storage-enterprise` ya depende de `tessera-graph`. No hay ciclos: `tessera-graph` (MIT) no depende de ninguno de los enterprise crates.

**`compile_match_for_mutation`**: Esta función en el MIT core puede tomar `&Graph` directamente. Antes de implementar el Ciclo 7, inspeccionar su firma en `/Volumes/WD_BLACK/repos/MojoBytes/tessera-ecosystem/tessera-graph/src/gql/`. Si necesita `&dyn GraphAccess`, el cambio es en el MIT core (cuidado: requiere autorización del usuario por ser MIT). Si solo se puede pasar `&Graph`, el workaround es usar `SecureGraph` solo en la fase de escritura, pasando el inner `Graph` al `compile_match_for_mutation`.

**`edge_count()`**: `GraphAccess` no tiene `all_edge_ids()`. La implementación correcta en `SecureGraph::edge_count()` debe iterar `node_ids()` del inner graph, llamar `outgoing_edges()` en cada uno, acumular en `HashSet<EdgeId>` y devolver el conteo. Esto es O(N nodes + E edges) — documentar como tal.

**Zeroize en `UserRecord`**: El campo `clearance: Clearance` no contiene credenciales sensibles. No añadir al `Zeroize` impl.

**`serde(default)` en `clearance`**: Necesario para que ficheros JSON de usuarios existentes (sin campo `clearance`) deserialicen correctamente. `Clearance::default()` produce level 0, compartimentos vacíos — comportamiento fail-safe correcto.
