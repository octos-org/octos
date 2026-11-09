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
        let keys: Vec<String> = self
            .lock()
            .entries
            .keys()
            .filter(|key| matches_deleted_session_key(key, session_key))
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
}

fn pending_message_bytes(message: &OutboundMessage) -> usize {
    message.channel.len()
        + message.chat_id.len()
        + message.content.len()
        + message.reply_to.as_deref().unwrap_or("").len()
        + message.media.iter().map(String::len).sum::<usize>()
        + message.metadata.to_string().len()
}

/// Base deletion covers all topic actors/buffers; a topic key remains exact.
pub(crate) fn matches_deleted_session_key(owner: &str, deleted: &str) -> bool {
    let owner = octos_core::SessionKey(owner.to_owned());
    let deleted = octos_core::SessionKey(deleted.to_owned());
    (owner.base_key() == deleted.base_key()
        || owner
            .base_key()
            .ends_with(&format!(":{}", deleted.base_key())))
        && (deleted.topic().is_none() || deleted.topic() == owner.topic())
}
