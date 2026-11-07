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

    fn provider_metadata(&self) -> crate::ProviderMetadata {
        // #2194 R4: delegate to the active sub-provider so its cache lane (and
        // real model/provider) reaches pricing, not the "router"/Residual stub.
        self.active_provider()
            .map(|p| p.provider_metadata())
            .unwrap_or_else(|_| {
                crate::ProviderMetadata::new(self.provider_name(), self.model_id(), None)
            })
    }

    fn provider_metadata_for_index(
        &self,
        provider_index: Option<usize>,
    ) -> crate::types::ProviderMetadata {
        self.active_provider()
            .map(|provider| provider.provider_metadata_for_index(provider_index))
            .unwrap_or_else(|_| self.provider_metadata())
    }

    fn provider_lane_count(&self) -> usize {
        self.active_provider()
            .map(|provider| provider.provider_lane_count())
            .unwrap_or(1)
    }

    fn api_style(&self) -> Option<crate::provider::ApiStyle> {
        self.active_provider()
            .ok()
            .and_then(|provider| provider.api_style())
    }

    fn supports_semantic_checkpoint_hints(&self) -> bool {
        self.active_provider()
            .is_ok_and(|provider| provider.supports_semantic_checkpoint_hints())
    }

    fn report_late_failure(&self) {
        if let Ok(p) = self.active_provider() {
            p.report_late_failure();
        }
    }
}

#[cfg(test)]
mod tests {}

#[cfg(test)]
mod provider_metadata_tests {}
