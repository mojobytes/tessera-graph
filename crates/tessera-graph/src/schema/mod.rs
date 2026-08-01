// SPDX-License-Identifier: Apache-2.0

//! Per-database schema catalog: declared indexes and unique constraints.
//!
//! [`SchemaCatalog`] is loaded from `schema.bin` on `Graph::open` and
//! persisted (via [`codec::serialize`]) on every DDL mutation and `flush`.

pub mod codec;

use std::collections::HashSet;

/// A declared index entry: `(label, property key)`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct IndexDecl {
    pub label: String,
    pub prop: String,
}

/// A declared unique constraint: `(label, property key)`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ConstraintDecl {
    pub label: String,
    pub prop: String,
}

/// A label declared append-only: nodes of this label are written once and never
/// versioned, so reads skip MVCC visibility resolution even when the graph has
/// MVCC enabled (issue #43 Part A).
///
/// `since_node_id` is the node id the declaration started applying from — the
/// graph's next id at the moment it was issued. Only nodes with an id at or
/// above it are covered, which is what makes the declaration non-retroactive
/// across a restart as well as in-session (issue #61).
///
/// Equality and hashing are by label alone: a label is declared or it is not,
/// and re-declaring an already-declared label must not produce a second entry.
/// [`SchemaCatalog::mark_label_append_only`] therefore leaves the original
/// `since_node_id` in place, which is the point — the boundary belongs to the
/// declaration in force, not to the most recent statement that repeated it.
#[derive(Debug, Clone, Eq)]
pub struct AppendOnlyDecl {
    pub label: String,
    /// First node id covered by this declaration. Nodes below it predate it and
    /// never take the fast path.
    pub since_node_id: u64,
}

impl PartialEq for AppendOnlyDecl {
    fn eq(&self, other: &Self) -> bool {
        self.label == other.label
    }
}

impl std::hash::Hash for AppendOnlyDecl {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.label.hash(state);
    }
}

/// In-memory catalog of DDL-declared schema entries for one database.
#[derive(Debug, Default, Clone)]
pub struct SchemaCatalog {
    indexes: HashSet<IndexDecl>,
    constraints: HashSet<ConstraintDecl>,
    append_only_labels: HashSet<AppendOnlyDecl>,
}

impl SchemaCatalog {
    /// Creates an empty catalog.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Declares an index on `(label, prop)`. Idempotent.
    pub fn add_index(&mut self, label: &str, prop: &str) {
        self.indexes.insert(IndexDecl {
            label: label.to_owned(),
            prop: prop.to_owned(),
        });
    }

    /// Removes a declared index. No-op if not present.
    pub fn remove_index(&mut self, label: &str, prop: &str) {
        self.indexes.remove(&IndexDecl {
            label: label.to_owned(),
            prop: prop.to_owned(),
        });
    }

    /// Returns `true` if an index is declared on `(label, prop)`.
    #[must_use]
    pub fn has_index(&self, label: &str, prop: &str) -> bool {
        self.indexes.contains(&IndexDecl {
            label: label.to_owned(),
            prop: prop.to_owned(),
        })
    }

    /// Returns all declared index entries.
    #[must_use]
    pub fn indexes(&self) -> Vec<&IndexDecl> {
        self.indexes.iter().collect()
    }

    /// Declares a unique constraint on `(label, prop)`. Idempotent.
    pub fn add_unique_constraint(&mut self, label: &str, prop: &str) {
        self.constraints.insert(ConstraintDecl {
            label: label.to_owned(),
            prop: prop.to_owned(),
        });
    }

    /// Removes a unique constraint. No-op if not present.
    pub fn remove_unique_constraint(&mut self, label: &str, prop: &str) {
        self.constraints.remove(&ConstraintDecl {
            label: label.to_owned(),
            prop: prop.to_owned(),
        });
    }

    /// Returns `true` if a unique constraint is declared on `(label, prop)`.
    #[must_use]
    pub fn has_unique_constraint(&self, label: &str, prop: &str) -> bool {
        self.constraints.contains(&ConstraintDecl {
            label: label.to_owned(),
            prop: prop.to_owned(),
        })
    }

    /// Returns all declared constraint entries.
    #[must_use]
    pub fn constraints(&self) -> Vec<&ConstraintDecl> {
        self.constraints.iter().collect()
    }

    /// Declares `label` append-only from node id `since_node_id` onwards.
    /// Idempotent: re-declaring an already-declared label keeps the original
    /// boundary rather than moving it forward.
    ///
    /// Nodes created under an append-only label skip MVCC versioning (issue #43
    /// Part A). The declaration is not retroactive — nodes with an id below
    /// `since_node_id` predate it and never take the fast path, which is what
    /// lets [`Graph::open`](crate::Graph::open) reconstruct exactly the same
    /// membership the running graph had (issue #61).
    ///
    /// Callers should pass the graph's next node id. Prefer
    /// [`Graph::set_label_append_only`](crate::Graph::set_label_append_only),
    /// which supplies it and keeps the graph's in-memory set in step.
    pub fn mark_label_append_only(&mut self, label: &str, since_node_id: u64) {
        self.append_only_labels.insert(AppendOnlyDecl {
            label: label.to_owned(),
            since_node_id,
        });
    }

    /// Removes an append-only declaration. No-op if not present.
    ///
    /// This records the withdrawal and nothing else. The set of nodes that
    /// actually takes the fast path lives on the `Graph` and is untouched here,
    /// so calling this directly leaves existing nodes still exempt until the
    /// next `Graph::open` rebuilds that set — the same call then freeing every
    /// node of the label at once. Callers that want the withdrawal to take
    /// effect should go through `Graph::set_label_append_only`, which does both
    /// halves (issue #61).
    pub fn unmark_label_append_only(&mut self, label: &str) {
        self.append_only_labels.remove(&Self::lookup_key(label));
    }

    /// A probe value for looking a declaration up by label.
    ///
    /// `AppendOnlyDecl` compares and hashes by label alone, so the bound here
    /// is never read — but writing `since_node_id: 0` inline at each call site
    /// reads like a claim about the stored value, which it is not. Naming the
    /// construction says once what the zero means, instead of relying on a
    /// comment repeated at every use.
    fn lookup_key(label: &str) -> AppendOnlyDecl {
        AppendOnlyDecl {
            label: label.to_owned(),
            since_node_id: 0, // never compared; see doc above
        }
    }

    /// Returns `true` if `label` is declared append-only.
    ///
    /// This answers "is the label declared", which is the right question when
    /// deciding whether a *new* node takes the fast path. To decide whether an
    /// *existing* node does, use [`Self::append_only_since`] and compare
    /// against the node's id — a declaration does not cover nodes older
    /// than itself (issue #61).
    #[must_use]
    pub fn is_label_append_only(&self, label: &str) -> bool {
        self.append_only_labels.contains(&Self::lookup_key(label))
    }

    /// Returns the first node id covered by `label`'s append-only declaration,
    /// or `None` if the label is not declared.
    ///
    /// A node of this label takes the fast path exactly when its id is at or
    /// above the returned value.
    #[must_use]
    pub fn append_only_since(&self, label: &str) -> Option<u64> {
        self.append_only_labels
            .get(&Self::lookup_key(label))
            .map(|d| d.since_node_id)
    }

    /// Returns all append-only label declarations.
    #[must_use]
    pub fn append_only_labels(&self) -> Vec<&AppendOnlyDecl> {
        self.append_only_labels.iter().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_and_remove_index() {
        let mut cat = SchemaCatalog::default();
        cat.add_index("Person", "id");
        assert!(cat.has_index("Person", "id"));
        cat.remove_index("Person", "id");
        assert!(!cat.has_index("Person", "id"));
    }

    #[test]
    fn add_and_remove_unique_constraint() {
        let mut cat = SchemaCatalog::default();
        cat.add_unique_constraint("Asset", "id");
        assert!(cat.has_unique_constraint("Asset", "id"));
        cat.remove_unique_constraint("Asset", "id");
        assert!(!cat.has_unique_constraint("Asset", "id"));
    }

    #[test]
    fn list_indexes_empty() {
        let cat = SchemaCatalog::default();
        assert!(cat.indexes().is_empty());
    }

    #[test]
    fn list_constraints_empty() {
        let cat = SchemaCatalog::default();
        assert!(cat.constraints().is_empty());
    }

    #[test]
    fn duplicate_index_is_idempotent() {
        let mut cat = SchemaCatalog::default();
        cat.add_index("L", "p");
        cat.add_index("L", "p"); // second call must not duplicate
        assert_eq!(cat.indexes().len(), 1);
    }

    #[test]
    fn remove_nonexistent_is_noop() {
        let mut cat = SchemaCatalog::default();
        cat.remove_index("X", "y"); // must not panic
        cat.remove_unique_constraint("X", "y"); // must not panic
    }

    // ── Issue #43 Part A: append-only label declarations ─────────────────

    #[test]
    fn mark_and_query_append_only_label() {
        let mut cat = SchemaCatalog::default();
        assert!(!cat.is_label_append_only("Event"));
        cat.mark_label_append_only("Event", 0);
        assert!(cat.is_label_append_only("Event"));
        assert!(!cat.is_label_append_only("Person"));
    }

    #[test]
    fn unmark_append_only_label() {
        let mut cat = SchemaCatalog::default();
        cat.mark_label_append_only("Event", 0);
        cat.unmark_label_append_only("Event");
        assert!(!cat.is_label_append_only("Event"));
    }

    #[test]
    fn duplicate_append_only_mark_is_idempotent() {
        let mut cat = SchemaCatalog::default();
        cat.mark_label_append_only("Event", 0);
        cat.mark_label_append_only("Event", 0); // second call must not duplicate
        assert_eq!(cat.append_only_labels().len(), 1);
    }

    /// Re-declaring an already-declared label must keep the ORIGINAL boundary.
    ///
    /// Moving it forward would strand every node created under the first
    /// declaration: they would fall below the new bound and silently lose the
    /// fast path at the next reopen. The declaration in force owns the
    /// boundary, not the most recent statement repeating it (issue #61).
    #[test]
    fn re_marking_a_declared_label_keeps_the_original_boundary() {
        let mut cat = SchemaCatalog::default();
        cat.mark_label_append_only("Event", 10);
        cat.mark_label_append_only("Event", 500);

        assert_eq!(cat.append_only_labels().len(), 1, "still one declaration");
        assert_eq!(
            cat.append_only_since("Event"),
            Some(10),
            "the boundary must not move forward and strand earlier nodes"
        );
    }

    /// Withdrawing drops the entry outright, so a later re-declaration starts a
    /// fresh boundary rather than resurrecting the old one — which is what
    /// keeps nodes freed by the withdrawal from being recaptured.
    #[test]
    fn re_declaring_after_a_withdrawal_takes_a_new_boundary() {
        let mut cat = SchemaCatalog::default();
        cat.mark_label_append_only("Event", 10);
        cat.unmark_label_append_only("Event");
        cat.mark_label_append_only("Event", 500);

        assert_eq!(
            cat.append_only_since("Event"),
            Some(500),
            "a withdrawal clears the entry, so the new declaration sets its own bound"
        );
    }

    #[test]
    fn unmark_nonexistent_append_only_is_noop() {
        let mut cat = SchemaCatalog::default();
        cat.unmark_label_append_only("Nope"); // must not panic
        assert!(cat.append_only_labels().is_empty());
    }
}
