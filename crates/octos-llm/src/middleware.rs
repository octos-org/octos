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

    fn context_window(&self) -> u32 {
        self.inner.context_window()
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
mod tests {
    use super::*;
    use crate::types::{StopReason, TokenUsage};

    struct FakeProvider;

    #[async_trait]
    impl LlmProvider for FakeProvider {
        async fn chat(
            &self,
            _messages: &[Message],
            _tools: &[ToolSpec],
            _config: &ChatConfig,
        ) -> Result<ChatResponse> {
            Ok(ChatResponse {
                content: Some("hello".into()),
                reasoning_content: None,
                tool_calls: vec![],
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage {
                    input_tokens: 10,
                    output_tokens: 5,
                    ..Default::default()
                },
                provider_index: None,
            })
        }

        fn model_id(&self) -> &str {
            "fake"
        }

        fn provider_name(&self) -> &str {
            "test"
        }
    }

    #[tokio::test]
    async fn should_pass_through_without_middleware() {
        let stack = MiddlewareStack::new(Arc::new(FakeProvider));
        let resp = stack.chat(&[], &[], &ChatConfig::default()).await.unwrap();
        assert_eq!(resp.content.as_deref(), Some("hello"));
    }

    #[tokio::test]
    async fn should_track_costs() {
        let tracker = Arc::new(CostTracker::new());
        let stack = MiddlewareStack::new(Arc::new(FakeProvider)).with(tracker.clone());

        stack.chat(&[], &[], &ChatConfig::default()).await.unwrap();
        stack.chat(&[], &[], &ChatConfig::default()).await.unwrap();

        assert_eq!(tracker.total_input_tokens(), 20);
        assert_eq!(tracker.total_output_tokens(), 10);
        assert_eq!(tracker.request_count(), 2);
    }

    #[tokio::test]
    async fn should_short_circuit_on_before() {
        struct CacheHit;

        #[async_trait]
        impl LlmMiddleware for CacheHit {
            async fn before(
                &self,
                _messages: &[Message],
                _tools: &[ToolSpec],
                _config: &ChatConfig,
            ) -> Result<Option<ChatResponse>> {
                Ok(Some(ChatResponse {
                    content: Some("cached".into()),
                    reasoning_content: None,
                    tool_calls: vec![],
                    stop_reason: StopReason::EndTurn,
                    usage: TokenUsage::default(),
                    provider_index: None,
                }))
            }
        }

        let stack = MiddlewareStack::new(Arc::new(FakeProvider)).with(Arc::new(CacheHit));

        let resp = stack.chat(&[], &[], &ChatConfig::default()).await.unwrap();
        assert_eq!(resp.content.as_deref(), Some("cached"));
    }

    #[tokio::test]
    async fn should_transform_response_in_after() {
        struct Uppercaser;

        #[async_trait]
        impl LlmMiddleware for Uppercaser {
            async fn after(
                &self,
                _messages: &[Message],
                mut response: ChatResponse,
            ) -> Result<ChatResponse> {
                if let Some(ref mut c) = response.content {
                    *c = c.to_uppercase();
                }
                Ok(response)
            }
        }

        let stack = MiddlewareStack::new(Arc::new(FakeProvider)).with(Arc::new(Uppercaser));

        let resp = stack.chat(&[], &[], &ChatConfig::default()).await.unwrap();
        assert_eq!(resp.content.as_deref(), Some("HELLO"));
    }

    #[test]
    fn should_count_layers() {
        let stack = MiddlewareStack::new(Arc::new(FakeProvider))
            .with(Arc::new(LoggingMiddleware))
            .with(Arc::new(CostTracker::new()));
        assert_eq!(stack.layer_count(), 2);
    }

    #[test]
    fn should_delegate_model_info() {
        let stack = MiddlewareStack::new(Arc::new(FakeProvider));
        assert_eq!(stack.model_id(), "fake");
        assert_eq!(stack.provider_name(), "test");
    }
}
