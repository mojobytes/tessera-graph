// SPDX-License-Identifier: MIT

//! Exception hierarchy for the `tessera_graph` Python module.
//!
//! - `TesseraError` — base exception for all tessera-graph errors.
//! - `NodeNotFoundError(TesseraError)` — raised when a node ID does not exist.
//! - `EdgeNotFoundError(TesseraError)` — raised when an edge ID does not exist.
//! - `GqlSyntaxError(TesseraError)` — raised on malformed GQL input.

use pyo3::create_exception;
use pyo3::prelude::*;

create_exception!(tessera_graph, TesseraError, pyo3::exceptions::PyException);
create_exception!(tessera_graph, NodeNotFoundError, TesseraError);
create_exception!(tessera_graph, EdgeNotFoundError, TesseraError);
create_exception!(tessera_graph, GqlSyntaxError, TesseraError);

/// Converts a `tessera_graph::Error` into the appropriate Python exception.
pub fn to_py_err(err: tessera_graph::Error) -> PyErr {
    match &err {
        tessera_graph::Error::NodeNotFound(_) => NodeNotFoundError::new_err(err.to_string()),
        tessera_graph::Error::EdgeNotFound(_) => EdgeNotFoundError::new_err(err.to_string()),
        tessera_graph::Error::GqlSyntaxError { .. } => GqlSyntaxError::new_err(err.to_string()),
        _ => TesseraError::new_err(err.to_string()),
    }
}

/// Registers all exception types in the given Python module.
pub fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add("TesseraError", m.py().get_type::<TesseraError>())?;
    m.add("NodeNotFoundError", m.py().get_type::<NodeNotFoundError>())?;
    m.add("EdgeNotFoundError", m.py().get_type::<EdgeNotFoundError>())?;
    m.add("GqlSyntaxError", m.py().get_type::<GqlSyntaxError>())?;
    Ok(())
}
