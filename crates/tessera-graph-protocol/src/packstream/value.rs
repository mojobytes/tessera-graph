// Copyright 2026 BelowZero Security OU. All rights reserved.

//! `PackStreamValue` — the in-memory representation of PackStream-encoded data.

use std::fmt;

/// A `PackStream` value — the binary serialization format used by the Neo4j Bolt protocol.
#[derive(Debug, Clone, PartialEq)]
pub enum PackStreamValue {
    /// Null / absent value.
    Null,
    /// Boolean.
    Bool(bool),
    /// 64-bit signed integer.
    Int(i64),
    /// 64-bit IEEE 754 floating-point.
    Float(f64),
    /// UTF-8 string.
    String(String),
    /// Raw byte array.
    Bytes(Vec<u8>),
    /// Ordered list of values.
    List(Vec<Self>),
    /// Ordered key-value map (keys are always strings).
    Dict(Vec<(String, Self)>),
    /// Tagged structure with an arbitrary field list.
    Struct {
        /// Structure tag byte identifying the concrete type.
        tag: u8,
        /// Ordered list of field values.
        fields: Vec<Self>,
    },
}

impl fmt::Display for PackStreamValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Null => write!(f, "null"),
            Self::Bool(b) => write!(f, "{b}"),
            Self::Int(i) => write!(f, "{i}"),
            Self::Float(v) => write!(f, "{v}"),
            Self::String(s) => write!(f, "\"{s}\""),
            Self::Bytes(b) => {
                write!(f, "bytes[")?;
                for (i, byte) in b.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "0x{byte:02X}")?;
                }
                write!(f, "]")
            }
            Self::List(items) => {
                write!(f, "[")?;
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{item}")?;
                }
                write!(f, "]")
            }
            Self::Dict(pairs) => {
                write!(f, "{{")?;
                for (i, (k, v)) in pairs.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "\"{k}\": {v}")?;
                }
                write!(f, "}}")
            }
            Self::Struct { tag, fields } => {
                write!(f, "Struct(0x{tag:02X})[")?;
                for (i, field) in fields.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{field}")?;
                }
                write!(f, "]")
            }
        }
    }
}
