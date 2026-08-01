// SPDX-License-Identifier: Apache-2.0

// Index layer: label-based indexes for fast entity lookup.

pub mod codec;

use std::collections::{BTreeMap, HashMap, HashSet};

use crate::property::{Properties, Property};

/// In-memory index mapping labels to sets of entity IDs.
///
/// Maintains a `HashMap<String, HashSet<u64>>` where the key is the label
/// string and the value is the set of raw IDs (node or edge) carrying that
/// label. Two separate instances are kept: one for nodes, one for edges.
#[derive(Debug, Default)]
pub struct LabelIndex {
    map: HashMap<String, HashSet<u64>>,
}

impl LabelIndex {
    /// Creates an empty index.
    pub fn new() -> Self {
        Self {
            map: HashMap::new(),
        }
    }

    /// Registers `id` under the given `label`. Idempotent: inserting the same
    /// (label, id) pair twice is a no-op.
    pub fn insert(&mut self, label: &str, id: u64) {
        // Avoid cloning the label string when the entry already exists.
        if let Some(ids) = self.map.get_mut(label) {
            ids.insert(id);
        } else {
            let mut set = HashSet::new();
            set.insert(id);
            self.map.insert(label.to_owned(), set);
        }
    }

    /// Removes `id` from the given `label`. If the label has no remaining IDs,
    /// the entry is removed entirely to avoid leaking empty sets.
    pub fn remove(&mut self, label: &str, id: u64) {
        if let Some(ids) = self.map.get_mut(label) {
            ids.remove(&id);
            if ids.is_empty() {
                self.map.remove(label);
            }
        }
    }

    /// Returns a snapshot of all IDs registered under `label`.
    ///
    /// Returns an empty `Vec` for unknown labels. The returned values are raw
    /// `u64` identifiers by design — callers in `Graph` wrap them in `NodeId`
    /// or `EdgeId` at the boundary. This avoids leaking domain types into the
    /// index layer.
    pub fn ids_for(&self, label: &str) -> Vec<u64> {
        self.map
            .get(label)
            .map(|set| set.iter().copied().collect())
            .unwrap_or_default()
    }

    /// Clears all entries from the index.
    #[allow(dead_code)] // Used in Phase 3 (WAL recovery)
    pub fn clear(&mut self) {
        self.map.clear();
    }

    /// Iterates over all (label, id) pairs. Used by the codec for serialization.
    pub fn iter(&self) -> impl Iterator<Item = (&str, u64)> {
        self.map
            .iter()
            .flat_map(|(label, ids)| ids.iter().copied().map(move |id| (label.as_str(), id)))
    }

    /// Returns the total number of (label, id) entries across all labels.
    pub fn entry_count(&self) -> usize {
        self.map.values().map(HashSet::len).sum()
    }

    /// Returns a sorted list of all distinct labels/types currently in the index.
    ///
    /// Only labels with at least one registered entity are returned; empty label
    /// entries are removed in [`Self::remove`], so the result never contains a
    /// label with zero IDs. Allocates a new `Vec<String>` on every call —
    /// intended for introspection (`CALL` procedures), which fire rarely, not for
    /// the hot-path insert/lookup.
    #[must_use]
    pub fn distinct_labels(&self) -> Vec<String> {
        let mut labels: Vec<String> = self.map.keys().cloned().collect();
        labels.sort_unstable();
        labels
    }
}

// ---------------------------------------------------------------------------
// PropertyIndex
// ---------------------------------------------------------------------------

/// Hashable wrapper for `Property` values used as keys in the property index.
///
/// `f64` values are stored as their bit-pattern (`to_bits()`) so that the
/// `Hash + Eq` contract is satisfied. This means two `f64` values that are
/// bitwise-equal are considered the same key (NaN != NaN is NOT enforced here —
/// callers should avoid indexing NaN values).
#[derive(Debug, Clone, Hash, Eq, PartialEq)]
enum PropertyValueKey {
    String(String),
    I64(i64),
    Bool(bool),
    Bytes(Vec<u8>),
    /// `f64` stored as its bit pattern so the `Hash + Eq` contract is satisfied.
    F64Bits(u64),
}

impl From<&Property> for PropertyValueKey {
    fn from(p: &Property) -> Self {
        match p {
            Property::String(s) => Self::String(s.clone()),
            Property::I64(v) => Self::I64(*v),
            Property::F64(v) => Self::F64Bits(v.to_bits()),
            Property::Bool(b) => Self::Bool(*b),
            Property::Bytes(v) => Self::Bytes(v.clone()),
        }
    }
}

/// In-memory index mapping `(label, prop_key, prop_value)` triples to sets of
/// node IDs for O(1) property equality lookups.
///
/// Structure:
/// ```text
/// label  →  prop_key  →  PropertyValueKey  →  HashSet<node_id>
/// ```
#[derive(Debug, Default)]
pub struct PropertyIndex {
    /// `label → prop_key → prop_value_key → HashSet<node_ids>`
    index: HashMap<String, HashMap<String, HashMap<PropertyValueKey, HashSet<u64>>>>,
    /// Ordered companion index, `I64` values only: `label → prop_key → value →
    /// HashSet<node_ids>`, kept in lockstep with `index` by `insert`/`remove`.
    /// The `BTreeMap` gives ordered access (min/max, range) that the hash index
    /// cannot; non-`I64` values are not stored here (issue #40/#41). Memory is
    /// only paid for `I64`-valued properties.
    ordered: HashMap<String, HashMap<String, BTreeMap<i64, HashSet<u64>>>>,
}

impl PropertyIndex {
    /// Creates an empty index.
    #[must_use]
    pub fn new() -> Self {
        Self {
            index: HashMap::new(),
            ordered: HashMap::new(),
        }
    }

    /// Registers `id` under `(label, key, value)`. Idempotent. When `value` is an
    /// `I64`, the ordered companion index is updated in lockstep.
    pub fn insert(&mut self, label: &str, key: &str, value: &Property, id: u64) {
        self.index
            .entry(label.to_owned())
            .or_default()
            .entry(key.to_owned())
            .or_default()
            .entry(PropertyValueKey::from(value))
            .or_default()
            .insert(id);
        if let Property::I64(v) = value {
            self.ordered
                .entry(label.to_owned())
                .or_default()
                .entry(key.to_owned())
                .or_default()
                .entry(*v)
                .or_default()
                .insert(id);
        }
    }

    /// Removes `id` from `(label, key, value)`. Cleans up empty inner maps.
    /// No-op if the entry does not exist.
    pub fn remove(&mut self, label: &str, key: &str, value: &Property, id: u64) {
        let vk = PropertyValueKey::from(value);
        if let Some(by_key) = self.index.get_mut(label) {
            if let Some(by_value) = by_key.get_mut(key) {
                if let Some(ids) = by_value.get_mut(&vk) {
                    ids.remove(&id);
                    if ids.is_empty() {
                        by_value.remove(&vk);
                    }
                }
                if by_value.is_empty() {
                    by_key.remove(key);
                }
            }
            if by_key.is_empty() {
                self.index.remove(label);
            }
        }
        // Mirror the removal in the ordered index (I64 only), cleaning empty
        // maps in cascade exactly like the hash side so the two never diverge.
        if let Property::I64(v) = value {
            if let Some(by_key) = self.ordered.get_mut(label) {
                if let Some(by_value) = by_key.get_mut(key) {
                    if let Some(ids) = by_value.get_mut(v) {
                        ids.remove(&id);
                        if ids.is_empty() {
                            by_value.remove(v);
                        }
                    }
                    if by_value.is_empty() {
                        by_key.remove(key);
                    }
                }
                if by_key.is_empty() {
                    self.ordered.remove(label);
                }
            }
        }
    }

    /// Returns all IDs indexed under `(label, key, value)`.
    /// Returns an empty `Vec` for unknown combinations.
    #[must_use]
    pub fn lookup(&self, label: &str, key: &str, value: &Property) -> Vec<u64> {
        self.lookup_set(label, key, value)
            .map(|ids| ids.iter().copied().collect())
            .unwrap_or_default()
    }

    /// Returns a reference to the set of IDs indexed under `(label, key, value)`,
    /// or `None` if no entries exist. Avoids allocating a `Vec` when the caller
    /// only needs to iterate or check membership.
    #[must_use]
    pub fn lookup_set(&self, label: &str, key: &str, value: &Property) -> Option<&HashSet<u64>> {
        let vk = PropertyValueKey::from(value);
        self.index
            .get(label)
            .and_then(|by_key| by_key.get(key))
            .and_then(|by_value| by_value.get(&vk))
    }

    /// Removes all property index entries for `id` with the given label and
    /// properties. Convenience wrapper that calls [`remove`](Self::remove) for
    /// each property.
    pub fn remove_node(&mut self, label: &str, properties: &Properties, id: u64) {
        for (key, value) in properties {
            self.remove(label, key, value, id);
        }
    }

    /// Inserts all property index entries for `id` with the given label and
    /// properties. Convenience wrapper that calls [`insert`](Self::insert) for
    /// each property.
    pub fn insert_node(&mut self, label: &str, properties: &Properties, id: u64) {
        for (key, value) in properties {
            self.insert(label, key, value, id);
        }
    }

    /// Returns the node IDs whose `I64` property `(label, key)` falls in the
    /// half-open range `[lo, hi)`. `lo = None` means unbounded below, `hi = None`
    /// unbounded above; both `None` returns every `I64`-valued node for the
    /// property. Order of returned IDs is not guaranteed. Cost is proportional to
    /// the number of matching values, not the whole scope (issue #41).
    #[must_use]
    pub fn range_i64(
        &self,
        label: &str,
        key: &str,
        lo: Option<i64>,
        hi: Option<i64>,
    ) -> Vec<u64> {
        use std::ops::Bound;
        let Some(tree) = self
            .ordered
            .get(label)
            .and_then(|by_key| by_key.get(key))
        else {
            return Vec::new();
        };
        let lower = lo.map_or(Bound::Unbounded, Bound::Included);
        let upper = hi.map_or(Bound::Unbounded, Bound::Excluded);
        tree.range((lower, upper))
            .flat_map(|(_, ids)| ids.iter().copied())
            .collect()
    }

    /// Iterates the `I64` value-sets for `(label, key)` from highest value to
    /// lowest. Empty iterator if the property has no `I64` entries. Lets a caller
    /// find the highest node that also satisfies an extra predicate (e.g. MVCC
    /// visibility) by walking down until one matches — issue #40's max, made
    /// robust to a not-yet-vacuumed deleted top value.
    pub fn iter_i64_desc(
        &self,
        label: &str,
        key: &str,
    ) -> impl Iterator<Item = (i64, &HashSet<u64>)> {
        self.ordered
            .get(label)
            .and_then(|by_key| by_key.get(key))
            .into_iter()
            .flat_map(|tree| tree.iter().rev().map(|(v, ids)| (*v, ids)))
    }

    /// Returns every node ID that has ANY value for property `(label, key)` —
    /// the union of all value-sets. Used to compute property absence by set
    /// difference against the label's full membership (issue #42 substitute).
    #[must_use]
    pub fn ids_with_property(&self, label: &str, key: &str) -> HashSet<u64> {
        self.index
            .get(label)
            .and_then(|by_key| by_key.get(key))
            .map(|by_value| by_value.values().flatten().copied().collect())
            .unwrap_or_default()
    }

    /// Clears all entries from the index.
    pub fn clear(&mut self) {
        self.index.clear();
        self.ordered.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distinct_labels_empty() {
        let idx = LabelIndex::new();
        assert!(idx.distinct_labels().is_empty());
    }

    #[test]
    fn distinct_labels_returns_all_keys() {
        let mut idx = LabelIndex::new();
        idx.insert("Person", 1);
        idx.insert("Person", 2);
        idx.insert("Asset", 3);
        let mut labels = idx.distinct_labels();
        labels.sort();
        assert_eq!(labels, vec!["Asset".to_owned(), "Person".to_owned()]);
    }

    #[test]
    fn distinct_labels_excludes_empty_labels() {
        // After removing the last ID for a label, that label must not appear.
        let mut idx = LabelIndex::new();
        idx.insert("Ghost", 1);
        idx.insert("Kept", 2);
        idx.remove("Ghost", 1);
        let mut labels = idx.distinct_labels();
        labels.sort();
        assert_eq!(labels, vec!["Kept".to_owned()]);
    }

    #[test]
    fn insert_and_lookup_returns_ids() {
        let mut idx = LabelIndex::new();
        idx.insert("Person", 1);
        idx.insert("Person", 2);
        idx.insert("Device", 10);

        let mut persons = idx.ids_for("Person");
        persons.sort_unstable();
        assert_eq!(persons, vec![1, 2]);

        assert_eq!(idx.ids_for("Device"), vec![10]);
    }

    #[test]
    fn insert_duplicate_id_is_idempotent() {
        let mut idx = LabelIndex::new();
        idx.insert("Person", 1);
        idx.insert("Person", 1);
        assert_eq!(idx.ids_for("Person").len(), 1);
    }

    #[test]
    fn remove_last_id_removes_label_entry() {
        let mut idx = LabelIndex::new();
        idx.insert("Person", 1);
        idx.remove("Person", 1);
        assert!(idx.ids_for("Person").is_empty());
        assert_eq!(idx.entry_count(), 0);
    }

    #[test]
    fn remove_nonexistent_id_is_noop() {
        let mut idx = LabelIndex::new();
        idx.insert("Person", 1);
        idx.remove("Person", 999);
        assert_eq!(idx.ids_for("Person").len(), 1);
        // Remove from unknown label — also a no-op
        idx.remove("Ghost", 1);
    }

    #[test]
    fn ids_for_unknown_label_returns_empty() {
        let idx = LabelIndex::new();
        assert!(idx.ids_for("Unknown").is_empty());
    }

    #[test]
    fn label_index_default_is_empty() {
        let idx = LabelIndex::default();
        assert_eq!(idx.entry_count(), 0);
        assert!(idx.ids_for("X").is_empty());
    }

    #[test]
    fn label_index_is_empty_after_clear() {
        let mut idx = LabelIndex::new();
        idx.insert("A", 1);
        idx.insert("B", 2);
        idx.clear();
        assert!(idx.ids_for("A").is_empty());
        assert!(idx.ids_for("B").is_empty());
        assert_eq!(idx.entry_count(), 0);
    }

    // -----------------------------------------------------------------------
    // PropertyIndex tests
    // -----------------------------------------------------------------------

    #[test]
    fn property_index_insert_and_lookup() {
        let mut idx = PropertyIndex::new();
        idx.insert("Person", "name", &Property::String("Alice".into()), 1);
        let ids = idx.lookup("Person", "name", &Property::String("Alice".into()));
        assert_eq!(ids, vec![1]);
    }

    #[test]
    fn property_index_multiple_nodes_same_value() {
        let mut idx = PropertyIndex::new();
        idx.insert("Person", "age", &Property::I64(30), 1);
        idx.insert("Person", "age", &Property::I64(30), 2);
        let mut ids = idx.lookup("Person", "age", &Property::I64(30));
        ids.sort_unstable();
        assert_eq!(ids, vec![1, 2]);
    }

    #[test]
    fn property_index_different_values_same_key() {
        let mut idx = PropertyIndex::new();
        idx.insert("Person", "age", &Property::I64(30), 1);
        idx.insert("Person", "age", &Property::I64(40), 2);
        assert_eq!(idx.lookup("Person", "age", &Property::I64(30)), vec![1]);
        assert_eq!(idx.lookup("Person", "age", &Property::I64(40)), vec![2]);
    }

    #[test]
    fn property_index_different_labels_same_property() {
        let mut idx = PropertyIndex::new();
        idx.insert("Person", "active", &Property::Bool(true), 1);
        idx.insert("Robot", "active", &Property::Bool(true), 2);
        assert_eq!(idx.lookup("Person", "active", &Property::Bool(true)), vec![1]);
        assert_eq!(idx.lookup("Robot", "active", &Property::Bool(true)), vec![2]);
    }

    #[test]
    fn property_index_remove_entry() {
        let mut idx = PropertyIndex::new();
        idx.insert("Person", "name", &Property::String("Alice".into()), 1);
        idx.remove("Person", "name", &Property::String("Alice".into()), 1);
        assert!(idx.lookup("Person", "name", &Property::String("Alice".into())).is_empty());
        // Verify empty maps were cleaned up — no entries remain
        assert!(idx.index.is_empty());
    }

    #[test]
    fn property_index_remove_nonexistent_is_noop() {
        let mut idx = PropertyIndex::new();
        idx.insert("Person", "name", &Property::String("Alice".into()), 1);
        // Remove an id that doesn't exist
        idx.remove("Person", "name", &Property::String("Alice".into()), 999);
        assert_eq!(idx.lookup("Person", "name", &Property::String("Alice".into())), vec![1]);
        // Remove from unknown label
        idx.remove("Ghost", "name", &Property::String("Alice".into()), 1);
    }

    #[test]
    fn property_index_lookup_unknown_returns_empty() {
        let idx = PropertyIndex::new();
        assert!(idx.lookup("Person", "name", &Property::String("Alice".into())).is_empty());
    }

    #[test]
    fn property_index_all_property_types() {
        let mut idx = PropertyIndex::new();
        idx.insert("N", "s", &Property::String("hello".into()), 1);
        idx.insert("N", "i", &Property::I64(42), 2);
        idx.insert("N", "f", &Property::F64(2.71), 3);
        idx.insert("N", "b", &Property::Bool(false), 4);
        idx.insert("N", "v", &Property::Bytes(vec![0xDE, 0xAD]), 5);

        assert_eq!(idx.lookup("N", "s", &Property::String("hello".into())), vec![1]);
        assert_eq!(idx.lookup("N", "i", &Property::I64(42)), vec![2]);
        assert_eq!(idx.lookup("N", "f", &Property::F64(2.71)), vec![3]);
        assert_eq!(idx.lookup("N", "b", &Property::Bool(false)), vec![4]);
        assert_eq!(idx.lookup("N", "v", &Property::Bytes(vec![0xDE, 0xAD])), vec![5]);
    }

    #[test]
    fn property_index_insert_node_convenience() {
        let mut idx = PropertyIndex::new();
        let mut props = Properties::new();
        props.insert("name".into(), Property::String("Bob".into()));
        props.insert("age".into(), Property::I64(25));
        idx.insert_node("Person", &props, 10);

        assert_eq!(idx.lookup("Person", "name", &Property::String("Bob".into())), vec![10]);
        assert_eq!(idx.lookup("Person", "age", &Property::I64(25)), vec![10]);
    }

    // ── Issue #40/#41: ordered I64 index ──────────────────────────────────────

    fn sorted(mut v: Vec<u64>) -> Vec<u64> {
        v.sort_unstable();
        v
    }

    #[test]
    fn property_index_ordered_i64_range_basic() {
        let mut idx = PropertyIndex::new();
        idx.insert("Event", "seq", &Property::I64(100), 1);
        idx.insert("Event", "seq", &Property::I64(200), 2);
        idx.insert("Event", "seq", &Property::I64(300), 3);
        // Half-open [150, 300): includes 200, excludes 300 and 100.
        assert_eq!(
            sorted(idx.range_i64("Event", "seq", Some(150), Some(300))),
            vec![2]
        );
        // Open lower bound (< 250): 100 and 200.
        assert_eq!(
            sorted(idx.range_i64("Event", "seq", None, Some(250))),
            vec![1, 2]
        );
        // Open upper bound (>= 200): 200 and 300.
        assert_eq!(
            sorted(idx.range_i64("Event", "seq", Some(200), None)),
            vec![2, 3]
        );
        // Fully open: all.
        assert_eq!(
            sorted(idx.range_i64("Event", "seq", None, None)),
            vec![1, 2, 3]
        );
    }

    #[test]
    fn property_index_ordered_i64_ignores_non_i64_values() {
        let mut idx = PropertyIndex::new();
        idx.insert("N", "k", &Property::String("hi".into()), 1);
        idx.insert("N", "k", &Property::F64(3.5), 2);
        idx.insert("N", "k", &Property::I64(7), 3);
        // Only the I64 value participates in the ordered range.
        assert_eq!(sorted(idx.range_i64("N", "k", None, None)), vec![3]);
    }

    #[test]
    fn property_index_ordered_i64_remove_keeps_hashmap_and_btree_coherent() {
        let mut idx = PropertyIndex::new();
        idx.insert("Event", "seq", &Property::I64(100), 1);
        idx.insert("Event", "seq", &Property::I64(100), 2);
        idx.remove("Event", "seq", &Property::I64(100), 1);
        // HashMap side: only id 2 left for value 100.
        assert_eq!(idx.lookup("Event", "seq", &Property::I64(100)), vec![2]);
        // BTree side: same — id 2 only.
        assert_eq!(sorted(idx.range_i64("Event", "seq", None, None)), vec![2]);
        // Removing the last id cleans both sides — the range is now empty.
        idx.remove("Event", "seq", &Property::I64(100), 2);
        assert!(idx.range_i64("Event", "seq", None, None).is_empty());
        assert!(idx.lookup("Event", "seq", &Property::I64(100)).is_empty());
    }
}
