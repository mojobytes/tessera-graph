// Copyright 2026 BelowZero Security OU. All rights reserved.

use std::io::BufRead as _;

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
///
/// Delegates to [`stream_gql_import`] — single source of splitting logic.
#[must_use]
pub fn split_gql_statements(content: &str) -> Vec<String> {
    let mut out = Vec::new();
    // The callback never fails, so the Result is always Ok.
    let _ = stream_gql_import(std::io::Cursor::new(content), |s| {
        out.push(s);
        Ok(())
    });
    out
}

/// Stream GQL statements from a reader, calling `on_stmt` for each complete statement.
///
/// This is the streaming equivalent of [`split_gql_statements`]: same splitting logic
/// (semicolons, comments, blank lines) but processes input line-by-line from a reader
/// instead of requiring the entire content in memory.
///
/// Returns the number of statements emitted.
///
/// # Errors
///
/// Returns any error propagated from `on_stmt`.
pub fn stream_gql_import<R: std::io::Read>(
    reader: R,
    mut on_stmt: impl FnMut(String) -> Result<(), CliError>,
) -> Result<usize, CliError> {
    let reader = std::io::BufReader::new(reader);
    let mut current = String::new();
    let mut count = 0usize;

    for line_result in reader.lines() {
        let line =
            line_result.map_err(|e| CliError::ImportExport(format!("read error: {e}")))?;
        let trimmed = line.trim();

        if trimmed.is_empty() || is_comment_line(trimmed) {
            continue;
        }

        if trimmed.ends_with(';') {
            let stmt_part = trimmed.trim_end_matches(';').trim_end();
            if current.is_empty() {
                if !stmt_part.is_empty() {
                    on_stmt(stmt_part.to_owned())?;
                    count += 1;
                }
            } else {
                current.push('\n');
                current.push_str(stmt_part);
                on_stmt(std::mem::take(&mut current))?;
                count += 1;
            }
        } else {
            if !current.is_empty() {
                current.push('\n');
            }
            current.push_str(trimmed);
        }
    }

    // Flush any trailing statement without semicolon
    if !current.is_empty() {
        on_stmt(current)?;
        count += 1;
    }

    Ok(count)
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

/// Build a GQL `CREATE (:<label><props>)` statement for a CSV node row.
///
/// The label is validated via [`write_gql_identifier`], preventing injection
/// and ensuring special characters are properly delimited.
fn finish_csv_node_stmt(label: &str, props_str: &str) -> Result<String, CliError> {
    let mut stmt = String::with_capacity(16 + label.len() + props_str.len());
    stmt.push_str("CREATE (:");
    write_gql_identifier(label, "node label", &mut stmt)?;
    stmt.push_str(props_str);
    stmt.push(')');
    Ok(stmt)
}

/// Generate GQL CREATE statements from CSV rows representing nodes.
///
/// First row is the header. The first column is the label.
/// Remaining columns become properties.
///
/// Delegates to [`stream_csv_import`] — single source of CSV parsing logic.
///
/// # Errors
///
/// Returns `CliError::ImportExport` if the CSV has no rows or the label column is empty.
pub fn csv_nodes_to_gql(csv_content: &str) -> Result<Vec<String>, CliError> {
    let mut out = Vec::new();
    stream_csv_import(csv_content.as_bytes(), |s| {
        out.push(s);
        Ok(())
    })?;
    Ok(out)
}

/// Stream CSV node rows from a reader, calling `on_stmt` for each generated GQL
/// CREATE statement.
///
/// This is the streaming equivalent of [`csv_nodes_to_gql`]: same label/property
/// logic but processes rows lazily from a reader instead of requiring the entire
/// content in memory.
///
/// Returns the number of statements emitted.
///
/// # Errors
///
/// Returns `CliError::ImportExport` on CSV parse errors, empty labels,
/// no data rows, or any error propagated from `on_stmt`.
pub fn stream_csv_import<R: std::io::Read>(
    reader: R,
    mut on_stmt: impl FnMut(String) -> Result<(), CliError>,
) -> Result<usize, CliError> {
    let mut csv_reader = csv::ReaderBuilder::new()
        .has_headers(true)
        .from_reader(reader);

    let headers: Vec<String> = csv_reader
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
    let mut count = 0usize;

    for result in csv_reader.records() {
        let record =
            result.map_err(|e| CliError::ImportExport(format!("invalid CSV row: {e}")))?;

        let label = record.get(0).filter(|s| !s.is_empty()).ok_or_else(|| {
            CliError::ImportExport(format!("empty {label_col} (label) in CSV row"))
        })?;

        let mut props = Vec::new();
        for (i, col) in prop_cols.iter().enumerate() {
            if let Some(val) = record.get(i + 1).filter(|s| !s.is_empty()) {
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

        on_stmt(finish_csv_node_stmt(label, &props_str)?)?;
        count += 1;
    }

    if count == 0 {
        return Err(CliError::ImportExport("CSV has no data rows".to_owned()));
    }

    Ok(count)
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
/// Delegates to [`stream_json_import`] — single source of JSON parsing logic.
///
/// # Errors
///
/// Returns `CliError::ImportExport` on invalid JSON, missing fields, or empty data.
pub fn json_to_gql_statements(json_text: &str) -> Result<Vec<String>, CliError> {
    let mut out = Vec::new();
    stream_json_import(json_text.as_bytes(), |s| {
        out.push(s);
        Ok(())
    })?;
    Ok(out)
}

/// Convert a single JSON node value to a GQL CREATE statement.
fn node_value_to_gql_stmt(node: &serde_json::Value) -> Result<String, CliError> {
    let label = node
        .get("label")
        .and_then(|v| v.as_str())
        .ok_or_else(|| CliError::ImportExport("node missing 'label'".into()))?;
    let props = node.get("properties").and_then(|v| v.as_object());
    let mut buf = String::with_capacity(256);
    buf.push_str("CREATE (:");
    write_gql_identifier(label, "node label", &mut buf)?;
    write_json_props_to_buf(props, &mut buf)?;
    buf.push(')');
    Ok(buf)
}

/// Convert a single JSON edge value to a GQL MATCH...CREATE statement.
fn edge_value_to_gql_stmt(edge: &serde_json::Value) -> Result<String, CliError> {
    let rel_label = edge
        .get("label")
        .and_then(|v| v.as_str())
        .ok_or_else(|| CliError::ImportExport("edge missing 'label'".into()))?;
    let mut buf = String::with_capacity(256);
    buf.push_str("MATCH (a");
    write_endpoint_match(edge, "source", &mut buf)?;
    buf.push_str("), (b");
    write_endpoint_match(edge, "target", &mut buf)?;
    buf.push_str(") CREATE (a)-[:");
    write_gql_identifier(rel_label, "edge label", &mut buf)?;

    let rel_props = edge.get("properties").and_then(|v| v.as_object());
    write_json_props_to_buf(rel_props, &mut buf)?;

    buf.push_str("]->(b)");
    Ok(buf)
}

// --- Streaming JSON serde types ---

use serde::de::{DeserializeSeed, Deserializer, MapAccess, SeqAccess, Visitor};

/// Seed that streams array elements, converting each to a GQL statement.
struct JsonArrayStreamSeed<'a, F> {
    on_stmt: &'a mut F,
    converter: fn(&serde_json::Value) -> Result<String, CliError>,
    count: &'a mut usize,
}

impl<'de, F> DeserializeSeed<'de> for JsonArrayStreamSeed<'_, F>
where
    F: FnMut(String) -> Result<(), CliError>,
{
    type Value = ();

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<(), D::Error> {
        deserializer.deserialize_seq(self)
    }
}

impl<'de, F> Visitor<'de> for JsonArrayStreamSeed<'_, F>
where
    F: FnMut(String) -> Result<(), CliError>,
{
    type Value = ();

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("an array of JSON objects")
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<(), A::Error> {
        while let Some(elem) = seq.next_element::<serde_json::Value>()? {
            let stmt = (self.converter)(&elem).map_err(serde::de::Error::custom)?;
            (self.on_stmt)(stmt).map_err(serde::de::Error::custom)?;
            *self.count += 1;
        }
        Ok(())
    }
}

/// Top-level visitor that processes `{"nodes": [...], "edges": [...]}`.
struct JsonRootVisitor<'a, F> {
    on_stmt: &'a mut F,
    count: &'a mut usize,
}

impl<'de, F> Visitor<'de> for JsonRootVisitor<'_, F>
where
    F: FnMut(String) -> Result<(), CliError>,
{
    type Value = bool; // true if any elements were found

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a JSON object with 'nodes' and/or 'edges' arrays")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<bool, A::Error> {
        let mut found_any = false;

        while let Some(key) = map.next_key::<String>()? {
            match key.as_str() {
                "nodes" => {
                    let before = *self.count;
                    map.next_value_seed(JsonArrayStreamSeed {
                        on_stmt: self.on_stmt,
                        converter: node_value_to_gql_stmt,
                        count: self.count,
                    })?;
                    if *self.count > before {
                        found_any = true;
                    }
                }
                "edges" => {
                    let before = *self.count;
                    map.next_value_seed(JsonArrayStreamSeed {
                        on_stmt: self.on_stmt,
                        converter: edge_value_to_gql_stmt,
                        count: self.count,
                    })?;
                    if *self.count > before {
                        found_any = true;
                    }
                }
                _ => {
                    map.next_value::<serde_json::Value>()?;
                }
            }
        }

        Ok(found_any)
    }
}

/// Seed wrapper to start root-level deserialization.
struct JsonRootSeed<'a, F> {
    on_stmt: &'a mut F,
    count: &'a mut usize,
}

impl<'de, F> DeserializeSeed<'de> for JsonRootSeed<'_, F>
where
    F: FnMut(String) -> Result<(), CliError>,
{
    type Value = bool;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<bool, D::Error> {
        deserializer.deserialize_map(JsonRootVisitor {
            on_stmt: self.on_stmt,
            count: self.count,
        })
    }
}

/// Stream JSON import from a reader, calling `on_stmt` for each node/edge statement.
///
/// This is the streaming equivalent of [`json_to_gql_statements`]: it reads a
/// `{"nodes": [...], "edges": [...]}` document one array element at a time using
/// serde's `DeserializeSeed` pattern, avoiding loading the entire DOM into memory.
///
/// Returns the number of statements emitted.
///
/// # Errors
///
/// Returns `CliError::ImportExport` on invalid JSON, missing fields, empty data,
/// or any error propagated from `on_stmt`.
pub fn stream_json_import<R: std::io::Read>(
    reader: R,
    mut on_stmt: impl FnMut(String) -> Result<(), CliError>,
) -> Result<usize, CliError> {
    let mut count = 0usize;

    let mut deserializer = serde_json::Deserializer::from_reader(reader);
    let found_any = JsonRootSeed {
        on_stmt: &mut on_stmt,
        count: &mut count,
    }
    .deserialize(&mut deserializer)
    .map_err(|e| CliError::ImportExport(format!("invalid JSON: {e}")))?;

    if !found_any {
        return Err(CliError::ImportExport(
            "no nodes or edges in JSON".into(),
        ));
    }

    Ok(count)
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
        write_json_value_to_buf(v, buf)?;
        first = false;
    }
    buf.push('}');
    Ok(())
}

/// Write a JSON value as a GQL literal into `buf`.
///
/// Arrays and objects are serialized as JSON strings (GQL does not have
/// native array/object literals). Use the caller's context to surface
/// a diagnostic if needed.
#[allow(clippy::unnecessary_wraps)] // Returns Result for consistency with write_gql_identifier chain
fn write_json_value_to_buf(v: &serde_json::Value, buf: &mut String) -> Result<(), CliError> {
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
    Ok(())
}

/// Write a MATCH clause for an edge endpoint into `buf`: `:Label {matchKey: 'matchVal'}`.
fn write_endpoint_match(
    edge: &serde_json::Value,
    endpoint_key: &str,
    buf: &mut String,
) -> Result<(), CliError> {
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

    if match_obj.len() != 1 {
        return Err(CliError::ImportExport(format!(
            "edge {endpoint_key}.match must have exactly one key, got {}",
            match_obj.len()
        )));
    }

    let Some((match_key, match_val)) = match_obj.iter().next() else {
        unreachable!("match_obj.len() == 1 guaranteed above");
    };

    if let Some(l) = label {
        buf.push(':');
        write_gql_identifier(l, &format!("edge {endpoint_key} label"), buf)?;
    }
    buf.push(' ');
    buf.push('{');
    write_gql_identifier(match_key, &format!("edge {endpoint_key} match key"), buf)?;
    buf.push_str(": ");
    write_json_value_to_buf(match_val, buf)?;
    buf.push('}');
    Ok(())
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
    use std::fmt::Write as _;

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

    // --- write_json_value_to_buf returns Result ---

    #[test]
    fn write_json_value_array_returns_ok() {
        use serde_json::json;
        let mut buf = String::new();
        let val = json!(["a", "b", "c"]);
        write_json_value_to_buf(&val, &mut buf).unwrap(); // OK: test
        assert_eq!(buf, r#"'["a","b","c"]'"#);
    }

    #[test]
    fn write_json_value_object_returns_ok() {
        use serde_json::json;
        let mut buf = String::new();
        let val = json!({"nested": "value"});
        write_json_value_to_buf(&val, &mut buf).unwrap(); // OK: test
        // serde_json preserves insertion order (IndexMap-backed Object).
        assert_eq!(buf, r#"'{"nested":"value"}'"#);
    }

    #[test]
    fn write_json_value_array_with_single_quote_is_escaped() {
        use serde_json::json;
        let mut buf = String::new();
        let val = json!(["O'Brien"]);
        write_json_value_to_buf(&val, &mut buf).unwrap(); // OK: test
        assert_eq!(buf, r#"'["O''Brien"]'"#);
    }

    // --- write_endpoint_match ---

    #[test]
    fn write_endpoint_match_empty_match_is_error() {
        use serde_json::json;
        let edge = json!({
            "source": { "label": "Person", "match": {} }
        });
        let mut buf = String::new();
        let result = write_endpoint_match(&edge, "source", &mut buf);
        assert!(result.is_err(), "empty match object must be rejected");
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

    // --- Streaming GQL import ---

    #[test]
    fn stream_gql_single_statement() {
        let input = std::io::Cursor::new("CREATE (:A);");
        let mut out = Vec::new();
        let count = stream_gql_import(input, |s| {
            out.push(s);
            Ok(())
        })
        .unwrap(); // OK: test
        assert_eq!(count, 1);
        assert_eq!(out, vec!["CREATE (:A)"]);
    }

    #[test]
    fn stream_gql_multiple_statements() {
        let input = std::io::Cursor::new("CREATE (:A);\nCREATE (:B);\nCREATE (:C);");
        let count = stream_gql_import(input, |_| Ok(())).unwrap(); // OK: test
        assert_eq!(count, 3);
    }

    #[test]
    fn stream_gql_skips_comments_and_blanks() {
        let input = std::io::Cursor::new("-- comment\n\n-- another\n");
        let mut out = Vec::new();
        let count = stream_gql_import(input, |s| {
            out.push(s);
            Ok(())
        })
        .unwrap(); // OK: test
        assert_eq!(count, 0);
        assert!(out.is_empty());
    }

    #[test]
    fn stream_gql_multiline_statement() {
        let input = std::io::Cursor::new("CREATE (:Person\n  {name: 'Alice'}\n);");
        let mut out = Vec::new();
        let count = stream_gql_import(input, |s| {
            out.push(s);
            Ok(())
        })
        .unwrap(); // OK: test
        assert_eq!(count, 1);
        assert!(out[0].contains("Person"));
        assert!(out[0].contains("Alice"));
    }

    #[test]
    fn stream_gql_no_trailing_semicolon() {
        let input = std::io::Cursor::new("CREATE (:X)");
        let mut out = Vec::new();
        let count = stream_gql_import(input, |s| {
            out.push(s);
            Ok(())
        })
        .unwrap(); // OK: test
        assert_eq!(count, 1);
        assert_eq!(out[0], "CREATE (:X)");
    }

    #[test]
    fn stream_gql_callback_error_propagates() {
        let input = std::io::Cursor::new("CREATE (:A);\nCREATE (:B);");
        let result = stream_gql_import(input, |_| {
            Err(CliError::ImportExport("stop".into()))
        });
        assert!(result.is_err());
    }

    #[test]
    fn stream_gql_parity_with_batch_corpus() {
        let corpus = "\
            CREATE (:A {x: 1});\n\
            CREATE (:B {y: 'hello'});\n\
            -- comment\n\
            \n\
            CREATE (:C);\n\
            CREATE (:D\n  {multi: 'line'}\n);\n\
            CREATE (:E);\n\
            CREATE (:F {z: 42});\n\
            CREATE (:G);\n\
            CREATE (:H);\n\
            CREATE (:I);\n\
            CREATE (:J);\n\
        ";
        let batch = split_gql_statements(corpus);
        let mut streaming = Vec::new();
        let count =
            stream_gql_import(std::io::Cursor::new(corpus), |s| {
                streaming.push(s);
                Ok(())
            })
            .unwrap(); // OK: test
        assert_eq!(count, batch.len());
        assert_eq!(streaming, batch);
    }

    // --- Streaming CSV import ---

    #[test]
    fn stream_csv_basic() {
        let csv = "label,name,age\nPerson,Alice,30\nPerson,Bob,25\n";
        let mut out = Vec::new();
        let count = stream_csv_import(std::io::Cursor::new(csv), |s| {
            out.push(s);
            Ok(())
        })
        .unwrap(); // OK: test
        assert_eq!(count, 2);
        assert!(out[0].contains(":Person"));
        assert!(out[0].contains("name: 'Alice'"));
        assert!(out[0].contains("age: 30"));
    }

    #[test]
    fn stream_csv_numeric_values() {
        let csv = "label,score,ratio\nItem,42,3.15\n";
        let mut out = Vec::new();
        stream_csv_import(std::io::Cursor::new(csv), |s| {
            out.push(s);
            Ok(())
        })
        .unwrap(); // OK: test
        assert!(out[0].contains("score: 42"));
        assert!(out[0].contains("ratio: 3.15"));
    }

    #[test]
    fn stream_csv_empty_props_omitted() {
        let csv = "label,name,age\nPerson,Alice,\n";
        let mut out = Vec::new();
        stream_csv_import(std::io::Cursor::new(csv), |s| {
            out.push(s);
            Ok(())
        })
        .unwrap(); // OK: test
        assert_eq!(out.len(), 1);
        assert!(!out[0].contains("age"));
    }

    #[test]
    fn stream_csv_empty_label_error() {
        let csv = "label,name\n,Alice\n";
        let result = stream_csv_import(std::io::Cursor::new(csv), |_| Ok(()));
        assert!(result.is_err());
    }

    #[test]
    fn stream_csv_no_data_rows_error() {
        let csv = "label,name\n";
        let result = stream_csv_import(std::io::Cursor::new(csv), |_| Ok(()));
        assert!(result.is_err());
    }

    #[test]
    fn stream_csv_callback_error_propagates() {
        let csv = "label,name\nPerson,Alice\nPerson,Bob\n";
        let result = stream_csv_import(std::io::Cursor::new(csv), |_| {
            Err(CliError::ImportExport("stop".into()))
        });
        assert!(result.is_err());
    }

    #[test]
    fn stream_csv_late_error_propagates_via_result() {
        // Row 3 has a label with double quote — write_gql_identifier rejects it.
        // The error must propagate as Err even though rows 1-2 succeeded.
        let csv = "label,name\nPerson,Alice\nPerson,Bob\nbad\"label,Mallory\n";
        let mut count = 0usize;
        let result = stream_csv_import(std::io::Cursor::new(csv), |_| {
            count += 1;
            Ok(())
        });
        assert!(
            result.is_err(),
            "late error (row 3) must propagate; count was {count}"
        );
        assert_eq!(count, 2, "first 2 rows should have been emitted before error");
    }

    #[test]
    fn stream_csv_parity_with_batch() {
        let csv = "label,name,age,score\n\
                    Person,Alice,30,100\n\
                    Person,Bob,25,200\n\
                    Person,O'Brien,40,300\n\
                    Item,Widget,,50\n\
                    Item,Gadget,10,\n";
        let batch = csv_nodes_to_gql(csv).unwrap(); // OK: test
        let mut streaming = Vec::new();
        let count = stream_csv_import(std::io::Cursor::new(csv), |s| {
            streaming.push(s);
            Ok(())
        })
        .unwrap(); // OK: test
        assert_eq!(count, batch.len());
        assert_eq!(streaming, batch);
    }

    // --- Streaming JSON import ---

    #[test]
    fn stream_json_nodes_only() {
        let json = r#"{"nodes": [
            {"label": "Person", "properties": {"name": "Alice"}},
            {"label": "Person", "properties": {"name": "Bob"}}
        ], "edges": []}"#;
        let mut out = Vec::new();
        let count = stream_json_import(std::io::Cursor::new(json), |s| {
            out.push(s);
            Ok(())
        })
        .unwrap(); // OK: test
        assert_eq!(count, 2);
        assert!(out[0].contains(":Person"));
        assert!(out[0].contains("name: 'Alice'"));
    }

    #[test]
    fn stream_json_edges_only() {
        // edges-only with no nodes: should produce edge stmts if edges are non-empty
        let json = r#"{"nodes": [], "edges": [
            {
                "source": {"label": "A", "match": {"id": "1"}},
                "target": {"label": "B", "match": {"id": "2"}},
                "label": "R",
                "properties": {}
            }
        ]}"#;
        let mut out = Vec::new();
        let count = stream_json_import(std::io::Cursor::new(json), |s| {
            out.push(s);
            Ok(())
        })
        .unwrap(); // OK: test
        assert_eq!(count, 1);
        assert!(out[0].contains("MATCH"));
    }

    #[test]
    fn stream_json_nodes_and_edges() {
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
        let mut out = Vec::new();
        let count = stream_json_import(std::io::Cursor::new(json), |s| {
            out.push(s);
            Ok(())
        })
        .unwrap(); // OK: test
        assert_eq!(count, 3);
        assert!(out[0].starts_with("CREATE"));
        assert!(out[1].starts_with("CREATE"));
        assert!(out[2].starts_with("MATCH"));
    }

    #[test]
    fn stream_json_single_quote_escaped() {
        let json = r#"{"nodes": [
            {"label": "Person", "properties": {"name": "O'Brien"}}
        ], "edges": []}"#;
        let mut out = Vec::new();
        stream_json_import(std::io::Cursor::new(json), |s| {
            out.push(s);
            Ok(())
        })
        .unwrap(); // OK: test
        assert!(out[0].contains("O''Brien"), "got: {}", out[0]);
    }

    #[test]
    fn stream_json_boolean_null() {
        let json = r#"{"nodes": [
            {"label": "Item", "properties": {"active": true, "deleted": false, "note": null}}
        ], "edges": []}"#;
        let mut out = Vec::new();
        stream_json_import(std::io::Cursor::new(json), |s| {
            out.push(s);
            Ok(())
        })
        .unwrap(); // OK: test
        assert!(out[0].contains("active: true"), "got: {}", out[0]);
        assert!(out[0].contains("deleted: false"), "got: {}", out[0]);
        assert!(out[0].contains("note: null"), "got: {}", out[0]);
    }

    #[test]
    fn stream_json_invalid_json_error() {
        let result = stream_json_import(std::io::Cursor::new("not json"), |_| Ok(()));
        assert!(result.is_err());
    }

    #[test]
    fn stream_json_empty_arrays_error() {
        let json = r#"{"nodes": [], "edges": []}"#;
        let result = stream_json_import(std::io::Cursor::new(json), |_| Ok(()));
        assert!(result.is_err());
    }

    #[test]
    fn stream_json_callback_error_propagates() {
        let json = r#"{"nodes": [
            {"label": "A", "properties": {}},
            {"label": "B", "properties": {}}
        ], "edges": []}"#;
        let result = stream_json_import(std::io::Cursor::new(json), |_| {
            Err(CliError::ImportExport("stop".into()))
        });
        assert!(result.is_err());
    }

    #[test]
    fn stream_json_parity_with_batch() {
        let json = r#"{"nodes": [
            {"label": "Person", "properties": {"id": "1", "name": "Alice", "age": 30}},
            {"label": "Person", "properties": {"id": "2", "name": "Bob", "age": 25}},
            {"label": "City", "properties": {"id": "3", "name": "Madrid"}},
            {"label": "City", "properties": {"id": "4", "name": "London"}},
            {"label": "Item", "properties": {"id": "5", "active": true}}
        ], "edges": [
            {
                "source": {"label": "Person", "match": {"id": "1"}},
                "target": {"label": "Person", "match": {"id": "2"}},
                "label": "KNOWS",
                "properties": {"since": 2024}
            },
            {
                "source": {"label": "Person", "match": {"id": "1"}},
                "target": {"label": "City", "match": {"id": "3"}},
                "label": "LIVES_IN",
                "properties": {}
            }
        ]}"#;
        let batch = json_to_gql_statements(json).unwrap(); // OK: test
        let mut streaming = Vec::new();
        let count = stream_json_import(std::io::Cursor::new(json), |s| {
            streaming.push(s);
            Ok(())
        })
        .unwrap(); // OK: test
        assert_eq!(count, batch.len());
        assert_eq!(streaming, batch);
    }

    // --- Throughput regression guards ---

    const THROUGHPUT_ELEMENT_COUNT: usize = 10_000;
    // Generous in debug (CI runners), strict in release
    const THROUGHPUT_TIMEOUT: std::time::Duration = if cfg!(debug_assertions) {
        std::time::Duration::from_secs(10)
    } else {
        std::time::Duration::from_secs(2)
    };

    #[test]
    fn throughput_stream_gql_10k() {
        let mut input = String::new();
        for i in 0..THROUGHPUT_ELEMENT_COUNT {
            writeln!(input, "CREATE (:Node {{id: {i}, name: 'node_{i}'}});").expect("write to String"); // OK: test
        }
        let start = std::time::Instant::now();
        let mut count = 0usize;
        stream_gql_import(std::io::Cursor::new(input), |_| {
            count += 1;
            Ok(())
        })
        .unwrap(); // OK: test
        let elapsed = start.elapsed();
        assert_eq!(count, THROUGHPUT_ELEMENT_COUNT);
        assert!(
            elapsed < THROUGHPUT_TIMEOUT,
            "stream_gql_import took {elapsed:?} for {THROUGHPUT_ELEMENT_COUNT} elements (limit: {THROUGHPUT_TIMEOUT:?})"
        );
    }

    #[test]
    fn throughput_stream_csv_10k() {
        let mut input = String::from("label,id,name,score\n");
        for i in 0..THROUGHPUT_ELEMENT_COUNT {
            writeln!(input, "Item,{i},item_{i},{}", i * 10).expect("write to String"); // OK: test
        }
        let start = std::time::Instant::now();
        let mut count = 0usize;
        stream_csv_import(std::io::Cursor::new(input), |_| {
            count += 1;
            Ok(())
        })
        .unwrap(); // OK: test
        let elapsed = start.elapsed();
        assert_eq!(count, THROUGHPUT_ELEMENT_COUNT);
        assert!(
            elapsed < THROUGHPUT_TIMEOUT,
            "stream_csv_import took {elapsed:?} for {THROUGHPUT_ELEMENT_COUNT} elements (limit: {THROUGHPUT_TIMEOUT:?})"
        );
    }

    #[test]
    fn throughput_stream_json_10k() {
        let mut nodes = Vec::with_capacity(THROUGHPUT_ELEMENT_COUNT);
        for i in 0..THROUGHPUT_ELEMENT_COUNT {
            nodes.push(format!(
                r#"{{"label":"Node","properties":{{"id":{i},"name":"node_{i}"}}}}"#
            ));
        }
        let json = format!(r#"{{"nodes":[{}],"edges":[]}}"#, nodes.join(","));
        let start = std::time::Instant::now();
        let mut count = 0usize;
        stream_json_import(std::io::Cursor::new(json), |_| {
            count += 1;
            Ok(())
        })
        .unwrap(); // OK: test
        let elapsed = start.elapsed();
        assert_eq!(count, THROUGHPUT_ELEMENT_COUNT);
        assert!(
            elapsed < THROUGHPUT_TIMEOUT,
            "stream_json_import took {elapsed:?} for {THROUGHPUT_ELEMENT_COUNT} elements (limit: {THROUGHPUT_TIMEOUT:?})"
        );
    }

    // --- H4: CSV label must pass through write_gql_identifier ---

    #[test]
    fn csv_nodes_label_with_space_uses_delimited_identifier() {
        let csv = "label,name\nMy Type,Alice\n";
        let stmts = csv_nodes_to_gql(csv).expect("csv to gql"); // OK: test
        assert!(
            stmts[0].contains(":\"My Type\""),
            "expected delimited identifier, got: {}",
            stmts[0]
        );
    }

    #[test]
    fn csv_nodes_label_injection_attempt_uses_delimited_identifier() {
        let csv = "label,name\nX {admin: true},Alice\n";
        let stmts = csv_nodes_to_gql(csv).expect("csv to gql"); // OK: test
        assert!(
            stmts[0].contains(":\"X {admin: true}\""),
            "expected delimited identifier to neutralize injection, got: {}",
            stmts[0]
        );
        assert!(stmts[0].contains("name: 'Alice'"), "got: {}", stmts[0]);
    }

    #[test]
    fn csv_nodes_label_with_double_quote_is_error() {
        let csv = "label,name\nbad\"label,Alice\n";
        let result = csv_nodes_to_gql(csv);
        assert!(result.is_err(), "double-quote in label must be rejected");
    }

    #[test]
    fn stream_csv_label_with_space_uses_delimited_identifier() {
        let csv = "label,name\nMy Type,Alice\n";
        let mut out = Vec::new();
        stream_csv_import(std::io::Cursor::new(csv), |s| {
            out.push(s);
            Ok(())
        })
        .expect("stream csv"); // OK: test
        assert!(
            out[0].contains(":\"My Type\""),
            "expected delimited identifier, got: {}",
            out[0]
        );
    }

    #[test]
    fn stream_csv_label_injection_attempt_uses_delimited_identifier() {
        let csv = "label,name\nX {admin: true},Alice\n";
        let mut out = Vec::new();
        stream_csv_import(std::io::Cursor::new(csv), |s| {
            out.push(s);
            Ok(())
        })
        .expect("stream csv"); // OK: test
        assert!(
            out[0].contains(":\"X {admin: true}\""),
            "expected delimited identifier, got: {}",
            out[0]
        );
    }

    #[test]
    fn stream_csv_label_with_double_quote_is_error() {
        let csv = "label,name\nbad\"label,Alice\n";
        let result = stream_csv_import(std::io::Cursor::new(csv), |_| Ok(()));
        assert!(result.is_err(), "double-quote in label must be rejected");
    }
}
