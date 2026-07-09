//! Semaphore-based throttling for LLM providers.

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use async_trait::async_trait;
use eyre::Result;
use futures::Stream;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use octos_core::Message;

use crate::config::ChatConfig;
use crate::provider::LlmProvider;
use crate::types::{ChatResponse, ChatStream, ProviderMetadata, StreamEvent, ToolSpec};

/// Wraps an [`LlmProvider`] and caps concurrent LLM calls with a shared semaphore.
///
/// The wrapper is deliberately provider-agnostic: it prevents thundering-herd
/// pipeline fan-outs without parsing provider-specific rate-limit responses or
/// competing with adaptive router circuit breakers.
pub struct SemaphoreThrottledProvider {
    inner: Arc<dyn LlmProvider>,
    semaphore: Arc<Semaphore>,
}

impl SemaphoreThrottledProvider {
    pub fn new(inner: Arc<dyn LlmProvider>, semaphore: Arc<Semaphore>) -> Self {
        Self { inner, semaphore }
    }

    pub fn with_limit(inner: Arc<dyn LlmProvider>, max_concurrent_calls: usize) -> Self {
        Self::new(inner, Arc::new(Semaphore::new(max_concurrent_calls.max(1))))
    }

    #[doc(hidden)]
    pub fn available_permits_for_test(&self) -> usize {
        self.semaphore.available_permits()
    }

    async fn acquire(&self) -> Result<OwnedSemaphorePermit> {
        self.semaphore
            .clone()
            .acquire_owned()
            .await
            .map_err(|error| eyre::eyre!("LLM throttle semaphore closed: {error}"))
    }
}

#[async_trait]
impl LlmProvider for SemaphoreThrottledProvider {
    async fn chat(
        &self,
        messages: &[Message],
        tools: &[ToolSpec],
        config: &ChatConfig,
    ) -> Result<ChatResponse> {
        let _permit = self.acquire().await?;
        self.inner.chat(messages, tools, config).await
    }

    async fn chat_stream(
        &self,
        messages: &[Message],
        tools: &[ToolSpec],
        config: &ChatConfig,
    ) -> Result<ChatStream> {
        let permit = self.acquire().await?;
        let stream = self.inner.chat_stream(messages, tools, config).await?;
        Ok(Box::pin(PermitHeldStream {
            inner: stream,
            _permit: permit,
        }))
    }

    fn context_window(&self) -> u32 {
        self.inner.context_window()
    }

    fn max_output_tokens(&self) -> u32 {
        self.inner.max_output_tokens()
    }

    fn model_id(&self) -> &str {
        self.inner.model_id()
    }

    fn provider_name(&self) -> &str {
        self.inner.provider_name()
    }

    fn provider_metadata(&self) -> ProviderMetadata {
        self.inner.provider_metadata()
    }

    fn provider_metadata_for_index(&self, provider_index: Option<usize>) -> ProviderMetadata {
        self.inner.provider_metadata_for_index(provider_index)
    }

    fn export_metrics(&self) -> Option<serde_json::Value> {
        self.inner.export_metrics()
    }

    fn report_late_failure(&self) {
        self.inner.report_late_failure();
    }

    fn report_stream_metrics(&self, output_tokens: u32, stream_duration_us: u64) {
        self.inner
            .report_stream_metrics(output_tokens, stream_duration_us);
    }
}

struct PermitHeldStream {
    inner: ChatStream,
    _permit: OwnedSemaphorePermit,
}

impl Stream for PermitHeldStream {
    type Item = StreamEvent;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        this.inner.as_mut().poll_next(cx)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use super::*;
    use crate::{StopReason, TokenUsage};

    struct SlowProvider {
        active: Arc<AtomicUsize>,
        max_seen: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl LlmProvider for SlowProvider {
        async fn chat(
            &self,
            _messages: &[Message],
            _tools: &[ToolSpec],
            _config: &ChatConfig,
        ) -> Result<ChatResponse> {
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_seen.fetch_max(active, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(50)).await;
            self.active.fetch_sub(1, Ordering::SeqCst);
            Ok(response())
        }

        fn model_id(&self) -> &str {
            "slow"
        }

        fn provider_name(&self) -> &str {
            "test"
        }
    }

    struct ImmediateProvider;

    #[async_trait::async_trait]
    impl LlmProvider for ImmediateProvider {
        async fn chat(
            &self,
            _messages: &[Message],
            _tools: &[ToolSpec],
            _config: &ChatConfig,
        ) -> Result<ChatResponse> {
            Ok(response())
        }

        async fn chat_stream(
            &self,
            _messages: &[Message],
            _tools: &[ToolSpec],
            _config: &ChatConfig,
        ) -> Result<ChatStream> {
            Ok(Box::pin(futures::stream::pending()))
        }

        fn model_id(&self) -> &str {
            "immediate"
        }

        fn provider_name(&self) -> &str {
            "test"
        }
    }

    fn response() -> ChatResponse {
        ChatResponse {
            content: Some("ok".into()),
            reasoning_content: None,
            tool_calls: vec![],
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage::default(),
            provider_index: None,
        }
    }

    #[tokio::test]
    async fn chat_calls_share_the_configured_concurrency_limit() {
        let active = Arc::new(AtomicUsize::new(0));
        let max_seen = Arc::new(AtomicUsize::new(0));
        let inner = Arc::new(SlowProvider {
            active: active.clone(),
            max_seen: max_seen.clone(),
        });
        let provider = Arc::new(SemaphoreThrottledProvider::with_limit(inner, 2));

        let mut tasks = Vec::new();
        for _ in 0..5 {
            let provider = provider.clone();
            tasks.push(tokio::spawn(async move {
                provider.chat(&[], &[], &ChatConfig::default()).await
            }));
        }

        for task in tasks {
            task.await.unwrap().unwrap();
        }

        assert_eq!(active.load(Ordering::SeqCst), 0);
        assert_eq!(max_seen.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn streaming_calls_hold_permit_until_stream_is_dropped() {
        let provider = SemaphoreThrottledProvider::with_limit(Arc::new(ImmediateProvider), 1);

        let stream = provider
            .chat_stream(&[], &[], &ChatConfig::default())
            .await
            .unwrap();
        assert_eq!(provider.available_permits_for_test(), 0);

        let blocked = tokio::time::timeout(
            Duration::from_millis(50),
            provider.chat(&[], &[], &ChatConfig::default()),
        )
        .await;
        assert!(
            blocked.is_err(),
            "second call should wait while the stream holds the only permit"
        );

        drop(stream);
        provider
            .chat(&[], &[], &ChatConfig::default())
            .await
            .unwrap();
        assert_eq!(provider.available_permits_for_test(), 1);
    }
}
