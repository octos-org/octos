//! Context window limits, token estimation, and model metadata.
//!
//! All model-specific data (context window, max output, descriptions) comes from
//! `model_catalog.json` at runtime. Hardcoded defaults are only used as a
//! conservative fallback when the catalog hasn't been loaded or doesn't contain
//! the requested model.

use octos_core::Message;
use std::collections::HashMap;
use std::sync::RwLock;

// ── Runtime catalog (loaded from model_catalog.json) ─────────

/// Cached model info from the runtime catalog.
struct CatalogModel {
    context_window: u64,
    max_output: u64,
}

/// Global runtime catalog, populated by `seed_from_catalog()`.
static CATALOG: RwLock<Option<HashMap<String, CatalogModel>>> = RwLock::new(None);

/// Seed the runtime catalog from model_catalog.json entries.
/// Called once at startup by the gateway after loading the catalog.
/// The `entries` parameter is a list of (provider_slash_model, context_window, max_output).
pub fn seed_from_catalog(entries: &[(String, u64, u64)]) {
    let mut map = HashMap::new();
    for (key, ctx, max_out) in entries {
        // Store by full key ("dashscope/qwen3.5-plus") and by model name alone ("qwen3.5-plus")
        map.insert(
            key.to_lowercase(),
            CatalogModel {
                context_window: *ctx,
                max_output: *max_out,
            },
        );
        if let Some(model) = key.split('/').next_back() {
            map.insert(
                model.to_lowercase(),
                CatalogModel {
                    context_window: *ctx,
                    max_output: *max_out,
                },
            );
        }
    }
    *CATALOG.write().unwrap_or_else(|e| e.into_inner()) = Some(map);
}

/// Look up a value from the runtime catalog by model ID.
fn catalog_lookup(model_id: &str) -> Option<(u64, u64)> {
    let guard = CATALOG.read().ok()?;
    let map = guard.as_ref()?;
    let m = model_id.to_lowercase();
    // Try exact match first, then substring match
    if let Some(entry) = map.get(&m) {
        return Some((entry.context_window, entry.max_output));
    }
    for (key, entry) in map {
        if m.contains(key) || key.contains(&m) {
            return Some((entry.context_window, entry.max_output));
        }
    }
    None
}

// ── Public API ────────────────────────────────────────────────

/// Context window size for a model. Checks runtime catalog first.
pub fn context_window_tokens(model_id: &str) -> u32 {
    if let Some((ctx, _)) = catalog_lookup(model_id) {
        if ctx > 0 {
            return ctx as u32;
        }
    }
    // Conservative default for unknown models
    128_000
}

/// Maximum output tokens for a model. Checks runtime catalog first.
pub fn max_output_tokens(model_id: &str) -> u32 {
    if let Some((_, max_out)) = catalog_lookup(model_id) {
        if max_out > 0 {
            return max_out as u32;
        }
    }
    // Model-specific defaults when catalog is unavailable.
    // Use the model's native max output to avoid truncation.
    let m = model_id.to_lowercase();
    if m.contains("kimi") || m.contains("qwen") || m.contains("gemini") {
        65_535
    } else if m.contains("glm") || m.contains("minimax") {
        128_000
    } else if m.contains("gpt-4") || m.contains("gpt-5") || m.contains("claude") {
        32_768
    } else if m.contains("deepseek") {
        8_000
    } else {
        // Conservative default for unknown models
        16_384
    }
}

/// Default max tokens per LLM call.
pub fn default_max_tokens() -> u32 {
    16_384
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
    fn test_context_window_default() {
        assert_eq!(context_window_tokens("unknown-model"), 128_000);
    }

    #[test]
    fn test_max_output_default() {
        assert_eq!(max_output_tokens("unknown-model"), 16_384);
    }

    #[test]
    fn test_catalog_seed_and_lookup() {
        // Hold the write lock across seed + verify to prevent races with
        // parallel tests that also touch the global CATALOG.
        let mut guard = CATALOG.write().unwrap_or_else(|e| e.into_inner());
        let mut map = HashMap::new();
        for (key, ctx, max_out) in [
            ("minimax/minimax-m2.7", 1_000_000u64, 65_536u64),
            ("deepseek/deepseek-chat", 128_000, 8_192),
        ] {
            let entry = CatalogModel {
                context_window: ctx,
                max_output: max_out,
            };
            map.insert(key.to_lowercase(), entry);
            if let Some(model) = key.split('/').next_back() {
                map.insert(
                    model.to_lowercase(),
                    CatalogModel {
                        context_window: ctx,
                        max_output: max_out,
                    },
                );
            }
        }
        *guard = Some(map);

        // Verify lookups while still holding the lock
        let map_ref = guard.as_ref().unwrap();
        let mm = map_ref.get("minimax-m2.7").unwrap();
        assert_eq!(mm.context_window, 1_000_000);
        assert_eq!(mm.max_output, 65_536);
        let ds = map_ref.get("deepseek-chat").unwrap();
        assert_eq!(ds.context_window, 128_000);
        assert_eq!(ds.max_output, 8_192);

        // Clean up
        *guard = None;
    }

    #[test]
    fn test_estimate_tokens_ascii() {
        assert_eq!(estimate_tokens("hello world"), 2);
        assert_eq!(estimate_tokens("a"), 1);
    }

    #[test]
    fn test_estimate_tokens_cjk() {
        let cjk = "你好世界测试";
        let ascii = "abcdef";
        assert!(estimate_tokens(cjk) > estimate_tokens(ascii));
    }

    #[test]
    fn test_estimate_message_tokens() {
        let msg = Message {
            role: octos_core::MessageRole::User,
            content: "Hello, how are you today?".to_string(),
            media: vec![],
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            timestamp: chrono::Utc::now(),
        };
        let tokens = estimate_message_tokens(&msg);
        assert_eq!(tokens, estimate_tokens("Hello, how are you today?") + 4);
    }
}
