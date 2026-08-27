//! Adaptive provider router with metrics-driven selection.
//!
//! Replaces static priority failover with a scoring system that tracks
//! per-provider latency (EMA + p95), error rates, and circuit breaker state.
//! Supports probe/canary requests to keep metrics fresh for non-primary providers.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use eyre::Result;
use futures::StreamExt;
use octos_core::Message;
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use crate::config::ChatConfig;
use crate::content_classifier::{ClassificationDecision, ContentClassifier};
use crate::credential_pool::{CredentialPool, ErrorId, rotation_reason};
use crate::provider::LlmProvider;
use crate::responsiveness::ResponsivenessObserver;
use crate::types::{ChatResponse, ChatStream, ProviderMetadata, StreamEvent, ToolSpec};

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Tuning knobs for the adaptive router.
#[derive(Debug, Clone)]
pub struct AdaptiveConfig {
    /// EMA smoothing factor (0..1). Higher = more responsive to recent latency.
    pub ema_alpha: f64,
    /// Consecutive failures before circuit breaker opens.
    pub failure_threshold: u32,
    /// Latency (ms) above which a soft penalty is applied.
    pub latency_threshold_ms: u64,
    /// Error rate (0..1) above which provider is deprioritized.
    pub error_rate_threshold: f64,
    /// Probability (0..1) of probing a non-primary provider.
    pub probe_probability: f64,
    /// Minimum seconds between probes to the same provider.
    pub probe_interval_secs: u64,
    /// Scoring weights (should sum to ~1.0).
    /// Controls quality+throughput factor (higher = prefer faster, higher-quality providers).
    pub weight_latency: f64,
    /// Controls stability factor (higher = penalize error-prone providers more).
    pub weight_error_rate: f64,
    pub weight_priority: f64,
    /// Weight for published token cost (0.0 = ignore cost).
    pub weight_cost: f64,
}

impl Default for AdaptiveConfig {
    fn default() -> Self {
        Self {
            ema_alpha: 0.3,
            failure_threshold: 3,
            latency_threshold_ms: 10_000,
            error_rate_threshold: 0.3,
            probe_probability: 0.1,
            probe_interval_secs: 60,
            weight_latency: 0.3,
            weight_error_rate: 0.3,
            weight_priority: 0.2,
            weight_cost: 0.2,
        }
    }
}

// ---------------------------------------------------------------------------
// Auto-escalation: latency-driven Lane -> Hedge self-promotion
// ---------------------------------------------------------------------------

/// Tunables for the per-session auto-escalation state machine.
///
/// When sustained-latency degradation is detected on a given session the
/// router self-promotes the global `AdaptiveMode` from `Lane` to `Hedge`
/// (and falls back to `Lane`/`Off` when latency recovers). The thresholds
/// match the legacy gateway-side `ResponsivenessObserver` defaults so
/// behavior is identical for `octos gateway` after the refactor.
#[derive(Debug, Clone)]
pub struct AutoEscalationConfig {
    /// Master switch. `false` disables all latency-tracking and mode flips.
    pub enabled: bool,
    /// Sliding window of recent turn latencies kept per session.
    pub window_size: usize,
    /// Number of warmup samples used to learn the baseline (median).
    pub baseline_samples: usize,
    /// Multiplier over baseline above which a single turn counts as "slow".
    /// e.g. `3.0` ⇒ slow if `latency > baseline * 3`.
    pub degradation_threshold: f64,
    /// Consecutive slow turns required to escalate.
    pub slow_trigger: u32,
    /// Hard ceiling — turns longer than this always count as slow once a
    /// baseline exists. Default 8000 ms, matches the FA-11/12 spec.
    pub latency_ceiling_ms: u64,
    /// Hysteresis fraction. After escalation, latency must drop below
    /// `latency_ceiling_ms * recovery_factor` for `should_deactivate()` to
    /// reset. Default `0.6` mirrors the existing single-fast-turn rule but
    /// adds a soft ceiling so a single below-threshold turn that is still
    /// noisy does not flap us back to Off.
    pub recovery_factor: f64,
}

impl Default for AutoEscalationConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            window_size: 5,
            baseline_samples: 5,
            degradation_threshold: 3.0,
            slow_trigger: 3,
            latency_ceiling_ms: 8_000,
            recovery_factor: 0.6,
        }
    }
}

/// Decision returned from [`AdaptiveRouter::record_turn_latency`].
///
/// Callers that want to drive UI/queue-mode side effects (gateway "⚡"
/// notification, `QueueMode::Speculative` flip) inspect this value;
/// callers that just want the router's own mode-flip behavior can ignore it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoEscalationDecision {
    /// No change — feature disabled, still warming up, or threshold not met.
    NoChange,
    /// Latency window just crossed the degradation threshold. Router has
    /// already flipped its mode to `Hedge`.
    Escalated,
    /// Latency window recovered. Router has already flipped back to
    /// the previous mode (recorded at the time of escalation).
    Deescalated,
}

/// Per-session auto-escalation state stored inside `AdaptiveRouter`.
struct SessionAutoState {
    observer: ResponsivenessObserver,
    /// Last latency sample (ms). Used by `should_deactivate_with_ceiling`.
    last_latency_ms: u64,
    /// Mode the router was in when we escalated, so we can restore it on
    /// recovery instead of dropping to `Off`. `None` while not escalated.
    pre_escalation_mode: Option<AdaptiveMode>,
}

impl SessionAutoState {
    fn new(cfg: &AutoEscalationConfig) -> Self {
        Self {
            observer: ResponsivenessObserver::with_params(
                cfg.window_size.max(cfg.baseline_samples),
                cfg.baseline_samples,
                cfg.degradation_threshold,
                cfg.slow_trigger,
            ),
            last_latency_ms: 0,
            pre_escalation_mode: None,
        }
    }
}

/// Notification fired when [`AdaptiveRouter`] auto-escalates or
/// de-escalates because of sustained latency on a session.
///
/// Wired by callers (gateway → "⚡ Detected slow responses…" message, web
/// → telemetry only) via [`AdaptiveRouter::set_auto_escalation_callback`].
#[derive(Debug, Clone)]
pub struct AutoEscalationEvent {
    /// The session id the router was driven by.
    pub session_id: String,
    /// Mode the router moved to (`Hedge` on escalate, restored mode on
    /// deescalate).
    pub new_mode: AdaptiveMode,
    /// Mode the router was in before this flip.
    pub previous_mode: AdaptiveMode,
    /// Latest latency sample that produced the flip (ms).
    pub latency_ms: u64,
    /// `true` for escalations, `false` for recoveries.
    pub escalated: bool,
}

/// Callback invoked when [`AdaptiveRouter`] auto-escalates or recovers.
/// Held under `RwLock` so it can be swapped at runtime without restarting
/// the router (mirrors `StatusCallback`).
pub type AutoEscalationCallback = Arc<dyn Fn(&AutoEscalationEvent) + Send + Sync>;

// ---------------------------------------------------------------------------
// Per-provider metrics
// ---------------------------------------------------------------------------

const LATENCY_BUFFER_SIZE: usize = 64;

/// Circular buffer for computing p95 latency.
struct LatencySamples {
    buf: [u64; LATENCY_BUFFER_SIZE],
    len: usize,
    pos: usize,
}

impl LatencySamples {
    fn new() -> Self {
        Self {
            buf: [0; LATENCY_BUFFER_SIZE],
            len: 0,
            pos: 0,
        }
    }

    fn push(&mut self, us: u64) {
        self.buf[self.pos] = us;
        self.pos = (self.pos + 1) % LATENCY_BUFFER_SIZE;
        if self.len < LATENCY_BUFFER_SIZE {
            self.len += 1;
        }
    }

    fn p95(&self) -> u64 {
        if self.len == 0 {
            return 0;
        }
        // Stack-allocated copy avoids per-call heap allocation.
        let mut sorted = self.buf;
        let slice = &mut sorted[..self.len];
        slice.sort_unstable();
        let idx = ((self.len as f64) * 0.95).ceil() as usize;
        slice[idx.min(self.len) - 1]
    }
}

/// Metrics for a single provider slot.
struct ProviderMetrics {
    /// Exponential moving average of latency (microseconds).
    latency_ema_us: AtomicU64,
    /// p95 latency (microseconds), updated on each sample.
    p95_latency_us: AtomicU64,
    /// Total successful requests (monotonic).
    success_count: AtomicU32,
    /// Total failed requests (monotonic).
    failure_count: AtomicU32,
    /// Consecutive failures (resets on success). Circuit breaker trigger.
    consecutive_failures: AtomicU32,
    /// Epoch micros of last successful request.
    last_success_us: AtomicU64,
    /// Epoch micros of last request (success or failure).
    last_request_us: AtomicU64,
    /// Total requests counter for periodic logging.
    total_requests: AtomicU32,
    /// Circular buffer for p95 computation.
    latency_samples: Mutex<LatencySamples>,
    /// Throughput EMA: output tokens per second. Task-normalized performance.
    throughput_ema: AtomicU64, // stored as f64 bits
}

impl ProviderMetrics {
    fn new() -> Self {
        Self {
            latency_ema_us: AtomicU64::new(0),
            p95_latency_us: AtomicU64::new(0),
            success_count: AtomicU32::new(0),
            failure_count: AtomicU32::new(0),
            consecutive_failures: AtomicU32::new(0),
            last_success_us: AtomicU64::new(0),
            last_request_us: AtomicU64::new(0),
            total_requests: AtomicU32::new(0),
            latency_samples: Mutex::new(LatencySamples::new()),
            throughput_ema: AtomicU64::new(0),
        }
    }

    /// Record throughput (output tokens per second) with EMA smoothing.
    fn record_throughput(&self, output_tokens: u32, latency_us: u64, alpha: f64) {
        if latency_us == 0 || output_tokens == 0 {
            return;
        }
        let tps = output_tokens as f64 / (latency_us as f64 / 1_000_000.0);
        let prev = f64::from_bits(self.throughput_ema.load(Ordering::Relaxed));
        let new_val = if prev == 0.0 {
            tps
        } else {
            alpha * tps + (1.0 - alpha) * prev
        };
        self.throughput_ema
            .store(new_val.to_bits(), Ordering::Relaxed);
    }

    fn throughput(&self) -> f64 {
        f64::from_bits(self.throughput_ema.load(Ordering::Relaxed))
    }

    fn record_success_with_alpha(&self, latency_us: u64, alpha: f64) {
        let now_us = now_epoch_us();

        let prev = self.latency_ema_us.load(Ordering::Relaxed);
        let new_ema = if prev == 0 {
            latency_us
        } else {
            ((alpha * latency_us as f64) + ((1.0 - alpha) * prev as f64)) as u64
        };
        self.latency_ema_us.store(new_ema, Ordering::Relaxed);

        if let Ok(mut samples) = self.latency_samples.lock() {
            samples.push(latency_us);
            self.p95_latency_us.store(samples.p95(), Ordering::Relaxed);
        }

        self.success_count.fetch_add(1, Ordering::Relaxed);
        self.consecutive_failures.store(0, Ordering::Relaxed);
        self.last_success_us.store(now_us, Ordering::Relaxed);
        self.last_request_us.store(now_us, Ordering::Relaxed);
        self.total_requests.fetch_add(1, Ordering::Relaxed);
    }

    fn record_failure(&self) {
        let now_us = now_epoch_us();
        self.failure_count.fetch_add(1, Ordering::Relaxed);
        self.consecutive_failures.fetch_add(1, Ordering::Relaxed);
        self.last_request_us.store(now_us, Ordering::Relaxed);
        self.total_requests.fetch_add(1, Ordering::Relaxed);
    }

    fn error_rate(&self) -> f64 {
        let s = self.success_count.load(Ordering::Relaxed);
        let f = self.failure_count.load(Ordering::Relaxed);
        let total = s + f;
        if total == 0 {
            0.0
        } else {
            f as f64 / total as f64
        }
    }

    fn is_circuit_open(&self, threshold: u32) -> bool {
        self.consecutive_failures.load(Ordering::Relaxed) >= threshold
    }

    fn is_stale(&self, probe_interval_secs: u64) -> bool {
        let last = self.last_request_us.load(Ordering::Relaxed);
        if last == 0 {
            return true; // Never used
        }
        let elapsed_us = now_epoch_us().saturating_sub(last);
        elapsed_us > probe_interval_secs * 1_000_000
    }

    fn snapshot(&self) -> MetricsSnapshot {
        let s = self.success_count.load(Ordering::Relaxed);
        let f = self.failure_count.load(Ordering::Relaxed);
        MetricsSnapshot {
            latency_ema_ms: self.latency_ema_us.load(Ordering::Relaxed) as f64 / 1000.0,
            p95_latency_ms: self.p95_latency_us.load(Ordering::Relaxed) as f64 / 1000.0,
            success_count: s,
            failure_count: f,
            consecutive_failures: self.consecutive_failures.load(Ordering::Relaxed),
            error_rate: if s + f == 0 {
                0.0
            } else {
                f as f64 / (s + f) as f64
            },
        }
    }
}

/// Public snapshot of provider metrics for observability.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricsSnapshot {
    pub latency_ema_ms: f64,
    pub p95_latency_ms: f64,
    pub success_count: u32,
    pub failure_count: u32,
    pub consecutive_failures: u32,
    pub error_rate: f64,
}

/// Baseline benchmark data for pre-seeding the adaptive router.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BaselineEntry {
    /// Provider key, e.g. "gemini/gemini-2.5-flash" or "dashscope/qwen3.5-plus".
    pub provider: String,
    /// Average latency in microseconds at max tool count.
    pub avg_latency_ms: u64,
    /// P95 latency in microseconds at max tool count.
    pub p95_latency_ms: u64,
    /// Stability score (0.0 to 1.0).
    pub stability: f64,
    /// Output cost in USD per million tokens (0.0 = unknown/free).
    #[serde(default)]
    pub cost_per_m_output: f64,
}

/// Model capability type for routing decisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelType {
    /// High-quality output, thorough analysis (>4000 tokens in deep search).
    Strong,
    /// Low latency, quick responses (<50s deep search or <1s tool call).
    Fast,
}

impl ModelType {
    fn to_u8(self) -> u8 {
        match self {
            ModelType::Strong => 0,
            ModelType::Fast => 1,
        }
    }

    fn from_u8(v: u8) -> Self {
        match v {
            0 => ModelType::Strong,
            _ => ModelType::Fast,
        }
    }
}

impl std::fmt::Display for ModelType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ModelType::Strong => write!(f, "STRONG"),
            ModelType::Fast => write!(f, "FAST"),
        }
    }
}

/// Unified model catalog entry — single source of truth for model metadata + live QoS.
///
fn is_false(value: &bool) -> bool {
    !*value
}

/// Static fields (type, cost, ds_output) are loaded from `model_catalog.json`.
/// Dynamic fields (stability, tool_avg_ms, p95_ms, score) are updated by the QoS scanner.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelCatalogEntry {
    /// Provider/model key, e.g. "minimax/MiniMax-M2.7".
    pub provider: String,
    /// Model capability type.
    #[serde(rename = "type")]
    pub model_type: ModelType,
    /// Whether this row is its provider family's default model — the model a
    /// family resolves to when a profile names no model. Exactly one row per
    /// family carries it; `registry::catalog_default_model` reads it.
    ///
    /// Skipped when false so the per-profile catalogs that `qos_catalog`
    /// rewrites do not sprout `"default": false` on all 150-odd rows.
    #[serde(default, rename = "default", skip_serializing_if = "is_false")]
    pub is_family_default: bool,
    /// Tool call stability (0.0 to 1.0). Updated by QoS scanner.
    pub stability: f64,
    /// Average tool call latency in ms. Updated by QoS scanner.
    pub tool_avg_ms: u64,
    /// P95 tool call latency in ms. Updated by QoS scanner.
    pub p95_ms: u64,
    /// Composite QoS score (lower = better). Updated by QoS scanner.
    pub score: f64,
    /// Input cost in USD per million tokens.
    pub cost_in: f64,
    /// Output cost in USD per million tokens.
    pub cost_out: f64,
    /// Deep search output token count (quality indicator). 0 = not evaluated.
    #[serde(default)]
    pub ds_output: u64,
    /// Context window size in tokens. 0 = unknown.
    #[serde(default)]
    pub context_window: u64,
    /// Maximum output tokens. 0 = unknown.
    #[serde(default)]
    pub max_output: u64,
}

/// Full model catalog with timestamp.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QosCatalog {
    pub updated_at: String,
    pub models: Vec<ModelCatalogEntry>,
}

/// Derive cold-start runtime scores from catalog metadata.
///
/// The heuristic model catalog is seed data, not a live score file. This
/// materializes an initial runtime catalog so downstream fallback code can use
/// the same score semantics before any live traffic has been observed.
pub fn derive_cold_start_catalog(
    entries: &[ModelCatalogEntry],
    config: &AdaptiveConfig,
    qos_ranking: bool,
) -> QosCatalog {
    let max_quality = entries
        .iter()
        .map(|entry| entry.ds_output as f64 * entry.stability.clamp(0.0, 1.0))
        .fold(0.0_f64, f64::max);
    let max_cost = if config.weight_cost > 0.0 {
        entries
            .iter()
            .map(|entry| entry.cost_out)
            .fold(0.0_f64, f64::max)
    } else {
        0.0
    };
    let max_priority = entries.len().max(1) as f64;

    let models = entries
        .iter()
        .enumerate()
        .map(|(idx, entry)| {
            let baseline_stab = entry.stability.clamp(0.0, 1.0);
            let blended_err = 1.0 - baseline_stab;

            let quality = entry.ds_output as f64 * baseline_stab;
            let norm_quality = if max_quality > 0.0 {
                1.0 - (quality / max_quality)
            } else {
                0.5
            };

            // No live throughput at cold start, so keep the throughput term neutral.
            let norm_throughput = 0.5;
            let norm_priority = idx as f64 / max_priority;
            let norm_cost = if max_cost > 0.0 && entry.cost_out > 0.0 {
                entry.cost_out / max_cost
            } else {
                0.0
            };
            let ranking_component = if qos_ranking {
                0.6 * norm_quality + 0.4 * norm_throughput
            } else {
                norm_throughput
            };

            let mut model = entry.clone();
            model.score = config.weight_error_rate * blended_err
                + config.weight_latency * ranking_component
                + config.weight_priority * norm_priority
                + config.weight_cost * norm_cost;
            model
        })
        .collect();

    QosCatalog {
        updated_at: chrono::Utc::now().to_rfc3339(),
        models,
    }
}

/// Adaptive routing policy parameters for observability.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SharedPolicy {
    pub ema_alpha: f64,
    pub failure_threshold: u32,
    pub latency_threshold_ms: u64,
    pub error_rate_threshold: f64,
    pub probe_probability: f64,
    pub probe_interval_secs: u64,
    pub weight_latency: f64,
    pub weight_error_rate: f64,
    pub weight_priority: f64,
    pub weight_cost: f64,
}

/// Shared metrics file format for inter-process export.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SharedMetrics {
    pub updated_at: String,
    pub policy: SharedPolicy,
    pub providers: Vec<SharedProviderMetrics>,
}

/// Per-provider metrics entry in the shared file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SharedProviderMetrics {
    pub provider: String,
    pub model: String,
    pub score: f64,
    #[serde(flatten)]
    pub metrics: MetricsSnapshot,
}

// ---------------------------------------------------------------------------
// Adaptive Router
// ---------------------------------------------------------------------------

/// A provider slot in the adaptive router.
struct AdaptiveSlot {
    provider: std::sync::Arc<dyn LlmProvider>,
    metrics: ProviderMetrics,
    /// Config-order priority (0 = primary, 1 = first fallback, etc.).
    priority: usize,
    /// Published output price in USD per million tokens (0.0 = unknown/free).
    cost_per_m: f64,
    /// Model capability type (Strong/Fast). Set from catalog seed.
    /// Encoded as AtomicU8 for lock-free reads in the routing hot path.
    model_type: AtomicU8,
    /// Input cost in USD per million tokens. Set from catalog seed.
    cost_in: AtomicU64,
    /// Original seeded cost_in — never overwritten by runtime, preserved across exports.
    seeded_cost_in: AtomicU64,
    /// Original seeded cost_out — never overwritten by runtime.
    seeded_cost_out: AtomicU64,
    /// Deep search output quality (token count). Set from catalog seed.
    ds_output: AtomicU64,
    /// Original seeded ds_output — never overwritten by runtime.
    seeded_ds_output: AtomicU64,
    /// Baseline stability from system catalog (used when no live data yet).
    baseline_stability: AtomicU64,
    /// Baseline tool_avg_ms from system catalog.
    baseline_tool_avg_ms: AtomicU64,
    /// Baseline p95_ms from system catalog.
    baseline_p95_ms: AtomicU64,
    /// Context window size in tokens.
    context_window: AtomicU64,
    /// Maximum output tokens.
    max_output: AtomicU64,
}

/// Adaptive routing mode — mutually exclusive strategies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AdaptiveMode {
    /// Static priority order. Failover only when a provider is circuit-broken
    /// (N consecutive failures). No scoring, no racing.
    Off = 0,
    /// Hedged racing: fire each request to 2 providers simultaneously,
    /// take the winner, cancel the loser. Both results accumulate QoS.
    Hedge = 1,
    /// Score-based lane changing: dynamically pick the best single provider
    /// based on latency/error/priority scoring. Cheaper than hedge.
    Lane = 2,
}

impl AdaptiveMode {
    fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Hedge,
            2 => Self::Lane,
            _ => Self::Off,
        }
    }
}

impl std::fmt::Display for AdaptiveMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Off => write!(f, "off"),
            Self::Hedge => write!(f, "hedge"),
            Self::Lane => write!(f, "lane"),
        }
    }
}

/// Runtime status of adaptive features (for dashboard / chat commands).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdaptiveStatus {
    pub mode: AdaptiveMode,
    pub qos_ranking: bool,
    pub failure_threshold: u32,
    pub provider_count: usize,
}

/// Wave4-A: per-router failover broadcast event. Pushed onto a
/// `tokio::sync::broadcast::Sender<FailoverEvent>` whenever the
/// adaptive router crosses from one lane to another inside `chat()` /
/// `chat_stream()`. Subscribers (API layer, telemetry) consume events
/// without blocking the router — the broadcast channel drops the
/// oldest event on a slow consumer.
///
/// Identifiers use the `<provider_name>/<model_id>` shape so they line
/// up with the `lane_scores` keys in `RouterStatusEvent`.
///
/// **Codex P1 (Wave4-A review)**: the AdaptiveRouter is *profile*-
/// scoped — every session on the same profile shares the same router
/// instance ([`ProfileRuntime`]). Without an originating identifier
/// here, two concurrent sessions on the same profile would each
/// re-emit one another's failovers as if it were their own. The
/// optional `originating_session_id` / `originating_turn_id` fields
/// let the API-layer forwarder filter to events whose context matches
/// its own session — see
/// [`with_router_context`] for the publisher-side hookup.
#[derive(Debug, Clone)]
pub struct FailoverEvent {
    pub from_provider: String,
    pub to_provider: String,
    pub reason: String,
    pub elapsed_ms: u64,
    /// Originating session id (free-form `SessionKey` string), captured
    /// via [`ROUTER_CONTEXT`] at publish time. `None` when chat() was
    /// invoked outside a context-aware scope (CLI smoke tests, etc.).
    pub originating_session_id: Option<String>,
    /// Originating turn id (UUID v7 hex), captured via [`ROUTER_CONTEXT`]
    /// at publish time.
    pub originating_turn_id: Option<String>,
}

/// Wave4-A: per-task context the API layer pushes BEFORE calling
/// `provider.chat()`. Read inside `AdaptiveRouter::publish_failover` to
/// stamp the originating session onto every emitted `FailoverEvent`.
///
/// Subscribers filter on this so a session B subscriber doesn't surface
/// session A's failover under session B. The context is `Cell`-style —
/// fork-friendly with `with_router_context`.
#[derive(Debug, Clone, Default)]
pub struct RouterContext {
    pub session_id: Option<String>,
    pub turn_id: Option<String>,
}

tokio::task_local! {
    /// Wave4-A: see [`RouterContext`]. Default `RouterContext::default()`
    /// when no scope wraps the chat() call (test paths, CLI smoke).
    pub static ROUTER_CONTEXT: RouterContext;
}

/// Wave4-A: run `fut` with the given `RouterContext` accessible via
/// [`ROUTER_CONTEXT`]. The API layer wraps `run_standalone_turn`'s
/// chat() path with this so the originating session id reaches the
/// router's failover publisher.
pub async fn with_router_context<F, T>(ctx: RouterContext, fut: F) -> T
where
    F: std::future::Future<Output = T>,
{
    ROUTER_CONTEXT.scope(ctx, fut).await
}

/// Snapshot the active [`RouterContext`]. Returns [`RouterContext::default`]
/// (no originating session/turn) when no scope wraps the caller. Mirrors
/// [`current_lane_context`](crate::current_lane_context) / [`current_llm_call_policy`](crate::current_llm_call_policy);
/// used to re-establish the routing context across a `tokio::spawn` boundary
/// (foreground tool tasks) so a tool's own LLM sub-call keeps the turn's
/// failover attribution instead of publishing unattributed events.
pub fn current_router_context() -> RouterContext {
    ROUTER_CONTEXT.try_with(|c| c.clone()).unwrap_or_default()
}

/// Adaptive provider router with metrics-driven selection.
///
/// Drop-in replacement for `ProviderChain`. Tracks latency and error rates
/// per provider, scores them dynamically, and routes to the best performer.
/// Probes stale providers to keep metrics fresh.
/// Callback for status updates (e.g. failover notifications).
/// The adaptive router calls this to inform the UI layer about provider
/// switches that happen inside `chat_stream()` failover.
pub type StatusCallback = Arc<dyn Fn(String) + Send + Sync>;

/// Callback invoked once per chat turn with a content classifier decision.
/// Wired by the agent layer to emit `octos.harness.event.v1 { kind: "routing.decision" }`
/// events and to bump the `octos_routing_decision_total` counter.
///
/// Invariant: this callback fires *before* the router picks a lane, so the
/// decision is observable even when the subsequent lane selection fails.
pub type RoutingDecisionCallback = Arc<dyn Fn(&ClassificationDecision) + Send + Sync>;

pub struct AdaptiveRouter {
    slots: Vec<AdaptiveSlot>,
    config: AdaptiveConfig,
    /// RNG state for probe selection (simple xorshift).
    rng_state: AtomicU64,
    /// Adaptive mode: Off / Hedge / Lane (mutually exclusive).
    mode: AtomicU8,
    /// Runtime toggle: QoS quality ranking (orthogonal to mode).
    qos_ranking: AtomicBool,
    /// Last provider index selected (for detecting switches).
    last_selected: AtomicU32,
    /// Optional callback for status updates (failover, provider switching).
    /// RwLock allows concurrent reads in the hot path (emit_status) while
    /// writes (set_status_callback) are rare setup-time operations.
    status_callback: RwLock<Option<StatusCallback>>,
    /// Content classifier that biases lane selection. `None` means "disabled"
    /// (router behaves as before — invariant #2 of issue #493). RwLock
    /// mirrors the status callback pattern so runtime toggles are safe.
    classifier: RwLock<Option<Arc<ContentClassifier>>>,
    /// Observer fired with the classifier decision on each chat entry.
    decision_callback: RwLock<Option<RoutingDecisionCallback>>,
    /// Optional per-slot credential pool. When attached, the router forwards
    /// rate-limit and auth failures to the pool so it can cool down or
    /// refresh the underlying credential. Empty vec means "no pools".
    credential_pools: RwLock<Vec<Option<Arc<dyn CredentialPool>>>>,
    /// Id of the credential currently in use per slot. Updated at acquire
    /// time so failure notifications can identify the right credential.
    current_credential_ids: Mutex<Vec<Option<String>>>,
    /// Wave4-A: lock-free broadcast channel for failover events. Senders
    /// publish into it from the `chat()` / `chat_stream()` failover loops;
    /// API-layer subscribers consume in a separate tokio task. The channel
    /// drops the oldest event under back-pressure (per
    /// `tokio::sync::broadcast` semantics) so a slow subscriber can NEVER
    /// stall the router's hot path. A capacity of 64 absorbs short bursts
    /// without forcing immediate drops.
    failover_tx: tokio::sync::broadcast::Sender<FailoverEvent>,
    /// Tuning for the latency-driven auto-escalation state machine. Cloned
    /// per-`record_turn_latency` call so threshold tweaks at runtime are
    /// rare — the cost is one Mutex acquire we'd already have to take.
    auto_escalation_config: RwLock<AutoEscalationConfig>,
    /// Per-session escalation state. Keyed by session id so a single
    /// degraded session does not poison metrics from other sessions and
    /// flap the global mode unnecessarily.
    auto_escalation_state: Mutex<HashMap<String, SessionAutoState>>,
    /// Callback fired on escalate / deescalate. Wired by gateway to send
    /// the "⚡ Detected slow responses…" chat message; wired by serve for
    /// telemetry.
    auto_escalation_callback: RwLock<Option<AutoEscalationCallback>>,
}

impl AdaptiveRouter {
    /// Create a new adaptive router from providers (in priority order).
    ///
    /// `costs` — published output price in USD/M tokens per provider.
    /// Pass an empty slice to use 0.0 (unknown) for all.
    ///
    /// Panics if `providers` is empty.
    pub fn new(
        providers: Vec<std::sync::Arc<dyn LlmProvider>>,
        costs: &[f64],
        config: AdaptiveConfig,
    ) -> Self {
        assert!(
            !providers.is_empty(),
            "AdaptiveRouter requires at least one provider"
        );
        let slots: Vec<AdaptiveSlot> = providers
            .into_iter()
            .enumerate()
            .map(|(i, p)| AdaptiveSlot {
                provider: p,
                metrics: ProviderMetrics::new(),
                priority: i,
                cost_per_m: costs.get(i).copied().unwrap_or(0.0),
                model_type: AtomicU8::new(ModelType::Fast.to_u8()), // default, overridden by catalog seed
                cost_in: AtomicU64::new(0),
                seeded_cost_in: AtomicU64::new(0),
                seeded_cost_out: AtomicU64::new(0),
                ds_output: AtomicU64::new(0),
                seeded_ds_output: AtomicU64::new(0),
                baseline_stability: AtomicU64::new(0),
                baseline_tool_avg_ms: AtomicU64::new(0),
                baseline_p95_ms: AtomicU64::new(0),
                context_window: AtomicU64::new(0),
                max_output: AtomicU64::new(0),
            })
            .collect();
        let slot_count = slots.len();
        // Wave4-A: capacity 64 absorbs short failover bursts without
        // dropping the oldest event. Slow subscribers fall behind and
        // observe a `RecvError::Lagged` — they MUST NOT stall the
        // router's hot path.
        let (failover_tx, _) = tokio::sync::broadcast::channel(64);
        Self {
            slots,
            config,
            rng_state: AtomicU64::new(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos() as u64,
            ),
            mode: AtomicU8::new(AdaptiveMode::Off as u8),
            qos_ranking: AtomicBool::new(false),
            last_selected: AtomicU32::new(0),
            status_callback: RwLock::new(None),
            classifier: RwLock::new(None),
            decision_callback: RwLock::new(None),
            credential_pools: RwLock::new(vec![None; slot_count]),
            current_credential_ids: Mutex::new(vec![None; slot_count]),
            failover_tx,
            auto_escalation_config: RwLock::new(AutoEscalationConfig::default()),
            auto_escalation_state: Mutex::new(HashMap::new()),
            auto_escalation_callback: RwLock::new(None),
        }
    }

    /// Wave4-A: subscribe to failover events. The subscriber receives a
    /// `FailoverEvent` each time the router crosses a lane in
    /// `chat()` / `chat_stream()`. The channel is `broadcast`-based, so
    /// every active subscriber gets every event; slow consumers may
    /// observe `RecvError::Lagged(n)` and MUST handle it (skip to head
    /// of channel; consider re-reading `adaptive_status()` to recover).
    pub fn subscribe_failover(&self) -> tokio::sync::broadcast::Receiver<FailoverEvent> {
        self.failover_tx.subscribe()
    }

    /// Wave4-A: publish a `FailoverEvent` (internal). `send` returns
    /// `Ok(receiver_count)` or `Err` when there are zero active
    /// subscribers — both outcomes are non-fatal for the router, so we
    /// ignore the result entirely.
    ///
    /// Reads [`ROUTER_CONTEXT`] (Codex P1): if the caller's task-local
    /// scope provides a `RouterContext`, stamps its `session_id` /
    /// `turn_id` onto the event so subscribers can filter to the
    /// originating session. Falls back to `None` outside such scopes
    /// (test paths / CLI smoke).
    fn publish_failover(
        &self,
        from_provider: &str,
        to_provider: &str,
        reason: &str,
        elapsed_ms: u64,
    ) {
        let (originating_session_id, originating_turn_id) = ROUTER_CONTEXT
            .try_with(|ctx| (ctx.session_id.clone(), ctx.turn_id.clone()))
            .unwrap_or_default();
        let _ = self.failover_tx.send(FailoverEvent {
            from_provider: from_provider.to_string(),
            to_provider: to_provider.to_string(),
            reason: reason.to_string(),
            elapsed_ms,
            originating_session_id,
            originating_turn_id,
        });
    }

    /// Wave-4 B3: synthesize and broadcast a `FailoverEvent` to all
    /// `subscribe_failover()` listeners. Wraps the internal
    /// `publish_failover` so out-of-band callers — gateway tests,
    /// synthetic monitoring — can drive the failover stream without
    /// going through the chat loop.
    ///
    /// **Gated behind `feature = "test-utils"`** so production builds
    /// can never synthesize false failovers. Downstream crates that
    /// need the helper in their integration tests enable the feature
    /// via `[dev-dependencies] octos-llm = { features = ["test-utils"] }`.
    #[cfg(any(test, feature = "test-utils"))]
    pub fn publish_failover_for_subscribers(
        &self,
        from_provider: &str,
        to_provider: &str,
        reason: &str,
        elapsed_ms: u64,
    ) {
        self.publish_failover(from_provider, to_provider, reason, elapsed_ms);
    }

    /// Wave4-A: snapshot per-lane scores keyed by
    /// `"<provider_name>/<model_id>"`. Returned as a `BTreeMap` so the
    /// caller can hand it straight to the UI protocol layer without
    /// re-sorting.
    pub fn lane_scores(&self) -> std::collections::BTreeMap<String, f64> {
        self.slots
            .iter()
            .map(|s| {
                (
                    format!("{}/{}", s.provider.provider_name(), s.provider.model_id()),
                    self.score(s),
                )
            })
            .collect()
    }

    /// Wave4-A: snapshot per-lane circuit-breaker state keyed by the same
    /// `"<provider_name>/<model_id>"` shape as [`lane_scores`]. Values
    /// are the string rendering — `"closed"`, `"open"`, or `"half_open"`
    /// — so the wire shape stays stable across enum changes.
    ///
    /// We don't have a tri-state breaker yet (today it's
    /// `consecutive_failures` past `failure_threshold`); `"half_open"`
    /// is reserved for when one lands.
    pub fn breaker_states(&self) -> std::collections::BTreeMap<String, String> {
        self.slots
            .iter()
            .map(|s| {
                let key = format!("{}/{}", s.provider.provider_name(), s.provider.model_id());
                let state = if s.metrics.is_circuit_open(self.config.failure_threshold) {
                    "open"
                } else {
                    "closed"
                };
                (key, state.to_string())
            })
            .collect()
    }

    /// Wave4-A: friendly accessor for the currently-selected lane in the
    /// `"<provider_name>/<model_id>"` form expected by
    /// `RouterStatusEvent::provider_name`. Falls back to `"unknown"`
    /// when `last_selected` is out of range (cold-start race).
    pub fn current_lane_key(&self) -> String {
        let idx = self.last_selected.load(Ordering::Relaxed) as usize;
        match self.slots.get(idx) {
            Some(slot) => format!(
                "{}/{}",
                slot.provider.provider_name(),
                slot.provider.model_id()
            ),
            None => "unknown".to_string(),
        }
    }

    /// Attach a credential pool to slot `idx`. The router forwards 429 and
    /// auth failures to the pool so keys can rotate without the caller
    /// orchestrating it. Silently ignores out-of-range indices.
    pub fn attach_credential_pool(&self, idx: usize, pool: Arc<dyn CredentialPool>) {
        let mut pools = self.credential_pools.write().unwrap();
        if idx < pools.len() {
            pools[idx] = Some(pool);
        }
    }

    /// Acquire the current credential for `idx` from the attached pool (if
    /// any). Returns `None` when no pool is attached, when the slot is out
    /// of range, or when every credential is in cooldown. Callers that don't
    /// use credential pools can ignore this entirely.
    pub async fn acquire_credential(&self, idx: usize, reason: &str) -> Option<String> {
        let pool = {
            let pools = self.credential_pools.read().unwrap();
            pools.get(idx).and_then(|opt| opt.clone())
        };
        let pool = pool?;
        match pool.acquire(reason).await {
            Ok(cred) => {
                let mut ids = self
                    .current_credential_ids
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                if let Some(slot) = ids.get_mut(idx) {
                    *slot = Some(cred.id.clone());
                }
                Some(cred.id)
            }
            Err(e) => {
                warn!(idx, error = %e, "credential pool acquire failed");
                None
            }
        }
    }

    /// Notify the attached credential pool (if any) that slot `idx` observed
    /// a recoverable failure so it can cool the credential down or refresh
    /// OAuth tokens. No-op when no pool is attached.
    ///
    /// `auth_failure` — treats the error as authentication and invokes the
    /// refresher at most once per `error_id`.
    /// `rate_limit_reset_us` — cooldown target for 429 errors.
    pub async fn notify_credential_failure(
        &self,
        idx: usize,
        auth_failure: bool,
        rate_limit_reset_us: Option<u64>,
        error_id: ErrorId,
    ) {
        let pool = {
            let pools = self.credential_pools.read().unwrap();
            pools.get(idx).and_then(|opt| opt.clone())
        };
        let Some(pool) = pool else {
            return;
        };
        let cred_id = {
            let ids = self
                .current_credential_ids
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            ids.get(idx).and_then(|slot| slot.clone())
        };
        let Some(cred_id) = cred_id else {
            debug!(idx, "notify_credential_failure without acquired id");
            return;
        };
        if auth_failure {
            if let Err(e) = pool.mark_auth_failure(&cred_id, error_id).await {
                warn!(idx, cred_id, error = %e, "mark_auth_failure failed");
            }
        } else if let Err(e) = pool.mark_rate_limited(&cred_id, rate_limit_reset_us).await {
            warn!(idx, cred_id, error = %e, "mark_rate_limited failed");
        }
    }

    /// Report a successful request for slot `idx` to its credential pool.
    pub async fn notify_credential_success(&self, idx: usize) {
        let pool = {
            let pools = self.credential_pools.read().unwrap();
            pools.get(idx).and_then(|opt| opt.clone())
        };
        let Some(pool) = pool else {
            return;
        };
        let cred_id = {
            let ids = self
                .current_credential_ids
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            ids.get(idx).and_then(|slot| slot.clone())
        };
        let Some(cred_id) = cred_id else {
            return;
        };
        if let Err(e) = pool.mark_success(&cred_id).await {
            warn!(idx, cred_id, error = %e, "mark_success failed");
        }
    }

    /// Convenience: acquire the initial credential for slot `idx`.
    pub async fn acquire_initial_credential(&self, idx: usize) -> Option<String> {
        self.acquire_credential(idx, rotation_reason::INITIAL_ACQUIRE)
            .await
    }

    /// Set initial adaptive mode and QoS toggle from config.
    /// Uses atomic stores (interior mutability) so `mut` is not required.
    pub fn with_adaptive_config(self, mode: AdaptiveMode, qos_ranking: bool) -> Self {
        self.mode.store(mode as u8, Ordering::Relaxed);
        self.qos_ranking.store(qos_ranking, Ordering::Relaxed);
        self
    }

    /// Get the current adaptive mode.
    pub fn mode(&self) -> AdaptiveMode {
        AdaptiveMode::from_u8(self.mode.load(Ordering::Relaxed))
    }

    /// Switch adaptive mode at runtime (lock-free, mutually exclusive).
    pub fn set_mode(&self, mode: AdaptiveMode) {
        self.mode.store(mode as u8, Ordering::Relaxed);
        info!(%mode, "adaptive mode changed");
    }

    /// Set a callback for status updates (failover notifications).
    /// Called from `chat_stream()` failover so the UI can inform the user.
    pub fn set_status_callback(&self, cb: Option<StatusCallback>) {
        *self.status_callback.write().unwrap() = cb;
    }

    /// Emit a status message through the callback (if set).
    fn emit_status(&self, message: String) {
        if let Some(cb) = self.status_callback.read().unwrap().as_ref() {
            cb(message);
        }
    }

    /// Replace the auto-escalation tunables at runtime. Subsequent
    /// `record_turn_latency` calls observe the new config; existing
    /// per-session state retains its already-built window.
    pub fn set_auto_escalation_config(&self, cfg: AutoEscalationConfig) {
        *self.auto_escalation_config.write().unwrap() = cfg;
    }

    /// Snapshot the current auto-escalation tunables (clone).
    pub fn auto_escalation_config(&self) -> AutoEscalationConfig {
        self.auto_escalation_config.read().unwrap().clone()
    }

    /// Install a callback invoked when the router auto-escalates or
    /// recovers. `None` clears it. Wired by gateway to send the
    /// "⚡ Detected slow responses…" notification; wired by serve to feed
    /// telemetry.
    pub fn set_auto_escalation_callback(&self, cb: Option<AutoEscalationCallback>) {
        *self.auto_escalation_callback.write().unwrap() = cb;
    }

    /// Record a turn's end-to-end LLM latency for a session and let the
    /// router decide whether to self-promote (`Lane`/`Off` → `Hedge`) or
    /// recover. Returns the decision so callers can drive gateway-only
    /// side effects (queue mode flip, "⚡" chat message).
    ///
    /// Concurrency: holds the per-router `auto_escalation_state` mutex
    /// for the duration of one record + check. The mutex is short-lived
    /// — this is not on the hot per-token path, only the once-per-turn
    /// boundary.
    ///
    /// When the feature is disabled via [`AutoEscalationConfig::enabled`]
    /// `false` the router is a no-op and returns
    /// [`AutoEscalationDecision::NoChange`].
    pub fn record_turn_latency(
        &self,
        session_id: &str,
        latency: Duration,
    ) -> AutoEscalationDecision {
        let cfg = self.auto_escalation_config.read().unwrap().clone();
        if !cfg.enabled {
            return AutoEscalationDecision::NoChange;
        }
        let latency_ms = latency.as_millis().min(u128::from(u64::MAX)) as u64;
        let (decision, event) = {
            let mut state_map = self
                .auto_escalation_state
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let state = state_map
                .entry(session_id.to_string())
                .or_insert_with(|| SessionAutoState::new(&cfg));
            state.last_latency_ms = latency_ms;
            // Use the ceiling-aware record so absolute-latency excursions
            // (e.g. 8s+) count as slow even when the per-session baseline
            // would otherwise normalize them.
            state
                .observer
                .record_with_ceiling(latency, Some(cfg.latency_ceiling_ms));
            let current_mode = self.mode();

            // Escalate when the observer says so AND we're not already
            // in Hedge mode. The observer's `should_activate` already
            // gates on internal `active` so we don't double-fire.
            let trigger_escalate = state.observer.should_activate();
            let trigger_deescalate = state.observer.should_deactivate()
                && Self::below_recovery_ceiling(latency_ms, &cfg);

            if trigger_escalate && current_mode != AdaptiveMode::Hedge {
                state.observer.set_active(true);
                state.pre_escalation_mode = Some(current_mode);
                self.set_mode(AdaptiveMode::Hedge);
                warn!(
                    session = session_id,
                    latency_ms,
                    previous_mode = %current_mode,
                    "auto-escalation: promoting AdaptiveMode → Hedge on sustained latency"
                );
                let event = AutoEscalationEvent {
                    session_id: session_id.to_string(),
                    new_mode: AdaptiveMode::Hedge,
                    previous_mode: current_mode,
                    latency_ms,
                    escalated: true,
                };
                (AutoEscalationDecision::Escalated, Some(event))
            } else if trigger_deescalate {
                // Operator-override guard: if the router is no longer in
                // Hedge mode (a `/adaptive off|lane` was issued by the
                // user/operator since we escalated), drop our cached
                // `pre_escalation_mode` without overriding their choice.
                // Otherwise restore to the mode we saw at escalation
                // time.
                state.observer.set_active(false);
                let stashed = state.pre_escalation_mode.take();
                if current_mode != AdaptiveMode::Hedge {
                    info!(
                        session = session_id,
                        latency_ms,
                        current_mode = %current_mode,
                        "auto-escalation: latency recovered but router was manually moved off Hedge — leaving the operator-chosen mode in place"
                    );
                    (AutoEscalationDecision::Deescalated, None)
                } else {
                    let restore = stashed.unwrap_or(AdaptiveMode::Off);
                    self.set_mode(restore);
                    info!(
                        session = session_id,
                        latency_ms,
                        restored_mode = %restore,
                        "auto-escalation: latency recovered, restoring mode"
                    );
                    let event = AutoEscalationEvent {
                        session_id: session_id.to_string(),
                        new_mode: restore,
                        previous_mode: AdaptiveMode::Hedge,
                        latency_ms,
                        escalated: false,
                    };
                    (AutoEscalationDecision::Deescalated, Some(event))
                }
            } else {
                (AutoEscalationDecision::NoChange, None)
            }
        };
        if let Some(event) = event {
            if let Some(cb) = self.auto_escalation_callback.read().unwrap().as_ref() {
                cb(&event);
            }
        }
        decision
    }

    fn below_recovery_ceiling(latency_ms: u64, cfg: &AutoEscalationConfig) -> bool {
        // Latency must be below `latency_ceiling_ms * recovery_factor` for
        // recovery to fire. This is hysteresis on top of `should_deactivate`
        // so a single fast turn at the noisy edge of the ceiling doesn't
        // immediately flap us back to the pre-escalation mode.
        let ceiling = (cfg.latency_ceiling_ms as f64 * cfg.recovery_factor) as u64;
        if ceiling == 0 {
            return true;
        }
        latency_ms <= ceiling
    }

    /// Latency baseline learned for `session_id`, if any. Exposed so
    /// gateway-side code (the speculative-overflow "patience" computation
    /// in `session_actor.rs`) can read the same baseline the router used
    /// to decide on escalation, instead of carrying its own observer.
    pub fn session_latency_baseline(&self, session_id: &str) -> Option<Duration> {
        let state_map = self
            .auto_escalation_state
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        state_map
            .get(session_id)
            .and_then(|state| state.observer.baseline())
    }

    /// Number of latency samples recorded for `session_id`. Mirrors
    /// `ResponsivenessObserver::sample_count` for the per-session entry.
    pub fn session_latency_samples(&self, session_id: &str) -> usize {
        let state_map = self
            .auto_escalation_state
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        state_map
            .get(session_id)
            .map(|state| state.observer.sample_count())
            .unwrap_or(0)
    }

    /// Drop the per-session auto-escalation state. Callers should call
    /// this when a session terminates so the router doesn't grow
    /// unbounded under many short-lived sessions.
    ///
    /// Side effect: if the dropped session was the one that owned the
    /// last escalation (i.e. its `pre_escalation_mode` was the only
    /// record of "what the router was before Hedge"), the router is
    /// restored to that pre-escalation mode so a session that exits
    /// while still escalated does not leave the router stuck in Hedge
    /// indefinitely.
    pub fn forget_session(&self, session_id: &str) -> bool {
        let dropped = {
            let mut state_map = self
                .auto_escalation_state
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            state_map.remove(session_id)
        };
        let Some(state) = dropped else {
            return false;
        };
        // If this session owned an active escalation AND no other
        // session has its own active escalation, drop the router back
        // to what we saw before promoting. Without this, an exit-while-
        // escalated would leave the router stuck in Hedge.
        if let Some(restore) = state.pre_escalation_mode {
            if self.mode() == AdaptiveMode::Hedge && !self.any_session_escalated() {
                self.set_mode(restore);
                info!(
                    session = session_id,
                    restored_mode = %restore,
                    "forget_session: session exited while escalated, restoring router mode"
                );
            }
        }
        true
    }

    fn any_session_escalated(&self) -> bool {
        let state_map = self
            .auto_escalation_state
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        state_map
            .values()
            .any(|s| s.pre_escalation_mode.is_some() && s.observer.is_active())
    }

    /// Toggle QoS quality ranking at runtime (orthogonal to mode).
    pub fn set_qos_ranking(&self, enabled: bool) {
        self.qos_ranking.store(enabled, Ordering::Relaxed);
        info!(enabled, "QoS quality ranking toggled");
    }

    /// Install the content classifier (M6.6). `None` disables — the router
    /// then behaves as if the classifier contract did not exist.
    ///
    /// Wiring for credential-pool-aware lane selection (M6.5) consumes the
    /// classifier's tier through `classify_turn()`. Until M6.5 lands the
    /// tier is emitted as an event + counter only.
    pub fn set_content_classifier(&self, classifier: Option<Arc<ContentClassifier>>) {
        *self.classifier.write().unwrap() = classifier;
    }

    /// Install the routing-decision observer. The agent wires this to emit
    /// the `routing.decision` harness event and bump the metric counter.
    pub fn set_routing_decision_callback(&self, cb: Option<RoutingDecisionCallback>) {
        *self.decision_callback.write().unwrap() = cb;
    }

    /// Classify the latest user turn and notify observers.
    ///
    /// Returns the decision so callers (and future M6.5 credential-pool lane
    /// selection) can act on it. Returns `None` when no classifier is
    /// attached, letting the router stay on its existing code path.
    pub fn classify_turn(&self, messages: &[Message]) -> Option<ClassificationDecision> {
        let classifier = self.classifier.read().unwrap().clone()?;
        let input = latest_user_text(messages);
        let decision = classifier.classify(&input);
        if let Some(cb) = self.decision_callback.read().unwrap().as_ref() {
            cb(&decision);
        }
        Some(decision)
    }

    /// Pre-seed metrics from benchmark baseline data so the router starts
    /// with informed scores instead of cold-start heuristics.
    ///
    /// Each entry is matched by `provider_name/model_id` (e.g. "gemini/gemini-2.5-flash").
    /// Matching uses substring: if the slot's `provider_name()` contains the entry's
    /// provider prefix AND `model_id()` contains the entry's model suffix, it matches.
    ///
    /// Seeded data uses a small synthetic sample count (10 success, N failure)
    /// so that real traffic quickly dominates via EMA.
    pub fn seed_baseline(&self, entries: &[BaselineEntry]) {
        for slot in &self.slots {
            let pname = slot.provider.provider_name();
            let model = slot.provider.model_id();
            let slot_key = format!("{}/{}", pname, model);

            if let Some(entry) = entries
                .iter()
                .find(|e| slot_key == e.provider || (slot_key.contains(&e.provider)))
            {
                let latency_us = entry.avg_latency_ms * 1000;
                let p95_us = entry.p95_latency_ms * 1000;

                // Seed EMA and P95
                slot.metrics
                    .latency_ema_us
                    .store(latency_us, Ordering::Relaxed);
                slot.metrics.p95_latency_us.store(p95_us, Ordering::Relaxed);

                // Seed latency buffer with a few synthetic samples around the average
                if let Ok(mut samples) = slot.metrics.latency_samples.lock() {
                    for _ in 0..5 {
                        samples.push(latency_us);
                    }
                    samples.push(p95_us); // one high sample for p95
                }

                // Seed success/failure counts based on stability score
                // Use small counts (10 total) so real traffic dominates quickly
                let total = 10u32;
                let failures = ((1.0 - entry.stability) * total as f64).round() as u32;
                let successes = total - failures;
                slot.metrics
                    .success_count
                    .store(successes, Ordering::Relaxed);
                slot.metrics
                    .failure_count
                    .store(failures, Ordering::Relaxed);

                // Mark as recently active so it's not considered stale
                let now = now_epoch_us();
                slot.metrics.last_success_us.store(now, Ordering::Relaxed);
                slot.metrics.last_request_us.store(now, Ordering::Relaxed);
                slot.metrics.total_requests.store(total, Ordering::Relaxed);

                info!(
                    provider = slot_key,
                    latency_ms = entry.avg_latency_ms,
                    p95_ms = entry.p95_latency_ms,
                    stability = format!("{:.0}%", entry.stability * 100.0),
                    "seeded baseline metrics"
                );
            }
        }
    }

    /// Seed static catalog fields (type, cost, ds_output) from a model catalog file.
    /// Call after `seed_baseline()` — this sets the non-QoS fields.
    pub fn seed_catalog(&self, entries: &[ModelCatalogEntry]) {
        for slot in &self.slots {
            let provider_name = slot.provider.provider_name();
            let model_id = slot.provider.model_id();
            let slot_key = format!("{provider_name}/{model_id}");
            // Prefer an exact match — a runtime-saved catalog stores the
            // host-tagged lane key (`moonshot@autodl/kimi-k2.5`). Fall back to
            // the normalized bare-family key so the canonical catalog, which
            // uses untagged families (`moonshot/kimi-k2.5`), still seeds an
            // OpenAI-compatible lane whose provider_name carries an `@host`
            // suffix — otherwise that lane's type/cost/context/QoS is skipped.
            let bare_key = format!("{}/{model_id}", normalized_provider_name(provider_name));
            if let Some(entry) = entries
                .iter()
                .find(|e| e.provider == slot_key)
                .or_else(|| entries.iter().find(|e| e.provider == bare_key))
            {
                slot.model_type
                    .store(entry.model_type.to_u8(), Ordering::Relaxed);
                slot.cost_in
                    .store(entry.cost_in.to_bits(), Ordering::Relaxed);
                if entry.cost_in > 0.0 {
                    slot.seeded_cost_in
                        .store(entry.cost_in.to_bits(), Ordering::Relaxed);
                }
                if entry.cost_out > 0.0 {
                    slot.seeded_cost_out
                        .store(entry.cost_out.to_bits(), Ordering::Relaxed);
                }
                slot.ds_output.store(entry.ds_output, Ordering::Relaxed);
                if entry.ds_output > 0 {
                    slot.seeded_ds_output
                        .store(entry.ds_output, Ordering::Relaxed);
                }
                // Store baseline values for fallback when no live data exists
                slot.baseline_stability
                    .store(entry.stability.to_bits(), Ordering::Relaxed);
                slot.baseline_tool_avg_ms
                    .store(entry.tool_avg_ms, Ordering::Relaxed);
                slot.baseline_p95_ms.store(entry.p95_ms, Ordering::Relaxed);
                // Only update context_window and max_output if catalog has non-zero values.
                // Runtime-saved catalogs may have zeros — preserve existing values.
                if entry.context_window > 0 {
                    slot.context_window
                        .store(entry.context_window, Ordering::Relaxed);
                }
                if entry.max_output > 0 {
                    slot.max_output.store(entry.max_output, Ordering::Relaxed);
                }
                info!(
                    provider = slot_key,
                    model_type = %entry.model_type,
                    cost_in = entry.cost_in,
                    cost_out = entry.cost_out,
                    ds_output = entry.ds_output,
                    "seeded catalog entry"
                );
            }
        }
    }

    /// Export the unified model catalog with live QoS blended into baseline data.
    /// Uses EMA blending: as more live data accumulates, it gradually replaces the baseline.
    /// Formula: blended = baseline * (1 - weight) + live * weight
    /// Weight grows with sample count: weight = min(1.0, total_calls / 10.0)
    /// This ensures cold-start providers keep their benchmark values while active
    /// providers smoothly transition to real-world metrics.
    pub fn export_model_catalog(&self) -> QosCatalog {
        let models: Vec<ModelCatalogEntry> = self
            .slots
            .iter()
            .map(|s| {
                let snap = s.metrics.snapshot();
                let total = snap.success_count + snap.failure_count;

                let baseline_stab = f64::from_bits(s.baseline_stability.load(Ordering::Relaxed));
                let baseline_avg = s.baseline_tool_avg_ms.load(Ordering::Relaxed) as f64;
                let baseline_p95 = s.baseline_p95_ms.load(Ordering::Relaxed) as f64;

                // Micro-adjustment weight: ramps slowly, capped at 0.5 so the
                // catalog baseline always retains at least 50% influence.
                // This prevents runtime metrics from zeroing out seeded baselines.
                let weight = (total as f64 / 20.0).min(0.5);

                let live_stab = if total > 0 {
                    snap.success_count as f64 / total as f64
                } else {
                    baseline_stab // no observations → preserve baseline unchanged
                };
                let live_avg = if snap.latency_ema_ms > 0.0 {
                    snap.latency_ema_ms
                } else {
                    baseline_avg
                };
                let live_p95 = if snap.p95_latency_ms > 0.0 {
                    snap.p95_latency_ms
                } else {
                    baseline_p95
                };

                // Blend: baseline anchors the score, runtime nudges it
                let stability = baseline_stab * (1.0 - weight) + live_stab * weight;
                let tool_avg_ms = (baseline_avg * (1.0 - weight) + live_avg * weight) as u64;
                let p95_ms = (baseline_p95 * (1.0 - weight) + live_p95 * weight) as u64;

                ModelCatalogEntry {
                    provider: format!("{}/{}", s.provider.provider_name(), s.provider.model_id()),
                    model_type: ModelType::from_u8(s.model_type.load(Ordering::Relaxed)),
                    // A live QoS row describes observed behaviour, never which
                    // model a family defaults to — that fact belongs only to
                    // the canonical catalog.
                    is_family_default: false,
                    stability,
                    tool_avg_ms,
                    p95_ms,
                    score: self.score(s),
                    cost_in: {
                        let runtime = f64::from_bits(s.cost_in.load(Ordering::Relaxed));
                        let seeded = f64::from_bits(s.seeded_cost_in.load(Ordering::Relaxed));
                        if runtime > 0.0 { runtime } else { seeded }
                    },
                    cost_out: {
                        let runtime = s.cost_per_m;
                        let seeded = f64::from_bits(s.seeded_cost_out.load(Ordering::Relaxed));
                        if runtime > 0.0 { runtime } else { seeded }
                    },
                    ds_output: {
                        let runtime = s.ds_output.load(Ordering::Relaxed);
                        let seeded = s.seeded_ds_output.load(Ordering::Relaxed);
                        if runtime > 0 { runtime } else { seeded }
                    },
                    context_window: {
                        let v = s.context_window.load(Ordering::Relaxed);
                        if v > 0 {
                            v
                        } else {
                            crate::context::context_window_tokens(s.provider.model_id()) as u64
                        }
                    },
                    max_output: {
                        let v = s.max_output.load(Ordering::Relaxed);
                        if v > 0 {
                            v
                        } else {
                            crate::context::max_output_tokens(s.provider.model_id()) as u64
                        }
                    },
                }
            })
            .collect();

        QosCatalog {
            updated_at: chrono::Utc::now().to_rfc3339(),
            models,
        }
    }

    /// Get the name of the currently selected provider (most recent selection).
    pub fn current_provider_name(&self) -> &str {
        let idx = self.last_selected.load(Ordering::Relaxed) as usize;
        self.slots
            .get(idx)
            .map(|s| s.provider.provider_name())
            .unwrap_or("unknown")
    }

    /// Get the current adaptive feature status (for dashboard / chat commands).
    pub fn adaptive_status(&self) -> AdaptiveStatus {
        AdaptiveStatus {
            mode: self.mode(),
            qos_ranking: self.qos_ranking.load(Ordering::Relaxed),
            failure_threshold: self.config.failure_threshold,
            provider_count: self.slots.len(),
        }
    }

    /// Get metrics snapshots for all providers (for observability / dashboard).
    pub fn metrics_snapshots(&self) -> Vec<(&str, &str, MetricsSnapshot)> {
        self.slots
            .iter()
            .map(|s| {
                (
                    s.provider.provider_name(),
                    s.provider.model_id(),
                    s.metrics.snapshot(),
                )
            })
            .collect()
    }

    /// Export metrics in the shared file format (sorted by score, lowest first).
    pub fn export_shared_metrics(&self) -> SharedMetrics {
        let mut providers: Vec<SharedProviderMetrics> = self
            .slots
            .iter()
            .map(|s| SharedProviderMetrics {
                provider: s.provider.provider_name().to_string(),
                model: s.provider.model_id().to_string(),
                score: self.score(s),
                metrics: s.metrics.snapshot(),
            })
            .collect();
        providers.sort_by(|a, b| {
            a.score
                .partial_cmp(&b.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        SharedMetrics {
            updated_at: chrono::Utc::now().to_rfc3339(),
            policy: SharedPolicy {
                ema_alpha: self.config.ema_alpha,
                failure_threshold: self.config.failure_threshold,
                latency_threshold_ms: self.config.latency_threshold_ms,
                error_rate_threshold: self.config.error_rate_threshold,
                probe_probability: self.config.probe_probability,
                probe_interval_secs: self.config.probe_interval_secs,
                weight_latency: self.config.weight_latency,
                weight_error_rate: self.config.weight_error_rate,
                weight_priority: self.config.weight_priority,
                weight_cost: self.config.weight_cost,
            },
            providers,
        }
    }

    /// Normalized cost for a slot (0..1). Providers with unknown cost (0.0) get 0.
    fn norm_cost(&self, slot: &AdaptiveSlot) -> f64 {
        if self.config.weight_cost <= 0.0 {
            return 0.0;
        }
        // Use cost_per_m if set, otherwise fall back to catalog cost_in
        let slot_cost = if slot.cost_per_m > 0.0 {
            slot.cost_per_m
        } else {
            f64::from_bits(slot.cost_in.load(Ordering::Relaxed))
        };
        if slot_cost <= 0.0 {
            return 0.5; // unknown cost — neutral score
        }
        let max_cost = self
            .slots
            .iter()
            .map(|s| {
                if s.cost_per_m > 0.0 {
                    s.cost_per_m
                } else {
                    f64::from_bits(s.cost_in.load(Ordering::Relaxed))
                }
            })
            .fold(0.0_f64, f64::max);
        if max_cost > 0.0 {
            slot_cost / max_cost
        } else {
            0.5
        }
    }

    /// Score a provider. Lower is better.
    ///
    /// Four factors:
    ///   - **Stability** (35%): blended baseline + live error rate. Does it complete reliably?
    ///   - **Quality** (30%, only when QoS ranking is on): catalog ds_output × stability.
    ///   - **Throughput** (20%): output tokens per second. Task-normalized speed.
    ///     Raw latency is NOT used — it depends on task complexity, not provider quality.
    ///   - **Cost** (15%): normalized output cost. Cheaper is better when quality is similar.
    fn score(&self, slot: &AdaptiveSlot) -> f64 {
        let total = slot.metrics.success_count.load(Ordering::Relaxed)
            + slot.metrics.failure_count.load(Ordering::Relaxed);

        // EMA blend weight: ramps from 0 (cold start) to 0.5 (cap) over 20 calls.
        // Baseline always retains ≥50% influence.
        let weight = (total as f64 / 20.0).min(0.5);

        // ── Stability ──
        // No data = neutral (0.5). Only observed data moves the score.
        let baseline_stab = f64::from_bits(slot.baseline_stability.load(Ordering::Relaxed));
        let baseline_err = if baseline_stab > 0.0 {
            1.0 - baseline_stab
        } else {
            0.5 // no data → neutral
        };
        let live_err_rate = if total > 0 {
            slot.metrics.error_rate()
        } else {
            0.5
        };
        let blended_err = baseline_err * (1.0 - weight) + live_err_rate * weight;

        // ── Quality ──
        // No data = neutral (0.5). Cost is the differentiator, not unobserved quality.
        let ds = slot.ds_output.load(Ordering::Relaxed) as f64;
        let max_ds = self
            .slots
            .iter()
            .map(|s| s.ds_output.load(Ordering::Relaxed) as f64)
            .fold(0.0_f64, f64::max);
        let norm_quality = if max_ds > 0.0 && ds > 0.0 {
            1.0 - (ds / max_ds)
        } else {
            0.5 // no data → neutral
        };

        // ── Throughput ──
        let throughput = slot.metrics.throughput();
        let max_throughput = self
            .slots
            .iter()
            .map(|s| s.metrics.throughput())
            .fold(0.0_f64, f64::max);
        let norm_throughput = if max_throughput > 0.0 && throughput > 0.0 {
            1.0 - (throughput / max_throughput)
        } else {
            0.5 // no data → neutral
        };

        // ── Priority ──
        let max_priority = self.slots.len().max(1) as f64;
        let norm_priority = slot.priority as f64 / max_priority;

        // ── Cost ──
        let norm_cost = self.norm_cost(slot);

        let ranking_component = if self.qos_ranking.load(Ordering::Relaxed) {
            0.6 * norm_quality + 0.4 * norm_throughput
        } else {
            norm_throughput
        };

        let we = self.config.weight_error_rate;
        let wl = self.config.weight_latency;
        let wp = self.config.weight_priority;
        let wc = self.config.weight_cost;
        we * blended_err + wl * ranking_component + wp * norm_priority + wc * norm_cost
    }

    /// RFC-3 (#1292) — when a per-turn [`crate::LaneContext`] is in
    /// scope and resolves to a non-`General` lane with at least one
    /// `(provider, model)` candidate that matches a router slot,
    /// return the matching slot indices in candidate order.
    ///
    /// Returns `None` for:
    /// - no `LANE_CONTEXT` active (outside a `with_lane_context`
    ///   scope — test paths, gateway pre-RFC-3 sessions)
    /// - lane is `General` or `None` (semantics: "no filter")
    /// - candidate list is empty
    /// - candidate list matches no provider in the chain
    ///
    /// All three "None" cases preserve the pre-RFC-3 behavior: every
    /// slot remains eligible for selection. This is the backward
    /// compat anchor — profiles that don't carry a topic resolve to
    /// `General`, get `None` back, and never observe a behavior
    /// change.
    ///
    /// **Codex P2 follow-up:** `provider_name()` for OpenAI-compatible
    /// providers (DeepSeek / Moonshot / Wisemodel via
    /// `OpenAIProvider::with_base_url`) is endpoint-tagged as
    /// `name@endpoint` (e.g. `moonshot@autodl`). Lane defaults and
    /// profile config use untagged family identifiers, so the
    /// comparison normalizes via [`normalized_provider_name`] which
    /// strips any `@suffix` before matching. Without this
    /// normalization, `code:*` against a Wisemodel-backed profile
    /// would produce zero matches and silently fall through.
    fn lane_filtered_slot_indices(&self) -> Option<Vec<usize>> {
        let ctx = crate::lane::current_lane_context();
        let lane = ctx.lane?;
        if lane == crate::Lane::General {
            return None;
        }
        let candidates = ctx.candidates();
        if candidates.is_empty() {
            return None;
        }
        let mut matched: Vec<usize> = Vec::new();
        for (want_provider, want_model) in &candidates {
            // Codex P2 follow-up #2: when a profile override
            // specifies a tagged candidate like `moonshot@autodl`,
            // honor the tag with an exact match so operators can
            // pin to a specific endpoint. Untagged candidates
            // (built-in defaults, plain family names) match against
            // the normalized slot label so endpoint-tagged slots
            // still light up. The decision is per-candidate, not
            // per-slot: a tagged candidate only matches a tagged
            // slot with the exact same label.
            let want_is_tagged = want_provider.contains('@');
            for (i, slot) in self.slots.iter().enumerate() {
                let slot_name = slot.provider.provider_name();
                let provider_matches = if want_is_tagged {
                    slot_name == want_provider.as_str()
                } else {
                    normalized_provider_name(slot_name) == want_provider.as_str()
                };
                if provider_matches
                    && slot.provider.model_id() == want_model
                    && !matched.contains(&i)
                {
                    matched.push(i);
                }
            }
        }
        if matched.is_empty() {
            // Candidates exist but none of them are in this chain.
            // Fall through to default selection rather than starving
            // the router. This matches the RFC-3 "lane defaults must
            // not break existing profiles" requirement.
            debug!(
                lane = lane.as_str(),
                "lane filter resolved zero matching slots; falling through to default selection"
            );
            return None;
        }
        debug!(
            lane = lane.as_str(),
            matched = matched.len(),
            total = self.slots.len(),
            "lane filter narrowed candidate slots"
        );
        Some(matched)
    }

    /// The DETERMINISTIC selection — everything `select_provider` does
    /// except the stochastic probe redirect (#2135 round-7 P1: the
    /// identity/readiness accessors need the same mode/lane rules and the
    /// same ASCENDING score order as real routing, not a parallel
    /// reimplementation; an earlier cut used max_by against a
    /// lower-is-better score and preferred the worst lane).
    ///
    /// - Off / Hedge: priority order, skip circuit-broken only.
    ///   (Hedge mode uses this to pick the primary for racing.)
    /// - Lane: score-based selection across all providers.
    ///
    /// RFC-3 (#1292): when a per-turn lane is in scope, the eligible
    /// set is narrowed to slots whose `(provider_name, model_id)` is
    /// in the lane's candidate list. When the lane filter yields
    /// zero matches we fall through to the full slot list so the
    /// router never starves (see [`Self::lane_filtered_slot_indices`]).
    fn select_provider_deterministic(&self) -> usize {
        let mode = self.mode();
        // RFC-3: lane-filtered eligible set, if any. None ⇒ no
        // filter; behave identically to pre-RFC-3.
        let lane_eligible = self.lane_filtered_slot_indices();

        // Off and Hedge both use priority order for the primary selection.
        // (Hedge picks the alternate separately in hedged_chat.)
        if mode != AdaptiveMode::Lane {
            // RFC-3: when a lane filter is active, walk the lane's
            // candidate list in declared order rather than the full
            // priority order. The first non-circuit-broken match
            // wins. Falls through to the full slot list if every
            // lane candidate has a circuit-open breaker.
            if let Some(ref eligible) = lane_eligible {
                for &i in eligible {
                    let slot = &self.slots[i];
                    if !slot.metrics.is_circuit_open(self.config.failure_threshold) {
                        let prev = self.last_selected.swap(i as u32, Ordering::Relaxed);
                        if prev != i as u32 {
                            info!(
                                from = self
                                    .slots
                                    .get(prev as usize)
                                    .map(|s| s.provider.provider_name())
                                    .unwrap_or("?"),
                                to = slot.provider.provider_name(),
                                "provider failover (lane filter, lane changing disabled)"
                            );
                        }
                        return i;
                    }
                }
                // All lane candidates circuit-broken → fall through
                // to the wider priority walk below.
            }
            for (i, slot) in self.slots.iter().enumerate() {
                if !slot.metrics.is_circuit_open(self.config.failure_threshold) {
                    let prev = self.last_selected.swap(i as u32, Ordering::Relaxed);
                    if prev != i as u32 {
                        info!(
                            from = self
                                .slots
                                .get(prev as usize)
                                .map(|s| s.provider.provider_name())
                                .unwrap_or("?"),
                            to = slot.provider.provider_name(),
                            "provider failover (circuit breaker, lane changing disabled)"
                        );
                    }
                    return i;
                }
            }
            // All circuit-broken — fall through to least-failed logic below
        }

        // Score all non-circuit-broken providers. RFC-3: if a lane
        // filter is active and at least one matching slot is up, the
        // scoring set is restricted to those slots. When the filter
        // produces zero usable slots we fall back to the full chain.
        let mut scored: Vec<(usize, f64)> = self
            .slots
            .iter()
            .enumerate()
            .filter(|(i, s)| {
                if !s.metrics.is_circuit_open(self.config.failure_threshold) {
                    match lane_eligible {
                        Some(ref eligible) => eligible.contains(i),
                        None => true,
                    }
                } else {
                    false
                }
            })
            .map(|(i, s)| (i, self.score(s)))
            .collect();
        // RFC-3 fall-through: if the lane filter excluded every
        // remaining slot, redo without the lane filter so the router
        // doesn't starve under transient lane outages.
        if scored.is_empty() && lane_eligible.is_some() {
            scored = self
                .slots
                .iter()
                .enumerate()
                .filter(|(_, s)| !s.metrics.is_circuit_open(self.config.failure_threshold))
                .map(|(i, s)| (i, self.score(s)))
                .collect();
        }

        // If all circuit-broken, pick least-failed
        if scored.is_empty() {
            let best = self
                .slots
                .iter()
                .enumerate()
                .min_by_key(|(_, s)| s.metrics.consecutive_failures.load(Ordering::Relaxed))
                .map(|(i, _)| i)
                .unwrap_or(0);
            warn!(
                provider = self.slots[best].provider.provider_name(),
                "all providers circuit-broken, using least-failed"
            );
            self.last_selected.store(best as u32, Ordering::Relaxed);
            return best;
        }

        scored.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
        let best_idx = scored[0].0;

        // Detect lane change. (This now runs before any probe redirect in
        // `select_provider` — a probe is a measurement detour, not a lane
        // change, so logging the deterministic selection is the accurate
        // record.)
        let prev = self.last_selected.swap(best_idx as u32, Ordering::Relaxed);
        if prev != best_idx as u32 && prev < self.slots.len() as u32 {
            info!(
                from = self.slots[prev as usize].provider.provider_name(),
                to = self.slots[best_idx].provider.provider_name(),
                from_score = format!("{:.3}", self.score(&self.slots[prev as usize])),
                to_score = format!("{:.3}", self.score(&self.slots[best_idx])),
                "adaptive lane change"
            );
        }

        best_idx
    }

    /// Select provider index and whether this is a probe request: the
    /// deterministic selection above, plus the stochastic stale-provider
    /// probe redirect used for actual request routing only.
    fn select_provider(&self) -> (usize, bool) {
        let best_idx = self.select_provider_deterministic();
        // Probe: with some probability, redirect to a stale non-primary provider.
        // RFC-3 (#1292) — codex P2: when a lane filter is active,
        // restrict probe targets to the lane's eligible slots so a
        // probe under `slides:*`/`code:*` can never route the user
        // turn to an out-of-lane model.
        if self.slots.len() > 1 && self.should_probe() {
            let lane_eligible = self.lane_filtered_slot_indices();
            for (i, slot) in self.slots.iter().enumerate() {
                if i != best_idx
                    && slot.metrics.is_stale(self.config.probe_interval_secs)
                    && !slot.metrics.is_circuit_open(self.config.failure_threshold)
                    && match lane_eligible {
                        Some(ref eligible) => eligible.contains(&i),
                        None => true,
                    }
                {
                    debug!(
                        probe_provider = slot.provider.provider_name(),
                        best_provider = self.slots[best_idx].provider.provider_name(),
                        "probing stale provider"
                    );
                    return (i, true);
                }
            }
        }
        (best_idx, false)
    }

    /// Simple RNG for probe decision.
    fn should_probe(&self) -> bool {
        let state = self.rng_state.load(Ordering::Relaxed);
        // xorshift64
        let mut x = state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.rng_state.store(x, Ordering::Relaxed);
        let prob = (x % 1000) as f64 / 1000.0;
        prob < self.config.probe_probability
    }

    /// Race request against two providers. Returns `Some(result)` if a race
    /// was executed, `None` if no second provider is available.
    ///
    /// Both providers record metrics regardless of win/lose — this is how
    /// QoS scores accumulate under hedging. The loser's future is dropped
    /// (cancelled) once the winner completes.
    async fn hedged_chat(
        &self,
        primary_idx: usize,
        messages: &[Message],
        tools: &[ToolSpec],
        config: &ChatConfig,
    ) -> Option<Result<ChatResponse>> {
        // Pick the cheapest alternate provider for hedging. When cost data is
        // available, always hedge with the lowest-cost provider. Falls back to
        // score-based selection when no cost data exists.
        //
        // RFC-3 (#1292) — codex P2: when a lane filter is active,
        // confine hedge alternates to the lane's eligible slots so a
        // race under `slides:*`/`code:*` can't be won by an
        // out-of-lane model. When the filter excludes every
        // alternate, the hedge skips (`None` return) and the caller
        // falls back to the single-provider path against the
        // primary — preserving lane integrity at the cost of
        // hedging on that turn.
        let primary_name = self.slots[primary_idx].provider.provider_name();
        let lane_eligible = self.lane_filtered_slot_indices();
        let candidates: Vec<(usize, &AdaptiveSlot)> = self
            .slots
            .iter()
            .enumerate()
            .filter(|(i, s)| {
                if *i == primary_idx
                    || s.provider.provider_name() == primary_name
                    || s.metrics.is_circuit_open(self.config.failure_threshold)
                {
                    return false;
                }
                match lane_eligible {
                    Some(ref eligible) => eligible.contains(i),
                    None => true,
                }
            })
            .collect();
        let alternate_idx = {
            // Prefer cheapest provider with known cost (cost_per_m > 0)
            let cheapest = candidates
                .iter()
                .filter(|(_, s)| s.cost_per_m > 0.0)
                .min_by(|a, b| {
                    a.1.cost_per_m
                        .partial_cmp(&b.1.cost_per_m)
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .map(|(i, _)| *i);
            // Fall back to best score if no cost data
            cheapest.or_else(|| {
                candidates
                    .iter()
                    .map(|(i, s)| (*i, self.score(s)))
                    .min_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
                    .map(|(i, _)| i)
            })?
        };

        info!(
            primary = self.slots[primary_idx].provider.provider_name(),
            alternate = self.slots[alternate_idx].provider.provider_name(),
            "hedged race: firing to 2 providers"
        );

        // Race! Both futures start simultaneously. When one completes, the
        // other is dropped (cancelled). Both record_success/record_failure
        // in try_chat before returning, so the winner's metrics are captured.
        // The loser's metrics are NOT recorded (future dropped mid-flight) —
        // this is correct: we only score completed requests.
        tokio::select! {
            result = self.try_chat(primary_idx, messages, tools, config) => {
                match &result {
                    Ok(_) => info!(
                        winner = self.slots[primary_idx].provider.provider_name(),
                        loser = self.slots[alternate_idx].provider.provider_name(),
                        "hedged race: primary won"
                    ),
                    Err(e) => warn!(
                        provider = self.slots[primary_idx].provider.provider_name(),
                        error = %e,
                        "hedged race: primary failed, waiting for alternate"
                    ),
                }
                if result.is_ok() {
                    return Some(result);
                }
                // Primary failed — try alternate sequentially (it was cancelled by select)
                Some(self.try_chat(alternate_idx, messages, tools, config).await)
            }
            result = self.try_chat(alternate_idx, messages, tools, config) => {
                match &result {
                    Ok(_) => info!(
                        winner = self.slots[alternate_idx].provider.provider_name(),
                        loser = self.slots[primary_idx].provider.provider_name(),
                        "hedged race: alternate won"
                    ),
                    Err(e) => warn!(
                        provider = self.slots[alternate_idx].provider.provider_name(),
                        error = %e,
                        "hedged race: alternate failed, waiting for primary"
                    ),
                }
                if result.is_ok() {
                    return Some(result);
                }
                // Alternate failed — try primary sequentially
                Some(self.try_chat(primary_idx, messages, tools, config).await)
            }
        }
    }

    /// Try a request on a specific provider, returning result and latency.
    async fn try_chat(
        &self,
        idx: usize,
        messages: &[Message],
        tools: &[ToolSpec],
        config: &ChatConfig,
    ) -> Result<ChatResponse> {
        let start = Instant::now();
        let result = self.slots[idx].provider.chat(messages, tools, config).await;
        let elapsed_us = start.elapsed().as_micros() as u64;

        match &result {
            Ok(resp) => {
                self.slots[idx]
                    .metrics
                    .record_success_with_alpha(elapsed_us, self.config.ema_alpha);
                self.slots[idx].metrics.record_throughput(
                    resp.usage.output_tokens,
                    elapsed_us,
                    self.config.ema_alpha,
                );
                let total = self.slots[idx]
                    .metrics
                    .total_requests
                    .load(Ordering::Relaxed);
                if total % 10 == 0 && total > 0 {
                    let snap = self.slots[idx].metrics.snapshot();
                    info!(
                        provider = self.slots[idx].provider.provider_name(),
                        model = self.slots[idx].provider.model_id(),
                        latency_ema_ms = format!("{:.0}", snap.latency_ema_ms),
                        p95_ms = format!("{:.0}", snap.p95_latency_ms),
                        error_rate = format!("{:.1}%", snap.error_rate * 100.0),
                        total_requests = total,
                        "adaptive router metrics"
                    );
                }
            }
            Err(e) => {
                self.slots[idx].metrics.record_failure();
                let consec = self.slots[idx]
                    .metrics
                    .consecutive_failures
                    .load(Ordering::Relaxed);
                if consec == self.config.failure_threshold {
                    warn!(
                        provider = self.slots[idx].provider.provider_name(),
                        consecutive_failures = consec,
                        "provider circuit breaker opened"
                    );
                }
                self.notify_credential_failure_from_error(idx, e).await;
            }
        }

        result.map(|mut response| {
            response.provider_index = Some(idx);
            response
        })
    }

    /// Classify `err` and forward the failure to slot `idx`'s credential
    /// pool (if attached). Runs once per error — the pool itself enforces
    /// at-most-once OAuth refresh per error id via its own guard.
    async fn notify_credential_failure_from_error(&self, idx: usize, err: &eyre::Report) {
        let text = err.to_string().to_lowercase();
        let is_auth = text.contains("401")
            || text.contains("403")
            || text.contains("authentication")
            || text.contains("unauthorized");
        let is_rate_limit = text.contains("429") || text.contains("rate limit");
        if is_auth {
            self.notify_credential_failure(idx, true, None, ErrorId::fresh())
                .await;
        } else if is_rate_limit {
            self.notify_credential_failure(idx, false, None, ErrorId::fresh())
                .await;
        }
    }

    /// Try a stream request on a specific provider.
    async fn try_chat_stream(
        &self,
        idx: usize,
        messages: &[Message],
        tools: &[ToolSpec],
        config: &ChatConfig,
    ) -> Result<ChatStream> {
        let start = Instant::now();
        let result = self.slots[idx]
            .provider
            .chat_stream(messages, tools, config)
            .await;
        let elapsed_us = start.elapsed().as_micros() as u64;

        match &result {
            Ok(_) => {
                // For streaming, we only measure time-to-first-byte (stream init)
                self.slots[idx]
                    .metrics
                    .record_success_with_alpha(elapsed_us, self.config.ema_alpha);
            }
            Err(e) => {
                self.slots[idx].metrics.record_failure();
                self.notify_credential_failure_from_error(idx, e).await;
            }
        }

        result.map(|stream| self.stream_with_provider_index(idx, stream))
    }

    fn stream_with_provider_index(&self, idx: usize, stream: ChatStream) -> ChatStream {
        Box::pin(
            futures::stream::once(async move { StreamEvent::ProviderIndex(idx) }).chain(stream),
        )
    }
}

#[async_trait]
impl LlmProvider for AdaptiveRouter {
    async fn chat(
        &self,
        messages: &[Message],
        tools: &[ToolSpec],
        config: &ChatConfig,
    ) -> Result<ChatResponse> {
        // Invariant #5 of issue #493: classify BEFORE selecting a lane so the
        // decision is observable even if lane selection fails. M6.5 will
        // consume the returned tier for credential-pool-aware selection;
        // today the downstream router code path remains unchanged so
        // `enabled: false` configs see identical behavior (invariant #2).
        let _classifier_decision = self.classify_turn(messages);
        let mode = self.mode();
        let (start_idx, is_probe) = self.select_provider();

        debug!(
            selected = self.slots[start_idx].provider.provider_name(),
            model = self.slots[start_idx].provider.model_id(),
            is_probe = is_probe,
            %mode,
            score = format!("{:.3}", self.score(&self.slots[start_idx])),
            "adaptive router selected provider"
        );

        let fail_fast = crate::current_llm_call_policy() == crate::LlmCallPolicy::FailFast;

        // ── Hedged racing: fire to 2 providers, take the winner ────────
        if !fail_fast && mode == AdaptiveMode::Hedge && self.slots.len() > 1 {
            if let Some(result) = self.hedged_chat(start_idx, messages, tools, config).await {
                return result;
            }
        }

        // ── Single-provider path (Off / Lane / fallthrough) ────────────
        // Wave4-A: track wall time from the first attempt so the failover
        // event's `elapsed_ms` reflects the user-visible latency before
        // the lane change, not just the time spent in the failover loop.
        let failover_started = Instant::now();
        match self.try_chat(start_idx, messages, tools, config).await {
            Ok(resp) => Ok(resp),
            Err(e) => {
                if self.slots.len() == 1 || fail_fast {
                    return Err(e);
                }

                warn!(
                    provider = self.slots[start_idx].provider.provider_name(),
                    error = %e,
                    "adaptive router failing over"
                );

                // Failover: try remaining providers in score order.
                // RFC-3 (#1292): when a lane filter is active, prefer
                // the lane's remaining candidates. If every lane
                // candidate is exhausted or excluded, fall back to the
                // full unfiltered set so the router never starves.
                let lane_eligible = self.lane_filtered_slot_indices();
                let scored = build_failover_candidates(self, start_idx, lane_eligible.as_ref());

                let mut last_error = e;
                for (idx, _) in scored {
                    self.emit_status(format!(
                        "Switching to {}...",
                        self.slots[idx].provider.provider_name()
                    ));
                    // Wave4-A: publish per-attempt failover events. We
                    // emit BEFORE the retry so a client can render the
                    // transition even if the retry never succeeds.
                    let from_key = format!(
                        "{}/{}",
                        self.slots[start_idx].provider.provider_name(),
                        self.slots[start_idx].provider.model_id()
                    );
                    let to_key = format!(
                        "{}/{}",
                        self.slots[idx].provider.provider_name(),
                        self.slots[idx].provider.model_id()
                    );
                    self.publish_failover(
                        &from_key,
                        &to_key,
                        &format!("chat_error: {last_error}"),
                        failover_started
                            .elapsed()
                            .as_millis()
                            .min(u128::from(u64::MAX)) as u64,
                    );
                    match self.try_chat(idx, messages, tools, config).await {
                        Ok(resp) => return Ok(resp),
                        Err(e) => {
                            warn!(
                                provider = self.slots[idx].provider.provider_name(),
                                error = %e,
                                "adaptive router failover also failed"
                            );
                            last_error = e;
                        }
                    }
                }
                Err(last_error)
            }
        }
    }

    async fn chat_stream(
        &self,
        messages: &[Message],
        tools: &[ToolSpec],
        config: &ChatConfig,
    ) -> Result<ChatStream> {
        // Classify the turn before lane selection (see invariant #5 above).
        let _classifier_decision = self.classify_turn(messages);
        let (start_idx, _is_probe) = self.select_provider();
        let fail_fast = crate::current_llm_call_policy() == crate::LlmCallPolicy::FailFast;

        // Wave4-A: failover elapsed-time anchor — see equivalent comment
        // in `chat()` above.
        let failover_started = Instant::now();
        match self
            .try_chat_stream(start_idx, messages, tools, config)
            .await
        {
            Ok(stream) => Ok(stream),
            Err(e) => {
                if self.slots.len() == 1 || fail_fast {
                    return Err(e);
                }

                warn!(
                    provider = self.slots[start_idx].provider.provider_name(),
                    error = %e,
                    "adaptive router failing over stream"
                );

                // RFC-3 (#1292): same lane-aware failover as chat() above.
                let lane_eligible = self.lane_filtered_slot_indices();
                let scored = build_failover_candidates(self, start_idx, lane_eligible.as_ref());

                let mut last_error = e;
                for (idx, _) in scored {
                    self.emit_status(format!(
                        "Switching to {}...",
                        self.slots[idx].provider.provider_name()
                    ));
                    let from_key = format!(
                        "{}/{}",
                        self.slots[start_idx].provider.provider_name(),
                        self.slots[start_idx].provider.model_id()
                    );
                    let to_key = format!(
                        "{}/{}",
                        self.slots[idx].provider.provider_name(),
                        self.slots[idx].provider.model_id()
                    );
                    self.publish_failover(
                        &from_key,
                        &to_key,
                        &format!("stream_error: {last_error}"),
                        failover_started
                            .elapsed()
                            .as_millis()
                            .min(u128::from(u64::MAX)) as u64,
                    );
                    match self.try_chat_stream(idx, messages, tools, config).await {
                        Ok(stream) => return Ok(stream),
                        Err(e) => {
                            warn!(
                                provider = self.slots[idx].provider.provider_name(),
                                error = %e,
                                "adaptive router failover also failed"
                            );
                            last_error = e;
                        }
                    }
                }
                Err(last_error)
            }
        }
    }

    // #2135 round-6 P1: the sizing accessors take the MINIMUM across all
    // slots — selection-INDEPENDENT and safe for every route this router
    // can take (Lane-mode stochastic probes, hedged sends, failover): a
    // prompt sized for a probed 256K local primary must not reach a 32K
    // lane. Per-route resizing at dispatch is the precise fix and belongs
    // to the router itself; until then the conservative envelope is the
    // pre-probe catalog behavior restored, minus its staleness.
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
        // Readiness preps the lane the router would DETERMINISTICALLY
        // prefer (best score, no stochastic exploration — #2135 round-6
        // P1): readiness may preload, and a coin-flipped lane must not
        // decide which model gets loaded into memory.
        let idx = self.select_provider_deterministic();
        self.slots[idx].provider.ensure_ready().await;
    }

    fn model_id(&self) -> &str {
        self.slots[self.select_provider_deterministic()]
            .provider
            .model_id()
    }

    fn provider_name(&self) -> &str {
        self.slots[self.select_provider_deterministic()]
            .provider
            .provider_name()
    }

    fn provider_metadata(&self) -> ProviderMetadata {
        self.slots[self.select_provider_deterministic()]
            .provider
            .provider_metadata()
    }

    fn provider_metadata_for_index(&self, provider_index: Option<usize>) -> ProviderMetadata {
        let idx = provider_index.unwrap_or_else(|| self.select_provider().0);
        self.slots
            .get(idx)
            .map(|slot| slot.provider.provider_metadata())
            .unwrap_or_else(|| self.provider_metadata())
    }

    fn export_metrics(&self) -> Option<serde_json::Value> {
        serde_json::to_value(self.export_model_catalog()).ok()
    }

    fn report_late_failure(&self) {
        let (idx, _) = self.select_provider();
        self.slots[idx].metrics.record_failure();
        let consec = self.slots[idx]
            .metrics
            .consecutive_failures
            .load(std::sync::atomic::Ordering::Relaxed);
        if consec >= self.config.failure_threshold {
            warn!(
                provider = self.slots[idx].provider.provider_name(),
                consecutive_failures = consec,
                "provider circuit breaker opened (late failure)"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Normalize a `provider_name()` for RFC-3 (#1292) lane matching.
///
/// `OpenAIProvider::with_base_url` tags the provider label with the
/// endpoint suffix when a non-canonical base URL is in play
/// (`moonshot@autodl`, `deepseek@api`, etc. — see
/// `openai.rs:with_base_url`). Lane defaults and profile config use
/// untagged family identifiers (`moonshot`, `deepseek`,
/// `wisemodel`), so this helper strips the `@suffix` before
/// comparison. Anthropic / Gemini / native OpenAI return their bare
/// name and pass through unchanged.
fn normalized_provider_name(name: &str) -> &str {
    name.split_once('@').map(|(p, _)| p).unwrap_or(name)
}

/// Build the score-ordered failover candidate list for a router after
/// the primary slot has errored. RFC-3 (#1292) preference: when
/// `lane_eligible` is `Some(_)`, only slots in that set are
/// considered. If that lane-filtered set is empty (every candidate
/// was either the primary or circuit-broken), the function silently
/// falls back to the full slot list so the router doesn't starve on
/// transient lane outages.
///
/// Shared between `chat()` and `chat_stream()` so both code paths
/// retain consistent failover behavior under lane filtering.
fn build_failover_candidates(
    router: &AdaptiveRouter,
    start_idx: usize,
    lane_eligible: Option<&Vec<usize>>,
) -> Vec<(usize, f64)> {
    let circuit_threshold = router.config.failure_threshold;
    let mut scored: Vec<(usize, f64)> = router
        .slots
        .iter()
        .enumerate()
        .filter(|(i, s)| {
            if *i == start_idx || s.metrics.is_circuit_open(circuit_threshold) {
                return false;
            }
            match lane_eligible {
                Some(eligible) => eligible.contains(i),
                None => true,
            }
        })
        .map(|(i, s)| (i, router.score(s)))
        .collect();
    if scored.is_empty() && lane_eligible.is_some() {
        // RFC-3 fall-through: lane filter excluded every remaining
        // slot; widen to the full set so failover still has somewhere
        // to go. The starting slot has already errored, so the
        // surviving candidates come from outside the lane.
        scored = router
            .slots
            .iter()
            .enumerate()
            .filter(|(i, s)| *i != start_idx && !s.metrics.is_circuit_open(circuit_threshold))
            .map(|(i, s)| (i, router.score(s)))
            .collect();
    }
    scored.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
    scored
}

fn now_epoch_us() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as u64
}

/// Extract the text of the most recent user message, or fall back to the last
/// message of any role. Returns an empty string if `messages` is empty.
///
/// The classifier runs against the "latest user turn" — this is the stable
/// definition of that input. Keeping it centralized means the router and
/// any future M6.5 credential-pool integration agree on the same slice.
fn latest_user_text(messages: &[Message]) -> String {
    if let Some(msg) = messages
        .iter()
        .rev()
        .find(|m| matches!(m.role, octos_core::MessageRole::User))
    {
        return msg.content.clone();
    }
    messages
        .last()
        .map(|m| m.content.clone())
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[path = "adaptive_tests.rs"]
mod tests;
