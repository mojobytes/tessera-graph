// SPDX-License-Identifier: MIT

use crate::error::NodeId;
use crate::property::Properties;
use crate::storage::codec::node_codec::ADJ_PAGE_ID_SENTINEL;

/// A node (vertex) in the property graph.
///
/// `adj_page_id`/`adj_flags` are a *physical* detail — the on-disk head of the
/// node's adjacency chain — carried on the in-memory `Node` so that every path
/// that re-serializes a slot from a `Node` (the MVCC vacuum, the WAL commit
/// redo) preserves the pointer instead of resetting it to the sentinel. They
/// are deliberately excluded from equality and hashing: two nodes with the same
/// id, label, and properties are the *same node* regardless of where their
/// edges happen to live on disk.
#[derive(Debug, Clone)]
pub struct Node {
    pub(crate) id: NodeId,
    pub(crate) label: String,
    pub(crate) properties: Properties,
    /// Head page of this node's outgoing adjacency chain, or
    /// [`ADJ_PAGE_ID_SENTINEL`] when the node has no outgoing edges yet.
    pub(crate) adj_page_id: u32,
    /// Head page of this node's incoming adjacency chain, or
    /// [`ADJ_PAGE_ID_SENTINEL`] when the node has no incoming edges yet.
    pub(crate) adj_incoming_page_id: u32,
    /// Direction flags for the adjacency chains (outgoing/incoming bits).
    pub(crate) adj_flags: u8,
}

impl PartialEq for Node {
    /// Logical equality: id, label, properties. The physical adjacency pointer
    /// is intentionally ignored (see the struct docs).
    ///
    /// NOTE: this is a hand-written impl, not a derive. If you add a *logical*
    /// field to `Node`, add it here too — the compiler will not flag its
    /// absence, and two nodes that differ only in the new field would silently
    /// compare equal. Physical/storage-only fields (like the adjacency pointer)
    /// stay out.
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id && self.label == other.label && self.properties == other.properties
    }
}

impl Node {
    /// Creates a new node with the given id, label, and properties.
    ///
    /// The adjacency pointer starts at the sentinel ("no edges yet"); it is set
    /// by the storage layer when the node's first edge is written and refreshed
    /// on every read from disk.
    ///
    /// WARNING for storage code: a `Node` built here carries the sentinel
    /// pointer. Re-serializing such a node with `encode_node_slot` writes the
    /// sentinel back, erasing any real adjacency head already on the page. Every
    /// path that re-persists an existing node's slot (vacuum, WAL redo) must
    /// obtain the node via `decode_node_slot`/`read_node` — which copies the
    /// on-disk pointer onto the `Node` — never via a fresh `Node::new`.
    pub(crate) fn new(id: NodeId, label: impl Into<String>, properties: Properties) -> Self {
        Self {
            id,
            label: label.into(),
            properties,
            adj_page_id: ADJ_PAGE_ID_SENTINEL,
            adj_incoming_page_id: ADJ_PAGE_ID_SENTINEL,
            adj_flags: 0,
        }
    }

    /// Returns the unique identifier of this node.
    #[must_use]
    pub const fn id(&self) -> NodeId {
        self.id
    }

    /// Returns the label (type) of this node.
    #[must_use]
    pub fn label(&self) -> &str {
        &self.label
    }

    /// Sets the label (type) of this node.
    pub fn set_label(&mut self, label: impl Into<String>) {
        self.label = label.into();
    }

    /// Returns a reference to the properties map.
    #[must_use]
    pub const fn properties(&self) -> &Properties {
        &self.properties
    }

    /// Returns a mutable reference to the properties map.
    pub const fn properties_mut(&mut self) -> &mut Properties {
        &mut self.properties
    }

    /// Constructs a `Node` for benchmarking purposes.
    #[cfg(feature = "benchmarks")]
    #[doc(hidden)]
    pub fn new_for_bench(id: NodeId, label: impl Into<String>, properties: Properties) -> Self {
        Self::new(id, label, properties)
    }
}
