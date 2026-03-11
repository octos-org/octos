//! Dynamic status indicator for gateway channels.
//!
//! Provides Claude Code-style "thinking" indicators:
//! - Typing indicator (platform-native, e.g. Telegram "typing...")
//! - Editable status message with rotating creative words + elapsed time + token counts
//! - Automatic cleanup when the real response arrives

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Instant;

use crew_agent::TokenTracker;
use crew_bus::Channel;
use crew_core::OutboundMessage;
use tokio::sync::Mutex;
use tracing::warn;

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

        tokio::spawn(async move {
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
        }
    }
}

/// Handle to an active status indicator. Call `stop()` to clean up.
pub struct StatusHandle {
    pub(crate) cancelled: Arc<AtomicBool>,
    pub(crate) status_msg_id: Arc<Mutex<Option<String>>>,
    channel: Arc<dyn Channel>,
    chat_id: String,
}

impl StatusHandle {
    /// Stop the status indicator and delete the status message.
    pub async fn stop(self) {
        self.cancelled.store(true, Ordering::Release);

        // Give the loop a moment to notice cancellation
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Delete the status message if one was sent
        let msg_id = self.status_msg_id.lock().await.take();
        if let Some(mid) = msg_id {
            if let Err(e) = self.channel.delete_message(&self.chat_id, &mid).await {
                // Not critical — message might already be gone
                warn!("failed to delete status message: {e}");
            }
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

    // Wait 2 seconds before sending a visible status message
    for _ in 0..4 {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        if cancelled.load(Ordering::Acquire) {
            return;
        }
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
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
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
}
