//! Context window override wrapper for LlmProvider.

use std::sync::Arc;

use async_trait::async_trait;
use eyre::Result;
use octos_core::Message;

use crate::config::ChatConfig;
use crate::provider::LlmProvider;
use crate::types::{ChatResponse, ChatStream, ToolSpec};

/// A thin wrapper that overrides `context_window()` while delegating
/// all other methods to the inner provider. Used when a sub-agent needs
/// a different context budget without changing the underlying model.
pub struct ContextWindowOverride {
    inner: Arc<dyn LlmProvider>,
    window: u32,
}

impl ContextWindowOverride {
    pub fn new(inner: Arc<dyn LlmProvider>, window: u32) -> Self {
        Self { inner, window }
    }
}

#[async_trait]
impl LlmProvider for ContextWindowOverride {
    async fn chat(
        &self,
        messages: &[Message],
        tools: &[ToolSpec],
        config: &ChatConfig,
    ) -> Result<ChatResponse> {
        self.inner.chat(messages, tools, config).await
    }

    async fn chat_stream(
        &self,
        messages: &[Message],
        tools: &[ToolSpec],
        config: &ChatConfig,
    ) -> Result<ChatStream> {
        self.inner.chat_stream(messages, tools, config).await
    }

    fn context_window(&self) -> u32 {
        self.window
    }

    fn model_id(&self) -> &str {
        self.inner.model_id()
    }

    fn provider_name(&self) -> &str {
        self.inner.provider_name()
    }

    fn report_late_failure(&self) {
        self.inner.report_late_failure();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::TokenUsage;

    struct DummyProvider;

    #[async_trait]
    impl LlmProvider for DummyProvider {
        async fn chat(
            &self,
            _messages: &[Message],
            _tools: &[ToolSpec],
            _config: &ChatConfig,
        ) -> Result<ChatResponse> {
            Ok(ChatResponse {
                content: Some("ok".into()),
                reasoning_content: None,
                tool_calls: vec![],
                stop_reason: crate::StopReason::EndTurn,
                usage: TokenUsage::default(),
            })
        }

        fn model_id(&self) -> &str {
            "test-model"
        }

        fn provider_name(&self) -> &str {
            "test"
        }
    }

    #[test]
    fn test_overrides_context_window() {
        let inner: Arc<dyn LlmProvider> = Arc::new(DummyProvider);
        assert_eq!(inner.context_window(), 128_000); // default from model_id lookup

        let overridden = ContextWindowOverride::new(inner, 4_000);
        assert_eq!(overridden.context_window(), 4_000);
        assert_eq!(overridden.model_id(), "test-model");
        assert_eq!(overridden.provider_name(), "test");
    }
}
