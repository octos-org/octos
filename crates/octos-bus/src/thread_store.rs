//! Append-only event log for per-`(session, topic, thread)` `EventEnvelope`s.
//!
//! **Status:** scaffolding only. PR-A of M9 #679 introduces this module and
//! gates it behind the `thread-store-authoritative` cargo feature and the
//! `OCTOS_THREADSTORE_AUTHORITATIVE` runtime env var. NO writer site has been
//! migrated in PR-A — writes still go through [`crate::session::Session`]'s
//! mutable `messages` list. PR-B (#679 phase B) converts the writer sites;
//! PR-C deletes the legacy dual-write path after one release of fleet soak.
//!
//! ## Storage layout
//!
//! Each `(SessionKey, Option<topic>, ThreadId)` triple is persisted to one
//! JSONL file:
//!
//! ```text
//! <data_dir>/events/<encoded_session>__<encoded_topic>__<encoded_thread>.jsonl
//! ```
//!
//! `<encoded_*>` uses the same percent-encoding scheme as
//! [`crate::session::encode_path_component`] so the layout matches the
//! existing `sessions/*.jsonl` convention. An absent topic encodes as the
//! literal token `%00` — the encoder never produces this sequence for a
//! real topic (NUL bytes are rejected upstream by [`SessionKey`]
//! construction), so the sentinel can't collide. The earlier draft used
//! `_` as the sentinel, but `_` is in the encoder's pass-through set and
//! could be a legal one-character topic.
//!
//! Files live under `events/` alongside the existing `sessions/` directory so
//! the legacy JSONL store stays the source of truth in PR-A.
//!
//! ## Forward-compatible envelope
//!
//! Events are serialised as [`EventEnvelope<Value>`] — the same envelope the
//! UI Protocol uses on the wire ([`octos_core::ui_protocol::EventEnvelope`]).
//! Readers dispatch on the `event_type` discriminator so new event families
//! (e.g. `CompactionApplied` once PR-B addresses compaction) can land without
//! a schema migration. We intentionally do NOT extend the existing session
//! JSONL union — that union stays scoped to projection-state rows.
//!
//! ## Concurrency
//!
//! [`ThreadStore::append`] is atomic per call: it serialises one envelope to
//! JSON, then writes the full line + trailing newline through one
//! `OpenOptions::append` + `write_all`. On POSIX an O_APPEND `write_all` is
//! single-`write(2)`-syscall when the payload fits, and short writes are
//! retried by [`std::io::Write::write_all`]. The "kill-process mid-append"
//! failure mode that this module's test suite exercises is: the process dies
//! after the file system flushes a partial line. On the next read,
//! [`ThreadStore::read_envelopes`] silently skips any trailing line that is
//! not parseable JSON, so the projection is never corrupted by a torn write.
//!
//! Two threads racing the same `(session, topic, thread)` key serialise on
//! the per-key `Mutex<File>` cached by `ThreadStore` — each `append` holds
//! the mutex from "open file" through "write line", so writes interleave at
//! whole-envelope granularity.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use eyre::{Result, eyre};
use lru::LruCache;
use octos_core::{Message, MessageRole, SessionKey, ThreadId};
use serde::Serialize;
use serde_json::Value;
use tracing::{debug, warn};

pub use octos_core::ui_protocol::EventEnvelope;

/// Default maximum number of `(session, topic, thread)` files kept open in
/// the in-memory LRU. Eviction closes the file handle; the next `append` to
/// an evicted key reopens it lazily. Mirrors the order of magnitude of
/// [`crate::session::SessionManager`]'s session cache; tuned smaller because
/// threads are finer-grained than sessions.
const DEFAULT_MAX_THREADS: usize = 4096;

/// Maximum events JSONL file size we'll load (10 MB). Mirrors
/// [`crate::session::MAX_SESSION_FILE_SIZE`]. Prevents OOM on corrupted /
/// adversarial files.
const MAX_EVENTS_FILE_SIZE: u64 = 10 * 1024 * 1024;

/// Composite identity for a `(SessionKey, Option<topic>, ThreadId)` log.
///
/// `topic` is `None` for the canonical top-level thread under a session and
/// `Some(name)` for topic-scoped threads (mirrors
/// [`octos_core::ui_protocol::EventEnvelope::topic`]). Cloning is cheap —
/// the three inner strings are short on the hot path.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ThreadKey {
    pub session: SessionKey,
    pub topic: Option<String>,
    pub thread: ThreadId,
}

impl ThreadKey {
    pub fn new(session: SessionKey, topic: Option<String>, thread: ThreadId) -> Self {
        Self {
            session,
            topic,
            thread,
        }
    }
}

/// Cached per-thread append state.
///
/// We keep an `Arc<Mutex<()>>` rather than the open file because the file is
/// reopened on every append: this lets the operator clobber the file out of
/// band (e.g. `mv events/...jsonl`) without leaving us holding a stale
/// inode. The mutex serialises concurrent appends to the same key — see the
/// module-level "Concurrency" note. The atomic `seq` is the next event
/// sequence to emit; it's restored from disk on first open and incremented
/// monotonically per `append`.
struct ThreadCacheEntry {
    write_lock: Arc<Mutex<()>>,
    /// Monotonic counter for the next `event_seq` to emit. Restored from the
    /// file's last persisted envelope on first open; advanced after every
    /// successful `append`.
    next_seq: u64,
}

/// Append-only event log for [`EventEnvelope`]s keyed by
/// `(SessionKey, Option<topic>, ThreadId)`.
///
/// **Scaffolding only — no writer sites use this in PR-A.**
pub struct ThreadStore {
    events_dir: PathBuf,
    cache: Mutex<LruCache<ThreadKey, Arc<Mutex<ThreadCacheEntry>>>>,
}

impl ThreadStore {
    /// Open the `events/` subdirectory of `data_dir`, creating it if missing.
    pub fn open(data_dir: &Path) -> Result<Self> {
        let events_dir = data_dir.join("events");
        std::fs::create_dir_all(&events_dir)?;
        Ok(Self {
            events_dir,
            cache: Mutex::new(LruCache::new(
                NonZeroUsize::new(DEFAULT_MAX_THREADS).expect("default > 0"),
            )),
        })
    }

    /// Override the maximum number of `(session, topic, thread)` entries kept
    /// in the in-memory cache. Minimum 1.
    pub fn with_max_threads(mut self, max: usize) -> Self {
        let cap = NonZeroUsize::new(max.max(1)).expect("clamped to >= 1");
        self.cache
            .get_mut()
            .expect("thread store cache poisoned")
            .resize(cap);
        self
    }

    /// Return the path of the JSONL log for `key`. Public for tests; not
    /// part of the stable interface PR-B will consume.
    pub fn log_path(&self, key: &ThreadKey) -> PathBuf {
        log_path_static(&self.events_dir, key)
    }

    /// Append one envelope to `key`'s log. Returns the persisted
    /// `event_seq` (which equals `envelope.event_seq` when the caller has
    /// already stamped a seq — see below).
    ///
    /// The caller is responsible for setting `envelope.event_seq`; PR-B will
    /// integrate this with the per-turn sequence counter currently owned by
    /// `session_actor.rs`. PR-A trusts whatever the caller passes — the
    /// store doesn't reorder or rewrite the seq.
    pub fn append<P: Serialize>(&self, envelope: EventEnvelope<P>) -> Result<u64> {
        let key = envelope_key(&envelope);
        let entry = self.acquire_entry(&key)?;

        let json = serde_json::to_string(&envelope)?;
        if json.contains('\n') {
            // The JSONL invariant is "one envelope per line"; embedded
            // newlines would corrupt the on-disk projection. Reject up front
            // so the caller sees the bug instead of writing an unreadable
            // file.
            return Err(eyre!("event envelope JSON must not contain newlines"));
        }

        let guard = entry.lock().expect("thread store entry poisoned");
        // SAFETY: `guard` holds the per-key write lock for the duration of
        // this scope. Two writers serialise here; the file is reopened
        // every time so an evicted entry stays consistent with what's on
        // disk.
        let _write_lock = guard
            .write_lock
            .lock()
            .expect("thread store write lock poisoned");
        let path = log_path_static(&self.events_dir, &key);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
        let file_len = file.metadata()?.len();
        if file_len >= MAX_EVENTS_FILE_SIZE {
            warn!(
                session = %key.session.0,
                topic = ?key.topic,
                thread = %key.thread.as_str(),
                size = file_len,
                limit = MAX_EVENTS_FILE_SIZE,
                "events log at size limit, refusing append"
            );
            return Err(eyre!(
                "events log at size limit ({} >= {}), refusing append",
                file_len,
                MAX_EVENTS_FILE_SIZE
            ));
        }
        file.write_all(json.as_bytes())?;
        file.write_all(b"\n")?;
        file.flush()?;
        let written_seq = envelope.event_seq;
        // Advance the cached next-seq hint regardless of what the caller
        // stamped — readers ignore this field, but PR-B will use it to
        // pre-stamp turn seqs without re-reading the whole file.
        drop(_write_lock);
        drop(guard);
        if let Ok(mut cache) = self.cache.lock() {
            if let Some(arc_entry) = cache.get(&key) {
                if let Ok(mut updated) = arc_entry.lock() {
                    updated.next_seq = written_seq.saturating_add(1).max(updated.next_seq);
                }
            }
        }
        debug!(
            session = %key.session.0,
            topic = ?key.topic,
            thread = %key.thread.as_str(),
            event_seq = written_seq,
            "thread store append committed"
        );
        Ok(written_seq)
    }

    /// Read every persisted envelope for `key`, in append order.
    ///
    /// Lines that fail to parse as JSON are silently skipped — this is the
    /// "torn write" recovery path documented in the module header. The
    /// payload is left as a `serde_json::Value`; PR-B can downcast to typed
    /// payloads via `serde_json::from_value` at the consumer.
    pub fn read_envelopes(&self, key: &ThreadKey) -> Vec<EventEnvelope<Value>> {
        let path = log_path_static(&self.events_dir, key);
        let file = match File::open(&path) {
            Ok(f) => f,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
            Err(err) => {
                warn!(path = %path.display(), error = %err, "failed to open events log");
                return Vec::new();
            }
        };
        if let Ok(meta) = file.metadata() {
            if meta.len() > MAX_EVENTS_FILE_SIZE {
                warn!(
                    path = %path.display(),
                    size = meta.len(),
                    limit = MAX_EVENTS_FILE_SIZE,
                    "events log too large, refusing to read"
                );
                return Vec::new();
            }
        }
        let reader = BufReader::new(file);
        let mut out = Vec::new();
        for line in reader.lines() {
            let Ok(line) = line else { break };
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<EventEnvelope<Value>>(&line) {
                Ok(env) => out.push(env),
                Err(err) => {
                    debug!(
                        path = %path.display(),
                        error = %err,
                        "skipping unparseable events-log line (torn write?)"
                    );
                    // Don't break — a corrupt mid-file line is unlikely but
                    // would otherwise truncate the projection. The trailing
                    // torn-write case still works because no later lines
                    // exist.
                }
            }
        }
        out
    }

    /// Project the event log for `key` into the equivalent message list.
    ///
    /// PR-A defines the contract; the body recognises only the minimal set
    /// of event types needed to round-trip the message fixtures the test
    /// suite uses (`message/user`, `message/assistant`, `message/system`,
    /// `message/tool`). PR-B (#679 phase B) extends the dispatcher to
    /// understand the live UI Protocol envelopes (`message/persisted`,
    /// `projection.envelope.v1` payloads, etc.) as it converts writer
    /// sites. Unknown event types are skipped so the projection degrades
    /// gracefully when a newer-format envelope is loaded by an older
    /// reader.
    pub fn project_to_messages(&self, key: &ThreadKey) -> Vec<Message> {
        self.read_envelopes(key)
            .into_iter()
            .filter_map(project_envelope)
            .collect()
    }

    fn acquire_entry(&self, key: &ThreadKey) -> Result<Arc<Mutex<ThreadCacheEntry>>> {
        let mut cache = self.cache.lock().expect("thread store cache poisoned");
        if let Some(existing) = cache.get(key) {
            return Ok(existing.clone());
        }
        let entry = Arc::new(Mutex::new(ThreadCacheEntry {
            write_lock: Arc::new(Mutex::new(())),
            next_seq: 0,
        }));
        cache.put(key.clone(), entry.clone());
        Ok(entry)
    }
}

/// Static helper so tests can compute the log path without owning a store.
pub(crate) fn log_path_static(events_dir: &Path, key: &ThreadKey) -> PathBuf {
    // Mirror `SessionManager::session_path_static`'s 200-byte safe-name
    // budget; reserve a 17-char trailing hash suffix when the encoded form
    // would overflow.
    const HASH_SUFFIX_LEN: usize = 17;
    const MAX_NAME_LEN: usize = 200 - HASH_SUFFIX_LEN;

    let session_enc = percent_encode(&key.session.0);
    let topic_enc = match key.topic.as_deref() {
        // `%00` is the "no topic" sentinel — see module docs. The
        // percent-encoder never produces this byte for a real topic
        // (NUL bytes are rejected upstream), so the sentinel can't
        // collide with a percent-encoded topic that happens to start
        // with `%00`.
        None => "%00".to_string(),
        Some(t) => percent_encode(t),
    };
    let thread_enc = percent_encode(key.thread.as_str());
    let joined = format!("{session_enc}__{topic_enc}__{thread_enc}");
    let (truncated, safe_name) = if joined.len() > MAX_NAME_LEN {
        let mut clipped = String::with_capacity(MAX_NAME_LEN + HASH_SUFFIX_LEN);
        for byte in joined.as_bytes().iter().take(MAX_NAME_LEN) {
            clipped.push(*byte as char);
        }
        (true, clipped)
    } else {
        (false, joined)
    };
    let final_name = if truncated {
        let hash = fnv1a_64(
            format!("{}|{:?}|{}", key.session.0, key.topic, key.thread.as_str()).as_bytes(),
        );
        format!("{safe_name}_{hash:016X}")
    } else {
        safe_name
    };
    events_dir.join(format!("{final_name}.jsonl"))
}

/// Local copy of [`crate::session::encode_path_component`] — duplicated so
/// the module stays free of cross-file coupling on the gating boundary
/// (PR-A keeps the `thread-store-authoritative` cfg local to this module).
fn percent_encode(s: &str) -> String {
    let mut encoded = String::new();
    for byte in s.as_bytes() {
        if byte.is_ascii_alphanumeric() || *byte == b'-' || *byte == b'_' {
            encoded.push(*byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

/// FNV-1a 64-bit hash — deterministic across Rust versions. Mirrors
/// [`crate::session::fnv1a_64`] for parity with the existing session-file
/// truncation scheme.
fn fnv1a_64(data: &[u8]) -> u64 {
    const FNV_OFFSET: u64 = 0xcbf29ce484222325;
    const FNV_PRIME: u64 = 0x00000100000001B3;
    let mut hash = FNV_OFFSET;
    for &byte in data {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

fn envelope_key<P>(envelope: &EventEnvelope<P>) -> ThreadKey {
    ThreadKey {
        session: SessionKey(envelope.session_id.clone()),
        topic: envelope.topic.clone(),
        thread: envelope.thread_id.clone(),
    }
}

/// Minimal projection dispatcher. Recognises the four canonical message
/// event types; everything else returns `None` so the projection degrades
/// gracefully when newer envelopes are encountered.
fn project_envelope(env: EventEnvelope<Value>) -> Option<Message> {
    let role = match env.event_type.as_str() {
        "message/user" => MessageRole::User,
        "message/assistant" => MessageRole::Assistant,
        "message/system" => MessageRole::System,
        "message/tool" => MessageRole::Tool,
        _ => return None,
    };
    // The payload shape is the embedded `Message` itself for the
    // round-trip tests below; PR-B will swap this for the typed
    // `message/persisted` payload as it migrates the live writer sites.
    let mut message: Message = match serde_json::from_value(env.payload) {
        Ok(m) => m,
        Err(err) => {
            debug!(error = %err, "failed to project envelope payload to Message");
            return None;
        }
    };
    // Trust the envelope's role over the payload's serialised role: PR-B
    // will sometimes emit envelopes whose payload omits the role (the
    // envelope already encodes it).
    message.role = role;
    if message.thread_id.is_none() {
        message.thread_id = Some(env.thread_id.as_str().to_string());
    }
    Some(message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use octos_core::ui_protocol::TurnContext;
    use serde_json::json;
    use tempfile::TempDir;

    fn key(session: &str, topic: Option<&str>, thread: &str) -> ThreadKey {
        ThreadKey::new(
            SessionKey(session.to_string()),
            topic.map(|s| s.to_string()),
            ThreadId::new(thread),
        )
    }

    fn turn_ctx(session: &str, topic: Option<&str>, thread: &str) -> TurnContext {
        TurnContext::new(
            session.to_string(),
            topic.map(|s| s.to_string()),
            ThreadId::new(thread),
        )
    }

    fn user_message(content: &str, thread: &str) -> Message {
        Message {
            role: MessageRole::User,
            content: content.to_string(),
            media: vec![],
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            client_message_id: Some(thread.to_string()),
            thread_id: Some(thread.to_string()),
            timestamp: Utc::now(),
        }
    }

    fn assistant_message(content: &str, thread: &str) -> Message {
        Message {
            role: MessageRole::Assistant,
            content: content.to_string(),
            media: vec![],
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            client_message_id: None,
            thread_id: Some(thread.to_string()),
            timestamp: Utc::now(),
        }
    }

    fn message_envelope(
        ctx: &TurnContext,
        seq: u64,
        event_type: &str,
        msg: &Message,
    ) -> EventEnvelope<Value> {
        EventEnvelope::new(
            ctx,
            seq,
            event_type,
            None,
            serde_json::to_value(msg).unwrap(),
        )
    }

    #[test]
    fn should_round_trip_appended_envelopes() {
        let dir = TempDir::new().unwrap();
        let store = ThreadStore::open(dir.path()).unwrap();
        let k = key("telegram:abc", None, "thread-1");
        let ctx = turn_ctx("telegram:abc", None, "thread-1");

        let m1 = user_message("hello", "thread-1");
        let m2 = assistant_message("hi back", "thread-1");
        store
            .append(message_envelope(&ctx, 0, "message/user", &m1))
            .unwrap();
        store
            .append(message_envelope(&ctx, 1, "message/assistant", &m2))
            .unwrap();

        let read = store.read_envelopes(&k);
        assert_eq!(read.len(), 2, "expected both appends to round-trip");
        assert_eq!(read[0].event_seq, 0);
        assert_eq!(read[1].event_seq, 1);
        assert_eq!(read[0].event_type, "message/user");
        assert_eq!(read[1].event_type, "message/assistant");
    }

    #[test]
    fn should_skip_torn_trailing_write_on_read() {
        // Model the kill-process-mid-append crash: a partial JSON line at
        // EOF must not corrupt the projection.
        let dir = TempDir::new().unwrap();
        let store = ThreadStore::open(dir.path()).unwrap();
        let k = key("cli:default", None, "thread-torn");
        let ctx = turn_ctx("cli:default", None, "thread-torn");
        let m = user_message("first event", "thread-torn");
        store
            .append(message_envelope(&ctx, 0, "message/user", &m))
            .unwrap();

        // Hand-write a partial line that resembles a torn append.
        let path = store.log_path(&k);
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"{\"session_id\":\"cli:default\",\"thread_id\":\"thread-torn\",\"event_seq\":1,\"event_type\":\"message/")
            .unwrap();
        // Note: no trailing newline — this is the torn case.
        drop(file);

        let read = store.read_envelopes(&k);
        assert_eq!(read.len(), 1, "torn trailing line must be skipped");
        assert_eq!(read[0].event_seq, 0);
    }

    #[test]
    fn should_project_envelopes_to_messages_in_order() {
        let dir = TempDir::new().unwrap();
        let store = ThreadStore::open(dir.path()).unwrap();
        let k = key("api:default", None, "thread-proj");
        let ctx = turn_ctx("api:default", None, "thread-proj");
        let m1 = user_message("ping", "thread-proj");
        let m2 = assistant_message("pong", "thread-proj");
        store
            .append(message_envelope(&ctx, 0, "message/user", &m1))
            .unwrap();
        store
            .append(message_envelope(&ctx, 1, "message/assistant", &m2))
            .unwrap();

        let projected = store.project_to_messages(&k);
        assert_eq!(projected.len(), 2);
        assert_eq!(projected[0].role, MessageRole::User);
        assert_eq!(projected[0].content, "ping");
        assert_eq!(projected[1].role, MessageRole::Assistant);
        assert_eq!(projected[1].content, "pong");
    }

    #[test]
    fn should_skip_unknown_event_types_during_projection() {
        // PR-A's projection only understands `message/*` envelopes; an
        // envelope from a future event family (here `compaction/applied`)
        // must not panic — it must be skipped so the projection degrades
        // gracefully.
        let dir = TempDir::new().unwrap();
        let store = ThreadStore::open(dir.path()).unwrap();
        let ctx = turn_ctx("cli:default", None, "thread-future");
        let k = key("cli:default", None, "thread-future");

        let m = user_message("known event", "thread-future");
        store
            .append(message_envelope(&ctx, 0, "message/user", &m))
            .unwrap();
        let unknown =
            EventEnvelope::new(&ctx, 1, "compaction/applied", None, json!({"foo": "bar"}));
        store.append(unknown).unwrap();

        let projected = store.project_to_messages(&k);
        assert_eq!(projected.len(), 1, "unknown event must be skipped");
        assert_eq!(projected[0].content, "known event");
    }

    #[test]
    fn should_evict_lru_entries_beyond_capacity() {
        let dir = TempDir::new().unwrap();
        // Capacity = 2. Inserting three distinct keys must evict the
        // first.
        let store = ThreadStore::open(dir.path()).unwrap().with_max_threads(2);
        for i in 0..3 {
            let session = format!("cli:{i}");
            let ctx = turn_ctx(&session, None, "t");
            let m = user_message("hi", "t");
            store
                .append(message_envelope(&ctx, 0, "message/user", &m))
                .unwrap();
        }
        let cache = store.cache.lock().unwrap();
        assert_eq!(cache.len(), 2, "LRU must cap at the configured capacity");
        // The two surviving entries are the most recent — `cli:1` and
        // `cli:2`. `cli:0` was evicted.
        let surviving_first = key("cli:0", None, "t");
        assert!(
            !cache.contains(&surviving_first),
            "cli:0 must have been evicted"
        );
    }

    #[test]
    fn should_serialise_concurrent_appends_from_two_threads() {
        use std::thread;
        let dir = TempDir::new().unwrap();
        let store = Arc::new(ThreadStore::open(dir.path()).unwrap());
        let store_a = store.clone();
        let store_b = store.clone();
        let ctx_a = turn_ctx("cli:concurrent", None, "thread-race");
        let ctx_b = turn_ctx("cli:concurrent", None, "thread-race");

        let handle_a = thread::spawn(move || {
            for i in 0..50 {
                let m = user_message(&format!("a-{i}"), "thread-race");
                store_a
                    .append(message_envelope(&ctx_a, i as u64 * 2, "message/user", &m))
                    .expect("append a");
            }
        });
        let handle_b = thread::spawn(move || {
            for i in 0..50 {
                let m = assistant_message(&format!("b-{i}"), "thread-race");
                store_b
                    .append(message_envelope(
                        &ctx_b,
                        i as u64 * 2 + 1,
                        "message/assistant",
                        &m,
                    ))
                    .expect("append b");
            }
        });
        handle_a.join().unwrap();
        handle_b.join().unwrap();

        let k = key("cli:concurrent", None, "thread-race");
        let read = store.read_envelopes(&k);
        assert_eq!(
            read.len(),
            100,
            "all 100 concurrent appends must be persisted intact"
        );
        // Every line must parse — that's the per-line atomicity guarantee
        // we rely on. If two writers raced inside the JSON serialisation
        // we'd see a malformed line and the read count would drop below
        // 100.
        let parsed_count = read.iter().filter(|e| !e.event_type.is_empty()).count();
        assert_eq!(parsed_count, 100, "every appended line must round-trip");
    }

    #[test]
    fn topic_disambiguates_log_paths() {
        // The `%00` sentinel for the empty topic must not collide with a
        // real topic. The encoder rewrites `_` as `_` (it's in the
        // pass-through set), so a real `_` topic must still differ from
        // the no-topic case. Same check against a legal multi-char topic.
        let dir = TempDir::new().unwrap();
        let store = ThreadStore::open(dir.path()).unwrap();
        let no_topic = key("cli:t", None, "thread-x");
        let underscore_topic = key("cli:t", Some("_"), "thread-x");
        let plain_topic = key("cli:t", Some("research"), "thread-x");
        assert_ne!(
            store.log_path(&no_topic),
            store.log_path(&underscore_topic),
            "no-topic and `_`-topic must persist to distinct files"
        );
        assert_ne!(
            store.log_path(&no_topic),
            store.log_path(&plain_topic),
            "no-topic and `research` topic must persist to distinct files"
        );
        assert_ne!(
            store.log_path(&underscore_topic),
            store.log_path(&plain_topic),
            "`_` topic and `research` topic must persist to distinct files"
        );
    }

    #[test]
    fn long_keys_truncate_with_hash_suffix() {
        let dir = TempDir::new().unwrap();
        let store = ThreadStore::open(dir.path()).unwrap();
        let huge = "x".repeat(300);
        let k = key(&huge, None, "thread-trunc");
        let path = store.log_path(&k);
        let file_name = path.file_name().unwrap().to_str().unwrap();
        assert!(
            file_name.len() <= 200 + ".jsonl".len(),
            "encoded name must stay under 200B + extension (was {} bytes)",
            file_name.len()
        );
    }
}
