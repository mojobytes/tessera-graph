// Copyright 2026 BelowZero Security OU. All rights reserved.

use crate::output::OutputFormat;

/// Meta-commands available in the REPL (prefixed with `\`).
#[derive(Debug, PartialEq, Eq)]
pub enum MetaCommand {
    Quit,
    Help,
    SetFormat(OutputFormat),
    SetLanguage(String),
    SetTiming(bool),
    Clear,
    Unknown(String),
}

/// Parse a line as a meta-command.
///
/// Returns `None` if the line does not start with `\`.
#[must_use]
pub fn parse_meta_command(input: &str) -> Option<MetaCommand> {
    let trimmed = input.trim();
    if !trimmed.starts_with('\\') {
        return None;
    }

    let without_prefix = &trimmed[1..];
    let mut parts = without_prefix.splitn(2, char::is_whitespace);
    let cmd = parts.next().unwrap_or(""); // OK: splitn always yields at least one element
    let arg = parts.next().map(str::trim);

    Some(match cmd {
        "q" | "quit" => MetaCommand::Quit,
        "h" | "?" | "help" => MetaCommand::Help,
        "format" => {
            let Some(fmt_str) = arg else {
                return Some(MetaCommand::Unknown("\\format requires an argument (table, json, csv)".to_owned()));
            };
            fmt_str.parse::<OutputFormat>().map_or_else(
                |_| MetaCommand::Unknown(format!("unknown format: {fmt_str}")),
                MetaCommand::SetFormat,
            )
        }
        "l" | "language" => {
            let Some(lang) = arg else {
                return Some(MetaCommand::Unknown("\\l requires an argument (gql, cypher)".to_owned()));
            };
            MetaCommand::SetLanguage(lang.to_owned())
        }
        "timing" => match arg {
            Some("on") => MetaCommand::SetTiming(true),
            Some("off") => MetaCommand::SetTiming(false),
            _ => MetaCommand::Unknown("\\timing requires on|off".to_owned()),
        },
        "clear" => MetaCommand::Clear,
        other => MetaCommand::Unknown(format!("unknown command: \\{other}")),
    })
}

/// Accumulates multi-line queries in the REPL.
///
/// A query is considered complete when:
/// - A line ends with `;` (semicolon convention, like `psql`)
/// - An empty line is entered after non-empty input
#[derive(Debug)]
pub struct QueryAccumulator {
    lines: Vec<String>,
}

impl QueryAccumulator {
    /// Create a new empty accumulator.
    #[must_use]
    pub const fn new() -> Self {
        Self { lines: Vec::new() }
    }

    /// Push a line of input. Returns `Some(query)` if the query is now complete.
    pub fn push(&mut self, line: &str) -> Option<String> {
        // Empty line on empty buffer → no-op
        if line.is_empty() && self.lines.is_empty() {
            return None;
        }

        // Empty line with pending input → complete
        if line.is_empty() {
            return Some(self.drain());
        }

        // Line ending with `;` → push and complete
        if line.trim_end().ends_with(';') {
            let stripped = line.trim_end().trim_end_matches(';').trim_end();
            self.lines.push(stripped.to_owned());
            return Some(self.drain());
        }

        // Otherwise, accumulate
        self.lines.push(line.to_owned());
        None
    }

    /// Whether there is pending (incomplete) input.
    #[must_use]
    pub fn is_pending(&self) -> bool {
        !self.lines.is_empty()
    }

    /// Join and clear accumulated lines.
    fn drain(&mut self) -> String {
        let query = self.lines.join("\n");
        self.lines.clear();
        query
    }
}

impl Default for QueryAccumulator {
    fn default() -> Self {
        Self::new()
    }
}

/// Build the REPL prompt string.
///
/// When `continuation` is false, produces: `tessera[user@host:port]> `
/// When `continuation` is true, produces a prompt of the same length ending with `-> `.
#[must_use]
pub fn format_prompt(username: &str, host: &str, port: u16, continuation: bool) -> String {
    let primary = format!("tessera[{username}@{host}:{port}]> ");
    if continuation {
        // Pad to same length, ending with "-> "
        let pad = primary.len().saturating_sub(3);
        format!("{:>pad$}-> ", "")
    } else {
        primary
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Meta-command parser ---

    #[test]
    fn quit_meta_command() {
        assert_eq!(parse_meta_command("\\q"), Some(MetaCommand::Quit));
    }

    #[test]
    fn quit_full_name() {
        assert_eq!(parse_meta_command("\\quit"), Some(MetaCommand::Quit));
    }

    #[test]
    fn help_aliases() {
        assert_eq!(parse_meta_command("\\h"), Some(MetaCommand::Help));
        assert_eq!(parse_meta_command("\\?"), Some(MetaCommand::Help));
        assert_eq!(parse_meta_command("\\help"), Some(MetaCommand::Help));
    }

    #[test]
    fn format_meta_command() {
        assert_eq!(
            parse_meta_command("\\format json"),
            Some(MetaCommand::SetFormat(OutputFormat::Json))
        );
    }

    #[test]
    fn format_table() {
        assert_eq!(
            parse_meta_command("\\format table"),
            Some(MetaCommand::SetFormat(OutputFormat::Table))
        );
    }

    #[test]
    fn format_csv() {
        assert_eq!(
            parse_meta_command("\\format csv"),
            Some(MetaCommand::SetFormat(OutputFormat::Csv))
        );
    }

    #[test]
    fn format_without_arg_is_unknown() {
        assert!(matches!(
            parse_meta_command("\\format"),
            Some(MetaCommand::Unknown(_))
        ));
    }

    #[test]
    fn format_invalid_is_unknown() {
        assert!(matches!(
            parse_meta_command("\\format xml"),
            Some(MetaCommand::Unknown(_))
        ));
    }

    #[test]
    fn language_meta_command() {
        assert_eq!(
            parse_meta_command("\\l cypher"),
            Some(MetaCommand::SetLanguage("cypher".to_owned()))
        );
    }

    #[test]
    fn language_full_name() {
        assert_eq!(
            parse_meta_command("\\language gql"),
            Some(MetaCommand::SetLanguage("gql".to_owned()))
        );
    }

    #[test]
    fn language_without_arg_is_unknown() {
        assert!(matches!(
            parse_meta_command("\\l"),
            Some(MetaCommand::Unknown(_))
        ));
    }

    #[test]
    fn timing_on_off() {
        assert_eq!(
            parse_meta_command("\\timing on"),
            Some(MetaCommand::SetTiming(true))
        );
        assert_eq!(
            parse_meta_command("\\timing off"),
            Some(MetaCommand::SetTiming(false))
        );
    }

    #[test]
    fn timing_without_arg_is_unknown() {
        assert!(matches!(
            parse_meta_command("\\timing"),
            Some(MetaCommand::Unknown(_))
        ));
    }

    #[test]
    fn clear_meta_command() {
        assert_eq!(parse_meta_command("\\clear"), Some(MetaCommand::Clear));
    }

    #[test]
    fn unknown_meta_command() {
        assert!(matches!(
            parse_meta_command("\\xyz"),
            Some(MetaCommand::Unknown(_))
        ));
    }

    #[test]
    fn non_meta_returns_none() {
        assert_eq!(parse_meta_command("MATCH (n) RETURN n"), None);
    }

    #[test]
    fn whitespace_only_is_not_meta() {
        assert_eq!(parse_meta_command("   "), None);
    }

    // --- Query accumulator ---

    #[test]
    fn single_line_with_semicolon_is_complete() {
        let mut acc = QueryAccumulator::new();
        let result = acc.push("MATCH (n) RETURN n;");
        assert_eq!(result, Some("MATCH (n) RETURN n".to_owned()));
    }

    #[test]
    fn line_without_semicolon_is_incomplete() {
        let mut acc = QueryAccumulator::new();
        let result = acc.push("MATCH (n)");
        assert_eq!(result, None);
        assert!(acc.is_pending());
    }

    #[test]
    fn multiline_completed_by_semicolon() {
        let mut acc = QueryAccumulator::new();
        assert_eq!(acc.push("MATCH (n)"), None);
        let result = acc.push("RETURN n;");
        assert_eq!(result, Some("MATCH (n)\nRETURN n".to_owned()));
    }

    #[test]
    fn empty_line_completes_pending_query() {
        let mut acc = QueryAccumulator::new();
        acc.push("MATCH (n) RETURN n");
        let result = acc.push("");
        assert_eq!(result, Some("MATCH (n) RETURN n".to_owned()));
    }

    #[test]
    fn empty_line_on_empty_buffer_is_noop() {
        let mut acc = QueryAccumulator::new();
        let result = acc.push("");
        assert_eq!(result, None);
        assert!(!acc.is_pending());
    }

    #[test]
    fn accumulator_clears_after_completion() {
        let mut acc = QueryAccumulator::new();
        acc.push("SELECT 1;");
        assert!(!acc.is_pending());
    }

    #[test]
    fn semicolon_with_trailing_spaces() {
        let mut acc = QueryAccumulator::new();
        let result = acc.push("MATCH (n) RETURN n;   ");
        assert_eq!(result, Some("MATCH (n) RETURN n".to_owned()));
    }

    #[test]
    fn three_line_query() {
        let mut acc = QueryAccumulator::new();
        assert_eq!(acc.push("MATCH (n:Person)"), None);
        assert_eq!(acc.push("WHERE n.age > 30"), None);
        let result = acc.push("RETURN n.name;");
        assert_eq!(
            result,
            Some("MATCH (n:Person)\nWHERE n.age > 30\nRETURN n.name".to_owned())
        );
    }

    // --- Prompt builder ---

    #[test]
    fn primary_prompt_format() {
        let p = format_prompt("admin", "localhost", 7687, false);
        assert_eq!(p, "tessera[admin@localhost:7687]> ");
    }

    #[test]
    fn continuation_prompt_is_aligned() {
        let primary = format_prompt("admin", "localhost", 7687, false);
        let continuation = format_prompt("admin", "localhost", 7687, true);
        assert!(continuation.ends_with("-> "));
        assert_eq!(primary.len(), continuation.len());
    }

    #[test]
    fn prompt_with_different_user() {
        let p = format_prompt("bob", "db.prod", 9000, false);
        assert_eq!(p, "tessera[bob@db.prod:9000]> ");
    }

    #[test]
    fn continuation_with_different_user() {
        let primary = format_prompt("bob", "db.prod", 9000, false);
        let cont = format_prompt("bob", "db.prod", 9000, true);
        assert_eq!(primary.len(), cont.len());
        assert!(cont.ends_with("-> "));
    }
}
