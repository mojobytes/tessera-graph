// Copyright 2026 BelowZero Security OU. All rights reserved.

use tessera_protocol::packstream::PackStreamValue;

use super::value_to_display;

/// Render query results as RFC 4180 CSV.
///
/// When `include_headers` is `true`, the first line contains column names.
/// The `csv` crate handles quoting of values containing commas, quotes, or newlines.
#[must_use]
pub fn render(columns: &[String], rows: &[Vec<PackStreamValue>], include_headers: bool) -> String {
    let mut wtr = csv::Writer::from_writer(Vec::new());

    if include_headers {
        // OK: writing to Vec<u8> cannot fail
        wtr.write_record(columns).unwrap_or_default();
    }

    for row in rows {
        let fields: Vec<String> = row.iter().map(value_to_display).collect();
        wtr.write_record(&fields).unwrap_or_default(); // OK: writing to Vec<u8>
    }

    // OK: flush to Vec<u8> cannot fail
    wtr.flush().unwrap_or_default();
    let bytes = wtr.into_inner().unwrap_or_default(); // OK: after flush, always succeeds
    String::from_utf8(bytes).unwrap_or_default() // OK: csv crate produces valid UTF-8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_row_matches_columns() {
        let cols = vec!["name".to_owned(), "age".to_owned()];
        let out = render(&cols, &[], true);
        let first_line = out.lines().next().expect("has header"); // OK: test
        assert_eq!(first_line, "name,age");
    }

    #[test]
    fn value_with_comma_is_quoted() {
        let cols = vec!["city".to_owned()];
        let rows = vec![vec![PackStreamValue::String("Tallinn, Estonia".to_owned())]];
        let out = render(&cols, &rows, true);
        let data_line = out.lines().nth(1).expect("has data"); // OK: test
        assert!(data_line.contains('"'));
        assert!(data_line.contains("Tallinn, Estonia"));
    }

    #[test]
    fn no_headers_omits_header_row() {
        let cols = vec!["x".to_owned()];
        let rows = vec![vec![PackStreamValue::Int(1)]];
        let out = render(&cols, &rows, false);
        let trimmed = out.trim();
        assert_eq!(trimmed.lines().count(), 1);
        assert_eq!(trimmed, "1");
    }

    #[test]
    fn null_renders_as_empty_field() {
        let cols = vec!["a".to_owned(), "b".to_owned()];
        let rows = vec![vec![PackStreamValue::Null, PackStreamValue::Int(1)]];
        let out = render(&cols, &rows, true);
        let line = out.lines().nth(1).expect("has data"); // OK: test
        assert_eq!(line, ",1");
    }

    #[test]
    fn multiple_rows() {
        let cols = vec!["n".to_owned()];
        let rows = vec![vec![PackStreamValue::Int(1)], vec![PackStreamValue::Int(2)]];
        let out = render(&cols, &rows, true);
        let lines: Vec<&str> = out.trim().lines().collect();
        assert_eq!(lines.len(), 3); // header + 2 data
        assert_eq!(lines[0], "n");
        assert_eq!(lines[1], "1");
        assert_eq!(lines[2], "2");
    }

    #[test]
    fn value_with_quotes_is_escaped() {
        let cols = vec!["x".to_owned()];
        let rows = vec![vec![PackStreamValue::String("say \"hello\"".to_owned())]];
        let out = render(&cols, &rows, false);
        // CSV escapes quotes by doubling them
        assert!(out.contains("\"\""));
    }

    #[test]
    fn bool_and_float_render() {
        let cols = vec!["a".to_owned(), "b".to_owned()];
        let rows = vec![vec![
            PackStreamValue::Bool(true),
            PackStreamValue::Float(3.15),
        ]];
        let out = render(&cols, &rows, false);
        let line = out.trim();
        assert_eq!(line, "true,3.15");
    }
}
