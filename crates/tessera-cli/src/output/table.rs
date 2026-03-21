// Copyright 2026 BelowZero Security OU. All rights reserved.

use std::time::Duration;

use comfy_table::Table;

use super::{rows_label, value_to_display};

/// Render query results as an ASCII table with optional timing footer.
#[must_use]
pub fn render(
    columns: &[String],
    rows: &[Vec<serde_json::Value>],
    elapsed: Option<Duration>,
) -> String {
    let mut table = Table::new();
    table.set_header(columns);

    for row in rows {
        let cells: Vec<String> = row.iter().map(value_to_display).collect();
        table.add_row(cells);
    }

    let n = rows.len();
    let footer = elapsed.map_or_else(
        || format!("{n} {}", rows_label(n)),
        |d| format!("{n} {} ({:.1} ms)", rows_label(n), d.as_secs_f64() * 1000.0),
    );

    format!("{table}\n{footer}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_result_shows_zero_rows() {
        let cols = vec!["name".to_owned(), "age".to_owned()];
        let rows: Vec<Vec<serde_json::Value>> = vec![];
        let out = render(&cols, &rows, None);
        assert!(out.contains("0 rows"));
        assert!(out.contains("name"));
        assert!(out.contains("age"));
    }

    #[test]
    fn single_row_renders_values() {
        let cols = vec!["name".to_owned()];
        let rows = vec![vec![serde_json::Value::String("Alice".to_owned())]];
        let out = render(&cols, &rows, None);
        assert!(out.contains("Alice"));
        assert!(out.contains("1 row"));
        // singular "row", not "rows"
        assert!(!out.contains("1 rows"));
    }

    #[test]
    fn multiple_rows() {
        let cols = vec!["n".to_owned()];
        let rows = vec![vec![serde_json::json!(1)], vec![serde_json::json!(2)]];
        let out = render(&cols, &rows, None);
        assert!(out.contains("2 rows"));
    }

    #[test]
    fn null_value_renders_as_empty() {
        let cols = vec!["x".to_owned()];
        let rows = vec![vec![serde_json::Value::Null]];
        let out = render(&cols, &rows, None);
        assert!(out.contains("1 row"));
    }

    #[test]
    fn timing_appears_when_provided() {
        let cols = vec!["n".to_owned()];
        let rows = vec![vec![serde_json::json!(1)]];
        let out = render(&cols, &rows, Some(Duration::from_millis(42)));
        assert!(out.contains("42.0 ms"));
    }

    #[test]
    fn timing_absent_when_none() {
        let cols = vec!["n".to_owned()];
        let rows = vec![vec![serde_json::json!(1)]];
        let out = render(&cols, &rows, None);
        assert!(!out.contains("ms"));
    }

    #[test]
    fn bool_and_number_render() {
        let cols = vec!["a".to_owned(), "b".to_owned()];
        let rows = vec![vec![serde_json::json!(true), serde_json::json!(3.15)]];
        let out = render(&cols, &rows, None);
        assert!(out.contains("true"));
        assert!(out.contains("3.15"));
    }
}
