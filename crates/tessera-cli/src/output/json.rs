// Copyright 2026 BelowZero Security OU. All rights reserved.

use tessera_protocol::packstream::PackStreamValue;

use super::value_to_json;

/// Render query results as NDJSON (one JSON object per line).
///
/// Each row is emitted as a JSON object with column names as keys.
/// Empty result sets produce an empty string.
#[must_use]
pub fn render(columns: &[String], rows: &[Vec<PackStreamValue>]) -> String {
    let mut lines = Vec::with_capacity(rows.len());
    for row in rows {
        let obj = row_to_object(columns, row);
        // OK: serializing a serde_json::Value into a String never fails
        let line = serde_json::to_string(&obj).unwrap_or_default();
        lines.push(line);
    }
    if lines.is_empty() {
        String::new()
    } else {
        let mut out = lines.join("\n");
        out.push('\n');
        out
    }
}

/// Zip column names with row values into a JSON object.
///
/// If `row.len() < columns.len()`, missing values are filled with `null`.
#[must_use]
pub fn row_to_object(
    columns: &[String],
    row: &[PackStreamValue],
) -> serde_json::Map<String, serde_json::Value> {
    let mut map = serde_json::Map::with_capacity(columns.len());
    for (i, col) in columns.iter().enumerate() {
        let val = row.get(i).map_or(serde_json::Value::Null, value_to_json);
        map.insert(col.clone(), val);
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_row_is_valid_ndjson() {
        let cols = vec!["name".to_owned(), "age".to_owned()];
        let rows = vec![vec![
            PackStreamValue::String("Alice".to_owned()),
            PackStreamValue::Int(30),
        ]];
        let out = render(&cols, &rows);
        let parsed: serde_json::Value = serde_json::from_str(out.trim()).expect("valid json"); // OK: test
        assert_eq!(parsed["name"], "Alice");
        assert_eq!(parsed["age"], 30);
    }

    #[test]
    fn two_rows_produce_two_lines() {
        let cols = vec!["x".to_owned()];
        let rows = vec![vec![PackStreamValue::Int(1)], vec![PackStreamValue::Int(2)]];
        let out = render(&cols, &rows);
        assert_eq!(out.trim().lines().count(), 2);
    }

    #[test]
    fn empty_result_produces_no_lines() {
        let cols = vec!["x".to_owned()];
        let out = render(&cols, &[]);
        assert!(out.is_empty());
    }

    #[test]
    fn null_value_appears_as_json_null() {
        let cols = vec!["x".to_owned()];
        let rows = vec![vec![PackStreamValue::Null]];
        let out = render(&cols, &rows);
        let parsed: serde_json::Value = serde_json::from_str(out.trim()).expect("valid json"); // OK: test
        assert!(parsed["x"].is_null());
    }

    #[test]
    fn each_line_is_independently_parseable() {
        let cols = vec!["a".to_owned()];
        let rows = vec![vec![PackStreamValue::Int(1)], vec![PackStreamValue::Int(2)]];
        let out = render(&cols, &rows);
        for line in out.trim().lines() {
            let _: serde_json::Value = serde_json::from_str(line).expect("each line is json"); // OK: test
        }
    }

    #[test]
    fn missing_values_filled_with_null() {
        let cols = vec!["a".to_owned(), "b".to_owned()];
        let rows = vec![vec![PackStreamValue::Int(1)]]; // only 1 value for 2 columns
        let out = render(&cols, &rows);
        let parsed: serde_json::Value = serde_json::from_str(out.trim()).expect("valid json"); // OK: test
        assert_eq!(parsed["a"], 1);
        assert!(parsed["b"].is_null());
    }

    #[test]
    fn row_to_object_basic() {
        let cols = vec!["x".to_owned()];
        let row = vec![PackStreamValue::Int(42)];
        let obj = row_to_object(&cols, &row);
        assert_eq!(obj["x"], 42);
    }
}
