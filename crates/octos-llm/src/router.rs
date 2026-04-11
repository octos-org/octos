//! Provider router for multi-model sub-agent support.
//!
//! Routes LLM calls to different sub-providers based on a prefix scheme.
//! A prefixed model ID like `"anthropic/claude-haiku"` is split on the first `/`:
//! the prefix `"anthropic"` selects the sub-provider, and the remainder
//! `"claude-haiku"` identifies the model within that provider.
//!
//! Inspired by aitk's `RouterClient` pattern, adapted for octos's
//! `Send + Sync` `LlmProvider` trait.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use eyre::Result;
use octos_core::Message;

use crate::config::ChatConfig;
use crate::pricing;
use crate::provider::LlmProvider;
use crate::types::{ChatResponse, ChatStream, ToolSpec};

/// Metadata about a registered sub-provider, exposed to the LLM via tool schemas.
#[derive(Debug, Clone)]
pub struct SubProviderMeta {
    /// The key under which this provider is registered (e.g. "cheap").
    pub key: String,
    /// The model ID (e.g. "gpt-4o-mini").
    pub model_id: String,
    /// Provider name (e.g. "openai").
    pub provider_name: String,
    /// Context window size in tokens (the model's maximum).
    pub context_window: u32,
    /// Maximum output tokens per call for this model.
    pub max_output_tokens: u32,
    /// Cost info auto-derived from pricing.rs (e.g. "$0.15/1M in, $0.60/1M out").
    pub cost_info: Option<String>,
    /// User-provided description of when/why to use this model.
    pub description: Option<String>,
    /// Default context window override applied automatically when this model is selected.
    /// If set, sub-agents get this context budget unless the LLM explicitly overrides.
    pub default_context_window: Option<u32>,
}

/// A composite `LlmProvider` that routes calls to sub-providers by prefix.
///
/// Sub-providers are registered under string keys. When `resolve()` is called
/// with `"key/model_id"`, the router looks up the key and returns the
/// corresponding provider. This is the recommended way to give sub-agents
/// access to different models: the `SpawnTool` calls `resolve()` and passes
/// the concrete provider to the child `Agent`.
///
/// The router also implements `LlmProvider` itself for use as a primary
/// agent provider. In that mode, it delegates to whichever sub-provider
/// is set as "active" via `set_active()`.
pub struct ProviderRouter {
    providers: RwLock<HashMap<String, Arc<dyn LlmProvider>>>,
    /// The key of the currently active sub-provider (for direct LlmProvider use).
    active_key: RwLock<Option<String>>,
    /// Metadata about each registered sub-provider (for LLM-visible tool schemas).
    metadata: RwLock<HashMap<String, SubProviderMeta>>,
    /// Cooldown timestamps: model_key → last failure time.
    /// Models in cooldown are skipped by compatible_fallbacks().
    cooldowns: RwLock<HashMap<String, std::time::Instant>>,
    /// Cooldown duration (default 60s). After this, a failed model is eligible again.
    cooldown_duration: std::time::Duration,
    /// QoS scores from model_catalog.json: model_key → (ds_output * stability).
    /// Used to sort fallbacks by quality instead of just max_output_tokens.
    qos_scores: RwLock<HashMap<String, f64>>,
}

impl ProviderRouter {
    /// Create an empty router.
    pub fn new() -> Self {
        Self {
            providers: RwLock::new(HashMap::new()),
            active_key: RwLock::new(None),
            metadata: RwLock::new(HashMap::new()),
            cooldowns: RwLock::new(HashMap::new()),
            cooldown_duration: std::time::Duration::from_secs(60),
            qos_scores: RwLock::new(HashMap::new()),
        }
    }

    /// Register a sub-provider under the given key.
    ///
    /// If no active key is set, the first registered provider becomes active.
    pub fn register(&self, key: &str, provider: Arc<dyn LlmProvider>) {
        let mut providers = self.providers.write().unwrap_or_else(|e| e.into_inner());
        let is_first = providers.is_empty();
        providers.insert(key.to_string(), provider);
        drop(providers);

        if is_first {
            *self.active_key.write().unwrap_or_else(|e| e.into_inner()) = Some(key.to_string());
        }
    }

    /// Remove a sub-provider by key.
    pub fn remove(&self, key: &str) {
        self.providers
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(key);
    }

    /// Set the active sub-provider key (used when the router is the primary provider).
    pub fn set_active(&self, key: &str) {
        *self.active_key.write().unwrap_or_else(|e| e.into_inner()) = Some(key.to_string());
    }

    /// Resolve a model key into a concrete sub-provider.
    ///
    /// Tries exact key match first (handles keys like `moonshotai/kimi-k2.5`
    /// where the slash is part of the model ID, not a prefix separator).
    /// Falls back to splitting on the first `/` to extract a prefix key.
    /// If there is no `/`, treats the entire string as a key lookup.
    pub fn resolve(&self, prefixed_model: &str) -> Result<Arc<dyn LlmProvider>> {
        let providers = self.providers.read().unwrap_or_else(|e| e.into_inner());

        // Try exact match first (supports compound keys like "minimaxai/minimax-m2.5")
        if let Some(provider) = providers.get(prefixed_model) {
            return Ok(provider.clone());
        }

        // Try case-insensitive exact match
        for (key, provider) in providers.iter() {
            if key.eq_ignore_ascii_case(prefixed_model) {
                return Ok(provider.clone());
            }
        }

        // Fall back to prefix/model split (e.g. "openai/gpt-4o" → lookup "openai")
        let prefix_key = match prefixed_model.split_once('/') {
            Some((k, _model)) => k,
            None => prefixed_model,
        };

        if let Some(provider) = providers.get(prefix_key) {
            return Ok(provider.clone());
        }

        // Last resort: check if any registered key ends with the requested model
        // Handles "minimax-m2.5" matching registered "minimaxai/minimax-m2.5"
        for (key, provider) in providers.iter() {
            if key.ends_with(prefixed_model) || prefixed_model.ends_with(key.as_str()) {
                return Ok(provider.clone());
            }
        }

        providers.get(prefix_key).cloned().ok_or_else(|| {
            let available: Vec<&String> = providers.keys().collect();
            eyre::eyre!(
                "no provider registered for key '{}' (available: {:?})",
                prefixed_model,
                available
            )
        })
    }

    /// List all registered provider keys.
    pub fn keys(&self) -> Vec<String> {
        let providers = self.providers.read().unwrap_or_else(|e| e.into_inner());
        providers.keys().cloned().collect()
    }

    /// List all available models as `"key/model_id"` strings.
    pub fn list_models(&self) -> Vec<String> {
        let providers = self.providers.read().unwrap_or_else(|e| e.into_inner());
        providers
            .iter()
            .map(|(key, provider)| format!("{}/{}", key, provider.model_id()))
            .collect()
    }

    /// Register a sub-provider with metadata for LLM-visible tool schemas.
    ///
    /// Stores the provider and auto-derives cost info from `pricing::model_pricing()`.
    /// If `default_context_window` is set, sub-agents using this provider get that
    /// context budget automatically (unless the LLM explicitly overrides it).
    pub fn register_with_meta(
        &self,
        key: &str,
        provider: Arc<dyn LlmProvider>,
        description: Option<String>,
        default_context_window: Option<u32>,
    ) {
        self.register_with_full_meta(key, provider, description, default_context_window, None);
    }

    /// Register with all metadata fields, including optional max_output_tokens override.
    pub fn register_with_full_meta(
        &self,
        key: &str,
        provider: Arc<dyn LlmProvider>,
        description: Option<String>,
        default_context_window: Option<u32>,
        max_output_tokens_override: Option<u32>,
    ) {
        let model_id = provider.model_id().to_string();
        let provider_name = provider.provider_name().to_string();
        let context_window = provider.context_window();
        let max_output_tokens =
            max_output_tokens_override.unwrap_or_else(|| provider.max_output_tokens());

        let cost_info = pricing::model_pricing(&model_id).map(|p| {
            format!(
                "${:.2}/1M in, ${:.2}/1M out",
                p.input_per_million, p.output_per_million
            )
        });

        let meta = SubProviderMeta {
            key: key.to_string(),
            model_id,
            provider_name,
            context_window,
            max_output_tokens,
            cost_info,
            description,
            default_context_window,
        };

        self.metadata
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key.to_string(), meta);

        self.register(key, provider);
    }

    /// List all registered sub-providers with their metadata.
    pub fn list_models_with_meta(&self) -> Vec<SubProviderMeta> {
        self.metadata
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned()
            .collect()
    }

    /// Record a provider failure — puts it in cooldown for `cooldown_duration`.
    /// Called by FallbackProvider when a provider errors.
    pub fn record_failure(&self, key: &str) {
        let mut cooldowns = self.cooldowns.write().unwrap_or_else(|e| e.into_inner());
        cooldowns.insert(key.to_string(), std::time::Instant::now());
        tracing::info!(
            model = key,
            cooldown_secs = self.cooldown_duration.as_secs(),
            "model entered cooldown"
        );
    }

    /// Check if a model is currently in cooldown.
    pub fn is_cooled_down(&self, key: &str) -> bool {
        let cooldowns = self.cooldowns.read().unwrap_or_else(|e| e.into_inner());
        if let Some(failed_at) = cooldowns.get(key) {
            failed_at.elapsed() < self.cooldown_duration
        } else {
            false
        }
    }

    /// Seed scores from model_catalog.json's `score` field for fallback ranking.
    /// Lower score = better (same as AdaptiveRouter).
    pub fn seed_qos_scores(&self, entries: &[(String, f64)]) {
        let mut scores = self.qos_scores.write().unwrap_or_else(|e| e.into_inner());
        for (key, score) in entries {
            scores.insert(key.clone(), *score);
            if let Some((_, model)) = key.split_once('/') {
                scores.insert(model.to_string(), *score);
            }
        }
    }

    /// Find fallback providers compatible with the given key's output capacity.
    /// Sorted by QoS score (best first), excludes cooled-down models and self.
    pub fn compatible_fallbacks(&self, key: &str) -> Vec<Arc<dyn LlmProvider>> {
        let metadata = self.metadata.read().unwrap_or_else(|e| e.into_inner());
        let providers = self.providers.read().unwrap_or_else(|e| e.into_inner());
        let qos = self.qos_scores.read().unwrap_or_else(|e| e.into_inner());

        // Resolve the actual metadata key
        let resolved_key = if metadata.contains_key(key) {
            key.to_string()
        } else {
            key.split_once('/')
                .map(|(k, _)| k.to_string())
                .unwrap_or_else(|| key.to_string())
        };

        let min_output = metadata
            .get(&resolved_key)
            .map(|m| m.max_output_tokens)
            .unwrap_or(0);

        // Exclude self and cooled-down models
        let mut candidates: Vec<(&str, f64)> = metadata
            .iter()
            .filter(|(k, m)| {
                k.as_str() != resolved_key
                    && m.max_output_tokens >= min_output
                    && !self.is_cooled_down(k)
            })
            .map(|(k, _)| {
                let score = qos.get(k.as_str()).copied().unwrap_or(0.0);
                (k.as_str(), score)
            })
            .collect();

        // Sort by score ascending (lower = better, best fallback first)
        candidates.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));

        candidates
            .into_iter()
            .filter_map(|(k, _)| providers.get(k).cloned())
            .collect()
    }

    /// Get the active sub-provider, if any.
    fn active_provider(&self) -> Result<Arc<dyn LlmProvider>> {
        let active_key = self
            .active_key
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let key = active_key.ok_or_else(|| eyre::eyre!("no active provider set in router"))?;
        let providers = self.providers.read().unwrap_or_else(|e| e.into_inner());
        providers
            .get(&key)
            .cloned()
            .ok_or_else(|| eyre::eyre!("active provider key '{key}' not found in router"))
    }
}

impl Default for ProviderRouter {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl LlmProvider for ProviderRouter {
    async fn chat(
        &self,
        messages: &[Message],
        tools: &[ToolSpec],
        config: &ChatConfig,
    ) -> Result<ChatResponse> {
        self.active_provider()?.chat(messages, tools, config).await
    }

    async fn chat_stream(
        &self,
        messages: &[Message],
        tools: &[ToolSpec],
        config: &ChatConfig,
    ) -> Result<ChatStream> {
        self.active_provider()?
            .chat_stream(messages, tools, config)
            .await
    }

    fn context_window(&self) -> u32 {
        self.active_provider()
            .map(|p| p.context_window())
            .unwrap_or(128_000)
    }

    fn model_id(&self) -> &str {
        // Cannot return dynamic &str from RwLock; return static identifier.
        // Callers that need the actual model should use resolve() to get
        // the concrete provider.
        "router"
    }

    fn provider_name(&self) -> &str {
        "router"
    }

    fn report_late_failure(&self) {
        if let Ok(p) = self.active_provider() {
            p.report_late_failure();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::TokenUsage;

    struct MockProvider {
        model: String,
        ctx_window: u32,
    }

    impl MockProvider {
        fn new(model: &str, ctx_window: u32) -> Self {
            Self {
                model: model.to_string(),
                ctx_window,
            }
        }
    }

    #[async_trait]
    impl LlmProvider for MockProvider {
        async fn chat(
            &self,
            _messages: &[Message],
            _tools: &[ToolSpec],
            _config: &ChatConfig,
        ) -> Result<ChatResponse> {
            Ok(ChatResponse {
                content: Some(format!("response from {}", self.model)),
                reasoning_content: None,
                tool_calls: vec![],
                stop_reason: crate::StopReason::EndTurn,
                usage: TokenUsage::default(),
                provider_index: None,
            })
        }

        fn context_window(&self) -> u32 {
            self.ctx_window
        }

        fn model_id(&self) -> &str {
            &self.model
        }

        fn provider_name(&self) -> &str {
            "mock"
        }
    }

    #[test]
    fn test_register_and_resolve() {
        let router = ProviderRouter::new();
        let provider: Arc<dyn LlmProvider> = Arc::new(MockProvider::new("gpt-4o", 128_000));
        router.register("openai", provider);

        let resolved = router.resolve("openai/gpt-4o").unwrap();
        assert_eq!(resolved.model_id(), "gpt-4o");
        assert_eq!(resolved.context_window(), 128_000);
    }

    #[test]
    fn test_resolve_without_slash() {
        let router = ProviderRouter::new();
        router.register("openai", Arc::new(MockProvider::new("gpt-4o", 128_000)));

        let resolved = router.resolve("openai").unwrap();
        assert_eq!(resolved.model_id(), "gpt-4o");
    }

    #[test]
    fn test_resolve_unknown_key() {
        let router = ProviderRouter::new();
        router.register("openai", Arc::new(MockProvider::new("gpt-4o", 128_000)));

        let result = router.resolve("anthropic/claude");
        let err = result.err().expect("should fail for unknown key");
        assert!(err.to_string().contains("anthropic"));
    }

    #[test]
    fn test_multiple_providers() {
        let router = ProviderRouter::new();
        router.register("openai", Arc::new(MockProvider::new("gpt-4o", 128_000)));
        router.register(
            "anthropic",
            Arc::new(MockProvider::new("claude-haiku", 200_000)),
        );

        let p1 = router.resolve("openai/gpt-4o").unwrap();
        assert_eq!(p1.model_id(), "gpt-4o");
        assert_eq!(p1.context_window(), 128_000);

        let p2 = router.resolve("anthropic/claude-haiku").unwrap();
        assert_eq!(p2.model_id(), "claude-haiku");
        assert_eq!(p2.context_window(), 200_000);
    }

    #[test]
    fn test_list_models() {
        let router = ProviderRouter::new();
        router.register("openai", Arc::new(MockProvider::new("gpt-4o", 128_000)));
        router.register(
            "anthropic",
            Arc::new(MockProvider::new("claude-haiku", 200_000)),
        );

        let mut models = router.list_models();
        models.sort();
        assert_eq!(models, vec!["anthropic/claude-haiku", "openai/gpt-4o"]);
    }

    #[test]
    fn test_first_registered_becomes_active() {
        let router = ProviderRouter::new();
        router.register("openai", Arc::new(MockProvider::new("gpt-4o", 128_000)));
        router.register(
            "anthropic",
            Arc::new(MockProvider::new("claude-haiku", 200_000)),
        );

        // First registered becomes active
        assert_eq!(router.context_window(), 128_000);
    }

    #[test]
    fn test_set_active() {
        let router = ProviderRouter::new();
        router.register("openai", Arc::new(MockProvider::new("gpt-4o", 128_000)));
        router.register(
            "anthropic",
            Arc::new(MockProvider::new("claude-haiku", 200_000)),
        );

        router.set_active("anthropic");
        assert_eq!(router.context_window(), 200_000);
    }

    #[tokio::test]
    async fn test_chat_delegates_to_active() {
        let router = ProviderRouter::new();
        router.register("openai", Arc::new(MockProvider::new("gpt-4o", 128_000)));

        let resp = router.chat(&[], &[], &ChatConfig::default()).await.unwrap();
        assert_eq!(resp.content.unwrap(), "response from gpt-4o");
    }

    #[test]
    fn test_remove_provider() {
        let router = ProviderRouter::new();
        router.register("openai", Arc::new(MockProvider::new("gpt-4o", 128_000)));
        assert!(router.resolve("openai").is_ok());

        router.remove("openai");
        assert!(router.resolve("openai").is_err());
    }

    #[test]
    fn test_keys() {
        let router = ProviderRouter::new();
        router.register("openai", Arc::new(MockProvider::new("gpt-4o", 128_000)));
        router.register(
            "anthropic",
            Arc::new(MockProvider::new("claude-haiku", 200_000)),
        );

        let mut keys = router.keys();
        keys.sort();
        assert_eq!(keys, vec!["anthropic", "openai"]);
    }

    #[test]
    fn test_register_with_meta_stores_metadata() {
        let router = ProviderRouter::new();
        let provider: Arc<dyn LlmProvider> = Arc::new(MockProvider::new("gpt-4o-mini", 128_000));
        router.register_with_meta("cheap", provider, Some("Fast and cheap".into()), None);

        let metas = router.list_models_with_meta();
        assert_eq!(metas.len(), 1);

        let m = &metas[0];
        assert_eq!(m.key, "cheap");
        assert_eq!(m.model_id, "gpt-4o-mini");
        assert_eq!(m.provider_name, "mock");
        assert_eq!(m.context_window, 128_000);
        assert_eq!(m.description.as_deref(), Some("Fast and cheap"));

        // gpt-4o-mini is a known model in pricing.rs
        assert!(m.cost_info.is_some());
        assert!(m.cost_info.as_ref().unwrap().contains("$0.15"));

        // Provider should also be resolvable
        let resolved = router.resolve("cheap/gpt-4o-mini").unwrap();
        assert_eq!(resolved.model_id(), "gpt-4o-mini");
    }

    #[test]
    fn test_register_with_meta_unknown_model_no_cost() {
        let router = ProviderRouter::new();
        let provider: Arc<dyn LlmProvider> = Arc::new(MockProvider::new("my-local-phi", 8_000));
        router.register_with_meta("local", provider, None, None);

        let metas = router.list_models_with_meta();
        assert_eq!(metas.len(), 1);
        assert_eq!(metas[0].key, "local");
        assert!(metas[0].cost_info.is_none());
        assert!(metas[0].description.is_none());
    }

    #[test]
    fn test_list_models_with_meta_multiple() {
        let router = ProviderRouter::new();
        router.register_with_meta(
            "cheap",
            Arc::new(MockProvider::new("gpt-4o-mini", 128_000)),
            Some("Cheap tasks".into()),
            Some(16_000),
        );
        router.register_with_meta(
            "strong",
            Arc::new(MockProvider::new("claude-sonnet-4-20250514", 200_000)),
            Some("Complex reasoning".into()),
            None,
        );

        let mut metas = router.list_models_with_meta();
        metas.sort_by(|a, b| a.key.cmp(&b.key));
        assert_eq!(metas.len(), 2);
        assert_eq!(metas[0].key, "cheap");
        assert_eq!(metas[1].key, "strong");
        // Both should have cost info (known models)
        assert!(metas[0].cost_info.is_some());
        assert!(metas[1].cost_info.is_some());
    }

    #[test]
    fn test_compatible_fallbacks() {
        let router = ProviderRouter::new();
        router.register_with_full_meta(
            "cheap",
            Arc::new(MockProvider::new("gpt-4o-mini", 128_000)),
            Some("Cheap".into()),
            None,
            Some(8192),
        );
        router.register_with_full_meta(
            "mid",
            Arc::new(MockProvider::new("deepseek-chat", 128_000)),
            Some("Mid".into()),
            None,
            Some(16384),
        );
        router.register_with_full_meta(
            "synth",
            Arc::new(MockProvider::new("gemini-3-flash", 1_000_000)),
            Some("Synth".into()),
            None,
            Some(65536),
        );

        // "cheap" (8k output) should have 2 fallbacks (mid=16k, synth=65k)
        let fb = router.compatible_fallbacks("cheap");
        assert_eq!(fb.len(), 2);

        // "synth" (65k output) should have 0 fallbacks (nothing else is >= 65k)
        let fb = router.compatible_fallbacks("synth");
        assert_eq!(fb.len(), 0);

        // "mid" (16k output) should have 1 fallback (synth=65k)
        let fb = router.compatible_fallbacks("mid");
        assert_eq!(fb.len(), 1);
    }

    #[test]
    fn test_compatible_fallbacks_prefers_lower_seeded_qos_score() {
        let router = ProviderRouter::new();
        router.register_with_full_meta(
            "gpt-4o-mini",
            Arc::new(MockProvider::new("gpt-4o-mini", 128_000)),
            Some("Primary".into()),
            None,
            Some(8_192),
        );
        router.register_with_full_meta(
            "deepseek-chat",
            Arc::new(MockProvider::new("deepseek-chat", 128_000)),
            Some("Better fallback".into()),
            None,
            Some(16_384),
        );
        router.register_with_full_meta(
            "gemini-3-flash",
            Arc::new(MockProvider::new("gemini-3-flash", 1_000_000)),
            Some("Worse fallback".into()),
            None,
            Some(16_384),
        );

        router.seed_qos_scores(&[
            ("openai/gpt-4o-mini".to_string(), 0.91),
            ("deepseek/deepseek-chat".to_string(), 0.18),
            ("gemini/gemini-3-flash".to_string(), 0.47),
        ]);

        let fallbacks = router.compatible_fallbacks("gpt-4o-mini");
        let ordered_models: Vec<&str> = fallbacks
            .iter()
            .map(|provider| provider.model_id())
            .collect();

        assert_eq!(ordered_models, vec!["deepseek-chat", "gemini-3-flash"]);
    }

    #[test]
    fn test_resolve_slash_in_key() {
        // NVIDIA model IDs contain a slash: "moonshotai/kimi-k2.5"
        // The full string is the key, not a prefix/model split.
        let router = ProviderRouter::new();
        router.register(
            "moonshotai/kimi-k2.5",
            Arc::new(MockProvider::new("kimi-k2.5", 128_000)),
        );

        // Exact match should work
        let resolved = router.resolve("moonshotai/kimi-k2.5").unwrap();
        assert_eq!(resolved.model_id(), "kimi-k2.5");

        // Prefix-only should NOT match (no "moonshotai" key registered)
        assert!(router.resolve("moonshotai").is_err());
    }

    #[test]
    fn test_resolve_prefers_exact_over_prefix() {
        // If both "openai" and "openai/gpt-4o" are registered,
        // exact match takes priority.
        let router = ProviderRouter::new();
        router.register("openai", Arc::new(MockProvider::new("gpt-4o", 128_000)));
        router.register(
            "openai/gpt-4o-mini",
            Arc::new(MockProvider::new("gpt-4o-mini", 128_000)),
        );

        // "openai/gpt-4o-mini" → exact match → gpt-4o-mini
        let resolved = router.resolve("openai/gpt-4o-mini").unwrap();
        assert_eq!(resolved.model_id(), "gpt-4o-mini");

        // "openai/gpt-4o" → no exact match → prefix split → "openai" → gpt-4o
        let resolved = router.resolve("openai/gpt-4o").unwrap();
        assert_eq!(resolved.model_id(), "gpt-4o");
    }
}
