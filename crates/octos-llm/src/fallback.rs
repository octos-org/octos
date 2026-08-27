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
                if crate::current_llm_call_policy() == crate::LlmCallPolicy::FailFast {
                    return Err(primary_err);
                }
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
                    // #2135 round-7 P1: the fallback receives the unchanged
                    // request — resolve its readiness and skip it when the
                    // prompt cannot fit its (possibly just-resolved) window.
                    if !crate::context::route_fits_request(fb, messages).await {
                        warn!(
                            fallback = fb.model_id(),
                            window = fb.context_window(),
                            "skipping fallback: prompt does not fit its context window"
                        );
                        continue;
                    }
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
                if crate::current_llm_call_policy() == crate::LlmCallPolicy::FailFast {
                    return Err(primary_err);
                }
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
                    // #2135 round-7 P1: same fit guard as the chat path.
                    if !crate::context::route_fits_request(fb, messages).await {
                        warn!(
                            fallback = fb.model_id(),
                            window = fb.context_window(),
                            "skipping fallback: prompt does not fit its context window"
                        );
                        continue;
                    }
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

    // #2135 round-6 P1: the MINIMUM across primary and every fallback —
    // this wrapper re-sends the SAME messages to a fallback on failure, so
    // a prompt sized for a probed 256K primary must fit the smallest route
    // it can take. (The pre-probe catalog value was accidentally safe this
    // way; per-route resizing before each fallback send is the precise
    // fix, tracked with the router follow-ups.)
    fn context_window(&self) -> u32 {
        std::iter::once(self.primary.context_window())
            .chain(self.fallbacks.iter().map(|fb| fb.context_window()))
            .min()
            .unwrap_or_else(|| self.primary.context_window())
    }

    async fn ensure_ready(&self) {
        self.primary.ensure_ready().await;
    }

    fn max_output_tokens(&self) -> u32 {
        std::iter::once(self.primary.max_output_tokens())
            .chain(self.fallbacks.iter().map(|fb| fb.max_output_tokens()))
            .min()
            .unwrap_or_else(|| self.primary.max_output_tokens())
    }

    fn report_stream_metrics(&self, output_tokens: u32, stream_duration_us: u64) {
        self.primary
            .report_stream_metrics(output_tokens, stream_duration_us);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;
    use eyre::Result;
    use octos_core::Message;

    use super::FallbackProvider;
    use crate::config::ChatConfig;
    use crate::error::{LlmError, LlmErrorKind};
    use crate::provider::LlmProvider;
    use crate::types::{ChatResponse, ChatStream, StopReason, TokenUsage, ToolSpec};

    /// A provider with a shared call counter that either always errors or
    /// always succeeds, depending on how it was constructed.
    struct CountingProvider {
        calls: Arc<AtomicUsize>,
        mode: CountingMode,
    }

    enum CountingMode {
        AlwaysErr500,
        AlwaysOk,
    }

    impl CountingProvider {
        fn always_err_500() -> Self {
            Self {
                calls: Arc::new(AtomicUsize::new(0)),
                mode: CountingMode::AlwaysErr500,
            }
        }

        fn ok() -> Self {
            Self {
                calls: Arc::new(AtomicUsize::new(0)),
                mode: CountingMode::AlwaysOk,
            }
        }
    }

    #[async_trait]
    impl LlmProvider for CountingProvider {
        async fn chat(
            &self,
            _messages: &[Message],
            _tools: &[ToolSpec],
            _config: &ChatConfig,
        ) -> Result<ChatResponse> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            match self.mode {
                CountingMode::AlwaysErr500 => Err(LlmError::new(
                    LlmErrorKind::ServerError { status: 500 },
                    "internal server error",
                )
                .into()),
                CountingMode::AlwaysOk => Ok(ChatResponse {
                    content: Some("ok".to_string()),
                    reasoning_content: None,
                    tool_calls: vec![],
                    stop_reason: StopReason::EndTurn,
                    usage: TokenUsage::default(),
                    provider_index: None,
                }),
            }
        }

        async fn chat_stream(
            &self,
            _messages: &[Message],
            _tools: &[ToolSpec],
            _config: &ChatConfig,
        ) -> Result<ChatStream> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            match self.mode {
                CountingMode::AlwaysErr500 => Err(LlmError::new(
                    LlmErrorKind::ServerError { status: 500 },
                    "internal server error",
                )
                .into()),
                CountingMode::AlwaysOk => {
                    let stream = futures::stream::empty();
                    Ok(Box::pin(stream))
                }
            }
        }

        fn model_id(&self) -> &str {
            "counting"
        }

        fn provider_name(&self) -> &str {
            "test"
        }
    }

    #[tokio::test]
    async fn should_not_failover_when_failfast() {
        use crate::{LlmCallPolicy, with_llm_call_policy};

        let primary = CountingProvider::always_err_500();
        let fallback = CountingProvider::ok();
        let fb_calls = fallback.calls.clone();
        let chain = FallbackProvider::new(Arc::new(primary), vec![Arc::new(fallback)]);

        let result = with_llm_call_policy(LlmCallPolicy::FailFast, async {
            chain.chat(&[], &[], &ChatConfig::default()).await
        })
        .await;

        assert!(
            result.is_err(),
            "FailFast returns primary error, no failover"
        );
        assert_eq!(
            fb_calls.load(Ordering::SeqCst),
            0,
            "fallback must not be called"
        );
    }

    #[tokio::test]
    async fn should_not_failover_stream_when_failfast() {
        use crate::{LlmCallPolicy, with_llm_call_policy};

        let primary = CountingProvider::always_err_500();
        let fallback = CountingProvider::ok();
        let fb_calls = fallback.calls.clone();
        let chain = FallbackProvider::new(Arc::new(primary), vec![Arc::new(fallback)]);

        let result = with_llm_call_policy(LlmCallPolicy::FailFast, async {
            chain.chat_stream(&[], &[], &ChatConfig::default()).await
        })
        .await;

        assert!(
            result.is_err(),
            "FailFast returns primary error, no failover"
        );
        assert_eq!(
            fb_calls.load(Ordering::SeqCst),
            0,
            "fallback must not be called on stream"
        );
    }

    #[tokio::test]
    async fn should_failover_when_normal_policy() {
        use crate::{LlmCallPolicy, with_llm_call_policy};

        let primary = CountingProvider::always_err_500();
        let fallback = CountingProvider::ok();
        let fb_calls = fallback.calls.clone();
        let chain = FallbackProvider::new(Arc::new(primary), vec![Arc::new(fallback)]);

        let result = with_llm_call_policy(LlmCallPolicy::Normal, async {
            chain.chat(&[], &[], &ChatConfig::default()).await
        })
        .await;

        assert!(result.is_ok(), "Normal policy should failover and succeed");
        assert_eq!(
            fb_calls.load(Ordering::SeqCst),
            1,
            "fallback must be called once"
        );
    }

    /// #2135 round-7 P1: a fallback whose window resolves SMALL only at
    /// dispatch time (its probe was unresolved when the prompt was sized)
    /// must be skipped, not sent an oversized request. The mock reports a
    /// huge window until ensure_ready(), then the truth: tiny.
    struct LateSmallProvider {
        resolved: std::sync::atomic::AtomicBool,
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl LlmProvider for LateSmallProvider {
        async fn chat(
            &self,
            _messages: &[Message],
            _tools: &[ToolSpec],
            _config: &ChatConfig,
        ) -> Result<ChatResponse> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(ChatResponse {
                content: Some("from-late-small".into()),
                reasoning_content: None,
                tool_calls: vec![],
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage::default(),
                provider_index: None,
            })
        }

        fn model_id(&self) -> &str {
            "late-small"
        }

        fn provider_name(&self) -> &str {
            "local"
        }

        fn context_window(&self) -> u32 {
            if self.resolved.load(Ordering::SeqCst) {
                1_024
            } else {
                131_072
            }
        }

        async fn ensure_ready(&self) {
            self.resolved.store(true, Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn should_skip_fallback_that_resolves_too_small_at_dispatch() {
        let failing_primary: Arc<dyn LlmProvider> = Arc::new(CountingProvider::always_err_500());
        let small_calls = Arc::new(AtomicUsize::new(0));
        let late_small: Arc<dyn LlmProvider> = Arc::new(LateSmallProvider {
            resolved: std::sync::atomic::AtomicBool::new(false),
            calls: small_calls.clone(),
        });
        let big_ok: Arc<dyn LlmProvider> = Arc::new(crate::ContextWindowOverride::new(
            Arc::new(CountingProvider::ok()),
            131_072,
        ));
        let provider = FallbackProvider::new(failing_primary, vec![late_small, big_ok]);
        // A prompt far larger than the late-resolving 1K window.
        let big_message = Message::user("x ".repeat(20_000));
        let response = provider
            .chat(&[big_message], &[], &ChatConfig::default())
            .await
            .expect("second fallback must serve the request");
        assert_eq!(response.content.unwrap(), "ok");
        assert_eq!(
            small_calls.load(Ordering::SeqCst),
            0,
            "the too-small fallback must be skipped at dispatch"
        );
    }

    /// #2135 round-6 P1: the same messages are re-sent to a fallback on
    /// failure, so the reported window must fit the SMALLEST possible
    /// route — a probed 256K primary over a 32K fallback budgets as 32K.
    #[test]
    fn should_report_minimum_window_across_routes() {
        let primary: Arc<dyn LlmProvider> = Arc::new(crate::ContextWindowOverride::new(
            Arc::new(CountingProvider::ok()),
            262_144,
        ));
        let small_fallback: Arc<dyn LlmProvider> = Arc::new(crate::ContextWindowOverride::new(
            Arc::new(CountingProvider::ok()),
            32_768,
        ));
        let provider = FallbackProvider::new(primary, vec![small_fallback]);
        assert_eq!(provider.context_window(), 32_768);
    }
}
