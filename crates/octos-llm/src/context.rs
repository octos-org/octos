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

/// The provider/host prefix of a catalog key: everything before the first `/`
/// (or the whole key when unqualified). Used to break bare-alias ties on the
/// native HOST rather than the raw key, so a `zai-coding/…` re-host cannot
/// out-sort the native `zai/…` row just because `-` sorts before `/`. Shared
/// with `pricing::seed_pricing_catalog` so the two catalogs agree.
pub(crate) fn provider_prefix(key: &str) -> &str {
    key.split_once('/').map(|(prefix, _)| prefix).unwrap_or(key)
}

/// Build the runtime catalog lookup map from `(provider/model, ctx, max_out)`
/// entries. Pure (touches no global state) so the alias-selection rules below
/// are unit-testable without racing the shared `CATALOG`.
fn build_catalog_map(entries: &[(String, u64, u64)]) -> HashMap<String, CatalogModel> {
    let mut map = HashMap::new();
    // See `pricing::seed_pricing_catalog`: a bare model id can be shared by the
    // native provider and its re-hosts, so award the bare alias to the row with
    // the FEWEST path segments (most canonical/native), deterministically. On an
    // EQUAL segment count the more-native HOST wins: compare the provider prefix
    // (the segment before the first `/`) FIRST, then the full lowercased key. A
    // plain full-key comparison is wrong when one host is a lexical prefix of
    // another — `zai-coding/glm-5.2` (200K) sorts BEFORE `zai/glm-5.2` (1M)
    // because `-` (0x2D) < `/` (0x2F), so the coding-plan re-host would steal the
    // bare `glm-5.2` alias from the native `zai` row. Splitting on the separator
    // makes `zai` < `zai-coding` (the shorter native prefix wins), keeping the
    // 1M window for a bare `glm-5.2` lookup. `minimax/MiniMax-M3` still beats
    // `r9s/minimax-m3` (both one slash), so the bare alias stays order-independent.
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
                        || (segments == *owned_seg
                            && (provider_prefix(&key_lower), key_lower.as_str())
                                < (provider_prefix(owner_key), owner_key.as_str()))
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

/// Whether a lowercased model id is DeepSeek's `deepseek-flash` (V4.1 Flash,
/// the official API's only Flash id), bare or family-qualified. Mirrors the
/// hint detection in `openai.rs`: the id carries no `v4` segment, so the
/// `deepseek-v4` substring branches below would otherwise miss it and fall to
/// the 128K / 8K unknown-model defaults.
fn is_deepseek_flash(lowercased_model_id: &str) -> bool {
    lowercased_model_id == "deepseek-flash" || lowercased_model_id.ends_with("/deepseek-flash")
}

/// Context window size for a model. Checks runtime catalog first.
pub fn context_window_tokens(model_id: &str) -> u32 {
    if let Some((ctx, _)) = catalog_lookup(model_id) {
        if ctx > 0 {
            return ctx as u32;
        }
    }
    // Model-specific defaults for known long-context models when the catalog
    // is unavailable or lacks the exact variant (e.g. deepseek-v4-flash, which
    // has no dedicated catalog lane). DeepSeek V4 / V4.1 Flash, MiniMax M3 and
    // Kimi K3 are 1M-context.
    // The Kimi coding plan (family `moonshot-coding`) exposes K3 under the bare
    // ids `k3` / `kimi-for-coding*`, which don't contain `kimi-k3` — match them
    // too so the `ctx N%` gauge shows K3's real ~1M window, not the 128K default.
    let m = model_id.to_lowercase();
    if m.contains("deepseek-v4")
        || is_deepseek_flash(&m)
        || m.contains("minimax-m3")
        || m.contains("kimi-k3")
        || m == "k3"
        || m.starts_with("kimi-for-coding")
    {
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
    if m.contains("deepseek-v4") || is_deepseek_flash(&m) {
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
    // Inline media is not text: a screenshot is thousands of tokens, not
    // the twenty of its caption. Counted for every row that carries it —
    // an older tool row's media is not re-sent, so this over-counts there,
    // which is the safe side for a budget.
    tokens += crate::tool_media::estimate_media_tokens(&msg.media);
    // Role/structural overhead (~4 tokens)
    tokens + 4
}

/// Rough token cost of ONE tool declaration in the request: providers
/// serialize every name, description, and full input schema (#2135
/// round-8 P1 — a guard that only totals messages passes a tiny prompt
/// whose tool schemas alone overflow a small window).
pub(crate) fn estimate_tool_tokens(tool: &crate::types::ToolSpec) -> u32 {
    estimate_tokens(&tool.name)
        + estimate_tokens(&tool.description)
        + estimate_tokens(&tool.input_schema.to_string())
        + 8 // serialization scaffolding per tool
}

/// Provider-agnostic base request-size estimate (#2143 part 3): messages plus
/// serialized tool declarations. This is what the route-fit guard summed
/// inline before #2143; it is now the DEFAULT body of
/// [`crate::provider::LlmProvider::estimate_request_tokens`], which concrete
/// providers override to add their own request-envelope overhead (a separate
/// system block, per-message content-block framing, cache-control metadata)
/// that this base does not model.
pub fn estimate_request_tokens_base(
    messages: &[octos_core::Message],
    tools: &[crate::types::ToolSpec],
) -> u32 {
    let message_tokens = messages.iter().map(estimate_message_tokens).sum::<u32>();
    let tool_tokens = crate::cache_manifest::cached_tool_spec_artifacts(tools)
        .map(|artifacts| artifacts.estimated_context_tokens)
        .unwrap_or_else(|| tools.iter().map(estimate_tool_tokens).sum());
    message_tokens + tool_tokens
}

/// #2143 part 2: turn-scoped cooperation so the router can RE-COMPACT a
/// conversation for a specific route instead of SKIPPING it.
///
/// A router wrapper cannot re-compact on its own — it has no
/// tool-result-placeholder / summary logic — so the prompt-construction layer
/// can inject a richer resizer for the turn via [`with_route_resizer`]. When
/// none is injected the router falls back to [`mechanical_route_refit`], a
/// safe best-effort trim. An implementation MUST return a message set that
/// preserves the provider-required tool_call/tool_result pairing, or `None` to
/// leave the route skipped (the pre-#2143 behavior).

#[async_trait::async_trait]
pub trait RouteResizer: Send + Sync {
    /// Re-fit `messages` to `window` tokens for one route, or `None` if it
    /// cannot.
    async fn refit(
        &self,
        messages: &[octos_core::Message],
        window: u32,
    ) -> Option<Vec<octos_core::Message>>;
}

tokio::task_local! {
    /// #2143 part 2: the optional turn-scoped route resizer. Absent by default
    /// (the built-in mechanical refit is used); the session layer may inject a
    /// full-compaction resizer for the turn.
    static ROUTE_RESIZER: std::sync::Arc<dyn RouteResizer>;
}

/// Run `fut` with `resizer` available to the router's per-route re-fit
/// (#2143 part 2). Nest inside a turn scope alongside `with_router_context`.
pub async fn with_route_resizer<F, T>(resizer: std::sync::Arc<dyn RouteResizer>, fut: F) -> T
where
    F: std::future::Future<Output = T>,
{
    ROUTE_RESIZER.scope(resizer, fut).await
}

fn current_route_resizer() -> Option<std::sync::Arc<dyn RouteResizer>> {
    ROUTE_RESIZER.try_with(std::sync::Arc::clone).ok()
}

/// Best-effort mechanical re-fit of a conversation to a route's window
/// (#2143 part 2). Drops the OLDEST history — always keeping the leading
/// system message(s), never starting the kept tail on a `Tool` message (which
/// would orphan a tool result from its assistant `tool_call`), and always
/// keeping the final message (the current turn) — until the request fits or
/// nothing more can be dropped. Deliberately simpler than full compaction (no
/// summaries/placeholders); a richer resizer can be injected via
/// [`with_route_resizer`]. Returns `None` when it cannot make the request fit,
/// so the route is skipped exactly as before.
async fn mechanical_route_refit(
    provider: &std::sync::Arc<dyn crate::provider::LlmProvider>,
    messages: &[octos_core::Message],
    tools: &[crate::types::ToolSpec],
) -> Option<Vec<octos_core::Message>> {
    use octos_core::MessageRole;
    let last = messages.len().checked_sub(1)?;
    // Leading system messages are always kept.
    let sys_end = messages
        .iter()
        .position(|m| m.role != MessageRole::System)
        .unwrap_or(0);
    if sys_end >= last {
        return None; // only system + one turn; nothing to drop
    }
    // Orphan-safety depends on octos's invariant that a tool_result sits in the
    // contiguous block immediately after its assistant tool_call (see
    // message_repair::synthesize_missing_tool_results): because the only
    // dropped span is the contiguous middle `[sys_end, split)` and the kept
    // tail never STARTS on a Tool message, a kept tool_result's call is always
    // kept too. If a future change interleaves a non-tool message between a
    // call and its result, this guard would need to widen.
    let mut split = sys_end + 1;
    while split < last {
        // Never begin the kept tail on a tool result — its assistant call may
        // have been in the dropped prefix.
        if messages[split].role == MessageRole::Tool {
            split += 1;
            continue;
        }
        let mut candidate: Vec<octos_core::Message> = messages[..sys_end].to_vec();
        candidate.extend_from_slice(&messages[split..]);
        if route_fits_request(provider, &candidate, tools).await {
            // #2143 review (item 4): re-fitting silently drops history — the
            // model answers from a truncated conversation and the old "route
            // skipped" error is suppressed. Surface it so operators can see the
            // context loss (which route, how much dropped) instead of it being
            // invisible.
            let dropped = messages.len().saturating_sub(candidate.len());
            tracing::debug!(
                provider = provider.provider_name(),
                window = provider.context_window(),
                messages_dropped = dropped,
                kept = candidate.len(),
                "route re-fit: trimmed conversation history to fit a smaller route"
            );
            metrics::counter!(
                "octos_route_refit_total",
                "provider" => provider.provider_name().to_string()
            )
            .increment(1);
            return Some(candidate);
        }
        split += 1;
    }
    None
}

/// #2143 part 2: when the request does not fit `provider`, ask the turn-scoped
/// [`RouteResizer`] (or the built-in mechanical refit) to re-compact for the
/// route's window, returning the refitted messages IFF they now actually fit.
/// `None` means "skip this route" — the pre-#2143 behavior.
pub(crate) async fn refit_for_route(
    provider: &std::sync::Arc<dyn crate::provider::LlmProvider>,
    messages: &[octos_core::Message],
    tools: &[crate::types::ToolSpec],
) -> Option<Vec<octos_core::Message>> {
    if let Some(resizer) = current_route_resizer() {
        let window = provider.context_window();
        if let Some(refit) = resizer.refit(messages, window).await
            && route_fits_request(provider, &refit, tools).await
        {
            return Some(refit);
        }
    }
    mechanical_route_refit(provider, messages, tools).await
}

/// The dispatch decision for one route (#2143 part 2).
pub(crate) enum RouteDecision {
    /// The request fits as-is; dispatch the original messages.
    Fits,
    /// The request was re-fitted for this route; dispatch these instead.
    Refit(Vec<octos_core::Message>),
    /// The route cannot serve this request even after a re-fit; skip it.
    Skip,
}

/// Decide how to dispatch `messages` to `provider` (#2143 part 2): send as-is
/// if they fit, re-fit for this route if a resizer (or the mechanical refit)
/// can make them fit, else skip. This replaces the pre-#2143 fit-or-skip guard
/// at the dispatch funnel.
pub(crate) async fn decide_route(
    provider: &std::sync::Arc<dyn crate::provider::LlmProvider>,
    messages: &[octos_core::Message],
    tools: &[crate::types::ToolSpec],
) -> RouteDecision {
    if route_fits_request(provider, messages, tools).await {
        return RouteDecision::Fits;
    }
    match refit_for_route(provider, messages, tools).await {
        Some(refit) => RouteDecision::Refit(refit),
        None => RouteDecision::Skip,
    }
}

/// Route-fit guard for failover/fallback dispatch (#2135 rounds 7-8, P1):
/// before re-sending an UNCHANGED request to an alternate route, resolve
/// that route's readiness (a lazily-probed local provider may still be
/// reporting its catalog guess) and check the ACTUAL request — messages
/// plus serialized tool declarations — plausibly fits its window. The
/// min-across-routes sizing accessors bound the envelope at PROMPT-BUILD
/// time with whatever was resolved then; this guard is the dispatch-time
/// recheck for routes that resolved smaller afterwards. A route that
/// cannot fit is SKIPPED like a failed route — an oversized request would
/// be truncated or rejected server-side anyway, with worse failure modes
/// than trying the next lane.
pub(crate) async fn route_fits_request(
    provider: &std::sync::Arc<dyn crate::provider::LlmProvider>,
    messages: &[octos_core::Message],
    tools: &[crate::types::ToolSpec],
) -> bool {
    provider.ensure_ready().await;
    let window = provider.context_window();
    // #2143 part 3: ask the provider itself, so its request-envelope overhead
    // (system-block framing, per-message metadata) is counted instead of only
    // approximated by the margin below. The default impl is the base estimate,
    // so unspecialized providers behave exactly as before.
    let estimated = provider.estimate_request_tokens(messages, tools);
    let margin = (window / 8).clamp(64, 1024);
    estimated <= window.saturating_sub(margin)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #2135 round-8 P1: the fit guard measures the ACTUAL request — a
    /// tiny message with a large tool schema must fail a small window.
    #[tokio::test]
    async fn should_count_tool_schemas_in_route_fit() {
        use std::sync::Arc;
        struct Tiny;
        #[async_trait::async_trait]
        impl crate::provider::LlmProvider for Tiny {
            async fn chat(
                &self,
                _m: &[octos_core::Message],
                _t: &[crate::types::ToolSpec],
                _c: &crate::config::ChatConfig,
            ) -> eyre::Result<crate::types::ChatResponse> {
                unreachable!()
            }
            fn model_id(&self) -> &str {
                "tiny"
            }
            fn provider_name(&self) -> &str {
                "local"
            }
        }
        let provider: Arc<dyn crate::provider::LlmProvider> =
            Arc::new(crate::ContextWindowOverride::new(Arc::new(Tiny), 2_000));
        let msg = [octos_core::Message::user("hi")];
        let fat_tool = crate::types::ToolSpec {
            name: "big".into(),
            description: "d".into(),
            input_schema: serde_json::json!({"properties": {"x": "y".repeat(20_000)}}),
        };
        assert!(
            route_fits_request(&provider, &msg, &[]).await,
            "message alone fits"
        );
        assert!(
            !route_fits_request(&provider, &msg, &[fat_tool]).await,
            "tool schema must count against the window"
        );
    }

    /// #2143 part 3: the route-fit guard asks the PROVIDER for the request size,
    /// so a provider's request-envelope overhead is counted — and the override
    /// must survive the RetryProvider + ContextWindowOverride wrappers the
    /// router dispatches through (delegation).
    #[tokio::test]
    async fn route_fit_uses_provider_request_estimate_through_wrappers() {
        use std::sync::Arc;
        struct Heavy;
        #[async_trait::async_trait]
        impl crate::provider::LlmProvider for Heavy {
            async fn chat(
                &self,
                _m: &[octos_core::Message],
                _t: &[crate::types::ToolSpec],
                _c: &crate::config::ChatConfig,
            ) -> eyre::Result<crate::types::ChatResponse> {
                unreachable!()
            }
            fn model_id(&self) -> &str {
                "heavy"
            }
            fn provider_name(&self) -> &str {
                "local"
            }
            fn estimate_request_tokens(
                &self,
                messages: &[octos_core::Message],
                tools: &[crate::types::ToolSpec],
            ) -> u32 {
                // A big request envelope the flat estimator would miss.
                estimate_request_tokens_base(messages, tools) + 3_000
            }
        }
        struct Light;
        #[async_trait::async_trait]
        impl crate::provider::LlmProvider for Light {
            async fn chat(
                &self,
                _m: &[octos_core::Message],
                _t: &[crate::types::ToolSpec],
                _c: &crate::config::ChatConfig,
            ) -> eyre::Result<crate::types::ChatResponse> {
                unreachable!()
            }
            fn model_id(&self) -> &str {
                "light"
            }
            fn provider_name(&self) -> &str {
                "local"
            }
        }
        let msg = [octos_core::Message::user("hi")];
        // Window 2000: the tiny message alone would fit, but Heavy's +3000
        // envelope pushes the request over — proving the estimate reaches the
        // guard through RetryProvider(ContextWindowOverride(..)).
        let heavy: Arc<dyn crate::provider::LlmProvider> = Arc::new(crate::RetryProvider::new(
            Arc::new(crate::ContextWindowOverride::new(Arc::new(Heavy), 2_000)),
        ));
        assert!(
            !route_fits_request(&heavy, &msg, &[]).await,
            "the provider's request-envelope estimate must count through the wrappers"
        );
        // Control: the default (base-only) provider fits the same window.
        let light: Arc<dyn crate::provider::LlmProvider> = Arc::new(crate::RetryProvider::new(
            Arc::new(crate::ContextWindowOverride::new(Arc::new(Light), 2_000)),
        ));
        assert!(
            route_fits_request(&light, &msg, &[]).await,
            "a base-estimate provider still fits"
        );
    }
}
