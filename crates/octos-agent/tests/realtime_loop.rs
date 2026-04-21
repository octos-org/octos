//! RP05 acceptance tests: realtime heartbeat + sensor context injection
//! wired to the agent loop.
//!
//! These tests exercise the public surface exported from `octos_agent`. They
//! do NOT touch the LLM provider (all providers are mocks) and do not sleep
//! in real time: stall detection uses the `Heartbeat::force_stall_for_test`
//! helper so the suite stays fast.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use eyre::Result;
use octos_agent::{
    Agent, AgentConfig, AgentError, Heartbeat, HeartbeatState, RealtimeConfig, RealtimeController,
    RealtimeHookEnricher, SensorContextInjector, SensorSnapshot, SensorSource, ToolRegistry,
};
use octos_core::{AgentId, Message};
use octos_llm::{ChatConfig, ChatResponse, LlmProvider, StopReason, ToolSpec};
use octos_memory::EpisodeStore;

// ---------- Shared helpers ----------

/// Provider that returns `EndTurn` after a fixed number of calls. Captures
/// the system prompt each invocation so tests can inspect injection.
struct RecordingProvider {
    calls: AtomicUsize,
    total_turns: usize,
    last_system_prompt: Mutex<String>,
}

#[async_trait]
impl LlmProvider for RecordingProvider {
    async fn chat(
        &self,
        messages: &[Message],
        _tools: &[ToolSpec],
        _config: &ChatConfig,
    ) -> Result<ChatResponse> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        if let Some(first) = messages.first() {
            *self.last_system_prompt.lock().unwrap() = first.content.clone();
        }
        let _ = self.total_turns; // retained for future multi-turn tests
        Ok(ChatResponse {
            content: Some(format!("turn {call}")),
            reasoning_content: None,
            tool_calls: vec![],
            stop_reason: StopReason::EndTurn,
            usage: Default::default(),
            provider_index: None,
        })
    }

    fn model_id(&self) -> &str {
        "mock"
    }

    fn provider_name(&self) -> &str {
        "mock"
    }
}

/// Provider that loops N tool calls before ending. Used to exercise >1
/// iterations in a turn so heartbeat counts beyond 1.
struct ToolLoopProvider {
    calls: AtomicUsize,
    tool_iterations: usize,
}

#[async_trait]
impl LlmProvider for ToolLoopProvider {
    async fn chat(
        &self,
        _messages: &[Message],
        _tools: &[ToolSpec],
        _config: &ChatConfig,
    ) -> Result<ChatResponse> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        if call < self.tool_iterations {
            Ok(ChatResponse {
                content: None,
                reasoning_content: None,
                tool_calls: vec![octos_core::ToolCall {
                    id: format!("call_noop_{call}"),
                    name: "noop".into(),
                    arguments: serde_json::json!({}),
                    metadata: None,
                }],
                stop_reason: StopReason::ToolUse,
                usage: Default::default(),
                provider_index: None,
            })
        } else {
            Ok(ChatResponse {
                content: Some("done".into()),
                reasoning_content: None,
                tool_calls: vec![],
                stop_reason: StopReason::EndTurn,
                usage: Default::default(),
                provider_index: None,
            })
        }
    }

    fn model_id(&self) -> &str {
        "mock"
    }

    fn provider_name(&self) -> &str {
        "mock"
    }
}

struct NoopTool;

#[async_trait]
impl octos_agent::Tool for NoopTool {
    fn name(&self) -> &str {
        "noop"
    }

    fn description(&self) -> &str {
        "no-op tool for loop heartbeat tests"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}})
    }

    async fn execute(&self, _args: &serde_json::Value) -> Result<octos_agent::ToolResult> {
        Ok(octos_agent::ToolResult {
            output: "ok".into(),
            success: true,
            ..Default::default()
        })
    }
}

/// Pluggable sensor source used by tests — holds a snapshot vector behind a
/// `Mutex` so each test can mutate it freely.
#[derive(Default)]
struct MockBus {
    snaps: Mutex<Vec<SensorSnapshot>>,
}

impl MockBus {
    fn with_snaps(snaps: Vec<SensorSnapshot>) -> Arc<Self> {
        Arc::new(Self {
            snaps: Mutex::new(snaps),
        })
    }
}

impl SensorSource for MockBus {
    fn latest_snapshots(&self) -> Vec<SensorSnapshot> {
        self.snaps.lock().unwrap().clone()
    }
}

/// Stalls the next call to `latest_snapshots` with an empty vec — simulates
/// a dead ROS/dora-rs bus that should degrade silently.
struct StalledBus;

impl SensorSource for StalledBus {
    fn latest_snapshots(&self) -> Vec<SensorSnapshot> {
        Vec::new()
    }
}

async fn test_agent(
    provider: Arc<dyn LlmProvider>,
    controller: Option<Arc<RealtimeController>>,
) -> Agent {
    let dir = tempfile::tempdir().unwrap();
    let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
    let mut tools = ToolRegistry::new();
    tools.register(NoopTool);
    let mut agent = Agent::new(AgentId::new("test"), provider, tools, memory)
        .with_config(AgentConfig::default());
    if let Some(ctrl) = controller {
        agent = agent.with_realtime(ctrl);
    }
    agent
}

// ---------- Acceptance tests ----------

#[tokio::test]
async fn should_beat_heartbeat_once_per_loop_iteration() {
    let provider: Arc<dyn LlmProvider> = Arc::new(ToolLoopProvider {
        calls: AtomicUsize::new(0),
        tool_iterations: 3,
    });
    let controller = Arc::new(RealtimeController::new(RealtimeConfig {
        enabled: true,
        heartbeat_timeout_ms: 30_000,
        ..Default::default()
    }));
    let agent = test_agent(provider.clone(), Some(controller.clone())).await;

    let before = controller.heartbeat().count();
    let _ = agent.process_message("hi", &[], vec![]).await.unwrap();
    let after = controller.heartbeat().count();

    // Provider was called 4 times (3 tool iterations + 1 final EndTurn call).
    // Each iteration beats the heartbeat exactly once.
    let iterations = 4u32;
    assert_eq!(
        after - before,
        iterations,
        "expected {iterations} heartbeat beats, got {}",
        after - before
    );
}

#[tokio::test]
async fn should_abort_iteration_when_heartbeat_stalled() {
    // Use a tight timeout so we can force a stall.
    let config = RealtimeConfig {
        enabled: true,
        heartbeat_timeout_ms: 1,
        ..Default::default()
    };
    let controller = Arc::new(RealtimeController::new(config));

    let provider: Arc<dyn LlmProvider> = Arc::new(RecordingProvider {
        calls: AtomicUsize::new(0),
        total_turns: 1,
        last_system_prompt: Mutex::new(String::new()),
    });
    let agent = test_agent(provider, Some(controller.clone())).await;

    // Force a stall before the loop runs by making the heartbeat think its
    // last beat was ages ago AND syncing the check marker so state()
    // immediately returns Stalled.
    controller
        .heartbeat()
        .force_stall_for_test(Duration::from_millis(100));

    // The first iteration should abort with the typed error.
    let err = agent
        .process_message("hi", &[], vec![])
        .await
        .expect_err("expected heartbeat stall error");

    let agent_err = err.downcast_ref::<AgentError>();
    assert!(
        matches!(agent_err, Some(AgentError::HeartbeatStalled { .. })),
        "expected HeartbeatStalled error, got {:?}",
        err
    );
}

#[tokio::test]
async fn should_inject_sensor_summary_within_budget() {
    let bus = MockBus::with_snaps(vec![
        SensorSnapshot {
            sensor_id: "joint_positions".into(),
            value: serde_json::json!([0.0, 0.5]),
            timestamp_ms: 1000,
        },
        SensorSnapshot {
            sensor_id: "battery".into(),
            value: serde_json::json!({"soc_pct": 78}),
            timestamp_ms: 1000,
        },
    ]);
    let injector = Arc::new(SensorContextInjector::with_source(
        8,
        bus.clone() as Arc<dyn SensorSource>,
    ));
    let controller = Arc::new(
        RealtimeController::new(RealtimeConfig {
            enabled: true,
            heartbeat_timeout_ms: 30_000,
            sensor_budget_tokens: 256,
            ..Default::default()
        })
        .with_injector(injector.clone()),
    );

    let provider_inner = Arc::new(RecordingProvider {
        calls: AtomicUsize::new(0),
        total_turns: 1,
        last_system_prompt: Mutex::new(String::new()),
    });
    let provider: Arc<dyn LlmProvider> = provider_inner.clone();
    let agent = test_agent(provider, Some(controller)).await;
    let _ = agent.process_message("go", &[], vec![]).await.unwrap();

    let captured = provider_inner.last_system_prompt.lock().unwrap().clone();
    assert!(
        captured.contains("## Live Sensor Data"),
        "expected sensor summary in system prompt, got: {captured}"
    );
    assert!(captured.contains("joint_positions"));
    assert!(captured.contains("battery"));
    // Budget * 4 = approximate byte ceiling; allow some extra for the base
    // worker prompt we prepend to.
    let summary_byte_ceiling = 256 * 4 + "\n... (sensor summary truncated)".len();
    // Extract sensor section to assert budget was honored; header present
    let sensor_start = captured.find("## Live Sensor Data").unwrap();
    let sensor_block = &captured[sensor_start..];
    assert!(
        sensor_block.len() <= summary_byte_ceiling,
        "sensor block {} exceeds byte ceiling {}",
        sensor_block.len(),
        summary_byte_ceiling
    );
}

#[tokio::test]
async fn should_truncate_oversize_sensor_summary() {
    let bus = MockBus::with_snaps(
        (0..32)
            .map(|i| SensorSnapshot {
                sensor_id: format!("sensor_{i:02}"),
                value: serde_json::json!({"payload": "x".repeat(100)}),
                timestamp_ms: 1000,
            })
            .collect(),
    );
    let injector = Arc::new(SensorContextInjector::with_source(
        64,
        bus.clone() as Arc<dyn SensorSource>,
    ));
    injector.refresh_from_source();

    let summary = injector.summarize(16); // ~64 bytes budget
    let marker = "(sensor summary truncated)";
    assert!(
        summary.contains(marker),
        "expected summary to contain `{marker}`, got `{summary}`"
    );
    // Hard ceiling invariant: never omit — always have SOME content.
    assert!(!summary.is_empty());
    let budget_bytes = 16 * 4 + "\n... (sensor summary truncated)".len();
    assert!(
        summary.len() <= budget_bytes,
        "summary size {} exceeds ceiling {}",
        summary.len(),
        budget_bytes
    );
}

#[tokio::test]
async fn should_degrade_silently_when_sensor_source_stalls() {
    let injector = Arc::new(SensorContextInjector::with_source(
        8,
        Arc::new(StalledBus) as Arc<dyn SensorSource>,
    ));
    let controller = Arc::new(
        RealtimeController::new(RealtimeConfig {
            enabled: true,
            heartbeat_timeout_ms: 30_000,
            ..Default::default()
        })
        .with_injector(injector),
    );
    let provider_inner = Arc::new(RecordingProvider {
        calls: AtomicUsize::new(0),
        total_turns: 1,
        last_system_prompt: Mutex::new(String::new()),
    });
    let provider: Arc<dyn LlmProvider> = provider_inner.clone();
    let agent = test_agent(provider, Some(controller)).await;

    // Must not panic or return Err.
    let response = agent
        .process_message("hello", &[], vec![])
        .await
        .expect("stalled sensor source must not abort the turn");

    // System prompt should not contain the `## Live Sensor Data` header
    // because the injector is empty.
    let captured = provider_inner.last_system_prompt.lock().unwrap().clone();
    assert!(
        !captured.contains("## Live Sensor Data"),
        "empty sensor bus should not inject a header, got: {captured}"
    );
    assert_eq!(response.content, "turn 0");
}

#[tokio::test]
async fn should_attach_sensor_snapshot_to_hook_domain_data() {
    use octos_agent::{HookEvent, HookPayload, HookPayloadEnricher};

    let bus = MockBus::with_snaps(vec![SensorSnapshot {
        sensor_id: "force_torque".into(),
        value: serde_json::json!([0.5, 0.1, 9.8]),
        timestamp_ms: 1000,
    }]);
    let enricher = RealtimeHookEnricher::new(bus.clone() as Arc<dyn SensorSource>);
    let mut payload = HookPayload::on_resume(None);
    enricher.enrich(&HookEvent::BeforeToolCall, &mut payload);
    let data = payload
        .domain_data
        .as_ref()
        .expect("domain_data should be set when source has snapshots");
    assert_eq!(data["source"], "octos_realtime");
    let snaps = data["snapshots"].as_array().expect("snapshots array");
    assert_eq!(snaps.len(), 1);
    assert_eq!(snaps[0]["sensor_id"], "force_torque");
}

#[tokio::test]
async fn realtime_heartbeat_example_runs_end_to_end() {
    // This smoke-test mirrors the example binary end-to-end without spawning
    // a child process: it wires realtime + a mock LLM, runs a turn, and
    // asserts the heartbeat counted the iterations and the sensor summary
    // landed in the system prompt.
    let bus = MockBus::with_snaps(vec![SensorSnapshot {
        sensor_id: "lidar_front".into(),
        value: serde_json::json!({"range_m": 3.2, "clear": true}),
        timestamp_ms: 1000,
    }]);
    let injector = Arc::new(SensorContextInjector::with_source(
        4,
        bus.clone() as Arc<dyn SensorSource>,
    ));
    let controller = Arc::new(
        RealtimeController::new(RealtimeConfig {
            enabled: true,
            heartbeat_timeout_ms: 30_000,
            sensor_budget_tokens: 128,
            ..Default::default()
        })
        .with_injector(injector.clone()),
    );
    let provider_inner = Arc::new(RecordingProvider {
        calls: AtomicUsize::new(0),
        total_turns: 1,
        last_system_prompt: Mutex::new(String::new()),
    });
    let provider: Arc<dyn LlmProvider> = provider_inner.clone();
    let agent = test_agent(provider, Some(controller.clone())).await;

    let before = controller.heartbeat().count();
    let response = agent.process_message("patrol", &[], vec![]).await.unwrap();
    let after = controller.heartbeat().count();

    assert!(after > before, "heartbeat must advance during a run");
    assert_eq!(response.content, "turn 0");
    let captured = provider_inner.last_system_prompt.lock().unwrap().clone();
    assert!(captured.contains("lidar_front"));
    assert_eq!(controller.heartbeat().state(), HeartbeatState::Alive);

    // Full stall demo off-loop, just to exercise the public surface.
    let hb = Heartbeat::new(Duration::from_millis(5));
    let _ = hb.state();
    hb.force_stall_for_test(Duration::from_millis(50));
    assert_eq!(hb.state(), HeartbeatState::Stalled);
}
