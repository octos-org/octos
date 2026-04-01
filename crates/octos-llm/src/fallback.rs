//! Fallback provider — wraps a primary provider with QoS-ranked fallbacks
//! and cooldown-based failure exclusion.

use std::sync::Arc;

use async_trait::async_trait;
use eyre::Result;
use octos_core::Message;
use tracing::warn;

use crate::config::ChatConfig;
use crate::provider::LlmProvider;
use crate::retry::RetryProvider;
use crate::router::ProviderRouter;
use crate::types::{ChatResponse, ChatStream, ToolSpec};

/// A provider that falls back to compatible alternatives on failure.
/// When a provider fails, it's put in cooldown via the router so future
/// requests avoid it temporarily.
pub struct FallbackProvider {
    primary: Arc<dyn LlmProvider>,
    fallbacks: Vec<Arc<dyn LlmProvider>>,
    /// Optional router reference for recording failures (cooldown).
    router: Option<Arc<ProviderRouter>>,
}

impl FallbackProvider {
    pub fn new(primary: Arc<dyn LlmProvider>, fallbacks: Vec<Arc<dyn LlmProvider>>) -> Self {
        Self {
            primary,
            fallbacks,
            router: None,
        }
    }

    /// Attach a router for cooldown tracking.
    pub fn with_router(mut self, router: Arc<ProviderRouter>) -> Self {
        self.router = Some(router);
        self
    }

    /// Create a FallbackProvider only if there are fallbacks available.
    /// Returns the primary provider directly if no fallbacks.
    pub fn wrap_if_needed(
        primary: Arc<dyn LlmProvider>,
        fallbacks: Vec<Arc<dyn LlmProvider>>,
    ) -> Arc<dyn LlmProvider> {
        if fallbacks.is_empty() {
            primary
        } else {
            Arc::new(Self::new(primary, fallbacks))
        }
    }

    /// Create with router for cooldown tracking.
    pub fn wrap_with_router(
        primary: Arc<dyn LlmProvider>,
        fallbacks: Vec<Arc<dyn LlmProvider>>,
        router: Arc<ProviderRouter>,
    ) -> Arc<dyn LlmProvider> {
        if fallbacks.is_empty() {
            primary
        } else {
            Arc::new(Self::new(primary, fallbacks).with_router(router))
        }
    }

    /// Record a failure for cooldown tracking.
    fn record_failure(&self, model_id: &str) {
        if let Some(ref router) = self.router {
            router.record_failure(model_id);
        }
    }
}

#[async_trait]
impl LlmProvider for FallbackProvider {
    async fn chat(
        &self,
        messages: &[Message],
        tools: &[ToolSpec],
        config: &ChatConfig,
    ) -> Result<ChatResponse> {
        match self.primary.chat(messages, tools, config).await {
            Ok(resp) => Ok(resp),
            Err(primary_err) => {
                if !RetryProvider::should_failover(&primary_err) {
                    return Err(primary_err);
                }
                self.record_failure(self.primary.model_id());
                warn!(
                    primary = self.primary.model_id(),
                    error = %primary_err,
                    fallback_count = self.fallbacks.len(),
                    "primary provider failed, trying fallbacks"
                );
                for (i, fb) in self.fallbacks.iter().enumerate() {
                    match fb.chat(messages, tools, config).await {
                        Ok(resp) => {
                            warn!(
                                primary = self.primary.model_id(),
                                fallback = fb.model_id(),
                                fallback_idx = i,
                                "fallback provider succeeded"
                            );
                            return Ok(resp);
                        }
                        Err(e) => {
                            self.record_failure(fb.model_id());
                            warn!(
                                fallback = fb.model_id(),
                                error = %e,
                                "fallback provider also failed"
                            );
                        }
                    }
                }
                Err(primary_err)
            }
        }
    }

    async fn chat_stream(
        &self,
        messages: &[Message],
        tools: &[ToolSpec],
        config: &ChatConfig,
    ) -> Result<ChatStream> {
        match self.primary.chat_stream(messages, tools, config).await {
            Ok(stream) => Ok(stream),
            Err(primary_err) => {
                if !RetryProvider::should_failover(&primary_err) {
                    return Err(primary_err);
                }
                self.record_failure(self.primary.model_id());
                warn!(
                    primary = self.primary.model_id(),
                    error = %primary_err,
                    "primary stream failed, trying fallbacks"
                );
                for fb in &self.fallbacks {
                    match fb.chat_stream(messages, tools, config).await {
                        Ok(stream) => return Ok(stream),
                        Err(e) => {
                            self.record_failure(fb.model_id());
                            warn!(fallback = fb.model_id(), error = %e, "fallback stream also failed");
                        }
                    }
                }
                Err(primary_err)
            }
        }
    }

    fn model_id(&self) -> &str {
        self.primary.model_id()
    }

    fn provider_name(&self) -> &str {
        self.primary.provider_name()
    }

    fn context_window(&self) -> u32 {
        self.primary.context_window()
    }

    fn max_output_tokens(&self) -> u32 {
        self.primary.max_output_tokens()
    }

    fn report_stream_metrics(&self, output_tokens: u32, stream_duration_us: u64) {
        self.primary.report_stream_metrics(output_tokens, stream_duration_us);
    }
