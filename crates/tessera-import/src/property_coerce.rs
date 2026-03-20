// Copyright 2026 BelowZero Security OU. All rights reserved.

use tessera_graph::Property;

/// Coerce a raw string to the most specific Property type.
/// Priority: i64 → f64 → bool → String.
pub fn coerce_str_value(raw: &str) -> Property {
    if let Ok(i) = raw.parse::<i64>() {
        return Property::I64(i);
    }
    if let Ok(f) = raw.parse::<f64>() {
        return Property::F64(f);
    }
    match raw {
        "true" => Property::Bool(true),
        "false" => Property::Bool(false),
        _ => Property::String(raw.to_owned()),
    }
}

/// Convert a `serde_json::Value` to a `Property`.
pub fn json_value_to_property(v: &serde_json::Value) -> Property {
    match v {
        serde_json::Value::Number(n) => n.as_i64().map_or_else(
            || {
                n.as_f64()
                    .map_or_else(|| Property::String(n.to_string()), Property::F64)
            },
            Property::I64,
        ),
        serde_json::Value::Bool(b) => Property::Bool(*b),
        serde_json::Value::String(s) => Property::String(s.clone()),
        other => Property::String(other.to_string()),
    }
}

/// Convert a `Property` to a `serde_json::Value`.
pub fn property_to_json(p: &Property) -> serde_json::Value {
    match p {
        Property::String(s) => serde_json::Value::String(s.clone()),
        Property::I64(i) => serde_json::json!(i),
        Property::F64(f) => serde_json::json!(f),
        Property::Bool(b) => serde_json::Value::Bool(*b),
        Property::Bytes(bytes) => serde_json::Value::String(format!("[{} bytes]", bytes.len())),
    }
}

/// Convert a `Property` to a GQL literal string.
pub fn property_to_gql_literal(p: &Property) -> String {
    match p {
        Property::String(s) => format!("'{}'", s.replace('\'', "\\'")),
        Property::I64(i) => i.to_string(),
        Property::F64(f) => f.to_string(),
        Property::Bool(b) => b.to_string(),
        Property::Bytes(_) => "'[bytes]'".to_owned(),
    }
}
