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

/// Gap 3.4 — DEFAULT result-size ceiling (in RAW bytes) used as a coarse
/// fast-path gate: a result whose RAW length is already under this never
/// needs the (more expensive) serialized-size check, because no possible
/// JSON escaping of a sub-256-KiB body can exceed the serialized budget by
/// enough to matter — see [`compute_result_ceiling`] for the precise bound.
///
/// "Hard limits are cliffs": a pipeline that emits a huge result can produce
/// an unbounded frame that trips the 1 MiB `MAX_TEXT_FRAME_BYTES` wedge
/// (`frame_too_large`). The real guarantee is the SERIALIZED bound below;
/// this raw constant is kept for the byte-accurate marker on the simple
/// (ascii) over-budget path and for symmetry with the pipeline-server
/// `MAX_INPUT_SIZE` (262_144).
pub const DEFAULT_RESULT_CEILING_BYTES: usize = 262_144;

/// The 1 MiB frame ceiling enforced by `octos_core::ui_protocol`'s
/// `MAX_TEXT_FRAME_BYTES`. Re-declared here (not imported) to keep
/// octos-pipeline free of an octos-core dependency edge for a single const;
/// a unit test would catch drift if the core constant ever moved.
const MAX_TEXT_FRAME_BYTES: usize = 1024 * 1024;

/// Blocker 1 — the serialized-size budget the bounded pipeline result body
/// (its JSON-ESCAPED length) is held under. Chosen at 512 KiB, i.e. HALF the
/// 1 MiB frame cap, so that even after the per-node footer, the RPC envelope
/// (method, id, params keys), and any residual escaping slack, the final
/// single-line JSON frame is provably well under `MAX_TEXT_FRAME_BYTES`.
///
/// Invariant: for ANY content (including an all-`\0` body that JSON-escapes
/// 6x), `json_escaped_len(bounded_body) + json_escaped_len(footer)` is
/// `<= MAX_FRAME_BUDGET_BYTES`, hence `< MAX_TEXT_FRAME_BYTES`.
pub const MAX_FRAME_BUDGET_BYTES: usize = 512 * 1024;

/// Reserved serialized headroom for the per-node execution-summary footer
/// that `tool.rs` appends AFTER the body. The body's serialized length is
/// bounded to `MAX_FRAME_BUDGET_BYTES - FOOTER_BUDGET_BYTES` so the footer
/// can never push the serialized total over the budget. 32 KiB comfortably
/// covers many-node summaries (each node line is ~60 bytes). [`bound_footer`]
/// enforces that the actual footer NEVER exceeds this serialized reservation,
/// truncating its node lines with an `[+N more nodes omitted]` marker when a
/// many-node (or long-id) pipeline would otherwise overflow.
pub const FOOTER_BUDGET_BYTES: usize = 32 * 1024;

/// Reserved serialized headroom for the body-truncation marker that
/// [`ResultCeiling::with_marker`] appends AFTER the capped body. The marker is
/// bounded — `"\n... [truncated: {kept} of {original} bytes — full result in
/// {name}]"` — at worst two 20-digit usize counts, the fixed scaffold, and the
/// synthetic report's `run_pipeline_<ts>_<pid>_<seq>.md` name (none of which
/// JSON-escape past 1:1 except the 3-byte em-dash that escapes 1:1). 256 bytes
/// is comfortably conservative. The body budget reserves this so that
/// `serialized(body + marker) <= MAX_FRAME_BUDGET_BYTES`, not just the bare
/// body.
pub const MARKER_RESERVE_BYTES: usize = 256;

// Compile-time drift guard + headline invariant: the chosen serialized
// budgets MUST stay strictly under the 1 MiB frame cap, and the footer (plus
// the body-marker reserve) must fit inside the body budget. If
// `MAX_TEXT_FRAME_BYTES` (mirrored from octos-core) or any budget is ever
// bumped past these bounds, the build fails loudly rather than letting the
// producer re-open the `frame_too_large` cliff. Also keeps
// `MAX_TEXT_FRAME_BYTES` live.
const _: () = {
    assert!(MAX_FRAME_BUDGET_BYTES < MAX_TEXT_FRAME_BYTES);
    assert!(FOOTER_BUDGET_BYTES + MARKER_RESERVE_BYTES < MAX_FRAME_BUDGET_BYTES);
    // THE end-to-end invariant. Worst case across the whole producer output:
    //   serialized(marked_body) + serialized(bounded_footer)
    // The body's escaped length is bounded to (BODY_BUDGET - MARKER_RESERVE)
    // and the marker's escaped length is bounded to MARKER_RESERVE, so the
    // marked body is <= BODY_BUDGET = MAX_FRAME_BUDGET_BYTES - FOOTER_BUDGET.
    // The footer's escaped length is bounded to FOOTER_BUDGET. Their sum is
    // therefore <= MAX_FRAME_BUDGET_BYTES, hence strictly < the frame cap.
    assert!(MAX_FRAME_BUDGET_BYTES + FOOTER_BUDGET_BYTES < MAX_TEXT_FRAME_BYTES);
};

/// The serialized budget the bounded result BODY (its JSON-escaped length,
/// AFTER its truncation marker is appended) is held under. Reserves room for
/// both the per-node footer and the body-truncation marker so that the final
/// `serialized(marked_body) + serialized(footer) <= MAX_FRAME_BUDGET_BYTES`.
const BODY_BUDGET_BYTES: usize = MAX_FRAME_BUDGET_BYTES - FOOTER_BUDGET_BYTES;

/// Per-byte JSON-escaped length of a UTF-8 string body, EXCLUDING the
/// surrounding quotes. Matches `serde_json`'s default string serialization:
/// * control byte `\0` and other C0 controls without a short escape → ` `
///   (6 bytes);
/// * `"`, `\`, `\n`, `\r`, `\t`, `` (backspace), `` (form feed)
///   → 2 bytes;
/// * everything else (incl. multi-byte UTF-8 lead/continuation bytes) → 1.
///
/// Operating on raw bytes lets the binary search probe arbitrary byte offsets
/// (even mid-scalar) without panicking on a non-boundary `&str` slice; the
/// per-byte cost is identical to the char-wise escape because every UTF-8
/// continuation/lead byte is in the 1:1 arm.
fn json_escaped_len_bytes(bytes: &[u8]) -> usize {
    let mut len = 0usize;
    for &b in bytes {
        len += match b {
            b'"' | b'\\' | b'\n' | b'\r' | b'\t' | 0x08 | 0x0c => 2,
            // Other C0 control bytes have no short escape -> \u00XX (6 bytes).
            0x00..=0x1f => 6,
            // ASCII printable and all UTF-8 continuation/lead bytes pass 1:1.
            _ => 1,
        };
    }
    len
}

/// Convenience over [`json_escaped_len_bytes`] for a whole string body.
///
/// Exposed `pub(crate)` so the harness-event emitter (Gap 4.2 / Blocker 1) can
/// reuse the SAME serialized-length accounting that bounds the pipeline result
/// body, rather than re-deriving the escape rules. Returns the JSON-escaped
/// length of `s` EXCLUDING the surrounding quotes.
pub(crate) fn json_escaped_len(s: &str) -> usize {
    json_escaped_len_bytes(s.as_bytes())
}

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

/// Outcome of bounding a pipeline result body. The `output` is the bounded
/// HEAD only (no marker) so the caller can append a marker that points at the
/// synthetic full-output report file once that filename is known.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResultCeiling {
    /// The bounded body (head). Marker NOT yet appended.
    pub output: String,
    /// Whether the body was truncated (head < original).
    pub truncated: bool,
    /// Bytes kept in `output` (raw, pre-marker).
    pub kept_len: usize,
    /// Bytes in the original (un-bounded) body.
    pub original_len: usize,
}

impl ResultCeiling {
    /// Render the bounded body with a truncation marker appended when the
    /// body was truncated. When `report` is `Some(name)`, the marker points
    /// the LLM/user at the file holding the FULL untruncated output. When the
    /// body was not truncated, the body is returned verbatim (no marker).
    pub fn with_marker(&self, report: Option<&str>) -> String {
        if !self.truncated {
            return self.output.clone();
        }
        let mut out = self.output.clone();
        match report {
            Some(name) => out.push_str(&format!(
                "\n... [truncated: {} of {} bytes — full result in {name}]",
                self.kept_len, self.original_len
            )),
            None => out.push_str(&format!(
                "\n... [truncated: {} of {} bytes]",
                self.kept_len, self.original_len
            )),
        }
        out
    }
}

/// Blocker 1 — bound a pipeline result body so that its JSON-ESCAPED
/// (serialized) length stays under [`MAX_FRAME_BUDGET_BYTES`] minus the
/// reserved [`FOOTER_BUDGET_BYTES`]. This is the real guarantee behind Gap
/// 3.4: regardless of content (incl. an all-control-byte body that escapes
/// up to 6x), the resulting frame is provably `< MAX_TEXT_FRAME_BYTES`.
///
/// Semantics:
/// * `declared = Some(mode)` — the pipeline annotated a fidelity mode; it
///   WINS verbatim (existing [`FidelityMode::apply`] semantics, incl. an
///   explicit `Full` opt-out). No serialized bound is imposed — the operator
///   asked for it, so they own the frame-size consequences.
/// * `declared = None` — bound the SERIALIZED size: if the body's escaped
///   length already fits the body budget, return it unchanged (no false
///   truncation); otherwise binary-search the largest UTF-8-boundary prefix
///   whose escaped length fits, then mark `truncated`.
///
/// Producer-side only. The returned head carries no marker; the caller
/// appends one via [`ResultCeiling::with_marker`] once it knows the report
/// filename.
pub fn compute_result_ceiling(output: &str, declared: Option<&FidelityMode>) -> ResultCeiling {
    let original_len = output.len();
    if let Some(mode) = declared {
        // Explicit annotation wins — including an explicit `Full` opt-out.
        // `FidelityMode::apply` already appends its own `[truncated]` marker
        // when it shortens, so we treat the applied form as the final body
        // and never re-mark it (truncated = false here).
        let applied = mode.apply(output);
        return ResultCeiling {
            kept_len: applied.len(),
            output: applied,
            truncated: false,
            original_len,
        };
    }

    // Reserve room for BOTH the per-node footer AND the body-truncation marker
    // that `with_marker` appends after this head. Bounding the head to
    // (BODY_BUDGET - MARKER_RESERVE) guarantees serialized(head + marker) <=
    // BODY_BUDGET = MAX_FRAME_BUDGET_BYTES - FOOTER_BUDGET, so the marked body
    // alone never eats into the footer's reservation. NOTE: the marker reserve
    // only applies on the TRUNCATED path (an untruncated body carries no
    // marker), so the fast path below still uses the full BODY_BUDGET.
    let body_budget = BODY_BUDGET_BYTES;
    let head_budget = body_budget - MARKER_RESERVE_BYTES;

    // Fast path: a small body (raw under the legacy ceiling) can never escape
    // past the body budget — even at the 6x NUL worst case, 256 KiB * 6 =
    // 1.5 MiB, which CAN exceed the 480 KiB body budget. So we cannot skip on
    // raw length alone for pathological content; only skip when the escaped
    // length is genuinely within budget. An untruncated body carries no
    // marker, so it may use the full body budget (no marker reserve).
    if json_escaped_len(output) <= body_budget {
        return ResultCeiling {
            output: output.to_string(),
            truncated: false,
            kept_len: original_len,
            original_len,
        };
    }

    // Over the serialized budget: find the largest byte offset whose
    // JSON-escaped prefix length fits the body budget. `json_escaped_len` is
    // monotonic non-decreasing in the prefix length, so binary search on the
    // RAW byte offset converges in O(log n); we drive lo/hi by the unsnapped
    // midpoint (guaranteeing halving) and only snap the final winning offset
    // down to a char boundary so a multi-byte scalar is never split.
    let bytes = output.as_bytes();
    let mut lo = 0usize; // largest offset known to FIT (escaped len <= budget)
    let mut hi = original_len; // smallest offset known NOT to fit (the whole body)
    while hi - lo > 1 {
        let mid = lo + (hi - lo) / 2;
        // Probe on the RAW byte prefix (may be mid-scalar); escaped length is
        // per-byte so this is exact, and we snap to a boundary afterwards.
        // Bound against the HEAD budget (body budget minus the marker reserve)
        // so the marker `with_marker` appends keeps serialized(head + marker)
        // within the body budget.
        if json_escaped_len_bytes(&bytes[..mid]) <= head_budget {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    // `lo` is the largest offset whose escaped prefix fits the budget. Snap it
    // DOWN to a UTF-8 char boundary so we never split a scalar (shrinking the
    // prefix only lowers the escaped length, so it still fits).
    let mut best = lo;
    while best > 0 && !output.is_char_boundary(best) {
        best -= 1;
    }
    let head = output[..best].to_string();
    ResultCeiling {
        kept_len: best,
        output: head,
        truncated: true,
        original_len,
    }
}

/// Backwards-compatible convenience wrapper: bound the body and return the
/// marked string (marker WITHOUT a report-file reference). Prefer
/// [`compute_result_ceiling`] + [`ResultCeiling::with_marker`] at call sites
/// that know the synthetic report filename so the marker can point at it.
pub fn apply_result_ceiling(output: &str, declared: Option<&FidelityMode>) -> String {
    compute_result_ceiling(output, declared).with_marker(None)
}

/// Blocker (footer) — assemble the per-node execution-summary footer that
/// `tool.rs` appends AFTER the bounded body, and BOUND its JSON-serialized
/// length to its reserved [`FOOTER_BUDGET_BYTES`].
///
/// The footer is the unbounded tail that closed the `frame_too_large` cliff:
/// it iterated ALL `node_summaries` (each line embeds an arbitrary-length
/// `node_id`/`model`), so a many-node pipeline (or one with very long node
/// IDs/models) could append far more than the reserved 32 KiB and push the
/// serialized total past the 1 MiB frame cap even though the BODY was capped.
///
/// `node_lines` are the already-formatted `- <id> (<model>): <ms>ms, …` lines.
/// This keeps the formatting (model-default substitution etc.) at the call
/// site and lets this helper focus purely on the serialized-size bound. The
/// returned string is the COMPLETE footer (leading scaffold, the kept node
/// lines, an `[+N more nodes omitted]` marker when truncated, and the trailing
/// `Total:` line), whose `json_escaped_len` is guaranteed `<=
/// FOOTER_BUDGET_BYTES`.
///
/// Truncation policy: keep the HEAD node lines (earliest = the entry/most
/// load-bearing nodes) that fit, then append a single
/// `… [+N more nodes omitted]` marker. Nodes are NEVER silently dropped — the
/// marker always names how many were elided. The scaffold + total line + the
/// (bounded) marker are always preserved verbatim; only the variable node
/// lines are elided.
pub fn bound_footer(node_lines: &[String], total_line: &str) -> String {
    const HEADER: &str = "\n\n---\nPipeline execution summary:\n";

    // Build the footer with at most `keep` node lines and an omitted-marker
    // for the rest. Returns the assembled footer string.
    let assemble = |keep: usize| -> String {
        let n = node_lines.len();
        let mut body = String::with_capacity(HEADER.len() + total_line.len() + 64);
        body.push_str(HEADER);
        for (i, line) in node_lines.iter().take(keep).enumerate() {
            if i > 0 {
                body.push('\n');
            }
            body.push_str(line);
        }
        if keep < n {
            // A short, bounded marker naming the elided count. Placed after the
            // kept head lines so the most useful (earliest) nodes survive.
            if keep > 0 {
                body.push('\n');
            }
            body.push_str(&format!("... [+{} more nodes omitted]", n - keep));
        }
        body.push('\n');
        body.push_str(total_line);
        body
    };

    // Fast path: the full footer already fits its serialized reservation.
    let full = assemble(node_lines.len());
    if json_escaped_len(&full) <= FOOTER_BUDGET_BYTES {
        return full;
    }

    // Over budget: binary-search the largest number of HEAD node lines whose
    // assembled footer (incl. the omitted-marker + scaffold + total line) still
    // fits FOOTER_BUDGET_BYTES. `assemble` is monotonic non-decreasing in
    // `keep` (adding a node line never shrinks the footer: the per-line bytes
    // dominate the constant-width count marker), so binary search converges in
    // O(log n) — the suite must stay fast.
    let mut lo = 0usize; // known to FIT
    let mut hi = node_lines.len(); // known NOT to fit (the full set is over)
    while hi - lo > 1 {
        let mid = lo + (hi - lo) / 2;
        if json_escaped_len(&assemble(mid)) <= FOOTER_BUDGET_BYTES {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    // `lo` keeps as many head lines as fit. Even `keep = 0` (scaffold + total
    // line + marker only) is guaranteed under budget because FOOTER_BUDGET is
    // 32 KiB and the scaffold/total are tiny; the const drift-guard keeps that
    // headroom honest.
    assemble(lo)
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

    /// An un-annotated result whose SERIALIZED form exceeds the body budget
    /// is bounded and carries an explicit byte-accurate truncation marker —
    /// never silently dropped, never unbounded. (Blocker 1 changed the cap
    /// from a raw-byte ceiling to a serialized-size budget, so the trigger is
    /// the escaped length, and plain ASCII is kept up to ~480 KiB.)
    #[test]
    fn should_truncate_unannotated_result_over_ceiling_with_marker() {
        let body_budget = MAX_FRAME_BUDGET_BYTES - FOOTER_BUDGET_BYTES;
        let total = body_budget + 50_000;
        let input = "a".repeat(total);
        let out = apply_result_ceiling(&input, None);
        assert!(
            json_escaped_len(out.split("\n... [truncated").next().unwrap()) <= body_budget,
            "head's serialized length must fit the body budget"
        );
        // Marker is byte-accurate: it names the kept and original byte counts.
        // For all-ASCII the escaped length equals the raw length, so the head
        // is bounded right at the body budget.
        assert!(
            out.contains(&format!("of {total} bytes")),
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

    /// Boundary: an ASCII body whose serialized form is exactly at the body
    /// budget is NOT truncated (no false truncation at the edge).
    #[test]
    fn should_not_truncate_unannotated_result_exactly_at_ceiling() {
        let body_budget = MAX_FRAME_BUDGET_BYTES - FOOTER_BUDGET_BYTES;
        let input = "d".repeat(body_budget);
        let out = apply_result_ceiling(&input, None);
        assert_eq!(out.len(), body_budget);
        assert!(!out.contains("[truncated"));
    }

    /// Truncation must respect UTF-8 boundaries — a multi-byte scalar
    /// straddling the cut point is dropped whole, never split. '€' escapes
    /// 1:1 (3 bytes raw, 3 bytes serialized), so fill past the body budget.
    #[test]
    fn should_truncate_unannotated_result_at_utf8_boundary() {
        let body_budget = MAX_FRAME_BUDGET_BYTES - FOOTER_BUDGET_BYTES;
        let count = (body_budget / 3) + 1000;
        let input = "€".repeat(count);
        let out = apply_result_ceiling(&input, None);
        // The head (before the marker) must be valid UTF-8 made only of '€'.
        let head = out.split("\n... [truncated").next().unwrap();
        assert!(head.chars().all(|c| c == '€'), "no split scalar in head");
        assert!(out.contains("[truncated:"));
    }

    // ---- Blocker 1: cap must bound the SERIALIZED frame, not the raw string ----

    /// `json_escaped_len` must match what `serde_json` actually emits for the
    /// string body (minus the two surrounding quotes), including the expensive
    /// ` ` (6-byte) NUL escape and the 2-byte escapes for `"`, `\`, `\n`,
    /// `\r`, `\t`. This is the load-bearing primitive for the serialized bound.
    #[test]
    fn json_escaped_len_matches_serde_json_for_control_bytes() {
        for sample in [
            "plain ascii",
            "quote\" and back\\slash",
            "newlines\n\r\ttabs",
            "\0\0\0 NUL bytes",
            "€ euro and 🎉 emoji",
            &"\0".repeat(1000),
        ] {
            let serde_len = serde_json::to_string(sample).unwrap().len() - 2; // strip quotes
            assert_eq!(
                json_escaped_len(sample),
                serde_len,
                "json_escaped_len disagreed with serde_json for {sample:?}"
            );
        }
    }

    /// THE Blocker-1 invariant. A pathological all-control-byte body sized so
    /// the RAW string is UNDER the 262 KiB raw ceiling but the JSON-escaped
    /// (serialized) form is OVER 1 MiB. After the fix, the SERIALIZED length
    /// of the bounded output PLUS a representative per-node footer must stay
    /// strictly under `MAX_TEXT_FRAME_BYTES` (1 MiB) for ANY content.
    #[test]
    fn should_bound_serialized_size_of_control_byte_result_under_frame_cap() {
        // NUL escapes to 6 bytes each. 200 KiB of NUL → ~1.2 MiB serialized,
        // yet the raw string is < DEFAULT_RESULT_CEILING_BYTES (262_144).
        let raw_len = 200 * 1024;
        assert!(
            raw_len < DEFAULT_RESULT_CEILING_BYTES,
            "precondition: raw must be under the old raw ceiling"
        );
        let input = "\0".repeat(raw_len);
        // Sanity: this is the pathological case — raw under cap, serialized over 1 MiB.
        assert!(
            json_escaped_len(&input) > 1024 * 1024,
            "precondition: serialized form must exceed 1 MiB to be a real test"
        );

        let ceiling = compute_result_ceiling(&input, None);
        let bounded = ceiling.output;
        // The producer appends a per-node summary footer AFTER the body. Model
        // a worst-ish footer to prove the TOTAL serialized output is bounded.
        let footer = "\n\n---\nPipeline execution summary:\n- node (model): 1234ms, 100+200 tokens\nTotal: 100 input + 200 output tokens";
        let serialized_total = json_escaped_len(&bounded) + json_escaped_len(footer);
        assert!(
            serialized_total < MAX_FRAME_BUDGET_BYTES,
            "serialized body+footer must stay under the frame-safe budget, got {serialized_total}"
        );
        assert!(
            serialized_total < 1024 * 1024,
            "serialized body+footer must be provably < 1 MiB (frame cap), got {serialized_total}"
        );
        assert!(
            ceiling.truncated,
            "pathological input must be marked truncated"
        );
    }

    /// The footer budget must be reserved: even when the body is bounded right
    /// at the budget edge, appending the footer can NOT push the serialized
    /// total over `MAX_FRAME_BUDGET_BYTES`.
    #[test]
    fn should_reserve_footer_budget_in_serialized_bound() {
        let input = "\0".repeat(500 * 1024); // huge serialized form
        let ceiling = compute_result_ceiling(&input, None);
        assert!(
            json_escaped_len(&ceiling.output) <= MAX_FRAME_BUDGET_BYTES - FOOTER_BUDGET_BYTES,
            "bounded body's serialized len must leave room for the reserved footer budget"
        );
    }

    /// A body of cheap (1-byte-escaped) ASCII that is just over the serialized
    /// budget is still bounded by the serialized form, keeping the head.
    #[test]
    fn should_bound_serialized_size_of_plain_ascii_over_budget() {
        let input = "a".repeat(MAX_FRAME_BUDGET_BYTES + 100_000);
        let ceiling = compute_result_ceiling(&input, None);
        assert!(
            json_escaped_len(&ceiling.output) <= MAX_FRAME_BUDGET_BYTES - FOOTER_BUDGET_BYTES,
            "plain ascii body must also be bounded by serialized budget"
        );
        assert!(ceiling.output.starts_with("aaaa"));
        assert!(ceiling.truncated);
    }

    /// The truncation marker must be able to name the synthetic report file so
    /// the LLM/user knows where the FULL output landed.
    #[test]
    fn should_format_truncation_marker_pointing_to_report() {
        let ceiling = ResultCeiling {
            output: "head".into(),
            truncated: true,
            kept_len: 4,
            original_len: 9_999,
        };
        let marked = ceiling.with_marker(Some("run_pipeline_123.md"));
        assert!(marked.contains("[truncated:"));
        assert!(marked.contains("4 of 9999 bytes"));
        assert!(
            marked.contains("full result in run_pipeline_123.md"),
            "marker must point at the report file; got {marked:?}"
        );
    }

    /// When no report file is available, the marker still degrades cleanly
    /// (no dangling "full result in" with an empty name).
    #[test]
    fn should_format_truncation_marker_without_report() {
        let ceiling = ResultCeiling {
            output: "head".into(),
            truncated: true,
            kept_len: 4,
            original_len: 9_999,
        };
        let marked = ceiling.with_marker(None);
        assert!(marked.contains("[truncated: 4 of 9999 bytes]"));
        assert!(!marked.contains("full result in"));
    }

    /// A non-truncated ceiling result emits no marker regardless of report arg.
    #[test]
    fn should_not_add_marker_when_not_truncated() {
        let ceiling = ResultCeiling {
            output: "small".into(),
            truncated: false,
            kept_len: 5,
            original_len: 5,
        };
        assert_eq!(ceiling.with_marker(Some("x.md")), "small");
        assert_eq!(ceiling.with_marker(None), "small");
    }

    // The budget-vs-frame-cap drift guard is enforced at COMPILE TIME via the
    // `const _: () = { assert!(...) }` block near the constant definitions, so
    // there is no runtime test for it here.

    /// Explicit fidelity still wins under the new compute path: a small body
    /// is not truncated and a tight explicit truncate clamps below the default.
    #[test]
    fn compute_result_ceiling_respects_explicit_and_does_not_false_truncate() {
        let small = "ok";
        let c = compute_result_ceiling(small, None);
        assert!(!c.truncated);
        assert_eq!(c.with_marker(Some("x.md")), "ok");

        let big = "z".repeat(MAX_FRAME_BUDGET_BYTES + 10_000);
        let c = compute_result_ceiling(&big, Some(&FidelityMode::Truncate { max_chars: 50 }));
        assert!(c.output.len() < 200, "explicit truncate:50 must win");
    }

    // ---- Footer-bound blocker: per-node summary footer must fit its 32 KiB
    //      SERIALIZED reservation, with an omitted-nodes marker ----

    /// Helper mirroring the per-node line formatting tool.rs uses.
    fn node_line(id: &str, model: &str, ms: u64, tin: u64, tout: u64) -> String {
        format!("- {id} ({model}): {ms}ms, {tin}+{tout} tokens")
    }

    /// A small / few-node footer is returned UNCHANGED (no false truncation,
    /// no spurious omitted-marker).
    #[test]
    fn should_leave_small_footer_unchanged() {
        let lines = vec![
            node_line("plan", "gpt-4", 100, 10, 20),
            node_line("write", "claude", 200, 30, 40),
        ];
        let total = "Total: 40 input + 60 output tokens";
        let footer = bound_footer(&lines, total);
        assert!(footer.contains("- plan (gpt-4): 100ms, 10+20 tokens"));
        assert!(footer.contains("- write (claude): 200ms, 30+40 tokens"));
        assert!(footer.contains(total));
        assert!(
            !footer.contains("more nodes omitted"),
            "few-node footer must NOT carry an omitted marker"
        );
        assert!(json_escaped_len(&footer) <= FOOTER_BUDGET_BYTES);
    }

    /// THE footer-bound invariant. A pipeline with THOUSANDS of node summaries
    /// AND very long node-id/model strings builds a footer that — unbounded —
    /// far exceeds its 32 KiB reservation. After the fix the footer's
    /// SERIALIZED length is `<= FOOTER_BUDGET_BYTES`, the HEAD node lines are
    /// kept, and an `[+N more nodes omitted]` marker names the elided count.
    #[test]
    fn should_bound_huge_footer_to_reserved_budget_with_omitted_marker() {
        // Long ids/models so even a handful of lines are kilobytes, plus a huge
        // count of them — the worst case the codex re-review flagged.
        let long_id = "n".repeat(400);
        let long_model = "m".repeat(400);
        let lines: Vec<String> = (0..5_000)
            .map(|i| node_line(&format!("{long_id}{i}"), &long_model, 1234, 100, 200))
            .collect();
        let total = "Total: 500000 input + 1000000 output tokens";

        // Precondition: the UNBOUNDED footer is way over the reservation.
        let unbounded: String = {
            let mut s = String::from("\n\n---\nPipeline execution summary:\n");
            s.push_str(&lines.join("\n"));
            s.push('\n');
            s.push_str(total);
            s
        };
        assert!(
            json_escaped_len(&unbounded) > FOOTER_BUDGET_BYTES * 10,
            "precondition: the unbounded footer must massively overflow"
        );

        let footer = bound_footer(&lines, total);
        assert!(
            json_escaped_len(&footer) <= FOOTER_BUDGET_BYTES,
            "bounded footer serialized len must fit its reservation, got {}",
            json_escaped_len(&footer)
        );
        assert!(
            footer.contains("more nodes omitted"),
            "must carry an omitted-nodes marker; never silently drop"
        );
        // The kept head must be the EARLIEST nodes (node 0 survives).
        assert!(
            footer.contains(&format!("{long_id}0 ")),
            "earliest node must be kept in the head"
        );
        // The scaffold + total line are always preserved.
        assert!(footer.starts_with("\n\n---\nPipeline execution summary:\n"));
        assert!(footer.contains(total));
    }

    /// THE end-to-end frame invariant. Worst case across the WHOLE producer
    /// output: an over-ceiling all-NUL body (escapes 6x) PLUS a huge many-node
    /// footer. The final serialized output — `serialized(marked_body) +
    /// serialized(bounded_footer)` — must stay strictly under the 1 MiB frame
    /// cap for ANY content AND any number/size of node summaries.
    #[test]
    fn should_keep_marked_body_plus_bounded_footer_under_frame_cap() {
        // Body: 500 KiB of NUL -> ~3 MiB serialized; forced over the ceiling.
        let body_input = "\0".repeat(500 * 1024);
        let ceiling = compute_result_ceiling(&body_input, None);
        assert!(ceiling.truncated);
        // Marked body, pointing at a realistic synthetic report name.
        let marked = ceiling.with_marker(Some("run_pipeline_1717400000_12345_0.md"));
        assert!(
            json_escaped_len(&marked) <= MAX_FRAME_BUDGET_BYTES - FOOTER_BUDGET_BYTES,
            "serialized(marked body) must fit the body budget (incl. marker reserve), got {}",
            json_escaped_len(&marked)
        );

        // Footer: thousands of long-id node lines -> unbounded would be MiBs.
        let long_id = "z".repeat(300);
        let lines: Vec<String> = (0..4_000)
            .map(|i| {
                node_line(
                    &format!("{long_id}{i}"),
                    "some-very-long-model-name",
                    9999,
                    1,
                    2,
                )
            })
            .collect();
        let footer = bound_footer(&lines, "Total: 1 input + 2 output tokens");

        let total_serialized = json_escaped_len(&marked) + json_escaped_len(&footer);
        assert!(
            total_serialized <= MAX_FRAME_BUDGET_BYTES,
            "marked body + bounded footer must fit the frame budget, got {total_serialized}"
        );
        assert!(
            total_serialized < 1024 * 1024,
            "marked body + bounded footer must be provably < 1 MiB frame cap, got {total_serialized}"
        );
    }

    /// Body-marker accounting: an over-ceiling body's marked form (body +
    /// truncation marker) must be `<= MAX_FRAME_BUDGET_BYTES - FOOTER_BUDGET`
    /// — i.e. the marker is reserved, not appended OVER the body budget.
    #[test]
    fn should_account_for_body_marker_in_body_budget() {
        let input = "a".repeat(MAX_FRAME_BUDGET_BYTES + 100_000);
        let ceiling = compute_result_ceiling(&input, None);
        assert!(ceiling.truncated);
        // The longest realistic marker form names a report file.
        let marked = ceiling.with_marker(Some(
            "run_pipeline_9999999999_4294967295_18446744073709551615.md",
        ));
        let body_budget = MAX_FRAME_BUDGET_BYTES - FOOTER_BUDGET_BYTES;
        assert!(
            json_escaped_len(&marked) <= body_budget,
            "serialized(body+marker) must stay within the body budget, got {} > {}",
            json_escaped_len(&marked),
            body_budget
        );
    }

    /// `bound_footer` keeps the entire scaffold + total line even when EVERY
    /// node line must be elided (an extreme overflow of one absurdly long
    /// node id), and still fits the reservation.
    #[test]
    fn should_keep_scaffold_and_total_when_all_node_lines_elided() {
        // A single node line larger than the whole footer budget.
        let giant = node_line(&"q".repeat(FOOTER_BUDGET_BYTES * 2), "model", 1, 1, 1);
        let total = "Total: 1 input + 1 output tokens";
        let footer = bound_footer(&[giant], total);
        assert!(json_escaped_len(&footer) <= FOOTER_BUDGET_BYTES);
        assert!(footer.contains("[+1 more nodes omitted]"));
        assert!(footer.contains(total));
        assert!(footer.starts_with("\n\n---\nPipeline execution summary:\n"));
    }
}
