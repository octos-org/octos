use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use metrics::counter;
use octos_core::OutboundMessage;
use tracing::warn;

/// Maximum number of pending messages buffered per inactive session.
pub(crate) const MAX_PENDING_PER_SESSION: usize = 50;
const MAX_PENDING_KEYS: usize = 256;
const MAX_PENDING_BYTES: usize = 16 * 1024 * 1024;
const PENDING_MESSAGE_TTL: Duration = Duration::from_secs(30 * 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PendingBufferResult {
    Buffered { is_first: bool },
    SessionFull,
    GlobalFull,
}

pub(crate) struct PendingEntry {
    messages: Vec<OutboundMessage>,
    bytes: usize,
    pub(crate) last_activity: Instant,
    buffer_full: bool,
}

#[derive(Default)]
pub(crate) struct PendingState {
    pub(crate) entries: HashMap<String, PendingEntry>,
    total_bytes: usize,
    max_keys: usize,
    pub(crate) max_bytes: usize,
    pub(crate) ttl: Duration,
}

impl PendingState {
    fn remove_key(&mut self, key: &str) -> Option<PendingEntry> {
        let entry = self.entries.remove(key)?;
        self.total_bytes = self.total_bytes.saturating_sub(key.len() + entry.bytes);
        Some(entry)
    }

    fn prune_expired(&mut self, now: Instant) -> usize {
        let ttl = self.ttl;
        let expired: Vec<String> = self
            .entries
            .iter()
            .filter(|(_, entry)| now.duration_since(entry.last_activity) >= ttl)
            .map(|(key, _)| key.clone())
            .collect();
        let count = expired.len();
        for key in expired {
            self.remove_key(&key);
        }
        count
    }
}

/// Shared, bounded buffer of outbound messages from inactive sessions.
/// Flushed in original order when the user switches to that session via `/s`.
/// The byte estimate covers keys, content, media strings, and serialized
/// metadata; entry counts and struct overhead are bounded separately.
#[derive(Clone)]
pub struct PendingMessages {
    state: Arc<StdMutex<PendingState>>,
}

impl Default for PendingMessages {
    fn default() -> Self {
        Self::with_limits(MAX_PENDING_KEYS, MAX_PENDING_BYTES, PENDING_MESSAGE_TTL)
    }
}

impl PendingMessages {
    fn with_limits(max_keys: usize, max_bytes: usize, ttl: Duration) -> Self {
        let state = Arc::new(StdMutex::new(PendingState {
            entries: HashMap::new(),
            total_bytes: 0,
            max_keys,
            max_bytes,
            ttl,
        }));
        // One weak reaper per store (never per session) makes TTL a real idle
        // lifecycle bound instead of waiting for the next push or dispatch.
        // Lazy pruning remains the fallback outside a Tokio runtime.
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let weak = Arc::downgrade(&state);
            let period = (ttl / 2).clamp(Duration::from_millis(10), Duration::from_secs(60));
            runtime.spawn(async move {
                let mut interval = tokio::time::interval(period);
                interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                loop {
                    interval.tick().await;
                    let Some(state) = weak.upgrade() else {
                        break;
                    };
                    let expired = {
                        let mut state = state
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                        state.prune_expired(Instant::now())
                    };
                    if expired > 0 {
                        counter!("octos_pending_messages_evicted_total", "reason" => "ttl")
                            .increment(expired as u64);
                    }
                }
            });
        }
        Self { state }
    }

    #[cfg(test)]
    pub(crate) fn with_limits_for_test(max_keys: usize, max_bytes: usize, ttl: Duration) -> Self {
        Self::with_limits(max_keys, max_bytes, ttl)
    }

    pub(crate) fn lock(&self) -> std::sync::MutexGuard<'_, PendingState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub(crate) fn push(&self, key: String, msg: OutboundMessage) -> PendingBufferResult {
        let now = Instant::now();
        let msg_bytes = pending_message_bytes(&msg);
        let mut state = self.lock();
        state.prune_expired(now);
        let at_session_cap = state
            .entries
            .get(&key)
            .is_some_and(|entry| entry.messages.len() >= MAX_PENDING_PER_SESSION);
        if at_session_cap {
            let entry = state.entries.get_mut(&key).expect("entry exists");
            entry.last_activity = now;
            // Record the notice without allocating it inside the bounded
            // buffer. `take` appends it after removing accounting, so even a
            // 100%-full byte budget cannot suppress the warning.
            entry.buffer_full = true;
            return PendingBufferResult::SessionFull;
        }

        if msg_bytes > state.max_bytes {
            counter!("octos_pending_messages_dropped_total", "reason" => "oversized").increment(1);
            return PendingBufferResult::GlobalFull;
        }

        let is_new_key = !state.entries.contains_key(&key);
        let added_bytes = msg_bytes + if is_new_key { key.len() } else { 0 };
        // Reject an impossible fit BEFORE evicting anyone. Otherwise one
        // oversized message could discard every other replayable session and
        // still be dropped itself.
        let current_key_bytes = state
            .entries
            .get(&key)
            .map(|entry| key.len() + entry.bytes)
            .unwrap_or(0);
        let required_bytes = current_key_bytes + msg_bytes + if is_new_key { key.len() } else { 0 };
        if required_bytes > state.max_bytes {
            if let Some(entry) = state.entries.get_mut(&key) {
                entry.last_activity = now;
            }
            counter!("octos_pending_messages_dropped_total", "reason" => "global_budget")
                .increment(1);
            return PendingBufferResult::GlobalFull;
        }
        loop {
            let key_over = is_new_key && state.entries.len() >= state.max_keys;
            let bytes_over = state.total_bytes + added_bytes > state.max_bytes;
            if !key_over && !bytes_over {
                break;
            }
            let oldest = state
                .entries
                .iter()
                .filter(|(existing, _)| existing.as_str() != key)
                .min_by_key(|(_, entry)| entry.last_activity)
                .map(|(existing, _)| existing.clone());
            let Some(oldest) = oldest else {
                if let Some(entry) = state.entries.get_mut(&key) {
                    entry.last_activity = now;
                }
                counter!("octos_pending_messages_dropped_total", "reason" => "global_budget")
                    .increment(1);
                return PendingBufferResult::GlobalFull;
            };
            warn!(session = %oldest, "evicting old pending session within global budget");
            state.remove_key(&oldest);
            counter!("octos_pending_messages_evicted_total", "reason" => "global_budget")
                .increment(1);
        }

        let is_first = state
            .entries
            .get(&key)
            .is_none_or(|entry| entry.messages.is_empty());
        if is_new_key {
            state.total_bytes += key.len();
        }
        state.total_bytes += msg_bytes;
        let entry = state.entries.entry(key).or_insert_with(|| PendingEntry {
            messages: Vec::new(),
            bytes: 0,
            last_activity: now,
            buffer_full: false,
        });
        entry.bytes += msg_bytes;
        entry.last_activity = now;
        entry.messages.push(msg);
        PendingBufferResult::Buffered { is_first }
    }

    pub(crate) fn take(&self, key: &str) -> Vec<OutboundMessage> {
        let Some(mut entry) = self.lock().remove_key(key) else {
            return Vec::new();
        };
        if entry.buffer_full
            && let Some(last) = entry.messages.last_mut()
        {
            last.content = format!(
                "{}\n\n⚠️ Buffer full ({MAX_PENDING_PER_SESSION} messages). \
                 Some responses were dropped. Switch to this session to continue.",
                last.content
            );
        }
        entry.messages
    }

    pub(crate) fn remove_session(&self, session_key: &str) {
        let scoped_suffix = format!(":{session_key}");
        let keys: Vec<String> = self
            .lock()
            .entries
            .keys()
            .filter(|key| key.as_str() == session_key || key.ends_with(&scoped_suffix))
            .cloned()
            .collect();
        let mut state = self.lock();
        for key in keys {
            state.remove_key(&key);
        }
    }

    pub(crate) fn prune_expired(&self) -> usize {
        self.lock().prune_expired(Instant::now())
    }

    #[cfg(test)]
    pub(crate) fn accounted_stats(&self) -> (usize, usize) {
        let state = self.lock();
        (state.entries.len(), state.total_bytes)
    }
}

fn pending_message_bytes(message: &OutboundMessage) -> usize {
    message.channel.len()
        + message.chat_id.len()
        + message.content.len()
        + message.reply_to.as_deref().unwrap_or("").len()
        + message.media.iter().map(String::len).sum::<usize>()
        + message.metadata.to_string().len()
}

#[cfg(test)]
pub(crate) async fn pending_insert_for_bench(
    pending: &PendingMessages,
    key: String,
    msg: OutboundMessage,
) {
    let _ = pending.push(key, msg);
}

#[cfg(test)]
pub(crate) async fn pending_stats_for_bench(pending: &PendingMessages) -> (usize, usize) {
    pending.accounted_stats()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pending_message(content: &str) -> OutboundMessage {
        OutboundMessage {
            channel: "telegram".to_owned(),
            chat_id: "pending".to_owned(),
            content: content.to_owned(),
            reply_to: None,
            media: vec![],
            metadata: serde_json::json!({}),
        }
    }

    fn expire_pending_entry(pending: &PendingMessages, key: &str) {
        let mut state = pending.lock();
        let ttl = state.ttl;
        let entry = state.entries.get_mut(key).expect("pending entry exists");
        entry.last_activity = Instant::now().checked_sub(ttl).unwrap_or_else(Instant::now);
    }

    #[test]
    fn should_bound_pending_keys_and_count_oversized_media_and_metadata() {
        let pending = PendingMessages::with_limits_for_test(2, 512, Duration::from_secs(60));
        let mut oversized = pending_message("small content");
        oversized.media = vec!["m".repeat(400)];
        oversized.metadata = serde_json::json!({"blob": "d".repeat(400)});
        assert_eq!(
            pending.push("oversized".to_owned(), oversized),
            PendingBufferResult::GlobalFull
        );
        assert_eq!(pending.accounted_stats(), (0, 0));

        for (key, content) in [("old", "old"), ("middle", "middle"), ("new", "new")] {
            assert!(matches!(
                pending.push(key.to_owned(), pending_message(content)),
                PendingBufferResult::Buffered { .. }
            ));
        }
        let (keys, bytes) = pending.accounted_stats();
        assert!(keys <= 2);
        assert!(bytes <= 512);
        assert!(pending.take("old").is_empty());
        assert_eq!(pending.take("new")[0].content, "new");
    }

    #[test]
    fn should_not_evict_live_entries_for_an_unbufferable_message() {
        let pending = PendingMessages::with_limits_for_test(2, 512, Duration::from_secs(60));
        pending.push("a".to_owned(), pending_message("a"));
        pending.push("b".to_owned(), pending_message("b"));
        let before = pending.accounted_stats();

        let rejected = pending.push(
            "new-key-with-a-long-name".to_owned(),
            pending_message(&"x".repeat(480)),
        );
        assert_eq!(rejected, PendingBufferResult::GlobalFull);
        assert_eq!(
            pending.accounted_stats(),
            before,
            "rejection must not evict"
        );

        let rejected = pending.push("a".to_owned(), pending_message(&"x".repeat(480)));
        assert_eq!(rejected, PendingBufferResult::GlobalFull);
        assert_eq!(pending.take("b")[0].content, "b");
        assert_eq!(pending.take("a")[0].content, "a");
    }

    #[test]
    fn should_preserve_flush_order_and_refresh_pending_ttl() {
        let pending = PendingMessages::with_limits_for_test(4, 4096, Duration::from_secs(60));
        pending.push("a".to_owned(), pending_message("a1"));
        pending.push("a".to_owned(), pending_message("a2"));
        pending.push("b".to_owned(), pending_message("b1"));
        let a: Vec<String> = pending
            .take("a")
            .into_iter()
            .map(|message| message.content)
            .collect();
        assert_eq!(a, ["a1", "a2"]);
        assert_eq!(pending.accounted_stats().0, 1);

        let previous = pending.lock().entries["b"].last_activity;
        pending.push("b".to_owned(), pending_message("b2"));
        assert!(pending.lock().entries["b"].last_activity >= previous);
        assert_eq!(
            pending.prune_expired(),
            0,
            "active entry must not be reaped"
        );
        let b: Vec<String> = pending
            .take("b")
            .into_iter()
            .map(|message| message.content)
            .collect();
        assert_eq!(b, ["b1", "b2"]);

        pending.push("expired".to_owned(), pending_message("expired"));
        expire_pending_entry(&pending, "expired");
        assert_eq!(pending.prune_expired(), 1);
        assert_eq!(pending.accounted_stats(), (0, 0));
    }

    #[test]
    fn should_keep_per_session_full_notice_within_the_global_budget() {
        let pending = PendingMessages::with_limits_for_test(2, 4096, Duration::from_secs(60));
        for index in 0..MAX_PENDING_PER_SESSION {
            pending.push(
                "full".to_owned(),
                pending_message(&format!("message-{index}")),
            );
        }
        assert_eq!(
            pending.push("full".to_owned(), pending_message(&"x".repeat(8192))),
            PendingBufferResult::SessionFull
        );
        let messages = pending.take("full");
        assert_eq!(messages.len(), MAX_PENDING_PER_SESSION);
        assert!(messages.last().unwrap().content.contains("Buffer full"));
        assert_eq!(pending.accounted_stats(), (0, 0));

        let capped = PendingMessages::with_limits_for_test(2, 4096, Duration::from_secs(60));
        for index in 0..MAX_PENDING_PER_SESSION {
            capped.push(
                "capped".to_owned(),
                pending_message(&format!("message-{index}")),
            );
        }
        let retained_bytes = capped.accounted_stats().1;
        capped.lock().max_bytes = retained_bytes;
        assert_eq!(
            capped.push("capped".to_owned(), pending_message("overflow")),
            PendingBufferResult::SessionFull
        );
        let messages = capped.take("capped");
        assert!(messages.last().unwrap().content.contains("Buffer full"));
    }

    #[tokio::test]
    async fn should_expire_pending_entries_without_another_dispatch() {
        let pending = PendingMessages::with_limits_for_test(4, 4096, Duration::from_millis(30));
        pending.push("idle-ttl".to_owned(), pending_message("expired"));
        tokio::time::timeout(Duration::from_secs(1), async {
            while pending.accounted_stats().0 != 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("background TTL reaper must run without a new dispatch");
        assert_eq!(pending.accounted_stats(), (0, 0));
    }

    fn test_rss_bytes() -> Option<usize> {
        #[cfg(target_os = "linux")]
        {
            std::fs::read_to_string("/proc/self/status")
                .unwrap()
                .lines()
                .find_map(|line| {
                    line.strip_prefix("VmRSS:")
                        .and_then(|value| value.trim().strip_suffix(" kB"))
                        .and_then(|value| value.trim().parse::<usize>().ok())
                        .map(|kib| kib * 1024)
                })
        }
        #[cfg(not(target_os = "linux"))]
        {
            None
        }
    }

    #[tokio::test]
    #[ignore = "opt-in S2 pressure benchmark; run without other parallel tests"]
    async fn s2_pending_messages_pressure_bench() {
        let pending = PendingMessages::default();
        let started = std::time::Instant::now();
        let rss_before = test_rss_bytes();
        for session in 0..512 {
            for message in 0..50 {
                pending_insert_for_bench(
                    &pending,
                    format!("telegram:bench-{session}:topic-{session}"),
                    OutboundMessage {
                        channel: "telegram".to_owned(),
                        chat_id: format!("bench-{session}"),
                        content: format!("message-{message}-{}", "c".repeat(2048)),
                        reply_to: None,
                        media: vec!["m".repeat(512)],
                        metadata: serde_json::json!({"blob": "d".repeat(512)}),
                    },
                )
                .await;
            }
        }
        let (keys, bytes) = pending_stats_for_bench(&pending).await;
        let rss_after = test_rss_bytes();
        let rss = rss_before
            .zip(rss_after)
            .map_or_else(String::new, |(before, after)| {
                format!(
                    ",\"rss_before_mib\":{:.3},\"rss_after_mib\":{:.3}",
                    before as f64 / (1024.0 * 1024.0),
                    after as f64 / (1024.0 * 1024.0)
                )
            });
        println!(
            "{{\"sessions\":512,\"messages_per_session\":50,\"keys\":{keys},\"accounted_mib\":{:.3},\"wall_ms\":{:.3}{rss}}}",
            bytes as f64 / (1024.0 * 1024.0),
            started.elapsed().as_secs_f64() * 1000.0,
        );
    }
}
