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
use futures::StreamExt;
use octos_core::Message;
use tracing::{info, warn};

use crate::config::ChatConfig;
use crate::error::LlmError;
use crate::provider::LlmProvider;
use crate::retry::RetryProvider;
use crate::types::{ChatResponse, ChatStream, ProviderMetadata, StreamEvent, ToolSpec};

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
        let mut last_error = None;

        for offset in 0..self.slots.len() {
            let idx = (start + offset) % self.slots.len();
            let slot = &self.slots[idx];

            // Skip degraded providers (unless it's our last resort)
            if offset > 0 && slot.failures.load(Ordering::Relaxed) >= self.failure_threshold {
                continue;
            }

            let result = self
                .with_lane_timeout(
                    slot.provider.provider_name(),
                    slot.provider.chat(messages, tools, config),
                )
                .await;

            match result {
                Ok(mut response) => {
                    self.record_success(idx);
                    response.provider_index = Some(idx);
                    return Ok(response);
                }
                Err(e) => {
                    let retryable = RetryProvider::should_failover(&e);
                    self.record_failure(idx);

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
                    return Err(e);
                }
            }
        }

        Err(last_error.unwrap_or_else(|| eyre::eyre!("all providers exhausted")))
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
        Box::pin(
            futures::stream::once(async move { StreamEvent::ProviderIndex(idx) }).chain(stream),
        )
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
        let mut last_error = None;

        for offset in 0..self.slots.len() {
            let idx = (start + offset) % self.slots.len();
            let slot = &self.slots[idx];

            if offset > 0 && slot.failures.load(Ordering::Relaxed) >= self.failure_threshold {
                continue;
            }

            // Cap stream *initialization* per lane; consuming the returned
            // stream is unaffected. A hung init is recorded below and the
            // chain fails over like any other retriable error.
            let result = self
                .with_lane_timeout(
                    slot.provider.provider_name(),
                    slot.provider.chat_stream(messages, tools, config),
                )
                .await;

            match result {
                Ok(stream) => {
                    self.record_success(idx);
                    return Ok(self.stream_with_provider_index(idx, stream));
                }
                Err(e) => {
                    let retryable = RetryProvider::should_failover(&e);
                    self.record_failure(idx);

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
                    return Err(e);
                }
            }
        }

        Err(last_error.unwrap_or_else(|| eyre::eyre!("all providers exhausted")))
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
        let idx = provider_index.unwrap_or_else(|| self.pick_start());
        self.slots
            .get(idx)
            .map(|slot| slot.provider.provider_metadata())
            .unwrap_or_else(|| self.provider_metadata())
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

    struct FailingProvider {
        name: &'static str,
        error: &'static str,
    }

    #[async_trait]
    impl LlmProvider for FailingProvider {
        async fn chat(
            &self,
            _messages: &[Message],
            _tools: &[ToolSpec],
            _config: &ChatConfig,
        ) -> Result<ChatResponse> {
            eyre::bail!("{} API error: 429 - rate limited", self.error)
        }

        fn model_id(&self) -> &str {
            "fail-model"
        }

        fn provider_name(&self) -> &str {
            self.name
        }
    }

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
    async fn test_failover_to_second_provider() {
        let chain = ProviderChain::new(vec![
            Arc::new(FailingProvider {
                name: "primary",
                error: "Primary",
            }),
            Arc::new(SuccessProvider { name: "fallback" }),
        ]);

        let result = chain.chat(&[], &[], &ChatConfig::default()).await.unwrap();
        assert_eq!(result.content.unwrap(), "ok");
    }

    #[tokio::test]
    async fn test_primary_succeeds_no_failover() {
        let chain = ProviderChain::new(vec![
            Arc::new(SuccessProvider { name: "primary" }),
            Arc::new(FailingProvider {
                name: "fallback",
                error: "Fallback",
            }),
        ]);

        let result = chain.chat(&[], &[], &ChatConfig::default()).await.unwrap();
        assert_eq!(result.content.unwrap(), "ok");
    }

    #[tokio::test]
    async fn test_all_providers_fail() {
        let chain = ProviderChain::new(vec![
            Arc::new(FailingProvider {
                name: "p1",
                error: "P1",
            }),
            Arc::new(FailingProvider {
                name: "p2",
                error: "P2",
            }),
        ]);

        let result = chain.chat(&[], &[], &ChatConfig::default()).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_circuit_breaker_degrades_provider() {
        let chain = ProviderChain::new(vec![
            Arc::new(FailingProvider {
                name: "primary",
                error: "Primary",
            }),
            Arc::new(SuccessProvider { name: "fallback" }),
        ])
        .with_failure_threshold(2);

        // Two failures should degrade primary
        let _ = chain.chat(&[], &[], &ChatConfig::default()).await;
        let _ = chain.chat(&[], &[], &ChatConfig::default()).await;

        // Third call should start from fallback (pick_start skips degraded)
        assert_eq!(chain.provider_name(), "fallback");
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

    #[test]
    #[should_panic(expected = "at least one provider")]
    fn test_empty_chain_panics() {
        let _ = ProviderChain::new(vec![]);
    }

    #[tokio::test]
    async fn should_not_switch_lane_when_failfast() {
        use crate::{LlmCallPolicy, with_llm_call_policy};

        // Primary fails with a failover-eligible 500; secondary would succeed.
        // Under FailFast the chain must return the primary error immediately
        // without calling the secondary provider.
        let secondary = SuccessProvider { name: "secondary" };
        // We can't count calls on SuccessProvider directly, so we use a
        // FailingProvider that would fail if called and check the error kind.
        let chain = ProviderChain::new(vec![
            Arc::new(FailingProvider {
                name: "primary",
                error: "P1 API error: 500 - server error",
            }),
            Arc::new(secondary),
        ])
        .with_max_request_duration(None);

        let result = with_llm_call_policy(LlmCallPolicy::FailFast, async {
            chain.chat(&[], &[], &ChatConfig::default()).await
        })
        .await;

        // Must fail (primary failed, no failover), and the error must come
        // from the primary (failure count on secondary slot stays 0).
        assert!(
            result.is_err(),
            "FailFast should not switch to secondary lane"
        );
        assert_eq!(
            chain.slots[1].failures.load(Ordering::Relaxed),
            0,
            "secondary slot must not be called under FailFast"
        );
    }

    #[tokio::test]
    async fn should_not_switch_lane_stream_when_failfast() {
        use crate::{LlmCallPolicy, with_llm_call_policy};

        struct FailingStreamProvider500 {
            name: &'static str,
        }

        #[async_trait]
        impl LlmProvider for FailingStreamProvider500 {
            async fn chat(
                &self,
                _messages: &[Message],
                _tools: &[ToolSpec],
                _config: &ChatConfig,
            ) -> Result<ChatResponse> {
                eyre::bail!("{} API error: 500 - server error", self.name)
            }

            async fn chat_stream(
                &self,
                _messages: &[Message],
                _tools: &[ToolSpec],
                _config: &ChatConfig,
            ) -> Result<ChatStream> {
                eyre::bail!("{} API error: 500 - server error", self.name)
            }

            fn model_id(&self) -> &str {
                "fail-stream-model"
            }

            fn provider_name(&self) -> &str {
                self.name
            }
        }

        let chain = ProviderChain::new(vec![
            Arc::new(FailingStreamProvider500 { name: "primary" }),
            Arc::new(SuccessProvider { name: "secondary" }),
        ])
        .with_max_request_duration(None);

        let result = with_llm_call_policy(LlmCallPolicy::FailFast, async {
            chain.chat_stream(&[], &[], &ChatConfig::default()).await
        })
        .await;

        assert!(
            result.is_err(),
            "FailFast stream should not switch to secondary lane"
        );
        assert_eq!(
            chain.slots[1].failures.load(Ordering::Relaxed),
            0,
            "secondary slot must not be called under FailFast stream"
        );
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
    async fn should_skip_hung_lane_when_failure_threshold_crossed() {
        let chain = ProviderChain::new(vec![
            Arc::new(HangingProvider { name: "hung" }),
            Arc::new(SuccessProvider { name: "fallback" }),
        ])
        .with_failure_threshold(2)
        .with_max_request_duration(Some(Duration::from_millis(50)));

        // Two hangs cross the threshold and degrade the lane.
        let _ = chain.chat(&[], &[], &ChatConfig::default()).await;
        let _ = chain.chat(&[], &[], &ChatConfig::default()).await;
        assert!(
            chain.slots[0].failures.load(Ordering::Relaxed) >= 2,
            "each hang must increment the lane's failure count"
        );

        // pick_start must now skip the hung lane entirely.
        assert_eq!(chain.provider_name(), "fallback");

        // The next call goes straight to the healthy lane: no new timeout
        // failure is recorded on the hung lane.
        let before = chain.slots[0].failures.load(Ordering::Relaxed);
        let result = chain
            .chat(&[], &[], &ChatConfig::default())
            .await
            .expect("degraded lane must be skipped, healthy lane succeeds");
        assert_eq!(result.provider_index, Some(1));
        assert_eq!(
            chain.slots[0].failures.load(Ordering::Relaxed),
            before,
            "degraded lane must not be re-awaited"
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

    /// #2135 round-6 P1: the chain fails over with the same request, so
    /// the reported window is the minimum across slots.
    #[test]
    fn should_report_minimum_window_across_slots() {
        let big: Arc<dyn LlmProvider> = Arc::new(crate::ContextWindowOverride::new(
            Arc::new(SuccessProvider { name: "big" }),
            262_144,
        ));
        let small: Arc<dyn LlmProvider> = Arc::new(crate::ContextWindowOverride::new(
            Arc::new(SuccessProvider { name: "small" }),
            32_768,
        ));
        let chain = ProviderChain::new(vec![big, small]);
        assert_eq!(chain.context_window(), 32_768);
    }
}
