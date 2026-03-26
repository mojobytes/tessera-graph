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
    pub const fn new(level: u16, compartments: BTreeSet<String>) -> Self {
        Self {
            level,
            compartments,
        }
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
    pub const fn new(level: u16, compartments: BTreeSet<String>) -> Self {
        Self {
            level,
            compartments,
        }
    }

    /// Returns `true` iff this clearance dominates the given label.
    ///
    /// Dominance: `self.level >= label.level` AND `label.compartments ⊆ self.compartments`.
    #[must_use]
    pub fn dominates(&self, label: &SecurityLabel) -> bool {
        self.level >= label.level && label.compartments.is_subset(&self.compartments)
    }
}

/// Keys used to store security labels as reserved node/edge properties.
///
/// These properties are invisible to users — they are injected/extracted/stripped
/// by `SecurityPolicy` and never exposed in query results.
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
        props.insert(
            Self::LEVEL_KEY.to_string(),
            Property::I64(i64::from(label.level)),
        );
        let encoded: String = label
            .compartments
            .iter()
            .cloned()
            .collect::<Vec<_>>()
            .join(",");
        props.insert(
            Self::COMPARTMENTS_KEY.to_string(),
            Property::String(encoded),
        );
    }

    /// Extracts a `SecurityLabel` from `props`.
    ///
    /// Missing or malformed properties fall back to level 0 / empty compartments.
    #[must_use]
    pub fn extract_label(props: &tessera_graph::Properties) -> SecurityLabel {
        let level = props
            .get(Self::LEVEL_KEY)
            .and_then(tessera_graph::Property::as_i64)
            .and_then(|v| u16::try_from(v).ok())
            .unwrap_or(0);

        let compartments = props
            .get(Self::COMPARTMENTS_KEY)
            .and_then(tessera_graph::Property::as_str)
            .map(|s| {
                if s.is_empty() {
                    BTreeSet::new()
                } else {
                    s.split(',').map(ToString::to_string).collect()
                }
            })
            .unwrap_or_default();

        SecurityLabel {
            level,
            compartments,
        }
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
