// SPDX-License-Identifier: MIT

//! Built-in CALL procedure registry.
//!
//! Maps `(namespace, procedure_name)` pairs to [`ProcedureKind`] values.
//! Supported namespaces: `ermya` (canonical) and `mg` (Memgraph alias,
//! for zero-friction migration of the pilot's existing queries).
//!
//! Adding a new procedure requires:
//! 1. a new [`ProcedureKind`] variant,
//! 2. a new arm in [`resolve_procedure`],
//! 3. a new arm in the server's call handler that reads the graph for it.

/// A resolved built-in procedure.
///
/// The call handler matches on this to select the correct graph read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcedureKind {
    /// `ermya.vertex_labels()` / `mg.vertex_labels()` — one row, one column
    /// (`vertex_labels`) carrying a list of all distinct node-label strings.
    VertexLabels,
    /// `ermya.edge_types()` / `mg.edge_types()` — one row, one column
    /// (`edge_types`) carrying a list of all distinct relationship-type strings.
    EdgeTypes,
    /// `ermya.snapshot(db, dest)` — admin-only online physical snapshot of a
    /// tenant. No `mg` alias (Memgraph has no equivalent). Dispatched by the
    /// server's handler against the registry, not by the sync call handler.
    Snapshot,
    /// `ermya.restore(db, snap)` — admin-only hot restore of a tenant from a
    /// snapshot. No `mg` alias. Dispatched against the registry like
    /// [`ProcedureKind::Snapshot`].
    Restore,
}

/// Resolves a `(namespace, procedure_name)` pair to a [`ProcedureKind`].
///
/// Returns `None` for an unknown namespace, an unknown procedure within a known
/// namespace, or a missing namespace — the caller surfaces a Bolt
/// `Neo.ClientError.Procedure.ProcedureNotFound` in that case.
///
/// # Supported procedures
///
/// | Namespace | Procedure       | Kind                            |
/// |-----------|-----------------|---------------------------------|
/// | `ermya` | `vertex_labels` | [`ProcedureKind::VertexLabels`] |
/// | `ermya` | `edge_types`    | [`ProcedureKind::EdgeTypes`]    |
/// | `mg`      | `vertex_labels` | [`ProcedureKind::VertexLabels`] |
/// | `mg`      | `edge_types`    | [`ProcedureKind::EdgeTypes`]    |
#[must_use]
pub fn resolve_procedure(namespace: Option<&str>, procedure: &str) -> Option<ProcedureKind> {
    match namespace? {
        // `ermya` exposes the introspection procedures AND the native admin
        // backup procedures (snapshot/restore — no Memgraph equivalent).
        "ermya" => match procedure {
            "vertex_labels" => Some(ProcedureKind::VertexLabels),
            "edge_types" => Some(ProcedureKind::EdgeTypes),
            "snapshot" => Some(ProcedureKind::Snapshot),
            "restore" => Some(ProcedureKind::Restore),
            _ => None,
        },
        // `mg` is the Memgraph-compatibility alias: only the procedures Memgraph
        // itself defines. snapshot/restore are ermya-native and deliberately
        // absent here.
        "mg" => match procedure {
            "vertex_labels" => Some(ProcedureKind::VertexLabels),
            "edge_types" => Some(ProcedureKind::EdgeTypes),
            _ => None,
        },
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_ermya_vertex_labels() {
        assert_eq!(
            resolve_procedure(Some("ermya"), "vertex_labels"),
            Some(ProcedureKind::VertexLabels)
        );
    }

    #[test]
    fn resolve_ermya_edge_types() {
        assert_eq!(
            resolve_procedure(Some("ermya"), "edge_types"),
            Some(ProcedureKind::EdgeTypes)
        );
    }

    #[test]
    fn resolve_mg_vertex_labels() {
        assert_eq!(
            resolve_procedure(Some("mg"), "vertex_labels"),
            Some(ProcedureKind::VertexLabels)
        );
    }

    #[test]
    fn resolve_mg_edge_types() {
        assert_eq!(
            resolve_procedure(Some("mg"), "edge_types"),
            Some(ProcedureKind::EdgeTypes)
        );
    }

    #[test]
    fn resolve_unknown_procedure_returns_none() {
        assert!(resolve_procedure(Some("ermya"), "unknown_proc").is_none());
    }

    #[test]
    fn resolve_unknown_namespace_returns_none() {
        assert!(resolve_procedure(Some("apoc"), "vertex_labels").is_none());
    }

    #[test]
    fn resolve_no_namespace_returns_none() {
        assert!(resolve_procedure(None, "vertex_labels").is_none());
    }

    #[test]
    fn procedure_kind_is_eq() {
        assert_eq!(ProcedureKind::VertexLabels, ProcedureKind::VertexLabels);
        assert_ne!(ProcedureKind::VertexLabels, ProcedureKind::EdgeTypes);
    }

    // --- Block 3 Feature B: snapshot/restore admin procedures (B-6) ---

    #[test]
    fn resolve_ermya_snapshot() {
        assert_eq!(
            resolve_procedure(Some("ermya"), "snapshot"),
            Some(ProcedureKind::Snapshot)
        );
    }

    #[test]
    fn resolve_ermya_restore() {
        assert_eq!(
            resolve_procedure(Some("ermya"), "restore"),
            Some(ProcedureKind::Restore)
        );
    }

    #[test]
    fn resolve_mg_snapshot_returns_none() {
        // snapshot/restore are ermya-native admin procedures; Memgraph has no
        // equivalent, so the `mg` alias must NOT resolve them.
        assert!(resolve_procedure(Some("mg"), "snapshot").is_none());
        assert!(resolve_procedure(Some("mg"), "restore").is_none());
    }
}
