// SPDX-License-Identifier: LicenseRef-TesseraGraph-Proprietary

use pyo3::prelude::*;

use crate::errors;

/// Graph traversal direction.
#[pyclass(name = "Direction", frozen, eq, hash)]
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub enum PyDirection {
    #[pyo3(name = "OUTGOING")]
    Outgoing,
    #[pyo3(name = "INCOMING")]
    Incoming,
    #[pyo3(name = "BOTH")]
    Both,
}

#[pymethods]
impl PyDirection {
    /// Parses a direction from a case-insensitive string.
    #[staticmethod]
    fn from_str(s: &str) -> PyResult<Self> {
        match s.to_ascii_lowercase().as_str() {
            "outgoing" => Ok(Self::Outgoing),
            "incoming" => Ok(Self::Incoming),
            "both" => Ok(Self::Both),
            _ => Err(errors::TesseraError::new_err(format!(
                "invalid direction: {s:?} (expected outgoing, incoming, or both)"
            ))),
        }
    }

    fn __repr__(&self) -> &'static str {
        match self {
            Self::Outgoing => "Direction.OUTGOING",
            Self::Incoming => "Direction.INCOMING",
            Self::Both => "Direction.BOTH",
        }
    }
}

impl From<PyDirection> for tessera_graph::Direction {
    fn from(d: PyDirection) -> Self {
        match d {
            PyDirection::Outgoing => Self::Outgoing,
            PyDirection::Incoming => Self::Incoming,
            PyDirection::Both => Self::Both,
        }
    }
}

/// Accepts either a `Direction` enum or a case-insensitive string and returns `PyDirection`.
pub fn coerce(val: &Bound<'_, PyAny>) -> PyResult<PyDirection> {
    if let Ok(d) = val.extract::<PyDirection>() {
        return Ok(d);
    }
    if let Ok(s) = val.extract::<String>() {
        return PyDirection::from_str(&s);
    }
    Err(pyo3::exceptions::PyTypeError::new_err(
        "expected Direction enum or string (outgoing, incoming, both)",
    ))
}
