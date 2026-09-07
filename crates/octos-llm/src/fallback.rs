//! Fallback provider — wraps a primary provider with QoS-ranked fallbacks
//! and cooldown-based failure exclusion.

use std::sync::Arc;

use async_trait::async_trait;
use eyre::Result;
use octos_core::Message;
use tracing::warn;

use crate::config::ChatConfig;
use crate::provider::{
    LANE_FAILED_FAIL_FAST, LANE_FAILED_NOT_FAILOVER_WORTHY, LANES_EXHAUSTED, LaneFailure,
    LlmProvider, attribute_lane_failures,
};
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

    /// Slot numbering shared by `chat`, `chat_stream`, and
    /// [`LlmProvider::provider_metadata_for_index`]: slot 0 is the primary,
    /// slot `i + 1` is `fallbacks[i]`. Response indices are FLAT leaf-lane
    /// indices: slot `s` owns `[lane_offset(s), lane_offset(s) + lane_count(s))`,
    /// so a nested composite's own index is carried inside the slot's range.
    fn slot_provider_ref(&self, slot: usize) -> &Arc<dyn LlmProvider> {
        match slot {
            0 => &self.primary,
            index => self.fallbacks.get(index - 1).unwrap_or(&self.primary),
        }
    }

    fn lane_counts(&self) -> Vec<usize> {
        std::iter::once(self.primary.provider_lane_count())
            .chain(self.fallbacks.iter().map(|fb| fb.provider_lane_count()))
            .collect()
    }

    fn lane_offset(&self, slot: usize) -> usize {
        crate::provider::lane_offset_for_slot(&self.lane_counts(), slot)
    }

    /// Flat index of the lane that served a response from slot `slot`,
    /// honoring an index a nested composite already tagged.
    fn flat_index(&self, slot: usize, inner: Option<usize>) -> usize {
        self.lane_offset(slot) + inner.unwrap_or(0)
    }

    /// Prefix the stream with the serving slot's flat lane offset, mirroring
    /// `ProviderChain`, so post-stream cache-usage attribution resolves the
    /// lane that answered — through a nested composite as well.
    fn stream_with_provider_index(&self, slot: usize, stream: ChatStream) -> ChatStream {
        crate::provider::stream_with_lane_offset(self.lane_offset(slot), stream)
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
            Ok(mut resp) => {
                resp.provider_index = Some(self.flat_index(0, resp.provider_index));
                Ok(resp)
            }
            Err(primary_err) => {
                let mut failures = vec![LaneFailure::capture(self.primary.as_ref(), &primary_err)];
                if crate::current_llm_call_policy() == crate::LlmCallPolicy::FailFast {
                    return Err(attribute_lane_failures(
                        primary_err,
                        LANE_FAILED_FAIL_FAST,
                        &failures,
                    ));
                }
                if !RetryProvider::should_failover(&primary_err) {
                    return Err(attribute_lane_failures(
                        primary_err,
                        LANE_FAILED_NOT_FAILOVER_WORTHY,
                        &failures,
                    ));
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
                            resp.provider_index = Some(self.flat_index(i + 1, resp.provider_index));
                            return Ok(resp);
                        }
                        Err(e) => {
                            self.record_failure(fb.model_id());
                            warn!(
                                fallback = fb.model_id(),
                                error = %e,
                                "fallback provider also failed"
                            );
                            failures.push(LaneFailure::capture(fb.as_ref(), &e));
                        }
                    }
                }
                // The primary's typed error stays the carrier (unchanged
                // classification for outer wrappers); the summary names
                // every lane that failed.
                Err(attribute_lane_failures(
                    primary_err,
                    LANES_EXHAUSTED,
                    &failures,
                ))
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
            Ok(stream) => Ok(self.stream_with_provider_index(0, stream)),
            Err(primary_err) => {
                let mut failures = vec![LaneFailure::capture(self.primary.as_ref(), &primary_err)];
                if crate::current_llm_call_policy() == crate::LlmCallPolicy::FailFast {
                    return Err(attribute_lane_failures(
                        primary_err,
                        LANE_FAILED_FAIL_FAST,
                        &failures,
                    ));
                }
                if !RetryProvider::should_failover(&primary_err) {
                    return Err(attribute_lane_failures(
                        primary_err,
                        LANE_FAILED_NOT_FAILOVER_WORTHY,
                        &failures,
                    ));
                }
                self.record_failure(self.primary.model_id());
                warn!(
                    primary = self.primary.model_id(),
                    error = %primary_err,
                    "primary stream failed, trying fallbacks"
                );
                for (i, fb) in self.fallbacks.iter().enumerate() {
                    // #2135: fallback still has to fit the complete request.
                    if !crate::context::route_fits_request(fb, messages, tools).await {
                        warn!(
                            fallback = fb.model_id(),
                            window = fb.context_window(),
                            "skipping fallback: prompt does not fit its context window"
                        );
                        continue;
                    }
                    match fb.chat_stream(messages, tools, config).await {
                        Ok(stream) => return Ok(self.stream_with_provider_index(i + 1, stream)),
                        Err(e) => {
                            self.record_failure(fb.model_id());
                            warn!(fallback = fb.model_id(), error = %e, "fallback stream also failed");
                            failures.push(LaneFailure::capture(fb.as_ref(), &e));
                        }
                    }
                }
                Err(attribute_lane_failures(
                    primary_err,
                    LANES_EXHAUSTED,
                    &failures,
                ))
            }
        }
    }

    fn model_id(&self) -> &str {
        self.primary.model_id()
    }

    fn provider_name(&self) -> &str {
        self.primary.provider_name()
    }

    /// The composite keeps identifying as its primary (backward compatible);
    /// use [`Self::provider_metadata_for_index`] with the response's
    /// `provider_index` to attribute a fallback-served response.
    fn provider_metadata(&self) -> crate::types::ProviderMetadata {
        self.primary.provider_metadata()
    }

    fn provider_metadata_for_index(
        &self,
        provider_index: Option<usize>,
    ) -> crate::types::ProviderMetadata {
        match provider_index {
            None => self.primary.provider_metadata_for_index(None),
            Some(index) => match crate::provider::slot_for_lane_index(&self.lane_counts(), index) {
                Some((slot, inner)) => self
                    .slot_provider_ref(slot)
                    .provider_metadata_for_index(Some(inner)),
                None => self.primary.provider_metadata(),
            },
        }
    }

    fn provider_lane_count(&self) -> usize {
        self.lane_counts().iter().sum()
    }

    fn api_style(&self) -> Option<crate::provider::ApiStyle> {
        self.primary.api_style()
    }

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

    fn supports_semantic_checkpoint_hints(&self) -> bool {
        self.primary.supports_semantic_checkpoint_hints()
            || self
                .fallbacks
                .iter()
                .any(|provider| provider.supports_semantic_checkpoint_hints())
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
        // Flat leaf lane 0 and an unspecified route resolve to the primary.
        assert_eq!(
            fp.provider_metadata_for_index(Some(0)).cache_lane,
            crate::CacheLane::Anthropic,
        );
        assert_eq!(
            fp.provider_metadata_for_index(Some(1)).cache_lane,
            crate::CacheLane::Residual,
            "the primary owns one leaf; index 1 belongs to the fallback",
        );
        assert_eq!(
            fp.provider_metadata_for_index(None).cache_lane,
            crate::CacheLane::Anthropic,
        );
        // A fallback win follows the primary's flat leaf range.
        assert_eq!(
            fp.provider_metadata_for_index(Some(1)).cache_lane,
            crate::CacheLane::Residual,
            "fallback slot 0 (OpenAI) -> residual lane",
        );
        // Out-of-range fallback tag falls back to the primary, never a panic.
        assert_eq!(
            fp.provider_metadata_for_index(Some(100)).cache_lane,
            crate::CacheLane::Anthropic,
        );
    }

    // #2199 is now covered without an ignore: each composite reserves the
    // complete flat leaf range of its primary before its own fallback range.
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
        // The inner's fallback answered, flat leaf 1; the OUTER's fallback
        // owns leaf 2 and must not steal this attribution.
        assert_eq!(
            outer.provider_metadata_for_index(Some(1)).cache_lane,
            crate::CacheLane::Residual,
        );
        assert_eq!(
            outer.provider_metadata_for_index(Some(2)).cache_lane,
            crate::CacheLane::Anthropic
        );
        assert_eq!(outer.provider_lane_count(), 3);
    }

    #[tokio::test]
    async fn primary_winner_preserves_child_attribution() {
        // A primary leaf becomes flat index 0. A nested primary's indices
        // retain their offsets, so the answering child's pricing survives.
        let primary = CountingProvider::ok();
        let fallback = CountingProvider::always_err_500();
        let fp = FallbackProvider::new(Arc::new(primary), vec![Arc::new(fallback)]);
        let result = fp.chat(&[], &[], &ChatConfig::default()).await.unwrap();
        assert_eq!(
            result.provider_index,
            Some(0),
            "the primary leaf occupies flat lane zero",
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
            Some(1),
            "first fallback follows the primary's one-leaf range",
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

#[cfg(test)]
mod provider_index_tests {
    use std::sync::Arc;

    use futures::StreamExt;

    use super::FallbackProvider;
    use crate::config::ChatConfig;
    use crate::provider::LlmProvider;
    use crate::provider::test_lanes::StubLane;
    use crate::types::StreamEvent;
    use crate::{LlmCallPolicy, with_llm_call_policy};

    fn chain_with_failing_primary() -> FallbackProvider {
        FallbackProvider::new(
            Arc::new(StubLane::failing("primary", "model-p")),
            vec![Arc::new(StubLane::ok("secondary", "model-s"))],
        )
    }

    /// Nested composition: a `ProviderChain` in the primary slot. The flat
    /// leaf-lane index must resolve the chain's serving lane (lane 2 =
    /// `chain-c`), not the fallback slot that shares the number in a flat
    /// slot numbering, on the chat AND the stream path.
    #[tokio::test]
    async fn should_resolve_serving_lane_when_fallback_wraps_provider_chain() {
        let chain = crate::ProviderChain::new(vec![
            Arc::new(StubLane::failing("chain-a", "model-a")),
            Arc::new(StubLane::failing("chain-b", "model-b")),
            Arc::new(StubLane::ok("chain-c", "model-c")),
        ]);
        let composite = FallbackProvider::new(
            Arc::new(chain),
            vec![Arc::new(StubLane::ok("secondary", "model-s"))],
        );
        assert_eq!(composite.provider_lane_count(), 4);

        let response = composite
            .chat(&[], &[], &ChatConfig::default())
            .await
            .expect("chain lane c serves");
        assert_eq!(response.provider_index, Some(2));
        assert_eq!(
            composite
                .provider_metadata_for_index(response.provider_index)
                .provider,
            "chain-c"
        );
        assert_eq!(
            composite.provider_metadata_for_index(Some(3)).provider,
            "secondary",
            "the fallback slot owns the lane after the chain's three lanes"
        );

        let mut stream = composite
            .chat_stream(&[], &[], &ChatConfig::default())
            .await
            .expect("chain lane c streams");
        let mut last_index = None;
        while let Some(event) = stream.next().await {
            if let StreamEvent::ProviderIndex(index) = event {
                last_index = Some(index);
            }
        }
        assert_eq!(
            last_index,
            Some(2),
            "nested stream indices are translated into the flat space"
        );
        assert_eq!(
            composite.provider_metadata_for_index(last_index).provider,
            "chain-c"
        );
    }

    #[tokio::test]
    async fn should_tag_provider_index_one_and_resolve_fallback_identity_when_fallback_serves_chat()
    {
        let chain = chain_with_failing_primary();
        let response = with_llm_call_policy(LlmCallPolicy::Normal, async {
            chain.chat(&[], &[], &ChatConfig::default()).await
        })
        .await
        .expect("fallback lane serves the request");

        assert_eq!(
            response.provider_index,
            Some(1),
            "fallback-served responses must be attributable to slot 1"
        );
        let metadata = chain.provider_metadata_for_index(response.provider_index);
        assert_eq!(
            (metadata.provider.as_str(), metadata.model.as_str()),
            ("secondary", "model-s"),
            "slot 1 must resolve to the fallback lane: {metadata:?}"
        );
        // Backward compatibility: the composite still identifies as its primary.
        assert_eq!(chain.provider_metadata().provider, "primary");
        assert_eq!(
            chain.provider_metadata_for_index(Some(0)).provider,
            "primary"
        );
        assert_eq!(chain.provider_metadata_for_index(None).provider, "primary");
    }

    #[tokio::test]
    async fn should_tag_provider_index_zero_when_primary_serves_chat() {
        let chain = FallbackProvider::new(
            Arc::new(StubLane::ok("primary", "model-p")),
            vec![Arc::new(StubLane::ok("secondary", "model-s"))],
        );
        let response = with_llm_call_policy(LlmCallPolicy::Normal, async {
            chain.chat(&[], &[], &ChatConfig::default()).await
        })
        .await
        .unwrap();
        assert_eq!(response.provider_index, Some(0));
        assert_eq!(
            chain
                .provider_metadata_for_index(response.provider_index)
                .provider,
            "primary"
        );
    }

    #[tokio::test]
    async fn should_prepend_provider_index_event_when_fallback_serves_stream() {
        let chain = chain_with_failing_primary();
        let mut stream = with_llm_call_policy(LlmCallPolicy::Normal, async {
            chain.chat_stream(&[], &[], &ChatConfig::default()).await
        })
        .await
        .expect("fallback lane serves the stream");
        match stream.next().await {
            Some(StreamEvent::ProviderIndex(index)) => assert_eq!(index, 1),
            other => panic!("expected ProviderIndex(1) first, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn should_prepend_provider_index_zero_when_primary_serves_stream() {
        let chain = FallbackProvider::new(
            Arc::new(StubLane::ok("primary", "model-p")),
            vec![Arc::new(StubLane::ok("secondary", "model-s"))],
        );
        let mut stream = chain
            .chat_stream(&[], &[], &ChatConfig::default())
            .await
            .unwrap();
        match stream.next().await {
            Some(StreamEvent::ProviderIndex(index)) => assert_eq!(index, 0),
            other => panic!("expected ProviderIndex(0) first, got {other:?}"),
        }
    }
}

#[cfg(test)]
mod lane_attribution_tests {
    use std::sync::Arc;

    use octos_core::Message;
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::FallbackProvider;
    use crate::anthropic::AnthropicProvider;
    use crate::config::ChatConfig;
    use crate::openai::OpenAIProvider;
    use crate::provider::LlmProvider;
    use crate::retry::RetryProvider;
    use crate::{LlmCallPolicy, with_llm_call_policy};

    /// A loopback URL with nothing listening (connection refused).
    async fn refused_url() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        format!("http://127.0.0.1:{port}")
    }

    #[tokio::test]
    async fn should_name_every_failed_lane_with_api_style_when_k3_fails_over_to_anthropic_compatible_lane()
     {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(500).set_body_string("k3 upstream exploded"))
            .mount(&server)
            .await;
        let k3: Arc<dyn LlmProvider> = Arc::new(
            OpenAIProvider::new("key", "k3")
                .with_base_url(server.uri())
                .with_provider_label("moonshot-coding@api"),
        );
        let zai: Arc<dyn LlmProvider> = Arc::new(
            AnthropicProvider::new("key", "glm-5.3")
                .with_base_url(refused_url().await)
                .with_provider_label("zai-coding"),
        );
        let chain = FallbackProvider::new(k3, vec![zai]);

        let result = with_llm_call_policy(LlmCallPolicy::Normal, async {
            chain
                .chat_stream(&[Message::user("hi")], &[], &ChatConfig::default())
                .await
        })
        .await;
        let Err(err) = result else {
            panic!("both lanes fail")
        };

        let display = err.to_string();
        let alternate = format!("{err:#}");
        for rendered in [&display, &alternate] {
            for needle in [
                "moonshot-coding@api",
                "k3",
                "zai-coding",
                "glm-5.3",
                "api_style=anthropic_messages",
                "api_style=openai_chat_completions",
            ] {
                assert!(
                    rendered.contains(needle),
                    "missing {needle:?} in: {rendered}"
                );
            }
            assert!(
                !rendered.contains("request to Anthropic"),
                "a lane the user never configured must not be named: {rendered}"
            );
        }
        assert!(
            RetryProvider::should_failover(&err),
            "wrapping must keep the typed lane error classifiable: {alternate}"
        );
    }
}
