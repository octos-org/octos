use std::sync::Arc;

use async_trait::async_trait;
use eyre::Result;
use octos_core::Message;
use octos_llm::{
    AdaptiveConfig, AdaptiveRouter, ChatConfig, ChatResponse, ContextWindowOverride,
    FallbackProvider, LlmProvider, MiddlewareStack, ProviderChain, ProviderRouter, RetryProvider,
    SemaphoreThrottledProvider, StopReason, SwappableProvider, TokenUsage, ToolSpec,
};

struct CapabilityProvider {
    supports_hints: bool,
}

#[async_trait]
impl LlmProvider for CapabilityProvider {
    async fn chat(
        &self,
        _messages: &[Message],
        _tools: &[ToolSpec],
        _config: &ChatConfig,
    ) -> Result<ChatResponse> {
        Ok(ChatResponse {
            content: Some("ok".to_owned()),
            reasoning_content: None,
            tool_calls: Vec::new(),
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage::default(),
            provider_index: None,
        })
    }

    fn model_id(&self) -> &str {
        if self.supports_hints {
            "semantic-model"
        } else {
            "hosted-model"
        }
    }

    fn provider_name(&self) -> &str {
        "capability-test"
    }

    fn supports_semantic_checkpoint_hints(&self) -> bool {
        self.supports_hints
    }
}

fn provider(supports_hints: bool) -> Arc<dyn LlmProvider> {
    Arc::new(CapabilityProvider { supports_hints })
}

#[test]
fn fixed_wrappers_forward_the_current_concrete_provider_capability() {
    assert!(ContextWindowOverride::new(provider(true), 8_000).supports_semantic_checkpoint_hints());
    assert!(RetryProvider::new(provider(true)).supports_semantic_checkpoint_hints());
    assert!(MiddlewareStack::new(provider(true)).supports_semantic_checkpoint_hints());
    assert!(
        SemaphoreThrottledProvider::with_limit(provider(true), 1)
            .supports_semantic_checkpoint_hints()
    );

    assert!(
        !ContextWindowOverride::new(provider(false), 8_000).supports_semantic_checkpoint_hints()
    );
    assert!(!RetryProvider::new(provider(false)).supports_semantic_checkpoint_hints());
    assert!(!MiddlewareStack::new(provider(false)).supports_semantic_checkpoint_hints());
    assert!(
        !SemaphoreThrottledProvider::with_limit(provider(false), 1)
            .supports_semantic_checkpoint_hints()
    );
}

#[test]
fn swappable_and_active_router_follow_the_provider_selected_now() {
    let swappable = SwappableProvider::new(provider(false));
    assert!(!swappable.supports_semantic_checkpoint_hints());
    swappable.swap(provider(true));
    assert!(swappable.supports_semantic_checkpoint_hints());

    let router = ProviderRouter::new();
    assert!(!router.supports_semantic_checkpoint_hints());
    router.register("hosted", provider(false));
    router.register("local", provider(true));
    assert!(!router.supports_semantic_checkpoint_hints());
    router.set_active("local");
    assert!(router.supports_semantic_checkpoint_hints());
    router.set_active("missing");
    assert!(!router.supports_semantic_checkpoint_hints());
}

#[test]
fn failover_capability_covers_every_reachable_concrete_lane() {
    let fallback = FallbackProvider::new(provider(false), vec![provider(true)]);
    assert!(fallback.supports_semantic_checkpoint_hints());

    let chain = ProviderChain::new(vec![provider(false), provider(true)]);
    assert!(chain.supports_semantic_checkpoint_hints());

    let adaptive = AdaptiveRouter::new(
        vec![provider(false), provider(true)],
        &[],
        AdaptiveConfig::default(),
    );
    assert!(adaptive.supports_semantic_checkpoint_hints());

    let unsupported_fallback = FallbackProvider::new(provider(false), vec![provider(false)]);
    assert!(!unsupported_fallback.supports_semantic_checkpoint_hints());
    let unsupported_chain = ProviderChain::new(vec![provider(false), provider(false)]);
    assert!(!unsupported_chain.supports_semantic_checkpoint_hints());
    let unsupported_adaptive = AdaptiveRouter::new(
        vec![provider(false), provider(false)],
        &[],
        AdaptiveConfig::default(),
    );
    assert!(!unsupported_adaptive.supports_semantic_checkpoint_hints());
}
