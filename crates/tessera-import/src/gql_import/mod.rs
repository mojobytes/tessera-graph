// Copyright 2026 BelowZero Security OU. All rights reserved.

//! GQL statement import for `TesseraGraph`.

use tessera_graph::{GqlStatement, Graph};

use crate::error::{ImportError, ImportResult};

/// Summary returned after a successful GQL import.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GqlImportSummary {
    /// Total number of GQL statements executed.
    pub statements_executed: usize,
    /// Total nodes created across all statements.
    pub nodes_created: u64,
    /// Total edges created across all statements.
    pub edges_created: u64,
}

/// Import GQL statements from a text string.
///
/// Each non-blank, non-comment line is treated as a GQL statement. Comments
/// are lines that start with `//` or `--` (after trimming whitespace).
///
/// Only mutation statements (CREATE, SET, DELETE, MERGE) are executed. Read-
/// only MATCH queries are silently skipped.
///
/// # Errors
///
/// Returns [`ImportError::GqlStatement`] if a statement cannot be parsed or
/// executed, including the 1-based line number and error description.
pub fn import_gql(graph: &mut Graph, gql_text: &str) -> ImportResult<GqlImportSummary> {
    let mut summary = GqlImportSummary::default();

    for (idx, line) in gql_text.lines().enumerate() {
        let line_num = idx + 1;
        let trimmed = line.trim();

        // Skip blank lines and comments.
        if trimmed.is_empty() || trimmed.starts_with("//") || trimmed.starts_with("--") {
            continue;
        }

        let stmt = tessera_graph::gql::parse_statement(trimmed).map_err(|e| {
            ImportError::GqlStatement {
                line: line_num,
                reason: e.to_string(),
            }
        })?;

        match stmt {
            GqlStatement::Mutation(m) => {
                let result =
                    tessera_storage_enterprise::gql::execute_mut(graph, &m).map_err(|e| {
                        ImportError::GqlStatement {
                            line: line_num,
                            reason: e.to_string(),
                        }
                    })?;
                summary.statements_executed += 1;
                summary.nodes_created += result.nodes_created;
                summary.edges_created += result.edges_created;
            }
            GqlStatement::Query(_) => {
                // Read-only queries are silently skipped.
            }
        }
    }

    Ok(summary)
}
