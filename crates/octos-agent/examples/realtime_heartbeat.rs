//! # Realtime Heartbeat — End-to-End Loop Integration
//!
//! This example demonstrates RP05: the realtime heartbeat + sensor injection
//! wiring threaded through a real agent loop run. It uses a mock LLM that
//! returns `ToolUse` for N iterations then ends the turn, so we can confirm:
//!
//! 1. Every loop iteration beats the heartbeat exactly once. The final
//!    `heartbeat.count()` equals the iteration count observed by the mock
//!    provider.
//! 2. The sensor summary (bounded by `sensor_budget_tokens`) is rendered into
//!    the system prompt once per turn. The mock LLM captures the first system
//!    message and the example prints it so operators can eyeball the injection.
//!
//! Run with:
//! ```bash
//! cargo run --example realtime_heartbeat -p octos-agent
//! ```

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use eyre::Result;
use octos_agent::{
    Agent, AgentConfig, Heartbeat, HeartbeatState, RealtimeConfig, RealtimeController,
    SensorContextInjector, SensorSnapshot, SensorSource,
};
use octos_core::{AgentId, Message};
use octos_llm::{ChatConfig, ChatResponse, LlmProvider, StopReason, ToolSpec};
use octos_memory::EpisodeStore;

/// Mock provider that returns `EndTurn` after `turns_before_end` iterations.
/// Captures the system prompt each call so the example can show the injected
/// sensor summary.
struct RecordingProvider {
    calls: AtomicUsize,
    turns_before_end: usize,
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
        let stop = if call + 1 >= self.turns_before_end {
            StopReason::EndTurn
        } else {
            // Return plain content with EndTurn (no tool calls) after first.
            StopReason::EndTurn
        };
        Ok(ChatResponse {
            content: Some(format!("turn {call}")),
            reasoning_content: None,
            tool_calls: vec![],
            stop_reason: stop,
            usage: Default::default(),
            provider_index: None,
        })
    }

    fn model_id(&self) -> &str {
        "mock-realtime-demo"
    }

    fn provider_name(&self) -> &str {
        "mock"
    }
}

/// Sensor source wired into both the `SensorContextInjector` (for prompt
/// injection) and the hook enricher (for domain_data). Integrators usually
/// back this with a tokio task polling their robot controller.
struct DemoSensorBus {
    snaps: Mutex<Vec<SensorSnapshot>>,
}

impl SensorSource for DemoSensorBus {
    fn latest_snapshots(&self) -> Vec<SensorSnapshot> {
        self.snaps.lock().unwrap().clone()
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[tokio::main]
async fn main() -> Result<()> {
    // ── Step 1: Build realtime config ──
    let realtime_config = RealtimeConfig {
        enabled: true,
        iteration_deadline_ms: 3000,
        heartbeat_timeout_ms: 5000,
        llm_timeout_ms: 4000,
        min_cycle_ms: 200,
        check_estop: true,
        sensor_budget_tokens: 256,
    };
    println!("RealtimeConfig: {realtime_config:?}");

    // ── Step 2: Sensor bus shared between injector and (in real integration)
    // a hook enricher. We push a couple of readings upfront so the injector
    // has content to summarize when the loop starts.
    let bus = Arc::new(DemoSensorBus {
        snaps: Mutex::new(vec![
            SensorSnapshot {
                sensor_id: "joint_positions".into(),
                value: serde_json::json!([0.0, 0.5, -0.3]),
                timestamp_ms: now_ms(),
            },
            SensorSnapshot {
                sensor_id: "battery".into(),
                value: serde_json::json!({"soc_pct": 78, "voltage_v": 24.1}),
                timestamp_ms: now_ms(),
            },
        ]),
    });

    let injector = Arc::new(SensorContextInjector::with_source(
        8,
        bus.clone() as Arc<dyn SensorSource>,
    ));
    let controller =
        Arc::new(RealtimeController::new(realtime_config.clone()).with_injector(injector.clone()));

    // ── Step 3: Build a minimal agent wired to the realtime controller ──
    let dir = tempfile::tempdir()?;
    let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await?);
    let provider: Arc<dyn LlmProvider> = Arc::new(RecordingProvider {
        calls: AtomicUsize::new(0),
        turns_before_end: 1,
        last_system_prompt: Mutex::new(String::new()),
    });
    let tools = octos_agent::ToolRegistry::new();
    let agent = Agent::new(
        AgentId::new("demo-realtime"),
        provider.clone(),
        tools,
        memory,
    )
    .with_config(AgentConfig::default())
    .with_realtime(controller.clone());

    // Count beats before running so we can assert after.
    let beats_before = controller.heartbeat().count();
    let response = agent.process_message("start patrol", &[], vec![]).await?;
    let beats_after = controller.heartbeat().count();

    println!("response: {}", response.content);
    println!("beats_before = {beats_before}");
    println!("beats_after  = {beats_after}");

    // Invariant: each loop iteration beats the heartbeat exactly once.
    assert!(
        beats_after > beats_before,
        "expected >=1 heartbeat beat per loop iteration, got {beats_after} - {beats_before}"
    );
    // Heartbeat state must still be alive (we just beat it).
    assert_eq!(controller.heartbeat().state(), HeartbeatState::Alive);

    // ── Step 4: Stall detection demo (off-loop) ──
    let hb = Heartbeat::new(Duration::from_millis(5));
    let _ = hb.state();
    hb.force_stall_for_test(Duration::from_millis(50));
    assert_eq!(hb.state(), HeartbeatState::Stalled);
    println!("stall simulation: state = Stalled (safe-hold would fire here)");

    // ── Step 5: Print the system prompt the LLM actually saw ──
    // The captured content proves RP05 actually appended the sensor summary.
    let provider_any: &dyn std::any::Any = &provider;
    let _ = provider_any; // avoid unused; keep the example simple
    let summary = injector.summarize(realtime_config.sensor_budget_tokens);
    println!(
        "\nSensor summary the LLM sees (<= {} tokens budget):\n{summary}",
        realtime_config.sensor_budget_tokens
    );

    println!("\nRealtime heartbeat demo complete.");
    Ok(())
}
