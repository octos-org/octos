//! Context window limits and token estimation.

use crew_core::Message;

/// Known context window sizes (in tokens) for common models.
///
/// These are best-effort defaults that may become stale as providers update
/// their models. Providers can override `LlmProvider::context_window()` to
/// return accurate values from API metadata or configuration.
pub fn context_window_tokens(model_id: &str) -> u32 {
    let m = model_id.to_lowercase();
    match () {
        // Anthropic Claude
        _ if m.contains("claude-opus-4") || m.contains("claude-sonnet-4") => 200_000,
        _ if m.contains("claude-3") => 200_000,
        // OpenAI
        _ if m.contains("gpt-4o") || m.contains("gpt-4-turbo") => 128_000,
        _ if m.contains("o1") || m.contains("o3") || m.contains("o4") => 200_000,
        _ if m.contains("gpt-4") => 128_000,
        _ if m.contains("gpt-3.5") => 16_385,
        // Google Gemini
        _ if m.contains("gemini-2") || m.contains("gemini-1.5") => 1_000_000,
        _ if m.contains("gemini") => 128_000,
        // DeepSeek
        _ if m.contains("deepseek") => 128_000,
        // Moonshot / Kimi
        _ if m.contains("kimi") || m.contains("moonshot") => 128_000,
        // Qwen / DashScope
        _ if m.contains("qwen") => 128_000,
        // Zhipu / GLM
        _ if m.contains("glm") || m.contains("zhipu") => 128_000,
        // MiniMax
        _ if m.contains("minimax") => 128_000,
        // Local (Llama, etc.)
        _ if m.contains("llama") => 128_000,
        // Conservative default for unknown models
        _ => 128_000,
    }
}

/// Raw JSON content of `model_limits.json`, embedded at compile time.
/// Re-exported so other crates don't need their own `include_str!` with fragile relative paths.
pub const MODEL_LIMITS_JSON: &str = include_str!("model_limits.json");

/// All model limits loaded from `model_limits.json` at compile time.
/// Edit the JSON file to change defaults — no Rust code changes needed.
static MODEL_LIMITS: std::sync::LazyLock<ModelLimitsConfig> = std::sync::LazyLock::new(|| {
    let raw = MODEL_LIMITS_JSON;
    let root: serde_json::Value = serde_json::from_str(raw).unwrap_or_default();

    let default_max_tokens = root
        .get("default_max_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(4096) as u32;

    let default_max_output = root
        .get("default_max_output")
        .and_then(|v| v.as_u64())
        .unwrap_or(8192) as u32;

    let models = root
        .get("models")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|e| {
                    let pattern = e.get("pattern")?.as_str()?.to_string();
                    let max_output = e.get("max_output")?.as_u64()? as u32;
                    let description = e.get("description").and_then(|v| v.as_str()).map(String::from);
                    let tier = e.get("tier").and_then(|v| v.as_str()).map(String::from);
                    Some(ModelLimitEntry { pattern, max_output, description, tier })
                })
                .collect()
        })
        .unwrap_or_default();

    ModelLimitsConfig {
        default_max_tokens,
        default_max_output,
        models,
    }
});

struct ModelLimitEntry {
    pattern: String,
    max_output: u32,
    description: Option<String>,
    #[allow(dead_code)]
    tier: Option<String>,
}

struct ModelLimitsConfig {
    default_max_tokens: u32,
    default_max_output: u32,
    models: Vec<ModelLimitEntry>,
}

/// Default max tokens per LLM call, loaded from `model_limits.json`.
pub fn default_max_tokens() -> u32 {
    MODEL_LIMITS.default_max_tokens
}

/// Look up maximum output token limit for a model.
///
/// Checks patterns from `model_limits.json` in order against the lowercase
/// model ID. Returns `default_max_output` from the JSON if no pattern matches.
pub fn max_output_tokens(model_id: &str) -> u32 {
    let m = model_id.to_lowercase();
    for entry in &MODEL_LIMITS.models {
        if m.contains(entry.pattern.as_str()) {
            return entry.max_output;
        }
    }
    MODEL_LIMITS.default_max_output
}

/// Look up the description for a model from `model_limits.json`.
///
/// Returns a human-readable description of the model's strengths and use cases,
/// or `None` if no matching pattern is found.
pub fn model_description(model_id: &str) -> Option<String> {
    let m = model_id.to_lowercase();
    for entry in &MODEL_LIMITS.models {
        if m.contains(entry.pattern.as_str()) {
            return entry.description.clone();
        }
    }
    None
}

/// Estimate token count from text using character heuristic.
///
/// Uses ~4 chars/token for ASCII (English/code) and ~1.5 chars/token for
/// non-ASCII (CJK, emoji, etc.). This is a rough guard — not a precise
/// tokenizer — so it intentionally overestimates slightly to be safe.
pub fn estimate_tokens(text: &str) -> u32 {
    let ascii_chars = text.bytes().filter(|b| b.is_ascii()).count() as u32;
    let non_ascii_chars = text.chars().count() as u32 - ascii_chars;
    let tokens = ascii_chars / 4 + (non_ascii_chars as f32 / 1.5) as u32;
    tokens.max(1)
}

/// Estimate tokens for a message (content + serialized tool calls + overhead).
pub fn estimate_message_tokens(msg: &Message) -> u32 {
    let mut tokens = estimate_tokens(&msg.content);
    if let Some(ref calls) = msg.tool_calls {
        for call in calls {
            tokens += estimate_tokens(&call.name);
            tokens += estimate_tokens(&call.arguments.to_string());
        }
    }
    // Role/structural overhead (~4 tokens)
    tokens + 4
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_context_window_claude() {
        assert_eq!(context_window_tokens("claude-sonnet-4-20250514"), 200_000);
        assert_eq!(context_window_tokens("claude-opus-4-20250514"), 200_000);
    }

    #[test]
    fn test_context_window_openai() {
        assert_eq!(context_window_tokens("gpt-4o"), 128_000);
        assert_eq!(context_window_tokens("o3-mini"), 200_000);
    }

    #[test]
    fn test_context_window_gemini() {
        assert_eq!(context_window_tokens("gemini-2.0-flash"), 1_000_000);
    }

    #[test]
    fn test_context_window_default() {
        assert_eq!(context_window_tokens("unknown-model"), 128_000);
    }

    #[test]
    fn test_max_output_tokens_known_models() {
        assert_eq!(max_output_tokens("claude-opus-4-20250514"), 128_000);
        assert_eq!(max_output_tokens("kimi-k2.5"), 65_535);
        assert_eq!(max_output_tokens("glm-5"), 131_072);
        assert_eq!(max_output_tokens("deepseek-chat"), 8_192);
        assert_eq!(max_output_tokens("gpt-4o"), 16_384);
        assert_eq!(max_output_tokens("gemini-3-pro"), 65_536);
    }

    #[test]
    fn test_max_output_tokens_default() {
        assert_eq!(max_output_tokens("unknown-model"), 8_192);
    }

    #[test]
    fn test_model_description_known() {
        let desc = model_description("kimi-k2.5");
        assert!(desc.is_some());
        assert!(desc.unwrap().contains("Moonshot"));
    }

    #[test]
    fn test_model_description_unknown() {
        assert!(model_description("unknown-xyz").is_none());
    }

    #[test]
    fn test_estimate_tokens_ascii() {
        // ~4 ASCII chars per token
        assert_eq!(estimate_tokens("hello world"), 2); // 11/4 = 2
        assert_eq!(estimate_tokens("a"), 1); // min 1
    }

    #[test]
    fn test_estimate_tokens_cjk() {
        // CJK: ~1.5 chars per token, should estimate higher than pure ASCII rate
        let cjk = "你好世界测试"; // 6 CJK chars
        let ascii = "abcdef"; // 6 ASCII chars = 1 token
        assert!(estimate_tokens(cjk) > estimate_tokens(ascii));
    }

    #[test]
    fn test_estimate_message_tokens() {
        let msg = Message {
            role: crew_core::MessageRole::User,
            content: "Hello, how are you today?".to_string(),
            media: vec![],
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            timestamp: chrono::Utc::now(),
        };
        let tokens = estimate_message_tokens(&msg);
        // Should be content tokens + 4 overhead
        assert_eq!(tokens, estimate_tokens("Hello, how are you today?") + 4);
    }

    #[test]
    fn test_estimate_message_tokens_with_tool_calls() {
        let msg = Message {
            role: crew_core::MessageRole::Assistant,
            content: String::new(),
            media: vec![],
            tool_calls: Some(vec![crew_core::ToolCall {
                id: "tc1".to_string(),
                name: "read_file".to_string(),
                arguments: serde_json::json!({"path": "src/main.rs"}),
                metadata: None,
            }]),
            tool_call_id: None,
            reasoning_content: None,
            timestamp: chrono::Utc::now(),
        };
        let tokens = estimate_message_tokens(&msg);
        // Should include tool name + arguments + overhead
        assert!(tokens > 4);
        assert!(tokens > estimate_tokens("read_file"));
    }
}
