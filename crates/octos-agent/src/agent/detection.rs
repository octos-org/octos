//! Detection of repetitive output and retriable responses.

use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering};

use octos_core::ToolCall;
use octos_llm::{ChatResponse, StopReason};
use regex::Regex;

use super::Agent;

/// Process-global monotonic counter for synthesizing inline-`<invoke>`
/// tool-call ids.
///
/// Models that emit tool calls as inline `<invoke name=...>` XML in assistant
/// TEXT (rather than native structured `tool_calls`) carry no call id, so octos
/// synthesizes one. Like the Gemini synthesizer (`call_gemini_`) and the
/// agent's empty-id fallback (`call_synth_`), this id MUST be process-unique:
/// it becomes `BackgroundTask::tool_call_id`, and the supervisor's synth-ack
/// set (commit 9e972d8a), the `mark_descendants_failed` pipeline cascade, and
/// the orphan-sweep liveness gate's tool_call_id-family exemption
/// (fix/orphan-sweep-liveness-gate) all key on it. A per-response positional
/// index reset every turn, so two first-position inline calls to the SAME tool
/// (e.g. `run_pipeline`) in different turns both got
/// `call_inline_0_run_pipeline` — colliding, which could match a stale
/// synth-ack or let a live task's tcid falsely exempt a dead one from orphan
/// reaping. A process-global monotonic counter never repeats within the
/// process; the `call_inline_` prefix keeps it disjoint from the other two
/// synthesized id spaces.
static INLINE_TOOL_CALL_SEQ: AtomicU64 = AtomicU64::new(0);

static INVOKE_TAG_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?is)<invoke\b(?P<attrs>[^>]*)>(?P<body>.*?)</invoke\s*>"#).unwrap()
});
static INVOKE_SELF_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?is)<invoke\b(?P<attrs>[^>]*)/\s*>"#).unwrap());
static ATTR_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?is)\b(?P<key>[A-Za-z_][A-Za-z0-9_-]*)\s*=\s*(?:"(?P<dq>[^"]*)"|'(?P<sq>[^']*)')"#,
    )
    .unwrap()
});
static PARAM_TAG_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?is)<parameter\b[^>]*?\bname\s*=\s*["']?(?P<name>[^"'>\s]+)["']?[^>]*>(?P<value>.*?)</parameter\s*>"#,
    )
    .unwrap()
});

impl Agent {
    /// Check if an LLM response is empty or abnormal and should be retried.
    /// Catches:
    /// - No answer or tool calls, even if reasoning/output tokens are present
    /// - Content filtered by safety/moderation
    /// - Repetitive output suppressed by the stream consumer (#1507): the
    ///   placeholder message stands in for degenerate output, so the retry /
    ///   non-streaming-fallback ladder still gets its chance to recover the
    ///   real content — only when that ladder is disabled or exhausted does
    ///   the placeholder reach the user (instead of the old blank bubble).
    pub(super) fn is_retriable_response(response: &ChatResponse) -> bool {
        // Reasoning is not a deliverable. Treat reasoning-only and whitespace
        // responses like other empty results so the bounded recovery ladder
        // gets a chance to obtain a real answer (or emits an error).
        let is_empty = response
            .content
            .as_ref()
            .is_none_or(|c| c.trim().is_empty())
            && response.tool_calls.is_empty();
        let is_abnormal_tool_use =
            response.stop_reason == StopReason::ToolUse && response.tool_calls.is_empty();
        let is_filtered = response.stop_reason == StopReason::ContentFiltered;
        let is_suppressed_repetition = response
            .content
            .as_deref()
            .is_some_and(|c| c == super::streaming::REPETITIVE_OUTPUT_MESSAGE)
            && response.tool_calls.is_empty();
        is_empty || is_filtered || is_abnormal_tool_use || is_suppressed_repetition
    }

    /// Normalize inline XML-style invocations into structured tool calls.
    ///
    /// Some providers emit text like `<invoke name="cron">{...}</invoke>`
    /// instead of native tool-call payloads. We recover those into `tool_calls`
    /// and strip the raw markup from assistant-visible content.
    pub(super) fn normalize_inline_invokes(response: &mut ChatResponse) {
        let Some(content) = response.content.clone() else {
            if response.stop_reason == StopReason::ToolUse && response.tool_calls.is_empty() {
                response.stop_reason = StopReason::EndTurn;
            }
            return;
        };

        let (cleaned, parsed_calls) = extract_inline_invokes(&content);
        if !parsed_calls.is_empty() && response.tool_calls.is_empty() {
            response.tool_calls = parsed_calls;
            response.stop_reason = StopReason::ToolUse;
        } else if response.stop_reason == StopReason::ToolUse && response.tool_calls.is_empty() {
            // Guard against provider/tool-call parser mismatches causing loops.
            response.stop_reason = StopReason::EndTurn;
        }

        let cleaned = cleaned.trim().to_string();
        response.content = if cleaned.is_empty() {
            None
        } else {
            Some(cleaned)
        };
    }

    /// Detect if text content is stuck in a repetitive loop.
    /// Returns true if the same phrase (>= 20 chars) repeats 5+ times.
    pub(super) fn is_repetitive_output(text: &str) -> bool {
        // Use char count for multi-byte safety (Chinese, emoji, etc.)
        let char_count = text.chars().count();
        if char_count < 200 {
            return false;
        }
        // Check last 500 chars for repeating patterns of 20-100 char lengths
        let check_region: String = if char_count > 500 {
            text.chars().skip(char_count - 500).collect()
        } else {
            text.to_string()
        };
        let region_chars: Vec<char> = check_region.chars().collect();
        let region_len = region_chars.len();
        for pattern_len in [20, 40, 60, 100] {
            if region_len < pattern_len * 3 {
                continue;
            }
            let pattern: String = region_chars[region_len - pattern_len..].iter().collect();
            let count = check_region.matches(&pattern).count();
            if count >= 4 {
                return true;
            }
        }
        false
    }

    /// Check if an error looks like a transient server issue worth retrying.
    ///
    /// Codex round (PR #1355): typed `StreamError`s now flow through this
    /// path. Downcast first so the variant's own retryability policy
    /// drives the decision — string-matching the rendered message would
    /// either over- or under-trigger (e.g. `MalformedArgs` carries a
    /// "stream error" substring under some rendering but is explicitly
    /// non-retryable so the model can self-correct on its next turn).
    pub(super) fn is_retryable_stream_error(err: &eyre::Report) -> bool {
        if let Some(stream_err) = err.downcast_ref::<octos_llm::StreamError>() {
            return stream_err.is_retryable();
        }
        let msg = err.to_string().to_lowercase();
        msg.contains("overloaded")
            || msg.contains("temporarily")
            || msg.contains("429")
            || msg.contains("502")
            || msg.contains("503")
            || msg.contains("1305")
            || msg.contains("rate limit")
            || msg.contains("decoding response")
            || msg.contains("stream error")
            || msg.contains("connection reset")
            || msg.contains("broken pipe")
    }

    /// Whether an error is specifically a truncated tool call (`#1712`): the
    /// turn hit the output cap mid-call. The retry loop uses this to raise the
    /// output budget for the next attempt (a plain retry at the same cap would
    /// re-truncate). Distinct from a genuinely malformed call.
    pub(super) fn is_truncated_tool_call_error(err: &eyre::Report) -> bool {
        matches!(
            err.downcast_ref::<octos_llm::StreamError>(),
            Some(octos_llm::StreamError::TruncatedToolCall { .. })
        )
    }
}

fn extract_inline_invokes(content: &str) -> (String, Vec<ToolCall>) {
    let mut cleaned = content.to_string();
    let mut calls: Vec<ToolCall> = Vec::new();

    for caps in INVOKE_TAG_RE.captures_iter(content) {
        let attrs = caps.name("attrs").map(|m| m.as_str()).unwrap_or("");
        let body = caps.name("body").map(|m| m.as_str()).unwrap_or("");
        if let Some(call) = build_tool_call_from_invoke(attrs, body) {
            calls.push(call);
        }
    }
    for caps in INVOKE_SELF_RE.captures_iter(content) {
        let attrs = caps.name("attrs").map(|m| m.as_str()).unwrap_or("");
        if let Some(call) = build_tool_call_from_invoke(attrs, "") {
            calls.push(call);
        }
    }

    cleaned = INVOKE_TAG_RE.replace_all(&cleaned, "").to_string();
    cleaned = INVOKE_SELF_RE.replace_all(&cleaned, "").to_string();

    (cleaned, calls)
}

/// Parse Anthropic-style `<parameter name="x">value</parameter>` sub-tags into
/// a JSON arguments object. Some models (e.g. Kimi-K3) emit inline tool calls as
/// `<invoke name="shell"><parameter name="command">…</parameter></invoke>`
/// instead of a JSON body; without this the arguments would be lost (repaired to
/// `{}`). Values that parse cleanly as a JSON number or boolean are typed as
/// such; everything else (including multi-line heredoc commands) is kept as a
/// trimmed string. Returns `None` when the text contains no `<parameter>` tags,
/// so the caller falls back to the JSON path. See #1711.
fn parse_invoke_parameter_tags(body: &str) -> Option<serde_json::Value> {
    let mut map = serde_json::Map::new();
    for caps in PARAM_TAG_RE.captures_iter(body) {
        let Some(name) = caps.name("name").map(|m| m.as_str().trim()) else {
            continue;
        };
        if name.is_empty() {
            continue;
        }
        let raw = caps.name("value").map(|m| m.as_str()).unwrap_or("").trim();
        // Only promote unambiguous scalar types; keep everything else (commands,
        // paths, JSON-looking strings) verbatim so shell payloads are untouched.
        let value = match serde_json::from_str::<serde_json::Value>(raw) {
            Ok(v @ serde_json::Value::Number(_)) | Ok(v @ serde_json::Value::Bool(_)) => v,
            _ => serde_json::Value::String(raw.to_string()),
        };
        map.insert(name.to_string(), value);
    }
    if map.is_empty() {
        None
    } else {
        Some(serde_json::Value::Object(map))
    }
}

/// Coerce inline tool-call argument text into a JSON **object**, repairing the
/// malformations that in-band-reasoning models produce under a tight output
/// budget: JSON truncated mid-object or mid-string (`finish_reason=length`),
/// trailing commas, and literal (unescaped) control characters inside string
/// values (common with heredoc `command` payloads).
///
/// The result is ALWAYS a JSON object. A valid object echoed back to a provider
/// can never trigger the "invalid function arguments json string" HTTP 400 that
/// a bare string or truncated fragment would (that 400 is non-retryable and
/// kills the whole task). An unrepairable arg degrades to `{}`, which surfaces
/// as an ordinary tool-input error the model can retry — never a fatal request
/// rejection. See #1711.
fn repair_tool_arguments_to_object(raw: &str) -> serde_json::Value {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return serde_json::json!({});
    }
    // Fast path: already a valid object.
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed)
        && value.is_object()
    {
        return value;
    }
    // Repair pass: escape literal control chars, drop trailing commas, and
    // close any structure/string left open by truncation, then re-parse.
    let repaired = repair_json_fragment(trimmed);
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(&repaired)
        && value.is_object()
    {
        tracing::warn!(
            target: "octos::toolcall_repair",
            original_len = trimmed.len(),
            "repaired malformed/truncated inline tool-call arguments (#1711)"
        );
        return value;
    }
    // Unrepairable → empty object (never a fatal non-object on the wire).
    tracing::warn!(
        target: "octos::toolcall_repair",
        preview = %trimmed.chars().take(80).collect::<String>(),
        "unrepairable inline tool-call arguments; using empty object (#1711)"
    );
    serde_json::json!({})
}

/// String-aware JSON repair for [`repair_tool_arguments_to_object`]. Single
/// left-to-right scan that (1) escapes literal control chars inside string
/// values, (2) removes trailing commas before `}`/`]`, and (3) closes any
/// string/object/array left open by mid-stream truncation. Operates only
/// outside string literals for structural edits so it never corrupts content.
fn repair_json_fragment(input: &str) -> String {
    let chars: Vec<char> = input.chars().collect();
    let mut out = String::with_capacity(input.len() + 8);
    let mut stack: Vec<char> = Vec::new();
    let mut in_string = false;
    let mut escaped = false;
    for i in 0..chars.len() {
        let c = chars[i];
        if in_string {
            if escaped {
                out.push(c);
                escaped = false;
            } else if c == '\\' {
                out.push(c);
                escaped = true;
            } else if c == '"' {
                out.push(c);
                in_string = false;
            } else if (c as u32) < 0x20 {
                // Literal control char inside a string → escape it so the
                // fragment is wire-valid (heredoc newlines, tabs, etc.).
                match c {
                    '\n' => out.push_str("\\n"),
                    '\t' => out.push_str("\\t"),
                    '\r' => out.push_str("\\r"),
                    other => out.push_str(&format!("\\u{:04x}", other as u32)),
                }
            } else {
                out.push(c);
            }
            continue;
        }
        match c {
            '"' => {
                in_string = true;
                out.push(c);
            }
            '{' => {
                stack.push('}');
                out.push(c);
            }
            '[' => {
                stack.push(']');
                out.push(c);
            }
            '}' | ']' => {
                if stack.last() == Some(&c) {
                    stack.pop();
                }
                out.push(c);
            }
            ',' => {
                // Drop a trailing comma: peek the next non-whitespace char.
                let mut j = i + 1;
                while j < chars.len() && chars[j].is_whitespace() {
                    j += 1;
                }
                if !(j < chars.len() && (chars[j] == '}' || chars[j] == ']')) {
                    out.push(c);
                }
            }
            _ => out.push(c),
        }
    }
    // Close a string left open by truncation (drop a dangling escape first).
    if in_string {
        if escaped {
            out.pop();
        }
        out.push('"');
    }
    // Close structures left open by truncation, innermost first.
    while let Some(closer) = stack.pop() {
        out.push(closer);
    }
    out
}

fn build_tool_call_from_invoke(attrs: &str, body: &str) -> Option<ToolCall> {
    let attr_map = parse_attrs(attrs);
    let name = attr_map.get("name")?.trim();
    if name.is_empty() {
        return None;
    }

    let raw_args = attr_map
        .get("arguments")
        .or_else(|| attr_map.get("args"))
        .or_else(|| attr_map.get("json"))
        .map(|s| s.as_str())
        .unwrap_or_else(|| body.trim());

    let raw_args = strip_code_fence(raw_args.trim());
    // Some models emit args as Anthropic-style `<parameter name="x">…</parameter>`
    // sub-tags inside the invoke body rather than a JSON object; parse those
    // first, otherwise fall back to JSON (with repair).
    let arguments = match parse_invoke_parameter_tags(raw_args) {
        Some(params) => {
            tracing::warn!(
                target: "octos::toolcall_repair",
                tool = name,
                "recovered inline tool-call from <parameter> tags (#1711)"
            );
            params
        }
        None => repair_tool_arguments_to_object(raw_args),
    };

    Some(ToolCall {
        id: format!(
            "call_inline_{}_{}",
            INLINE_TOOL_CALL_SEQ.fetch_add(1, Ordering::Relaxed),
            sanitize_tool_name(name)
        ),
        name: name.to_string(),
        arguments,
        metadata: None,
    })
}

fn parse_attrs(attrs: &str) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    for caps in ATTR_RE.captures_iter(attrs) {
        let key = caps
            .name("key")
            .map(|m| m.as_str().to_ascii_lowercase())
            .unwrap_or_default();
        let value = caps
            .name("dq")
            .or_else(|| caps.name("sq"))
            .map(|m| m.as_str().to_string())
            .unwrap_or_default();
        out.insert(key, value);
    }
    out
}

fn sanitize_tool_name(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '_' | '-' => c,
            _ => '_',
        })
        .collect()
}

fn strip_code_fence(input: &str) -> &str {
    let trimmed = input.trim();
    if !trimmed.starts_with("```") {
        return trimmed;
    }
    let without_open = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```JSON"))
        .or_else(|| trimmed.strip_prefix("```"))
        .unwrap_or(trimmed);
    without_open
        .trim()
        .strip_suffix("```")
        .unwrap_or(without_open.trim())
        .trim()
}
