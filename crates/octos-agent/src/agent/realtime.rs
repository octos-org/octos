//! Real-time agent loop extensions for robotic operation.
//!
//! Provides timing guarantees, heartbeat monitoring, and sensor context
//! injection for safe 24/7 robotic agent operation.
//!
//! ## Wiring (RP05)
//!
//! - `RealtimeController` bundles the optional heartbeat, sensor source, and
//!   injector. The agent loop (`loop_runner`) holds one as `Option<Arc<_>>` and
//!   calls `beat_and_check()` at the top of each iteration.
//! - `SensorContextInjector::summarize(budget)` renders a hard-ceiling budget
//!   bounded summary that `execution.rs` appends to the system prompt.
//! - `RealtimeHookEnricher` implements `HookPayloadEnricher` (RP03) and writes
//!   the latest `SensorSnapshot` into `HookPayload.domain_data`.

use std::collections::VecDeque;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, RwLock};
use std::time::{Duration, Instant};

use metrics::{counter, histogram};
use serde::{Deserialize, Serialize};

use crate::hooks::{HookEvent, HookPayload, HookPayloadEnricher};

/// Typed agent-loop errors surfaced via `eyre::Result`. Callers that need to
/// react differently to a stall (e.g. send safe-hold commands) should
/// `err.downcast_ref::<AgentError>()` at the top-level `run_task` boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentError {
    /// The heartbeat reported `Stalled` before the next iteration could begin.
    HeartbeatStalled { iteration: u32, timeout_ms: u64 },
}

impl fmt::Display for AgentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HeartbeatStalled {
                iteration,
                timeout_ms,
            } => write!(
                f,
                "heartbeat stalled at iteration {iteration} (timeout {timeout_ms}ms)"
            ),
        }
    }
}

impl std::error::Error for AgentError {}

/// Configuration for real-time agent loop behavior.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RealtimeConfig {
    /// Master switch. Defaults to `false` so absent/unset config is a no-op.
    #[serde(default)]
    pub enabled: bool,

    /// Maximum time per agent loop iteration (LLM call + tool execution).
    /// If exceeded, the loop logs a warning and continues.
    #[serde(default = "default_iteration_deadline_ms")]
    pub iteration_deadline_ms: u64,

    /// Heartbeat interval. If no `beat()` within this period, the agent
    /// is considered stalled and should enter safe-hold.
    #[serde(default = "default_heartbeat_timeout_ms")]
    pub heartbeat_timeout_ms: u64,

    /// LLM call timeout. Aborts the LLM request if it exceeds this.
    #[serde(default = "default_llm_timeout_ms")]
    pub llm_timeout_ms: u64,

    /// Minimum cycle time. The loop sleeps to fill remaining time,
    /// preventing busy-spinning on fast iterations.
    #[serde(default = "default_min_cycle_ms")]
    pub min_cycle_ms: u64,

    /// Whether to check e-stop state before each iteration.
    #[serde(default = "default_true")]
    pub check_estop: bool,

    /// Hard ceiling for the sensor summary appended to the system prompt.
    /// A rough estimate of 4 bytes per token is used to keep this pure and
    /// dependency-free.
    #[serde(default = "default_sensor_budget_tokens")]
    pub sensor_budget_tokens: u32,
}

fn default_iteration_deadline_ms() -> u64 {
    5000
}
fn default_heartbeat_timeout_ms() -> u64 {
    10000
}
fn default_llm_timeout_ms() -> u64 {
    8000
}
fn default_min_cycle_ms() -> u64 {
    100
}
fn default_true() -> bool {
    true
}
fn default_sensor_budget_tokens() -> u32 {
    256
}

impl Default for RealtimeConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            iteration_deadline_ms: default_iteration_deadline_ms(),
            heartbeat_timeout_ms: default_heartbeat_timeout_ms(),
            llm_timeout_ms: default_llm_timeout_ms(),
            min_cycle_ms: default_min_cycle_ms(),
            check_estop: true,
            sensor_budget_tokens: default_sensor_budget_tokens(),
        }
    }
}

/// Atomic heartbeat counter for monitoring agent liveness.
///
/// The agent loop calls `beat()` each iteration. External monitors
/// read `state()` to detect stalls.
pub struct Heartbeat {
    counter: AtomicU32,
    last_check_value: AtomicU32,
    timeout: Duration,
    last_beat: Mutex<Instant>,
}

/// Heartbeat health states.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeartbeatState {
    /// Agent is actively beating.
    Alive,
    /// No beat received within the timeout period.
    Stalled,
}

impl Heartbeat {
    pub fn new(timeout: Duration) -> Self {
        Self {
            counter: AtomicU32::new(0),
            last_check_value: AtomicU32::new(0),
            timeout,
            last_beat: Mutex::new(Instant::now()),
        }
    }

    /// Record a heartbeat (called each agent loop iteration).
    pub fn beat(&self) {
        self.counter.fetch_add(1, Ordering::Relaxed);
        *self.last_beat.lock().unwrap_or_else(|e| e.into_inner()) = Instant::now();
        counter!("octos_realtime_heartbeat_beats_total").increment(1);
    }

    /// Get the current beat count.
    pub fn count(&self) -> u32 {
        self.counter.load(Ordering::Relaxed)
    }

    /// Check the heartbeat state. Returns `Stalled` if no beat since last check
    /// and the timeout has elapsed.
    pub fn state(&self) -> HeartbeatState {
        let current = self.counter.load(Ordering::Relaxed);
        let prev = self.last_check_value.swap(current, Ordering::Relaxed);

        if current != prev {
            return HeartbeatState::Alive;
        }

        let last = *self.last_beat.lock().unwrap_or_else(|e| e.into_inner());
        if last.elapsed() > self.timeout {
            counter!("octos_realtime_heartbeat_stalls_total").increment(1);
            HeartbeatState::Stalled
        } else {
            HeartbeatState::Alive
        }
    }

    /// Force the stall timer by pretending no beat has landed for `age`.
    /// Intended for tests that want to assert stall-detection behavior
    /// without sleeping in real time.
    #[doc(hidden)]
    pub fn force_stall_for_test(&self, age: Duration) {
        let mut guard = self.last_beat.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(back) = Instant::now().checked_sub(age) {
            *guard = back;
        }
        // Also sync the check marker so state() will immediately compare
        // "no new beats since last check" to true.
        let current = self.counter.load(Ordering::Relaxed);
        self.last_check_value.store(current, Ordering::Relaxed);
    }
}

/// A timestamped snapshot of sensor data for LLM context injection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SensorSnapshot {
    /// Sensor identifier (e.g., "joint_positions", "force_torque").
    pub sensor_id: String,
    /// Sensor value as JSON.
    pub value: serde_json::Value,
    /// Unix timestamp in milliseconds.
    pub timestamp_ms: u64,
}

impl SensorSnapshot {
    /// Format as a compact text line for LLM context injection.
    pub fn to_context_line(&self) -> String {
        let age_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
            .saturating_sub(self.timestamp_ms);
        format!("[{}] {} ({}ms ago)", self.sensor_id, self.value, age_ms)
    }
}

/// Trait for reading the most recent sensor snapshots. Integrators plug an
/// `Arc<dyn SensorSource>` into the `RealtimeController` so the loop can inject
/// telemetry without knowing about ROS/dora-rs/etc.
///
/// Implementations must be fast, non-blocking, and tolerant of missing data
/// (return an empty `Vec` rather than blocking a stalled bus).
pub trait SensorSource: Send + Sync {
    /// Return the latest snapshots. Ordering is implementation-defined but
    /// callers treat the most recent reading per `sensor_id` as canonical.
    fn latest_snapshots(&self) -> Vec<SensorSnapshot>;
}

/// Ring buffer that accumulates sensor snapshots and formats them
/// for injection into the LLM system prompt.
pub struct SensorContextInjector {
    buffer: RwLock<VecDeque<SensorSnapshot>>,
    capacity: usize,
    source: Option<Arc<dyn SensorSource>>,
}

impl SensorContextInjector {
    pub fn new(capacity: usize) -> Self {
        Self {
            buffer: RwLock::new(VecDeque::with_capacity(capacity)),
            capacity,
            source: None,
        }
    }

    /// Construct an injector backed by a pluggable sensor source. The agent
    /// loop calls `refresh_from_source()` once per turn to pick up new data.
    pub fn with_source(capacity: usize, source: Arc<dyn SensorSource>) -> Self {
        Self {
            buffer: RwLock::new(VecDeque::with_capacity(capacity)),
            capacity,
            source: Some(source),
        }
    }

    /// Push a new sensor snapshot, evicting the oldest if at capacity.
    pub fn push(&self, snapshot: SensorSnapshot) {
        let mut buf = self.buffer.write().unwrap_or_else(|e| e.into_inner());
        if buf.len() >= self.capacity {
            buf.pop_front();
        }
        buf.push_back(snapshot);
    }

    /// Get the number of snapshots in the buffer.
    pub fn len(&self) -> usize {
        self.buffer.read().unwrap_or_else(|e| e.into_inner()).len()
    }

    /// Check if the buffer is empty.
    pub fn is_empty(&self) -> bool {
        self.buffer
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty()
    }

    /// Format all snapshots as a text block for LLM context injection.
    pub fn to_context_block(&self) -> String {
        let buf = self.buffer.read().unwrap_or_else(|e| e.into_inner());
        if buf.is_empty() {
            return String::new();
        }
        let mut lines = vec!["## Live Sensor Data".to_string()];
        for snap in buf.iter() {
            lines.push(snap.to_context_line());
        }
        lines.join("\n")
    }

    /// Get the latest snapshot for a given sensor ID.
    pub fn latest(&self, sensor_id: &str) -> Option<SensorSnapshot> {
        let buf = self.buffer.read().unwrap_or_else(|e| e.into_inner());
        buf.iter().rev().find(|s| s.sensor_id == sensor_id).cloned()
    }

    /// Return the most recently pushed snapshot (any sensor_id).
    pub fn latest_any(&self) -> Option<SensorSnapshot> {
        let buf = self.buffer.read().unwrap_or_else(|e| e.into_inner());
        buf.back().cloned()
    }

    /// Pull fresh snapshots from the configured `SensorSource` and push them
    /// into the ring buffer. Missing/empty sources degrade silently (no-op).
    pub fn refresh_from_source(&self) {
        let Some(source) = self.source.as_ref() else {
            return;
        };
        let snaps = source.latest_snapshots();
        for snap in snaps {
            self.push(snap);
        }
    }

    /// Render a short context summary bounded by `budget_tokens` (rough 4-bytes
    /// per token ceiling). Oversize summaries are truncated, never omitted, so
    /// the LLM always sees at least a header. Emits the
    /// `octos_realtime_sensor_injection_tokens` histogram.
    pub fn summarize(&self, budget_tokens: u32) -> String {
        let block = self.to_context_block();
        let summary = enforce_token_budget(&block, budget_tokens);
        let approx_tokens = approx_tokens_from_bytes(summary.len()) as f64;
        histogram!("octos_realtime_sensor_injection_tokens").record(approx_tokens);
        summary
    }
}

/// Approximate token count from a byte length using a 4-bytes-per-token rule.
/// The rule is intentionally conservative — LLM tokenizers split shorter than
/// 4 bytes in CJK but longer for whitespace-heavy ASCII; this keeps the
/// injection layer deterministic and dependency-free.
fn approx_tokens_from_bytes(bytes: usize) -> usize {
    bytes.div_ceil(4)
}

/// Truncate `text` so its byte length stays within the token budget while
/// keeping a `"\n... (truncated)"` marker if cutting was necessary. Never
/// returns an empty string when `text` is non-empty: we preserve at least the
/// first line so the LLM knows sensor data exists even when the budget is
/// zero.
fn enforce_token_budget(text: &str, budget_tokens: u32) -> String {
    if text.is_empty() {
        return String::new();
    }
    let byte_budget = (budget_tokens as usize).saturating_mul(4);
    if byte_budget == 0 {
        // Degenerate config: still show the header line so operators can see
        // that sensor data is available but fully suppressed.
        return first_line_marker(text);
    }
    if text.len() <= byte_budget {
        return text.to_string();
    }

    const MARKER: &str = "\n... (sensor summary truncated)";
    let marker_bytes = MARKER.len();
    if byte_budget <= marker_bytes {
        return first_line_marker(text);
    }
    let mut keep = byte_budget.saturating_sub(marker_bytes);
    while keep > 0 && !text.is_char_boundary(keep) {
        keep -= 1;
    }
    let mut out = String::with_capacity(keep + marker_bytes);
    out.push_str(&text[..keep]);
    out.push_str(MARKER);
    out
}

fn first_line_marker(text: &str) -> String {
    let first = text.lines().next().unwrap_or("");
    format!("{first} ... (sensor summary truncated)")
}

/// Controller bundling the components the agent loop needs for realtime
/// operation. Instances are held behind `Arc<_>` and shared with tools.
pub struct RealtimeController {
    config: RealtimeConfig,
    heartbeat: Arc<Heartbeat>,
    injector: Option<Arc<SensorContextInjector>>,
}

impl RealtimeController {
    pub fn new(config: RealtimeConfig) -> Self {
        let heartbeat = Arc::new(Heartbeat::new(Duration::from_millis(
            config.heartbeat_timeout_ms,
        )));
        Self {
            config,
            heartbeat,
            injector: None,
        }
    }

    pub fn with_injector(mut self, injector: Arc<SensorContextInjector>) -> Self {
        self.injector = Some(injector);
        self
    }

    pub fn config(&self) -> &RealtimeConfig {
        &self.config
    }

    pub fn heartbeat(&self) -> &Arc<Heartbeat> {
        &self.heartbeat
    }

    pub fn injector(&self) -> Option<&Arc<SensorContextInjector>> {
        self.injector.as_ref()
    }

    /// Check whether the heartbeat is stalled, then beat it only on Alive.
    /// The ordering guarantees:
    /// - `Stalled` returns without mutating the counter, so callers that
    ///   observe a stall see consistent history and do not log a bogus beat
    ///   immediately before aborting.
    /// - `Alive` beats the heartbeat so the iteration count matches the beat
    ///   count, satisfying the "beat_count == iteration_count" invariant.
    pub fn beat_and_check(&self) -> HeartbeatState {
        let state = self.heartbeat.state();
        if state == HeartbeatState::Alive {
            self.heartbeat.beat();
        }
        state
    }

    /// Produce a sensor summary for the current system prompt, bounded by the
    /// configured token budget. Returns `None` when no injector is configured
    /// (e.g. `realtime.enabled = true` without a `SensorSource`).
    pub fn sensor_summary(&self) -> Option<String> {
        let injector = self.injector.as_ref()?;
        injector.refresh_from_source();
        if injector.is_empty() {
            return None;
        }
        let summary = injector.summarize(self.config.sensor_budget_tokens);
        if summary.is_empty() {
            None
        } else {
            Some(summary)
        }
    }
}

/// Hook payload enricher that attaches the latest `SensorSnapshot` (per
/// `sensor_id`) to `HookPayload.domain_data`, so shell-based before/after
/// hooks can filter on live robot telemetry without a custom event variant.
pub struct RealtimeHookEnricher {
    source: Arc<dyn SensorSource>,
}

impl RealtimeHookEnricher {
    pub fn new(source: Arc<dyn SensorSource>) -> Self {
        Self { source }
    }
}

impl HookPayloadEnricher for RealtimeHookEnricher {
    fn enrich(&self, _event: &HookEvent, payload: &mut HookPayload) {
        let snapshots = self.source.latest_snapshots();
        if snapshots.is_empty() {
            return;
        }
        let snapshots_json: Vec<serde_json::Value> = snapshots
            .iter()
            .map(|s| serde_json::to_value(s).unwrap_or(serde_json::Value::Null))
            .collect();
        payload.domain_data = Some(serde_json::json!({
            "source": "octos_realtime",
            "snapshots": snapshots_json,
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_track_heartbeat_alive() {
        let hb = Heartbeat::new(Duration::from_millis(100));
        hb.beat();
        assert_eq!(hb.state(), HeartbeatState::Alive);
        assert_eq!(hb.count(), 1);
    }

    #[test]
    fn should_detect_stalled_heartbeat() {
        let hb = Heartbeat::new(Duration::from_millis(1));
        // Seed the check marker so state() knows "no new beats since last check".
        let _ = hb.state();
        hb.force_stall_for_test(Duration::from_millis(50));
        assert_eq!(hb.state(), HeartbeatState::Stalled);
    }

    #[test]
    fn should_use_ring_buffer_for_sensors() {
        let injector = SensorContextInjector::new(3);
        for i in 0..5 {
            injector.push(SensorSnapshot {
                sensor_id: format!("sensor_{i}"),
                value: serde_json::json!(i),
                timestamp_ms: 1000 + i as u64,
            });
        }
        assert_eq!(injector.len(), 3);
        // Oldest two evicted, should have sensor_2, sensor_3, sensor_4
        assert!(injector.latest("sensor_0").is_none());
        assert!(injector.latest("sensor_4").is_some());
    }

    #[test]
    fn should_format_sensor_context() {
        let injector = SensorContextInjector::new(10);
        injector.push(SensorSnapshot {
            sensor_id: "joint_positions".to_string(),
            value: serde_json::json!([0.0, 1.0, 2.0]),
            timestamp_ms: 999_999_000,
        });
        let block = injector.to_context_block();
        assert!(block.contains("## Live Sensor Data"));
        assert!(block.contains("joint_positions"));
    }

    #[test]
    fn should_use_default_config() {
        let config = RealtimeConfig::default();
        assert!(!config.enabled);
        assert_eq!(config.iteration_deadline_ms, 5000);
        assert_eq!(config.heartbeat_timeout_ms, 10000);
        assert_eq!(config.llm_timeout_ms, 8000);
        assert_eq!(config.min_cycle_ms, 100);
        assert!(config.check_estop);
        assert_eq!(config.sensor_budget_tokens, 256);
    }

    #[test]
    fn summarize_truncates_when_over_budget() {
        let injector = SensorContextInjector::new(32);
        // Push many snapshots so the block overflows a small budget.
        for i in 0..32 {
            injector.push(SensorSnapshot {
                sensor_id: format!("sensor_{i:02}"),
                value: serde_json::json!({"x": i, "payload": "x".repeat(80)}),
                timestamp_ms: 1000,
            });
        }
        let summary = injector.summarize(16); // ~64 bytes budget
        assert!(summary.contains("(sensor summary truncated)"));
        assert!(summary.len() <= 64 + "\n... (sensor summary truncated)".len());
    }

    #[test]
    fn summarize_empty_when_no_data() {
        let injector = SensorContextInjector::new(4);
        assert_eq!(injector.summarize(64), "");
    }

    struct FixedSensorSource {
        snaps: Vec<SensorSnapshot>,
    }

    impl SensorSource for FixedSensorSource {
        fn latest_snapshots(&self) -> Vec<SensorSnapshot> {
            self.snaps.clone()
        }
    }

    #[test]
    fn enricher_writes_domain_data_from_source() {
        let source = Arc::new(FixedSensorSource {
            snaps: vec![SensorSnapshot {
                sensor_id: "force_torque".into(),
                value: serde_json::json!([0.5, 0.1, 9.8]),
                timestamp_ms: 1000,
            }],
        });
        let enricher = RealtimeHookEnricher::new(source);

        let mut payload = HookPayload::on_resume(None);
        enricher.enrich(&HookEvent::BeforeToolCall, &mut payload);
        let data = payload.domain_data.expect("domain_data should be set");
        assert_eq!(data["source"], "octos_realtime");
        assert_eq!(data["snapshots"][0]["sensor_id"], "force_torque");
    }

    #[test]
    fn enricher_leaves_domain_data_when_source_empty() {
        let source = Arc::new(FixedSensorSource { snaps: Vec::new() });
        let enricher = RealtimeHookEnricher::new(source);
        let mut payload = HookPayload::on_resume(None);
        enricher.enrich(&HookEvent::BeforeToolCall, &mut payload);
        assert!(payload.domain_data.is_none());
    }
}
