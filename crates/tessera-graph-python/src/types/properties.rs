// SPDX-License-Identifier: MIT

//! Conversion helpers between Python `dict[str, Any]` and Rust `Properties`.

use pyo3::prelude::*;
use pyo3::types::{PyBool, PyBytes, PyDict, PyFloat, PyInt, PyString};
use tessera_graph::{Properties, Property};

/// Converts a Rust `Properties` map into a Python `dict[str, Any]`.
pub fn to_py_dict<'py>(py: Python<'py>, props: &Properties) -> PyResult<Bound<'py, PyDict>> {
    let dict = PyDict::new(py);
    for (key, val) in props {
        let py_val: Py<PyAny> = match val {
            Property::String(s) => s.into_pyobject(py)?.into_any().unbind(),
            Property::I64(v) => v.into_pyobject(py)?.into_any().unbind(),
            Property::F64(v) => v.into_pyobject(py)?.into_any().unbind(),
            Property::Bool(b) => b.into_pyobject(py)?.to_owned().into_any().unbind(),
            Property::Bytes(b) => PyBytes::new(py, b).into_any().unbind(),
        };
        dict.set_item(key, py_val)?;
    }
    Ok(dict)
}

/// Converts a Python `dict[str, Any]` into a Rust `Properties` map.
///
/// Supported Python types: `str`, `int`, `float`, `bool`, `bytes`.
/// Raises `TypeError` for unsupported value types.
pub fn from_py_dict(dict: &Bound<'_, PyDict>) -> PyResult<Properties> {
    let mut props = Properties::new();
    for (key, val) in dict.iter() {
        let key_str: String = key.extract()?;
        let prop = py_to_property(&val)?;
        props.insert(key_str, prop);
    }
    Ok(props)
}

/// Converts a single Python object into a `Property`.
///
/// Must check `bool` before `int` because in Python `bool` is a subclass of `int`.
pub fn py_to_property(val: &Bound<'_, PyAny>) -> PyResult<Property> {
    // bool before int — Python bool is a subclass of int
    if val.is_instance_of::<PyBool>() {
        return Ok(Property::Bool(val.extract::<bool>()?));
    }
    if val.is_instance_of::<PyInt>() {
        return Ok(Property::I64(val.extract::<i64>()?));
    }
    if val.is_instance_of::<PyFloat>() {
        return Ok(Property::F64(val.extract::<f64>()?));
    }
    if val.is_instance_of::<PyString>() {
        return Ok(Property::String(val.extract::<String>()?));
    }
    if val.is_instance_of::<PyBytes>() {
        return Ok(Property::Bytes(val.extract::<Vec<u8>>()?));
    }
    Err(pyo3::exceptions::PyTypeError::new_err(format!(
        "unsupported property type: {}",
        val.get_type().name()?
    )))
}
