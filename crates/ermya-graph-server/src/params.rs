// SPDX-License-Identifier: BSL-1.1

//! Bolt `RUN` parameter conversion.
//!
//! Converts the `params: BoltDict` field of a Bolt `RUN` message into a
//! `HashMap<String, GqlValue>` that `param_substitution::apply` consumes
//! before the AST reaches the compiler. The conversion lives in
//! `ermya-graph-server` (not `ermya-graph-protocol`) to keep the
//! protocol crate independent of the engine's value type — only the
//! server crate depends on both.
//!
//! Wired by the handler in cycle 11 of the parser fix.

use std::collections::HashMap;

use thiserror::Error;

use ermya_graph::gql::GqlValue;
use ermya_graph_protocol::bolt_message::BoltDict;
use ermya_graph_protocol::packstream::PackStreamValue;

/// Errors surfaced when a Bolt `RUN.params` value cannot be represented
/// as a [`GqlValue`].
///
/// Each variant maps to a stable Bolt wire code at the handler seam — see
/// the handler's failure mapping in cycle 11. The `thiserror` derive
/// keeps the variant's `Display`/`Error` impls colocated with the data
/// shape, matching the convention used by `gql::param_substitution::ParamError`.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum BoltParamError {
    /// A parameter value is of a type that the engine does not represent
    /// as a `GqlValue` (`Bytes`, `Dict`, `Struct`). Carries the parameter
    /// key and a canonical name for the unsupported variant.
    #[error("parameter ${key} has wire type {got} which cannot be used as a query parameter")]
    UnsupportedValueType {
        /// The parameter key under which the unsupported value appeared.
        /// For nested values inside a list, the key is suffixed with `[i]`
        /// using the offending element's index.
        key: String,
        /// The canonical name of the `PackStreamValue` variant that could
        /// not be lowered, e.g. `"Bytes"`, `"Dict"`, or `"Struct"`.
        got: &'static str,
    },
}

/// Converts a [`BoltDict`] (parameters as decoded from a Bolt `RUN`
/// message) into a `HashMap<String, GqlValue>` ready for
/// `param_substitution::apply`.
///
/// `Dict` values lower to [`GqlValue::Map`] (recursively), enabling
/// parameterised property maps (`SET n = $map`, `MERGE … $param`,
/// `CREATE (n $map)`). `Bytes` and `Struct` remain unsupported and produce
/// [`BoltParamError::UnsupportedValueType`].
///
/// # Errors
///
/// Returns `Err` on the first unsupported value the helper encounters —
/// processing stops at that point.
pub fn bolt_dict_to_value_map(
    dict: &BoltDict,
) -> Result<HashMap<String, GqlValue>, BoltParamError> {
    let mut map = HashMap::with_capacity(dict.len());
    for (k, v) in dict {
        let value = packstream_to_gql_value(v, k)?;
        map.insert(k.clone(), value);
    }
    Ok(map)
}

/// Lowers a single [`PackStreamValue`] to a [`GqlValue`].
///
/// `key` is forwarded into the error for diagnostics; when recursing
/// into a list element, the caller appends `[i]` to disambiguate.
fn packstream_to_gql_value(v: &PackStreamValue, key: &str) -> Result<GqlValue, BoltParamError> {
    match v {
        PackStreamValue::Null => Ok(GqlValue::Null),
        PackStreamValue::Bool(b) => Ok(GqlValue::Bool(*b)),
        PackStreamValue::Int(i) => Ok(GqlValue::Int(*i)),
        PackStreamValue::Float(f) => Ok(GqlValue::Float(*f)),
        PackStreamValue::String(s) => Ok(GqlValue::Str(s.clone())),
        PackStreamValue::List(items) => {
            let mut out = Vec::with_capacity(items.len());
            for (i, item) in items.iter().enumerate() {
                let nested_key = format!("{key}[{i}]");
                out.push(packstream_to_gql_value(item, &nested_key)?);
            }
            Ok(GqlValue::List(out))
        }
        PackStreamValue::Bytes(_) => Err(BoltParamError::UnsupportedValueType {
            key: key.to_owned(),
            got: "Bytes",
        }),
        PackStreamValue::Dict(entries) => {
            let mut out = std::collections::HashMap::with_capacity(entries.len());
            for (k, dv) in entries {
                let nested_key = format!("{key}.{k}");
                out.insert(k.clone(), packstream_to_gql_value(dv, &nested_key)?);
            }
            Ok(GqlValue::Map(out))
        }
        PackStreamValue::Struct { .. } => Err(BoltParamError::UnsupportedValueType {
            key: key.to_owned(),
            got: "Struct",
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dict_with(pairs: &[(&str, PackStreamValue)]) -> BoltDict {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), v.clone()))
            .collect()
    }

    #[test]
    fn bolt_dict_to_value_map_int() {
        let d = dict_with(&[("x", PackStreamValue::Int(42))]);
        let map = bolt_dict_to_value_map(&d).unwrap();
        assert_eq!(map.get("x"), Some(&GqlValue::Int(42)));
    }

    #[test]
    fn bolt_dict_to_value_map_str() {
        let d = dict_with(&[("s", PackStreamValue::String("hi".into()))]);
        let map = bolt_dict_to_value_map(&d).unwrap();
        assert_eq!(map.get("s"), Some(&GqlValue::Str("hi".into())));
    }

    #[test]
    fn bolt_dict_to_value_map_float() {
        let d = dict_with(&[("f", PackStreamValue::Float(2.5))]);
        let map = bolt_dict_to_value_map(&d).unwrap();
        match map.get("f") {
            Some(GqlValue::Float(v)) => assert!((*v - 2.5).abs() < 1e-12),
            other => panic!("expected Float, got {other:?}"),
        }
    }

    #[test]
    fn bolt_dict_to_value_map_bool() {
        let d = dict_with(&[("b", PackStreamValue::Bool(true))]);
        let map = bolt_dict_to_value_map(&d).unwrap();
        assert_eq!(map.get("b"), Some(&GqlValue::Bool(true)));
    }

    #[test]
    fn bolt_dict_to_value_map_null() {
        let d = dict_with(&[("n", PackStreamValue::Null)]);
        let map = bolt_dict_to_value_map(&d).unwrap();
        assert_eq!(map.get("n"), Some(&GqlValue::Null));
    }

    #[test]
    fn bolt_dict_to_value_map_list_of_ints() {
        let d = dict_with(&[(
            "xs",
            PackStreamValue::List(vec![
                PackStreamValue::Int(1),
                PackStreamValue::Int(2),
                PackStreamValue::Int(3),
            ]),
        )]);
        let map = bolt_dict_to_value_map(&d).unwrap();
        assert_eq!(
            map.get("xs"),
            Some(&GqlValue::List(vec![
                GqlValue::Int(1),
                GqlValue::Int(2),
                GqlValue::Int(3),
            ])),
        );
    }

    #[test]
    fn bolt_dict_to_value_map_nested_list() {
        let d = dict_with(&[(
            "xs",
            PackStreamValue::List(vec![PackStreamValue::List(vec![PackStreamValue::Int(7)])]),
        )]);
        let map = bolt_dict_to_value_map(&d).unwrap();
        assert_eq!(
            map.get("xs"),
            Some(&GqlValue::List(vec![GqlValue::List(vec![GqlValue::Int(
                7
            )])])),
        );
    }

    #[test]
    fn bolt_dict_to_value_map_multiple_keys() {
        let d = dict_with(&[
            ("id", PackStreamValue::Int(99)),
            ("name", PackStreamValue::String("Alice".into())),
            ("active", PackStreamValue::Bool(true)),
        ]);
        let map = bolt_dict_to_value_map(&d).unwrap();
        assert_eq!(map.len(), 3);
        assert_eq!(map.get("id"), Some(&GqlValue::Int(99)));
        assert_eq!(map.get("name"), Some(&GqlValue::Str("Alice".into())));
        assert_eq!(map.get("active"), Some(&GqlValue::Bool(true)));
    }

    // ── Cycle 2.1: Dict lowers to GqlValue::Map ──────────────────────────────

    #[test]
    fn bolt_dict_to_value_map_dict_becomes_gql_map() {
        let d = dict_with(&[(
            "props",
            PackStreamValue::Dict(vec![
                ("name".to_owned(), PackStreamValue::String("Alice".into())),
                ("age".to_owned(), PackStreamValue::Int(30)),
            ]),
        )]);
        let map = bolt_dict_to_value_map(&d).unwrap();
        let inner = match map.get("props") {
            Some(GqlValue::Map(m)) => m,
            other => panic!("expected GqlValue::Map, got {other:?}"),
        };
        assert_eq!(inner.get("name"), Some(&GqlValue::Str("Alice".into())));
        assert_eq!(inner.get("age"), Some(&GqlValue::Int(30)));
    }

    #[test]
    fn bolt_dict_to_value_map_nested_dict_recursive() {
        let d = dict_with(&[(
            "outer",
            PackStreamValue::Dict(vec![(
                "inner".to_owned(),
                PackStreamValue::Dict(vec![("x".to_owned(), PackStreamValue::Int(7))]),
            )]),
        )]);
        let map = bolt_dict_to_value_map(&d).unwrap();
        let outer = match map.get("outer") {
            Some(GqlValue::Map(m)) => m,
            other => panic!("expected GqlValue::Map, got {other:?}"),
        };
        let inner = match outer.get("inner") {
            Some(GqlValue::Map(m)) => m,
            other => panic!("expected nested GqlValue::Map, got {other:?}"),
        };
        assert_eq!(inner.get("x"), Some(&GqlValue::Int(7)));
    }

    #[test]
    fn bolt_dict_to_value_map_dict_inside_list_becomes_gql_map() {
        let d = dict_with(&[(
            "xs",
            PackStreamValue::List(vec![PackStreamValue::Dict(vec![(
                "k".to_owned(),
                PackStreamValue::Bool(true),
            )])]),
        )]);
        let map = bolt_dict_to_value_map(&d).unwrap();
        match map.get("xs") {
            Some(GqlValue::List(items)) => match &items[0] {
                GqlValue::Map(m) => assert_eq!(m.get("k"), Some(&GqlValue::Bool(true))),
                other => panic!("expected Map inside List, got {other:?}"),
            },
            other => panic!("expected List, got {other:?}"),
        }
    }

    #[test]
    fn bolt_dict_to_value_map_bytes_value_returns_error() {
        let d = dict_with(&[("blob", PackStreamValue::Bytes(vec![0xDE, 0xAD]))]);
        let err = bolt_dict_to_value_map(&d).unwrap_err();
        assert_eq!(
            err,
            BoltParamError::UnsupportedValueType {
                key: "blob".to_owned(),
                got: "Bytes",
            },
        );
    }

    #[test]
    fn bolt_dict_to_value_map_struct_value_returns_error() {
        let d = dict_with(&[(
            "node",
            PackStreamValue::Struct {
                tag: 0x4E,
                fields: vec![PackStreamValue::Int(1)],
            },
        )]);
        let err = bolt_dict_to_value_map(&d).unwrap_err();
        assert_eq!(
            err,
            BoltParamError::UnsupportedValueType {
                key: "node".to_owned(),
                got: "Struct",
            },
        );
    }

    #[test]
    fn bolt_dict_to_value_map_unsupported_inside_list_carries_indexed_key() {
        // An unsupported value (`Bytes`) inside the list at position 2 should
        // produce an error whose `key` is `xs[2]` for diagnostics. (Dict is now
        // supported — lowered to a Map — so Bytes is the unsupported probe.)
        let d = dict_with(&[(
            "xs",
            PackStreamValue::List(vec![
                PackStreamValue::Int(1),
                PackStreamValue::Int(2),
                PackStreamValue::Bytes(vec![0x01]),
            ]),
        )]);
        let err = bolt_dict_to_value_map(&d).unwrap_err();
        assert_eq!(
            err,
            BoltParamError::UnsupportedValueType {
                key: "xs[2]".to_owned(),
                got: "Bytes",
            },
        );
    }

    #[test]
    fn bolt_dict_to_value_map_empty_returns_empty_map() {
        let d: BoltDict = vec![];
        let map = bolt_dict_to_value_map(&d).unwrap();
        assert!(map.is_empty());
    }
}
