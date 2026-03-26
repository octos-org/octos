//! Dynamic status indicator for gateway channels.
//!
//! Provides Claude Code-style "thinking" indicators:
//! - Typing indicator (platform-native, e.g. Telegram "typing...")
//! - Editable status message with rotating creative words + elapsed time + token counts
//! - Automatic cleanup when the real response arrives

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use octos_agent::TokenTracker;
use octos_bus::Channel;
use octos_core::OutboundMessage;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tracing::warn;

/// Maximum time `stop()` will wait before giving up on cleanup.
const STOP_TIMEOUT: Duration = Duration::from_secs(3);

/// Manages status indicators for a specific channel.
pub struct StatusIndicator {
    channel: Arc<dyn Channel>,
    status_words: Arc<std::sync::RwLock<Vec<String>>>,
    /// Global word index — increments once per session start for slow rotation.
    word_index: AtomicUsize,
}

impl StatusIndicator {
    /// Get a reference to the underlying channel (for stream forwarding).
    pub fn channel(&self) -> &Arc<dyn Channel> {
        &self.channel
    }
}

impl StatusIndicator {
    pub fn new(channel: Arc<dyn Channel>, status_words: Vec<String>) -> Self {
        Self {
            channel,
            status_words: Arc::new(std::sync::RwLock::new(status_words)),
            word_index: AtomicUsize::new(0),
        }
    }

    /// Update the status word pool (called when PersonaService refreshes).
    pub fn set_words(&self, words: Vec<String>) {
        if let Ok(mut w) = self.status_words.write() {
            *w = words;
        }
    }

    /// Get a shared handle to the status words (for pipeline status bridge).
    pub fn status_words_handle(&self) -> Arc<std::sync::RwLock<Vec<String>>> {
        Arc::clone(&self.status_words)
    }

    /// Start showing status for a chat session. Returns a handle to stop it.
    ///
    /// `message_text` is the inbound user message — used to detect language
    /// and pick Chinese or English status words accordingly.
    ///
    /// `tracker` is shared with the agent loop to show live token counts.
    pub fn start(
        &self,
        chat_id: String,
        message_text: &str,
        tracker: Arc<TokenTracker>,
        voice_transcript: Option<String>,
    ) -> StatusHandle {
        let cancelled = Arc::new(AtomicBool::new(false));
        let status_msg_id = Arc::new(Mutex::new(None::<String>));
        let channel = Arc::clone(&self.channel);

        let is_chinese = has_cjk(message_text);

        // Pick the next word (slow rotation across sessions)
        let idx = self.word_index.fetch_add(1, Ordering::Relaxed);
        let words = self.status_words.read().unwrap_or_else(|e| e.into_inner());

        // Filter words by detected language
        let filtered: Vec<String> = words
            .iter()
            .filter(|w| if is_chinese { has_cjk(w) } else { !has_cjk(w) })
            .cloned()
            .collect();

        // Fall back to language-appropriate defaults if filter yielded nothing
        let words_snapshot = if filtered.is_empty() {
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
        };
        drop(words);

        let word_count = words_snapshot.len().max(1);
        let initial_word_idx = idx % word_count;

        let cancelled_clone = Arc::clone(&cancelled);
        let msg_id_clone = Arc::clone(&status_msg_id);
        let channel_clone = Arc::clone(&channel);
        let chat_id_clone = chat_id.clone();
        let tracker_clone = Arc::clone(&tracker);

        let join_handle = tokio::spawn(async move {
            run_status_loop(
                channel_clone,
                chat_id_clone,
                words_snapshot,
                initial_word_idx,
                cancelled_clone,
                msg_id_clone,
                tracker_clone,
                voice_transcript,
            )
            .await;
        });

        StatusHandle {
            cancelled,
            status_msg_id,
            channel,
            chat_id,
            join_handle: Some(join_handle),
        }
    }
}

/// Handle to an active status indicator. Call `stop()` to clean up.
pub struct StatusHandle {
    pub(crate) cancelled: Arc<AtomicBool>,
    pub(crate) status_msg_id: Arc<Mutex<Option<String>>>,
    channel: Arc<dyn Channel>,
    chat_id: String,
    /// Wrapped in Option so we can take it in stop() without conflicting
    /// with the Drop impl.
    join_handle: Option<JoinHandle<()>>,
}

impl StatusHandle {
    /// Stop the status indicator and delete the status message.
    ///
    /// Bounded by `STOP_TIMEOUT` to prevent blocking the session actor
    /// if a channel operation (delete_message) hangs due to network
    /// issues or rate limiting.
    pub async fn stop(mut self) {
        // Signal cancellation — the loop checks this every iteration.
        self.cancelled.store(true, Ordering::Release);

        // Abort the loop task so it doesn't keep running if stuck in a
        // slow channel operation (send_typing, edit_message, send_with_id).
        if let Some(handle) = self.join_handle.take() {
            handle.abort();
            // Wait for the loop task to finish (should be near-instant
            // since we just aborted it).
            let _ = handle.await;
        }

        // Best-effort cleanup: delete the status message with a timeout
        // so we never block the session actor indefinitely.
        let msg_id = self.status_msg_id.clone();
        let channel = self.channel.clone();
        let chat_id = self.chat_id.clone();

        let cleanup = async move {
            let _ = channel.stop_typing(&chat_id).await;
            let mid = msg_id.lock().await.take();
            if let Some(mid) = mid {
                if let Err(e) = channel.delete_message(&chat_id, &mid).await {
                    warn!("failed to delete status message: {e}");
                }
            }
        };

        if tokio::time::timeout(STOP_TIMEOUT, cleanup).await.is_err() {
            warn!("status indicator stop timed out after {STOP_TIMEOUT:?}");
        }
    }
}

impl Drop for StatusHandle {
    fn drop(&mut self) {
        // Safety net: if stop() was never called, abort the loop task
        // to prevent leaked background tasks.
        self.cancelled.store(true, Ordering::Release);
        if let Some(ref handle) = self.join_handle {
            handle.abort();
        }
    }
}

/// Background loop that sends typing indicators and updates the status message.
#[allow(clippy::too_many_arguments)]
async fn run_status_loop(
    channel: Arc<dyn Channel>,
    chat_id: String,
    words: Vec<String>,
    initial_word_idx: usize,
    cancelled: Arc<AtomicBool>,
    status_msg_id: Arc<Mutex<Option<String>>>,
    tracker: Arc<TokenTracker>,
    voice_transcript: Option<String>,
) {
    let start = Instant::now();
    let word_count = words.len().max(1);
    let mut word_idx = initial_word_idx;

    // Immediately send typing indicator
    let _ = channel.send_typing(&chat_id).await;

    // Wait 2 seconds before sending a visible status message.
    // Use a single sleep instead of a polling loop — task abort handles
    // cancellation instantly.
    tokio::time::sleep(Duration::from_secs(2)).await;
    if cancelled.load(Ordering::Acquire) {
        return;
    }

    // Send the initial status message
    let word = &words[word_idx % word_count];
    let ti = tracker.input_tokens.load(Ordering::Relaxed);
    let to = tracker.output_tokens.load(Ordering::Relaxed);
    let status_text = format_status(
        word,
        start.elapsed().as_secs(),
        ti,
        to,
        voice_transcript.as_deref(),
    );

    let msg = OutboundMessage {
        channel: channel.name().to_string(),
        chat_id: chat_id.clone(),
        content: status_text,
        reply_to: None,
        media: vec![],
        metadata: serde_json::json!({}),
    };

    // Only send the status message on channels that support editing.
    // Channels without edit support (WeCom bot, etc.) can't delete or update it,
    // so it would remain as a stale "Thinking..." message.
    if channel.supports_edit() {
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

    // Update loop: typing every 5s, edit status message every 8s
    let mut tick = 0u32;
    loop {
        tokio::time::sleep(Duration::from_secs(1)).await;
        if cancelled.load(Ordering::Acquire) {
            return;
        }
        tick += 1;

        // Send typing indicator every 5 seconds
        if tick % 5 == 0 {
            let _ = channel.send_typing(&chat_id).await;
        }

        // Edit status message every 8 seconds
        if tick % 8 == 0 {
            word_idx += 1;
            let word = &words[word_idx % word_count];
            let elapsed = start.elapsed().as_secs();
            let ti = tracker.input_tokens.load(Ordering::Relaxed);
            let to = tracker.output_tokens.load(Ordering::Relaxed);
            let new_text = format_status(word, elapsed, ti, to, voice_transcript.as_deref());

            let mid = status_msg_id.lock().await.clone();
            if let Some(ref mid) = mid {
                let _ = channel.edit_message(&chat_id, mid, &new_text).await;
            }
        }
    }
}

/// Check if text contains CJK characters (Chinese/Japanese/Korean).
fn has_cjk(s: &str) -> bool {
    s.chars()
        .any(|c| matches!(c, '\u{4E00}'..='\u{9FFF}' | '\u{3400}'..='\u{4DBF}' | '\u{F900}'..='\u{FAFF}'))
}

/// Format a compact token count like "1.2k" or "350".
fn fmt_tokens(n: u32) -> String {
    if n >= 1000 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else {
        n.to_string()
    }
}

/// Format a status message like "✦ Considering... (12s · 1.2k↑ 350↓)"
/// With voice transcript: "✦ Considering... 🎙 what about today's weather (12s)"
fn format_status(
    word: &str,
    elapsed_secs: u64,
    input_tokens: u32,
    output_tokens: u32,
    voice_transcript: Option<&str>,
) -> String {
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

    // Combine parts with " · " separator
    let details: Vec<&str> = [time_part.as_str(), token_part.as_str()]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect();

    // Voice transcript suffix (truncate long transcripts, char-safe)
    let voice_part = voice_transcript.map(|t| {
        let truncated: String = t.chars().take(80).collect();
        if truncated.len() < t.len() {
            format!("\n🎙 {truncated}...")
        } else {
            format!("\n🎙 {truncated}")
        }
    });

    let base = if details.is_empty() {
        format!("✦ {word}...")
    } else {
        format!("✦ {word}... ({})", details.join(" · "))
    };

    match voice_part {
        Some(vp) => format!("{base}{vp}"),
        None => base,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_status_short() {
        assert_eq!(format_status("Thinking", 1, 0, 0, None), "✦ Thinking...");
        assert_eq!(format_status("Thinking", 2, 0, 0, None), "✦ Thinking...");
    }

    #[test]
    fn test_format_status_seconds() {
        assert_eq!(
            format_status("Pondering", 15, 0, 0, None),
            "✦ Pondering... (15s)"
        );
    }

    #[test]
    fn test_format_status_with_tokens() {
        assert_eq!(
            format_status("Pondering", 15, 1200, 350, None),
            "✦ Pondering... (15s · 1.2k↑ 350↓)"
        );
    }

    #[test]
    fn test_format_status_tokens_only() {
        assert_eq!(
            format_status("Thinking", 1, 500, 100, None),
            "✦ Thinking... (500↑ 100↓)"
        );
    }

    #[test]
    fn test_format_status_minutes_with_tokens() {
        assert_eq!(
            format_status("Synthesizing", 65, 5000, 1200, None),
            "✦ Synthesizing... (1m 5s · 5.0k↑ 1.2k↓)"
        );
    }

    #[test]
    fn test_has_cjk() {
        assert!(has_cjk("你好世界"));
        assert!(has_cjk("正在炼丹"));
        assert!(has_cjk("hello 世界"));
        assert!(!has_cjk("Considering"));
        assert!(!has_cjk("Synthesizing"));
        assert!(!has_cjk(""));
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

    #[tokio::test]
    async fn should_stop_within_timeout_when_loop_is_running() {
        // Verify that stop() completes within STOP_TIMEOUT even if the
        // loop task is spawned (abort + timeout prevents deadlock).
        let cancelled = Arc::new(AtomicBool::new(false));
        let status_msg_id = Arc::new(Mutex::new(None::<String>));
        let cancelled_clone = Arc::clone(&cancelled);

        let handle = tokio::spawn(async move {
            // Simulate a long-running status loop
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                if cancelled_clone.load(Ordering::Acquire) {
                    return;
                }
            }
        });

        let status_handle = StatusHandle {
            cancelled,
            status_msg_id,
            channel: Arc::new(NullChannel),
            chat_id: "test".to_string(),
            join_handle: Some(handle),
        };

        let start = Instant::now();
        status_handle.stop().await;
        // stop() should complete almost instantly (abort + no message to delete)
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    /// Reproduces the deadlock: stop() blocks forever when delete_message hangs.
    /// This test uses the OLD stop() logic (no abort, no timeout) to prove it hangs,
    /// then verifies the NEW logic completes quickly.
    #[tokio::test]
    async fn should_not_deadlock_when_delete_message_hangs() {
        // Simulate a channel where delete_message blocks for 60 seconds
        // (e.g. Telegram API rate-limited or network stall).
        let channel: Arc<dyn Channel> = Arc::new(SlowDeleteChannel);

        let cancelled = Arc::new(AtomicBool::new(false));
        let status_msg_id = Arc::new(Mutex::new(Some("msg123".to_string())));
        let cancelled_clone = Arc::clone(&cancelled);

        let handle = tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                if cancelled_clone.load(Ordering::Acquire) {
                    return;
                }
            }
        });

        let status_handle = StatusHandle {
            cancelled,
            status_msg_id,
            channel,
            chat_id: "test_chat".to_string(),
            join_handle: Some(handle),
        };

        // With the fix: stop() must complete within STOP_TIMEOUT (3s),
        // NOT block for 60s like the slow channel would cause.
        let start = Instant::now();
        status_handle.stop().await;
        let elapsed = start.elapsed();

        assert!(
            elapsed < Duration::from_secs(5),
            "stop() took {elapsed:?} — should complete within STOP_TIMEOUT, not hang"
        );
    }

    /// Minimal channel impl for testing.
    struct NullChannel;

    #[async_trait::async_trait]
    impl Channel for NullChannel {
        fn name(&self) -> &str {
            "null"
        }
        async fn start(
            &self,
            _tx: tokio::sync::mpsc::Sender<octos_core::InboundMessage>,
        ) -> eyre::Result<()> {
            Ok(())
        }
        async fn send(&self, _msg: &OutboundMessage) -> eyre::Result<()> {
            Ok(())
        }
    }

    #[derive(Default)]
    struct StopTypingChannel {
        stop_calls: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl Channel for StopTypingChannel {
        fn name(&self) -> &str {
            "stop-typing"
        }
        async fn start(
            &self,
            _tx: tokio::sync::mpsc::Sender<octos_core::InboundMessage>,
        ) -> eyre::Result<()> {
            Ok(())
        }
        async fn send(&self, _msg: &OutboundMessage) -> eyre::Result<()> {
            Ok(())
        }
        async fn stop_typing(&self, _chat_id: &str) -> eyre::Result<()> {
            self.stop_calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    #[tokio::test]
    async fn should_stop_typing_on_stop() {
        let channel = Arc::new(StopTypingChannel::default());
        let status_handle = StatusHandle {
            cancelled: Arc::new(AtomicBool::new(false)),
            status_msg_id: Arc::new(Mutex::new(None::<String>)),
            channel: channel.clone(),
            chat_id: "test_chat".to_string(),
            join_handle: None,
        };

        status_handle.stop().await;

        assert_eq!(channel.stop_calls.load(Ordering::SeqCst), 1);
    }

    /// Channel where delete_message blocks for 60 seconds (simulates hung API).
    struct SlowDeleteChannel;

    #[async_trait::async_trait]
    impl Channel for SlowDeleteChannel {
        fn name(&self) -> &str {
            "slow"
        }
        async fn start(
            &self,
            _tx: tokio::sync::mpsc::Sender<octos_core::InboundMessage>,
        ) -> eyre::Result<()> {
            Ok(())
        }
        async fn send(&self, _msg: &OutboundMessage) -> eyre::Result<()> {
            Ok(())
        }
        async fn delete_message(&self, _chat_id: &str, _message_id: &str) -> eyre::Result<()> {
            // Simulate a hung Telegram API call
            tokio::time::sleep(Duration::from_secs(60)).await;
            Ok(())
        }
    }
}
