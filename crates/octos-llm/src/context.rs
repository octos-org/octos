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
    *CATALOG.write().unwrap_or_else(|e| e.into_inner()) = Some(build_catalog_map(entries));
}

/// Build the runtime catalog lookup map from `(provider/model, ctx, max_out)`
/// entries. Pure (touches no global state) so the alias-selection rules below
/// are unit-testable without racing the shared `CATALOG`.
fn build_catalog_map(entries: &[(String, u64, u64)]) -> HashMap<String, CatalogModel> {
    let mut map = HashMap::new();
    // See `pricing::seed_pricing_catalog`: a bare model id can be shared by the
    // native provider and its re-hosts, so award the bare alias to the row with
    // the FEWEST path segments (most canonical/native), deterministically. On an
    // EQUAL segment count the lexicographically-smaller lowercased full key wins
    // (e.g. `minimax/MiniMax-M3` beats `r9s/minimax-m3`, both one slash), so the
    // bare alias does not depend on seed/export order.
    let mut bare_owner: HashMap<String, (usize, String)> = HashMap::new();
    for (key, ctx, max_out) in entries {
        // Store by full key ("dashscope/qwen3.5-plus") and by model name alone ("qwen3.5-plus")
        let key_lower = key.to_lowercase();
        map.insert(
            key_lower.clone(),
            CatalogModel {
                context_window: *ctx,
                max_output: *max_out,
            },
        );
        if let Some(model) = key.split('/').next_back() {
            let bare = model.to_lowercase();
            let segments = key.matches('/').count();
            let take = match bare_owner.get(&bare) {
                None => true,
                Some((owned_seg, owner_key)) => {
                    segments < *owned_seg
                        || (segments == *owned_seg && key_lower.as_str() < owner_key.as_str())
                }
            };
            if take {
                map.insert(
                    bare.clone(),
                    CatalogModel {
                        context_window: *ctx,
                        max_output: *max_out,
                    },
                );
                bare_owner.insert(bare, (segments, key_lower));
            }
        }
    }
    map
}

/// Look up a value from the runtime catalog by model ID.
fn catalog_lookup(model_id: &str) -> Option<(u64, u64)> {
    let guard = CATALOG.read().ok()?;
    let map = guard.as_ref()?;
    catalog_lookup_in(map, model_id)
}

/// Pure catalog matcher over a supplied map (no global state), so the matching
/// rules are unit-testable without racing the shared `CATALOG`.
fn catalog_lookup_in(map: &HashMap<String, CatalogModel>, model_id: &str) -> Option<(u64, u64)> {
    let m = model_id.to_lowercase();
    // Try exact match first, then substring match.
    if let Some(entry) = map.get(&m) {
        return Some((entry.context_window, entry.max_output));
    }
    // Deterministic substring match, mirroring pricing.rs so both agree:
    //   1. Among keys the model id CONTAINS (families the id extends), the
    //      LONGEST (most specific) wins.
    //   2. Otherwise, among keys that CONTAIN the model id, the SHORTEST is the
    //      closest match.
    // Ties break lexicographically so equal-length keys stay stable. Returning
    // the first HashMap hit (as before) was nondeterministic across processes.
    if let Some((_, entry)) = map
        .iter()
        .filter(|(key, _)| m.contains(key.as_str()))
        .max_by(|(a, _), (b, _)| a.len().cmp(&b.len()).then_with(|| b.cmp(a)))
    {
        return Some((entry.context_window, entry.max_output));
    }
    map.iter()
        .filter(|(key, _)| key.contains(&m))
        .min_by(|(a, _), (b, _)| a.len().cmp(&b.len()).then_with(|| a.cmp(b)))
        .map(|(_, entry)| (entry.context_window, entry.max_output))
}

// ── Public API ────────────────────────────────────────────────

/// Context window size for a model. Checks runtime catalog first.
pub fn context_window_tokens(model_id: &str) -> u32 {
    if let Some((ctx, _)) = catalog_lookup(model_id) {
        if ctx > 0 {
            return ctx as u32;
        }
    }
    // Model-specific defaults for known long-context models when the catalog
    // is unavailable or lacks the exact variant (e.g. deepseek-v4-flash, which
    // has no dedicated catalog lane). DeepSeek V4, MiniMax M3 and Kimi K3 are
    // 1M-context.
    let m = model_id.to_lowercase();
    if m.contains("deepseek-v4") || m.contains("minimax-m3") || m.contains("kimi-k3") {
        return 1_048_576;
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
    // Check the newest model families first so they win over the broader
    // substring branches below (e.g. minimax-m3 before the generic minimax).
    if m.contains("deepseek-v4") {
        384_000
    } else if m.contains("minimax-m3") || m.contains("kimi-k3") {
        // kimi-k3 default max completion is 131072 (settable up to 1M);
        // must win over the broader "kimi" branch below.
        131_072
    } else if m.contains("kimi") || m.contains("qwen") || m.contains("gemini") {
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
    fn should_use_1m_context_for_deepseek_v4_and_minimax_m3_when_not_in_catalog() {
        // deepseek-v4-* and minimax-m3 are never seeded by the unit-test
        // catalog fixtures, so these exercise the hardcoded long-context
        // fallbacks regardless of CATALOG state.
        assert_eq!(context_window_tokens("deepseek-v4-pro"), 1_048_576);
        assert_eq!(context_window_tokens("deepseek-v4-flash"), 1_048_576);
        assert_eq!(context_window_tokens("MiniMax-M3"), 1_048_576);
        // max output: v4 -> 384k, m3 -> 128k (131072), checked before the
        // broader deepseek/minimax substring branches.
        assert_eq!(max_output_tokens("deepseek-v4-pro"), 384_000);
        assert_eq!(max_output_tokens("deepseek-v4-flash"), 384_000);
        assert_eq!(max_output_tokens("minimax-m3"), 131_072);
    }

    #[test]
    fn should_use_1m_context_for_kimi_k3_when_not_in_catalog() {
        // kimi-k3 is never seeded by the unit-test catalog fixtures, so this
        // exercises the hardcoded fallbacks regardless of CATALOG state:
        // 1M window and 131072 default max completion, checked before the
        // broader "kimi" substring branch (65_535).
        assert_eq!(context_window_tokens("kimi-k3"), 1_048_576);
        assert_eq!(context_window_tokens("moonshot/kimi-k3"), 1_048_576);
        assert_eq!(max_output_tokens("kimi-k3"), 131_072);
        assert_eq!(max_output_tokens("moonshot/kimi-k3"), 131_072);
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
    fn should_deterministically_match_by_substring_mirroring_pricing() {
        // Regression: the substring fallback returned the first HashMap hit, so
        // the winner depended on nondeterministic iteration order. Test the pure
        // matcher over a LOCAL map (no shared CATALOG race), using model ids that
        // are NOT exact keys so lookup goes through the substring path.
        let mut map = HashMap::new();
        for (key, ctx, out) in [
            ("gpt", 8_000u64, 1_000u64),
            ("gpt-4o-mini", 128_000, 16_000),
        ] {
            map.insert(
                key.to_string(),
                CatalogModel {
                    context_window: ctx,
                    max_output: out,
                },
            );
        }

        // Branch 1 (model id EXTENDS a family): "gpt-4o-mini-2024-07-18" is not a
        // key; it contains both "gpt" and "gpt-4o-mini" — the LONGEST wins.
        // Repeated to shake out any iteration-order dependence.
        for _ in 0..20 {
            assert_eq!(
                catalog_lookup_in(&map, "gpt-4o-mini-2024-07-18"),
                Some((128_000, 16_000))
            );
        }
        // Branch 2 (a catalog key EXTENDS the model id): "4o-mini" contains no
        // key, but the key "gpt-4o-mini" contains it, so branch 2 picks that
        // (shortest containing key).
        assert_eq!(catalog_lookup_in(&map, "4o-mini"), Some((128_000, 16_000)));
        // Exact key still short-circuits via map.get.
        assert_eq!(
            catalog_lookup_in(&map, "gpt-4o-mini"),
            Some((128_000, 16_000))
        );
    }

    #[test]
    fn should_break_equal_length_substring_ties_deterministically() {
        // Two equal-length keys both contained in the model id: length can't
        // decide, so the lexical tie-break must pick a stable winner (matching
        // pricing.rs). Repeated to shake out HashMap iteration-order dependence.
        let mut map = HashMap::new();
        map.insert(
            "m-aaa".to_string(),
            CatalogModel {
                context_window: 111,
                max_output: 1,
            },
        );
        map.insert(
            "m-bbb".to_string(),
            CatalogModel {
                context_window: 222,
                max_output: 2,
            },
        );
        // "x-m-aaa-m-bbb-y" contains both equal-length keys; the lex-smaller
        // "m-aaa" wins deterministically.
        for _ in 0..20 {
            assert_eq!(catalog_lookup_in(&map, "x-m-aaa-m-bbb-y"), Some((111, 1)));
        }
    }

    #[test]
    fn build_catalog_map_awards_bare_alias_to_native_then_lexicographically() {
        // A deeper re-host loses the bare alias to the fewest-segments native
        // row regardless of order.
        let native = build_catalog_map(&[
            ("rehost/vendor/mdeep-9".to_string(), 111, 1), // 2 seg, listed first
            ("native/mdeep-9".to_string(), 1_000_000, 8_192), // 1 seg (native) wins
        ]);
        let bare = native.get("mdeep-9").unwrap();
        assert_eq!(bare.context_window, 1_000_000);
        assert_eq!(bare.max_output, 8_192);
        // Deeper re-host still resolves under its own fully-qualified key.
        assert_eq!(
            native.get("rehost/vendor/mdeep-9").unwrap().context_window,
            111
        );

        // EQUAL depth (both one segment): segment count can't break the tie, so
        // the lexicographically-smaller lowercased key wins deterministically —
        // mirrors `minimax/MiniMax-M3` vs `r9s/minimax-m3`. Larger key first to
        // prove order-independence.
        let tie = build_catalog_map(&[
            ("zeta/mtie-9".to_string(), 500_000, 4_096), // larger key, first
            ("alpha/mtie-9".to_string(), 1_000_000, 8_192), // smaller key wins
        ]);
        let bare = tie.get("mtie-9").unwrap();
        assert_eq!(
            bare.context_window, 1_000_000,
            "equal-depth bare alias resolves to the lexicographically-smaller key"
        );
        assert_eq!(bare.max_output, 8_192);
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
            client_message_id: None,
            thread_id: None,
            timestamp: chrono::Utc::now(),
        };
        let tokens = estimate_message_tokens(&msg);
        assert_eq!(tokens, estimate_tokens("Hello, how are you today?") + 4);
    }
}
