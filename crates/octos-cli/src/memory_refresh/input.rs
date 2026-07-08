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
const MEMORY_TOOL_NAMES: &[&str] = &["memory_note", "save_memory", "recall_memory"];
/// Per-tool-result truncation (chars) before the global budget applies.
const TOOL_RESULT_MAX_CHARS: usize = 1_000;

/// One renderable transcript line: original index + role + sanitized text.
pub(crate) struct InputLine {
    pub idx: usize,
    pub role: MessageRole,
    pub text: String,
}

/// Build sanitized, indexed input lines from an exported transcript.
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
                    text: redact_secrets(&text),
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
                lines.push(InputLine {
                    idx: *idx,
                    role: msg.role,
                    text: redact_secrets(&text),
                });
            }
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(role: MessageRole, content: &str) -> Message {
        Message {
            role,
            content: content.to_string(),
            media: vec![],
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            client_message_id: None,
            thread_id: None,
            timestamp: chrono::Utc::now(),
        }
    }

    #[test]
    fn should_drop_system_messages_when_building_input() {
        let transcript = vec![
            (
                0,
                msg(MessageRole::System, "## Long-term Memory\nsecret block"),
            ),
            (1, msg(MessageRole::User, "hello")),
        ];
        let lines = build_input_lines(&transcript);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].idx, 1);
    }

    #[test]
    fn should_drop_memory_tool_results_when_building_input() {
        let mut call = msg(MessageRole::Assistant, "noting that");
        call.tool_calls = Some(vec![octos_core::ToolCall {
            id: "tc1".to_string(),
            name: "memory_note".to_string(),
            arguments: serde_json::json!({"kind":"fact","content":"x"}),
            metadata: None,
        }]);
        let mut result = msg(MessageRole::Tool, "Noted for consolidation.");
        result.tool_call_id = Some("tc1".to_string());
        let transcript = vec![
            (0, msg(MessageRole::User, "remember x")),
            (1, call),
            (2, result),
        ];
        let lines = build_input_lines(&transcript);
        // user + assistant text survive; the memory_note call marker and
        // its result are gone.
        assert_eq!(lines.len(), 2);
        assert!(lines.iter().all(|l| !l.text.contains("memory_note")));
        assert!(
            lines
                .iter()
                .all(|l| !l.text.contains("Noted for consolidation"))
        );
    }

    #[test]
    fn should_truncate_tool_results_when_oversized() {
        let mut call = msg(MessageRole::Assistant, "checking");
        call.tool_calls = Some(vec![octos_core::ToolCall {
            id: "tc9".to_string(),
            name: "shell".to_string(),
            arguments: serde_json::json!({}),
            metadata: None,
        }]);
        let mut result = msg(MessageRole::Tool, &"y".repeat(5000));
        result.tool_call_id = Some("tc9".to_string());
        let transcript = vec![(0, call), (1, result)];
        let lines = build_input_lines(&transcript);
        let tool_line = lines.iter().find(|l| l.role == MessageRole::Tool).unwrap();
        assert!(tool_line.text.contains("…[truncated]"));
        assert!(tool_line.text.len() < 1_200);
    }

    #[test]
    fn should_keep_original_indices_in_render() {
        let transcript = vec![
            (0, msg(MessageRole::System, "sys")),
            (1, msg(MessageRole::User, "first")),
            (2, msg(MessageRole::Assistant, "reply")),
        ];
        let lines = build_input_lines(&transcript);
        let text = render_transcript(&lines, 10_000, 1_000_000);
        assert!(text.contains("[1:user] first"));
        assert!(text.contains("[2:assistant] reply"));
    }

    #[test]
    fn should_drop_oldest_lines_when_over_budget() {
        let transcript: Vec<(usize, Message)> = (0..20)
            .map(|i| {
                (
                    i,
                    msg(
                        MessageRole::User,
                        &format!("message number {i} {}", "x".repeat(200)),
                    ),
                )
            })
            .collect();
        let lines = build_input_lines(&transcript);
        let text = render_transcript(&lines, 300, 1_000_000);
        assert!(text.contains("omitted by input budget"));
        assert!(text.contains("message number 19"));
        assert!(!text.contains("message number 0 "));
    }

    #[test]
    fn should_hard_cap_when_single_message_exceeds_budget() {
        let huge = "z".repeat(400_000);
        let transcript = vec![(0, msg(MessageRole::User, &huge))];
        let lines = build_input_lines(&transcript);
        let text = render_transcript(&lines, 1_000, 8_000);
        assert!(text.len() <= 8_000, "byte cap violated: {}", text.len());
        assert!(octos_memory::estimate_tokens(&text) <= 1_000);
        assert!(text.contains("truncated by input budget"));
        assert!(text.ends_with('z'), "tail must be kept");
        assert!(
            text.starts_with("[0:user] "),
            "the citable [idx:role] label must survive truncation: {}",
            &text[..40.min(text.len())]
        );
    }

    #[test]
    fn should_redact_secrets_when_rendering() {
        let transcript = vec![(0, msg(MessageRole::User, "my key sk-abc123def456ghi789"))];
        let lines = build_input_lines(&transcript);
        assert!(lines[0].text.contains("[redacted]"));
    }
}
