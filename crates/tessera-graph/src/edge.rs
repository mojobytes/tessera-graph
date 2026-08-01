// SPDX-License-Identifier: Apache-2.0

use crate::error::{EdgeId, NodeId};
use crate::property::Properties;

/// A directed edge (relationship) in the property graph.
#[derive(Debug, Clone, PartialEq)]
pub struct Edge {
    pub(crate) id: EdgeId,
    pub(crate) label: String,
    pub(crate) source: NodeId,
    pub(crate) target: NodeId,
    pub(crate) properties: Properties,
}

impl Edge {
    /// Creates a new edge with the given id, label, source, target, and properties.
    pub(crate) fn new(
        id: EdgeId,
        label: impl Into<String>,
        source: NodeId,
        target: NodeId,
        properties: Properties,
    ) -> Self {
        Self {
            id,
            label: label.into(),
            source,
            target,
            properties,
        }
    }

    /// Returns the unique identifier of this edge.
    #[must_use]
    pub const fn id(&self) -> EdgeId {
        self.id
    }

    /// Returns the label (type) of this edge.
    #[must_use]
    pub fn label(&self) -> &str {
        &self.label
    }

    /// Sets the label (type) of this edge.
    pub fn set_label(&mut self, label: impl Into<String>) {
        self.label = label.into();
    }

    /// Returns the source (origin) node identifier.
    #[must_use]
    pub const fn source(&self) -> NodeId {
        self.source
    }

    /// Returns the target (destination) node identifier.
    #[must_use]
    pub const fn target(&self) -> NodeId {
        self.target
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

    /// Constructs an `Edge` for benchmarking purposes.
    #[cfg(feature = "benchmarks")]
    #[doc(hidden)]
    pub fn new_for_bench(
        id: EdgeId,
        label: impl Into<String>,
        source: NodeId,
        target: NodeId,
        properties: Properties,
    ) -> Self {
        Self::new(id, label, source, target, properties)
    }
}
