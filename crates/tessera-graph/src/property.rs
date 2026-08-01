// SPDX-License-Identifier: Apache-2.0

use std::collections::HashMap;
use std::fmt;

/// A dynamically-typed property value stored on nodes and edges.
#[derive(Debug, Clone, PartialEq)]
pub enum Property {
    String(String),
    I64(i64),
    F64(f64),
    Bool(bool),
    Bytes(Vec<u8>),
}

impl Property {
    /// Estimated heap bytes this value owns beyond its enum discriminant, used
    /// by the MVCC per-transaction memory cap. Scalars own no heap (0);
    /// `String`/`Bytes` own their length. This is a lower bound on real
    /// allocation (it ignores capacity slack), which is the right bias for a
    /// defensive cap: it never under-charges the payload itself.
    #[must_use]
    pub fn approx_heap_size(&self) -> usize {
        match self {
            Self::String(s) => s.len(),
            Self::Bytes(b) => b.len(),
            Self::I64(_) | Self::F64(_) | Self::Bool(_) => 0,
        }
    }

    /// Returns the string value if this is a `String` variant.
    #[must_use]
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(s) => Some(s),
            _ => None,
        }
    }

    /// Returns the `i64` value if this is an `I64` variant.
    #[must_use]
    pub const fn as_i64(&self) -> Option<i64> {
        match self {
            Self::I64(v) => Some(*v),
            _ => None,
        }
    }

    /// Returns the `f64` value if this is an `F64` variant.
    #[must_use]
    pub const fn as_f64(&self) -> Option<f64> {
        match self {
            Self::F64(v) => Some(*v),
            _ => None,
        }
    }

    /// Returns the `bool` value if this is a `Bool` variant.
    #[must_use]
    pub const fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Bool(v) => Some(*v),
            _ => None,
        }
    }

    /// Returns the byte slice if this is a `Bytes` variant.
    #[must_use]
    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Self::Bytes(v) => Some(v),
            _ => None,
        }
    }
}

/// A map of named properties attached to a node or edge.
pub type Properties = HashMap<String, Property>;

// ---------------------------------------------------------------------------
// Ergonomic conversions: `From<T>` for common Rust types
// ---------------------------------------------------------------------------

impl From<&str> for Property {
    fn from(v: &str) -> Self {
        Self::String(v.to_owned())
    }
}

impl From<String> for Property {
    fn from(v: String) -> Self {
        Self::String(v)
    }
}

impl From<i64> for Property {
    fn from(v: i64) -> Self {
        Self::I64(v)
    }
}

impl From<i32> for Property {
    fn from(v: i32) -> Self {
        Self::I64(i64::from(v))
    }
}

impl From<f64> for Property {
    fn from(v: f64) -> Self {
        Self::F64(v)
    }
}

impl From<bool> for Property {
    fn from(v: bool) -> Self {
        Self::Bool(v)
    }
}

impl From<Vec<u8>> for Property {
    fn from(v: Vec<u8>) -> Self {
        Self::Bytes(v)
    }
}

impl fmt::Display for Property {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::String(s) => write!(f, "{s}"),
            Self::I64(v) => write!(f, "{v}"),
            Self::F64(v) => write!(f, "{v}"),
            Self::Bool(v) => write!(f, "{v}"),
            Self::Bytes(v) => write!(f, "[{} bytes]", v.len()),
        }
    }
}

/// Convenience macro to build a `Properties` map inline.
///
/// # Examples
///
/// ```
/// use tessera_graph::props;
///
/// let p = props! { "name" => "Solar Plant A", "country" => "ES" };
/// assert_eq!(p.len(), 2);
/// ```
#[macro_export]
macro_rules! props {
    {} => {
        ::std::collections::HashMap::<String, $crate::Property>::new()
    };
    { $($key:expr => $val:expr),+ $(,)? } => {{
        let mut map = ::std::collections::HashMap::<String, $crate::Property>::new();
        $( map.insert($key.to_string(), $crate::Property::from($val)); )+
        map
    }};
}

#[cfg(test)]
mod tests {
    use super::Property;

    #[test]
    fn approx_heap_size_counts_only_owned_bytes() {
        assert_eq!(Property::String("hello".into()).approx_heap_size(), 5);
        assert_eq!(Property::Bytes(vec![0u8; 12]).approx_heap_size(), 12);
        assert_eq!(Property::I64(42).approx_heap_size(), 0);
        assert_eq!(Property::F64(1.5).approx_heap_size(), 0);
        assert_eq!(Property::Bool(true).approx_heap_size(), 0);
    }
}
