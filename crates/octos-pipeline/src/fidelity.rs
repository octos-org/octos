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

/// Gap 3.4 — DEFAULT result-size ceiling (in bytes) applied to a pipeline
/// result that did NOT explicitly annotate a [`FidelityMode`].
///
/// "Hard limits are cliffs": a pipeline that emits a huge result can produce
/// an unbounded frame that trips the 1 MiB `MAX_TEXT_FRAME_BYTES` wedge
/// (`frame_too_large`). This producer-side cap guarantees that an
/// UN-annotated pipeline result DEGRADES (truncates with a marker) rather
/// than wedging at the frame layer.
///
/// 256 KiB chosen for ample headroom under the 1 MiB frame cap: the tool
/// result still appends a per-node execution-summary footer, and the whole
/// string is then JSON-escaped + wrapped in an RPC envelope before hitting
/// `MAX_TEXT_FRAME_BYTES` — escaping alone can up to ~2x pathological
/// content. 256 KiB leaves ~768 KiB of slack for footer + escaping +
/// envelope. It also matches the existing pipeline-server `MAX_INPUT_SIZE`
/// (262_144) so the input and output caps are symmetric.
pub const DEFAULT_RESULT_CEILING_BYTES: usize = 262_144;

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

/// Gap 3.4 — bound a pipeline result string, preferring the pipeline's
/// explicitly-declared [`FidelityMode`] and otherwise applying the DEFAULT
/// byte ceiling so an un-annotated result can never emit an unbounded frame.
///
/// Semantics:
/// * `declared = Some(mode)` — the pipeline annotated a fidelity mode; it
///   WINS. We apply it verbatim (existing `FidelityMode::apply` semantics).
///   An explicit `Full` is an explicit opt-out of the default ceiling.
/// * `declared = None` and `output.len() > DEFAULT_RESULT_CEILING_BYTES` —
///   no annotation and over budget: truncate at a UTF-8 boundary to the
///   ceiling and append a `\n... [truncated: N of M bytes]` marker so the
///   degradation is never silent.
/// * `declared = None` and within budget — returned unchanged (no false
///   truncation).
///
/// Producer-side only: the returned string is what becomes the tool result;
/// the frame layer (Gap 3.1) is untouched here.
pub fn apply_result_ceiling(output: &str, declared: Option<&FidelityMode>) -> String {
    if let Some(mode) = declared {
        // Explicit annotation wins — including an explicit `Full` opt-out.
        return mode.apply(output);
    }
    let total = output.len();
    if total <= DEFAULT_RESULT_CEILING_BYTES {
        return output.to_string();
    }
    // Over the default ceiling with no annotation: keep the head, mark the
    // drop. Walk back to a char boundary so we never split a UTF-8 scalar.
    let mut end = DEFAULT_RESULT_CEILING_BYTES;
    while end > 0 && !output.is_char_boundary(end) {
        end -= 1;
    }
    let mut result = output[..end].to_string();
    result.push_str(&format!("\n... [truncated: {end} of {total} bytes]"));
    result
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

    // ---- Gap 3.4: default pipeline-result ceiling ----

    /// An un-annotated result that EXCEEDS the default ceiling is bounded to
    /// at most the ceiling-plus-marker and carries an explicit truncation
    /// marker — never silently dropped, never unbounded.
    #[test]
    fn should_truncate_unannotated_result_over_ceiling_with_marker() {
        let total = DEFAULT_RESULT_CEILING_BYTES + 50_000;
        let input = "a".repeat(total);
        let out = apply_result_ceiling(&input, None);
        assert!(
            out.len() <= DEFAULT_RESULT_CEILING_BYTES + 64,
            "bounded to ~ceiling (+ short marker), got {} bytes",
            out.len()
        );
        assert!(
            out.contains(&format!(
                "[truncated: {DEFAULT_RESULT_CEILING_BYTES} of {total} bytes]"
            )),
            "must carry a byte-accurate truncation marker; got tail: {:?}",
            &out[out.len().saturating_sub(80)..]
        );
        assert!(out.starts_with("aaaa"), "must keep the head of the output");
    }

    /// An explicit FidelityMode annotation WINS over the default ceiling —
    /// here a tighter `truncate:100` bounds far below the default.
    #[test]
    fn should_let_explicit_fidelity_win_over_default_ceiling() {
        let input = "b".repeat(DEFAULT_RESULT_CEILING_BYTES + 10_000);
        let declared = FidelityMode::Truncate { max_chars: 100 };
        let out = apply_result_ceiling(&input, Some(&declared));
        assert!(
            out.len() < 200,
            "explicit truncate:100 must win, got {} bytes",
            out.len()
        );
        assert!(out.contains("[truncated]"));
    }

    /// An explicit `Full` annotation is an explicit opt-out — the default
    /// ceiling does NOT clamp it, so the (huge) output passes through whole.
    #[test]
    fn should_let_explicit_full_opt_out_of_default_ceiling() {
        let input = "c".repeat(DEFAULT_RESULT_CEILING_BYTES + 10_000);
        let out = apply_result_ceiling(&input, Some(&FidelityMode::Full));
        assert_eq!(out.len(), input.len(), "explicit Full must not truncate");
        assert!(!out.contains("[truncated"));
    }

    /// A small un-annotated result (under the ceiling) is returned unchanged
    /// — no false truncation.
    #[test]
    fn should_leave_small_unannotated_result_unchanged() {
        let input = "small result";
        let out = apply_result_ceiling(input, None);
        assert_eq!(out, input);
        assert!(!out.contains("[truncated"));
    }

    /// Boundary: exactly-at-ceiling un-annotated output is NOT truncated.
    #[test]
    fn should_not_truncate_unannotated_result_exactly_at_ceiling() {
        let input = "d".repeat(DEFAULT_RESULT_CEILING_BYTES);
        let out = apply_result_ceiling(&input, None);
        assert_eq!(out.len(), DEFAULT_RESULT_CEILING_BYTES);
        assert!(!out.contains("[truncated"));
    }

    /// Truncation must respect UTF-8 boundaries — a multi-byte scalar
    /// straddling the cut point is dropped whole, never split.
    #[test]
    fn should_truncate_unannotated_result_at_utf8_boundary() {
        // Each '€' is 3 bytes; fill past the ceiling with them.
        let count = (DEFAULT_RESULT_CEILING_BYTES / 3) + 1000;
        let input = "€".repeat(count);
        let out = apply_result_ceiling(&input, None);
        // The head (before the marker) must be valid UTF-8 made only of '€'.
        let head = out.split("\n... [truncated").next().unwrap();
        assert!(head.chars().all(|c| c == '€'), "no split scalar in head");
        assert!(out.contains("[truncated:"));
    }
}
