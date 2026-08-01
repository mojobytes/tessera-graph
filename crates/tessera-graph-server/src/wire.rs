// SPDX-License-Identifier: BSL-1.1

//! Conversion from runtime `GqlValue` to Bolt `PackStreamValue` (the wire
//! layer). Lives in its own module so the Node/Relationship/Path struct
//! encoding (Fase B) is unit-testable without standing up a Bolt session;
//! the handler delegates here.

use std::collections::HashMap;

use tessera_graph::gql::{GqlNode, GqlPath, GqlRelationship, GqlValue};
use tessera_graph_protocol::packstream::PackStreamValue;
use tessera_graph_protocol::packstream::markers::{
    TAG_NODE, TAG_PATH, TAG_RELATIONSHIP, TAG_UNBOUND_RELATIONSHIP,
};

/// Encodes a property map as a Bolt `Dict`. Keys are sorted so the wire output
/// is deterministic regardless of `HashMap` iteration order — clients and the
/// equality assertions in tests both rely on a stable ordering.
fn props_to_dict(props: &HashMap<String, GqlValue>) -> PackStreamValue {
    let mut entries: Vec<(String, PackStreamValue)> = props
        .iter()
        .map(|(k, v)| (k.clone(), gql_value_to_packstream(Some(v))))
        .collect();
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    PackStreamValue::Dict(entries)
}

/// Bolt Node struct (tag `0x4E`): `[id, labels, props]`.
fn node_to_struct(n: &GqlNode) -> PackStreamValue {
    PackStreamValue::Struct {
        tag: TAG_NODE,
        fields: vec![
            PackStreamValue::Int(n.id),
            PackStreamValue::List(
                n.labels
                    .iter()
                    .cloned()
                    .map(PackStreamValue::String)
                    .collect(),
            ),
            props_to_dict(&n.props),
        ],
    }
}

/// Bolt Relationship struct (tag `0x52`): `[id, start, end, type, props]`.
fn rel_to_struct(r: &GqlRelationship) -> PackStreamValue {
    PackStreamValue::Struct {
        tag: TAG_RELATIONSHIP,
        fields: vec![
            PackStreamValue::Int(r.id),
            PackStreamValue::Int(r.start_id),
            PackStreamValue::Int(r.end_id),
            PackStreamValue::String(r.rel_type.clone()),
            props_to_dict(&r.props),
        ],
    }
}

/// Builds the Bolt Path struct (tag `0x50`): `[nodes, unbound_rels, indices]`.
///
/// `indices` interleaves `[signed_rel_index, node_index, …]`. For a simple
/// forward chain the rel index is `i+1` (1-based, positive = traversed in its
/// stored direction); the node index is the position of the next node in the
/// `nodes` list. This shape is verified against the real driver in C7.
fn path_to_struct(p: &GqlPath) -> PackStreamValue {
    let nodes = PackStreamValue::List(p.nodes.iter().map(node_to_struct).collect());
    let unbound = PackStreamValue::List(
        p.rels
            .iter()
            .map(|r| PackStreamValue::Struct {
                tag: TAG_UNBOUND_RELATIONSHIP,
                fields: vec![
                    PackStreamValue::Int(r.id),
                    PackStreamValue::String(r.rel_type.clone()),
                    props_to_dict(&r.props),
                ],
            })
            .collect(),
    );
    let mut indices = Vec::with_capacity(p.rels.len() * 2);
    for i in 0..p.rels.len() {
        #[allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation)]
        let idx = (i as i64) + 1;
        indices.push(PackStreamValue::Int(idx)); // signed rel index, 1-based
        indices.push(PackStreamValue::Int(idx)); // index of next node
    }
    PackStreamValue::Struct {
        tag: TAG_PATH,
        fields: vec![nodes, unbound, PackStreamValue::List(indices)],
    }
}

/// Convert a GQL value to `PackStreamValue` for the Bolt wire.
#[must_use]
pub fn gql_value_to_packstream(val: Option<&GqlValue>) -> PackStreamValue {
    match val {
        None | Some(GqlValue::Null) => PackStreamValue::Null,
        Some(GqlValue::Bool(b)) => PackStreamValue::Bool(*b),
        Some(GqlValue::Int(n)) => PackStreamValue::Int(*n),
        Some(GqlValue::Float(f)) => PackStreamValue::Float(*f),
        Some(GqlValue::Str(s)) => PackStreamValue::String(s.clone()),
        Some(GqlValue::List(items)) => PackStreamValue::List(
            items
                .iter()
                .map(|v| gql_value_to_packstream(Some(v)))
                .collect(),
        ),
        Some(GqlValue::Map(m)) => props_to_dict(m),
        Some(GqlValue::Node(n)) => node_to_struct(n),
        Some(GqlValue::Relationship(r)) => rel_to_struct(r),
        Some(GqlValue::Path(p)) => path_to_struct(p),
    }
}
