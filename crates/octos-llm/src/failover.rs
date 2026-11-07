//! Provider failover chain with circuit breaker.
//!
//! Wraps multiple LLM providers and transparently fails over to the next
//! when one returns a retriable error (429, 5xx, connection failure).
//! Each provider has a circuit breaker that degrades after repeated failures
//! and resets on success.

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use eyre::Result;
use octos_core::Message;
use tracing::{info, warn};

use crate::config::ChatConfig;
use crate::error::LlmError;
use crate::provider::{
    LANE_FAILED_FAIL_FAST, LANE_FAILED_NOT_FAILOVER_WORTHY, LANES_EXHAUSTED, LaneFailure,
    LlmProvider, attribute_lane_failures,
};
use crate::retry::RetryProvider;
#[cfg(test)]
use crate::types::StreamEvent;
use crate::types::{ChatResponse, ChatStream, ProviderMetadata, ToolSpec};
#[cfg(test)]
use futures::StreamExt;

/// Circuit breaker state for a single provider.
struct ProviderSlot {
    provider: Arc<dyn LlmProvider>,
    failures: AtomicU32,
}

/// Multi-provider failover chain.
///
/// Tries providers in order, skipping degraded ones (failure count >= threshold).
/// On retriable error, moves to the next provider. On success, resets the
/// provider's failure count.
/// Default per-lane timeout for a single provider attempt (that provider's
/// internal retries included). A lane that exceeds it is recorded as failed
/// and the chain fails over to the next lane.
const DEFAULT_MAX_REQUEST_DURATION: Duration = Duration::from_secs(120);

pub struct ProviderChain {
    slots: Vec<ProviderSlot>,
    /// Number of consecutive failures before a provider is considered degraded.
    failure_threshold: u32,
    /// Index of the last provider that returned a successful response.
    /// Used by `report_late_failure` to penalize the correct provider.
    last_success_index: AtomicU32,
    /// Per-lane wall-clock timeout for a single provider attempt (that
    /// provider's internal retries included). A lane that hangs past it is
    /// recorded as failed — so `pick_start` stops re-selecting it — and the
    /// chain fails over. Total chain time is bounded by
    /// `slots.len()` x this duration.
    max_request_duration: Option<Duration>,
}

impl ProviderChain {
    /// Create a chain from multiple providers.
    ///
    /// Panics if `providers` is empty.
    pub fn new(providers: Vec<Arc<dyn LlmProvider>>) -> Self {
        assert!(
            !providers.is_empty(),
            "ProviderChain requires at least one provider"
        );
        let slots = providers
            .into_iter()
            .map(|p| ProviderSlot {
                provider: p,
                failures: AtomicU32::new(0),
            })
            .collect();
        Self {
            slots,
            failure_threshold: 3,
            last_success_index: AtomicU32::new(0),
            max_request_duration: Some(DEFAULT_MAX_REQUEST_DURATION),
        }
    }

    /// Set the failure threshold for circuit breaking.
    pub fn with_failure_threshold(mut self, threshold: u32) -> Self {
        self.failure_threshold = threshold;
        self
    }

    /// Set the per-lane timeout for a single provider attempt. `None`
    /// disables the cap.
    pub fn with_max_request_duration(mut self, duration: Option<Duration>) -> Self {
        self.max_request_duration = duration;
        self
    }

    /// Find the first non-degraded provider index, or fall back to the one
    /// with the fewest failures if all are degraded.
    fn pick_start(&self) -> usize {
        // Prefer first non-degraded
        for (i, slot) in self.slots.iter().enumerate() {
            if slot.failures.load(Ordering::Relaxed) < self.failure_threshold {
                return i;
            }
        }
        // All degraded: pick the one with fewest failures
        self.slots
            .iter()
            .enumerate()
            .min_by_key(|(_, s)| s.failures.load(Ordering::Relaxed))
            .map(|(i, _)| i)
            .unwrap_or(0)
    }

    fn record_success(&self, index: usize) {
        self.last_success_index
            .store(index as u32, Ordering::Relaxed);
        let prev = self.slots[index].failures.swap(0, Ordering::Relaxed);
        if prev > 0 {
            info!(
                provider = self.slots[index].provider.provider_name(),
                prev_failures = prev,
                "provider recovered, resetting circuit breaker"
            );
        }
    }

    /// Await a single lane's request, capped by `max_request_duration`.
    ///
    /// A lane that exceeds the cap yields a typed `LlmErrorKind::Timeout`
    /// error attributed to that lane. The caller's `Err` arm then treats it
    /// like any other retriable failure: the lane's failure count is
    /// incremented (so `pick_start` stops re-selecting a hung lane) and the
    /// chain fails over to the next lane within the same call.
    async fn with_lane_timeout<T>(
        &self,
        provider_name: &str,
        fut: impl Future<Output = Result<T>>,
    ) -> Result<T> {
        match self.max_request_duration {
            Some(dur) => match tokio::time::timeout(dur, fut).await {
                Ok(result) => result,
                Err(_) => Err(LlmError::timeout(format!(
                    "no response after {:.0}s",
                    dur.as_secs_f64()
                ))
                .with_provider(provider_name)
                .into()),
            },
            None => fut.await,
        }
    }

    async fn chat_inner(
        &self,
        messages: &[Message],
        tools: &[ToolSpec],
        config: &ChatConfig,
    ) -> Result<ChatResponse> {
        let start = self.pick_start();
        let mut failures: Vec<LaneFailure> = Vec::new();
        let mut last_error = None;

        for offset in 0..self.slots.len() {
            let idx = (start + offset) % self.slots.len();
            let slot = &self.slots[idx];

            // Skip degraded providers (unless it's our last resort)
            if offset > 0 && slot.failures.load(Ordering::Relaxed) >= self.failure_threshold {
                continue;
            }

            let result = self
                .with_lane_timeout(slot.provider.provider_name(), async {
                    // #2135 rounds 7-8: an ALTERNATE lane receives the
                    // unchanged request — resolve its readiness and skip it
                    // when the request cannot fit its window. The guard
                    // runs INSIDE the lane timeout so a slow local
                    // readiness counts against the SAME deadline as the
                    // request and cannot delay failover (round-8 P2).
                    // Ok(None) = unfit skip, distinct from a lane failure.
                    if offset > 0
                        && !crate::context::route_fits_request(&slot.provider, messages, tools)
                            .await
                    {
                        return Ok(None);
                    }
                    slot.provider.chat(messages, tools, config).await.map(Some)
                })
                .await;

            match result {
                Ok(None) => {
                    tracing::warn!(
                        provider = slot.provider.provider_name(),
                        window = slot.provider.context_window(),
                        "skipping failover lane: request does not fit its context window"
                    );
                    continue;
                }
                Ok(Some(mut response)) => {
                    self.record_success(idx);
                    response.provider_index = Some(self.flat_index(idx, response.provider_index));
                    return Ok(response);
                }
                Err(e) => {
                    let retryable = RetryProvider::should_failover(&e);
                    self.record_failure(idx);
                    failures.push(LaneFailure::capture(slot.provider.as_ref(), &e));

                    let fail_fast =
                        crate::current_llm_call_policy() == crate::LlmCallPolicy::FailFast;
                    if !fail_fast && retryable && offset + 1 < self.slots.len() {
                        warn!(
                            provider = slot.provider.provider_name(),
                            error = %e,
                            "failing over to next provider"
                        );
                        last_error = Some(e);
                        continue;
                    }
                    let outcome = if fail_fast {
                        LANE_FAILED_FAIL_FAST
                    } else if !retryable {
                        LANE_FAILED_NOT_FAILOVER_WORTHY
                    } else {
                        LANES_EXHAUSTED
                    };
                    return Err(attribute_lane_failures(e, outcome, &failures));
                }
            }
        }

        Err(last_error
            .map(|e| attribute_lane_failures(e, LANES_EXHAUSTED, &failures))
            .unwrap_or_else(|| eyre::eyre!("all providers exhausted")))
    }

    fn record_failure(&self, index: usize) {
        let count = self.slots[index].failures.fetch_add(1, Ordering::Relaxed) + 1;
        let name = self.slots[index].provider.provider_name();
        if count == self.failure_threshold {
            warn!(
                provider = name,
                failures = count,
                "provider degraded (circuit breaker open)"
            );
        }
    }

    fn stream_with_provider_index(&self, idx: usize, stream: ChatStream) -> ChatStream {
        crate::provider::stream_with_lane_offset(self.lane_offset(idx), stream)
    }

    /// Flat leaf-lane bookkeeping (see [`LlmProvider::provider_lane_count`]):
    /// slot `idx` owns `[lane_offset(idx), lane_offset(idx) + lane_count)`.
    fn lane_counts(&self) -> Vec<usize> {
        self.slots
            .iter()
            .map(|slot| slot.provider.provider_lane_count())
            .collect()
    }

    fn lane_offset(&self, idx: usize) -> usize {
        crate::provider::lane_offset_for_slot(&self.lane_counts(), idx)
    }

    fn flat_index(&self, idx: usize, inner: Option<usize>) -> usize {
        self.lane_offset(idx) + inner.unwrap_or(0)
    }
}

#[async_trait]
impl LlmProvider for ProviderChain {
    async fn chat(
        &self,
        messages: &[Message],
        tools: &[ToolSpec],
        config: &ChatConfig,
    ) -> Result<ChatResponse> {
        // The per-lane timeout lives inside `chat_inner` (around each
        // provider await) so a hung lane is attributed via
        // `record_failure(idx)` and the chain can still fail over to a
        // healthy lane within this same call.
        self.chat_inner(messages, tools, config).await
    }

    async fn chat_stream(
        &self,
        messages: &[Message],
        tools: &[ToolSpec],
        config: &ChatConfig,
    ) -> Result<ChatStream> {
        let start = self.pick_start();
        let mut failures: Vec<LaneFailure> = Vec::new();
        let mut last_error = None;

        for offset in 0..self.slots.len() {
            let idx = (start + offset) % self.slots.len();
            let slot = &self.slots[idx];

            if offset > 0 && slot.failures.load(Ordering::Relaxed) >= self.failure_threshold {
                continue;
            }

            // Cap stream *initialization* per lane; consuming the returned
            // stream is unaffected. A hung init is recorded below and the
            // chain fails over like any other retriable error. The fit
            // guard runs INSIDE the cap (#2135 round-8 P2), and Ok(None)
            // means an unfit skip, distinct from a lane failure.
            let result = self
                .with_lane_timeout(slot.provider.provider_name(), async {
                    if offset > 0
                        && !crate::context::route_fits_request(&slot.provider, messages, tools)
                            .await
                    {
                        return Ok(None);
                    }
                    slot.provider
                        .chat_stream(messages, tools, config)
                        .await
                        .map(Some)
                })
                .await;

            match result {
                Ok(None) => {
                    tracing::warn!(
                        provider = slot.provider.provider_name(),
                        window = slot.provider.context_window(),
                        "skipping failover lane: request does not fit its context window"
                    );
                    continue;
                }
                Ok(Some(stream)) => {
                    self.record_success(idx);
                    return Ok(self.stream_with_provider_index(idx, stream));
                }
                Err(e) => {
                    let retryable = RetryProvider::should_failover(&e);
                    self.record_failure(idx);
                    failures.push(LaneFailure::capture(slot.provider.as_ref(), &e));

                    let fail_fast =
                        crate::current_llm_call_policy() == crate::LlmCallPolicy::FailFast;
                    if !fail_fast && retryable && offset + 1 < self.slots.len() {
                        warn!(
                            provider = slot.provider.provider_name(),
                            error = %e,
                            "failing over stream to next provider"
                        );
                        last_error = Some(e);
                        continue;
                    }
                    let outcome = if fail_fast {
                        LANE_FAILED_FAIL_FAST
                    } else if !retryable {
                        LANE_FAILED_NOT_FAILOVER_WORTHY
                    } else {
                        LANES_EXHAUSTED
                    };
                    return Err(attribute_lane_failures(e, outcome, &failures));
                }
            }
        }

        Err(last_error
            .map(|e| attribute_lane_failures(e, LANES_EXHAUSTED, &failures))
            .unwrap_or_else(|| eyre::eyre!("all providers exhausted")))
    }

    // #2135 round-6 P1: the MINIMUM across every slot — the chain fails
    // over with the SAME request, so the prompt must fit the smallest lane
    // it can land on, not just the preferred one.
    fn context_window(&self) -> u32 {
        self.slots
            .iter()
            .map(|slot| slot.provider.context_window())
            .min()
            .unwrap_or(32_768)
    }

    fn max_output_tokens(&self) -> u32 {
        self.slots
            .iter()
            .map(|slot| slot.provider.max_output_tokens())
            .min()
            .unwrap_or(4096)
    }

    async fn ensure_ready(&self) {
        let idx = self.pick_start();
        self.slots[idx].provider.ensure_ready().await;
    }

    fn model_id(&self) -> &str {
        let idx = self.pick_start();
        self.slots[idx].provider.model_id()
    }

    fn provider_name(&self) -> &str {
        let idx = self.pick_start();
        self.slots[idx].provider.provider_name()
    }

    fn provider_metadata(&self) -> ProviderMetadata {
        let idx = self.pick_start();
        self.slots[idx].provider.provider_metadata()
    }

    fn provider_metadata_for_index(&self, provider_index: Option<usize>) -> ProviderMetadata {
        let Some(index) = provider_index else {
            let idx = self.pick_start();
            return self.slots[idx].provider.provider_metadata_for_index(None);
        };
        match crate::provider::slot_for_lane_index(&self.lane_counts(), index) {
            Some((slot, inner)) => self.slots[slot]
                .provider
                .provider_metadata_for_index(Some(inner)),
            None => self.provider_metadata(),
        }
    }

    fn provider_lane_count(&self) -> usize {
        self.lane_counts().iter().sum()
    }

    fn api_style(&self) -> Option<crate::provider::ApiStyle> {
        let idx = self.pick_start();
        self.slots[idx].provider.api_style()
    }

    fn supports_semantic_checkpoint_hints(&self) -> bool {
        self.slots
            .iter()
            .any(|slot| slot.provider.supports_semantic_checkpoint_hints())
    }

    fn report_late_failure(&self) {
        let idx = self.last_success_index.load(Ordering::Relaxed) as usize;
        self.record_failure(idx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::TokenUsage;

    struct SuccessProvider {
        name: &'static str,
    }

    #[async_trait]
    impl LlmProvider for SuccessProvider {
        async fn chat(
            &self,
            _messages: &[Message],
            _tools: &[ToolSpec],
            _config: &ChatConfig,
        ) -> Result<ChatResponse> {
            Ok(ChatResponse {
                content: Some("ok".to_string()),
                reasoning_content: None,
                tool_calls: vec![],
                stop_reason: crate::types::StopReason::EndTurn,
                usage: TokenUsage::default(),
                provider_index: None,
            })
        }

        fn model_id(&self) -> &str {
            "success-model"
        }

        fn provider_name(&self) -> &str {
            self.name
        }
    }

    /// Provider whose `chat()` never resolves — simulates a hung lane
    /// (e.g. a TCP connection that accepts but never responds).
    struct HangingProvider {
        name: &'static str,
    }

    #[async_trait]
    impl LlmProvider for HangingProvider {
        async fn chat(
            &self,
            _messages: &[Message],
            _tools: &[ToolSpec],
            _config: &ChatConfig,
        ) -> Result<ChatResponse> {
            std::future::pending::<()>().await;
            unreachable!("hanging provider never resolves")
        }

        fn model_id(&self) -> &str {
            "hang-model"
        }

        fn provider_name(&self) -> &str {
            self.name
        }
    }

    #[tokio::test]
    async fn test_circuit_breaker_resets_on_success() {
        let chain = ProviderChain::new(vec![
            Arc::new(SuccessProvider { name: "primary" }),
            Arc::new(SuccessProvider { name: "fallback" }),
        ])
        .with_failure_threshold(3);

        // Manually set failures
        chain.slots[0].failures.store(5, Ordering::Relaxed);
        assert_eq!(chain.provider_name(), "fallback");

        // Success on primary resets it
        chain.record_success(0);
        assert_eq!(chain.provider_name(), "primary");
    }

    #[tokio::test]
    async fn should_failover_after_report_late_failure() {
        let chain = ProviderChain::new(vec![
            Arc::new(SuccessProvider { name: "primary" }),
            Arc::new(SuccessProvider { name: "fallback" }),
        ])
        .with_failure_threshold(1);

        // Initially routes to primary
        let resp = chain.chat(&[], &[], &ChatConfig::default()).await.unwrap();
        assert_eq!(resp.content.as_deref(), Some("ok"));
        assert_eq!(chain.provider_name(), "primary");

        // Report late failure degrades primary
        chain.report_late_failure();
        assert_eq!(
            chain.slots[0].failures.load(Ordering::Relaxed),
            1,
            "late failure should increment failure count"
        );

        // Now should route to fallback (primary is degraded)
        assert_eq!(chain.provider_name(), "fallback");
    }

    #[tokio::test]
    async fn should_failover_to_healthy_lane_when_chat_hangs() {
        let chain = ProviderChain::new(vec![
            Arc::new(HangingProvider { name: "hung" }),
            Arc::new(SuccessProvider { name: "fallback" }),
        ])
        .with_max_request_duration(Some(Duration::from_millis(50)));

        let result = chain
            .chat(&[], &[], &ChatConfig::default())
            .await
            .expect("chain must fail over past the hung lane and succeed");
        assert_eq!(result.content.as_deref(), Some("ok"));
        assert_eq!(result.provider_index, Some(1));
        assert_eq!(
            chain.slots[0].failures.load(Ordering::Relaxed),
            1,
            "hung lane must be recorded as failed"
        );
    }

    #[tokio::test]
    async fn should_failover_stream_init_when_chat_stream_hangs() {
        let chain = ProviderChain::new(vec![
            Arc::new(HangingProvider { name: "hung" }),
            Arc::new(SuccessProvider { name: "fallback" }),
        ])
        .with_max_request_duration(Some(Duration::from_millis(50)));

        // Guard with a generous outer timeout so a regression fails the
        // test instead of hanging the suite forever.
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            chain.chat_stream(&[], &[], &ChatConfig::default()),
        )
        .await
        .expect("chat_stream must not hang when a lane hangs");

        let mut stream = result.expect("stream must fail over to the healthy lane");
        assert_eq!(
            chain.slots[0].failures.load(Ordering::Relaxed),
            1,
            "hung stream init must be recorded as failed"
        );
        match stream.next().await {
            Some(StreamEvent::ProviderIndex(idx)) => assert_eq!(idx, 1),
            other => panic!("expected ProviderIndex(1) first, got {other:?}"),
        }
    }
}
