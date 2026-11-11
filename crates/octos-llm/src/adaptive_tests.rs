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

fn mock_provider(
    name: &'static str,
    model: &'static str,
    latency_ms: u64,
    fail: bool,
    error_msg: &'static str,
) -> Arc<dyn LlmProvider> {
    Arc::new(MockProvider {
        name,
        model,
        latency_ms,
        fail,
        error_msg,
    })
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

fn hedged_router(providers: Vec<Arc<dyn LlmProvider>>) -> AdaptiveRouter {
    let config = AdaptiveConfig {
        probe_probability: 0.0,
        ..Default::default()
    };
    let router = AdaptiveRouter::new(providers, &[], config);
    router.set_mode(AdaptiveMode::Hedge);
    router
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
            mock_provider("primary", "m1", 0, true, "Primary"),
            mock_provider("fallback", "m2", 0, false, ""),
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
async fn test_hedged_racing_picks_faster_provider() {
    let router = hedged_router(vec![
        mock_provider("slow-primary", "m1", 200, false, ""),
        mock_provider("fast-fallback", "m2", 10, false, ""),
    ]);

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
    let router = hedged_router(vec![
        mock_provider("failing-primary", "m1", 0, true, "Primary"),
        mock_provider("good-fallback", "m2", 10, false, ""),
    ]);

    let resp = router.chat(&[], &[], &ChatConfig::default()).await.unwrap();
    assert_eq!(resp.content.as_deref(), Some("from-good-fallback"));
}

// ── Auto-escalation tests ─────────────────────────────────────────────

/// Helper: build a 2-provider router with permissive defaults so we can
/// drive the auto-escalation state machine in isolation.
fn auto_escalation_router() -> AdaptiveRouter {
    let providers: Vec<Arc<dyn LlmProvider>> = vec![
        mock_provider("primary", "m1", 0, false, ""),
        mock_provider("fallback", "m2", 0, false, ""),
    ];
    AdaptiveRouter::new(providers, &[], AdaptiveConfig::default())
        .with_adaptive_config(AdaptiveMode::Lane, false)
}

// ------------------------------------------------------------------
// Wave4-A: failover broadcast channel tests.
// ------------------------------------------------------------------

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
        vec![mock_provider("p1", "m1", 0, false, "")],
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

// ── Auto-escalation tests (merged from #945) ──────────────────────────

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

// ──────────────────────────────────────────────────────────────────
// RFC-3 (#1292) — lane-aware provider selection
// ──────────────────────────────────────────────────────────────────

// ── FailFast policy tests ─────────────────────────────────────────────
