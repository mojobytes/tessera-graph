// Copyright 2026 BelowZero Security OU. All rights reserved.

use crate::error::CliError;

/// Check if a trimmed line is a comment (starts with `--` outside quotes).
///
/// A line like `-- this is a comment` returns true, but
/// `CREATE (:X {val: '-- not a comment'})` returns false because the `--`
/// is inside a single-quoted string.
fn is_comment_line(trimmed: &str) -> bool {
    if !trimmed.contains("--") {
        return false;
    }
    // Track whether we're inside a single-quoted string
    let mut in_quotes = false;
    let bytes = trimmed.as_bytes();
    let len = bytes.len();
    let mut i = 0;
    while i < len {
        if bytes[i] == b'\'' {
            // GQL uses doubled single-quote escape: '' inside strings.
            if in_quotes && i + 1 < len && bytes[i + 1] == b'\'' {
                i += 2; // skip both quotes — stays in string
                continue;
            }
            in_quotes = !in_quotes;
        } else if !in_quotes && i + 1 < len && bytes[i] == b'-' && bytes[i + 1] == b'-' {
            // `--` found outside quotes — rest of line is comment
            // If this is at position 0, the entire line is a comment
            return i == 0 || trimmed[..i].trim().is_empty();
        }
        i += 1;
    }
    false
}

/// Split a GQL file into individual statements by semicolons.
///
/// - Blank lines and comment-only lines (`--` prefix) are skipped.
/// - Each statement is trimmed and the trailing semicolon is stripped.
/// - Multi-line statements are preserved (joined with newlines).
#[must_use]
pub fn split_gql_statements(content: &str) -> Vec<String> {
    let mut statements = Vec::new();
    let mut current = String::new();

    for line in content.lines() {
        let trimmed = line.trim();

        // Skip blank lines and comment-only lines
        if trimmed.is_empty() || is_comment_line(trimmed) {
            continue;
        }

        if trimmed.ends_with(';') {
            // Complete this statement
            let stmt_part = trimmed.trim_end_matches(';').trim_end();
            if current.is_empty() {
                if !stmt_part.is_empty() {
                    statements.push(stmt_part.to_owned());
                }
            } else {
                current.push('\n');
                current.push_str(stmt_part);
                statements.push(std::mem::take(&mut current));
            }
        } else {
            // Accumulate
            if !current.is_empty() {
                current.push('\n');
            }
            current.push_str(trimmed);
        }
    }

    // If there's remaining content without a trailing semicolon, include it
    if !current.is_empty() {
        statements.push(current);
    }

    statements
}

/// Plan for importing statements in batches.
#[derive(Debug)]
pub struct ImportPlan {
    statements: Vec<String>,
    batch_size: usize,
}

impl ImportPlan {
    /// Create a plan from pre-parsed statements with the given batch size.
    #[must_use]
    pub const fn new(statements: Vec<String>, batch_size: usize) -> Self {
        Self {
            statements,
            batch_size,
        }
    }

    /// Read-only view of all statements.
    #[must_use]
    pub fn statements(&self) -> &[String] {
        &self.statements
    }

    /// Create a plan from raw GQL file content.
    ///
    /// # Errors
    ///
    /// Returns `CliError::ImportExport` if no statements are found.
    pub fn from_gql_content(content: &str, batch_size: usize) -> Result<Self, CliError> {
        let statements = split_gql_statements(content);
        if statements.is_empty() {
            return Err(CliError::ImportExport(
                "no statements found in file".to_owned(),
            ));
        }
        Ok(Self {
            statements,
            batch_size,
        })
    }

    /// Number of batches needed to execute all statements.
    #[must_use]
    pub fn batch_count(&self) -> usize {
        self.statements.len().div_ceil(self.batch_size)
    }

    /// Get a specific batch of statements.
    #[must_use]
    pub fn batch(&self, index: usize) -> &[String] {
        let start = index * self.batch_size;
        let end = (start + self.batch_size).min(self.statements.len());
        if start >= self.statements.len() {
            &[]
        } else {
            &self.statements[start..end]
        }
    }

    /// Format a dry-run summary of the plan.
    #[must_use]
    pub fn dry_run_summary(&self) -> String {
        use std::fmt::Write;

        let mut out = String::new();
        let _ = writeln!(
            out,
            "Dry run: {} statements in {} batches (batch size: {})\n",
            self.statements.len(),
            self.batch_count(),
            self.batch_size,
        );

        for (i, stmt) in self.statements.iter().enumerate() {
            let _ = writeln!(out, "  [{:>3}] {}", i + 1, truncate_line(stmt, 80));
        }

        out
    }
}

/// Generate GQL CREATE statements from CSV rows representing nodes.
///
/// First row is the header. The first column is the label.
/// Remaining columns become properties.
///
/// # Errors
///
/// Returns `CliError::ImportExport` if the CSV has no rows or the label column is empty.
pub fn csv_nodes_to_gql(csv_content: &str) -> Result<Vec<String>, CliError> {
    let mut reader = csv::ReaderBuilder::new()
        .has_headers(true)
        .from_reader(csv_content.as_bytes());

    let headers: Vec<String> = reader
        .headers()
        .map_err(|e| CliError::ImportExport(format!("invalid CSV headers: {e}")))?
        .iter()
        .map(String::from)
        .collect();

    if headers.is_empty() {
        return Err(CliError::ImportExport("CSV has no columns".to_owned()));
    }

    let label_col = &headers[0];
    let prop_cols = &headers[1..];

    let mut statements = Vec::new();

    for result in reader.records() {
        let record = result.map_err(|e| CliError::ImportExport(format!("invalid CSV row: {e}")))?;

        let label = record.get(0).filter(|s| !s.is_empty()).ok_or_else(|| {
            CliError::ImportExport(format!("empty {label_col} (label) in CSV row"))
        })?;

        let mut props = Vec::new();
        for (i, col) in prop_cols.iter().enumerate() {
            if let Some(val) = record.get(i + 1).filter(|s| !s.is_empty()) {
                // Try to parse as number, otherwise use string
                let formatted = if val.parse::<i64>().is_ok() || val.parse::<f64>().is_ok() {
                    format!("{col}: {val}")
                } else {
                    format!("{col}: '{}'", val.replace('\'', "''"))
                };
                props.push(formatted);
            }
        }

        let props_str = if props.is_empty() {
            String::new()
        } else {
            format!(" {{{}}}", props.join(", "))
        };

        statements.push(format!("CREATE (:{label}{props_str})"));
    }

    if statements.is_empty() {
        return Err(CliError::ImportExport("CSV has no data rows".to_owned()));
    }

    Ok(statements)
}

/// Generate GQL statements from a JSON string in tessera-import format.
///
/// Expected format:
/// ```json
/// {
///   "nodes": [{"label": "L", "properties": {"k": "v"}}],
///   "edges": [{"source": {"label": "L", "match": {"id": "x"}},
///              "target": {"label": "L", "match": {"id": "y"}},
///              "label": "REL", "properties": {}}]
/// }
/// ```
///
/// Nodes produce `CREATE (:Label {props})` statements.
/// Edges produce `MATCH (a:L {k: 'v'}) MATCH (b:L {k: 'v'}) CREATE (a)-[:REL]->(b)`.
///
/// # Errors
///
/// Returns `CliError::ImportExport` on invalid JSON, missing fields, or empty data.
pub fn json_to_gql_statements(json_text: &str) -> Result<Vec<String>, CliError> {
    let root: serde_json::Value = serde_json::from_str(json_text)
        .map_err(|e| CliError::ImportExport(format!("invalid JSON: {e}")))?;

    let obj = root
        .as_object()
        .ok_or_else(|| CliError::ImportExport("root must be a JSON object".into()))?;

    let nodes = obj
        .get("nodes")
        .and_then(|v| v.as_array())
        .ok_or_else(|| CliError::ImportExport("missing 'nodes' array".into()))?;

    let edges = obj
        .get("edges")
        .and_then(|v| v.as_array())
        .ok_or_else(|| CliError::ImportExport("missing 'edges' array".into()))?;

    if nodes.is_empty() && edges.is_empty() {
        return Err(CliError::ImportExport("no nodes or edges in JSON".into()));
    }

    let mut statements = Vec::with_capacity(nodes.len() + edges.len());
    let mut buf = String::with_capacity(256);

    for node in nodes {
        let label = node
            .get("label")
            .and_then(|v| v.as_str())
            .ok_or_else(|| CliError::ImportExport("node missing 'label'".into()))?;
        let props = node.get("properties").and_then(|v| v.as_object());
        buf.clear();
        buf.push_str("CREATE (:");
        write_gql_identifier(label, "node label", &mut buf)?;
        write_json_props_to_buf(props, &mut buf)?;
        buf.push(')');
        statements.push(std::mem::take(&mut buf));
    }

    for edge in edges {
        let rel_label = edge
            .get("label")
            .and_then(|v| v.as_str())
            .ok_or_else(|| CliError::ImportExport("edge missing 'label'".into()))?;
        let source_match = format_endpoint_match(edge, "source")?;
        let target_match = format_endpoint_match(edge, "target")?;

        buf.clear();
        buf.push_str("MATCH (a");
        buf.push_str(&source_match);
        buf.push_str("), (b");
        buf.push_str(&target_match);
        buf.push_str(") CREATE (a)-[:");
        write_gql_identifier(rel_label, "edge label", &mut buf)?;

        let rel_props = edge.get("properties").and_then(|v| v.as_object());
        write_json_props_to_buf(rel_props, &mut buf)?;

        buf.push_str("]->(b)");
        statements.push(std::mem::take(&mut buf));
    }

    Ok(statements)
}

/// Check whether `s` is a valid simple GQL identifier (safe for unquoted emission).
///
/// Rule: non-empty, starts with ASCII letter or `_`, every subsequent char
/// is ASCII alphanumeric or `_`.
#[inline]
fn is_simple_gql_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        None => false,
        Some(c) if !(c.is_ascii_alphabetic() || c == '_') => false,
        _ => chars.all(|c| c.is_ascii_alphanumeric() || c == '_'),
    }
}

/// Write a GQL identifier into `buf`, using delimited form (`"..."`) if needed.
///
/// Simple identifiers (alphanumeric + underscore) are emitted unquoted.
/// Identifiers with spaces or special characters are emitted as GQL
/// delimited identifiers (double-quoted, per ISO/IEC 39075).
///
/// Returns `Err` if the identifier is empty or contains double quotes
/// (which cannot be represented in a delimited identifier).
fn write_gql_identifier(s: &str, context: &str, buf: &mut String) -> Result<(), CliError> {
    if s.is_empty() {
        return Err(CliError::ImportExport(format!("empty {context}")));
    }
    if s.contains('"') {
        return Err(CliError::ImportExport(format!(
            "invalid {context}: {s:?} (contains double quote)"
        )));
    }
    if s.bytes().any(|b| b < 0x20) {
        return Err(CliError::ImportExport(format!(
            "invalid {context}: {s:?} (contains control character)"
        )));
    }
    if is_simple_gql_identifier(s) {
        buf.push_str(s);
    } else {
        buf.push('"');
        buf.push_str(s);
        buf.push('"');
    }
    Ok(())
}

/// Write a JSON properties map as a GQL property string into `buf`.
///
/// Produces nothing if `props` is `None` or empty, otherwise writes
/// ` {k1: v1, k2: v2, ...}` — note the leading space.
///
/// Property keys are emitted via [`write_gql_identifier`].
fn write_json_props_to_buf(
    props: Option<&serde_json::Map<String, serde_json::Value>>,
    buf: &mut String,
) -> Result<(), CliError> {
    let Some(props) = props else { return Ok(()) };
    if props.is_empty() {
        return Ok(());
    }

    buf.push_str(" {");
    let mut first = true;
    for (k, v) in props {
        if !first {
            buf.push_str(", ");
        }
        write_gql_identifier(k, "property key", buf)?;
        buf.push_str(": ");
        write_json_value_to_buf(v, buf);
        first = false;
    }
    buf.push('}');
    Ok(())
}

/// Write a JSON value as a GQL literal into `buf`.
fn write_json_value_to_buf(v: &serde_json::Value, buf: &mut String) {
    match v {
        serde_json::Value::Null => buf.push_str("null"),
        serde_json::Value::Bool(true) => buf.push_str("true"),
        serde_json::Value::Bool(false) => buf.push_str("false"),
        serde_json::Value::Number(n) => {
            use std::fmt::Write as _;
            let _ = write!(buf, "{n}");
        }
        serde_json::Value::String(s) => {
            buf.push('\'');
            for ch in s.chars() {
                if ch == '\'' {
                    buf.push_str("''");
                } else {
                    buf.push(ch);
                }
            }
            buf.push('\'');
        }
        other => {
            // Arrays and objects are stored as JSON strings. The serialized
            // JSON may contain backslashes (e.g. `\"` inside nested strings);
            // these are passed through as-is since GQL string literals treat
            // backslash as a literal character.
            eprintln!(
                "Warning: property stored as JSON string \
                 (array/object values are not natively supported)"
            );
            buf.push('\'');
            let serialized = other.to_string();
            for ch in serialized.chars() {
                if ch == '\'' {
                    buf.push_str("''");
                } else {
                    buf.push(ch);
                }
            }
            buf.push('\'');
        }
    }
}

/// Format a MATCH clause for an edge endpoint: `:Label {matchKey: 'matchVal'}`.
fn format_endpoint_match(
    edge: &serde_json::Value,
    endpoint_key: &str,
) -> Result<String, CliError> {
    let ep = edge
        .get(endpoint_key)
        .and_then(|v| v.as_object())
        .ok_or_else(|| {
            CliError::ImportExport(format!("edge missing '{endpoint_key}' object"))
        })?;

    let label = ep.get("label").and_then(|v| v.as_str());

    let match_obj =
        ep.get("match")
            .and_then(|v| v.as_object())
            .ok_or_else(|| {
                CliError::ImportExport(format!(
                    "edge {endpoint_key} missing 'match' object"
                ))
            })?;

    if match_obj.len() > 1 {
        return Err(CliError::ImportExport(format!(
            "edge {endpoint_key}.match must have exactly one key, got {}",
            match_obj.len()
        )));
    }

    let (match_key, match_val) = match_obj.iter().next().ok_or_else(|| {
        CliError::ImportExport(format!("edge {endpoint_key}.match is empty"))
    })?;

    let mut result = String::with_capacity(64);
    if let Some(l) = label {
        result.push(':');
        write_gql_identifier(l, &format!("edge {endpoint_key} label"), &mut result)?;
        result.push(' ');
    } else {
        result.push(' ');
    }
    result.push('{');
    write_gql_identifier(match_key, &format!("edge {endpoint_key} match key"), &mut result)?;
    result.push_str(": ");
    write_json_value_to_buf(match_val, &mut result);
    result.push('}');
    Ok(result)
}

/// Truncate a single-line string for display, replacing newlines.
fn truncate_line(s: &str, max: usize) -> String {
    let oneline = s.replace('\n', " ");
    let char_count = oneline.chars().count();
    if char_count <= max {
        oneline
    } else {
        let keep = max.saturating_sub(3);
        let byte_end = oneline
            .char_indices()
            .nth(keep)
            .map_or(oneline.len(), |(i, _)| i);
        format!("{}...", &oneline[..byte_end])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- GQL splitter ---

    #[test]
    fn split_single_statement() {
        let stmts = split_gql_statements("CREATE (:Person {name: 'Alice'});");
        assert_eq!(stmts.len(), 1);
        assert_eq!(stmts[0], "CREATE (:Person {name: 'Alice'})");
    }

    #[test]
    fn split_multiple_statements() {
        let input = "CREATE (:A);\nCREATE (:B);\nCREATE (:C);";
        let stmts = split_gql_statements(input);
        assert_eq!(stmts.len(), 3);
    }

    #[test]
    fn skip_blank_lines() {
        let input = "CREATE (:A);\n\n\nCREATE (:B);";
        let stmts = split_gql_statements(input);
        assert_eq!(stmts.len(), 2);
    }

    #[test]
    fn skip_comments() {
        let input = "-- This is a comment\nCREATE (:A);\n-- Another comment\nCREATE (:B);";
        let stmts = split_gql_statements(input);
        assert_eq!(stmts.len(), 2);
    }

    #[test]
    fn multiline_statement() {
        let input = "CREATE (:Person\n  {name: 'Alice'}\n);";
        let stmts = split_gql_statements(input);
        assert_eq!(stmts.len(), 1);
        assert!(stmts[0].contains("Person"));
        assert!(stmts[0].contains("Alice"));
    }

    #[test]
    fn comment_inside_string_not_skipped() {
        let input = "CREATE (:X {val: '-- not a comment'});";
        let stmts = split_gql_statements(input);
        assert_eq!(stmts.len(), 1);
        assert!(stmts[0].contains("-- not a comment"));
    }

    #[test]
    fn inline_comment_after_data_preserved() {
        // A line with data followed by `--` comment: the data should be preserved
        let input = "CREATE (:A {name: 'test'}); -- this is a comment";
        let stmts = split_gql_statements(input);
        // The whole line ends with a non-semicolon char, so it won't split on `;`
        // in the middle. But our splitter operates line-by-line. The `;` is not at
        // the end of the trimmed line because ` -- this is a comment` follows.
        // This is a known limitation — we test the current behavior.
        assert!(!stmts.is_empty());
    }

    #[test]
    fn is_comment_line_basic() {
        assert!(is_comment_line("-- a comment"));
        assert!(!is_comment_line("CREATE (:X)"));
        assert!(!is_comment_line("CREATE (:X {val: '-- inside quotes'})"));
    }

    #[test]
    fn statement_without_trailing_semicolon() {
        let stmts = split_gql_statements("CREATE (:X)");
        assert_eq!(stmts.len(), 1);
        assert_eq!(stmts[0], "CREATE (:X)");
    }

    #[test]
    fn empty_content_returns_empty() {
        assert!(split_gql_statements("").is_empty());
        assert!(split_gql_statements("-- only comments").is_empty());
    }

    // --- Import plan ---

    #[test]
    fn plan_batch_count() {
        let plan = ImportPlan::from_gql_content(
            "CREATE (:A);\nCREATE (:B);\nCREATE (:C);\nCREATE (:D);\nCREATE (:E);",
            2,
        )
        .expect("plan"); // OK: test
        assert_eq!(plan.statements.len(), 5);
        assert_eq!(plan.batch_count(), 3);
    }

    #[test]
    fn plan_batch_indexing() {
        let plan = ImportPlan::from_gql_content("CREATE (:A);\nCREATE (:B);\nCREATE (:C);", 2)
            .expect("plan"); // OK: test
        assert_eq!(plan.batch(0).len(), 2);
        assert_eq!(plan.batch(1).len(), 1);
        assert!(plan.batch(2).is_empty());
    }

    #[test]
    fn plan_empty_content_is_error() {
        let result = ImportPlan::from_gql_content("", 100);
        assert!(result.is_err());
    }

    #[test]
    fn dry_run_summary_format() {
        let plan = ImportPlan::from_gql_content("CREATE (:A);\nCREATE (:B);", 100).expect("plan"); // OK: test
        let summary = plan.dry_run_summary();
        assert!(summary.contains("2 statements"));
        assert!(summary.contains("1 batches"));
        assert!(summary.contains("CREATE (:A)"));
        assert!(summary.contains("CREATE (:B)"));
    }

    // --- CSV nodes → GQL ---

    #[test]
    fn csv_nodes_basic() {
        let csv = "label,name,age\nPerson,Alice,30\nPerson,Bob,25\n";
        let stmts = csv_nodes_to_gql(csv).expect("csv to gql"); // OK: test
        assert_eq!(stmts.len(), 2);
        assert!(stmts[0].contains(":Person"));
        assert!(stmts[0].contains("name: 'Alice'"));
        assert!(stmts[0].contains("age: 30"));
    }

    #[test]
    fn csv_nodes_empty_properties_omitted() {
        let csv = "label,name,age\nPerson,Alice,\n";
        let stmts = csv_nodes_to_gql(csv).expect("csv to gql"); // OK: test
        assert_eq!(stmts.len(), 1);
        // age should be omitted since it's empty
        assert!(!stmts[0].contains("age"));
    }

    #[test]
    fn csv_nodes_empty_label_is_error() {
        let csv = "label,name\n,Alice\n";
        let result = csv_nodes_to_gql(csv);
        assert!(result.is_err());
    }

    #[test]
    fn csv_nodes_no_data_rows_is_error() {
        let csv = "label,name\n";
        let result = csv_nodes_to_gql(csv);
        assert!(result.is_err());
    }

    #[test]
    fn csv_nodes_string_value_with_quote() {
        let csv = "label,name\nPerson,O'Brien\n";
        let stmts = csv_nodes_to_gql(csv).expect("csv to gql"); // OK: test
        assert!(stmts[0].contains("O''Brien"));
    }

    #[test]
    fn csv_nodes_numeric_values() {
        let csv = "label,score,ratio\nItem,42,3.15\n";
        let stmts = csv_nodes_to_gql(csv).expect("csv to gql"); // OK: test
        // Numbers should not be quoted
        assert!(stmts[0].contains("score: 42"));
        assert!(stmts[0].contains("ratio: 3.15"));
    }

    // --- truncate_line ---

    #[test]
    fn truncate_short_line() {
        assert_eq!(truncate_line("hello", 80), "hello");
    }

    #[test]
    fn truncate_long_line() {
        let long = "x".repeat(100);
        let result = truncate_line(&long, 20);
        assert!(result.len() <= 20);
        assert!(result.ends_with("..."));
    }

    #[test]
    fn truncate_replaces_newlines() {
        assert_eq!(truncate_line("a\nb\nc", 80), "a b c");
    }

    #[test]
    fn import_plan_new_constructor() {
        let stmts = vec!["CREATE (:A)".to_owned(), "CREATE (:B)".to_owned()];
        let plan = ImportPlan::new(stmts, 1);
        assert_eq!(plan.batch_count(), 2);
        assert_eq!(plan.batch(0), &["CREATE (:A)"]);
        assert_eq!(plan.batch(1), &["CREATE (:B)"]);
    }

    #[test]
    fn import_plan_statements_accessor() {
        let stmts = vec!["S1".to_owned(), "S2".to_owned(), "S3".to_owned()];
        let plan = ImportPlan::new(stmts.clone(), 10);
        assert_eq!(plan.statements(), stmts.as_slice());
    }

    #[test]
    fn plan_statements_matches_batch_iteration() {
        let content = "CREATE (:A);\nCREATE (:B);\nCREATE (:C);";
        let plan = ImportPlan::from_gql_content(content, 2).expect("plan"); // OK: test
        let via_batches: Vec<&str> = (0..plan.batch_count())
            .flat_map(|i| plan.batch(i).iter().map(String::as_str))
            .collect();
        let via_statements: Vec<&str> = plan.statements().iter().map(String::as_str).collect();
        assert_eq!(via_batches, via_statements);
    }

    #[test]
    fn truncate_multibyte_unicode_no_panic() {
        let s = "€€€€€€€€€€"; // 10 × 3 bytes = 30 bytes
        let result = truncate_line(s, 8);
        assert!(result.ends_with("..."));
    }

    #[test]
    fn truncate_multibyte_exactly_at_boundary() {
        let s = "αβγδεζηθ"; // 8 × 2 bytes = 16 bytes
        let result = truncate_line(s, 5);
        assert!(result.ends_with("..."));
    }

    // --- JSON → GQL ---

    #[test]
    fn json_nodes_to_gql_basic() {
        let json = r#"{"nodes": [
            {"label": "Person", "properties": {"name": "Alice", "age": 30}},
            {"label": "Person", "properties": {"name": "Bob"}}
        ], "edges": []}"#;
        let stmts = json_to_gql_statements(json).unwrap(); // OK: test
        assert_eq!(stmts.len(), 2);
        assert!(stmts[0].starts_with("CREATE (:Person"), "got: {}", stmts[0]);
        assert!(stmts[0].contains("name: 'Alice'"), "got: {}", stmts[0]);
        assert!(stmts[0].contains("age: 30"), "got: {}", stmts[0]);
        assert!(stmts[1].contains("name: 'Bob'"), "got: {}", stmts[1]);
    }

    #[test]
    fn json_empty_data_is_error() {
        let json = r#"{"nodes": [], "edges": []}"#;
        assert!(json_to_gql_statements(json).is_err());
    }

    #[test]
    fn json_invalid_json_is_error() {
        assert!(json_to_gql_statements("not json").is_err());
    }

    #[test]
    fn json_edges_to_match_create() {
        let json = r#"{"nodes": [
            {"label": "Person", "properties": {"id": "1", "name": "Alice"}},
            {"label": "Person", "properties": {"id": "2", "name": "Bob"}}
        ], "edges": [
            {
                "source": {"label": "Person", "match": {"id": "1"}},
                "target": {"label": "Person", "match": {"id": "2"}},
                "label": "KNOWS",
                "properties": {}
            }
        ]}"#;
        let stmts = json_to_gql_statements(json).unwrap(); // OK: test
        assert_eq!(stmts.len(), 3); // 2 nodes + 1 edge
        let edge_stmt = &stmts[2];
        assert!(edge_stmt.starts_with("MATCH (a:Person"), "got: {edge_stmt}");
        assert!(edge_stmt.contains("), (b:Person"), "got: {edge_stmt}");
        assert!(edge_stmt.contains("CREATE (a)-[:KNOWS]->(b)"), "got: {edge_stmt}");
    }

    #[test]
    fn json_edge_with_properties() {
        let json = r#"{"nodes": [
            {"label": "Person", "properties": {"id": "1"}},
            {"label": "Person", "properties": {"id": "2"}}
        ], "edges": [
            {
                "source": {"label": "Person", "match": {"id": "1"}},
                "target": {"label": "Person", "match": {"id": "2"}},
                "label": "KNOWS",
                "properties": {"since": 2024}
            }
        ]}"#;
        let stmts = json_to_gql_statements(json).unwrap(); // OK: test
        let edge_stmt = &stmts[2];
        assert!(edge_stmt.contains("{since: 2024}"), "got: {edge_stmt}");
    }

    #[test]
    fn json_edge_missing_source_is_error() {
        let json = r#"{"nodes": [], "edges": [
            {"target": {"label": "X", "match": {"id": "1"}}, "label": "R", "properties": {}}
        ]}"#;
        assert!(json_to_gql_statements(json).is_err());
    }

    #[test]
    fn json_property_with_single_quotes_escaped() {
        let json = r#"{"nodes": [
            {"label": "Person", "properties": {"name": "O'Brien"}}
        ], "edges": []}"#;
        let stmts = json_to_gql_statements(json).unwrap(); // OK: test
        assert!(stmts[0].contains("O''Brien"), "got: {}", stmts[0]);
    }

    #[test]
    fn json_boolean_and_null_properties() {
        let json = r#"{"nodes": [
            {"label": "Item", "properties": {"active": true, "deleted": false, "note": null}}
        ], "edges": []}"#;
        let stmts = json_to_gql_statements(json).unwrap(); // OK: test
        assert!(stmts[0].contains("active: true"), "got: {}", stmts[0]);
        assert!(stmts[0].contains("deleted: false"), "got: {}", stmts[0]);
        assert!(stmts[0].contains("note: null"), "got: {}", stmts[0]);
    }

    #[test]
    fn json_array_property_stored_as_json_string() {
        let json = r#"{"nodes": [
            {"label": "X", "properties": {"tags": ["a", "b"]}}
        ], "edges": []}"#;
        let stmts = json_to_gql_statements(json).unwrap(); // OK: test
        assert!(stmts[0].contains("tags: '"), "got: {}", stmts[0]);
    }

    // --- escaped-quote in comments ---

    #[test]
    fn comment_not_skipped_when_dash_dash_follows_doubled_quote() {
        // GQL escapes single quotes by doubling: 'it''s'
        let line = "CREATE (:X {val: 'it''s -- tricky'});";
        assert!(!is_comment_line(line), "line should NOT be a comment");
        let stmts = split_gql_statements(line);
        assert_eq!(stmts.len(), 1);
    }

    // --- GQL identifier validation ---

    #[test]
    fn simple_gql_identifier_valid_cases() {
        assert!(is_simple_gql_identifier("Person"));
        assert!(is_simple_gql_identifier("_id"));
        assert!(is_simple_gql_identifier("REL_TYPE"));
        assert!(is_simple_gql_identifier("a1"));
    }

    #[test]
    fn simple_gql_identifier_invalid_cases() {
        assert!(!is_simple_gql_identifier(""));
        assert!(!is_simple_gql_identifier("1name"));
        assert!(!is_simple_gql_identifier("has space"));
        assert!(!is_simple_gql_identifier("has-hyphen"));
        assert!(!is_simple_gql_identifier("has.dot"));
    }

    #[test]
    fn write_gql_identifier_simple() {
        let mut buf = String::new();
        write_gql_identifier("Person", "test", &mut buf).unwrap(); // OK: test
        assert_eq!(buf, "Person");
    }

    #[test]
    fn write_gql_identifier_delimited() {
        let mut buf = String::new();
        write_gql_identifier("Average Pyranometer", "test", &mut buf).unwrap(); // OK: test
        assert_eq!(buf, "\"Average Pyranometer\"");
    }

    #[test]
    fn write_gql_identifier_rejects_double_quote() {
        let mut buf = String::new();
        assert!(write_gql_identifier("bad\"name", "test", &mut buf).is_err());
    }

    #[test]
    fn write_gql_identifier_rejects_empty() {
        let mut buf = String::new();
        assert!(write_gql_identifier("", "test", &mut buf).is_err());
    }

    #[test]
    fn write_gql_identifier_rejects_null_byte() {
        let mut buf = String::new();
        assert!(write_gql_identifier("bad\x00name", "test", &mut buf).is_err());
    }

    #[test]
    fn write_gql_identifier_rejects_control_char() {
        let mut buf = String::new();
        assert!(write_gql_identifier("bad\x01name", "test", &mut buf).is_err());
    }

    // --- Injection rejection ---

    #[test]
    fn json_node_with_numeric_start_label_uses_delimited() {
        let json = r#"{"nodes": [{"label": "1Bad", "properties": {}}], "edges": []}"#;
        let stmts = json_to_gql_statements(json).unwrap(); // OK: test
        assert!(stmts[0].contains(":\"1Bad\""), "got: {}", stmts[0]);
    }

    #[test]
    fn json_node_with_space_label_uses_delimited() {
        let json = r#"{"nodes": [{"label": "My Label", "properties": {}}], "edges": []}"#;
        let stmts = json_to_gql_statements(json).unwrap(); // OK: test
        assert!(stmts[0].contains(":\"My Label\""), "got: {}", stmts[0]);
    }

    #[test]
    fn json_node_with_double_quote_label_is_error() {
        let json = r#"{"nodes": [{"label": "bad\"name", "properties": {}}], "edges": []}"#;
        assert!(json_to_gql_statements(json).is_err());
    }

    #[test]
    fn json_edge_with_dash_rel_label_uses_delimited() {
        let json = r#"{"nodes": [
            {"label": "A", "properties": {"id": "1"}},
            {"label": "B", "properties": {"id": "2"}}
        ], "edges": [
            {"source": {"label": "A", "match": {"id": "1"}},
             "target": {"label": "B", "match": {"id": "2"}},
             "label": "has-dash", "properties": {}}
        ]}"#;
        let stmts = json_to_gql_statements(json).unwrap(); // OK: test
        let edge = &stmts[2];
        assert!(edge.contains(":\"has-dash\""), "got: {edge}");
    }

    #[test]
    fn json_property_key_with_spaces_uses_delimited() {
        let json = r#"{"nodes": [{"label": "X", "properties": {"Average Pyranometer": "val"}}], "edges": []}"#;
        let stmts = json_to_gql_statements(json).unwrap(); // OK: test
        assert!(stmts[0].contains("\"Average Pyranometer\": 'val'"), "got: {}", stmts[0]);
    }

    #[test]
    fn json_property_key_with_double_quote_is_error() {
        let json = r#"{"nodes": [{"label": "X", "properties": {"bad\"key": "val"}}], "edges": []}"#;
        assert!(json_to_gql_statements(json).is_err());
    }

    #[test]
    fn json_endpoint_label_with_spaces_uses_delimited() {
        let json = r#"{"nodes": [
            {"label": "A", "properties": {"id": "1"}}
        ], "edges": [
            {"source": {"label": "My Type", "match": {"id": "1"}},
             "target": {"label": "A", "match": {"id": "1"}},
             "label": "R", "properties": {}}
        ]}"#;
        let stmts = json_to_gql_statements(json).unwrap(); // OK: test
        let edge = &stmts[1];
        assert!(edge.contains(":\"My Type\""), "got: {edge}");
    }

    // --- Multi-key match rejection ---

    #[test]
    fn json_edge_endpoint_without_label() {
        let json = r#"{"nodes": [
            {"label": "A", "properties": {"id": "1"}},
            {"label": "B", "properties": {"id": "2"}}
        ], "edges": [
            {
                "source": {"match": {"id": "1"}},
                "target": {"label": "B", "match": {"id": "2"}},
                "label": "R",
                "properties": {}
            }
        ]}"#;
        let stmts = json_to_gql_statements(json).unwrap(); // OK: test
        let edge_stmt = &stmts[2];
        assert!(edge_stmt.starts_with("MATCH (a {id:"), "got: {edge_stmt}");
    }

    #[test]
    fn json_edge_match_value_numeric_is_unquoted() {
        let json = r#"{"nodes": [
            {"label": "A", "properties": {"id": "1"}}
        ], "edges": [
            {
                "source": {"label": "A", "match": {"id": 1}},
                "target": {"label": "A", "match": {"id": 1}},
                "label": "R",
                "properties": {}
            }
        ]}"#;
        let stmts = json_to_gql_statements(json).unwrap(); // OK: test
        let edge_stmt = &stmts[1];
        assert!(edge_stmt.contains("{id: 1}"), "got: {edge_stmt}");
    }

    #[test]
    fn json_edge_match_value_string_is_quoted() {
        let json = r#"{"nodes": [
            {"label": "A", "properties": {"id": "1"}}
        ], "edges": [
            {
                "source": {"label": "A", "match": {"id": "1"}},
                "target": {"label": "A", "match": {"id": "1"}},
                "label": "R",
                "properties": {}
            }
        ]}"#;
        let stmts = json_to_gql_statements(json).unwrap(); // OK: test
        let edge_stmt = &stmts[1];
        assert!(edge_stmt.contains("{id: '1'}"), "got: {edge_stmt}");
    }

    #[test]
    fn json_edge_with_multiple_match_keys_is_error() {
        let json = r#"{"nodes": [
            {"label": "A", "properties": {"id": "1", "name": "Alice"}},
            {"label": "A", "properties": {"id": "2", "name": "Bob"}}
        ], "edges": [
            {"source": {"label": "A", "match": {"id": "1", "name": "Alice"}},
             "target": {"label": "A", "match": {"id": "2"}},
             "label": "R", "properties": {}}
        ]}"#;
        assert!(json_to_gql_statements(json).is_err());
    }
}
