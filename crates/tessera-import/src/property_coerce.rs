// Copyright 2026 BelowZero Security OU. All rights reserved.

use tessera_graph::Property;

use crate::error::{ExportError, ExportResult};

/// Coerce a raw string to the most specific Property type.
/// Priority: i64 → f64 → bool → String.
/// NaN and infinity are rejected as f64 — they fall through to String.
pub fn coerce_str_value(raw: &str) -> Property {
    if let Ok(i) = raw.parse::<i64>() {
        return Property::I64(i);
    }
    if let Ok(f) = raw.parse::<f64>() {
        if f.is_finite() {
            return Property::F64(f);
        }
        // NaN / infinity: fall through to String
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
///
/// # Errors
///
/// Returns [`ExportError::UnsupportedType`] for [`Property::Bytes`], which has
/// no meaningful JSON representation.
pub fn property_to_json(p: &Property) -> ExportResult<serde_json::Value> {
    match p {
        Property::String(s) => Ok(serde_json::Value::String(s.clone())),
        Property::I64(i) => Ok(serde_json::json!(i)),
        Property::F64(f) => Ok(serde_json::json!(f)),
        Property::Bool(b) => Ok(serde_json::Value::Bool(*b)),
        Property::Bytes(_) => Err(ExportError::UnsupportedType {
            context: "json export".to_owned(),
            type_name: "Bytes".to_owned(),
        }),
    }
}

/// Convert a `Property` to a GQL literal string.
///
/// # Errors
///
/// Returns [`ExportError::UnsupportedType`] for [`Property::Bytes`], which has
/// no meaningful GQL literal representation.
pub fn property_to_gql_literal(p: &Property) -> ExportResult<String> {
    match p {
        Property::String(s) => Ok(format!("'{}'", s.replace('\'', "\\'"))),
        Property::I64(i) => Ok(i.to_string()),
        Property::F64(f) => Ok(f.to_string()),
        Property::Bool(b) => Ok(b.to_string()),
        Property::Bytes(_) => Err(ExportError::UnsupportedType {
            context: "gql export".to_owned(),
            type_name: "Bytes".to_owned(),
        }),
    }
}

/// Validate that a property key matches `[a-zA-Z_][a-zA-Z0-9_]*`.
///
/// # Errors
///
/// Returns [`ExportError::InvalidPropertyKey`] if the key is empty or contains
/// characters outside the allowed set.
pub fn validate_property_key(key: &str) -> Result<(), ExportError> {
    let mut chars = key.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return Err(ExportError::InvalidPropertyKey(key.to_owned())),
    }
    for c in chars {
        if !c.is_ascii_alphanumeric() && c != '_' {
            return Err(ExportError::InvalidPropertyKey(key.to_owned()));
        }
    }
    Ok(())
}
