// Copyright 2026 BelowZero Security OU. All rights reserved.

use crate::error::CliError;

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

        // Skip blank lines and comments
        if trimmed.is_empty() || trimmed.starts_with("--") {
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
    pub statements: Vec<String>,
    pub batch_size: usize,
}

impl ImportPlan {
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
        self.statements
            .len()
            .div_ceil(self.batch_size)
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
        let record =
            result.map_err(|e| CliError::ImportExport(format!("invalid CSV row: {e}")))?;

        let label = record
            .get(0)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                CliError::ImportExport(format!("empty {label_col} (label) in CSV row"))
            })?;

        let mut props = Vec::new();
        for (i, col) in prop_cols.iter().enumerate() {
            if let Some(val) = record.get(i + 1).filter(|s| !s.is_empty()) {
                // Try to parse as number, otherwise use string
                let formatted = if val.parse::<i64>().is_ok() || val.parse::<f64>().is_ok() {
                    format!("{col}: {val}")
                } else {
                    format!("{col}: '{}'", val.replace('\'', "\\'"))
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

/// Truncate a single-line string for display, replacing newlines.
fn truncate_line(s: &str, max: usize) -> String {
    let oneline = s.replace('\n', " ");
    if oneline.len() <= max {
        oneline
    } else {
        format!("{}...", &oneline[..max.saturating_sub(3)])
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
        let plan = ImportPlan::from_gql_content(
            "CREATE (:A);\nCREATE (:B);\nCREATE (:C);",
            2,
        )
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
        let plan = ImportPlan::from_gql_content("CREATE (:A);\nCREATE (:B);", 100)
            .expect("plan"); // OK: test
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
        assert!(stmts[0].contains("O\\'Brien"));
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
}
