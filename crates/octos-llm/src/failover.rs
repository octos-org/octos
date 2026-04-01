//! Provider failover chain with circuit breaker.
//!
//! Wraps multiple LLM providers and transparently fails over to the next
//! when one returns a retriable error (429, 5xx, connection failure).
//! Each provider has a circuit breaker that degrades after repeated failures
//! and resets on success.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use eyre::Result;
use octos_core::Message;
use tracing::{info, warn};

use crate::config::ChatConfig;
use crate::provider::LlmProvider;
use crate::retry::RetryProvider;
use crate::types::{ChatResponse, ChatStream, ToolSpec};

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
/// Default wall-clock timeout for an entire `chat()` call across all providers.
const DEFAULT_MAX_REQUEST_DURATION: Duration = Duration::from_secs(120);

pub struct ProviderChain {
    slots: Vec<ProviderSlot>,
    /// Number of consecutive failures before a provider is considered degraded.
    failure_threshold: u32,
    /// Index of the last provider that returned a successful response.
    /// Used by `report_late_failure` to penalize the correct provider.
    last_success_index: AtomicU32,
    /// Wall-clock timeout for the entire chain (retry + failover included).
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

    /// Set the wall-clock timeout for the entire chain. `None` disables the cap.
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

            match slot.provider.chat(messages, tools, config).await {
                Ok(mut response) => {
                    self.record_success(idx);
                    response.provider_index = Some(idx);
                    return Ok(response);
                }
                Err(e) => {
                    let retryable = RetryProvider::should_failover(&e);
                    self.record_failure(idx);

                    if retryable && offset + 1 < self.slots.len() {
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
}

#[async_trait]
impl LlmProvider for ProviderChain {
    async fn chat(
        &self,
        messages: &[Message],
        tools: &[ToolSpec],
        config: &ChatConfig,
    ) -> Result<ChatResponse> {
        let fut = self.chat_inner(messages, tools, config);
        match self.max_request_duration {
            Some(dur) => tokio::time::timeout(dur, fut).await.map_err(|_| {
                eyre::eyre!(
                    "ProviderChain timed out after {:.0}s (all retries + failovers)",
                    dur.as_secs_f64()
                )
            })?,
            None => fut.await,
        }
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

            match slot.provider.chat_stream(messages, tools, config).await {
                Ok(stream) => {
                    self.record_success(idx);
                    return Ok(stream);
                }
                Err(e) => {
                    let retryable = RetryProvider::should_failover(&e);
                    self.record_failure(idx);

                    if retryable && offset + 1 < self.slots.len() {
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

    fn model_id(&self) -> &str {
        let idx = self.pick_start();
        self.slots[idx].provider.model_id()
    }

    fn provider_name(&self) -> &str {
        let idx = self.pick_start();
        self.slots[idx].provider.provider_name()
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
}
