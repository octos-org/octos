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
    /// - Empty content with no tool calls and no reasoning (including output_tokens > 0 bug)
    /// - Content filtered by safety/moderation
    pub(super) fn is_retriable_response(response: &ChatResponse) -> bool {
        let has_reasoning = response
            .reasoning_content
            .as_ref()
            .is_some_and(|r| !r.is_empty());
        let is_empty = response.content.as_ref().is_none_or(|c| c.is_empty())
            && response.tool_calls.is_empty()
            && !has_reasoning;
        let is_abnormal_tool_use =
            response.stop_reason == StopReason::ToolUse && response.tool_calls.is_empty();
        let is_filtered = response.stop_reason == StopReason::ContentFiltered;
        is_empty || is_filtered || is_abnormal_tool_use
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
        return value;
    }
    // Unrepairable → empty object (never a fatal non-object on the wire).
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
    let arguments = parse_invoke_parameter_tags(raw_args)
        .unwrap_or_else(|| repair_tool_arguments_to_object(raw_args));

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

#[cfg(test)]
mod tests {
    use super::*;
    use octos_llm::{ChatResponse, StopReason, TokenUsage as LlmTokenUsage};

    fn make_response(
        content: Option<&str>,
        tool_calls: Vec<ToolCall>,
        output_tokens: u32,
    ) -> ChatResponse {
        make_response_with_stop(content, tool_calls, output_tokens, StopReason::EndTurn)
    }

    fn make_response_with_stop(
        content: Option<&str>,
        tool_calls: Vec<ToolCall>,
        output_tokens: u32,
        stop_reason: StopReason,
    ) -> ChatResponse {
        ChatResponse {
            content: content.map(String::from),
            reasoning_content: None,
            tool_calls,
            stop_reason,
            usage: LlmTokenUsage {
                input_tokens: 0,
                output_tokens,
                ..Default::default()
            },
            provider_index: None,
        }
    }

    // ---------- repair_tool_arguments_to_object (#1711) ----------

    #[test]
    fn repair_passes_through_a_valid_object() {
        let v = repair_tool_arguments_to_object(r#"{"command":"ls -la /tmp"}"#);
        assert_eq!(v["command"], "ls -la /tmp");
        assert!(v.is_object());
    }

    #[test]
    fn repair_empty_or_blank_is_empty_object() {
        assert_eq!(repair_tool_arguments_to_object(""), serde_json::json!({}));
        assert_eq!(
            repair_tool_arguments_to_object("   "),
            serde_json::json!({})
        );
    }

    #[test]
    fn repair_never_returns_a_bare_string() {
        // The old fallback stored `Value::String(raw)`, which serialized back to
        // the provider as an invalid `function.arguments` → fatal HTTP 400.
        let v = repair_tool_arguments_to_object("not json at all");
        assert!(v.is_object(), "must coerce to an object, got {v}");
        assert_eq!(v, serde_json::json!({}));
    }

    #[test]
    fn repair_closes_a_truncated_object() {
        // finish_reason=length cut the JSON mid-value.
        let v = repair_tool_arguments_to_object(r#"{"command":"cat report.md"#);
        assert!(v.is_object());
        assert_eq!(v["command"], "cat report.md");
    }

    #[test]
    fn repair_closes_a_string_and_object_truncated_mid_heredoc() {
        // Truncated in the middle of a heredoc command string.
        let v = repair_tool_arguments_to_object("{\"command\":\"cat > f <<EOF\nline1\nline2");
        assert!(v.is_object());
        assert!(
            v["command"].as_str().unwrap().contains("line1"),
            "salvaged command: {}",
            v["command"]
        );
    }

    #[test]
    fn repair_escapes_literal_control_chars_in_strings() {
        // A heredoc payload with LITERAL newlines/tabs (unescaped) is invalid
        // JSON; the repair escapes them instead of failing.
        let v = repair_tool_arguments_to_object("{\"command\":\"echo a\nb\tc\"}");
        assert!(v.is_object());
        assert_eq!(v["command"], "echo a\nb\tc");
    }

    #[test]
    fn repair_strips_trailing_commas() {
        let v = repair_tool_arguments_to_object(r#"{"a":1,"b":2,}"#);
        assert_eq!(v["a"], 1);
        assert_eq!(v["b"], 2);
    }

    #[test]
    fn repair_of_a_json_non_object_degrades_to_empty_object() {
        // A bare JSON array/string/number is not valid tool arguments.
        assert_eq!(
            repair_tool_arguments_to_object("[1,2,3]"),
            serde_json::json!({})
        );
        assert_eq!(
            repair_tool_arguments_to_object("\"just a string\""),
            serde_json::json!({})
        );
    }

    #[test]
    fn inline_invoke_with_malformed_args_yields_object_not_bare_string() {
        // End-to-end through the inline detector: an `<invoke>` whose body is a
        // truncated/unclosed JSON object (what in-band-reasoning models emit
        // under budget pressure) must still recover to a JSON *object* — never a
        // bare string that would round-trip as a fatal HTTP 400.
        let (_, calls) =
            extract_inline_invokes("<invoke name=\"shell\">{\"command\":\"git clone</invoke>");
        assert_eq!(calls.len(), 1);
        assert!(
            calls[0].arguments.is_object(),
            "arguments must be an object, got {}",
            calls[0].arguments
        );
        assert!(
            calls[0].arguments["command"]
                .as_str()
                .unwrap_or_default()
                .contains("git clone")
        );
    }

    // ---------- parse_invoke_parameter_tags (#1711) ----------

    #[test]
    fn parameter_tags_parse_a_single_string_param() {
        let v = parse_invoke_parameter_tags("<parameter name=\"command\">ls -la /tmp</parameter>")
            .expect("params");
        assert_eq!(v["command"], "ls -la /tmp");
    }

    #[test]
    fn parameter_tags_type_numbers_and_bools_but_keep_commands_as_strings() {
        let v = parse_invoke_parameter_tags(
            "<parameter name=\"command\">grep -r foo .</parameter>\
             <parameter name=\"timeout_seconds\">30</parameter>\
             <parameter name=\"recursive\">true</parameter>",
        )
        .expect("params");
        assert_eq!(v["command"], "grep -r foo .");
        assert_eq!(v["timeout_seconds"], 30);
        assert_eq!(v["recursive"], true);
    }

    #[test]
    fn parameter_tags_preserve_multiline_heredoc_value() {
        let body =
            "<parameter name=\"command\">cat > /tmp/r.md <<'EOF'\n# Review\nline\nEOF</parameter>";
        let v = parse_invoke_parameter_tags(body).expect("params");
        let cmd = v["command"].as_str().unwrap();
        assert!(cmd.contains("<<'EOF'"), "cmd: {cmd}");
        assert!(cmd.contains("# Review"));
    }

    #[test]
    fn parameter_tags_absent_returns_none() {
        assert!(parse_invoke_parameter_tags(r#"{"command":"ls"}"#).is_none());
        assert!(parse_invoke_parameter_tags("plain text").is_none());
    }

    #[test]
    fn inline_invoke_with_parameter_tags_recovers_real_args() {
        // The Kimi-K3 shape: <invoke><parameter name="command">…</parameter></invoke>
        let (_, calls) = extract_inline_invokes(
            "<invoke name=\"shell\"><parameter name=\"command\">cat > f <<'EOF'\nhi\nEOF</parameter></invoke>",
        );
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "shell");
        assert!(calls[0].arguments.is_object());
        assert!(
            calls[0].arguments["command"]
                .as_str()
                .unwrap_or_default()
                .contains("<<'EOF'"),
            "recovered args: {}",
            calls[0].arguments
        );
    }

    // ---------- Agent::is_retriable_response ----------

    #[test]
    fn should_retry_when_all_empty() {
        let r = make_response(None, vec![], 0);
        assert!(Agent::is_retriable_response(&r));

        let r2 = make_response(Some(""), vec![], 0);
        assert!(Agent::is_retriable_response(&r2));
    }

    #[test]
    fn should_not_retry_with_content() {
        let r = make_response(Some("hello"), vec![], 0);
        assert!(!Agent::is_retriable_response(&r));
    }

    #[test]
    fn should_not_retry_with_tool_calls() {
        let tc = ToolCall {
            id: "1".into(),
            name: "test".into(),
            arguments: serde_json::json!({}),
            metadata: None,
        };
        let r = make_response(None, vec![tc], 0);
        assert!(!Agent::is_retriable_response(&r));
    }

    #[test]
    fn should_retry_with_tokens_but_no_content() {
        let r = make_response(None, vec![], 10);
        assert!(Agent::is_retriable_response(&r));
    }

    #[test]
    fn should_retry_when_content_filtered() {
        let r = make_response_with_stop(None, vec![], 0, StopReason::ContentFiltered);
        assert!(Agent::is_retriable_response(&r));

        // Even with partial content, content_filtered should retry
        let r2 = make_response_with_stop(Some("partial"), vec![], 10, StopReason::ContentFiltered);
        assert!(Agent::is_retriable_response(&r2));
    }

    #[test]
    fn should_retry_when_stop_reason_tooluse_but_no_calls() {
        let r = make_response_with_stop(Some("thinking"), vec![], 5, StopReason::ToolUse);
        assert!(Agent::is_retriable_response(&r));
    }

    #[test]
    fn should_normalize_inline_invoke_block_into_tool_call() {
        let mut r = make_response_with_stop(
            Some("<invoke name=\"cron\">{\"action\":\"list\"}</invoke>"),
            vec![],
            10,
            StopReason::EndTurn,
        );
        Agent::normalize_inline_invokes(&mut r);
        assert_eq!(r.stop_reason, StopReason::ToolUse);
        assert_eq!(r.tool_calls.len(), 1);
        assert_eq!(r.tool_calls[0].name, "cron");
        assert_eq!(r.tool_calls[0].arguments["action"], "list");
        assert!(r.content.is_none());
    }

    #[test]
    fn should_normalize_inline_invoke_self_closing_with_args_attr() {
        let mut r = make_response_with_stop(
            Some("before <invoke name=\"cron\" args='{\"action\":\"list\"}' /> after"),
            vec![],
            10,
            StopReason::EndTurn,
        );
        Agent::normalize_inline_invokes(&mut r);
        assert_eq!(r.stop_reason, StopReason::ToolUse);
        assert_eq!(r.tool_calls.len(), 1);
        assert_eq!(r.tool_calls[0].name, "cron");
        assert_eq!(r.tool_calls[0].arguments["action"], "list");
        assert_eq!(r.content.as_deref(), Some("before  after"));
    }

    /// Regression (codex round-4 / fix/orphan-sweep-liveness-gate): inline
    /// `<invoke>` tool-call ids must be PROCESS-UNIQUE, not positional. The id
    /// previously embedded the within-response index, so the FIRST inline call
    /// to a given tool in any response was always `call_inline_0_<tool>`. Two
    /// separate responses each calling `run_pipeline` first thus collided —
    /// breaking the tool_call_id-uniqueness invariant the supervisor's
    /// synth-ack set, the `mark_descendants_failed` pipeline cascade, and the
    /// orphan-sweep tool_call_id-family exemption all rely on.
    #[test]
    fn inline_invoke_ids_are_unique_across_responses() {
        let body = "<invoke name=\"run_pipeline\">{\"k\":\"deep_research\"}</invoke>";
        let (_, calls1) = extract_inline_invokes(body);
        let (_, calls2) = extract_inline_invokes(body);
        assert_eq!(calls1.len(), 1);
        assert_eq!(calls2.len(), 1);
        assert!(
            calls1[0].id.starts_with("call_inline_"),
            "got {}",
            calls1[0].id
        );
        assert!(
            calls1[0].id.ends_with("_run_pipeline"),
            "keeps the readable tool-name suffix: {}",
            calls1[0].id
        );
        assert_ne!(
            calls1[0].id, calls2[0].id,
            "the same tool at position 0 in two responses must NOT collide",
        );
    }

    /// Within a single response, multiple inline calls still get distinct ids
    /// (the monotonic counter increments per call).
    #[test]
    fn inline_invoke_ids_distinct_within_one_response() {
        let body = "<invoke name=\"a\">{}</invoke><invoke name=\"b\">{}</invoke>";
        let (_, calls) = extract_inline_invokes(body);
        assert_eq!(calls.len(), 2);
        assert_ne!(calls[0].id, calls[1].id);
    }

    #[test]
    fn should_downgrade_empty_tooluse_to_endturn_after_normalization() {
        let mut r = make_response_with_stop(Some("plain text"), vec![], 10, StopReason::ToolUse);
        Agent::normalize_inline_invokes(&mut r);
        assert_eq!(r.stop_reason, StopReason::EndTurn);
        assert!(r.tool_calls.is_empty());
        assert_eq!(r.content.as_deref(), Some("plain text"));
    }

    // ---------- Agent::is_repetitive_output ----------

    #[test]
    fn should_detect_repetitive_output() {
        let repeated = "This is a test phrase. ".repeat(30);
        assert!(Agent::is_repetitive_output(&repeated));
    }

    #[test]
    fn should_not_flag_normal_output() {
        let normal = "The quick brown fox jumps over the lazy dog. \
                      Pack my box with five dozen liquor jugs. \
                      How vexingly quick daft zebras jump.";
        assert!(!Agent::is_repetitive_output(normal));
    }

    #[test]
    fn should_not_flag_short_text() {
        assert!(!Agent::is_repetitive_output("hello hello hello"));
    }

    // ---------- Agent::is_retryable_stream_error ----------

    #[test]
    fn is_retryable_stream_error_transient_errors() {
        for keyword in ["overloaded", "429", "503", "rate limit"] {
            let err = eyre::eyre!("Server error: {}", keyword);
            assert!(
                Agent::is_retryable_stream_error(&err),
                "expected retryable for: {keyword}"
            );
        }
    }

    #[test]
    fn is_retryable_stream_error_non_retryable() {
        let err = eyre::eyre!("invalid json");
        assert!(!Agent::is_retryable_stream_error(&err));
    }

    // ──────────────────────────────────────────────────────────────────────
    // Codex round (PR #1355): typed StreamError downcast — these tests
    // pin the boundary contract. Without the downcast path, MalformedArgs
    // would match the string "stream" fallback and get retried forever,
    // hiding the diagnostic from the model.
    // ──────────────────────────────────────────────────────────────────────

    #[test]
    fn is_retryable_stream_error_idle_timeout_is_typed_retryable() {
        let typed = octos_llm::StreamError::IdleTimeout { idle_secs: 180 };
        let err = eyre::Report::new(typed);
        assert!(
            Agent::is_retryable_stream_error(&err),
            "IdleTimeout must be retryable through the typed downcast"
        );
    }

    #[test]
    fn is_retryable_stream_error_malformed_args_is_typed_not_retryable() {
        let typed = octos_llm::StreamError::MalformedArgs {
            tool_id: "call_0".to_string(),
            tool_name: "mofa_slides".to_string(),
            error: "EOF while parsing a string at column 4123".to_string(),
        };
        let err = eyre::Report::new(typed);
        // The rendered string contains "stream" / similar substrings under
        // some formatters; the downcast path must short-circuit BEFORE the
        // string match falls through.
        assert!(
            !Agent::is_retryable_stream_error(&err),
            "MalformedArgs must be NOT retryable so the model sees the diagnostic"
        );
    }

    #[test]
    fn is_retryable_stream_error_incomplete_is_typed_retryable() {
        let typed = octos_llm::StreamError::Incomplete {
            detail: "stream ended without Done".to_string(),
        };
        let err = eyre::Report::new(typed);
        assert!(Agent::is_retryable_stream_error(&err));
    }

    #[test]
    fn is_retryable_stream_error_transport_is_typed_retryable() {
        let typed = octos_llm::StreamError::Transport {
            detail: "broken pipe".to_string(),
        };
        let err = eyre::Report::new(typed);
        assert!(Agent::is_retryable_stream_error(&err));
    }
}
