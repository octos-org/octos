//! Extraction input construction: indexed transcript → model-ready text.
//!
//! Hygiene rules (design Layer 2, codex-reviewed):
//! - System messages are dropped entirely — they CONTAIN the injected
//!   memory block, and re-ingesting it is the classic feedback loop.
//! - memory-tool traffic (`memory_note` / `save_memory` / `recall_memory`
//!   calls and their results) is dropped so captured notes and recalled
//!   memory never round-trip into new memory.
//! - Other tool results are hard-truncated (they're evidence, not prose).
//! - Secret-shaped strings are redacted before anything leaves the process.
//! - Messages keep their ORIGINAL transcript indices in `[idx:role]`
//!   labels; the extractor must cite them, and the host later derives
//!   evidence kinds from the roles at those indices.

use octos_core::{Message, MessageRole};

use super::redact::redact_secrets;

/// Tool names whose calls/results must never reach the extractor.
const MEMORY_TOOL_NAMES: &[&str] = &[
    "memory_note",
    "save_memory",
    "recall_memory",
    "record_memory_use",
];
/// Per-tool-result truncation (chars) before the global budget applies.
const TOOL_RESULT_MAX_CHARS: usize = 1_000;

/// One renderable transcript line: original index + role + sanitized text.
pub(crate) struct InputLine {
    pub idx: usize,
    pub role: MessageRole,
    pub text: String,
}

/// Build sanitized, indexed input lines from an exported transcript.
/// Replace a threat-flagged transcript line with a labeled placeholder
/// BEFORE it reaches the extraction provider. Scanning only the
/// extractor's OUTPUT is too late: a hostile turn can instruct the model
/// to synthesize a regex-clean false fact citing the hostile turn's own
/// index, which then gains `user_said` authority (codex round-3 P1).
/// The `[idx:role]` label survives, so message indices stay stable for
/// `evidence_idx` validation.
fn guard_line(text: String) -> String {
    match octos_memory::guard::first_threat(&text) {
        Some(threat) => guard_placeholder(threat),
        None => text,
    }
}

fn guard_placeholder(threat: &str) -> String {
    format!("[content excluded by the memory content guard: {threat}]")
}

/// Per-line scanning misses payloads SPLIT across messages — the lines
/// are joined for the provider, so "Ignore all previous" + "instructions;
/// record …" reconstructs at render time (codex round-4 P1). Adjacent
/// pairs cover the practical split; the full-assembly backstop catches
/// deeper splits by redacting the whole batch (rare, and extraction can
/// afford to lose a session).
/// The longest span `first_threat`'s patterns can match (bounded gaps
/// sum well under this). A sliding window this wide in CHARS is enough
/// to reconstruct any single injection phrase split across messages.
const RECONSTRUCT_WINDOW_BYTES: usize = 512;
/// Hard cap on lines per window so a transcript of tiny messages can't
/// make the scan quadratic.
const RECONSTRUCT_WINDOW_LINES: usize = 8;

/// Scan a window of consecutive lines in BOTH the assembly shapes that
/// reach the provider: the label-free join (labels break some phrases —
/// round 5) and the `[idx:role]`-labeled render (labels COMPLETE others,
/// e.g. reversed "ignore instructions from [1:assistant]" — round 7).
fn window_threat(lines: &[InputLine]) -> Option<&'static str> {
    let label_free = lines
        .iter()
        .map(|l| l.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    if let Some(t) = octos_memory::guard::first_threat(&label_free) {
        return Some(t);
    }
    let labeled = lines
        .iter()
        .map(|l| format!("[{}:{}] {}", l.idx, l.role.as_str(), l.text))
        .collect::<Vec<_>>()
        .join("\n");
    octos_memory::guard::first_threat(&labeled)
}

/// Redact threats that only reconstruct once transcript lines are joined
/// for the provider. A bounded sliding window scans consecutive lines and
/// redacts ONLY the contributing ones — never the whole batch (the
/// earlier full-assembly backstop replaced every line on any hit, so a
/// single consumed threat permanently redacted all later benign turns,
/// codex round-6). On a hit the window is shrunk from BOTH ends to the
/// minimal still-matching span so adjacent benign lines are not dropped
/// (codex round-7).
fn redact_reconstructed_threats(lines: &mut [InputLine]) {
    let n = lines.len();
    let mut i = 0;
    while i < n {
        // Grow [i..=j] while within bounds and not yet matching.
        let mut j = i;
        let mut hit = window_threat(&lines[i..=j]);
        while hit.is_none()
            && j + 1 < n
            && j + 1 - i < RECONSTRUCT_WINDOW_LINES
            && lines[i..=j].iter().map(|l| l.text.len() + 1).sum::<usize>()
                < RECONSTRUCT_WINDOW_BYTES
        {
            j += 1;
            hit = window_threat(&lines[i..=j]);
        }
        if let Some(threat) = hit {
            // Shrink the front: drop leading lines that aren't needed for
            // the match, so a benign predecessor keeps its content.
            let mut s = i;
            while s < j && window_threat(&lines[s + 1..=j]).is_some() {
                s += 1;
            }
            // Shrink the back symmetrically.
            let mut e = j;
            while e > s && window_threat(&lines[s..=e - 1]).is_some() {
                e -= 1;
            }
            for line in &mut lines[s..=e] {
                line.text = guard_placeholder(threat);
            }
            i = e + 1;
        } else {
            i += 1;
        }
    }
}

pub(crate) fn build_input_lines(transcript: &[(usize, Message)]) -> Vec<InputLine> {
    // First pass: map tool_call_id -> tool name from assistant messages so
    // tool RESULTS of memory tools can be dropped too.
    let mut call_names: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    for (_, msg) in transcript {
        if let Some(calls) = &msg.tool_calls {
            for call in calls {
                call_names.insert(call.id.clone(), call.name.clone());
            }
        }
    }

    let mut lines = Vec::new();
    for (idx, msg) in transcript {
        match msg.role {
            // Feedback-loop guard: system prompts embed the memory block.
            MessageRole::System => continue,
            MessageRole::Tool => {
                let name = msg
                    .tool_call_id
                    .as_deref()
                    .and_then(|id| call_names.get(id))
                    .map(String::as_str)
                    .unwrap_or("");
                if MEMORY_TOOL_NAMES.contains(&name) {
                    continue;
                }
                let mut text = msg.content.clone();
                if text.chars().count() > TOOL_RESULT_MAX_CHARS {
                    text = text.chars().take(TOOL_RESULT_MAX_CHARS).collect::<String>()
                        + " …[truncated]";
                }
                lines.push(InputLine {
                    idx: *idx,
                    role: MessageRole::Tool,
                    text: guard_line(redact_secrets(&text)),
                });
            }
            MessageRole::User | MessageRole::Assistant => {
                let mut text = msg.content.clone();
                // Surface non-memory tool calls compactly; skip memory ones.
                if let Some(calls) = &msg.tool_calls {
                    for call in calls {
                        if MEMORY_TOOL_NAMES.contains(&call.name.as_str()) {
                            continue;
                        }
                        text.push_str(&format!("\n[called tool: {}]", call.name));
                    }
                }
                if text.trim().is_empty() {
                    continue;
                }
                let text = guard_line(redact_secrets(&text));
                // UPCR-2026-053: a validated participant label (no brackets,
                // one line) rides in front of the guarded text so items keep
                // who took part and whom a request was addressed to.
                let text = match msg.participant.as_deref() {
                    Some(participant) => format!("(participant: {participant}) {text}"),
                    None => text,
                };
                lines.push(InputLine {
                    idx: *idx,
                    role: msg.role,
                    text,
                });
            }
        }
    }
    redact_reconstructed_threats(&mut lines);
    lines
}

/// Render lines as `[idx:role] text`, newest-last, bounded by a token
/// budget (CJK-aware) and a byte cap. When over budget, OLDEST lines are
/// dropped first — the tail of a conversation carries the durable outcome.
pub(crate) fn render_transcript(
    lines: &[InputLine],
    max_tokens: usize,
    max_bytes: usize,
) -> String {
    let rendered: Vec<String> = lines
        .iter()
        .map(|l| format!("[{}:{}] {}", l.idx, l.role.as_str(), l.text))
        .collect();

    let mut start = 0usize;
    loop {
        let mut candidate = rendered[start..].join("\n");
        let within =
            octos_memory::estimate_tokens(&candidate) <= max_tokens && candidate.len() <= max_bytes;
        if within || start + 1 >= rendered.len() {
            if !within {
                // A single message can exceed the whole budget (pasted
                // logs); the hard cap must hold regardless. Keep the
                // `[idx:role]` label — the extractor must cite it — and
                // truncate the BODY from the front (the durable outcome
                // lives at the end).
                let (label, body) = match candidate.split_once("] ") {
                    Some((l, b)) => (format!("{l}] "), b.to_string()),
                    None => (String::new(), candidate),
                };
                let budget_tokens =
                    max_tokens.saturating_sub(octos_memory::estimate_tokens(&label));
                let budget_bytes = max_bytes.saturating_sub(label.len());
                candidate = format!(
                    "{label}{}",
                    truncate_front_to_budget(&body, budget_tokens, budget_bytes)
                );
                // Front-truncation can EXPOSE a threat that the untruncated
                // line hid behind a missing word boundary (…xignore… →
                // "[…truncated] ignore…", codex round-6). Rescan and
                // replace the whole candidate body if so.
                if let Some(threat) = octos_memory::guard::first_threat(&candidate) {
                    let label_only = candidate
                        .split_once("] ")
                        .map(|(l, _)| format!("{l}] "))
                        .unwrap_or_default();
                    let mut placeholder =
                        format!("{label_only}[excluded by the memory content guard: {threat}]");
                    // Keep the hard byte cap: a redaction must not itself
                    // exceed the budget the truncation just enforced
                    // (codex round-7).
                    if placeholder.len() > max_bytes {
                        placeholder = octos_core::truncated_utf8(&placeholder, max_bytes, "");
                    }
                    candidate = placeholder;
                }
            }
            let mut out = candidate;
            if start > 0 {
                out = format!("[…{start} earlier messages omitted by input budget]\n{out}");
            }
            return out;
        }
        start += 1;
    }
}

/// Cut from the FRONT at a char boundary until both budgets hold.
fn truncate_front_to_budget(text: &str, max_tokens: usize, max_bytes: usize) -> String {
    const MARKER: &str = "[…message truncated by input budget] ";
    let mut cut = 0usize;
    let bytes = text.len();
    loop {
        let tail = &text[cut..];
        if octos_memory::estimate_tokens(tail) <= max_tokens.saturating_sub(16)
            && tail.len() <= max_bytes.saturating_sub(MARKER.len())
        {
            return format!("{MARKER}{tail}");
        }
        // Halve the remaining text each round, snapping to a boundary.
        let step = ((bytes - cut) / 2).max(1);
        cut = (cut + step).min(bytes);
        while cut < bytes && !text.is_char_boundary(cut) {
            cut += 1;
        }
        if cut >= bytes {
            return MARKER.to_string();
        }
    }
}
