//! Middleware/interceptor pipeline for LLM requests and responses.
//!
//! Wraps `LlmProvider` with composable layers that can inspect, modify,
//! or short-circuit requests and responses (logging, cost tracking,
//! rate limiting, caching, etc.).

use std::sync::Arc;

use async_trait::async_trait;
use eyre::Result;
use octos_core::Message;

use crate::config::ChatConfig;
use crate::provider::LlmProvider;
use crate::types::{ChatResponse, ChatStream, ToolSpec};

/// A middleware that wraps an LLM provider call.

#[async_trait]
pub trait LlmMiddleware: Send + Sync {
    /// Called before the LLM request. Can modify messages/config or short-circuit.
    /// Return `None` to proceed to the next middleware/provider.
    /// Return `Some(response)` to short-circuit and skip the actual LLM call.
    async fn before(
        &self,
        _messages: &[Message],
        _tools: &[ToolSpec],
        _config: &ChatConfig,
    ) -> Result<Option<ChatResponse>> {
        Ok(None)
    }

    /// Called after a successful LLM response. Can inspect or transform.
    async fn after(&self, _messages: &[Message], response: ChatResponse) -> Result<ChatResponse> {
        Ok(response)
    }

    /// Called when the LLM request fails. Can inspect the error.
    /// Default: propagates the error unchanged.
    fn on_error(&self, _error: &eyre::Report) {}
}

/// An LLM provider wrapped with a middleware stack.
pub struct MiddlewareStack {
    inner: Arc<dyn LlmProvider>,
    layers: Vec<Arc<dyn LlmMiddleware>>,
}

impl MiddlewareStack {
    /// Create a stack wrapping the given provider.
    pub fn new(provider: Arc<dyn LlmProvider>) -> Self {
        Self {
            inner: provider,
            layers: Vec::new(),
        }
    }

    /// Add a middleware layer. Layers execute in insertion order.
    pub fn with(mut self, layer: Arc<dyn LlmMiddleware>) -> Self {
        self.layers.push(layer);
        self
    }

    /// Number of middleware layers.
    pub fn layer_count(&self) -> usize {
        self.layers.len()
    }
}

#[async_trait]
impl LlmProvider for MiddlewareStack {
    async fn chat(
        &self,
        messages: &[Message],
        tools: &[ToolSpec],
        config: &ChatConfig,
    ) -> Result<ChatResponse> {
        // Run before hooks — any can short-circuit
        for layer in &self.layers {
            if let Some(cached) = layer.before(messages, tools, config).await? {
                return Ok(cached);
            }
        }

        // Call the actual provider
        let result = self.inner.chat(messages, tools, config).await;

        match result {
            Ok(response) => {
                // Run after hooks in order
                let mut response = response;
                for layer in &self.layers {
                    response = layer.after(messages, response).await?;
                }
                Ok(response)
            }
            Err(e) => {
                for layer in &self.layers {
                    layer.on_error(&e);
                }
                Err(e)
            }
        }
    }

    async fn chat_stream(
        &self,
        messages: &[Message],
        tools: &[ToolSpec],
        config: &ChatConfig,
    ) -> Result<ChatStream> {
        if !self.layers.is_empty() {
            tracing::debug!(
                layer_count = self.layers.len(),
                "streaming call bypasses {} middleware layer(s)",
                self.layers.len()
            );
        }
        self.inner.chat_stream(messages, tools, config).await
    }

    fn model_id(&self) -> &str {
        self.inner.model_id()
    }

    fn provider_name(&self) -> &str {
        self.inner.provider_name()
    }

    fn provider_metadata(&self) -> crate::ProviderMetadata {
        // #2194 R4: transparent wrapper — carry the inner slot's cache lane
        // (and identity) through, or pricing sees the default Residual lane.
        self.inner.provider_metadata()
    }

    fn provider_metadata_for_index(
        &self,
        provider_index: Option<usize>,
    ) -> crate::types::ProviderMetadata {
        self.inner.provider_metadata_for_index(provider_index)
    }

    fn provider_lane_count(&self) -> usize {
        self.inner.provider_lane_count()
    }

    fn api_style(&self) -> Option<crate::provider::ApiStyle> {
        self.inner.api_style()
    }

    fn context_window(&self) -> u32 {
        self.inner.context_window()
    }

    fn supports_semantic_checkpoint_hints(&self) -> bool {
        self.inner.supports_semantic_checkpoint_hints()
    }

    fn report_late_failure(&self) {
        self.inner.report_late_failure();
    }
}

/// Middleware that logs request/response metadata via `tracing`.
pub struct LoggingMiddleware;

#[async_trait]
impl LlmMiddleware for LoggingMiddleware {
    async fn before(
        &self,
        messages: &[Message],
        tools: &[ToolSpec],
        _config: &ChatConfig,
    ) -> Result<Option<ChatResponse>> {
        tracing::debug!(
            message_count = messages.len(),
            tool_count = tools.len(),
            "LLM request"
        );
        Ok(None)
    }

    async fn after(&self, _messages: &[Message], response: ChatResponse) -> Result<ChatResponse> {
        tracing::debug!(
            input_tokens = response.usage.input_tokens,
            output_tokens = response.usage.output_tokens,
            tool_calls = response.tool_calls.len(),
            stop = ?response.stop_reason,
            "LLM response"
        );
        Ok(response)
    }

    fn on_error(&self, error: &eyre::Report) {
        tracing::warn!("LLM error: {error}");
    }
}

/// Middleware that tracks cumulative token usage.
pub struct CostTracker {
    total_input: std::sync::atomic::AtomicU64,
    total_output: std::sync::atomic::AtomicU64,
    request_count: std::sync::atomic::AtomicU64,
}

impl CostTracker {
    pub fn new() -> Self {
        Self {
            total_input: std::sync::atomic::AtomicU64::new(0),
            total_output: std::sync::atomic::AtomicU64::new(0),
            request_count: std::sync::atomic::AtomicU64::new(0),
        }
    }

    pub fn total_input_tokens(&self) -> u64 {
        self.total_input.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn total_output_tokens(&self) -> u64 {
        self.total_output.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn request_count(&self) -> u64 {
        self.request_count
            .load(std::sync::atomic::Ordering::Relaxed)
    }
}

impl Default for CostTracker {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl LlmMiddleware for CostTracker {
    async fn after(&self, _messages: &[Message], response: ChatResponse) -> Result<ChatResponse> {
        use std::sync::atomic::Ordering::Relaxed;
        self.total_input
            .fetch_add(u64::from(response.usage.input_tokens), Relaxed);
        self.total_output
            .fetch_add(u64::from(response.usage.output_tokens), Relaxed);
        self.request_count.fetch_add(1, Relaxed);
        Ok(response)
    }
}

#[cfg(test)]
mod tests {}

#[cfg(test)]
mod provider_metadata_tests {}
