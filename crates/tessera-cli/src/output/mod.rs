// Copyright 2026 BelowZero Security OU. All rights reserved.

pub mod csv;
pub mod json;
pub mod table;

use std::fmt;
use std::str::FromStr;
use std::time::Duration;

use tessera_protocol::packstream::PackStreamValue;

use crate::error::CliError;

/// Output format for query results.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    Table,
    Json,
    Csv,
}

impl FromStr for OutputFormat {
    type Err = CliError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "table" => Ok(Self::Table),
            "json" => Ok(Self::Json),
            "csv" => Ok(Self::Csv),
            other => Err(CliError::Config(format!("unknown output format: {other}"))),
        }
    }
}

impl fmt::Display for OutputFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Table => write!(f, "table"),
            Self::Json => write!(f, "json"),
            Self::Csv => write!(f, "csv"),
        }
    }
}

/// Render query results in the specified format.
///
/// # Errors
///
/// Returns `CliError::Query` if formatting fails.
pub fn render(
    format: OutputFormat,
    columns: &[String],
    rows: &[Vec<PackStreamValue>],
    elapsed: Option<Duration>,
    include_headers: bool,
) -> Result<String, CliError> {
    match format {
        OutputFormat::Table => Ok(table::render(columns, rows, elapsed)),
        OutputFormat::Json => Ok(json::render(columns, rows)),
        OutputFormat::Csv => Ok(csv::render(columns, rows, include_headers)),
    }
}

/// Convert a `PackStreamValue` to a display string for table/CSV output.
#[must_use]
pub fn value_to_display(v: &PackStreamValue) -> String {
    match v {
        PackStreamValue::Null => String::new(),
        PackStreamValue::Bool(b) => b.to_string(),
        PackStreamValue::Int(i) => i.to_string(),
        PackStreamValue::Float(f) => f.to_string(),
        PackStreamValue::String(s) => s.clone(),
        PackStreamValue::List(items) => {
            let inner: Vec<String> = items.iter().map(value_to_display).collect();
            format!("[{}]", inner.join(", "))
        }
        PackStreamValue::Dict(pairs) => {
            let inner: Vec<String> = pairs
                .iter()
                .map(|(k, v)| format!("{k}: {}", value_to_display(v)))
                .collect();
            format!("{{{}}}", inner.join(", "))
        }
        PackStreamValue::Bytes(b) => format!("<{} bytes>", b.len()),
        PackStreamValue::Struct { tag, fields } => {
            let inner: Vec<String> = fields.iter().map(value_to_display).collect();
            format!("<struct 0x{tag:02X} [{inner}]>", inner = inner.join(", "))
        }
    }
}

/// Convert a `PackStreamValue` to a `serde_json::Value` for JSON output.
#[must_use]
pub fn value_to_json(v: &PackStreamValue) -> serde_json::Value {
    match v {
        PackStreamValue::Null => serde_json::Value::Null,
        PackStreamValue::Bool(b) => serde_json::Value::Bool(*b),
        PackStreamValue::Int(i) => serde_json::json!(*i),
        PackStreamValue::Float(f) => serde_json::json!(*f),
        PackStreamValue::String(s) => serde_json::Value::String(s.clone()),
        PackStreamValue::List(items) => {
            serde_json::Value::Array(items.iter().map(value_to_json).collect())
        }
        PackStreamValue::Dict(pairs) => {
            let map: serde_json::Map<String, serde_json::Value> = pairs
                .iter()
                .map(|(k, v)| (k.clone(), value_to_json(v)))
                .collect();
            serde_json::Value::Object(map)
        }
        PackStreamValue::Bytes(b) => serde_json::json!(format!("<{} bytes>", b.len())),
        PackStreamValue::Struct { tag, fields } => {
            serde_json::json!({
                "tag": format!("0x{tag:02X}"),
                "fields": fields.iter().map(value_to_json).collect::<Vec<_>>(),
            })
        }
    }
}

/// Pluralized label for row count.
#[must_use]
pub const fn rows_label(n: usize) -> &'static str {
    if n == 1 { "row" } else { "rows" }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_table_format() {
        let fmt: OutputFormat = "table".parse().expect("parse"); // OK: test
        assert_eq!(fmt, OutputFormat::Table);
    }

    #[test]
    fn parse_json_format() {
        let fmt: OutputFormat = "json".parse().expect("parse"); // OK: test
        assert_eq!(fmt, OutputFormat::Json);
    }

    #[test]
    fn parse_csv_format() {
        let fmt: OutputFormat = "csv".parse().expect("parse"); // OK: test
        assert_eq!(fmt, OutputFormat::Csv);
    }

    #[test]
    fn parse_case_insensitive() {
        let fmt: OutputFormat = "JSON".parse().expect("parse"); // OK: test
        assert_eq!(fmt, OutputFormat::Json);
    }

    #[test]
    fn unknown_format_is_error() {
        let result: Result<OutputFormat, _> = "xml".parse();
        assert!(result.is_err());
    }

    #[test]
    fn display_roundtrips() {
        for fmt in [OutputFormat::Table, OutputFormat::Json, OutputFormat::Csv] {
            let s = fmt.to_string();
            let parsed: OutputFormat = s.parse().expect("roundtrip"); // OK: test
            assert_eq!(parsed, fmt);
        }
    }

    #[test]
    fn rows_label_singular() {
        assert_eq!(rows_label(1), "row");
    }

    #[test]
    fn rows_label_plural() {
        assert_eq!(rows_label(0), "rows");
        assert_eq!(rows_label(2), "rows");
    }

    #[test]
    fn value_to_display_null() {
        assert_eq!(value_to_display(&PackStreamValue::Null), "");
    }

    #[test]
    fn value_to_display_string() {
        let v = PackStreamValue::String("hello".to_owned());
        assert_eq!(value_to_display(&v), "hello");
    }

    #[test]
    fn value_to_display_int() {
        assert_eq!(value_to_display(&PackStreamValue::Int(42)), "42");
    }

    #[test]
    fn value_to_display_float() {
        assert_eq!(value_to_display(&PackStreamValue::Float(3.125)), "3.125");
    }

    #[test]
    fn value_to_display_bool() {
        assert_eq!(value_to_display(&PackStreamValue::Bool(true)), "true");
    }

    #[test]
    fn value_to_display_list() {
        let v = PackStreamValue::List(vec![
            PackStreamValue::Int(1),
            PackStreamValue::Int(2),
        ]);
        assert_eq!(value_to_display(&v), "[1, 2]");
    }

    #[test]
    fn value_to_json_converts_correctly() {
        assert_eq!(value_to_json(&PackStreamValue::Null), serde_json::Value::Null);
        assert_eq!(value_to_json(&PackStreamValue::Int(42)), serde_json::json!(42));
        assert_eq!(
            value_to_json(&PackStreamValue::String("hello".to_owned())),
            serde_json::json!("hello")
        );
    }

    #[test]
    fn render_dispatches_to_table() {
        let cols = vec!["x".to_owned()];
        let rows = vec![vec![PackStreamValue::Int(1)]];
        let out = render(OutputFormat::Table, &cols, &rows, None, true).expect("render"); // OK: test
        assert!(out.contains("1 row"));
    }

    #[test]
    fn render_dispatches_to_json() {
        let cols = vec!["x".to_owned()];
        let rows = vec![vec![PackStreamValue::Int(1)]];
        let out = render(OutputFormat::Json, &cols, &rows, None, true).expect("render"); // OK: test
        let parsed: serde_json::Value = serde_json::from_str(out.trim()).expect("json"); // OK: test
        assert_eq!(parsed["x"], 1);
    }

    #[test]
    fn render_dispatches_to_csv() {
        let cols = vec!["x".to_owned()];
        let rows = vec![vec![PackStreamValue::Int(1)]];
        let out = render(OutputFormat::Csv, &cols, &rows, None, true).expect("render"); // OK: test
        assert!(out.starts_with('x'));
    }
}
