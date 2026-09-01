//! Fallback provider — wraps a primary provider with QoS-ranked fallbacks
//! and cooldown-based failure exclusion.

use std::sync::Arc;

use async_trait::async_trait;
use eyre::Result;
use futures::StreamExt;
use octos_core::Message;
use tracing::warn;

use crate::config::ChatConfig;
use crate::provider::LlmProvider;
use crate::retry::RetryProvider;
use crate::router::ProviderRouter;
use crate::types::{ChatResponse, ChatStream, ProviderMetadata, StreamEvent, ToolSpec};

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

    /// Prepend the winning slot index to a stream so a downstream consumer can
    /// attribute the response to the exact answering provider (mirrors
    /// `ProviderChain::stream_with_provider_index`).
    fn stream_with_provider_index(&self, idx: usize, stream: ChatStream) -> ChatStream {
        Box::pin(
            futures::stream::once(async move { StreamEvent::ProviderIndex(idx) }).chain(stream),
        )
    }
}

/// Fallback wins are attributed with `FALLBACK_INDEX_BASE + j`, a range
/// disjoint from any realistic primary slot index, so metadata resolution can
/// tell a fallback win from the primary's OWN (possibly nested-container)
/// winner index WITHOUT clobbering the latter. No provider has a million slots.
///
/// KNOWN LIMITATION (#2199): this flat tag does NOT compose when the PRIMARY is
/// itself a `FallbackProvider` — the inner's `FALLBACK_INDEX_BASE + j` tag is
/// indistinguishable from the outer's own, so a preserved inner-fallback index
/// resolves to the OUTER's fallback. Same root cause as #2199 (a flat
/// `provider_index` cannot carry `(which-child, child-index)` through nesting);
/// the durable fix carries the answering leaf's metadata on the response.
pub(crate) const FALLBACK_INDEX_BASE: usize = 1_000_000;

#[async_trait]
impl LlmProvider for FallbackProvider {
    async fn chat(
        &self,
        messages: &[Message],
        tools: &[ToolSpec],
        config: &ChatConfig,
    ) -> Result<ChatResponse> {
        match self.primary.chat(messages, tools, config).await {
            // #2194 R6: PRESERVE the primary's own attribution. A nested
            // container primary already stamped its real winner; clobbering it
            // with a flat slot index would mis-resolve a mixed-lane primary.
            // Resolution treats any primary-range index as the primary's own.
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
                    if !crate::context::route_fits_request(fb, messages, tools).await {
                        warn!(
                            fallback = fb.model_id(),
                            window = fb.context_window(),
                            "skipping fallback: prompt does not fit its context window"
                        );
                        continue;
                    }
                    match fb.chat(messages, tools, config).await {
                        Ok(mut resp) => {
                            warn!(
                                primary = self.primary.model_id(),
                                fallback = fb.model_id(),
                                fallback_idx = i,
                                "fallback provider succeeded"
                            );
                            // Tag in a range disjoint from any primary index
                            // so resolution can tell a fallback win from the
                            // primary's own index.
                            resp.provider_index = Some(FALLBACK_INDEX_BASE + i);
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
            // Pass the primary's stream through unchanged so its own
            // ProviderIndex (a nested container's real winner) survives.
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
                for (i, fb) in self.fallbacks.iter().enumerate() {
                    // #2135 round-7 P1: same fit guard as the chat path.
                    if !crate::context::route_fits_request(fb, messages, tools).await {
                        warn!(
                            fallback = fb.model_id(),
                            window = fb.context_window(),
                            "skipping fallback: prompt does not fit its context window"
                        );
                        continue;
                    }
                    match fb.chat_stream(messages, tools, config).await {
                        Ok(stream) => {
                            return Ok(
                                self.stream_with_provider_index(FALLBACK_INDEX_BASE + i, stream)
                            );
                        }
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

    fn provider_metadata(&self) -> ProviderMetadata {
        // #2194 R5: slot 0 (primary) is the identity/default.
        self.primary.provider_metadata()
    }

    fn provider_metadata_for_index(&self, provider_index: Option<usize>) -> ProviderMetadata {
        match provider_index {
            // A fallback answered (tagged FALLBACK_INDEX_BASE + j): resolve to
            // that fallback. KNOWN LIMITATION (tracked): if the fallback is
            // itself a mixed-lane container, its exact answering slot is not
            // resolved here — its default metadata is used.
            Some(i) if i >= FALLBACK_INDEX_BASE => self
                .fallbacks
                .get(i - FALLBACK_INDEX_BASE)
                .map(|fb| fb.provider_metadata())
                .unwrap_or_else(|| self.primary.provider_metadata()),
            // The primary answered: the index is the primary's OWN (preserved),
            // so DELEGATE — a mixed-lane container primary then resolves the
            // exact answering slot's cache lane rather than its default.
            Some(i) => self.primary.provider_metadata_for_index(Some(i)),
            None => self.primary.provider_metadata(),
        }
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

    #[test]
    fn fallback_propagates_lane_and_identity_by_index() {
        // #2194 R5: FallbackProvider wraps [primary, fallbacks...]; pricing must
        // see the ANSWERING provider's cache lane, not the default Residual, and
        // the primary's identity must survive for adaptive-lane matching.
        let primary: Arc<dyn LlmProvider> = Arc::new(
            crate::anthropic::AnthropicProvider::new("k", "claude-3-5-sonnet")
                .with_provider_label("custom"),
        );
        let fallback: Arc<dyn LlmProvider> = Arc::new(
            crate::openai::OpenAIProvider::new("k", "gpt-4o").with_provider_label("openai"),
        );
        let fp = FallbackProvider::new(primary, vec![fallback]);

        let meta = fp.provider_metadata();
        assert_eq!(meta.provider, "custom", "identity is the primary's label");
        assert_eq!(
            meta.cache_lane,
            crate::CacheLane::Anthropic,
            "primary's Anthropic lane, not the default Residual",
        );
        // Primary-range indices (incl. a nested container primary's own
        // winner) and None resolve THROUGH the primary.
        assert_eq!(
            fp.provider_metadata_for_index(Some(0)).cache_lane,
            crate::CacheLane::Anthropic,
        );
        assert_eq!(
            fp.provider_metadata_for_index(Some(1)).cache_lane,
            crate::CacheLane::Anthropic,
            "index 1 is a primary-range index, delegated to the primary",
        );
        assert_eq!(
            fp.provider_metadata_for_index(None).cache_lane,
            crate::CacheLane::Anthropic,
        );
        // A fallback win is tagged FALLBACK_INDEX_BASE + j.
        assert_eq!(
            fp.provider_metadata_for_index(Some(super::FALLBACK_INDEX_BASE))
                .cache_lane,
            crate::CacheLane::Residual,
            "fallback slot 0 (OpenAI) -> residual lane",
        );
        // Out-of-range fallback tag falls back to the primary, never a panic.
        assert_eq!(
            fp.provider_metadata_for_index(Some(super::FALLBACK_INDEX_BASE + 99))
                .cache_lane,
            crate::CacheLane::Anthropic,
        );
    }

    // KNOWN LIMITATION (#2199): a flat provider_index does not compose when a
    // FallbackProvider is the PRIMARY of another FallbackProvider. The inner's
    // fallback tag (FALLBACK_INDEX_BASE + j) is misread by the outer as its own
    // fallback index, so resolution routes to outer.fallbacks[j] instead of the
    // inner's answering fallback. Ignored until the durable answering-metadata
    // fix (carry the resolved leaf metadata on the response) lands.
    #[ignore = "flat-index nesting non-compositionality; tracked in #2199"]
    #[test]
    fn nested_fallback_as_primary_resolves_inner_fallback_lane() {
        let inner_primary: Arc<dyn LlmProvider> = Arc::new(
            crate::anthropic::AnthropicProvider::new("k", "claude-3-5-sonnet")
                .with_provider_label("custom"),
        );
        let inner_fallback: Arc<dyn LlmProvider> = Arc::new(
            crate::openai::OpenAIProvider::new("k", "gpt-4o").with_provider_label("openai"),
        );
        let inner: Arc<dyn LlmProvider> =
            Arc::new(FallbackProvider::new(inner_primary, vec![inner_fallback]));
        let outer_fallback: Arc<dyn LlmProvider> = Arc::new(
            crate::anthropic::AnthropicProvider::new("k", "claude-3-opus")
                .with_provider_label("other-anthropic"),
        );
        let outer = FallbackProvider::new(inner, vec![outer_fallback]);
        // The inner's fallback answered -> its tag is Some(FALLBACK_INDEX_BASE).
        // DESIRED: resolve to the inner's OpenAI fallback (residual lane).
        // ACTUAL (bug): resolves to outer.fallbacks[0] (Anthropic).
        assert_eq!(
            outer
                .provider_metadata_for_index(Some(super::FALLBACK_INDEX_BASE))
                .cache_lane,
            crate::CacheLane::Residual,
        );
    }

    #[tokio::test]
    async fn primary_winner_preserves_child_attribution() {
        // #2194 R6: a leaf primary reports no index; FallbackProvider must NOT
        // clobber it, so a nested container primary's real winner survives and
        // pricing resolves through the primary.
        let primary = CountingProvider::ok();
        let fallback = CountingProvider::always_err_500();
        let fp = FallbackProvider::new(Arc::new(primary), vec![Arc::new(fallback)]);
        let result = fp.chat(&[], &[], &ChatConfig::default()).await.unwrap();
        assert_eq!(
            result.provider_index, None,
            "leaf primary's own attribution (None) is preserved, not clobbered",
        );
    }

    #[tokio::test]
    async fn fallback_winner_is_attributed_with_its_slot_index() {
        use crate::{LlmCallPolicy, with_llm_call_policy};
        // #2194 R5: primary fails, fallback answers -> the response must carry
        // the fallback's slot index (1) so pricing resolves the fallback's lane.
        let primary = CountingProvider::always_err_500();
        let fallback = CountingProvider::ok();
        let fp = FallbackProvider::new(Arc::new(primary), vec![Arc::new(fallback)]);
        let result = with_llm_call_policy(LlmCallPolicy::Normal, async {
            fp.chat(&[], &[], &ChatConfig::default()).await
        })
        .await
        .expect("fallback answers under Normal policy");
        assert_eq!(
            result.provider_index,
            Some(super::FALLBACK_INDEX_BASE),
            "fallback slot 0 answered -> tagged FALLBACK_INDEX_BASE + 0",
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
