// SPDX-License-Identifier: MIT

//! Exception hierarchy for the `ermya_graph` Python module.
//!
//! - `ErmyaError` — base exception for all ermya-graph errors.
//! - `NodeNotFoundError(ErmyaError)` — raised when a node ID does not exist.
//! - `EdgeNotFoundError(ErmyaError)` — raised when an edge ID does not exist.
//! - `GqlSyntaxError(ErmyaError)` — raised on malformed GQL input.

use pyo3::create_exception;
use pyo3::prelude::*;

create_exception!(ermya_graph, ErmyaError, pyo3::exceptions::PyException);
create_exception!(ermya_graph, NodeNotFoundError, ErmyaError);
create_exception!(ermya_graph, EdgeNotFoundError, ErmyaError);
create_exception!(ermya_graph, GqlSyntaxError, ErmyaError);

/// Converts a `ermya_graph::Error` into the appropriate Python exception.
pub fn to_py_err(err: ermya_graph::Error) -> PyErr {
    match &err {
        ermya_graph::Error::NodeNotFound(_) => NodeNotFoundError::new_err(err.to_string()),
        ermya_graph::Error::EdgeNotFound(_) => EdgeNotFoundError::new_err(err.to_string()),
        ermya_graph::Error::GqlSyntaxError { .. } => GqlSyntaxError::new_err(err.to_string()),
        _ => ErmyaError::new_err(err.to_string()),
    }
}

/// Registers all exception types in the given Python module.
pub fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add("ErmyaError", m.py().get_type::<ErmyaError>())?;
    m.add("NodeNotFoundError", m.py().get_type::<NodeNotFoundError>())?;
    m.add("EdgeNotFoundError", m.py().get_type::<EdgeNotFoundError>())?;
    m.add("GqlSyntaxError", m.py().get_type::<GqlSyntaxError>())?;
    Ok(())
}
