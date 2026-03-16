//! Fidelity modes for context carryover between pipeline nodes.
//!
//! Controls how much of a predecessor node's output is carried forward:
//! - Full: entire output
//! - Truncate(n): first n characters
//! - Compact: strip tool call details, keep results
//! - Summary(n): first n lines as a summary

use serde::{Deserialize, Serialize};

/// Maximum allowed `max_chars` for truncation (10 MB).
const MAX_TRUNCATE_CHARS: usize = 10_000_000;

/// Maximum allowed `max_lines` for summary.
const MAX_SUMMARY_LINES: usize = 100_000;

/// Fidelity mode controlling context carryover between nodes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FidelityMode {
    /// Pass the full output unchanged.
    #[default]
    Full,
    /// Truncate to at most `max_chars` characters.
    Truncate { max_chars: usize },
    /// Strip tool call arguments, keep tool results and final output.
    Compact,
    /// Keep only the first `max_lines` lines.
    Summary { max_lines: usize },
}

impl FidelityMode {
    /// Parse a fidelity mode from a DOT attribute string.
    ///
    /// Formats: "full", "compact", "truncate:N", "summary:N"
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim();
        match s {
            "full" => Some(Self::Full),
            "compact" => Some(Self::Compact),
            _ if s.starts_with("truncate:") => {
                s["truncate:".len()..]
                    .parse::<usize>()
                    .ok()
                    .map(|n| Self::Truncate {
                        max_chars: n.min(MAX_TRUNCATE_CHARS),
                    })
            }
            _ if s.starts_with("summary:") => {
                s["summary:".len()..]
                    .parse::<usize>()
                    .ok()
                    .map(|n| Self::Summary {
                        max_lines: n.min(MAX_SUMMARY_LINES),
                    })
            }
            _ => None,
        }
    }

    /// Apply the fidelity mode to an output string.
    pub fn apply(&self, output: &str) -> String {
        match self {
            Self::Full => output.to_string(),
            Self::Truncate { max_chars } => {
                if output.len() <= *max_chars {
                    output.to_string()
                } else {
                    // Truncate at char boundary
                    let mut end = *max_chars;
                    while end > 0 && !output.is_char_boundary(end) {
                        end -= 1;
                    }
                    let mut result = output[..end].to_string();
                    result.push_str("\n... [truncated]");
                    result
                }
            }
            Self::Compact => compact_output(output),
            Self::Summary { max_lines } => {
                let lines: Vec<&str> = output.lines().take(*max_lines).collect();
                let mut result = lines.join("\n");
                // Check if there are more lines without counting them all
                let has_more = output.lines().nth(*max_lines).is_some();
                if has_more {
                    result.push_str("\n... [truncated]");
                }
                result
            }
        }
    }
}

/// Strip tool call blocks from output, keeping results and final text.
///
/// Recognizes lines prefixed with "Tool call: " and "Arguments: " as tool
/// invocation blocks, and "Result: " / "Output: " as result lines.
/// This heuristic works on text-formatted agent output (e.g. pipeline run
/// summaries), not on structured `Message` types.
fn compact_output(output: &str) -> String {
    let mut result = Vec::new();
    let mut in_tool_call = false;

    for line in output.lines() {
        if line.starts_with("Tool call: ") || line.starts_with("Arguments: ") {
            in_tool_call = true;
            continue;
        }
        if line.starts_with("Result: ") || line.starts_with("Output: ") {
            in_tool_call = false;
            result.push(line);
            continue;
        }
        if !in_tool_call {
            result.push(line);
        }
    }

    result.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_parse_full() {
        assert_eq!(FidelityMode::parse("full"), Some(FidelityMode::Full));
    }

    #[test]
    fn should_parse_compact() {
        assert_eq!(FidelityMode::parse("compact"), Some(FidelityMode::Compact));
    }

    #[test]
    fn should_parse_truncate() {
        assert_eq!(
            FidelityMode::parse("truncate:1000"),
            Some(FidelityMode::Truncate { max_chars: 1000 })
        );
    }

    #[test]
    fn should_parse_summary() {
        assert_eq!(
            FidelityMode::parse("summary:5"),
            Some(FidelityMode::Summary { max_lines: 5 })
        );
    }

    #[test]
    fn should_reject_invalid() {
        assert_eq!(FidelityMode::parse("unknown"), None);
        assert_eq!(FidelityMode::parse("truncate:abc"), None);
    }

    #[test]
    fn should_apply_full() {
        let mode = FidelityMode::Full;
        assert_eq!(mode.apply("hello world"), "hello world");
    }

    #[test]
    fn should_apply_truncate() {
        let mode = FidelityMode::Truncate { max_chars: 5 };
        let result = mode.apply("hello world");
        assert!(result.starts_with("hello"));
        assert!(result.contains("[truncated]"));
    }

    #[test]
    fn should_apply_summary() {
        let mode = FidelityMode::Summary { max_lines: 2 };
        let input = "line1\nline2\nline3\nline4";
        let result = mode.apply(input);
        assert!(result.starts_with("line1\nline2"));
        assert!(result.contains("[truncated]"));
    }

    #[test]
    fn should_apply_compact() {
        let input = "Start\nTool call: shell\nArguments: {\"cmd\":\"ls\"}\nResult: file.rs\nEnd";
        let result = FidelityMode::Compact.apply(input);
        assert!(result.contains("Start"));
        assert!(result.contains("Result: file.rs"));
        assert!(result.contains("End"));
        assert!(!result.contains("Tool call:"));
        assert!(!result.contains("Arguments:"));
    }

    #[test]
    fn should_default_to_full() {
        assert_eq!(FidelityMode::default(), FidelityMode::Full);
    }
}
