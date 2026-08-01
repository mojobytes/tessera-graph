// SPDX-License-Identifier: LicenseRef-TesseraGraph-Proprietary

//! Behavioral tests for Node/Relationship/Path → `PackStream` struct conversion
//! (Fase B C2). Asserts the Bolt struct tags `0x4E`/`0x52`/`0x50`/`0x72` and
//! the path `indices` shape. The wire-level deserialisation against the real
//! driver is the C7 e2e gate; these tests pin the in-process struct shape.

use std::collections::HashMap;

use tessera_graph::gql::{GqlNode, GqlPath, GqlRelationship, GqlValue};
use tessera_graph_protocol::packstream::PackStreamValue;
use tessera_graph_server::wire::gql_value_to_packstream;

#[test]
fn node_serializes_as_struct_0x4e() {
    let n = GqlNode {
        id: 1,
        labels: vec!["User".into()],
        props: HashMap::new(),
    };
    match gql_value_to_packstream(Some(&GqlValue::Node(n))) {
        PackStreamValue::Struct { tag, fields } => {
            assert_eq!(tag, 0x4E);
            assert_eq!(fields.len(), 3); // id, labels, props
            assert_eq!(fields[0], PackStreamValue::Int(1));
            assert_eq!(
                fields[1],
                PackStreamValue::List(vec![PackStreamValue::String("User".into())])
            );
        }
        other => panic!("expected Node struct, got {other:?}"),
    }
}

#[test]
fn relationship_serializes_as_struct_0x52() {
    let r = GqlRelationship {
        id: 9,
        start_id: 1,
        end_id: 2,
        rel_type: "OWNS".into(),
        props: HashMap::new(),
    };
    match gql_value_to_packstream(Some(&GqlValue::Relationship(r))) {
        PackStreamValue::Struct { tag, fields } => {
            assert_eq!(tag, 0x52);
            assert_eq!(fields.len(), 5); // id, start, end, type, props
            assert_eq!(fields[0], PackStreamValue::Int(9));
            assert_eq!(fields[1], PackStreamValue::Int(1));
            assert_eq!(fields[2], PackStreamValue::Int(2));
            assert_eq!(fields[3], PackStreamValue::String("OWNS".into()));
        }
        other => panic!("expected Relationship struct, got {other:?}"),
    }
}

#[test]
fn path_serializes_as_struct_0x50_with_unbound_rels() {
    let n0 = GqlNode {
        id: 1,
        labels: vec!["A".into()],
        props: HashMap::new(),
    };
    let n1 = GqlNode {
        id: 2,
        labels: vec!["B".into()],
        props: HashMap::new(),
    };
    let r = GqlRelationship {
        id: 9,
        start_id: 1,
        end_id: 2,
        rel_type: "E".into(),
        props: HashMap::new(),
    };
    let p = GqlPath {
        nodes: vec![n0, n1],
        rels: vec![r],
    };
    match gql_value_to_packstream(Some(&GqlValue::Path(p))) {
        PackStreamValue::Struct { tag, fields } => {
            assert_eq!(tag, 0x50);
            assert_eq!(fields.len(), 3); // nodes, rels, indices

            // nodes field is a list of 0x4E structs.
            let PackStreamValue::List(nodes) = &fields[0] else {
                panic!("nodes field not a list");
            };
            assert_eq!(nodes.len(), 2);

            // rels list holds UnboundRelationship (0x72), NOT 0x52.
            let PackStreamValue::List(rels) = &fields[1] else {
                panic!("rels field not a list");
            };
            let PackStreamValue::Struct {
                tag: rtag,
                fields: rf,
            } = &rels[0]
            else {
                panic!("rel not a struct");
            };
            assert_eq!(*rtag, 0x72);
            assert_eq!(rf.len(), 3); // id, type, props (no start/end)
            assert_eq!(rf[0], PackStreamValue::Int(9));
            assert_eq!(rf[1], PackStreamValue::String("E".into()));

            // indices interleave [signed_rel_index, node_index]; 1-rel path → [1, 1].
            assert_eq!(
                fields[2],
                PackStreamValue::List(vec![PackStreamValue::Int(1), PackStreamValue::Int(1)])
            );
        }
        other => panic!("expected Path struct, got {other:?}"),
    }
}
