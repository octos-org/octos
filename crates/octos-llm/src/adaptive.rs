//! Adaptive provider router with metrics-driven selection.
//!
//! Replaces static priority failover with a scoring system that tracks
//! per-provider latency (EMA + p95), error rates, and circuit breaker state.
//! Supports probe/canary requests to keep metrics fresh for non-primary providers.

use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use async_trait::async_trait;
use eyre::Result;
use octos_core::Message;
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use crate::config::ChatConfig;
use crate::provider::LlmProvider;
use crate::types::{ChatResponse, ChatStream, ToolSpec};

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
    pub weight_latency: f64,
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
/// Static fields (type, cost, ds_output) are loaded from `model_catalog.json`.
/// Dynamic fields (stability, tool_avg_ms, p95_ms, score) are updated by the QoS scanner.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelCatalogEntry {
    /// Provider/model key, e.g. "minimax/MiniMax-M2.7".
    pub provider: String,
    /// Model capability type.
    #[serde(rename = "type")]
    pub model_type: ModelType,
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
    #[expect(dead_code)]
    priority: usize,
    /// Published output price in USD per million tokens (0.0 = unknown/free).
    cost_per_m: f64,
    /// Model capability type (Strong/Fast). Set from catalog seed.
    model_type: Mutex<ModelType>,
    /// Input cost in USD per million tokens. Set from catalog seed.
    cost_in: AtomicU64,
    /// Deep search output quality (token count). Set from catalog seed.
    ds_output: AtomicU64,
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

/// Adaptive provider router with metrics-driven selection.
///
/// Drop-in replacement for `ProviderChain`. Tracks latency and error rates
/// per provider, scores them dynamically, and routes to the best performer.
/// Probes stale providers to keep metrics fresh.
/// Callback for status updates (e.g. failover notifications).
/// The adaptive router calls this to inform the UI layer about provider
/// switches that happen inside `chat_stream()` failover.
pub type StatusCallback = Arc<dyn Fn(String) + Send + Sync>;

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
    status_callback: Mutex<Option<StatusCallback>>,
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
        let slots = providers
            .into_iter()
            .enumerate()
            .map(|(i, p)| AdaptiveSlot {
                provider: p,
                metrics: ProviderMetrics::new(),
                priority: i,
                cost_per_m: costs.get(i).copied().unwrap_or(0.0),
                model_type: Mutex::new(ModelType::Fast), // default, overridden by catalog seed
                cost_in: AtomicU64::new(0),
                ds_output: AtomicU64::new(0),
                baseline_stability: AtomicU64::new(0),
                baseline_tool_avg_ms: AtomicU64::new(0),
                baseline_p95_ms: AtomicU64::new(0),
                context_window: AtomicU64::new(0),
                max_output: AtomicU64::new(0),
            })
            .collect();
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
            status_callback: Mutex::new(None),
        }
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
        *self.status_callback.lock().unwrap() = cb;
    }

    /// Emit a status message through the callback (if set).
    fn emit_status(&self, message: String) {
        if let Some(cb) = self.status_callback.lock().unwrap().as_ref() {
            cb(message);
        }
    }

    /// Toggle QoS quality ranking at runtime (orthogonal to mode).
    pub fn set_qos_ranking(&self, enabled: bool) {
        self.qos_ranking.store(enabled, Ordering::Relaxed);
        info!(enabled, "QoS quality ranking toggled");
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
            let slot_key = format!(
                "{}/{}",
                slot.provider.provider_name(),
                slot.provider.model_id()
            );
            if let Some(entry) = entries.iter().find(|e| e.provider == slot_key) {
                *slot.model_type.lock().unwrap() = entry.model_type;
                slot.cost_in
                    .store(entry.cost_in.to_bits(), Ordering::Relaxed);
                slot.ds_output.store(entry.ds_output, Ordering::Relaxed);
                // Store baseline values for fallback when no live data exists
                slot.baseline_stability
                    .store(entry.stability.to_bits(), Ordering::Relaxed);
                slot.baseline_tool_avg_ms
                    .store(entry.tool_avg_ms, Ordering::Relaxed);
                slot.baseline_p95_ms.store(entry.p95_ms, Ordering::Relaxed);
                slot.context_window
                    .store(entry.context_window, Ordering::Relaxed);
                slot.max_output.store(entry.max_output, Ordering::Relaxed);
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

                // EMA blending weight: 0.0 at cold start, ramps to 1.0 after 10 calls
                let weight = (total as f64 / 10.0).min(1.0);

                let live_stab = if total > 0 {
                    snap.success_count as f64 / total as f64
                } else {
                    baseline_stab
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

                // Blend: baseline * (1 - weight) + live * weight
                let stability = baseline_stab * (1.0 - weight) + live_stab * weight;
                let tool_avg_ms = (baseline_avg * (1.0 - weight) + live_avg * weight) as u64;
                let p95_ms = (baseline_p95 * (1.0 - weight) + live_p95 * weight) as u64;

                ModelCatalogEntry {
                    provider: format!("{}/{}", s.provider.provider_name(), s.provider.model_id()),
                    model_type: *s.model_type.lock().unwrap(),
                    stability,
                    tool_avg_ms,
                    p95_ms,
                    score: self.score(s),
                    cost_in: f64::from_bits(s.cost_in.load(Ordering::Relaxed)),
                    cost_out: s.cost_per_m,
                    ds_output: s.ds_output.load(Ordering::Relaxed),
                    context_window: s.context_window.load(Ordering::Relaxed),
                    max_output: s.max_output.load(Ordering::Relaxed),
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
        if self.config.weight_cost <= 0.0 || slot.cost_per_m <= 0.0 {
            return 0.0;
        }
        let max_cost = self
            .slots
            .iter()
            .map(|s| s.cost_per_m)
            .fold(0.0_f64, f64::max);
        if max_cost > 0.0 {
            slot.cost_per_m / max_cost
        } else {
            0.0
        }
    }

    /// Score a provider. Lower is better.
    ///
    /// Four factors:
    ///   - **Stability** (35%): blended baseline + live error rate. Does it complete reliably?
    ///   - **Quality** (30%): catalog ds_output × stability. Does it produce good output?
    ///   - **Throughput** (20%): output tokens per second. Task-normalized speed.
    ///     Raw latency is NOT used — it depends on task complexity, not provider quality.
    ///   - **Cost** (15%): normalized output cost. Cheaper is better when quality is similar.
    fn score(&self, slot: &AdaptiveSlot) -> f64 {
        let total = slot.metrics.success_count.load(Ordering::Relaxed)
            + slot.metrics.failure_count.load(Ordering::Relaxed);

        // EMA blend weight: 0.0 at cold start → 1.0 after 10 calls
        let weight = (total as f64 / 10.0).min(1.0);

        // ── Stability (35%) ──
        let baseline_stab = f64::from_bits(slot.baseline_stability.load(Ordering::Relaxed));
        let live_err_rate = slot.metrics.error_rate();
        let baseline_err = 1.0 - baseline_stab;
        let blended_err = baseline_err * (1.0 - weight) + live_err_rate * weight;

        // ── Quality (30%) ──
        let ds = slot.ds_output.load(Ordering::Relaxed) as f64;
        let quality = ds * baseline_stab;
        let max_quality = self
            .slots
            .iter()
            .map(|s| {
                let d = s.ds_output.load(Ordering::Relaxed) as f64;
                let st = f64::from_bits(s.baseline_stability.load(Ordering::Relaxed));
                d * st
            })
            .fold(0.0_f64, f64::max);
        let norm_quality = if max_quality > 0.0 {
            1.0 - (quality / max_quality)
        } else {
            0.5
        };

        // ── Throughput (20%) ──
        // Tokens per second — higher is better. Invert for lower-is-better score.
        let throughput = slot.metrics.throughput();
        let max_throughput = self
            .slots
            .iter()
            .map(|s| s.metrics.throughput())
            .fold(0.0_f64, f64::max);
        let norm_throughput = if max_throughput > 0.0 && throughput > 0.0 {
            1.0 - (throughput / max_throughput)
        } else {
            0.5 // No data yet — neutral
        };

        // ── Cost (15%) ──
        let norm_cost = self.norm_cost(slot);

        0.35 * blended_err + 0.30 * norm_quality + 0.20 * norm_throughput + 0.15 * norm_cost
    }

    /// Select provider index and whether this is a probe request.
    ///
    /// - Off / Hedge: priority order, skip circuit-broken only.
    ///   (Hedge mode uses this to pick the primary for racing.)
    /// - Lane: score-based selection across all providers.
    fn select_provider(&self) -> (usize, bool) {
        let mode = self.mode();

        // Off and Hedge both use priority order for the primary selection.
        // (Hedge picks the alternate separately in hedged_chat.)
        if mode != AdaptiveMode::Lane {
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
                    return (i, false);
                }
            }
            // All circuit-broken — fall through to least-failed logic below
        }

        // Score all non-circuit-broken providers
        let mut scored: Vec<(usize, f64)> = self
            .slots
            .iter()
            .enumerate()
            .filter(|(_, s)| !s.metrics.is_circuit_open(self.config.failure_threshold))
            .map(|(i, s)| (i, self.score(s)))
            .collect();

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
            return (best, false);
        }

        scored.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
        let best_idx = scored[0].0;

        // Probe: with some probability, redirect to a stale non-primary provider
        if self.slots.len() > 1 && self.should_probe() {
            // Find a stale provider that isn't the best
            for (i, slot) in self.slots.iter().enumerate() {
                if i != best_idx
                    && slot.metrics.is_stale(self.config.probe_interval_secs)
                    && !slot.metrics.is_circuit_open(self.config.failure_threshold)
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

        // Detect lane change
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
        let primary_name = self.slots[primary_idx].provider.provider_name();
        let candidates: Vec<(usize, &AdaptiveSlot)> = self
            .slots
            .iter()
            .enumerate()
            .filter(|(i, s)| {
                *i != primary_idx
                    && s.provider.provider_name() != primary_name
                    && !s.metrics.is_circuit_open(self.config.failure_threshold)
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
            Err(_) => {
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
            }
        }

        result
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
            Err(_) => {
                self.slots[idx].metrics.record_failure();
            }
        }

        result
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

        // ── Hedged racing: fire to 2 providers, take the winner ────────
        if mode == AdaptiveMode::Hedge && self.slots.len() > 1 {
            if let Some(result) = self.hedged_chat(start_idx, messages, tools, config).await {
                return result;
            }
        }

        // ── Single-provider path (Off / Lane / fallthrough) ────────────
        match self.try_chat(start_idx, messages, tools, config).await {
            Ok(resp) => Ok(resp),
            Err(e) => {
                if self.slots.len() == 1 {
                    return Err(e);
                }

                warn!(
                    provider = self.slots[start_idx].provider.provider_name(),
                    error = %e,
                    "adaptive router failing over"
                );

                // Failover: try remaining providers in score order.
                let mut scored: Vec<(usize, f64)> = self
                    .slots
                    .iter()
                    .enumerate()
                    .filter(|(i, s)| {
                        *i != start_idx && !s.metrics.is_circuit_open(self.config.failure_threshold)
                    })
                    .map(|(i, s)| (i, self.score(s)))
                    .collect();
                scored.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));

                let mut last_error = e;
                for (idx, _) in scored {
                    self.emit_status(format!(
                        "Switching to {}...",
                        self.slots[idx].provider.provider_name()
                    ));
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
        let (start_idx, _is_probe) = self.select_provider();

        match self
            .try_chat_stream(start_idx, messages, tools, config)
            .await
        {
            Ok(stream) => Ok(stream),
            Err(e) => {
                if self.slots.len() == 1 {
                    return Err(e);
                }

                warn!(
                    provider = self.slots[start_idx].provider.provider_name(),
                    error = %e,
                    "adaptive router failing over stream"
                );

                let mut scored: Vec<(usize, f64)> = self
                    .slots
                    .iter()
                    .enumerate()
                    .filter(|(i, s)| {
                        *i != start_idx && !s.metrics.is_circuit_open(self.config.failure_threshold)
                    })
                    .map(|(i, s)| (i, self.score(s)))
                    .collect();
                scored.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));

                let mut last_error = e;
                for (idx, _) in scored {
                    self.emit_status(format!(
                        "Switching to {}...",
                        self.slots[idx].provider.provider_name()
                    ));
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

    fn model_id(&self) -> &str {
        let (idx, _) = self.select_provider();
        self.slots[idx].provider.model_id()
    }

    fn provider_name(&self) -> &str {
        let (idx, _) = self.select_provider();
        self.slots[idx].provider.provider_name()
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

fn now_epoch_us() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as u64
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
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
            })
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
        assert!(p95 >= 90_000 && p95 <= 100_000, "p95 was {}", p95 / 1000);
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
    /// Warm up in Off mode (priority order), then switch to Lane.
    /// With metrics showing primary is slow, Lane switches to faster provider.
    #[tokio::test]
    async fn test_lane_mode_picks_best_by_score() {
        let config = AdaptiveConfig {
            probe_probability: 0.0,
            latency_threshold_ms: 100, // Low threshold for fast test
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

        // Warm up in Off mode (priority order → primary always selected)
        router.set_mode(AdaptiveMode::Off);
        for _ in 0..5 {
            let resp = router.chat(&[], &[], &ChatConfig::default()).await.unwrap();
            assert_eq!(resp.content.as_deref(), Some("from-slow-primary"));
        }

        // Switch to Lane mode. Primary has metrics: ~50ms latency.
        // primary score  = 0.5*(50/100) + 0 + 0.3*(0/2) = 0.25
        // fallback (cold) = 0.3*(1/2) = 0.15
        // 0.15 < 0.25 → lane picks faster fallback
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
}
