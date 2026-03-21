// Copyright 2026 BelowZero Security OU. All rights reserved.

use crate::error::CliError;
use crate::output;

/// Format query results as GQL CREATE statements.
///
/// Each row is assumed to represent a node with properties from the columns.
#[must_use]
pub fn format_as_gql(columns: &[String], rows: &[Vec<serde_json::Value>]) -> String {
    use std::fmt::Write;

    let mut out = String::new();

    for row in rows {
        let mut props = Vec::new();
        for (i, col) in columns.iter().enumerate() {
            if let Some(val) = row.get(i) {
                if !val.is_null() {
                    let formatted = match val {
                        serde_json::Value::String(s) => {
                            format!("{col}: '{}'", s.replace('\'', "\\'"))
                        }
                        serde_json::Value::Number(n) => format!("{col}: {n}"),
                        serde_json::Value::Bool(b) => format!("{col}: {b}"),
                        _ => format!("{col}: '{val}'"),
                    };
                    props.push(formatted);
                }
            }
        }

        let props_str = if props.is_empty() {
            String::new()
        } else {
            format!(" {{{}}}", props.join(", "))
        };

        let _ = writeln!(out, "CREATE (n{props_str});");
    }

    out
}

/// Export query results in the specified format.
///
/// # Errors
///
/// Returns `CliError::ImportExport` on formatting failure.
pub fn format_export(
    format: &str,
    columns: &[String],
    rows: &[Vec<serde_json::Value>],
) -> Result<String, CliError> {
    match format {
        "gql" => Ok(format_as_gql(columns, rows)),
        "json" => Ok(output::json::render(columns, rows)),
        "csv" => Ok(output::csv::render(columns, rows, true)),
        other => Err(CliError::ImportExport(format!(
            "unsupported export format: {other}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gql_export_single_node() {
        let cols = vec!["name".to_owned(), "age".to_owned()];
        let rows = vec![vec![
            serde_json::json!("Alice"),
            serde_json::json!(30),
        ]];
        let out = format_as_gql(&cols, &rows);
        assert!(out.contains("CREATE (n {name: 'Alice', age: 30})"));
        assert!(out.ends_with(";\n"));
    }

    #[test]
    fn gql_export_multiple_nodes() {
        let cols = vec!["name".to_owned()];
        let rows = vec![
            vec![serde_json::json!("Alice")],
            vec![serde_json::json!("Bob")],
        ];
        let out = format_as_gql(&cols, &rows);
        assert_eq!(out.lines().count(), 2);
    }

    #[test]
    fn gql_export_null_properties_omitted() {
        let cols = vec!["name".to_owned(), "age".to_owned()];
        let rows = vec![vec![serde_json::json!("Alice"), serde_json::Value::Null]];
        let out = format_as_gql(&cols, &rows);
        assert!(out.contains("name: 'Alice'"));
        assert!(!out.contains("age"));
    }

    #[test]
    fn gql_export_empty_rows() {
        let out = format_as_gql(&["n".to_owned()], &[]);
        assert!(out.is_empty());
    }

    #[test]
    fn gql_export_bool_property() {
        let cols = vec!["active".to_owned()];
        let rows = vec![vec![serde_json::json!(true)]];
        let out = format_as_gql(&cols, &rows);
        assert!(out.contains("active: true"));
    }

    #[test]
    fn gql_export_string_with_quote() {
        let cols = vec!["name".to_owned()];
        let rows = vec![vec![serde_json::json!("O'Brien")]];
        let out = format_as_gql(&cols, &rows);
        assert!(out.contains("O\\'Brien"));
    }

    #[test]
    fn format_export_gql() {
        let cols = vec!["x".to_owned()];
        let rows = vec![vec![serde_json::json!(1)]];
        let out = format_export("gql", &cols, &rows).expect("export"); // OK: test
        assert!(out.contains("CREATE"));
    }

    #[test]
    fn format_export_json() {
        let cols = vec!["x".to_owned()];
        let rows = vec![vec![serde_json::json!(1)]];
        let out = format_export("json", &cols, &rows).expect("export"); // OK: test
        let parsed: serde_json::Value = serde_json::from_str(out.trim()).expect("json"); // OK: test
        assert_eq!(parsed["x"], 1);
    }

    #[test]
    fn format_export_csv() {
        let cols = vec!["x".to_owned()];
        let rows = vec![vec![serde_json::json!(1)]];
        let out = format_export("csv", &cols, &rows).expect("export"); // OK: test
        assert!(out.starts_with('x'));
    }

    #[test]
    fn format_export_unknown_is_error() {
        let result = format_export("xml", &[], &[]);
        assert!(result.is_err());
    }
}
