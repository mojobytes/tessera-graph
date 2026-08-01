// SPDX-License-Identifier: LicenseRef-TesseraGraph-Proprietary

//! `PyPatternBuilder` — Python wrapper for `PatternBuilder`.
//!
//! Stores pattern steps as an internal IR and reconstructs the Rust
//! `PatternBuilder` in `execute()`.

use pyo3::prelude::*;

use crate::errors::to_py_err;
use crate::graph::PyGraph;
use crate::types::direction::{self, PyDirection};
use crate::types::pattern_match::PyPatternMatch;
use crate::types::properties;

/// Internal representation of a single pattern step.
#[derive(Clone)]
enum Step {
    Node { var: String },
    Edge { var: Option<String>, direction: PyDirection },
    Label(String),
    WhereProp { key: String, value: tessera_graph::Property },
    WhereEdgeProp { key: String, value: tessera_graph::Property },
}

/// Builder for graph pattern queries.
#[pyclass(name = "PatternBuilder")]
pub struct PyPatternBuilder {
    graph: Py<PyGraph>,
    steps: Vec<Step>,
}

impl PyPatternBuilder {
    pub fn new(graph: Py<PyGraph>) -> Self {
        Self {
            graph,
            steps: Vec::new(),
        }
    }
}

#[pymethods]
impl PyPatternBuilder {
    /// Adds a node step with the given variable name.
    fn node(mut slf: PyRefMut<'_, Self>, var: &str) -> Py<Self> {
        slf.steps.push(Step::Node { var: var.to_owned() });
        slf.into()
    }

    /// Adds an unnamed edge step. Accepts `Direction` enum or string.
    fn edge(mut slf: PyRefMut<'_, Self>, direction: &Bound<'_, PyAny>) -> PyResult<Py<Self>> {
        let d = direction::coerce(direction)?;
        slf.steps.push(Step::Edge { var: None, direction: d });
        Ok(slf.into())
    }

    /// Adds a named edge step. Accepts `Direction` enum or string.
    fn edge_var(
        mut slf: PyRefMut<'_, Self>,
        var: &str,
        direction: &Bound<'_, PyAny>,
    ) -> PyResult<Py<Self>> {
        let d = direction::coerce(direction)?;
        slf.steps.push(Step::Edge {
            var: Some(var.to_owned()),
            direction: d,
        });
        Ok(slf.into())
    }

    /// Sets the label constraint on the last step.
    fn label(mut slf: PyRefMut<'_, Self>, label: &str) -> Py<Self> {
        slf.steps.push(Step::Label(label.to_owned()));
        slf.into()
    }

    /// Adds a property filter to the last node step.
    fn where_prop(
        mut slf: PyRefMut<'_, Self>,
        key: &str,
        value: &Bound<'_, PyAny>,
    ) -> PyResult<Py<Self>> {
        let prop = properties::py_to_property(value)?;
        slf.steps.push(Step::WhereProp {
            key: key.to_owned(),
            value: prop,
        });
        Ok(slf.into())
    }

    /// Adds a property filter to the last edge step.
    fn where_edge_prop(
        mut slf: PyRefMut<'_, Self>,
        key: &str,
        value: &Bound<'_, PyAny>,
    ) -> PyResult<Py<Self>> {
        let prop = properties::py_to_property(value)?;
        slf.steps.push(Step::WhereEdgeProp {
            key: key.to_owned(),
            value: prop,
        });
        Ok(slf.into())
    }

    /// Executes the pattern and returns all matches.
    fn execute(&self, py: Python<'_>) -> PyResult<Vec<PyPatternMatch>> {
        let g = self.graph.borrow(py);
        let mut b = g.inner.pattern();
        let mut node_vars = Vec::new();
        let mut edge_vars = Vec::new();
        for step in &self.steps {
            match step {
                Step::Node { var } => {
                    b = b.node(var.as_str());
                    node_vars.push(var.clone());
                }
                Step::Edge { var: None, direction } => b = b.edge((*direction).into()),
                Step::Edge {
                    var: Some(v),
                    direction,
                } => {
                    b = b.edge_var(v.as_str(), (*direction).into());
                    edge_vars.push(v.clone());
                }
                Step::Label(l) => b = b.label(l.as_str()),
                Step::WhereProp { key, value } => b = b.where_prop(key.as_str(), value.clone()),
                Step::WhereEdgeProp { key, value } => {
                    b = b.where_edge_prop(key.as_str(), value.clone());
                }
            }
        }
        // Eager collection: PyO3 requires owned Python-managed objects — Rust
        // iterators cannot cross the Python/Rust boundary as lazy references.
        // A future PyPatternMatchIter (__iter__/__next__) would avoid this
        // allocation for large result sets.
        let matches: Vec<tessera_graph::PatternMatch> = b
            .execute()
            .map_err(to_py_err)?
            .collect::<tessera_graph::Result<Vec<_>>>()
            .map_err(to_py_err)?;
        Ok(matches
            .into_iter()
            .map(|m| PyPatternMatch {
                inner: m,
                node_vars: node_vars.clone(),
                edge_vars: edge_vars.clone(),
            })
            .collect())
    }
}
