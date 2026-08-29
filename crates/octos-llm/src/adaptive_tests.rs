use super::*;
use crate::types::{StopReason, TokenUsage};
use std::sync::Arc;

struct MockProvider {
    name: &'static str,
    model: &'static str,
    latency_ms: u64,
    fail: bool,
    error_msg: &'static str,
}

#[async_trait]
impl LlmProvider for MockProvider {
    async fn chat(
        &self,
        _messages: &[Message],
        _tools: &[ToolSpec],
        _config: &ChatConfig,
    ) -> Result<ChatResponse> {
        tokio::time::sleep(std::time::Duration::from_millis(self.latency_ms)).await;
        if self.fail {
            eyre::bail!("{} API error: 429 - rate limited", self.error_msg);
        }
        Ok(ChatResponse {
            content: Some(format!("from-{}", self.name)),
            reasoning_content: None,
            tool_calls: vec![],
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage::default(),
            provider_index: None,
        })
    }

    fn model_id(&self) -> &str {
        self.model
    }

    fn provider_name(&self) -> &str {
        self.name
    }
}

/// Failure provider that is ready on its first poll.
///
/// This is intentionally separate from [`MockProvider`]: a zero-duration
/// Tokio sleep still yields, while hedge-ordering tests need an immediate
/// failure without changing the scheduling semantics of every zero-latency
/// mock in this module.
struct ImmediateFailureProvider {
    name: &'static str,
    model: &'static str,
    error_msg: &'static str,
}

#[async_trait]
impl LlmProvider for ImmediateFailureProvider {
    async fn chat(
        &self,
        _messages: &[Message],
        _tools: &[ToolSpec],
        _config: &ChatConfig,
    ) -> Result<ChatResponse> {
        eyre::bail!("{} API error: 429 - rate limited", self.error_msg);
    }

    fn model_id(&self) -> &str {
        self.model
    }

    fn provider_name(&self) -> &str {
        self.name
    }
}

#[tokio::test]
async fn test_selects_primary_on_cold_start() {
    let router = AdaptiveRouter::new(
        vec![
            Arc::new(MockProvider {
                name: "primary",
                model: "m1",
                latency_ms: 0,
                fail: false,
                error_msg: "",
            }),
            Arc::new(MockProvider {
                name: "fallback",
                model: "m2",
                latency_ms: 0,
                fail: false,
                error_msg: "",
            }),
        ],
        &[],
        AdaptiveConfig {
            probe_probability: 0.0, // Disable probes for determinism
            ..Default::default()
        },
    );

    let resp = router.chat(&[], &[], &ChatConfig::default()).await.unwrap();
    assert_eq!(resp.content.unwrap(), "from-primary");
}

#[tokio::test]
async fn test_chat_returns_exact_provider_index_after_failover() {
    let router = AdaptiveRouter::new(
        vec![
            Arc::new(MockProvider {
                name: "primary",
                model: "m1",
                latency_ms: 0,
                fail: true,
                error_msg: "Primary",
            }),
            Arc::new(MockProvider {
                name: "fallback",
                model: "m2",
                latency_ms: 0,
                fail: false,
                error_msg: "",
            }),
        ],
        &[],
        AdaptiveConfig {
            probe_probability: 0.0,
            ..Default::default()
        },
    );

    let resp = router.chat(&[], &[], &ChatConfig::default()).await.unwrap();
    assert_eq!(resp.content.as_deref(), Some("from-fallback"));
    assert_eq!(resp.provider_index, Some(1));

    let metadata = router.provider_metadata_for_index(resp.provider_index);
    assert_eq!(metadata.provider, "fallback");
    assert_eq!(metadata.model, "m2");
    assert_eq!(metadata.display_label(), "fallback/m2");
}

#[tokio::test]
async fn test_chat_stream_emits_exact_provider_index_after_failover() {
    let router = AdaptiveRouter::new(
        vec![
            Arc::new(MockProvider {
                name: "primary",
                model: "m1",
                latency_ms: 0,
                fail: true,
                error_msg: "Primary",
            }),
            Arc::new(MockProvider {
                name: "fallback",
                model: "m2",
                latency_ms: 0,
                fail: false,
                error_msg: "",
            }),
        ],
        &[],
        AdaptiveConfig {
            probe_probability: 0.0,
            ..Default::default()
        },
    );

    let mut stream = router
        .chat_stream(&[], &[], &ChatConfig::default())
        .await
        .unwrap();

    let first = stream.next().await.expect("provider index event");
    assert!(matches!(first, StreamEvent::ProviderIndex(1)));

    let second = stream.next().await.expect("text event");
    assert!(matches!(second, StreamEvent::TextDelta(ref text) if text == "from-fallback"));
}

#[tokio::test]
async fn test_failover_on_error() {
    let router = AdaptiveRouter::new(
        vec![
            Arc::new(MockProvider {
                name: "primary",
                model: "m1",
                latency_ms: 0,
                fail: true,
                error_msg: "Primary",
            }),
            Arc::new(MockProvider {
                name: "fallback",
                model: "m2",
                latency_ms: 0,
                fail: false,
                error_msg: "",
            }),
        ],
        &[],
        AdaptiveConfig::default(),
    );

    let resp = router.chat(&[], &[], &ChatConfig::default()).await.unwrap();
    assert_eq!(resp.content.unwrap(), "from-fallback");
}

#[tokio::test]
async fn test_circuit_breaker_skips_degraded() {
    let config = AdaptiveConfig {
        failure_threshold: 1,
        probe_probability: 0.0, // Disable probes for determinism
        ..Default::default()
    };
    let router = AdaptiveRouter::new(
        vec![
            Arc::new(MockProvider {
                name: "primary",
                model: "m1",
                latency_ms: 0,
                fail: true,
                error_msg: "Primary",
            }),
            Arc::new(MockProvider {
                name: "fallback",
                model: "m2",
                latency_ms: 0,
                fail: false,
                error_msg: "",
            }),
        ],
        &[],
        config,
    );

    // First call: primary fails (consecutive_failures=1, trips circuit breaker),
    // failover to fallback succeeds
    let resp = router.chat(&[], &[], &ChatConfig::default()).await.unwrap();
    assert_eq!(resp.content.unwrap(), "from-fallback");

    // Primary is now circuit-broken
    assert!(router.slots[0].metrics.is_circuit_open(1));

    // Second call: should skip primary entirely, go straight to fallback
    let resp = router.chat(&[], &[], &ChatConfig::default()).await.unwrap();
    assert_eq!(resp.content.unwrap(), "from-fallback");
}

#[tokio::test]
async fn test_all_providers_fail() {
    let router = AdaptiveRouter::new(
        vec![
            Arc::new(MockProvider {
                name: "p1",
                model: "m1",
                latency_ms: 0,
                fail: true,
                error_msg: "P1",
            }),
            Arc::new(MockProvider {
                name: "p2",
                model: "m2",
                latency_ms: 0,
                fail: true,
                error_msg: "P2",
            }),
        ],
        &[],
        AdaptiveConfig::default(),
    );

    let result = router.chat(&[], &[], &ChatConfig::default()).await;
    assert!(result.is_err());
}

#[tokio::test]
async fn test_metrics_snapshot() {
    let router = AdaptiveRouter::new(
        vec![Arc::new(MockProvider {
            name: "test",
            model: "m1",
            latency_ms: 10,
            fail: false,
            error_msg: "",
        })],
        &[],
        AdaptiveConfig::default(),
    );

    let _ = router.chat(&[], &[], &ChatConfig::default()).await;

    let snaps = router.metrics_snapshots();
    assert_eq!(snaps.len(), 1);
    assert_eq!(snaps[0].0, "test");
    assert_eq!(snaps[0].2.success_count, 1);
    assert_eq!(snaps[0].2.failure_count, 0);
    assert!(snaps[0].2.latency_ema_ms > 0.0);
}

#[test]
fn test_scoring_cold_start_respects_priority() {
    let router = AdaptiveRouter::new(
        vec![
            Arc::new(MockProvider {
                name: "p1",
                model: "m1",
                latency_ms: 0,
                fail: false,
                error_msg: "",
            }),
            Arc::new(MockProvider {
                name: "p2",
                model: "m2",
                latency_ms: 0,
                fail: false,
                error_msg: "",
            }),
        ],
        &[],
        AdaptiveConfig::default(),
    );

    // On cold start, primary (priority=0) should score lower than fallback (priority=1)
    let score_primary = router.score(&router.slots[0]);
    let score_fallback = router.score(&router.slots[1]);
    assert!(score_primary < score_fallback);
}

#[test]
fn test_latency_samples_p95() {
    let mut samples = LatencySamples::new();
    // Push 100 values: 1..=100
    for i in 1..=100u64 {
        samples.push(i * 1000);
    }
    // p95 of 1..100 should be around 95-96
    let p95 = samples.p95();
    // Buffer is 64 slots, so we have values 37..100
    // p95 of 37..100 = ceil(64*0.95) = 61st value = 97
    assert!((90_000..=100_000).contains(&p95), "p95 was {}", p95 / 1000);
}

#[tokio::test]
async fn test_lane_changing_off_uses_priority_order() {
    let config = AdaptiveConfig {
        failure_threshold: 2,
        probe_probability: 0.0,
        ..Default::default()
    };
    let router = AdaptiveRouter::new(
        vec![
            Arc::new(MockProvider {
                name: "primary",
                model: "m1",
                latency_ms: 50, // slower
                fail: false,
                error_msg: "",
            }),
            Arc::new(MockProvider {
                name: "fast-fallback",
                model: "m2",
                latency_ms: 1, // faster
                fail: false,
                error_msg: "",
            }),
        ],
        &[],
        config,
    );

    // Lane changing OFF (default) — should always pick primary despite higher latency
    router.set_mode(AdaptiveMode::Off);

    // Warm up metrics so the score-based path would prefer fast-fallback
    for _ in 0..5 {
        let resp = router.chat(&[], &[], &ChatConfig::default()).await.unwrap();
        assert_eq!(resp.content.as_deref(), Some("from-primary"));
    }

    // Even after metrics show primary is slower, lane_changing=OFF sticks to priority
    let resp = router.chat(&[], &[], &ChatConfig::default()).await.unwrap();
    assert_eq!(resp.content.as_deref(), Some("from-primary"));
}

#[tokio::test]
async fn test_lane_changing_off_skips_circuit_broken() {
    let config = AdaptiveConfig {
        failure_threshold: 1, // trip after 1 failure
        probe_probability: 0.0,
        ..Default::default()
    };
    let router = AdaptiveRouter::new(
        vec![
            Arc::new(MockProvider {
                name: "primary",
                model: "m1",
                latency_ms: 0,
                fail: true,
                error_msg: "Primary",
            }),
            Arc::new(MockProvider {
                name: "fallback",
                model: "m2",
                latency_ms: 0,
                fail: false,
                error_msg: "",
            }),
        ],
        &[],
        config,
    );
    router.set_mode(AdaptiveMode::Off);

    // Primary fails → circuit breaks → falls over to fallback
    let resp = router.chat(&[], &[], &ChatConfig::default()).await.unwrap();
    assert_eq!(resp.content.as_deref(), Some("from-fallback"));

    // Now primary is circuit-broken; lane_changing=OFF should skip it
    assert!(router.slots[0].metrics.is_circuit_open(1));
    let resp = router.chat(&[], &[], &ChatConfig::default()).await.unwrap();
    assert_eq!(resp.content.as_deref(), Some("from-fallback"));
}

#[tokio::test]
async fn test_hedged_racing_picks_faster_provider() {
    let config = AdaptiveConfig {
        probe_probability: 0.0,
        ..Default::default()
    };
    let router = AdaptiveRouter::new(
        vec![
            Arc::new(MockProvider {
                name: "slow-primary",
                model: "m1",
                latency_ms: 200, // slow
                fail: false,
                error_msg: "",
            }),
            Arc::new(MockProvider {
                name: "fast-fallback",
                model: "m2",
                latency_ms: 10, // fast
                fail: false,
                error_msg: "",
            }),
        ],
        &[],
        config,
    );

    // Enable hedged racing
    router.set_mode(AdaptiveMode::Hedge);

    let start = Instant::now();
    let resp = router.chat(&[], &[], &ChatConfig::default()).await.unwrap();
    let elapsed = start.elapsed();

    // Should get the fast provider's response (race winner)
    assert_eq!(resp.content.as_deref(), Some("from-fast-fallback"));
    // Should complete in ~10ms, not ~200ms
    assert!(
        elapsed.as_millis() < 150,
        "took {}ms, expected <150ms",
        elapsed.as_millis()
    );
}

#[tokio::test]
async fn test_hedged_racing_survives_one_failure() {
    let config = AdaptiveConfig {
        probe_probability: 0.0,
        ..Default::default()
    };
    let router = AdaptiveRouter::new(
        vec![
            Arc::new(MockProvider {
                name: "failing-primary",
                model: "m1",
                latency_ms: 0,
                fail: true,
                error_msg: "Primary",
            }),
            Arc::new(MockProvider {
                name: "good-fallback",
                model: "m2",
                latency_ms: 10,
                fail: false,
                error_msg: "",
            }),
        ],
        &[],
        config,
    );

    router.set_mode(AdaptiveMode::Hedge);

    let resp = router.chat(&[], &[], &ChatConfig::default()).await.unwrap();
    assert_eq!(resp.content.as_deref(), Some("from-good-fallback"));
}

#[tokio::test]
async fn test_hedged_off_uses_single_provider() {
    let config = AdaptiveConfig {
        probe_probability: 0.0,
        ..Default::default()
    };
    let router = AdaptiveRouter::new(
        vec![
            Arc::new(MockProvider {
                name: "slow-primary",
                model: "m1",
                latency_ms: 50,
                fail: false,
                error_msg: "",
            }),
            Arc::new(MockProvider {
                name: "fast-fallback",
                model: "m2",
                latency_ms: 1,
                fail: false,
                error_msg: "",
            }),
        ],
        &[],
        config,
    );

    // Hedging OFF (default) — should use primary (priority order)
    let resp = router.chat(&[], &[], &ChatConfig::default()).await.unwrap();
    assert_eq!(resp.content.as_deref(), Some("from-slow-primary"));
}

#[test]
#[should_panic(expected = "at least one provider")]
fn test_empty_router_panics() {
    let _ = AdaptiveRouter::new(vec![], &[], AdaptiveConfig::default());
}

/// Lane mode selects best provider by score after warm-up.
/// Primary is warmed up with high error rate, then Lane switches to fallback.
#[tokio::test]
async fn test_lane_mode_picks_best_by_score() {
    let config = AdaptiveConfig {
        probe_probability: 0.0,
        latency_threshold_ms: 100,
        weight_priority: 0.05, // Low priority weight so metrics dominate
        weight_latency: 0.3,
        weight_error_rate: 0.45,
        weight_cost: 0.2,
        ..Default::default()
    };
    let router = AdaptiveRouter::new(
        vec![
            Arc::new(MockProvider {
                name: "slow-primary",
                model: "m1",
                latency_ms: 50,
                fail: false,
                error_msg: "",
            }),
            Arc::new(MockProvider {
                name: "fast-fallback",
                model: "m2",
                latency_ms: 5,
                fail: false,
                error_msg: "",
            }),
        ],
        &[],
        config,
    );

    // Warm up in Off mode (priority order → primary always selected).
    router.set_mode(AdaptiveMode::Off);
    for _ in 0..12 {
        let _ = router.chat(&[], &[], &ChatConfig::default()).await.unwrap();
    }

    // Inject failure metrics on the primary to make it score worse.
    // record_failure increments failure_count which raises error_rate.
    for _ in 0..8 {
        router.slots[0].metrics.record_failure();
    }

    // Switch to Lane mode. Primary has high error rate + high latency.
    // Fallback is cold (neutral scores) but has no errors.
    // With weight_error_rate=0.45, primary's high error score should
    // push Lane to prefer fallback despite its higher priority index.
    router.set_mode(AdaptiveMode::Lane);
    let resp = router.chat(&[], &[], &ChatConfig::default()).await.unwrap();
    assert_eq!(resp.content.as_deref(), Some("from-fast-fallback"));
}

/// Hedge mode with single provider falls through to single-provider path.
#[tokio::test]
async fn test_hedge_single_provider_falls_through() {
    let config = AdaptiveConfig {
        probe_probability: 0.0,
        ..Default::default()
    };
    let router = AdaptiveRouter::new(
        vec![Arc::new(MockProvider {
            name: "only",
            model: "m1",
            latency_ms: 10,
            fail: false,
            error_msg: "",
        })],
        &[],
        config,
    );
    router.set_mode(AdaptiveMode::Hedge);

    // Should succeed via single-provider path (hedged_chat returns None)
    let resp = router.chat(&[], &[], &ChatConfig::default()).await.unwrap();
    assert_eq!(resp.content.as_deref(), Some("from-only"));
}

/// Runtime mode switching works correctly.
#[test]
fn test_mode_switch_at_runtime() {
    let router = AdaptiveRouter::new(
        vec![
            Arc::new(MockProvider {
                name: "p1",
                model: "m1",
                latency_ms: 0,
                fail: false,
                error_msg: "",
            }),
            Arc::new(MockProvider {
                name: "p2",
                model: "m2",
                latency_ms: 0,
                fail: false,
                error_msg: "",
            }),
        ],
        &[],
        AdaptiveConfig::default(),
    );

    assert_eq!(router.mode(), AdaptiveMode::Off);
    router.set_mode(AdaptiveMode::Hedge);
    assert_eq!(router.mode(), AdaptiveMode::Hedge);
    router.set_mode(AdaptiveMode::Lane);
    assert_eq!(router.mode(), AdaptiveMode::Lane);
    router.set_mode(AdaptiveMode::Off);
    assert_eq!(router.mode(), AdaptiveMode::Off);
}

/// Adaptive status reports current mode and provider count.
#[tokio::test]
async fn test_adaptive_status_reports_correctly() {
    let router = AdaptiveRouter::new(
        vec![
            Arc::new(MockProvider {
                name: "p1",
                model: "m1",
                latency_ms: 10,
                fail: false,
                error_msg: "",
            }),
            Arc::new(MockProvider {
                name: "p2",
                model: "m2",
                latency_ms: 5,
                fail: false,
                error_msg: "",
            }),
        ],
        &[],
        AdaptiveConfig::default(),
    );

    let status = router.adaptive_status();
    assert_eq!(status.mode, AdaptiveMode::Off);
    assert_eq!(status.provider_count, 2);

    router.set_mode(AdaptiveMode::Hedge);
    let status = router.adaptive_status();
    assert_eq!(status.mode, AdaptiveMode::Hedge);
}

/// Metrics export includes all providers after calls.
#[tokio::test]
async fn test_metrics_export_after_calls() {
    let router = AdaptiveRouter::new(
        vec![
            Arc::new(MockProvider {
                name: "primary",
                model: "m1",
                latency_ms: 10,
                fail: false,
                error_msg: "",
            }),
            Arc::new(MockProvider {
                name: "fallback",
                model: "m2",
                latency_ms: 5,
                fail: false,
                error_msg: "",
            }),
        ],
        &[],
        AdaptiveConfig {
            probe_probability: 0.0,
            ..Default::default()
        },
    );

    // Make some calls
    for _ in 0..3 {
        let _ = router.chat(&[], &[], &ChatConfig::default()).await;
    }

    let shared = router.export_shared_metrics();
    assert_eq!(shared.providers.len(), 2);
    // Primary was called 3 times
    let primary = shared
        .providers
        .iter()
        .find(|p| p.provider == "primary")
        .unwrap();
    assert_eq!(primary.metrics.success_count, 3);
    // Fallback not called (Off mode uses priority)
    let fallback = shared
        .providers
        .iter()
        .find(|p| p.provider == "fallback")
        .unwrap();
    assert_eq!(fallback.metrics.success_count, 0);
}

/// QoS ranking toggle is independent of mode.
#[test]
fn test_qos_ranking_toggle() {
    let router = AdaptiveRouter::new(
        vec![Arc::new(MockProvider {
            name: "p1",
            model: "m1",
            latency_ms: 0,
            fail: false,
            error_msg: "",
        })],
        &[],
        AdaptiveConfig::default(),
    );

    let status = router.adaptive_status();
    assert!(!status.qos_ranking);

    router.set_qos_ranking(true);
    let status = router.adaptive_status();
    assert!(status.qos_ranking);

    // QoS ranking can be on with any mode
    router.set_mode(AdaptiveMode::Hedge);
    let status = router.adaptive_status();
    assert!(status.qos_ranking);
    assert_eq!(status.mode, AdaptiveMode::Hedge);
}

#[test]
fn should_record_failure_on_report_late_failure() {
    let config = AdaptiveConfig {
        failure_threshold: 2,
        probe_probability: 0.0,
        ..Default::default()
    };
    let router = AdaptiveRouter::new(
        vec![
            Arc::new(MockProvider {
                name: "primary",
                model: "m1",
                latency_ms: 0,
                fail: false,
                error_msg: "",
            }),
            Arc::new(MockProvider {
                name: "fallback",
                model: "m2",
                latency_ms: 0,
                fail: false,
                error_msg: "",
            }),
        ],
        &[],
        config,
    );

    // Initially no failures
    assert_eq!(
        router.slots[0]
            .metrics
            .consecutive_failures
            .load(Ordering::Relaxed),
        0
    );

    // Report late failure increments failure count on selected provider
    router.report_late_failure();
    assert_eq!(
        router.slots[0]
            .metrics
            .consecutive_failures
            .load(Ordering::Relaxed),
        1
    );

    // Second late failure trips the circuit breaker (threshold=2)
    router.report_late_failure();
    assert!(router.slots[0].metrics.is_circuit_open(2));
}

#[tokio::test]
async fn should_failover_after_late_failure_opens_circuit() {
    let config = AdaptiveConfig {
        failure_threshold: 1,
        probe_probability: 0.0,
        ..Default::default()
    };
    let router = AdaptiveRouter::new(
        vec![
            Arc::new(MockProvider {
                name: "primary",
                model: "m1",
                latency_ms: 0,
                fail: false,
                error_msg: "",
            }),
            Arc::new(MockProvider {
                name: "fallback",
                model: "m2",
                latency_ms: 0,
                fail: false,
                error_msg: "",
            }),
        ],
        &[],
        config,
    );

    // Late failure opens circuit breaker on primary
    router.report_late_failure();
    assert!(router.slots[0].metrics.is_circuit_open(1));

    // Next call should skip circuit-broken primary and go to fallback
    let resp = router.chat(&[], &[], &ChatConfig::default()).await.unwrap();
    assert_eq!(resp.content.as_deref(), Some("from-fallback"));
}

#[tokio::test]
async fn test_qos_ranking_changes_lane_selection() {
    let config = AdaptiveConfig {
        probe_probability: 0.0,
        ..AdaptiveConfig::default()
    };
    let router = AdaptiveRouter::new(
        vec![
            Arc::new(MockProvider {
                name: "priority-primary",
                model: "m1",
                latency_ms: 10,
                fail: false,
                error_msg: "",
            }),
            Arc::new(MockProvider {
                name: "quality-fallback",
                model: "m2",
                latency_ms: 10,
                fail: false,
                error_msg: "",
            }),
        ],
        &[0.0, 0.0],
        config,
    );
    router.seed_catalog(&[
        ModelCatalogEntry {
            provider: "priority-primary/m1".into(),
            model_type: ModelType::Strong,
            is_family_default: false,
            stability: 1.0,
            tool_avg_ms: 200,
            p95_ms: 300,
            score: 0.0,
            cost_in: 0.0,
            cost_out: 0.0,
            ds_output: 1000,
            context_window: 128_000,
            max_output: 8_192,
        },
        ModelCatalogEntry {
            provider: "quality-fallback/m2".into(),
            model_type: ModelType::Strong,
            is_family_default: false,
            stability: 1.0,
            tool_avg_ms: 200,
            p95_ms: 300,
            score: 0.0,
            cost_in: 0.0,
            cost_out: 0.0,
            ds_output: 5000,
            context_window: 128_000,
            max_output: 8_192,
        },
    ]);

    router.set_mode(AdaptiveMode::Lane);
    router.set_qos_ranking(false);
    let without_qos = router.chat(&[], &[], &ChatConfig::default()).await.unwrap();
    assert_eq!(
        without_qos.content.as_deref(),
        Some("from-priority-primary")
    );

    router.set_qos_ranking(true);
    let with_qos = router.chat(&[], &[], &ChatConfig::default()).await.unwrap();
    assert_eq!(with_qos.content.as_deref(), Some("from-quality-fallback"));
}

/// An OpenAI-compatible lane carries an `@host` suffix in its
/// provider_name (`moonshot@api`), but the canonical catalog uses the
/// untagged family key (`moonshot/kimi-k2.5`). `seed_catalog` must fall back
/// to the normalized key so the lane still receives its catalogued
/// type/cost/context/output — otherwise those are silently skipped.
#[test]
fn seed_catalog_matches_host_tagged_lane_to_bare_catalog_key() {
    use std::sync::atomic::Ordering;
    let router = AdaptiveRouter::new(
        vec![Arc::new(MockProvider {
            name: "moonshot@api",
            model: "kimi-k2.5",
            latency_ms: 10,
            fail: false,
            error_msg: "",
        })],
        &[0.0],
        AdaptiveConfig::default(),
    );
    router.seed_catalog(&[ModelCatalogEntry {
        provider: "moonshot/kimi-k2.5".into(),
        model_type: ModelType::Strong,
        is_family_default: false,
        stability: 1.0,
        tool_avg_ms: 200,
        p95_ms: 300,
        score: 0.0,
        cost_in: 0.6,
        cost_out: 2.4,
        ds_output: 1000,
        context_window: 262_144,
        max_output: 98_304,
    }]);
    // The host-tagged lane was seeded from the bare canonical entry.
    assert_eq!(
        router.slots[0].context_window.load(Ordering::Relaxed),
        262_144
    );
    assert_eq!(router.slots[0].max_output.load(Ordering::Relaxed), 98_304);
    assert_eq!(
        f64::from_bits(router.slots[0].seeded_cost_in.load(Ordering::Relaxed)),
        0.6
    );
}

#[test]
fn test_derive_cold_start_catalog_assigns_non_zero_scores() {
    let catalog = derive_cold_start_catalog(
        &[
            ModelCatalogEntry {
                provider: "moonshot/kimi-k2.5".into(),
                model_type: ModelType::Strong,
                is_family_default: false,
                stability: 0.93,
                tool_avg_ms: 1200,
                p95_ms: 2200,
                score: 0.0,
                cost_in: 2.0,
                cost_out: 10.0,
                ds_output: 4200,
                context_window: 128_000,
                max_output: 8_192,
            },
            ModelCatalogEntry {
                provider: "deepseek/deepseek-chat".into(),
                model_type: ModelType::Fast,
                is_family_default: false,
                stability: 1.0,
                tool_avg_ms: 1400,
                p95_ms: 2600,
                score: 0.0,
                cost_in: 1.0,
                cost_out: 4.0,
                ds_output: 4300,
                context_window: 64_000,
                max_output: 8_192,
            },
        ],
        &AdaptiveConfig::default(),
        true,
    );

    assert_eq!(catalog.models.len(), 2);
    assert!(catalog.models.iter().all(|model| model.score > 0.0));
    assert_ne!(catalog.models[0].score, catalog.models[1].score);
}

/// Hedge mode should NOT race the same provider against itself.
/// When all slots share the same provider_name, hedged_chat returns None
/// and the single-provider path is used instead.
#[tokio::test]
async fn should_skip_hedge_when_all_providers_same_name() {
    let config = AdaptiveConfig {
        probe_probability: 0.0,
        ..Default::default()
    };
    let router = AdaptiveRouter::new(
        vec![
            Arc::new(MockProvider {
                name: "moonshot",
                model: "kimi-k2.5",
                latency_ms: 10,
                fail: false,
                error_msg: "",
            }),
            Arc::new(MockProvider {
                name: "moonshot",
                model: "kimi-k2.5-alt",
                latency_ms: 5,
                fail: false,
                error_msg: "",
            }),
        ],
        &[],
        config,
    );
    router.set_mode(AdaptiveMode::Hedge);

    // Should succeed via single-provider path (hedged_chat skips same-name)
    let resp = router.chat(&[], &[], &ChatConfig::default()).await.unwrap();
    assert_eq!(resp.content.as_deref(), Some("from-moonshot"));
}

/// Hedge mode picks a different-named provider as alternate.
#[tokio::test]
async fn should_hedge_with_different_provider_names() {
    let config = AdaptiveConfig {
        probe_probability: 0.0,
        ..Default::default()
    };
    let router = AdaptiveRouter::new(
        vec![
            Arc::new(MockProvider {
                name: "moonshot",
                model: "kimi-k2.5",
                latency_ms: 200, // slow
                fail: false,
                error_msg: "",
            }),
            Arc::new(MockProvider {
                name: "moonshot",
                model: "kimi-alt",
                latency_ms: 5,
                fail: false,
                error_msg: "",
            }),
            Arc::new(MockProvider {
                name: "deepseek",
                model: "deepseek-chat",
                latency_ms: 10, // fast, different provider
                fail: false,
                error_msg: "",
            }),
        ],
        &[],
        config,
    );
    router.set_mode(AdaptiveMode::Hedge);

    // Should race moonshot vs deepseek (skipping moonshot[1] same name)
    let resp = router.chat(&[], &[], &ChatConfig::default()).await.unwrap();
    // deepseek is faster, so it wins the race
    assert_eq!(resp.content.as_deref(), Some("from-deepseek"));
}

#[test]
fn test_seed_baseline() {
    let router = AdaptiveRouter::new(
        vec![
            Arc::new(MockProvider {
                name: "dashscope",
                model: "qwen3.5-plus",
                latency_ms: 0,
                fail: false,
                error_msg: "",
            }),
            Arc::new(MockProvider {
                name: "gemini",
                model: "gemini-2.5-flash",
                latency_ms: 0,
                fail: false,
                error_msg: "",
            }),
        ],
        &[0.688, 0.60],
        AdaptiveConfig::default(),
    );

    let baseline = vec![
        BaselineEntry {
            provider: "dashscope/qwen3.5-plus".into(),
            avg_latency_ms: 2564,
            p95_latency_ms: 3560,
            stability: 1.0,
            cost_per_m_output: 0.688,
        },
        BaselineEntry {
            provider: "gemini/gemini-2.5-flash".into(),
            avg_latency_ms: 976,
            p95_latency_ms: 1090,
            stability: 1.0,
            cost_per_m_output: 0.60,
        },
    ];

    router.seed_baseline(&baseline);

    let snapshots = router.metrics_snapshots();
    // dashscope should have ~2564ms latency
    let (_, _, dash_metrics) = &snapshots[0];
    assert!(
        dash_metrics.latency_ema_ms > 2000.0,
        "dashscope EMA should be ~2564ms, got {}",
        dash_metrics.latency_ema_ms
    );
    assert_eq!(dash_metrics.success_count, 10);
    assert_eq!(dash_metrics.failure_count, 0);

    // gemini should have ~976ms latency
    let (_, _, gem_metrics) = &snapshots[1];
    assert!(
        gem_metrics.latency_ema_ms > 800.0,
        "gemini EMA should be ~976ms, got {}",
        gem_metrics.latency_ema_ms
    );
    assert!(gem_metrics.latency_ema_ms < 1200.0);

    // With Lane mode, scores should reflect seeded data (not cold start)
    router.set_mode(AdaptiveMode::Lane);
    let gemini_score = router.score(&router.slots[1]);
    let dash_score = router.score(&router.slots[0]);
    // Both should be non-zero (seeded, not cold start)
    assert!(
        gemini_score > 0.0,
        "gemini score should be non-zero after seeding"
    );
    assert!(
        dash_score > 0.0,
        "dashscope score should be non-zero after seeding"
    );
    // dashscope has higher latency → higher latency component
    // but lower priority (0 vs 1) → lower priority component
    // The exact ordering depends on weight balance, but latency should differ
    let gemini_latency = router.slots[1]
        .metrics
        .latency_ema_us
        .load(Ordering::Relaxed);
    let dash_latency = router.slots[0]
        .metrics
        .latency_ema_us
        .load(Ordering::Relaxed);
    assert!(
        dash_latency > gemini_latency,
        "dashscope latency should be higher than gemini"
    );
}

/// QoS score must actually move in response to live traffic.
///
/// Existing tests verify the score *function* is wired up
/// (`test_lane_mode_picks_best_by_score`, `test_metrics_export_after_calls`),
/// but none of them assert that calling `chat()` repeatedly causes the
/// composite score to *drift* per provider.
///
/// If the EMA / error-rate / consecutive-failure counters silently
/// stop updating, the router would silently freeze on its cold-start
/// scores — the test fleet would still hedge, the lane scorer would
/// still pick a "best" provider, but the choice would never adapt.
/// This test pins down two invariants:
///
///   1. **Scores move.** Before any traffic, both providers' scores
///      are at the cold-start baseline. After 8 chats both scores
///      must differ from baseline by at least a small epsilon.
///
///   2. **Scores reflect quality.** A fast/reliable provider must
///      score better (lower) than a frequently-failing one — not
///      just because of priority bias, but because traffic taught
///      the router so.
///
/// The setup uses Hedge mode but with a *fast-failing* second lane
/// (`fail=true, latency_ms=0`) instead of a slow-failing one. The
/// hedge race cancels the loser mid-flight and discards its
/// metrics, so a slow-failing lane would silently never record.
/// A fast-failing lane returns first with Err, drives the
/// "primary failed" branch, then the slow-good lane completes
/// sequentially and is recorded too. Both lanes get traffic.
#[tokio::test]
async fn should_drift_qos_score_in_response_to_live_traffic() {
    // Set failure_threshold high so the failing lane stays open
    // and keeps accumulating failures throughout the run — we
    // want the score to move, not to short-circuit out.
    let config = AdaptiveConfig {
        failure_threshold: 1000,
        probe_probability: 0.0,
        ..Default::default()
    };

    // Provider order matters: a 2nd-opinion review pointed out
    // that putting the failing lane at slot 0 stacks the priority
    // weight in its favor (priority bias rewards slot 0). After 8
    // chats, error_rate has to overcome priority bias to flip
    // the score order — that's a genuine signal but a narrow one.
    // To make the test honest, put the *good* lane at slot 0 so
    // the score-flip we assert is driven by traffic, not by
    // priority. The "scores move" assertion still catches a
    // frozen scorer regardless of priority bias.
    let router = AdaptiveRouter::new(
        vec![
            Arc::new(MockProvider {
                name: "slow-good",
                model: "m1",
                latency_ms: 10,
                fail: false,
                error_msg: "",
            }),
            Arc::new(ImmediateFailureProvider {
                name: "fast-fails",
                model: "m2",
                error_msg: "rate-limited",
            }),
        ],
        &[],
        config,
    );

    router.set_mode(AdaptiveMode::Hedge);

    let cold = router.export_shared_metrics();
    let cold_fail = cold
        .providers
        .iter()
        .find(|p| p.provider == "fast-fails")
        .expect("fast-fails in cold-start snapshot")
        .score;
    let cold_good = cold
        .providers
        .iter()
        .find(|p| p.provider == "slow-good")
        .expect("slow-good in cold-start snapshot")
        .score;

    // Drive enough traffic to populate the EMAs and error counters.
    // Each chat: fast-fails returns Err first → "primary failed"
    // path awaits slow-good sequentially → both lanes recorded.
    const RUNS: usize = 8;
    for _ in 0..RUNS {
        let _ = router.chat(&[], &[], &ChatConfig::default()).await;
    }

    let warm = router.export_shared_metrics();
    let warm_fail = warm
        .providers
        .iter()
        .find(|p| p.provider == "fast-fails")
        .expect("fast-fails in warm snapshot");
    let warm_good = warm
        .providers
        .iter()
        .find(|p| p.provider == "slow-good")
        .expect("slow-good in warm snapshot");

    // Sanity (per 2nd-opinion review): tight count + latency
    // assertions catch the case where some counters update but
    // others (latency EMA, throughput) are silently frozen — a
    // class of bug where the scorer "looks" alive but only
    // reflects error_rate.
    assert_eq!(
        warm_fail.metrics.failure_count, RUNS as u32,
        "fast-fails should have exactly {} failures, got {}",
        RUNS, warm_fail.metrics.failure_count,
    );
    assert_eq!(
        warm_good.metrics.success_count, RUNS as u32,
        "slow-good should have exactly {} successes, got {}",
        RUNS, warm_good.metrics.success_count,
    );
    assert!(
        warm_good.metrics.latency_ema_ms > 0.0,
        "slow-good latency_ema_ms should be > 0 after {} successful chats; got {}. EMA may be frozen even though success counters move.",
        RUNS,
        warm_good.metrics.latency_ema_ms,
    );

    // (1) Both scores must MOVE from cold start.
    let fail_drift = (warm_fail.score - cold_fail).abs();
    let good_drift = (warm_good.score - cold_good).abs();
    assert!(
        fail_drift > 1e-6,
        "fast-fails score did not move from cold start ({}) to warm ({}); QoS scoring may be frozen",
        cold_fail,
        warm_fail.score,
    );
    assert!(
        good_drift > 1e-6,
        "slow-good score did not move from cold start ({}) to warm ({}); QoS scoring may be frozen",
        cold_good,
        warm_good.score,
    );

    // (2) The reliable provider must score better (lower) than
    // the failing one. If this inverts after live traffic, the
    // weighting is broken — the router would route AWAY from
    // healthy providers, exactly the failure mode the QoS scorer
    // is meant to prevent. With slow-good at slot 0, priority bias
    // also favors it — so a flip would require both error_rate
    // and priority to invert, which is a stronger guarantee.
    assert!(
        warm_good.score < warm_fail.score,
        "slow-good ({}) did NOT score better than fast-fails ({}) after {} chats. Drifts: good Δ{:.4}, fail Δ{:.4}. error_rate good={:.2}, fail={:.2}",
        warm_good.score,
        warm_fail.score,
        RUNS,
        good_drift,
        fail_drift,
        warm_good.metrics.error_rate,
        warm_fail.metrics.error_rate,
    );
}

// ── Auto-escalation tests ─────────────────────────────────────────────

/// Helper: build a 2-provider router with permissive defaults so we can
/// drive the auto-escalation state machine in isolation.
fn auto_escalation_router() -> AdaptiveRouter {
    let providers: Vec<Arc<dyn LlmProvider>> = vec![
        Arc::new(MockProvider {
            name: "primary",
            model: "m1",
            latency_ms: 0,
            fail: false,
            error_msg: "",
        }),
        Arc::new(MockProvider {
            name: "fallback",
            model: "m2",
            latency_ms: 0,
            fail: false,
            error_msg: "",
        }),
    ];
    AdaptiveRouter::new(providers, &[], AdaptiveConfig::default())
        .with_adaptive_config(AdaptiveMode::Lane, false)
}

// ------------------------------------------------------------------
// Wave4-A: failover broadcast channel tests.
// ------------------------------------------------------------------

/// Wave4-A: subscribers receive a `FailoverEvent` when the router
/// crosses a lane in `chat()`. The first provider fails forcing
/// a failover to the second.
#[tokio::test]
async fn failover_broadcast_publishes_event_on_lane_change() {
    let config = AdaptiveConfig {
        failure_threshold: 5,
        probe_probability: 0.0,
        ..Default::default()
    };
    let router = AdaptiveRouter::new(
        vec![
            Arc::new(MockProvider {
                name: "primary",
                model: "m1",
                latency_ms: 0,
                fail: true,
                error_msg: "primary down",
            }),
            Arc::new(MockProvider {
                name: "fallback",
                model: "m2",
                latency_ms: 0,
                fail: false,
                error_msg: "",
            }),
        ],
        &[],
        config,
    );

    let mut rx = router.subscribe_failover();

    let messages = vec![Message::user("hello")];
    let tools: Vec<ToolSpec> = vec![];
    let cfg = ChatConfig::default();
    let _ = router.chat(&messages, &tools, &cfg).await.unwrap();

    // Wave4-A: the failover loop SHOULD have published at least one
    // event. Wait up to 100ms — broadcast::Sender::send is sync but
    // the chat loop yields, so we give the scheduler one tick.
    let event = tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv())
        .await
        .expect("broadcast channel receive timed out")
        .expect("broadcast channel was closed without an event");
    assert_eq!(event.from_provider, "primary/m1");
    assert_eq!(event.to_provider, "fallback/m2");
    assert!(
        event.reason.contains("chat_error"),
        "reason should describe the underlying chat error — got {}",
        event.reason
    );
}

/// Wave4-A: the broadcast channel MUST NOT block the router under
/// back-pressure. A slow subscriber falls behind and observes
/// `RecvError::Lagged` — but the router keeps publishing.
///
/// We construct a 64-deep burst (channel capacity) and verify that
/// the router survives without blocking and a fresh subscriber can
/// still read events afterwards.
#[tokio::test]
async fn failover_broadcast_does_not_block_router_under_backpressure() {
    let router = AdaptiveRouter::new(
        vec![Arc::new(MockProvider {
            name: "p1",
            model: "m1",
            latency_ms: 0,
            fail: false,
            error_msg: "",
        })],
        &[],
        AdaptiveConfig::default(),
    );

    // Subscribe but never drain — the channel capacity is 64, so the
    // 65th publish will start dropping the oldest event.
    let _stuck_rx = router.subscribe_failover();

    // Publish 100 events directly (the public API doesn't accept
    // direct publish; we go through the same internal entry point
    // the failover loops use).
    for i in 0..100 {
        router.publish_failover("from/m1", "to/m2", "test-burst", i);
    }

    // A fresh subscriber sees nothing of the previous 100 events
    // (broadcast::Receiver only sees events sent AFTER subscribe()),
    // but the channel must still be usable.
    let mut fresh_rx = router.subscribe_failover();
    router.publish_failover("from/m1", "to/m2", "post-burst", 999);
    let event = tokio::time::timeout(std::time::Duration::from_millis(100), fresh_rx.recv())
        .await
        .expect("fresh subscriber receive timed out")
        .expect("fresh subscriber channel closed");
    assert_eq!(event.elapsed_ms, 999);
    assert_eq!(event.reason, "post-burst");
}

/// Wave4-A: `lane_scores()` returns one entry per slot keyed by
/// `"<provider_name>/<model_id>"` in BTreeMap order.
#[test]
fn lane_scores_returns_deterministic_per_slot_snapshot() {
    let router = AdaptiveRouter::new(
        vec![
            Arc::new(MockProvider {
                name: "zai",
                model: "glm-5-turbo",
                latency_ms: 0,
                fail: false,
                error_msg: "",
            }),
            Arc::new(MockProvider {
                name: "ollama",
                model: "llama3.2",
                latency_ms: 0,
                fail: false,
                error_msg: "",
            }),
        ],
        &[],
        AdaptiveConfig::default(),
    );

    let scores = router.lane_scores();
    let keys: Vec<&String> = scores.keys().collect();
    assert_eq!(
        keys,
        vec![
            &"ollama/llama3.2".to_string(),
            &"zai/glm-5-turbo".to_string(),
        ],
        "lane_scores keys must be sorted (BTreeMap order)"
    );
}

/// Wave4-A: `breaker_states()` reports `"closed"` initially and
/// `"open"` once the consecutive-failure threshold trips.
#[tokio::test]
async fn breaker_states_reports_open_after_threshold() {
    let config = AdaptiveConfig {
        failure_threshold: 1,
        probe_probability: 0.0,
        ..Default::default()
    };
    let router = AdaptiveRouter::new(
        vec![
            Arc::new(MockProvider {
                name: "fail",
                model: "m1",
                latency_ms: 0,
                fail: true,
                error_msg: "always_down",
            }),
            Arc::new(MockProvider {
                name: "ok",
                model: "m2",
                latency_ms: 0,
                fail: false,
                error_msg: "",
            }),
        ],
        &[],
        config,
    );

    // Initially closed.
    let states = router.breaker_states();
    assert_eq!(states.get("fail/m1").map(String::as_str), Some("closed"));
    assert_eq!(states.get("ok/m2").map(String::as_str), Some("closed"));

    // Trip primary's breaker.
    let _ = router
        .chat(&[Message::user("hi")], &[], &ChatConfig::default())
        .await
        .unwrap();

    let states = router.breaker_states();
    assert_eq!(
        states.get("fail/m1").map(String::as_str),
        Some("open"),
        "primary breaker should be open after failure_threshold trips"
    );
}

/// Wave4-A (Codex P1): the failover publisher reads `ROUTER_CONTEXT`
/// via task_local and stamps the originating session/turn id onto
/// every emitted `FailoverEvent`. Subscribers filter on this so
/// concurrent sessions on the same profile-scoped router don't
/// receive each other's failovers.
#[tokio::test]
async fn failover_event_carries_originating_session_from_router_context() {
    let config = AdaptiveConfig {
        failure_threshold: 5,
        probe_probability: 0.0,
        ..Default::default()
    };
    let router = Arc::new(AdaptiveRouter::new(
        vec![
            Arc::new(MockProvider {
                name: "primary",
                model: "m1",
                latency_ms: 0,
                fail: true,
                error_msg: "primary down",
            }),
            Arc::new(MockProvider {
                name: "fallback",
                model: "m2",
                latency_ms: 0,
                fail: false,
                error_msg: "",
            }),
        ],
        &[],
        config,
    ));
    let mut rx = router.subscribe_failover();

    let router_clone = router.clone();
    let ctx = RouterContext {
        session_id: Some("local:session-A".into()),
        turn_id: Some("turn-A".into()),
    };
    let messages = vec![Message::user("hello")];
    let cfg = ChatConfig::default();
    let _ = with_router_context(ctx, async move {
        router_clone.chat(&messages, &[], &cfg).await.unwrap()
    })
    .await;

    let event = tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv())
        .await
        .expect("failover event timed out")
        .expect("broadcast channel closed");
    assert_eq!(event.from_provider, "primary/m1");
    assert_eq!(event.to_provider, "fallback/m2");
    assert_eq!(
        event.originating_session_id.as_deref(),
        Some("local:session-A"),
        "publisher must stamp originating session from ROUTER_CONTEXT"
    );
    assert_eq!(
        event.originating_turn_id.as_deref(),
        Some("turn-A"),
        "publisher must stamp originating turn from ROUTER_CONTEXT"
    );
}

/// Wave4-A (Codex P1): without a wrapping `ROUTER_CONTEXT` scope
/// (CLI smoke / test paths), the publisher falls back to `None` so
/// existing callers don't break.
#[tokio::test]
async fn failover_event_originating_id_is_none_outside_context_scope() {
    let config = AdaptiveConfig {
        failure_threshold: 5,
        probe_probability: 0.0,
        ..Default::default()
    };
    let router = AdaptiveRouter::new(
        vec![
            Arc::new(MockProvider {
                name: "primary",
                model: "m1",
                latency_ms: 0,
                fail: true,
                error_msg: "primary down",
            }),
            Arc::new(MockProvider {
                name: "fallback",
                model: "m2",
                latency_ms: 0,
                fail: false,
                error_msg: "",
            }),
        ],
        &[],
        config,
    );
    let mut rx = router.subscribe_failover();

    let _ = router
        .chat(&[Message::user("hi")], &[], &ChatConfig::default())
        .await
        .unwrap();

    let event = tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv())
        .await
        .expect("failover event timed out")
        .expect("broadcast channel closed");
    assert_eq!(
        event.originating_session_id, None,
        "outside ROUTER_CONTEXT scope, originating_session_id must be None"
    );
    assert_eq!(event.originating_turn_id, None);
}

// ── Auto-escalation tests (merged from #945) ──────────────────────────

/// Sustained slow turns on a single session promote the router to Hedge.
#[test]
fn auto_escalation_promotes_to_hedge_on_sustained_latency() {
    let router = auto_escalation_router();
    assert_eq!(router.mode(), AdaptiveMode::Lane);

    // Warmup: 5 fast samples to establish baseline ~100ms.
    for _ in 0..5 {
        let decision = router.record_turn_latency("s1", Duration::from_millis(100));
        assert_eq!(decision, AutoEscalationDecision::NoChange);
    }
    assert_eq!(router.mode(), AdaptiveMode::Lane);
    // Three slow turns (4x baseline > 3x threshold) → escalate on the third.
    for i in 0..3 {
        let decision = router.record_turn_latency("s1", Duration::from_millis(400));
        if i < 2 {
            assert_eq!(
                decision,
                AutoEscalationDecision::NoChange,
                "did not expect escalation at turn {i}"
            );
        }
    }
    assert_eq!(router.mode(), AdaptiveMode::Hedge);
}

/// Disabling the feature is a no-op even under sustained latency.
#[test]
fn auto_escalation_disabled_is_noop() {
    let router = auto_escalation_router();
    router.set_auto_escalation_config(AutoEscalationConfig {
        enabled: false,
        ..AutoEscalationConfig::default()
    });
    for _ in 0..5 {
        router.record_turn_latency("s1", Duration::from_millis(100));
    }
    for _ in 0..5 {
        router.record_turn_latency("s1", Duration::from_millis(400));
    }
    assert_eq!(
        router.mode(),
        AdaptiveMode::Lane,
        "router should not have escalated with auto_escalation disabled"
    );
}

/// Two different sessions track independently — slow turns on session A
/// do not pollute session B's window.
#[test]
fn auto_escalation_state_is_session_scoped() {
    let router = auto_escalation_router();
    // Warm both.
    for _ in 0..5 {
        router.record_turn_latency("s1", Duration::from_millis(100));
        router.record_turn_latency("s2", Duration::from_millis(100));
    }
    // s1 takes 3 slow turns → escalate.
    for _ in 0..3 {
        router.record_turn_latency("s1", Duration::from_millis(400));
    }
    assert_eq!(router.mode(), AdaptiveMode::Hedge);
    // But s2's observer should still be at consecutive_slow=0.
    // We verify indirectly: feed s2 ONE slow turn and confirm it does NOT
    // re-trigger escalation (router is already Hedge so trigger_escalate
    // is suppressed) — what we care about is that s2's baseline and slow
    // count are independent. Check via the helper accessors.
    let s1_baseline = router.session_latency_baseline("s1");
    let s2_baseline = router.session_latency_baseline("s2");
    assert!(s1_baseline.is_some());
    assert!(s2_baseline.is_some());
    assert_eq!(s2_baseline, Some(Duration::from_millis(100)));
    // s2 sample count = 5 (warmup only, fully consumed by window).
    // s1 sample count: window_size defaults to max(window_size,
    // baseline_samples) = 5, so 5 warmup + 3 slow = 8 records but the
    // observer's window caps at 5 (newest first). s2 stayed at 5.
    assert_eq!(router.session_latency_samples("s2"), 5);
    assert_eq!(router.session_latency_samples("s1"), 5);
}

/// Hysteresis: a single fast turn after escalation that is still above
/// `latency_ceiling_ms * recovery_factor` must NOT trigger recovery.
#[test]
fn auto_escalation_hysteresis_prevents_flapping() {
    let router = auto_escalation_router();
    router.set_auto_escalation_config(AutoEscalationConfig {
        // Tighter ceiling so the regression test is precise: with
        // ceiling=200, recovery_factor=0.6 → must be ≤120ms.
        latency_ceiling_ms: 200,
        recovery_factor: 0.6,
        ..AutoEscalationConfig::default()
    });
    // Warm at 100ms, then escalate via 3x400ms.
    for _ in 0..5 {
        router.record_turn_latency("s1", Duration::from_millis(100));
    }
    for _ in 0..3 {
        router.record_turn_latency("s1", Duration::from_millis(400));
    }
    assert_eq!(router.mode(), AdaptiveMode::Hedge);
    // One sample at 150ms (above ceiling*0.6=120ms but below baseline*3=300ms).
    // observer.should_deactivate() WOULD fire, but ceiling check suppresses.
    let decision = router.record_turn_latency("s1", Duration::from_millis(150));
    assert_eq!(
        decision,
        AutoEscalationDecision::NoChange,
        "expected hysteresis to suppress recovery at 150ms above ceiling*factor"
    );
    assert_eq!(router.mode(), AdaptiveMode::Hedge);
    // Now a sample below the recovery ceiling → recover.
    let decision = router.record_turn_latency("s1", Duration::from_millis(50));
    assert_eq!(decision, AutoEscalationDecision::Deescalated);
    assert_eq!(router.mode(), AdaptiveMode::Lane);
}

/// Recovery restores the pre-escalation mode (not just Off).
#[test]
fn auto_escalation_restores_previous_mode() {
    let router = auto_escalation_router();
    // Start in Lane.
    router.set_mode(AdaptiveMode::Lane);
    for _ in 0..5 {
        router.record_turn_latency("s1", Duration::from_millis(100));
    }
    for _ in 0..3 {
        router.record_turn_latency("s1", Duration::from_millis(400));
    }
    assert_eq!(router.mode(), AdaptiveMode::Hedge);
    router.record_turn_latency("s1", Duration::from_millis(50));
    assert_eq!(
        router.mode(),
        AdaptiveMode::Lane,
        "router should restore the pre-escalation mode (Lane), not Off"
    );
}

/// Callback fires on escalate AND deescalate with full event payload.
#[test]
fn auto_escalation_callback_fires_on_both_edges() {
    let router = auto_escalation_router();
    let captured: Arc<Mutex<Vec<AutoEscalationEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let recv = captured.clone();
    router.set_auto_escalation_callback(Some(Arc::new(move |e| {
        recv.lock().unwrap().push(e.clone());
    })));
    for _ in 0..5 {
        router.record_turn_latency("s1", Duration::from_millis(100));
    }
    for _ in 0..3 {
        router.record_turn_latency("s1", Duration::from_millis(400));
    }
    router.record_turn_latency("s1", Duration::from_millis(50));
    let events = captured.lock().unwrap();
    assert_eq!(events.len(), 2, "expected 2 callback fires (esc + de-esc)");
    assert!(events[0].escalated);
    assert_eq!(events[0].new_mode, AdaptiveMode::Hedge);
    assert_eq!(events[0].previous_mode, AdaptiveMode::Lane);
    assert!(!events[1].escalated);
    assert_eq!(events[1].new_mode, AdaptiveMode::Lane);
    assert_eq!(events[1].previous_mode, AdaptiveMode::Hedge);
}

/// 4-turn fake slow run still does NOT escalate (slow_trigger default = 3).
/// The single-sample boundary is exercised by `forget_session`.
#[test]
fn auto_escalation_forget_session_drops_state() {
    let router = auto_escalation_router();
    for _ in 0..5 {
        router.record_turn_latency("s1", Duration::from_millis(100));
    }
    assert!(router.session_latency_baseline("s1").is_some());
    assert!(router.forget_session("s1"));
    assert!(router.session_latency_baseline("s1").is_none());
    assert!(!router.forget_session("s1"));
}

/// Codex review P1.2: if a session exits while still escalated,
/// forget_session restores the router mode so we don't get stuck in
/// Hedge with no record of how to recover.
#[test]
fn auto_escalation_forget_session_restores_mode() {
    let router = auto_escalation_router();
    assert_eq!(router.mode(), AdaptiveMode::Lane);
    for _ in 0..5 {
        router.record_turn_latency("s1", Duration::from_millis(100));
    }
    for _ in 0..3 {
        router.record_turn_latency("s1", Duration::from_millis(400));
    }
    assert_eq!(router.mode(), AdaptiveMode::Hedge);
    // s1 exits while still escalated.
    router.forget_session("s1");
    assert_eq!(
        router.mode(),
        AdaptiveMode::Lane,
        "router should restore the pre-escalation mode when the escalating session is forgotten"
    );
}

/// Codex review P1.3: if the operator manually moves the router off
/// Hedge (`/adaptive off|lane`) during an active escalation, a
/// subsequent fast turn must NOT override the operator's choice via
/// the cached pre_escalation_mode.
#[test]
fn auto_escalation_respects_operator_override() {
    let router = auto_escalation_router();
    for _ in 0..5 {
        router.record_turn_latency("s1", Duration::from_millis(100));
    }
    for _ in 0..3 {
        router.record_turn_latency("s1", Duration::from_millis(400));
    }
    assert_eq!(router.mode(), AdaptiveMode::Hedge);
    // Operator decides to force the router off (e.g. costs).
    router.set_mode(AdaptiveMode::Off);
    // A fast turn arrives — recovery would normally restore Lane.
    router.record_turn_latency("s1", Duration::from_millis(50));
    assert_eq!(
        router.mode(),
        AdaptiveMode::Off,
        "router should respect the operator's manual override and not restore the pre-escalation mode"
    );
}

/// Codex review P1.4: a session whose baseline drifts up to e.g. 5s
/// will not normally consider 8s "slow" (8 < 5*3=15). The
/// `latency_ceiling_ms` config knob must still trigger escalation
/// when an absolute ceiling is exceeded.
#[test]
fn auto_escalation_latency_ceiling_triggers_escalation() {
    let router = auto_escalation_router();
    // Configure a tight ceiling: 1500ms.
    router.set_auto_escalation_config(AutoEscalationConfig {
        latency_ceiling_ms: 1_500,
        recovery_factor: 0.6,
        // Keep slow_trigger=3 so the test mirrors gateway defaults.
        ..AutoEscalationConfig::default()
    });
    // Warm a high baseline at 1s so 3x baseline = 3s > 1.5s ceiling.
    // The legacy baseline-only logic would NOT fire on 2s samples
    // (2 < 3) — only the ceiling-aware path catches them.
    for _ in 0..5 {
        router.record_turn_latency("s1", Duration::from_millis(1_000));
    }
    // 3 samples at 2s: each is below 3x baseline (3s) but above
    // ceiling (1.5s) → must escalate.
    for _ in 0..3 {
        router.record_turn_latency("s1", Duration::from_millis(2_000));
    }
    assert_eq!(
        router.mode(),
        AdaptiveMode::Hedge,
        "router should have escalated on the latency_ceiling_ms path"
    );
}

// ──────────────────────────────────────────────────────────────────
// RFC-3 (#1292) — lane-aware provider selection
// ──────────────────────────────────────────────────────────────────

#[tokio::test]
async fn rfc3_lane_filter_steers_chat_to_first_matching_candidate() {
    // Build a 3-slot router so the lane filter has meaningful
    // narrowing to do (priority order [primary, code, strong]).
    let router = AdaptiveRouter::new(
        vec![
            Arc::new(MockProvider {
                name: "openrouter",
                model: "gpt-4o-mini",
                latency_ms: 0,
                fail: false,
                error_msg: "",
            }),
            Arc::new(MockProvider {
                name: "deepseek",
                model: "deepseek-coder",
                latency_ms: 0,
                fail: false,
                error_msg: "",
            }),
            Arc::new(MockProvider {
                name: "anthropic",
                model: "claude-sonnet-4-6",
                latency_ms: 0,
                fail: false,
                error_msg: "",
            }),
        ],
        &[],
        AdaptiveConfig {
            probe_probability: 0.0,
            ..Default::default()
        },
    );
    // Default mode is Off → priority order; pre-lane this would
    // pick `openrouter` (index 0).
    let baseline = router.chat(&[], &[], &ChatConfig::default()).await.unwrap();
    assert_eq!(baseline.content.as_deref(), Some("from-openrouter"));

    // Now scope a CodeCapable lane — the first lane candidate is
    // `(anthropic, claude-sonnet-4-6)`, so the router should
    // route to anthropic even though its priority is lowest.
    let ctx = crate::LaneContext::for_topic(Some("code:refactor"), None);
    let resp = crate::with_lane_context(ctx, async {
        router.chat(&[], &[], &ChatConfig::default()).await
    })
    .await
    .unwrap();
    assert_eq!(resp.content.as_deref(), Some("from-anthropic"));
}

#[tokio::test]
async fn rfc3_lane_filter_unknown_topic_preserves_default_behavior() {
    // Built-in default for `chat:*` is `General` → no filter.
    let router = AdaptiveRouter::new(
        vec![
            Arc::new(MockProvider {
                name: "wisemodel",
                model: "kimi-k2.6",
                latency_ms: 0,
                fail: false,
                error_msg: "",
            }),
            Arc::new(MockProvider {
                name: "anthropic",
                model: "claude-sonnet-4-6",
                latency_ms: 0,
                fail: false,
                error_msg: "",
            }),
        ],
        &[],
        AdaptiveConfig {
            probe_probability: 0.0,
            ..Default::default()
        },
    );
    let ctx = crate::LaneContext::for_topic(Some("chat:hello"), None);
    let resp = crate::with_lane_context(ctx, async {
        router.chat(&[], &[], &ChatConfig::default()).await
    })
    .await
    .unwrap();
    // General lane = no filter, so priority-0 (wisemodel) wins.
    assert_eq!(resp.content.as_deref(), Some("from-wisemodel"));
}

#[tokio::test]
async fn rfc3_lane_filter_zero_matches_falls_through_to_full_chain() {
    // None of the registered providers match the InstructionStrong
    // defaults — the filter must not starve the router.
    let router = AdaptiveRouter::new(
        vec![
            Arc::new(MockProvider {
                name: "custom-provider",
                model: "custom-model",
                latency_ms: 0,
                fail: false,
                error_msg: "",
            }),
            Arc::new(MockProvider {
                name: "other-provider",
                model: "other-model",
                latency_ms: 0,
                fail: false,
                error_msg: "",
            }),
        ],
        &[],
        AdaptiveConfig {
            probe_probability: 0.0,
            ..Default::default()
        },
    );
    let ctx = crate::LaneContext::for_topic(Some("slides:demo"), None);
    let resp = crate::with_lane_context(ctx, async {
        router.chat(&[], &[], &ChatConfig::default()).await
    })
    .await
    .unwrap();
    // Zero lane matches → priority-0 wins (no starvation).
    assert_eq!(resp.content.as_deref(), Some("from-custom-provider"));
}

#[tokio::test]
async fn rfc3_circuit_breaker_open_on_first_lane_candidate_falls_through_to_second() {
    // Lane has 2 candidates; the first is failing and trips its
    // circuit, the second is healthy.
    let router = AdaptiveRouter::new(
        vec![
            // Slot 0 — fast-chat lane's first candidate, failing.
            Arc::new(MockProvider {
                name: "wisemodel",
                model: "kimi-k2.6",
                latency_ms: 0,
                fail: true,
                error_msg: "503 down",
            }),
            // Slot 1 — fast-chat lane's second candidate, healthy.
            Arc::new(MockProvider {
                name: "deepseek",
                model: "deepseek-chat",
                latency_ms: 0,
                fail: false,
                error_msg: "",
            }),
            // Slot 2 — outside any lane, low-priority backstop.
            Arc::new(MockProvider {
                name: "anthropic",
                model: "claude-sonnet-4-6",
                latency_ms: 0,
                fail: false,
                error_msg: "",
            }),
        ],
        &[],
        AdaptiveConfig {
            probe_probability: 0.0,
            failure_threshold: 1,
            ..Default::default()
        },
    );
    // Trip the circuit on slot 0 first via a non-lane call.
    let _ = router.chat(&[], &[], &ChatConfig::default()).await;
    assert!(router.slots[0].metrics.is_circuit_open(1));

    // Now lane scope `FastChat` — the first candidate is
    // circuit-open, so the router should advance to the second
    // (`deepseek-chat`) rather than fall through to anthropic.
    let mut cfg = crate::LaneRoutingConfig::default();
    cfg.topic_lanes
        .insert("loop".to_string(), crate::Lane::FastChat);
    let ctx = crate::LaneContext::for_topic(Some("loop:test"), Some(&cfg));
    let resp = crate::with_lane_context(ctx, async {
        router.chat(&[], &[], &ChatConfig::default()).await
    })
    .await
    .unwrap();
    assert_eq!(resp.content.as_deref(), Some("from-deepseek"));
}

#[tokio::test]
async fn rfc3_lane_filter_general_is_no_op() {
    // A `General` lane has no candidates and must not change
    // anything about how the router behaves vs. an absent scope.
    let router = AdaptiveRouter::new(
        vec![
            Arc::new(MockProvider {
                name: "primary",
                model: "m1",
                latency_ms: 0,
                fail: false,
                error_msg: "",
            }),
            Arc::new(MockProvider {
                name: "fallback",
                model: "m2",
                latency_ms: 0,
                fail: false,
                error_msg: "",
            }),
        ],
        &[],
        AdaptiveConfig {
            probe_probability: 0.0,
            ..Default::default()
        },
    );
    let ctx = crate::LaneContext {
        lane: Some(crate::Lane::General),
        config: None,
    };
    let resp = crate::with_lane_context(ctx, async {
        router.chat(&[], &[], &ChatConfig::default()).await
    })
    .await
    .unwrap();
    assert_eq!(resp.content.as_deref(), Some("from-primary"));
}

#[tokio::test]
async fn rfc3_lane_filter_normalizes_endpoint_tagged_provider_names() {
    // Codex P2 follow-up: profiles using OpenAI-compatible
    // providers carry endpoint-tagged labels (e.g.
    // `wisemodel@autodl`). The lane filter normalizes by
    // stripping the `@suffix` before matching against lane
    // candidate strings.
    let router = AdaptiveRouter::new(
        vec![
            // Out-of-lane (untagged).
            Arc::new(MockProvider {
                name: "openrouter",
                model: "gpt-4o-mini",
                latency_ms: 0,
                fail: false,
                error_msg: "",
            }),
            // In-lane after `@` strip: `wisemodel@autodl` →
            // `wisemodel` matches the FastChat default.
            Arc::new(MockProvider {
                name: "wisemodel@autodl",
                model: "kimi-k2.6",
                latency_ms: 0,
                fail: false,
                error_msg: "",
            }),
        ],
        &[],
        AdaptiveConfig {
            probe_probability: 0.0,
            ..Default::default()
        },
    );
    let mut cfg = crate::LaneRoutingConfig::default();
    cfg.topic_lanes.insert("loop".into(), crate::Lane::FastChat);
    let ctx = crate::LaneContext::for_topic(Some("loop:t"), Some(&cfg));
    let resp = crate::with_lane_context(ctx, async {
        router.chat(&[], &[], &ChatConfig::default()).await
    })
    .await
    .unwrap();
    // Should pick the tagged wisemodel slot, not the
    // out-of-lane openrouter slot.
    assert_eq!(resp.content.as_deref(), Some("from-wisemodel@autodl"));
}

#[tokio::test]
async fn rfc3_tagged_candidate_in_override_matches_only_tagged_slot() {
    // Codex P2 follow-up #2: a profile override that names an
    // endpoint-tagged label (e.g. `moonshot@autodl`) MUST pin to
    // exactly that slot — the untagged `moonshot` slot or a
    // differently-tagged `moonshot@other` slot should NOT
    // satisfy the override.
    let router = AdaptiveRouter::new(
        vec![
            Arc::new(MockProvider {
                name: "moonshot",
                model: "k2",
                latency_ms: 0,
                fail: false,
                error_msg: "",
            }),
            Arc::new(MockProvider {
                name: "moonshot@autodl",
                model: "k2",
                latency_ms: 0,
                fail: false,
                error_msg: "",
            }),
        ],
        &[],
        AdaptiveConfig {
            probe_probability: 0.0,
            ..Default::default()
        },
    );
    let mut cfg = crate::LaneRoutingConfig::default();
    cfg.topic_lanes
        .insert("loop".to_string(), crate::Lane::CodeCapable);
    cfg.lane_models.insert(
        crate::Lane::CodeCapable,
        vec![("moonshot@autodl".to_string(), "k2".to_string())],
    );
    let ctx = crate::LaneContext::for_topic(Some("loop:test"), Some(&cfg));
    let resp = crate::with_lane_context(ctx, async {
        router.chat(&[], &[], &ChatConfig::default()).await
    })
    .await
    .unwrap();
    // Tagged candidate MUST pick the tagged slot, not the
    // untagged primary.
    assert_eq!(resp.content.as_deref(), Some("from-moonshot@autodl"));
}

#[tokio::test]
async fn rfc3_hedge_confines_alternate_to_lane_when_filter_active() {
    // Codex P2 follow-up: under `AdaptiveMode::Hedge`, the
    // alternate hedge target must be in-lane. Build a chain
    // where the lane candidate is a SLOW out-of-priority slot
    // and a non-lane slot is fast: without the fix, hedge would
    // race the fast non-lane slot and return its content; with
    // the fix, hedge skips when no in-lane alternate is healthy
    // and the single-provider path runs against the primary.
    //
    // Topology:
    //   slot 0 primary = `anthropic / claude-sonnet-4-6` (in lane, fast)
    //   slot 1 alt     = `openrouter / out-of-lane` (fast, NOT in lane)
    // Expectation: hedge candidate set is empty (the only
    // out-of-lane slot is filtered out), so hedge falls through
    // and the response comes from the primary.
    let router = AdaptiveRouter::new(
        vec![
            Arc::new(MockProvider {
                name: "anthropic",
                model: "claude-sonnet-4-6",
                latency_ms: 5,
                fail: false,
                error_msg: "",
            }),
            Arc::new(MockProvider {
                name: "openrouter",
                model: "out-of-lane",
                latency_ms: 5,
                fail: false,
                error_msg: "",
            }),
        ],
        &[],
        AdaptiveConfig {
            probe_probability: 0.0,
            ..Default::default()
        },
    );
    router.set_mode(AdaptiveMode::Hedge);

    let ctx = crate::LaneContext::for_topic(Some("slides:demo"), None);
    let resp = crate::with_lane_context(ctx, async {
        router.chat(&[], &[], &ChatConfig::default()).await
    })
    .await
    .unwrap();
    // Lane filter excludes openrouter from the hedge alternate
    // set, so the only hedge candidate is none → fall through
    // to single-provider path against the primary (anthropic
    // is in-lane).
    assert_eq!(resp.content.as_deref(), Some("from-anthropic"));
}

// ── FailFast policy tests ─────────────────────────────────────────────

/// FailFast must skip hedged_chat entirely: two successful providers
/// under Hedge mode should see a combined call count of exactly 1.
#[tokio::test]
async fn should_call_single_provider_when_failfast_in_hedge_mode() {
    use crate::{LlmCallPolicy, with_llm_call_policy};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CountingProvider {
        name: &'static str,
        calls: Arc<AtomicUsize>,
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
            Ok(ChatResponse {
                content: Some(format!("from-{}", self.name)),
                reasoning_content: None,
                tool_calls: vec![],
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage::default(),
                provider_index: None,
            })
        }

        fn model_id(&self) -> &str {
            "m1"
        }

        fn provider_name(&self) -> &str {
            self.name
        }
    }

    let calls0 = Arc::new(AtomicUsize::new(0));
    let calls1 = Arc::new(AtomicUsize::new(0));

    let router = AdaptiveRouter::new(
        vec![
            Arc::new(CountingProvider {
                name: "provider-a",
                calls: calls0.clone(),
            }),
            Arc::new(CountingProvider {
                name: "provider-b",
                calls: calls1.clone(),
            }),
        ],
        &[],
        AdaptiveConfig {
            probe_probability: 0.0,
            ..Default::default()
        },
    );
    router.set_mode(AdaptiveMode::Hedge);

    let _ = with_llm_call_policy(LlmCallPolicy::FailFast, async {
        router.chat(&[], &[], &ChatConfig::default()).await
    })
    .await;

    let total = calls0.load(Ordering::SeqCst) + calls1.load(Ordering::SeqCst);
    assert_eq!(
        total, 1,
        "FailFast must skip hedged_chat (no proactive double-call); got {total}"
    );
}

/// FailFast must NOT failover after an error: with two providers where
/// the first fails, FailFast should return the error without trying
/// the second provider.
#[tokio::test]
async fn should_not_failover_when_failfast_and_primary_fails() {
    use crate::{LlmCallPolicy, with_llm_call_policy};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CountingProvider {
        name: &'static str,
        fail: bool,
        calls: Arc<AtomicUsize>,
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
            if self.fail {
                eyre::bail!("500 server error from {}", self.name);
            }
            Ok(ChatResponse {
                content: Some(format!("from-{}", self.name)),
                reasoning_content: None,
                tool_calls: vec![],
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage::default(),
                provider_index: None,
            })
        }

        fn model_id(&self) -> &str {
            "m1"
        }

        fn provider_name(&self) -> &str {
            self.name
        }
    }

    let calls0 = Arc::new(AtomicUsize::new(0));
    let calls1 = Arc::new(AtomicUsize::new(0));

    let router = AdaptiveRouter::new(
        vec![
            Arc::new(CountingProvider {
                name: "failing-primary",
                fail: true,
                calls: calls0.clone(),
            }),
            Arc::new(CountingProvider {
                name: "good-fallback",
                fail: false,
                calls: calls1.clone(),
            }),
        ],
        &[],
        AdaptiveConfig {
            probe_probability: 0.0,
            ..Default::default()
        },
    );

    let result = with_llm_call_policy(LlmCallPolicy::FailFast, async {
        router.chat(&[], &[], &ChatConfig::default()).await
    })
    .await;

    assert!(result.is_err(), "FailFast should propagate the error");
    assert_eq!(
        calls0.load(Ordering::SeqCst),
        1,
        "primary should be called exactly once"
    );
    assert_eq!(
        calls1.load(Ordering::SeqCst),
        0,
        "FailFast must NOT call fallback provider"
    );
}

/// #2135 round-6 P1: sizing accessors take the minimum across slots
/// (selection-independent — safe for hedge/probe/failover routing), and
/// the identity accessors are DETERMINISTIC (best-scored slot, no
/// stochastic exploration).
#[tokio::test]
async fn test_sizing_is_min_across_slots_and_identity_is_deterministic() {
    let big: Arc<dyn LlmProvider> = Arc::new(crate::ContextWindowOverride::new(
        Arc::new(MockProvider {
            name: "primary",
            model: "m1",
            latency_ms: 0,
            fail: false,
            error_msg: "",
        }),
        262_144,
    ));
    let small: Arc<dyn LlmProvider> = Arc::new(crate::ContextWindowOverride::new(
        Arc::new(MockProvider {
            name: "fallback",
            model: "m2",
            latency_ms: 0,
            fail: false,
            error_msg: "",
        }),
        32_768,
    ));
    let router = AdaptiveRouter::new(
        vec![big, small],
        &[],
        AdaptiveConfig {
            // Exploration ON: identity must be stable regardless.
            probe_probability: 0.5,
            ..Default::default()
        },
    );
    assert_eq!(router.context_window(), 32_768, "min across slots");
    let first = router.model_id().to_string();
    for _ in 0..50 {
        assert_eq!(
            router.model_id(),
            first,
            "identity must not flip stochastically"
        );
    }
    // #2135 round-7 P1: the deterministic selection must pick the BEST
    // slot (ascending score, same rules as routing) — an earlier cut used
    // max_by over a lower-is-better score and preferred the worst lane.
    // With identical cold providers, priority ordering makes the PRIMARY
    // the correct choice.
    assert_eq!(first, "m1", "cold identical slots must select the primary");
}

/// #2143 part 1: INSIDE a turn scope, the sizing accessors resolve to the ONE
/// pinned route's window (not the conservative min), and the pin is stable for
/// the whole turn — so a prompt is sized for the exact route the send takes.
#[tokio::test]
async fn turn_pin_sizes_for_the_pinned_route_not_the_min() {
    let big: Arc<dyn LlmProvider> = Arc::new(crate::ContextWindowOverride::new(
        Arc::new(MockProvider {
            name: "primary",
            model: "m1",
            latency_ms: 0,
            fail: false,
            error_msg: "",
        }),
        262_144,
    ));
    let small: Arc<dyn LlmProvider> = Arc::new(crate::ContextWindowOverride::new(
        Arc::new(MockProvider {
            name: "fallback",
            model: "m2",
            latency_ms: 0,
            fail: false,
            error_msg: "",
        }),
        32_768,
    ));
    let router = Arc::new(AdaptiveRouter::new(
        vec![big, small],
        &[],
        AdaptiveConfig {
            // No stochastic probe so the pin resolves to the deterministic
            // primary and the assertion is stable.
            probe_probability: 0.0,
            ..Default::default()
        },
    ));

    // Outside a turn: the pre-#2143 conservative min-across-slots envelope.
    assert_eq!(
        router.context_window(),
        32_768,
        "unpinned sizing is the min"
    );

    // Inside a turn: pinned to the deterministic primary → its full window,
    // stable across repeated sizing/identity calls.
    let r = router.clone();
    with_router_context(RouterContext::default(), async move {
        assert_eq!(
            r.context_window(),
            262_144,
            "turn sizing must use the pinned route's window"
        );
        for _ in 0..10 {
            assert_eq!(
                r.context_window(),
                262_144,
                "the pin is stable for the turn"
            );
        }
        assert_eq!(r.max_output_tokens(), r.max_output_tokens());
        assert_eq!(r.model_id(), "m1", "identity resolves to the pinned route");
        r.ensure_ready().await;
    })
    .await;

    // A fresh turn re-resolves the pin (no leakage across turns).
    let r2 = router.clone();
    with_router_context(RouterContext::default(), async move {
        assert_eq!(r2.context_window(), 262_144);
    })
    .await;
}

/// #2135 round-8 P1: every adaptive dispatch passes the route-fit guard —
/// a lane that resolves too small at dispatch is skipped (its chat never
/// invoked), failover serves from a fitting lane, and the skip must NOT
/// poison the skipped lane's circuit breaker.
#[tokio::test]
async fn test_unfit_lane_is_skipped_without_breaker_pollution() {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    struct LateSmall {
        resolved: AtomicBool,
        calls: Arc<AtomicUsize>,
    }
    #[async_trait::async_trait]
    impl LlmProvider for LateSmall {
        async fn chat(
            &self,
            _m: &[Message],
            _t: &[ToolSpec],
            _c: &ChatConfig,
        ) -> eyre::Result<ChatResponse> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(ChatResponse {
                content: Some("from-small".into()),
                reasoning_content: None,
                tool_calls: vec![],
                stop_reason: crate::StopReason::EndTurn,
                usage: TokenUsage::default(),
                provider_index: None,
            })
        }
        fn model_id(&self) -> &str {
            "m-small"
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

    let small_calls = Arc::new(AtomicUsize::new(0));
    let router = AdaptiveRouter::new(
        vec![
            Arc::new(LateSmall {
                resolved: AtomicBool::new(false),
                calls: small_calls.clone(),
            }),
            Arc::new(MockProvider {
                name: "fallback",
                model: "m2",
                latency_ms: 0,
                fail: false,
                error_msg: "",
            }),
        ],
        &[],
        AdaptiveConfig {
            probe_probability: 0.0,
            ..Default::default()
        },
    );
    let big = Message::user("x ".repeat(20_000));
    for _ in 0..4 {
        let resp = router
            .chat(std::slice::from_ref(&big), &[], &ChatConfig::default())
            .await
            .expect("the fitting lane must serve the request");
        assert_eq!(resp.content.unwrap(), "from-fallback");
    }
    assert_eq!(
        small_calls.load(Ordering::SeqCst),
        0,
        "unfit lane must never be dispatched for oversized prompts"
    );
    // Breaker not poisoned: for a SMALL prompt the lane fits again and —
    // being the deterministic primary — must serve it. Four unfit skips
    // above exceed the default failure threshold, so if skips recorded
    // failures the breaker would be open and this would route to the
    // fallback instead.
    let resp = router
        .chat(&[Message::user("hi")], &[], &ChatConfig::default())
        .await
        .expect("small prompt must succeed");
    assert_eq!(
        resp.content.unwrap(),
        "from-small",
        "skips must not open the unfit lane's breaker"
    );
}
