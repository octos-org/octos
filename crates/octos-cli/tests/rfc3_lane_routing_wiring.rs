//! RFC-3 (#1292) — per-topic model lane routing wiring tests.
//!
//! Three behaviors are pinned here to prevent silent regressions:
//!
//! 1. Sessions opened against a `slides:*` topic resolve to the
//!    `InstructionStrong` lane and the [`octos_llm::LaneContext`]
//!    carries that lane through to the chat call.
//! 2. When a turn is dispatched with a lane scope active, the
//!    [`octos_llm::AdaptiveRouter`] filters its candidate set to the
//!    lane's `(provider, model)` list before scoring.
//! 3. When the first candidate of a lane has its circuit-breaker
//!    open, the router advances to the next candidate in the lane
//!    list (rather than falling through to a non-lane backstop).
//!
//! Equivalent unit-level coverage lives in
//! `crates/octos-llm/src/lane.rs::tests` (resolver + config) and
//! `crates/octos-llm/src/adaptive.rs::tests` (provider selection);
//! this file is the cross-crate wiring contract.

#![cfg(feature = "api")]

use std::sync::Arc;

use async_trait::async_trait;
use eyre::Result;
use octos_core::Message;
use octos_llm::{
    AdaptiveConfig, AdaptiveRouter, ChatConfig, ChatResponse, ChatStream, Lane, LaneContext,
    LaneRoutingConfig, LlmProvider, StopReason, TokenUsage, ToolSpec, with_lane_context,
};

struct StubProvider {
    name: &'static str,
    model: &'static str,
    fail: bool,
}

#[async_trait]
impl LlmProvider for StubProvider {
    async fn chat(
        &self,
        _messages: &[Message],
        _tools: &[ToolSpec],
        _config: &ChatConfig,
    ) -> Result<ChatResponse> {
        if self.fail {
            eyre::bail!("{}/{} simulated failure", self.name, self.model);
        }
        Ok(ChatResponse {
            content: Some(format!("from-{}/{}", self.name, self.model)),
            reasoning_content: None,
            tool_calls: vec![],
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage::default(),
            provider_index: None,
        })
    }

    async fn chat_stream(
        &self,
        _messages: &[Message],
        _tools: &[ToolSpec],
        _config: &ChatConfig,
    ) -> Result<ChatStream> {
        eyre::bail!("chat_stream not used in this test")
    }

    fn model_id(&self) -> &str {
        self.model
    }

    fn provider_name(&self) -> &str {
        self.name
    }
}

#[tokio::test]
async fn session_with_slides_topic_stamps_instruction_strong_lane() {
    // RFC-3 contract: a session whose topic prefix is `slides`
    // resolves to the InstructionStrong lane via the built-in
    // defaults, with no profile config required.
    let ctx = LaneContext::for_topic(Some("slides:fastchain-demo"), None);
    assert_eq!(ctx.lane, Some(Lane::InstructionStrong));

    let candidates = ctx.candidates();
    assert!(
        !candidates.is_empty(),
        "InstructionStrong lane must carry built-in candidates"
    );
    assert!(
        candidates
            .iter()
            .any(|(p, m)| p == "anthropic" && m == "claude-sonnet-4-6"),
        "claude-sonnet-4-6 must be in the InstructionStrong candidate list"
    );
}

#[tokio::test]
async fn turn_dispatch_with_lane_filters_candidates_to_lane_list() {
    // RFC-3 contract: when the per-turn LaneContext resolves to a
    // non-General lane with matching candidates in the router's
    // slot list, the router routes to a lane candidate even when
    // a non-lane slot would otherwise win on priority.
    let router = AdaptiveRouter::new(
        vec![
            // Slot 0: high-priority backstop not in the lane.
            Arc::new(StubProvider {
                name: "openrouter",
                model: "gpt-4o-mini",
                fail: false,
            }),
            // Slot 1: in the InstructionStrong lane (claude-sonnet-4-6
            // is the first default candidate).
            Arc::new(StubProvider {
                name: "anthropic",
                model: "claude-sonnet-4-6",
                fail: false,
            }),
        ],
        &[],
        AdaptiveConfig {
            probe_probability: 0.0,
            ..Default::default()
        },
    );

    // Without the lane scope, priority order picks slot 0.
    let baseline = router
        .chat(&[], &[], &ChatConfig::default())
        .await
        .expect("baseline turn");
    assert_eq!(
        baseline.content.as_deref(),
        Some("from-openrouter/gpt-4o-mini")
    );

    // With a `slides:` topic in scope the lane filter narrows
    // selection to the InstructionStrong candidate list, so the
    // router picks the anthropic slot.
    let ctx = LaneContext::for_topic(Some("slides:demo"), None);
    let scoped = with_lane_context(ctx, async {
        router.chat(&[], &[], &ChatConfig::default()).await
    })
    .await
    .expect("scoped turn");
    assert_eq!(
        scoped.content.as_deref(),
        Some("from-anthropic/claude-sonnet-4-6"),
    );
}

#[tokio::test]
async fn circuit_breaker_open_on_first_candidate_falls_through_to_second() {
    // Profile override: lane `slides` → FastChat, lane_models
    // FastChat = [primary-fail, secondary-ok]. Trip the primary's
    // circuit, then assert the next call routes to the secondary
    // (within the lane), not to the out-of-lane slot.
    let router = AdaptiveRouter::new(
        vec![
            // Slot 0: lane primary, failing.
            Arc::new(StubProvider {
                name: "wisemodel",
                model: "kimi-k2.6",
                fail: true,
            }),
            // Slot 1: lane secondary, healthy.
            Arc::new(StubProvider {
                name: "deepseek",
                model: "deepseek-chat",
                fail: false,
            }),
            // Slot 2: out-of-lane backstop.
            Arc::new(StubProvider {
                name: "openrouter",
                model: "fallback",
                fail: false,
            }),
        ],
        &[],
        AdaptiveConfig {
            probe_probability: 0.0,
            failure_threshold: 1,
            ..Default::default()
        },
    );

    // Trip slot 0's circuit with a baseline call. With
    // `failure_threshold: 1`, a single failing chat() trips the
    // breaker; subsequent calls bypass slot 0.
    let _ = router.chat(&[], &[], &ChatConfig::default()).await;

    // Now the lane filter should advance past slot 0 (circuit
    // open) and pick slot 1, NOT fall through to slot 2.
    let mut cfg = LaneRoutingConfig::default();
    cfg.topic_lanes.insert("loop".to_string(), Lane::FastChat);
    let ctx = LaneContext::for_topic(Some("loop:test"), Some(&cfg));
    let resp = with_lane_context(ctx, async {
        router.chat(&[], &[], &ChatConfig::default()).await
    })
    .await
    .expect("scoped turn after circuit trip");
    assert_eq!(
        resp.content.as_deref(),
        Some("from-deepseek/deepseek-chat"),
        "lane filter should advance to the second candidate, not the out-of-lane slot",
    );
}

#[tokio::test]
async fn profile_lane_routing_field_round_trips_through_serde() {
    // RFC-3 backward compat anchor: a profile-config JSON without
    // a `lane_routing` block must continue to deserialize, and a
    // round-trip serialize+deserialize must preserve any custom
    // overrides.
    let bare = serde_json::json!({
        "id": "p1",
        "name": "Test",
        "enabled": false,
        "config": {},
        "created_at": "2026-05-25T00:00:00Z",
        "updated_at": "2026-05-25T00:00:00Z",
    });
    let parsed: octos_cli::profiles::UserProfile =
        serde_json::from_value(bare).expect("bare profile must deserialize");
    assert!(
        parsed.config.lane_routing.is_none(),
        "profile without lane_routing must parse with field = None",
    );

    let mut profile = parsed;
    let mut cfg = LaneRoutingConfig::default();
    cfg.topic_lanes.insert("custom".to_string(), Lane::FastChat);
    cfg.lane_models.insert(
        Lane::InstructionStrong,
        vec![("my-provider".to_string(), "my-model".to_string())],
    );
    profile.config.lane_routing = Some(cfg.clone());

    let serialized =
        serde_json::to_string(&profile).expect("profile with lane_routing must serialize");
    let round_trip: octos_cli::profiles::UserProfile =
        serde_json::from_str(&serialized).expect("round-trip must deserialize");
    assert_eq!(round_trip.config.lane_routing, Some(cfg));
}
