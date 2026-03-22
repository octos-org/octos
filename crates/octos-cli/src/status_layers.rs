//! Composable multi-layer status message system.
//!
//! Replaces the flat `StatusIndicator` with a `StatusComposer` that holds
//! ordered, independently-updatable layers. Each layer has a display policy
//! (fixed, rotating, transient, replaceable) and a priority that determines
//! its vertical position in the composed message.
//!
//! When any layer changes, the single editable channel message is re-composed
//! and edited (throttled at 1s to respect API rate limits).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use octos_agent::TokenTracker;
use octos_bus::Channel;
use octos_core::{METADATA_SENDER_USER_ID, OutboundMessage};
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, Notify};
use tracing::warn;

// ---------------------------------------------------------------------------
// Layer display policies
// ---------------------------------------------------------------------------

/// How a layer's content is displayed and updated.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LayerPolicy {
    /// Always visible, content set once (e.g. provider name).
    Fixed,
    /// Cycles through a list of values on a timer (e.g. operation words).
    Rotating { interval_secs: u64 },
    /// Appears temporarily then auto-clears after duration.
    Transient { duration_secs: u64 },
    /// Shows latest value, replaced by next update (e.g. retry status).
    #[default]
    Replaceable,
}

// ---------------------------------------------------------------------------
// Layer slots
// ---------------------------------------------------------------------------

/// A single layer in the status composer.
#[derive(Debug)]
struct StatusLayer {
    /// Layer identifier.
    id: String,
    /// Display priority — lower number renders higher in the message.
    priority: u8,
    /// Display policy.
    policy: LayerPolicy,
    /// Current content (None = hidden).
    content: RwLock<Option<String>>,
    /// For Rotating: list of values to cycle through.
    rotating_values: RwLock<Vec<String>>,
    /// For Rotating: current index.
    rotating_index: std::sync::atomic::AtomicUsize,
    /// For Transient: when the content was set (for auto-clear).
    set_at: RwLock<Option<Instant>>,
}

impl StatusLayer {
    fn new(id: impl Into<String>, priority: u8, policy: LayerPolicy) -> Self {
        Self {
            id: id.into(),
            priority,
            policy,
            content: RwLock::new(None),
            rotating_values: RwLock::new(Vec::new()),
            rotating_index: std::sync::atomic::AtomicUsize::new(0),
            set_at: RwLock::new(None),
        }
    }

    fn get(&self) -> Option<String> {
        // Check transient expiry
        if let LayerPolicy::Transient { duration_secs } = &self.policy {
            if let Ok(set_at) = self.set_at.read() {
                if let Some(at) = *set_at {
                    if at.elapsed() > Duration::from_secs(*duration_secs) {
                        return None;
                    }
                }
            }
        }
        self.content.read().ok()?.clone()
    }

    fn set(&self, value: Option<String>) {
        if let Ok(mut c) = self.content.write() {
            *c = value;
        }
        if let Ok(mut t) = self.set_at.write() {
            *t = Some(Instant::now());
        }
    }

    /// Advance to next rotating value. Returns true if content changed.
    fn rotate(&self) -> bool {
        if let LayerPolicy::Rotating { .. } = &self.policy {
            let values = self.rotating_values.read().unwrap();
            if values.is_empty() {
                return false;
            }
            let idx = self
                .rotating_index
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let value = &values[idx % values.len()];
            if let Ok(mut c) = self.content.write() {
                *c = Some(value.clone());
                return true;
            }
        }
        false
    }

    fn set_rotating_values(&self, values: Vec<String>) {
        if let Ok(mut v) = self.rotating_values.write() {
            *v = values;
        }
    }
}

// ---------------------------------------------------------------------------
// Per-user status configuration
// ---------------------------------------------------------------------------

/// Custom layer definition stored in user config.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CustomLayerDef {
    /// Layer identifier.
    pub id: String,
    /// Display priority (lower = higher in message).
    pub priority: u8,
    /// Display policy.
    pub policy: LayerPolicy,
    /// Content template or static text.
    pub content: String,
}

/// Per-user status configuration, persisted to disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserStatusConfig {
    /// Greeting template (e.g. "Hey {name}! On it..."). None = no greeting.
    #[serde(default)]
    pub greeting_template: Option<String>,
    /// Whether to show the provider/model layer.
    #[serde(default = "default_true")]
    pub provider_visible: bool,
    /// Whether to show the metrics layer (elapsed time + token counts).
    #[serde(default = "default_true")]
    pub metrics_visible: bool,
    /// Per-user status word override for the operation layer.
    #[serde(default)]
    pub status_words: Option<Vec<String>>,
    /// Force locale for status formatting (e.g. "zh", "en").
    #[serde(default)]
    pub locale: Option<String>,
    /// User-defined custom layers.
    #[serde(default)]
    pub custom_layers: Vec<CustomLayerDef>,
    /// Transient duration for greeting layer in seconds.
    #[serde(default = "default_greeting_duration")]
    pub greeting_duration_secs: u64,
}

fn default_true() -> bool {
    true
}

fn default_greeting_duration() -> u64 {
    5
}

impl Default for UserStatusConfig {
    fn default() -> Self {
        Self {
            greeting_template: None,
            provider_visible: true,
            metrics_visible: true,
            status_words: None,
            locale: None,
            custom_layers: Vec::new(),
            greeting_duration_secs: 5,
        }
    }
}

impl UserStatusConfig {
    /// Load from disk, or return default if not found.
    pub fn load(data_dir: &Path, base_key: &str) -> Self {
        let path = Self::config_path(data_dir, base_key);
        match std::fs::read_to_string(&path) {
            Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    /// Save to disk.
    pub fn save(&self, data_dir: &Path, base_key: &str) -> std::io::Result<()> {
        let path = Self::config_path(data_dir, base_key);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        std::fs::write(&path, json)
    }

    fn config_path(data_dir: &Path, base_key: &str) -> PathBuf {
        let encoded = octos_bus::session::encode_path_component(base_key);
        data_dir
            .join("users")
            .join(encoded)
            .join("status_config.json")
    }
}

// ---------------------------------------------------------------------------
// Well-known layer IDs and priorities
// ---------------------------------------------------------------------------

/// Well-known layer IDs.
pub mod layer_id {
    pub const GREETING: &str = "greeting";
    pub const OPERATION: &str = "operation";
    pub const PROVIDER: &str = "provider";
    pub const RETRY: &str = "retry";
    pub const METRICS: &str = "metrics";
    pub const FEEDBACK: &str = "feedback";
}

/// Default priorities (lower = rendered higher).
mod default_priority {
    pub const GREETING: u8 = 10;
    pub const OPERATION: u8 = 20;
    pub const PROVIDER: u8 = 30;
    pub const RETRY: u8 = 25;
    pub const METRICS: u8 = 40;
    pub const FEEDBACK: u8 = 15;
}

// ---------------------------------------------------------------------------
// Status Composer
// ---------------------------------------------------------------------------

/// Minimum interval between message edits (rate limit protection).
const EDIT_THROTTLE: Duration = Duration::from_millis(1000);

/// Composable status message system for messaging channels.
///
/// Holds ordered layers that are independently updatable. A background task
/// re-composes the message and edits it on the channel whenever a layer changes.
pub struct StatusComposer {
    channel: Arc<dyn Channel>,
    /// Status words pool (shared with PersonaService for updates).
    status_words: Arc<RwLock<Vec<String>>>,
    /// Global word index for slow rotation across sessions.
    word_index: std::sync::atomic::AtomicUsize,
}

impl StatusComposer {
    pub fn new(channel: Arc<dyn Channel>, status_words: Vec<String>) -> Self {
        Self {
            channel,
            status_words: Arc::new(RwLock::new(status_words)),
            word_index: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// Get a reference to the underlying channel.
    pub fn channel(&self) -> &Arc<dyn Channel> {
        &self.channel
    }

    /// Update the status word pool (called when PersonaService refreshes).
    pub fn set_words(&self, words: Vec<String>) {
        if let Ok(mut w) = self.status_words.write() {
            *w = words;
        }
    }

    /// Get a shared handle to the status words.
    pub fn status_words_handle(&self) -> Arc<RwLock<Vec<String>>> {
        Arc::clone(&self.status_words)
    }

    /// Start the status composer for a chat session.
    ///
    /// Creates all layers based on `user_config`, starts the compose loop,
    /// and returns a handle for updating layers and stopping.
    pub fn start(
        &self,
        chat_id: String,
        message_text: &str,
        tracker: Arc<TokenTracker>,
        voice_transcript: Option<String>,
        user_config: &UserStatusConfig,
        sender_user_id: Option<String>,
    ) -> ComposerHandle {
        let cancelled = Arc::new(AtomicBool::new(false));
        let status_msg_id = Arc::new(Mutex::new(None::<String>));
        let notify = Arc::new(Notify::new());
        let channel = Arc::clone(&self.channel);

        let is_chinese = user_config
            .locale
            .as_deref()
            .map(|l| l.starts_with("zh"))
            .unwrap_or_else(|| has_cjk(message_text));

        // Build layers
        let layers = Arc::new(build_layers(user_config));

        // Set up operation rotating words
        let words = self.resolve_words(user_config, is_chinese);
        let idx = self.word_index.fetch_add(1, Ordering::Relaxed);
        if let Some(op_layer) = find_layer(&layers, layer_id::OPERATION) {
            op_layer.set_rotating_values(words);
            op_layer
                .rotating_index
                .store(idx, std::sync::atomic::Ordering::Relaxed);
            op_layer.rotate();
        }

        // Set greeting if configured
        if let Some(ref template) = user_config.greeting_template {
            if let Some(layer) = find_layer(&layers, layer_id::GREETING) {
                layer.set(Some(template.clone()));
            }
        }

        // Set custom layer content
        for custom in &user_config.custom_layers {
            if let Some(layer) = find_layer(&layers, &custom.id) {
                layer.set(Some(custom.content.clone()));
            }
        }

        let cancelled_clone = Arc::clone(&cancelled);
        let msg_id_clone = Arc::clone(&status_msg_id);
        let notify_clone = Arc::clone(&notify);
        let layers_clone = Arc::clone(&layers);
        let channel_clone = Arc::clone(&channel);
        let chat_id_clone = chat_id.clone();
        let tracker_clone = Arc::clone(&tracker);
        let metrics_visible = user_config.metrics_visible;
        let provider_visible = user_config.provider_visible;

        tokio::spawn(async move {
            run_compose_loop(
                channel_clone,
                chat_id_clone,
                layers_clone,
                cancelled_clone,
                msg_id_clone,
                notify_clone,
                tracker_clone,
                voice_transcript,
                metrics_visible,
                provider_visible,
                sender_user_id,
            )
            .await;
        });

        ComposerHandle {
            cancelled,
            status_msg_id,
            notify,
            layers,
            channel,
            chat_id,
        }
    }

    fn resolve_words(&self, user_config: &UserStatusConfig, is_chinese: bool) -> Vec<String> {
        // User override first
        if let Some(ref words) = user_config.status_words {
            if !words.is_empty() {
                return words.clone();
            }
        }

        // Then global pool, filtered by language
        let global = self.status_words.read().unwrap_or_else(|e| e.into_inner());
        let filtered: Vec<String> = global
            .iter()
            .filter(|w| if is_chinese { has_cjk(w) } else { !has_cjk(w) })
            .cloned()
            .collect();

        if filtered.is_empty() {
            if is_chinese {
                crate::persona_service::DEFAULT_STATUS_WORDS_ZH
                    .iter()
                    .map(|s| (*s).to_string())
                    .collect()
            } else {
                crate::persona_service::DEFAULT_STATUS_WORDS
                    .iter()
                    .map(|s| (*s).to_string())
                    .collect()
            }
        } else {
            filtered
        }
    }
}

// ---------------------------------------------------------------------------
// Composer Handle
// ---------------------------------------------------------------------------

/// Handle to an active status composer. Provides typed methods for updating
/// individual layers and stopping the composer.
pub struct ComposerHandle {
    pub(crate) cancelled: Arc<AtomicBool>,
    pub(crate) status_msg_id: Arc<Mutex<Option<String>>>,
    notify: Arc<Notify>,
    layers: Arc<Vec<StatusLayer>>,
    channel: Arc<dyn Channel>,
    chat_id: String,
}

impl ComposerHandle {
    /// Update the operation layer (thinking status word).
    pub fn set_operation(&self, text: &str) {
        if let Some(layer) = find_layer(&self.layers, layer_id::OPERATION) {
            layer.set(Some(format!("✦ {text}...")));
            self.notify.notify_one();
        }
    }

    /// Update the provider layer.
    pub fn set_provider(&self, provider: &str, model: &str) {
        if let Some(layer) = find_layer(&self.layers, layer_id::PROVIDER) {
            layer.set(Some(format!("via {provider} ({model})")));
            self.notify.notify_one();
        }
    }

    /// Update the retry/failover layer.
    pub fn set_retry(&self, message: &str) {
        if let Some(layer) = find_layer(&self.layers, layer_id::RETRY) {
            layer.set(Some(format!("⟳ {message}")));
            self.notify.notify_one();
        }
    }

    /// Clear the retry layer (e.g. when retry succeeds).
    pub fn clear_retry(&self) {
        if let Some(layer) = find_layer(&self.layers, layer_id::RETRY) {
            layer.set(None);
            self.notify.notify_one();
        }
    }

    /// Set feedback message (e.g. "📨 Got it" for queued messages).
    pub fn set_feedback(&self, text: &str) {
        if let Some(layer) = find_layer(&self.layers, layer_id::FEEDBACK) {
            layer.set(Some(text.to_string()));
            self.notify.notify_one();
        }
    }

    /// Update a custom layer by ID.
    pub fn set_layer(&self, id: &str, content: Option<String>) {
        if let Some(layer) = find_layer(&self.layers, id) {
            layer.set(content);
            self.notify.notify_one();
        }
    }

    /// Stop the composer and delete the status message.
    pub async fn stop(self) {
        self.cancelled.store(true, Ordering::Release);
        self.notify.notify_one();

        // Give the loop a moment to notice cancellation
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Delete the status message if one was sent
        let msg_id = self.status_msg_id.lock().await.take();
        if let Some(mid) = msg_id {
            if let Err(e) = self.channel.delete_message(&self.chat_id, &mid).await {
                warn!("failed to delete status message: {e}");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Build default layers
// ---------------------------------------------------------------------------

fn build_layers(config: &UserStatusConfig) -> Vec<StatusLayer> {
    let mut layers = vec![
        StatusLayer::new(
            layer_id::GREETING,
            default_priority::GREETING,
            LayerPolicy::Transient {
                duration_secs: config.greeting_duration_secs,
            },
        ),
        StatusLayer::new(
            layer_id::OPERATION,
            default_priority::OPERATION,
            LayerPolicy::Rotating { interval_secs: 8 },
        ),
        StatusLayer::new(
            layer_id::PROVIDER,
            default_priority::PROVIDER,
            LayerPolicy::Fixed,
        ),
        StatusLayer::new(
            layer_id::RETRY,
            default_priority::RETRY,
            LayerPolicy::Replaceable,
        ),
        StatusLayer::new(
            layer_id::METRICS,
            default_priority::METRICS,
            LayerPolicy::Replaceable,
        ),
        StatusLayer::new(
            layer_id::FEEDBACK,
            default_priority::FEEDBACK,
            LayerPolicy::Transient { duration_secs: 3 },
        ),
    ];

    // Add user-defined custom layers
    for custom in &config.custom_layers {
        layers.push(StatusLayer::new(
            &custom.id,
            custom.priority,
            custom.policy.clone(),
        ));
    }

    // Sort by priority (stable sort preserves insertion order for equal priorities)
    layers.sort_by_key(|l| l.priority);
    layers
}

fn find_layer<'a>(layers: &'a [StatusLayer], id: &str) -> Option<&'a StatusLayer> {
    layers.iter().find(|l| l.id == id)
}

// ---------------------------------------------------------------------------
// Compose loop
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
async fn run_compose_loop(
    channel: Arc<dyn Channel>,
    chat_id: String,
    layers: Arc<Vec<StatusLayer>>,
    cancelled: Arc<AtomicBool>,
    status_msg_id: Arc<Mutex<Option<String>>>,
    notify: Arc<Notify>,
    tracker: Arc<TokenTracker>,
    voice_transcript: Option<String>,
    metrics_visible: bool,
    provider_visible: bool,
    sender_user_id: Option<String>,
) {
    let start = Instant::now();
    let mut last_edit = Instant::now() - EDIT_THROTTLE;
    let mut last_composed = String::new();

    // Immediately send typing indicator
    let _ = channel
        .send_typing_as(&chat_id, sender_user_id.as_deref())
        .await;

    // Wait 2 seconds before sending a visible status message
    for _ in 0..4 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        if cancelled.load(Ordering::Acquire) {
            return;
        }
    }

    // Compose and send initial message
    if !channel.supports_edit() {
        // Can't edit — skip status message entirely
        // Just keep sending typing indicators
        loop {
            tokio::time::sleep(Duration::from_secs(5)).await;
            if cancelled.load(Ordering::Acquire) {
                return;
            }
            let _ = channel
                .send_typing_as(&chat_id, sender_user_id.as_deref())
                .await;
        }
    }

    // Main compose loop
    let mut tick: u32 = 0;
    loop {
        // Wait for a layer change notification or 1s tick
        tokio::select! {
            _ = notify.notified() => {}
            _ = tokio::time::sleep(Duration::from_secs(1)) => {}
        }

        if cancelled.load(Ordering::Acquire) {
            return;
        }

        tick += 1;

        // Send typing indicator every 5 seconds
        if tick % 5 == 0 {
            let _ = channel
                .send_typing_as(&chat_id, sender_user_id.as_deref())
                .await;
        }

        // Rotate operation words on their interval
        if tick % 8 == 0 {
            if let Some(layer) = find_layer(&layers, layer_id::OPERATION) {
                layer.rotate();
            }
        }

        // Update metrics layer
        if metrics_visible {
            let elapsed = start.elapsed().as_secs();
            let ti = tracker.input_tokens.load(Ordering::Relaxed);
            let to = tracker.output_tokens.load(Ordering::Relaxed);
            let metrics_text = format_metrics(elapsed, ti, to);
            if let Some(layer) = find_layer(&layers, layer_id::METRICS) {
                if !metrics_text.is_empty() {
                    layer.set(Some(metrics_text));
                }
            }
        }

        // Compose all visible layers
        let composed = compose_message(
            &layers,
            provider_visible,
            metrics_visible,
            voice_transcript.as_deref(),
        );

        if composed.is_empty() {
            continue;
        }

        // Only edit if content changed and throttle allows
        if composed != last_composed && last_edit.elapsed() >= EDIT_THROTTLE {
            let mid = status_msg_id.lock().await.clone();
            if let Some(ref mid) = mid {
                let _ = channel.edit_message(&chat_id, mid, &composed).await;
            } else {
                // Send initial status message
                let msg = OutboundMessage {
                    channel: channel.name().to_string(),
                    chat_id: chat_id.clone(),
                    content: composed.clone(),
                    reply_to: None,
                    media: vec![],
                    metadata: sender_user_id
                        .as_ref()
                        .map(|uid| serde_json::json!({ METADATA_SENDER_USER_ID: uid }))
                        .unwrap_or_else(|| serde_json::json!({})),
                };
                match channel.send_with_id(&msg).await {
                    Ok(Some(mid)) => {
                        *status_msg_id.lock().await = Some(mid);
                    }
                    Ok(None) => {}
                    Err(e) => {
                        warn!("failed to send status message: {e}");
                    }
                }
            }
            last_edit = Instant::now();
            last_composed = composed;
        }
    }
}

/// Compose all visible layers into a single message string.
fn compose_message(
    layers: &[StatusLayer],
    provider_visible: bool,
    metrics_visible: bool,
    voice_transcript: Option<&str>,
) -> String {
    let mut lines: Vec<String> = Vec::new();

    for layer in layers {
        // Skip disabled layers
        if layer.id == layer_id::PROVIDER && !provider_visible {
            continue;
        }
        if layer.id == layer_id::METRICS && !metrics_visible {
            continue;
        }

        if let Some(content) = layer.get() {
            if !content.is_empty() {
                lines.push(content);
            }
        }
    }

    // Append voice transcript if present
    if let Some(transcript) = voice_transcript {
        let truncated: String = transcript.chars().take(80).collect();
        if truncated.len() < transcript.len() {
            lines.push(format!("🎙 {truncated}..."));
        } else {
            lines.push(format!("🎙 {truncated}"));
        }
    }

    lines.join("\n")
}

/// Format metrics line: "12s · 1.2k↑ 350↓"
fn format_metrics(elapsed_secs: u64, input_tokens: u32, output_tokens: u32) -> String {
    let has_tokens = input_tokens > 0 || output_tokens > 0;

    let time_part = if elapsed_secs < 3 {
        String::new()
    } else if elapsed_secs < 60 {
        format!("{elapsed_secs}s")
    } else {
        let mins = elapsed_secs / 60;
        let secs = elapsed_secs % 60;
        format!("{mins}m {secs}s")
    };

    let token_part = if has_tokens {
        format!(
            "{}↑ {}↓",
            fmt_tokens(input_tokens),
            fmt_tokens(output_tokens)
        )
    } else {
        String::new()
    };

    let parts: Vec<&str> = [time_part.as_str(), token_part.as_str()]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect();

    if parts.is_empty() {
        String::new()
    } else {
        parts.join(" · ")
    }
}

// ---------------------------------------------------------------------------
// Utility functions (preserved from status_indicator.rs)
// ---------------------------------------------------------------------------

/// Check if text contains CJK characters (Chinese/Japanese/Korean).
pub(crate) fn has_cjk(s: &str) -> bool {
    s.chars().any(|c| {
        matches!(c, '\u{4E00}'..='\u{9FFF}' | '\u{3400}'..='\u{4DBF}' | '\u{F900}'..='\u{FAFF}')
    })
}

/// Format a compact token count like "1.2k" or "350".
pub(crate) fn fmt_tokens(n: u32) -> String {
    if n >= 1000 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else {
        n.to_string()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use async_trait::async_trait;
    use octos_core::{InboundMessage, METADATA_SENDER_USER_ID};
    use tokio::sync::{Mutex, mpsc};

    #[derive(Default)]
    struct MockChannel {
        sent: Arc<Mutex<Vec<OutboundMessage>>>,
        typing_senders: Arc<Mutex<Vec<Option<String>>>>,
    }

    #[async_trait]
    impl Channel for MockChannel {
        fn name(&self) -> &str {
            "matrix"
        }

        async fn start(&self, _inbound_tx: mpsc::Sender<InboundMessage>) -> eyre::Result<()> {
            Ok(())
        }

        async fn send(&self, msg: &OutboundMessage) -> eyre::Result<()> {
            self.sent.lock().await.push(msg.clone());
            Ok(())
        }

        async fn send_with_id(&self, msg: &OutboundMessage) -> eyre::Result<Option<String>> {
            self.sent.lock().await.push(msg.clone());
            Ok(Some("$status-1".to_string()))
        }

        async fn send_typing_as(
            &self,
            _chat_id: &str,
            sender_user_id: Option<&str>,
        ) -> eyre::Result<()> {
            self.typing_senders
                .lock()
                .await
                .push(sender_user_id.map(str::to_string));
            Ok(())
        }

        fn supports_edit(&self) -> bool {
            true
        }
    }

    #[test]
    fn should_compose_visible_layers_in_priority_order() {
        let config = UserStatusConfig::default();
        let layers = build_layers(&config);

        // Set operation and metrics
        find_layer(&layers, layer_id::OPERATION)
            .unwrap()
            .set(Some("✦ Pondering...".to_string()));
        find_layer(&layers, layer_id::METRICS)
            .unwrap()
            .set(Some("15s · 1.2k↑ 350↓".to_string()));

        let composed = compose_message(&layers, true, true, None);
        assert_eq!(composed, "✦ Pondering...\n15s · 1.2k↑ 350↓");
    }

    #[test]
    fn should_skip_none_layers() {
        let config = UserStatusConfig::default();
        let layers = build_layers(&config);

        find_layer(&layers, layer_id::OPERATION)
            .unwrap()
            .set(Some("✦ Thinking...".to_string()));
        // retry, greeting, etc. are None

        let composed = compose_message(&layers, true, true, None);
        assert_eq!(composed, "✦ Thinking...");
    }

    #[test]
    fn should_respect_provider_visible_flag() {
        let config = UserStatusConfig::default();
        let layers = build_layers(&config);

        find_layer(&layers, layer_id::OPERATION)
            .unwrap()
            .set(Some("✦ Thinking...".to_string()));
        find_layer(&layers, layer_id::PROVIDER)
            .unwrap()
            .set(Some("via moonshot (kimi-2.5)".to_string()));

        let with = compose_message(&layers, true, true, None);
        assert!(with.contains("via moonshot"));

        let without = compose_message(&layers, false, true, None);
        assert!(!without.contains("via moonshot"));
    }

    #[test]
    fn should_respect_metrics_visible_flag() {
        let config = UserStatusConfig::default();
        let layers = build_layers(&config);

        find_layer(&layers, layer_id::OPERATION)
            .unwrap()
            .set(Some("✦ Thinking...".to_string()));
        find_layer(&layers, layer_id::METRICS)
            .unwrap()
            .set(Some("10s".to_string()));

        let with = compose_message(&layers, true, true, None);
        assert!(with.contains("10s"));

        let without = compose_message(&layers, true, false, None);
        assert!(!without.contains("10s"));
    }

    #[test]
    fn should_include_all_layers_in_order() {
        let config = UserStatusConfig::default();
        let layers = build_layers(&config);

        find_layer(&layers, layer_id::GREETING)
            .unwrap()
            .set(Some("👋 Hey!".to_string()));
        find_layer(&layers, layer_id::OPERATION)
            .unwrap()
            .set(Some("✦ Pondering...".to_string()));
        find_layer(&layers, layer_id::RETRY)
            .unwrap()
            .set(Some("⟳ Switching to deepseek...".to_string()));
        find_layer(&layers, layer_id::PROVIDER)
            .unwrap()
            .set(Some("via moonshot".to_string()));
        find_layer(&layers, layer_id::METRICS)
            .unwrap()
            .set(Some("5s".to_string()));

        let composed = compose_message(&layers, true, true, None);
        let lines: Vec<&str> = composed.lines().collect();
        // Priority order: greeting(10), operation(20), retry(25), provider(30), metrics(40)
        assert_eq!(lines[0], "👋 Hey!");
        assert_eq!(lines[1], "✦ Pondering...");
        assert_eq!(lines[2], "⟳ Switching to deepseek...");
        assert_eq!(lines[3], "via moonshot");
        assert_eq!(lines[4], "5s");
    }

    #[test]
    fn should_append_voice_transcript() {
        let config = UserStatusConfig::default();
        let layers = build_layers(&config);

        find_layer(&layers, layer_id::OPERATION)
            .unwrap()
            .set(Some("✦ Thinking...".to_string()));

        let composed = compose_message(&layers, true, true, Some("what about today"));
        assert!(composed.contains("🎙 what about today"));
    }

    #[tokio::test]
    async fn should_send_status_message_with_sender_user_id() {
        let channel = Arc::new(MockChannel::default());
        let composer = StatusComposer::new(channel.clone(), vec!["✦ Thinking...".to_string()]);
        let tracker = Arc::new(TokenTracker::new());

        let handle = composer.start(
            "!room:localhost".to_string(),
            "hello",
            tracker,
            None,
            &UserStatusConfig::default(),
            Some("@bot_mybot:localhost".to_string()),
        );

        tokio::time::sleep(Duration::from_millis(3300)).await;
        handle.cancelled.store(true, Ordering::Release);

        let sent = channel.sent.lock().await;
        let first = sent.first().expect("status message should be sent");
        assert_eq!(
            first
                .metadata
                .get(METADATA_SENDER_USER_ID)
                .and_then(|v| v.as_str()),
            Some("@bot_mybot:localhost")
        );
    }

    #[tokio::test]
    async fn should_send_typing_with_sender_user_id() {
        let channel = Arc::new(MockChannel::default());
        let composer = StatusComposer::new(channel.clone(), vec!["✦ Thinking...".to_string()]);
        let tracker = Arc::new(TokenTracker::new());

        let handle = composer.start(
            "!room:localhost".to_string(),
            "hello",
            tracker,
            None,
            &UserStatusConfig::default(),
            Some("@bot_mybot:localhost".to_string()),
        );

        tokio::time::sleep(Duration::from_millis(50)).await;
        handle.cancelled.store(true, Ordering::Release);

        let typing = channel.typing_senders.lock().await;
        assert_eq!(
            typing.first().and_then(|v| v.as_deref()),
            Some("@bot_mybot:localhost")
        );
    }

    #[test]
    fn should_rotate_operation_words() {
        let layer = StatusLayer::new("op", 20, LayerPolicy::Rotating { interval_secs: 8 });
        layer.set_rotating_values(vec![
            "✦ Thinking...".to_string(),
            "✦ Pondering...".to_string(),
            "✦ Reflecting...".to_string(),
        ]);

        layer.rotate();
        assert_eq!(layer.get().unwrap(), "✦ Thinking...");
        layer.rotate();
        assert_eq!(layer.get().unwrap(), "✦ Pondering...");
        layer.rotate();
        assert_eq!(layer.get().unwrap(), "✦ Reflecting...");
        layer.rotate();
        assert_eq!(layer.get().unwrap(), "✦ Thinking..."); // wraps around
    }

    #[test]
    fn should_auto_clear_transient_layer() {
        let layer = StatusLayer::new("greet", 10, LayerPolicy::Transient { duration_secs: 0 });
        layer.set(Some("Hello!".to_string()));
        // duration_secs=0, so it should be expired immediately
        std::thread::sleep(Duration::from_millis(10));
        assert!(layer.get().is_none());
    }

    #[test]
    fn should_format_metrics_correctly() {
        assert_eq!(format_metrics(1, 0, 0), "");
        assert_eq!(format_metrics(15, 0, 0), "15s");
        assert_eq!(format_metrics(15, 1200, 350), "15s · 1.2k↑ 350↓");
        assert_eq!(format_metrics(65, 5000, 1200), "1m 5s · 5.0k↑ 1.2k↓");
        assert_eq!(format_metrics(1, 500, 100), "500↑ 100↓");
    }

    #[test]
    fn should_include_custom_layers() {
        let config = UserStatusConfig {
            custom_layers: vec![CustomLayerDef {
                id: "coffee".to_string(),
                priority: 35,
                policy: LayerPolicy::Fixed,
                content: "☕ Brewing ideas...".to_string(),
            }],
            ..Default::default()
        };
        let layers = build_layers(&config);

        // Custom layer should be present
        let coffee = find_layer(&layers, "coffee");
        assert!(coffee.is_some());

        // Set it and compose
        coffee.unwrap().set(Some("☕ Brewing ideas...".to_string()));
        find_layer(&layers, layer_id::OPERATION)
            .unwrap()
            .set(Some("✦ Thinking...".to_string()));

        let composed = compose_message(&layers, true, true, None);
        let lines: Vec<&str> = composed.lines().collect();
        // operation(20) before coffee(35)
        assert_eq!(lines[0], "✦ Thinking...");
        assert_eq!(lines[1], "☕ Brewing ideas...");
    }

    #[test]
    fn should_serialize_deserialize_user_config() {
        let config = UserStatusConfig {
            greeting_template: Some("Hey {name}!".to_string()),
            provider_visible: false,
            metrics_visible: true,
            status_words: Some(vec!["Pondering".to_string(), "施法中".to_string()]),
            locale: Some("zh".to_string()),
            custom_layers: vec![CustomLayerDef {
                id: "mood".to_string(),
                priority: 50,
                policy: LayerPolicy::Fixed,
                content: "😊 Feeling good".to_string(),
            }],
            greeting_duration_secs: 10,
        };

        let json = serde_json::to_string(&config).unwrap();
        let restored: UserStatusConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.greeting_template, config.greeting_template);
        assert!(!restored.provider_visible);
        assert_eq!(restored.custom_layers.len(), 1);
        assert_eq!(restored.custom_layers[0].id, "mood");
    }

    #[test]
    fn should_default_missing_fields_on_deserialize() {
        let json = r#"{"greeting_template": "Hi!"}"#;
        let config: UserStatusConfig = serde_json::from_str(json).unwrap();
        assert!(config.provider_visible);
        assert!(config.metrics_visible);
        assert!(config.custom_layers.is_empty());
        assert_eq!(config.greeting_duration_secs, 5);
    }

    #[test]
    fn test_fmt_tokens() {
        assert_eq!(fmt_tokens(0), "0");
        assert_eq!(fmt_tokens(350), "350");
        assert_eq!(fmt_tokens(999), "999");
        assert_eq!(fmt_tokens(1000), "1.0k");
        assert_eq!(fmt_tokens(1200), "1.2k");
        assert_eq!(fmt_tokens(15600), "15.6k");
    }

    #[test]
    fn test_has_cjk() {
        assert!(has_cjk("你好世界"));
        assert!(has_cjk("正在炼丹"));
        assert!(has_cjk("hello 世界"));
        assert!(!has_cjk("Considering"));
        assert!(!has_cjk(""));
    }
}
