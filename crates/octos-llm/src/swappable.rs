//! Runtime-swappable LLM provider wrapper.
//!
//! Wraps an `Arc<dyn LlmProvider>` behind an `RwLock` so the active provider
//! can be atomically replaced at runtime (e.g. via a `switch_model` tool).

use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use eyre::Result;
use octos_core::Message;

use crate::config::ChatConfig;
use crate::provider::LlmProvider;
use crate::types::{ChatResponse, ChatStream, ToolSpec};

/// Leak a `String` into a `&'static str`.
///
/// Used so `model_id()` / `provider_name()` can return `&str` without
/// borrowing from behind the `RwLock`. Each swap leaks ~50 bytes — acceptable
/// since swaps are rare, user-initiated actions.
fn leak_str(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}

/// A provider wrapper that allows swapping the inner provider at runtime.
///
/// `model_id()` and `provider_name()` return `&str`, which can't safely
/// borrow from behind an `RwLock`. We solve this by leaking the cached
/// strings into `&'static str` — since `&'static str` is `Copy`, reading
/// it out of the `RwLock` guard doesn't require the guard to stay alive.
/// Each swap leaks ~50 bytes, which is fine for rare user-initiated swaps.
pub struct SwappableProvider {
    inner: RwLock<Arc<dyn LlmProvider>>,
    cached_model_id: RwLock<&'static str>,
    cached_provider_name: RwLock<&'static str>,
}

impl SwappableProvider {
    /// Create a new swappable provider wrapping the given provider.
    pub fn new(provider: Arc<dyn LlmProvider>) -> Self {
        let model_id = leak_str(provider.model_id().to_string());
        let provider_name = leak_str(provider.provider_name().to_string());
        Self {
            inner: RwLock::new(provider),
            cached_model_id: RwLock::new(model_id),
            cached_provider_name: RwLock::new(provider_name),
        }
    }

    /// Atomically replace the inner provider with a new one.
    pub fn swap(&self, new_provider: Arc<dyn LlmProvider>) {
        let model_id = leak_str(new_provider.model_id().to_string());
        let provider_name = leak_str(new_provider.provider_name().to_string());
        *self.inner.write().unwrap() = new_provider;
        *self.cached_model_id.write().unwrap() = model_id;
        *self.cached_provider_name.write().unwrap() = provider_name;
    }

    /// Get the current provider name and model ID as owned strings.
    pub fn provider_info(&self) -> (String, String) {
        let name = (*self.cached_provider_name.read().unwrap()).to_string();
        let model = (*self.cached_model_id.read().unwrap()).to_string();
        (name, model)
    }

    /// Get a clone of the current inner provider Arc.
    pub fn current(&self) -> Arc<dyn LlmProvider> {
        self.inner.read().unwrap().clone()
    }
}

#[async_trait]
impl LlmProvider for SwappableProvider {
    async fn chat(
        &self,
        messages: &[Message],
        tools: &[ToolSpec],
        config: &ChatConfig,
    ) -> Result<ChatResponse> {
        // Clone the Arc to release the lock before the async call.
        let provider = self.inner.read().unwrap().clone();
        provider.chat(messages, tools, config).await
    }

    async fn chat_stream(
        &self,
        messages: &[Message],
        tools: &[ToolSpec],
        config: &ChatConfig,
    ) -> Result<ChatStream> {
        let provider = self.inner.read().unwrap().clone();
        provider.chat_stream(messages, tools, config).await
    }

    fn context_window(&self) -> u32 {
        self.inner.read().unwrap().context_window()
    }

    fn model_id(&self) -> &str {
        // &'static str is Copy — reading it from the guard yields a value
        // that doesn't depend on the guard's lifetime.
        *self.cached_model_id.read().unwrap()
    }

    fn provider_name(&self) -> &str {
        *self.cached_provider_name.read().unwrap()
    }

    fn export_metrics(&self) -> Option<serde_json::Value> {
        self.inner.read().unwrap().export_metrics()
    }

    fn report_late_failure(&self) {
        self.inner.read().unwrap().report_late_failure();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{StopReason, TokenUsage};

    struct MockProvider {
        name: &'static str,
        model: &'static str,
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
                content: Some(format!("from {}", self.name)),
                reasoning_content: None,
                tool_calls: vec![],
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage::default(),
                provider_index: None,
            })
        }

        fn model_id(&self) -> &str {
            self.model
        }

        fn provider_name(&self) -> &str {
            self.name
        }
    }

    #[test]
    fn test_provider_info() {
        let p = SwappableProvider::new(Arc::new(MockProvider {
            name: "test",
            model: "test-model",
        }));
        assert_eq!(p.provider_info(), ("test".into(), "test-model".into()));
    }

    #[test]
    fn test_swap_updates_info() {
        let p = SwappableProvider::new(Arc::new(MockProvider {
            name: "old",
            model: "old-model",
        }));
        p.swap(Arc::new(MockProvider {
            name: "new",
            model: "new-model",
        }));
        assert_eq!(p.provider_info(), ("new".into(), "new-model".into()));
        assert_eq!(p.model_id(), "new-model");
        assert_eq!(p.provider_name(), "new");
    }

    #[tokio::test]
    async fn test_chat_delegates() {
        let p = SwappableProvider::new(Arc::new(MockProvider {
            name: "test",
            model: "m1",
        }));
        let resp = p.chat(&[], &[], &ChatConfig::default()).await.unwrap();
        assert_eq!(resp.content.unwrap(), "from test");

        // Swap and verify delegation changes
        p.swap(Arc::new(MockProvider {
            name: "other",
            model: "m2",
        }));
        let resp = p.chat(&[], &[], &ChatConfig::default()).await.unwrap();
        assert_eq!(resp.content.unwrap(), "from other");
    }
}
