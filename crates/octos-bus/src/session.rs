//! Session management with JSONL persistence and LRU eviction.

use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use chrono::{DateTime, Utc};
use eyre::Result;
use lru::LruCache;
use metrics::counter;
use octos_core::{Message, MessageRole, SessionKey};
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

/// Current schema version for session JSONL files.
const CURRENT_SESSION_SCHEMA: u32 = 1;

/// Per-process counter for unique rewrite-temp-file names.
///
/// Two writers racing the same session file (e.g. fanout children of one
/// parent terminating in the same millisecond, both calling
/// `parent.upsert_child_contract → parent.rewrite()`) used to share a
/// single `<file>.jsonl.tmp` path. They'd both `File::create` it (the
/// second truncating the first), and only one `rename` would succeed —
/// the loser saw `ENOENT` and returned an error. In the spawn lifecycle
/// this manifested as the unlucky child being marked `Orphaned` instead
/// of `Joined` despite both terminal states being `Completed`.
static REWRITE_TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Build a unique temp-file path for atomic rewrite of a session JSONL.
///
/// PID + monotonic counter make the suffix collision-free across:
/// - Concurrent rewrites of the same parent file from different tokio tasks
///   (counter ticks)
/// - Concurrent rewrites from different processes sharing a data dir
///   (PID disambiguates)
fn rewrite_tmp_path(target: &Path) -> PathBuf {
    let pid = std::process::id();
    let seq = REWRITE_TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    target.with_extension(format!("jsonl.{pid}-{seq}.tmp"))
}

/// FNV-1a 64-bit hash — deterministic across Rust versions (unlike DefaultHasher).
/// Used for session filename suffixes on truncated keys.
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

/// Encode a string for safe use as a directory/file name component.
/// Alphanumerics, `-`, `_` pass through; everything else is percent-encoded.
pub fn encode_path_component(s: &str) -> String {
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

/// Derive a stable child session key from a parent session key and a child id.
///
/// The child id is percent-encoded so it remains safe for filenames and
/// ledger metadata.
pub fn child_session_key(parent: &SessionKey, child_id: &str) -> SessionKey {
    let child_id = encode_path_component(child_id);
    SessionKey(format!("{}#child-{child_id}", parent.0))
}

fn default_session_schema() -> u32 {
    CURRENT_SESSION_SCHEMA
}

/// Derive a 50-char display title from a user message's text content.
///
/// Trims whitespace, strips JSON content-array wrappers if present, and
/// truncates to 50 Unicode characters at a UTF-8 boundary so the result
/// is safe to persist and round-trip through serde.
fn derive_title_from_message(content: &str) -> String {
    let plain = content.trim();
    // Many UI clients send `[{"type":"text","text":"..."}]`-shaped content;
    // unwrap to the inner text part if so. Plain strings pass through.
    let text = serde_json::from_str::<Vec<serde_json::Value>>(plain)
        .ok()
        .and_then(|parts| {
            parts
                .into_iter()
                .find_map(|p| p.get("text").and_then(|t| t.as_str()).map(String::from))
        })
        .unwrap_or_else(|| plain.to_string());
    let trimmed = text.trim();
    trimmed
        .chars()
        .take(50)
        .collect::<String>()
        .trim()
        .to_string()
}

fn record_session_persist(outcome: &'static str) {
    counter!(
        "octos_session_persist_total",
        "outcome" => outcome.to_string()
    )
    .increment(1);
}

fn record_session_rewrite(outcome: &'static str) {
    counter!(
        "octos_session_rewrite_total",
        "outcome" => outcome.to_string()
    )
    .increment(1);
}

fn record_child_session_fork(outcome: &'static str) {
    counter!(
        "octos_child_session_lifecycle_total",
        "kind" => "fork".to_string(),
        "outcome" => outcome.to_string()
    )
    .increment(1);
}

/// Structured terminal outcome for a child session.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ChildSessionTerminalState {
    Completed,
    RetryableFailure,
    TerminalFailure,
}

/// Whether the child session terminal contract was joined back to a parent.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ChildSessionJoinState {
    Joined,
    Orphaned,
}

/// Explicit failure policy for terminal child-session outcomes.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ChildSessionFailureAction {
    Retry,
    Escalate,
}

/// Durable child-session contract persisted alongside the session history.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChildSessionContract {
    pub task_id: String,
    pub task_label: String,
    pub parent_session_key: String,
    pub child_session_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_phase: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_state: Option<ChildSessionTerminalState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub join_state: Option<ChildSessionJoinState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub joined_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_action: Option<ChildSessionFailureAction>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub output_files: Vec<String>,
}

fn merge_child_contracts(
    flat: Vec<ChildSessionContract>,
    per_user: Vec<ChildSessionContract>,
) -> Vec<ChildSessionContract> {
    let mut merged = flat;
    for contract in per_user {
        if let Some(existing) = merged
            .iter_mut()
            .find(|existing| existing.task_id == contract.task_id)
        {
            Session::merge_child_contract(contract, existing);
        } else {
            merged.push(contract);
        }
    }
    merged
}

/// Metadata stored as the first line of each JSONL session file.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SessionMeta {
    /// Schema version for forward-compatible deserialization.
    #[serde(default = "default_session_schema")]
    schema_version: u32,
    session_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    parent_key: Option<String>,
    /// Topic name for multi-session support (e.g. "research", "code").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    topic: Option<String>,
    /// Short summary of the session (first user message, truncated).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    summary: Option<String>,
    /// Display title for sidebar/listings. Auto-derived from first user
    /// message; preserved if set manually via [`SessionManager::update_title`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    /// Whether `title` was set manually (preserved across auto-derivation).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    title_manual: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    child_contracts: Vec<ChildSessionContract>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

/// A conversation session with message history.
#[derive(Debug, Clone)]
pub struct Session {
    pub key: SessionKey,
    /// Parent session key if this session was forked.
    pub parent_key: Option<SessionKey>,
    /// Topic name for multi-session support.
    pub topic: Option<String>,
    /// Short summary of the session content.
    pub summary: Option<String>,
    /// Display title (auto-derived from first user message; manual rename via
    /// [`SessionManager::update_title`] preserves across new messages).
    pub title: Option<String>,
    /// True if title was set manually and should not be overwritten by
    /// auto-derivation.
    pub title_manual: bool,
    /// Durable child-session contracts associated with this session.
    pub child_contracts: Vec<ChildSessionContract>,
    pub messages: Vec<Message>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Session {
    fn new(key: SessionKey) -> Self {
        let now = Utc::now();
        let topic = key.topic().map(|t| t.to_string());
        Self {
            key,
            parent_key: None,
            topic,
            summary: None,
            title: None,
            title_manual: false,
            child_contracts: vec![],
            messages: vec![],
            created_at: now,
            updated_at: now,
        }
    }

    /// Get the most recent N messages from history.
    pub fn get_history(&self, max: usize) -> &[Message] {
        let len = self.messages.len();
        if len <= max {
            &self.messages
        } else {
            &self.messages[len - max..]
        }
    }

    /// Sort messages by timestamp. Used after concurrent writes (speculative
    /// overflow) to restore chronological order. Stable sort preserves
    /// insertion order for messages with identical timestamps.
    pub fn sort_by_timestamp(&mut self) {
        self.messages.sort_by_key(|m| m.timestamp);
    }

    fn merge_child_contract(update: ChildSessionContract, existing: &mut ChildSessionContract) {
        existing.task_label = update.task_label;
        existing.parent_session_key = update.parent_session_key;
        existing.child_session_key = update.child_session_key;
        if update.workflow_kind.is_some() {
            existing.workflow_kind = update.workflow_kind;
        }
        if update.current_phase.is_some() {
            existing.current_phase = update.current_phase;
        }
        if update.terminal_state.is_some() {
            existing.terminal_state = update.terminal_state;
        }
        if update.join_state.is_some() {
            existing.join_state = update.join_state;
        }
        if update.joined_at.is_some() {
            existing.joined_at = update.joined_at;
        }
        if update.failure_action.is_some() {
            existing.failure_action = update.failure_action;
        }
        if update.error.is_some() {
            existing.error = update.error;
        }
        if !update.output_files.is_empty() {
            existing.output_files = update.output_files;
        }
    }

    /// Insert or update a durable child-session contract.
    pub fn upsert_child_contract(&mut self, contract: ChildSessionContract) -> bool {
        if let Some(existing) = self
            .child_contracts
            .iter_mut()
            .find(|existing| existing.task_id == contract.task_id)
        {
            Self::merge_child_contract(contract, existing);
            true
        } else {
            self.child_contracts.push(contract);
            false
        }
    }
}

/// Default maximum number of sessions kept in memory.
const DEFAULT_MAX_SESSIONS: usize = 1000;

/// Maximum session file size we'll load (10 MB). Prevents OOM on corrupted/adversarial files.
const MAX_SESSION_FILE_SIZE: u64 = 10 * 1024 * 1024;

/// Manages sessions with in-memory LRU cache and JSONL disk persistence.
///
/// Uses `lru::LruCache` for O(1) get/put with automatic eviction of the
/// least-recently-used session when capacity is exceeded. Evicted sessions
/// remain on disk and are lazy-loaded on next access.
pub struct SessionManager {
    sessions_dir: PathBuf,
    cache: LruCache<String, Session>,
}

impl SessionManager {
    pub fn open(data_dir: &Path) -> Result<Self> {
        let sessions_dir = data_dir.join("sessions");
        std::fs::create_dir_all(&sessions_dir)?;
        Ok(Self {
            sessions_dir,
            cache: LruCache::new(NonZeroUsize::new(DEFAULT_MAX_SESSIONS).expect("default > 0")),
        })
    }

    /// Set the maximum number of sessions to keep in memory (minimum 1).
    /// Sessions evicted from memory are NOT deleted from disk.
    pub fn with_max_sessions(mut self, max: usize) -> Self {
        let cap = NonZeroUsize::new(max.max(1)).expect("clamped to >= 1");
        self.cache.resize(cap);
        self
    }

    /// List all sessions (ID + message count) from disk, including internal
    /// runtime topics (`child-*`, `*.tasks`).
    ///
    /// Scans both the legacy flat layout (`sessions/*.jsonl`) and the per-user
    /// layout (`users/{base_key}/sessions/{topic}.jsonl`).
    /// Counts lines efficiently using `BufRead` to avoid loading entire files.
    ///
    /// Use [`Self::list_top_level_sessions`] for the user-facing listing path:
    /// the all-inclusive walk is O(N) over every JSONL on disk and becomes a
    /// hard bottleneck once spawn-fanout sessions accumulate (one user dir
    /// observed in the wild had 65k+ `child-*.jsonl` siblings, each
    /// line-counted by [`Self::count_lines`], hanging `/api/sessions` for
    /// 30 s+).
    pub fn list_sessions(&self) -> Vec<(String, usize)> {
        self.list_sessions_inner(false)
    }

    /// List only top-level sessions — those whose topic is empty (the
    /// canonical `default.jsonl` per user dir) or a user-facing topic such
    /// as `research`. Internal runtime topics (`child-*` spawn fanouts and
    /// `*.tasks` background-task ledgers) are skipped at the directory walk,
    /// before any line counting, so the cost stays O(top-level sessions)
    /// regardless of how many child sessions a parent has accumulated.
    ///
    /// This is the helper that should back the user-facing
    /// `GET /api/sessions` path. Child sessions are surfaced only when an
    /// individual session's history is explicitly opened via
    /// `/api/sessions/{id}/messages`.
    pub fn list_top_level_sessions(&self) -> Vec<(String, usize)> {
        self.list_sessions_inner(true)
    }

    /// Like [`Self::list_top_level_sessions`] but also returns the persisted
    /// title for each session (None when the file has no `title` field, e.g.
    /// pre-#617 sessions).
    pub fn list_top_level_sessions_with_title(&self) -> Vec<(String, usize, Option<String>)> {
        self.list_sessions_inner_with_title(true)
    }

    fn list_sessions_inner_with_title(
        &self,
        skip_internal_topics: bool,
    ) -> Vec<(String, usize, Option<String>)> {
        // Reuse the path discovery from list_sessions_inner, but read each
        // file's first line to extract the title alongside the line count.
        let mut seen = std::collections::HashSet::new();
        let mut result = Vec::new();

        let push_with_title =
            |path: &Path,
             session_key: String,
             seen: &mut std::collections::HashSet<String>,
             out: &mut Vec<(String, usize, Option<String>)>| {
                if seen.contains(&session_key) {
                    return;
                }
                // Read just the first line for metadata; fall back to count_lines
                // for the message count.
                let title = std::fs::read_to_string(path).ok().and_then(|content| {
                    content
                        .lines()
                        .next()
                        .and_then(|first| serde_json::from_str::<SessionMeta>(first).ok())
                        .and_then(|meta| meta.title)
                });
                let count = Self::count_lines(path);
                seen.insert(session_key.clone());
                out.push((session_key, count, title));
            };

        if let Ok(entries) = std::fs::read_dir(&self.sessions_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                    continue;
                }
                let Some(name) = path.file_stem().and_then(|n| n.to_str()) else {
                    continue;
                };
                let decoded = Self::decode_filename(name);
                if skip_internal_topics && Self::is_internal_session_key(&decoded) {
                    continue;
                }
                push_with_title(&path, decoded, &mut seen, &mut result);
            }
        }

        let users_dir = self
            .sessions_dir
            .parent()
            .unwrap_or(&self.sessions_dir)
            .join("users");
        if let Ok(user_entries) = std::fs::read_dir(&users_dir) {
            for user_entry in user_entries.flatten() {
                let user_path = user_entry.path();
                if !user_path.is_dir() {
                    continue;
                }
                let base_key_encoded = match user_path.file_name().and_then(|n| n.to_str()) {
                    Some(n) => n,
                    None => continue,
                };
                let base_key = Self::decode_filename(base_key_encoded);
                let sessions_subdir = user_path.join("sessions");
                if let Ok(session_files) = std::fs::read_dir(&sessions_subdir) {
                    for file_entry in session_files.flatten() {
                        let file_path = file_entry.path();
                        if file_path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                            continue;
                        }
                        let topic_encoded = match file_path.file_stem().and_then(|n| n.to_str()) {
                            Some(n) => n,
                            None => continue,
                        };
                        let topic = Self::decode_filename(topic_encoded);
                        if skip_internal_topics && Self::is_internal_session_topic(&topic) {
                            continue;
                        }
                        let session_key = if topic == "default" {
                            base_key.clone()
                        } else {
                            format!("{base_key}#{topic}")
                        };
                        push_with_title(&file_path, session_key, &mut seen, &mut result);
                    }
                }
            }
        }

        result
    }

    fn list_sessions_inner(&self, skip_internal_topics: bool) -> Vec<(String, usize)> {
        let mut seen = std::collections::HashSet::new();
        let mut result = Vec::new();

        // 1. Legacy flat layout: data/sessions/*.jsonl
        if let Ok(entries) = std::fs::read_dir(&self.sessions_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                    if let Some(name) = path.file_stem().and_then(|n| n.to_str()) {
                        let decoded = Self::decode_filename(name);
                        if skip_internal_topics && Self::is_internal_session_key(&decoded) {
                            continue;
                        }
                        let count = Self::count_lines(&path);
                        if seen.insert(decoded.clone()) {
                            result.push((decoded, count));
                        }
                    }
                }
            }
        }

        // 2. Per-user layout: data/users/{base_key}/sessions/{topic}.jsonl
        let users_dir = self
            .sessions_dir
            .parent()
            .unwrap_or(&self.sessions_dir)
            .join("users");
        if let Ok(user_entries) = std::fs::read_dir(&users_dir) {
            for user_entry in user_entries.flatten() {
                let user_path = user_entry.path();
                if !user_path.is_dir() {
                    continue;
                }
                let base_key_encoded = match user_path.file_name().and_then(|n| n.to_str()) {
                    Some(n) => n,
                    None => continue,
                };
                let base_key = Self::decode_filename(base_key_encoded);
                let sessions_subdir = user_path.join("sessions");
                if let Ok(session_files) = std::fs::read_dir(&sessions_subdir) {
                    for file_entry in session_files.flatten() {
                        let file_path = file_entry.path();
                        if file_path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                            continue;
                        }
                        let topic_encoded = match file_path.file_stem().and_then(|n| n.to_str()) {
                            Some(n) => n,
                            None => continue,
                        };
                        let topic = Self::decode_filename(topic_encoded);
                        if skip_internal_topics && Self::is_internal_session_topic(&topic) {
                            // Skip child-* and *.tasks files BEFORE counting
                            // lines: line counting opens every file, and on
                            // user dirs with tens of thousands of spawn
                            // children the cumulative I/O blocks the
                            // /api/sessions handler for tens of seconds.
                            continue;
                        }
                        // Reconstruct the full session key
                        let session_key = if topic == "default" {
                            base_key.clone()
                        } else {
                            format!("{base_key}#{topic}")
                        };
                        let count = Self::count_lines(&file_path);
                        if seen.insert(session_key.clone()) {
                            result.push((session_key, count));
                        }
                    }
                }
            }
        }

        result
    }

    /// True for runtime-internal topics that should never appear in the
    /// user-facing session listing (`child-*` spawn fanouts and `*.tasks`
    /// background-task ledger sidecars).
    fn is_internal_session_topic(topic: &str) -> bool {
        topic.starts_with("child-") || topic == "default.tasks" || topic.ends_with(".tasks")
    }

    /// True for full session keys (legacy flat layout, encoded as
    /// `{base}#{topic}` after decoding) whose topic is internal.
    fn is_internal_session_key(decoded_key: &str) -> bool {
        decoded_key
            .split_once('#')
            .is_some_and(|(_, topic)| Self::is_internal_session_topic(topic))
    }

    /// Count lines in a JSONL session file, skipping oversized files.
    fn count_lines(path: &Path) -> usize {
        let too_large = path
            .metadata()
            .map(|m| m.len() > MAX_SESSION_FILE_SIZE)
            .unwrap_or(false);
        if too_large {
            return 0;
        }
        std::fs::File::open(path)
            .ok()
            .map(|f| {
                use std::io::BufRead;
                std::io::BufReader::new(f).lines().count()
            })
            .unwrap_or(0)
    }

    /// Load a session from disk (read-only). Returns None if not found.
    pub async fn load(&self, key: &SessionKey) -> Option<Session> {
        self.load_from_disk(key).await
    }

    /// Get or create a session. Loads from disk on first access.
    pub async fn get_or_create(&mut self, key: &SessionKey) -> &mut Session {
        let key_str = key.0.clone();
        let disk_session = if self.cache.contains(&key_str) {
            None
        } else {
            Some(
                self.load_from_disk(key)
                    .await
                    .unwrap_or_else(|| Session::new(key.clone())),
            )
        };
        self.cache.get_or_insert_mut(key_str, || {
            disk_session.unwrap_or_else(|| Session::new(key.clone()))
        })
    }

    /// Add a message to a session and persist it.
    pub async fn add_message(&mut self, key: &SessionKey, message: Message) -> Result<()> {
        self.add_message_with_seq(key, message).await.map(|_| ())
    }

    /// Add a message to a session, persist it, and return its committed sequence.
    pub async fn add_message_with_seq(
        &mut self,
        key: &SessionKey,
        message: Message,
    ) -> Result<usize> {
        // Auto-derive title from first user message before persistence so the
        // first append_to_disk includes the title in the JSONL meta line.
        // Manual titles (set via update_title) are preserved.
        if matches!(message.role, MessageRole::User) {
            let session = self.get_or_create(key).await;
            if !session.title_manual && session.title.is_none() {
                let derived = derive_title_from_message(&message.content);
                if !derived.is_empty() {
                    session.title = Some(derived);
                }
            }
        }

        let _ = self.get_or_create(key).await;
        if let Err(error) = self.append_to_disk(key, &message).await {
            record_session_persist("failed");
            return Err(error);
        }
        let session = self.get_or_create(key).await;
        session.messages.push(message);
        session.updated_at = Utc::now();
        record_session_persist("committed");
        Ok(session.messages.len().saturating_sub(1))
    }

    /// Get the JSONL file path for a session key.
    ///
    /// Uses byte-level percent-encoding for non-safe characters to ensure
    /// different keys always produce different filenames. Operating on raw
    /// UTF-8 bytes (not Unicode codepoints) makes this immune to normalization
    /// collisions on filesystems like APFS/HFS+.
    ///
    /// Truncates encoded name to 200 chars to stay within the 255-byte
    /// filesystem filename limit (reserving space for ".jsonl" suffix).
    pub fn session_path(&self, key: &SessionKey) -> PathBuf {
        Self::session_path_static(&self.sessions_dir, key)
    }

    /// Return the data directory (parent of sessions_dir).
    pub fn data_dir(&self) -> PathBuf {
        self.sessions_dir
            .parent()
            .unwrap_or(&self.sessions_dir)
            .to_path_buf()
    }

    /// Static version of `session_path` — used by `SessionHandle` too.
    pub(crate) fn session_path_static(sessions_dir: &Path, key: &SessionKey) -> PathBuf {
        // Max encoded name length: 200 chars + ".jsonl" (6) = 206, well within 255.
        // When truncation occurs, append a hash suffix to avoid collisions between
        // keys that differ only past the truncation point.
        const HASH_SUFFIX_LEN: usize = 17; // "_{hash:016X}"
        const MAX_NAME_LEN: usize = 200 - HASH_SUFFIX_LEN;
        let mut safe_name = String::new();
        let mut truncated = false;
        for byte in key.0.as_bytes() {
            if safe_name.len() >= MAX_NAME_LEN {
                truncated = true;
                break;
            }
            if byte.is_ascii_alphanumeric() || *byte == b'-' || *byte == b'_' {
                safe_name.push(*byte as char);
            } else {
                // Percent-encode each byte: ':' -> '%3A', non-ASCII -> '%XX' per byte
                safe_name.push_str(&format!("%{byte:02X}"));
            }
        }
        if truncated {
            // Append 16-char hex hash of full key to prevent collisions.
            // Uses FNV-1a (stable across Rust versions) instead of DefaultHasher
            // (which wraps SipHash and is NOT guaranteed stable across toolchain upgrades).
            let hash = fnv1a_64(key.0.as_bytes());
            safe_name.push_str(&format!("_{hash:016X}"));
        }
        sessions_dir.join(format!("{safe_name}.jsonl"))
    }

    /// Decode a percent-encoded session filename back to the original session key.
    pub fn decode_filename(encoded: &str) -> String {
        let mut bytes = Vec::new();
        let mut chars = encoded.chars();
        while let Some(c) = chars.next() {
            if c == '%' {
                let hi = chars.next().unwrap_or('0');
                let lo = chars.next().unwrap_or('0');
                if let Ok(byte) = u8::from_str_radix(&format!("{hi}{lo}"), 16) {
                    bytes.push(byte);
                } else {
                    bytes.push(b'%');
                    bytes.extend_from_slice(hi.encode_utf8(&mut [0; 4]).as_bytes());
                    bytes.extend_from_slice(lo.encode_utf8(&mut [0; 4]).as_bytes());
                }
            } else {
                bytes.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes());
            }
        }
        String::from_utf8(bytes).unwrap_or_else(|_| encoded.to_string())
    }

    /// Load a session from its JSONL file.
    ///
    /// Checks the legacy flat layout first, then the per-user directory layout.
    /// Uses spawn_blocking to avoid blocking the async runtime.
    async fn load_from_disk(&self, key: &SessionKey) -> Option<Session> {
        let flat_path = self.session_path(key);
        let base_key = key.base_key();
        let encoded_base = encode_path_component(base_key);
        let topic = key.topic().unwrap_or("default");
        let encoded_topic = encode_path_component(topic);
        let users_dir = self
            .sessions_dir
            .parent()
            .unwrap_or(&self.sessions_dir)
            .join("users");
        let per_user_path = users_dir
            .join(&encoded_base)
            .join("sessions")
            .join(format!("{encoded_topic}.jsonl"));

        if !flat_path.exists() && !per_user_path.exists() {
            return None;
        }

        let key_clone = key.clone();
        tokio::task::spawn_blocking(move || {
            fn load_session_file(path: &Path, key: &SessionKey) -> Option<Session> {
                // Guard against oversized files to prevent OOM
                if let Ok(meta) = std::fs::metadata(path) {
                    if meta.len() > MAX_SESSION_FILE_SIZE {
                        warn!(
                            key = %key,
                            path = %path.display(),
                            size = meta.len(),
                            limit = MAX_SESSION_FILE_SIZE,
                            "session file too large, skipping"
                        );
                        return None;
                    }
                }

                let content = std::fs::read_to_string(path).ok()?;
                let mut lines = content.lines();

                let meta_line = lines.next()?;
                let meta: SessionMeta = serde_json::from_str(meta_line).ok()?;

                if meta.schema_version > CURRENT_SESSION_SCHEMA {
                    warn!(
                        key = %key,
                        path = %path.display(),
                        file_version = meta.schema_version,
                        current_version = CURRENT_SESSION_SCHEMA,
                        "session file has newer schema version, skipping"
                    );
                    return None;
                }

                let messages: Vec<Message> = lines
                    .filter(|line| !line.trim().is_empty())
                    .filter_map(|line| serde_json::from_str(line).ok())
                    .collect();

                Some(Session {
                    key: key.clone(),
                    parent_key: meta.parent_key.map(SessionKey),
                    topic: meta.topic,
                    summary: meta.summary,
                    title: meta.title,
                    title_manual: meta.title_manual,
                    child_contracts: meta.child_contracts,
                    messages,
                    created_at: meta.created_at,
                    updated_at: meta.updated_at,
                })
            }

            let flat = flat_path
                .exists()
                .then(|| load_session_file(&flat_path, &key_clone))
                .flatten();
            let per_user = per_user_path
                .exists()
                .then(|| load_session_file(&per_user_path, &key_clone))
                .flatten();

            let merged = match (flat, per_user) {
                (Some(flat), Some(per_user)) => {
                    let mut merged_messages = Vec::with_capacity(
                        flat.messages.len().saturating_add(per_user.messages.len()),
                    );
                    let mut seen = std::collections::HashSet::new();

                    for message in flat.messages.into_iter().chain(per_user.messages) {
                        let Ok(fingerprint) = serde_json::to_string(&message) else {
                            continue;
                        };
                        if seen.insert(fingerprint) {
                            merged_messages.push(message);
                        }
                    }
                    merged_messages.sort_by_key(|message| message.timestamp);

                    Session {
                        key: key_clone.clone(),
                        parent_key: per_user.parent_key.or(flat.parent_key),
                        topic: per_user.topic.or(flat.topic),
                        summary: per_user.summary.or(flat.summary),
                        title: per_user.title.or(flat.title),
                        title_manual: per_user.title_manual || flat.title_manual,
                        child_contracts: merge_child_contracts(
                            flat.child_contracts,
                            per_user.child_contracts,
                        ),
                        messages: merged_messages,
                        created_at: flat.created_at.min(per_user.created_at),
                        updated_at: flat.updated_at.max(per_user.updated_at),
                    }
                }
                (Some(session), None) | (None, Some(session)) => session,
                (None, None) => return None,
            };

            debug!(
                key = %key_clone,
                messages = merged.messages.len(),
                flat_exists = flat_path.exists(),
                per_user_exists = per_user_path.exists(),
                "Loaded session from disk"
            );

            Some(merged)
        })
        .await
        .ok()
        .flatten()
    }

    /// Append a message to the JSONL file. Creates the file with metadata if new.
    /// Uses spawn_blocking to avoid blocking the async runtime.
    async fn append_to_disk(&self, key: &SessionKey, message: &Message) -> Result<()> {
        let path = self.session_path(key);

        // Prepare metadata outside spawn_blocking (needs cache access)
        let session_peek = self.cache.peek(&key.0);
        let parent_key = session_peek.and_then(|s| s.parent_key.as_ref().map(|k| k.0.clone()));
        let topic = session_peek.and_then(|s| s.topic.clone());
        let summary = session_peek.and_then(|s| s.summary.clone());
        let title = session_peek.and_then(|s| s.title.clone());
        let title_manual = session_peek.map(|s| s.title_manual).unwrap_or(false);
        let child_contracts = session_peek
            .map(|session| session.child_contracts.clone())
            .unwrap_or_default();
        let key_str = key.0.clone();
        let msg_json = serde_json::to_string(message)?;

        tokio::task::spawn_blocking(move || {
            use std::io::Write;

            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)?;

            // Check file size after open to avoid TOCTOU race with exists() check
            let file_len = file.metadata()?.len();
            let is_new = file_len == 0;

            // Refuse to append if the file is already at the size limit.
            // The session should be compacted before it reaches this point.
            if !is_new && file_len >= MAX_SESSION_FILE_SIZE {
                warn!(
                    key = key_str,
                    size = file_len,
                    limit = MAX_SESSION_FILE_SIZE,
                    "session file at size limit, skipping append"
                );
                return Ok(());
            }

            if is_new {
                let meta = SessionMeta {
                    schema_version: CURRENT_SESSION_SCHEMA,
                    session_key: key_str,
                    parent_key,
                    topic,
                    summary,
                    title,
                    title_manual,
                    child_contracts,
                    created_at: Utc::now(),
                    updated_at: Utc::now(),
                };
                writeln!(file, "{}", serde_json::to_string(&meta)?)?;
            }

            writeln!(file, "{}", msg_json)?;
            Ok::<_, eyre::Report>(())
        })
        .await
        .map_err(|e| eyre::eyre!("spawn_blocking join error: {e}"))??;

        Ok(())
    }

    /// Rewrite a session's JSONL file from the in-memory state.
    /// Uses atomic write-then-rename to avoid corruption on crash.
    /// Uses spawn_blocking to avoid blocking the async runtime.
    pub async fn rewrite(&self, key: &SessionKey) -> Result<()> {
        let session = self
            .cache
            .peek(&key.0)
            .ok_or_else(|| eyre::eyre!("session not in cache: {}", key))?;

        // Build the full content string synchronously (no I/O)
        let meta = SessionMeta {
            schema_version: CURRENT_SESSION_SCHEMA,
            session_key: key.0.clone(),
            parent_key: session.parent_key.as_ref().map(|k| k.0.clone()),
            topic: session.topic.clone(),
            summary: session.summary.clone(),
            title: session.title.clone(),
            title_manual: session.title_manual,
            child_contracts: session.child_contracts.clone(),
            created_at: session.created_at,
            updated_at: session.updated_at,
        };
        let mut content = serde_json::to_string(&meta)?;
        content.push('\n');
        for msg in &session.messages {
            content.push_str(&serde_json::to_string(msg)?);
            content.push('\n');
        }

        let msg_count = session.messages.len();
        let path = self.session_path(key);
        let key_display = key.to_string();

        let rewrite_result = tokio::task::spawn_blocking(move || {
            use std::io::Write;
            let tmp_path = rewrite_tmp_path(&path);
            let mut file = std::fs::File::create(&tmp_path)?;
            file.write_all(content.as_bytes())?;
            file.flush()?;
            // Atomic rename (on same filesystem)
            std::fs::rename(&tmp_path, &path)?;
            Ok::<_, eyre::Report>(())
        })
        .await
        .map_err(|e| eyre::eyre!("spawn_blocking join error: {e}"))?;
        if let Err(error) = rewrite_result {
            record_session_rewrite("failed");
            return Err(error);
        }

        debug!(key = %key_display, messages = msg_count, "Rewrote session to disk");
        record_session_rewrite("committed");
        Ok(())
    }

    /// Fork a session: create a new session that copies the last N messages from the parent.
    ///
    /// The new session's channel is taken from the parent key; `new_chat_id` becomes the chat ID.
    /// Returns the new session's key.
    pub async fn fork(
        &mut self,
        parent_key: &SessionKey,
        new_chat_id: &str,
        copy_messages: usize,
    ) -> Result<SessionKey> {
        let parent = self.get_or_create(parent_key).await;
        let messages: Vec<Message> = parent.get_history(copy_messages).to_vec();
        // Derive channel from parent key (format: "channel:chat_id")
        let channel = parent_key.0.split(':').next().unwrap_or("cli");
        let new_key = SessionKey::new(channel, new_chat_id);

        let now = Utc::now();
        let session = Session {
            key: new_key.clone(),
            parent_key: Some(parent_key.clone()),
            topic: None,
            summary: None,
            title: None,
            title_manual: false,
            child_contracts: vec![],
            messages,
            created_at: now,
            updated_at: now,
        };
        self.cache.put(new_key.0.clone(), session);
        self.rewrite(&new_key).await?;

        debug!(
            parent = %parent_key,
            child = %new_key,
            copied = copy_messages,
            "Forked session"
        );
        Ok(new_key)
    }

    /// Clear a session's chat history (both in-memory and on disk).
    ///
    /// Removes session data from:
    /// 1. In-memory LRU cache
    /// 2. Flat layout JSONL (`sessions/{encoded_key}.jsonl`)
    /// 3. Per-user layout JSONL (`users/{encoded_base}/sessions/{topic}.jsonl`)
    ///
    /// Does NOT remove the user workspace directory — workspace data (slides,
    /// git repos, artifacts) has a separate lifecycle from chat history.
    pub async fn clear(&mut self, key: &SessionKey) -> Result<()> {
        self.cache.pop(&key.0);

        // 1. Flat layout JSONL
        let flat_path = self.session_path(key);
        if flat_path.exists() {
            tokio::fs::remove_file(&flat_path).await?;
        }

        // 2. Per-user layout JSONL
        let base_key = key.base_key();
        let encoded_base = encode_path_component(base_key);
        let users_dir = self
            .sessions_dir
            .parent()
            .unwrap_or(&self.sessions_dir)
            .join("users");
        let user_dir = users_dir.join(&encoded_base);

        if user_dir.exists() {
            let topic = key.topic().unwrap_or("default");
            let encoded_topic = encode_path_component(topic);
            let per_user_path = user_dir
                .join("sessions")
                .join(format!("{encoded_topic}.jsonl"));
            if per_user_path.exists() {
                if let Err(e) = tokio::fs::remove_file(&per_user_path).await {
                    warn!(
                        key = %key,
                        path = %per_user_path.display(),
                        error = %e,
                        "failed to delete per-user session file"
                    );
                }
            }
        }

        Ok(())
    }

    /// Number of sessions currently in memory.
    pub fn cache_len(&self) -> usize {
        self.cache.len()
    }

    /// Number of sessions the LRU cache can hold.
    pub fn capacity(&self) -> usize {
        self.cache.cap().get()
    }

    /// Delete session files that haven't been updated in `max_age` days.
    ///
    /// Returns the number of files removed. Only touches disk files;
    /// stale entries still in the LRU cache are also evicted.
    pub fn purge_stale(&mut self, max_age_days: u64) -> usize {
        let cutoff = Utc::now() - chrono::Duration::days(max_age_days as i64);
        let mut removed = 0;

        let Ok(dir) = std::fs::read_dir(&self.sessions_dir) else {
            return 0;
        };

        for entry in dir.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }

            // Read only the first line (metadata) to check updated_at
            let Ok(content) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Some(meta_line) = content.lines().next() else {
                continue;
            };
            let Ok(meta) = serde_json::from_str::<SessionMeta>(meta_line) else {
                continue;
            };

            if meta.updated_at < cutoff {
                // Evict from LRU cache if present
                self.cache.pop(&meta.session_key);
                if std::fs::remove_file(&path).is_ok() {
                    debug!(key = meta.session_key, "purged stale session");
                    removed += 1;
                }
            }
        }

        removed
    }

    /// Drop the cached in-memory copy of a session so the next read consults
    /// disk. Required by callers that write through alternate channels (e.g.
    /// `SessionHandle`) and must keep the manager's LRU cache from serving
    /// stale post-write reads.
    pub fn invalidate_cache(&mut self, key: &SessionKey) {
        self.cache.pop(&key.0);
    }

    /// Scan the per-user layout for every JSONL belonging to `base_key` and
    /// return their reconstructed `SessionKey`s. The default file maps back to
    /// the base key (no topic suffix); other files map to `{base_key}#{topic}`.
    ///
    /// Used by topic-less watcher reconnects so the replay path can union
    /// every topic-specific JSONL the actor has written under this user even
    /// when the URL didn't carry an explicit `?topic=...` parameter.
    pub fn list_user_session_keys(&self, base_key: &str) -> Vec<SessionKey> {
        let encoded_base = encode_path_component(base_key);
        let user_sessions_dir = self
            .sessions_dir
            .parent()
            .unwrap_or(&self.sessions_dir)
            .join("users")
            .join(&encoded_base)
            .join("sessions");
        let mut keys = Vec::new();
        let Ok(read_dir) = std::fs::read_dir(&user_sessions_dir) else {
            return keys;
        };
        for entry in read_dir.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|n| n.to_str()) else {
                continue;
            };
            let topic = Self::decode_filename(stem);
            let session_key = if topic == "default" {
                SessionKey(base_key.to_string())
            } else {
                SessionKey(format!("{base_key}#{topic}"))
            };
            keys.push(session_key);
        }
        keys
    }

    /// Ensure a session file exists in the per-user layout so that
    /// `list_user_sessions` can discover it.  Creates an empty JSONL
    /// (metadata-only) if the file does not already exist.
    ///
    /// `base_key` must match the value passed to `list_user_sessions`
    /// (e.g. `"_main:telegram:8516089817"` or `"telegram:8516089817"`).
    pub fn touch_user_session(&self, base_key: &str, topic: &str) {
        let encoded_base = encode_path_component(base_key);
        let user_sessions_dir = self
            .sessions_dir
            .parent()
            .unwrap_or(&self.sessions_dir)
            .join("users")
            .join(&encoded_base)
            .join("sessions");
        let _ = std::fs::create_dir_all(&user_sessions_dir);

        let effective_topic = if topic.is_empty() { "default" } else { topic };
        let encoded_topic = encode_path_component(effective_topic);
        let path = user_sessions_dir.join(format!("{encoded_topic}.jsonl"));

        if !path.exists() {
            let session_key_str = if topic.is_empty() {
                base_key.to_string()
            } else {
                format!("{base_key}#{topic}")
            };
            let meta = SessionMeta {
                schema_version: CURRENT_SESSION_SCHEMA,
                session_key: session_key_str,
                parent_key: None,
                topic: if topic.is_empty() {
                    None
                } else {
                    Some(topic.to_string())
                },
                summary: None,
                title: None,
                title_manual: false,
                child_contracts: vec![],
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
            };
            if let Ok(json) = serde_json::to_string(&meta) {
                if let Err(e) = std::fs::write(&path, format!("{json}\n")) {
                    warn!(path = %path.display(), error = %e, "failed to write session metadata");
                }
            }
        }
    }
}

// ── SessionHandle ──────────────────────────────────────────────────────────

/// Per-session file handle — owns one session's in-memory state and I/O.
///
/// Used by `SessionActor` to eliminate the shared `SessionManager` mutex.
/// Each actor gets its own `SessionHandle`, so there is zero cross-session
/// lock contention.
///
/// File layout (per-user directory structure):
/// ```text
/// {data_dir}/users/{encoded_base_key}/sessions/{topic_or_default}.jsonl
/// ```
/// This enables future filesystem-level isolation (quotas, chroot, sandboxing).
pub struct SessionHandle {
    sessions_dir: PathBuf,
    session: Session,
}

/// Per-key persist lock map.
///
/// Two writers (e.g. `SessionActor` and `ApiChannel::persist_to_session`) can
/// each open a fresh `SessionHandle` for the same session_key concurrently.
/// Each handle loads disk into its OWN per-instance `messages: Vec<_>`.
/// Without serialisation, both observe `len = N`, both append, both return
/// `seq = N` — duplicate seqs that break watcher correlation.
///
/// This map gives `persist_message_through_canonical_path` a per-key Tokio
/// mutex so all writes for the same `SessionKey.0` serialise. The mutex is
/// scoped to the session_key string (NOT the file path) so callers reaching
/// the canonical per-user JSONL via different code paths still contend on
/// the same lock.
///
/// Memory note: entries leak forever, one per active session_key. In a long-
/// lived bus process this grows with active distinct sessions; given
/// production keys are typically `<profile>:api:<chat>` and bounded by user
/// count, this is acceptable. We can add LRU eviction later if needed.
fn persist_lock_for(key: &SessionKey) -> std::sync::Arc<tokio::sync::Mutex<()>> {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    static MAP: OnceLock<Mutex<HashMap<String, std::sync::Arc<tokio::sync::Mutex<()>>>>> =
        OnceLock::new();
    let map = MAP.get_or_init(|| Mutex::new(HashMap::new()));
    let mut guard = map.lock().expect("persist lock map poisoned");
    guard
        .entry(key.0.clone())
        .or_insert_with(|| std::sync::Arc::new(tokio::sync::Mutex::new(())))
        .clone()
}

/// Persist a single message to the canonical per-user `<topic>.jsonl` file
/// the `SessionActor` and `ApiChannel` both target. Returns the committed
/// per-session sequence number.
///
/// Writes for the same `key` serialise via a per-key Tokio mutex (see
/// [`persist_lock_for`]). This is the contract that closed the concurrent-
/// persist seq race: every caller — `SessionActor::persist_assistant_message`,
/// `ApiChannel::persist_to_session`, and the standalone `octos serve` `/chat`
/// handlers — funnels through this helper so the storage layer is the single
/// ordering point. Callers that also keep an in-memory `SessionHandle` mirror
/// the message via [`SessionHandle::push_message_in_memory`] AFTER the disk
/// write commits, so their local Vec stays consistent without double-writing.
///
/// Preserves the canonical migration path (legacy flat → per-user) inside
/// `SessionHandle::open`.
pub async fn persist_message_through_canonical_path(
    data_dir: &Path,
    key: &SessionKey,
    message: Message,
) -> Result<usize> {
    let lock = persist_lock_for(key);
    let _guard = lock.lock().await;
    let mut handle = SessionHandle::open(data_dir, key);
    handle.add_message_with_seq(message).await
}

impl SessionHandle {
    /// Open or create a session handle for the given key.
    ///
    /// Uses per-user directory layout: `{data_dir}/users/{base_key}/sessions/{topic}.jsonl`.
    /// Falls back to the legacy flat layout for migration.
    pub fn open(data_dir: &Path, key: &SessionKey) -> Self {
        let base_key = key.base_key();
        let encoded_base = Self::encode_path_component(base_key);
        let user_sessions_dir = data_dir.join("users").join(&encoded_base).join("sessions");
        let _ = std::fs::create_dir_all(&user_sessions_dir);

        let topic_filename = Self::topic_filename(key);
        let new_path = user_sessions_dir.join(&topic_filename);
        let marker_path = Self::migration_marker_path(&user_sessions_dir, key);
        let legacy_dir = data_dir.join("sessions");
        let legacy_path = SessionManager::session_path_static(&legacy_dir, key);

        // Migration state machine — three real cases:
        //   (A) marker present              -> migration is done; per-user is
        //                                      authoritative. Skip legacy load
        //                                      AND skip legacy delete (a stale
        //                                      legacy file is left in place so
        //                                      operator-level cleanup can find
        //                                      it; the per-user merge in
        //                                      `list_user_sessions` would still
        //                                      dedup if it surfaces).
        //   (B) per-user + legacy + no marker
        //                                   -> previous boot's `rewrite_blocking`
        //                                      succeeded but `remove_file(legacy)`
        //                                      failed. Best-effort retry the
        //                                      removal; on success write the
        //                                      marker. On failure log and keep
        //                                      going (per-user already wins).
        //   (C) per-user only               -> normal read.
        //   (D) legacy only                 -> first-time migration: load,
        //                                      rewrite into per-user, remove
        //                                      legacy, write marker.
        //   (else)                          -> empty session.
        let session = if marker_path.exists() {
            // Case (A): marker says migration is done. The per-user file is
            // authoritative even if a stale legacy file co-exists.
            Self::load_from_file(&new_path, key)
        } else if new_path.exists() {
            if legacy_path.exists() {
                // Case (B): partial-migration leftover. Retry the legacy
                // removal so subsequent boots take the cheap (A) path.
                match std::fs::remove_file(&legacy_path) {
                    Ok(()) => {
                        let _ = std::fs::write(&marker_path, b"migrated-from-flat\n");
                    }
                    Err(error) => {
                        warn!(
                            key = %key,
                            legacy_path = %legacy_path.display(),
                            error = %error,
                            "failed to retry legacy session removal during open; \
                             per-user file remains authoritative"
                        );
                    }
                }
            }
            // Case (C): per-user only — straight read.
            Self::load_from_file(&new_path, key)
        } else if legacy_path.exists() {
            // Case (D): first-time migration. Persist into the per-user JSONL
            // BEFORE removing the legacy file so a subsequent incremental
            // `add_message_with_seq` (which only appends a single line) does
            // not silently drop the pre-migration messages.
            debug!(key = %key, "migrating session from legacy flat layout");
            let session = Self::load_from_file(&legacy_path, key);
            if let Some(loaded) = session.as_ref() {
                if let Err(error) = Self::rewrite_blocking(&new_path, loaded) {
                    warn!(
                        key = %key,
                        path = %new_path.display(),
                        error = %error,
                        "failed to materialize legacy session into per-user layout; \
                         leaving legacy file in place"
                    );
                    return Self {
                        sessions_dir: user_sessions_dir,
                        session: loaded.clone(),
                    };
                }
                if std::fs::remove_file(&legacy_path).is_ok() {
                    let _ = std::fs::write(&marker_path, b"migrated-from-flat\n");
                }
            }
            session
        } else {
            None
        }
        .unwrap_or_else(|| Session::new(key.clone()));

        Self {
            sessions_dir: user_sessions_dir,
            session,
        }
    }

    /// Path of the per-key migration marker written after a successful
    /// rewrite + legacy-remove pair. Used by [`Self::open`] to detect a
    /// completed migration on subsequent opens (so a stale legacy file —
    /// e.g. from a remove_file failure on a prior boot — does not cause
    /// double-history reads).
    fn migration_marker_path(user_sessions_dir: &Path, key: &SessionKey) -> PathBuf {
        let topic = key.topic().unwrap_or("default");
        let encoded = encode_path_component(topic);
        user_sessions_dir.join(format!(".migrated.{encoded}"))
    }

    /// Check whether a session file exists in either the per-user or legacy layout.
    pub fn session_exists(data_dir: &Path, key: &SessionKey) -> bool {
        let base_key = key.base_key();
        let encoded_base = Self::encode_path_component(base_key);
        let topic = key.topic().unwrap_or("default");
        let encoded_topic = Self::encode_path_component(topic);

        let per_user_path = data_dir
            .join("users")
            .join(&encoded_base)
            .join("sessions")
            .join(format!("{encoded_topic}.jsonl"));
        if per_user_path.exists() {
            return true;
        }

        let legacy_path = SessionManager::session_path_static(&data_dir.join("sessions"), key);
        legacy_path.exists()
    }

    /// Seed a child session from a parent session if the child does not already exist.
    ///
    /// Copies the parent's most recent `copy_messages` messages into the child
    /// when the child is empty, repairs a missing parent linkage on existing
    /// child sessions, and persists the result. Existing child history is never
    /// overwritten.
    pub async fn fork_from_parent_if_missing(
        data_dir: &Path,
        parent_key: &SessionKey,
        child_key: &SessionKey,
        copy_messages: usize,
    ) -> Result<()> {
        let parent_history = {
            let parent = Self::open(data_dir, parent_key);
            parent.get_history(copy_messages).to_vec()
        };

        let mut child = Self::open(data_dir, child_key);
        if child
            .session
            .parent_key
            .as_ref()
            .is_some_and(|existing| existing != parent_key)
        {
            record_child_session_fork("skipped_existing");
            return Ok(());
        }

        let mut changed = false;
        let mut seeded_history = false;

        if child.session.parent_key.is_none() {
            child.session.parent_key = Some(parent_key.clone());
            changed = true;
        }
        if child.session.messages.is_empty() {
            child.session.messages = parent_history;
            changed = true;
            seeded_history = true;
        }
        if !changed {
            record_child_session_fork("skipped_existing");
            return Ok(());
        }

        child.session.updated_at = Utc::now();
        child.rewrite().await?;
        record_child_session_fork(if seeded_history {
            "seeded"
        } else {
            "linked_existing"
        });
        Ok(())
    }

    /// Encode a path component (base key) for safe directory names.
    fn encode_path_component(s: &str) -> String {
        encode_path_component(s)
    }

    /// Get the JSONL filename for a session key's topic.
    /// Default session → `default.jsonl`, topic → `{topic}.jsonl`.
    fn topic_filename(key: &SessionKey) -> String {
        let topic = key.topic().unwrap_or("default");
        let encoded = Self::encode_path_component(topic);
        format!("{encoded}.jsonl")
    }

    /// The session key.
    pub fn key(&self) -> &SessionKey {
        &self.session.key
    }

    /// Immutable access to the session.
    pub fn session(&self) -> &Session {
        &self.session
    }

    /// Mutable access to the session.
    pub fn session_mut(&mut self) -> &mut Session {
        &mut self.session
    }

    /// Returns `true` when this session has a recorded parent (i.e. a
    /// background/child session forked from a top-level chat). Used by the
    /// session actor (M8.6 fix-first item 3) to distinguish top-level
    /// resume refusals (start fresh) from child resume refusals (mark task
    /// failed).
    pub fn is_child_session(&self) -> bool {
        self.session.parent_key.is_some()
    }

    /// Drop all in-memory messages without persisting. Used by the session
    /// actor (M8.6 fix-first item 3) on a top-level worktree-missing
    /// refusal: the unsafe transcript must not flow into the first LLM
    /// call. Caller is expected to follow up with a fresh
    /// [`Self::rewrite`] if it wants the empty state to survive on disk.
    pub fn clear_messages_for_unsafe_resume(&mut self) {
        self.session.messages.clear();
    }

    /// Get the most recent N messages from history.
    pub fn get_history(&self, max: usize) -> &[Message] {
        self.session.get_history(max)
    }

    /// Get or initialize the session (always returns a reference).
    pub fn get_or_create(&mut self) -> &mut Session {
        &mut self.session
    }

    /// Sanitize the loaded transcript via [`crate::ResumePolicy`] (M8.6).
    ///
    /// Runs the four filter passes described in `resume_policy`, replaces
    /// `self.session.messages` with the sanitized list, and returns the
    /// typed report so callers can log it or forward it to a harness event
    /// sink. A missing worktree is reported via
    /// [`crate::SanitizeError::WorktreeMissing`] — the session's in-memory
    /// messages are NOT mutated in that case so callers retain the
    /// original transcript for operator inspection.
    ///
    /// NOTE: this does not persist the sanitized transcript to disk. Call
    /// [`Self::rewrite`] afterward if the caller wants the sanitized
    /// version to survive a subsequent reload.
    pub fn sanitize_loaded_messages(
        &mut self,
        retry_state: Option<&dyn crate::RetryStateView>,
        workspace_root: Option<&Path>,
    ) -> Result<
        (
            crate::SessionSanitizeReport,
            Vec<crate::ReplacementStateRef>,
        ),
        crate::SanitizeError,
    > {
        // Clone so we can restore the original on the worktree-missing
        // path without a partial-move hazard.
        let messages = self.session.messages.clone();
        match crate::ResumePolicy::sanitize(messages, retry_state, workspace_root) {
            Ok(outcome) => {
                self.session.messages = outcome.messages;
                Ok((outcome.report, outcome.content_replacements))
            }
            Err(error) => {
                let crate::SanitizeError::WorktreeMissing { report, .. } = &error;
                warn!(
                    key = %self.session.key,
                    report = %report,
                    "resume sanitize refused: worktree missing"
                );
                Err(error)
            }
        }
    }

    /// Add a message to the session and persist it.
    pub async fn add_message(&mut self, message: Message) -> Result<()> {
        self.add_message_with_seq(message).await.map(|_| ())
    }

    /// Add a message to the session, persist it, and return its committed sequence.
    pub async fn add_message_with_seq(&mut self, message: Message) -> Result<usize> {
        self.session.messages.push(message.clone());
        self.session.updated_at = Utc::now();
        if let Err(error) = self.append_to_disk(&message).await {
            record_session_persist("failed");
            return Err(error);
        }
        record_session_persist("committed");
        Ok(self.session.messages.len().saturating_sub(1))
    }

    /// Append a message to the in-memory transcript only — no disk I/O.
    ///
    /// Used by callers that funneled the persist through
    /// [`persist_message_through_canonical_path`] and now need to keep the
    /// per-actor handle's in-memory `messages` consistent with disk WITHOUT
    /// double-writing (the canonical helper already wrote the JSONL line).
    pub fn push_message_in_memory(&mut self, message: Message) {
        self.session.messages.push(message);
        self.session.updated_at = Utc::now();
    }

    /// Insert or update a durable child-session contract and persist it.
    pub async fn upsert_child_contract(&mut self, contract: ChildSessionContract) -> Result<bool> {
        let existed = self.session.upsert_child_contract(contract);
        self.session.updated_at = Utc::now();
        self.rewrite().await?;
        Ok(existed)
    }

    /// Sort messages by timestamp (for speculative overflow ordering).
    pub fn sort_by_timestamp(&mut self) {
        self.session.sort_by_timestamp();
    }

    /// Rewrite the session to disk (atomic write-then-rename).
    pub async fn rewrite(&self) -> Result<()> {
        let meta = SessionMeta {
            schema_version: CURRENT_SESSION_SCHEMA,
            session_key: self.session.key.0.clone(),
            parent_key: self.session.parent_key.as_ref().map(|k| k.0.clone()),
            topic: self.session.topic.clone(),
            summary: self.session.summary.clone(),
            title: self.session.title.clone(),
            title_manual: self.session.title_manual,
            child_contracts: self.session.child_contracts.clone(),
            created_at: self.session.created_at,
            updated_at: self.session.updated_at,
        };
        let mut content = serde_json::to_string(&meta)?;
        content.push('\n');
        for msg in &self.session.messages {
            content.push_str(&serde_json::to_string(msg)?);
            content.push('\n');
        }

        let msg_count = self.session.messages.len();
        let path = self.session_path();
        let key_display = self.session.key.to_string();

        let rewrite_result = tokio::task::spawn_blocking(move || {
            use std::io::Write;
            let tmp_path = rewrite_tmp_path(&path);
            let mut file = std::fs::File::create(&tmp_path)?;
            file.write_all(content.as_bytes())?;
            file.flush()?;
            std::fs::rename(&tmp_path, &path)?;
            Ok::<_, eyre::Report>(())
        })
        .await
        .map_err(|e| eyre::eyre!("spawn_blocking join error: {e}"))?;
        if let Err(error) = rewrite_result {
            record_session_rewrite("failed");
            return Err(error);
        }

        debug!(key = %key_display, messages = msg_count, "Rewrote session to disk");
        record_session_rewrite("committed");
        Ok(())
    }

    /// Path for the append-only background task ledger sidecar.
    pub fn task_state_path(&self) -> PathBuf {
        self.session_path().with_extension("tasks.jsonl")
    }

    /// Clear the session (in-memory and on disk).
    pub async fn clear(&mut self) -> Result<()> {
        self.session = Session::new(self.session.key.clone());
        let path = self.session_path();
        if path.exists() {
            tokio::fs::remove_file(&path).await?;
        }
        Ok(())
    }

    fn session_path(&self) -> PathBuf {
        self.sessions_dir
            .join(Self::topic_filename(&self.session.key))
    }

    /// Synchronously rewrite a session JSONL at `path` from an in-memory
    /// `Session`. Used by the migration path in [`Self::open`] where the
    /// caller is not yet inside an async context. Atomic write-then-rename.
    ///
    /// Cleans up the tmp file if the write or rename fails so a partial
    /// migration does not leak `<path>.<pid>-<seq>.tmp` files on disk.
    /// Records the same `octos_session_rewrite_total` metric as the async
    /// `rewrite()` so operators see a unified rewrite count regardless of
    /// the originating call path.
    fn rewrite_blocking(path: &Path, session: &Session) -> Result<()> {
        let result = Self::rewrite_blocking_inner(path, session);
        match &result {
            Ok(()) => record_session_rewrite("committed"),
            Err(_) => record_session_rewrite("failed"),
        }
        result
    }

    fn rewrite_blocking_inner(path: &Path, session: &Session) -> Result<()> {
        use std::io::Write;
        let meta = SessionMeta {
            schema_version: CURRENT_SESSION_SCHEMA,
            session_key: session.key.0.clone(),
            parent_key: session.parent_key.as_ref().map(|k| k.0.clone()),
            topic: session.topic.clone(),
            summary: session.summary.clone(),
            title: session.title.clone(),
            title_manual: session.title_manual,
            child_contracts: session.child_contracts.clone(),
            created_at: session.created_at,
            updated_at: session.updated_at,
        };
        let mut content = serde_json::to_string(&meta)?;
        content.push('\n');
        for msg in &session.messages {
            content.push_str(&serde_json::to_string(msg)?);
            content.push('\n');
        }
        let tmp_path = rewrite_tmp_path(path);
        let write_result = (|| -> Result<()> {
            let mut file = std::fs::File::create(&tmp_path)?;
            file.write_all(content.as_bytes())?;
            file.flush()?;
            std::fs::rename(&tmp_path, path)?;
            Ok(())
        })();
        if write_result.is_err() {
            // Best-effort tmp cleanup. If the rename succeeded but a later
            // step failed (currently impossible — rename is the last step)
            // we'd skip this; if `File::create` or `write_all` fail, the
            // tmp file may exist and must not leak.
            let _ = std::fs::remove_file(&tmp_path);
        }
        write_result
    }

    /// Append a single message to the JSONL file.
    async fn append_to_disk(&self, message: &Message) -> Result<()> {
        let path = self.session_path();
        let parent_key = self.session.parent_key.as_ref().map(|k| k.0.clone());
        let topic = self.session.topic.clone();
        let summary = self.session.summary.clone();
        let title = self.session.title.clone();
        let title_manual = self.session.title_manual;
        let child_contracts = self.session.child_contracts.clone();
        let key_str = self.session.key.0.clone();
        let msg_json = serde_json::to_string(message)?;

        tokio::task::spawn_blocking(move || {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)?;

            let file_len = file.metadata()?.len();
            let is_new = file_len == 0;

            if !is_new && file_len >= MAX_SESSION_FILE_SIZE {
                warn!(
                    key = key_str,
                    size = file_len,
                    limit = MAX_SESSION_FILE_SIZE,
                    "session file at size limit, skipping append"
                );
                return Ok(());
            }

            if is_new {
                let meta = SessionMeta {
                    schema_version: CURRENT_SESSION_SCHEMA,
                    session_key: key_str,
                    parent_key,
                    topic,
                    summary,
                    title,
                    title_manual,
                    child_contracts,
                    created_at: Utc::now(),
                    updated_at: Utc::now(),
                };
                writeln!(file, "{}", serde_json::to_string(&meta)?)?;
            }

            writeln!(file, "{}", msg_json)?;
            Ok::<_, eyre::Report>(())
        })
        .await
        .map_err(|e| eyre::eyre!("spawn_blocking join error: {e}"))??;

        Ok(())
    }

    /// Load a session from a specific file path.
    fn load_from_file(path: &Path, key: &SessionKey) -> Option<Session> {
        if let Ok(meta) = std::fs::metadata(path) {
            if meta.len() > MAX_SESSION_FILE_SIZE {
                warn!(key = %key, size = meta.len(), "session file too large, skipping");
                return None;
            }
        }

        let content = std::fs::read_to_string(path).ok()?;
        let mut lines = content.lines();

        let meta_line = lines.next()?;
        let meta: SessionMeta = serde_json::from_str(meta_line).ok()?;

        if meta.schema_version > CURRENT_SESSION_SCHEMA {
            warn!(key = %key, file_version = meta.schema_version, "newer schema, skipping");
            return None;
        }

        let messages: Vec<Message> = lines
            .filter(|line| !line.trim().is_empty())
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect();

        debug!(key = %key, messages = messages.len(), "Loaded session from disk");

        Some(Session {
            key: key.clone(),
            parent_key: meta.parent_key.map(SessionKey),
            topic: meta.topic,
            summary: meta.summary,
            title: meta.title,
            title_manual: meta.title_manual,
            child_contracts: meta.child_contracts,
            messages,
            created_at: meta.created_at,
            updated_at: meta.updated_at,
        })
    }
}

/// Entry describing a session for listing purposes.
#[derive(Debug, Clone)]
pub struct SessionListEntry {
    /// Topic name (None = default session).
    pub topic: Option<String>,
    /// Number of messages in the session.
    pub message_count: usize,
    /// Last updated timestamp.
    pub updated_at: DateTime<Utc>,
    /// Short summary of the session.
    pub summary: Option<String>,
    /// Display title (derived from first user message or set manually).
    pub title: Option<String>,
}

impl SessionManager {
    /// List all sessions belonging to a specific chat (base key without topic).
    ///
    /// Scans the sessions directory for files matching the base key or base key + topic suffix.
    /// Returns entries sorted by updated_at descending (most recent first).
    pub fn list_sessions_for_chat(&self, base_key: &str) -> Vec<SessionListEntry> {
        let mut entries = Vec::new();
        let Ok(dir) = std::fs::read_dir(&self.sessions_dir) else {
            return entries;
        };

        for entry in dir.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let Some(name) = path.file_stem().and_then(|n| n.to_str()) else {
                continue;
            };

            // Skip oversized files
            if path
                .metadata()
                .map(|m| m.len() > MAX_SESSION_FILE_SIZE)
                .unwrap_or(false)
            {
                continue;
            }

            let decoded = Self::decode_filename(name);

            // Check if this session belongs to the given base key
            let session_base = decoded.split('#').next().unwrap_or(&decoded);
            if session_base != base_key {
                continue;
            }

            // Read first line (metadata) and count remaining lines
            let Ok(content) = std::fs::read_to_string(&path) else {
                continue;
            };
            let mut lines = content.lines();
            let Some(meta_line) = lines.next() else {
                continue;
            };
            let meta: SessionMeta = match serde_json::from_str(meta_line) {
                Ok(m) => m,
                Err(_) => continue,
            };

            let message_count = lines.filter(|l| !l.trim().is_empty()).count();
            let topic = decoded.split_once('#').map(|(_, t)| t.to_string());

            entries.push(SessionListEntry {
                topic,
                message_count,
                updated_at: meta.updated_at,
                summary: meta.summary,
                title: meta.title,
            });
        }

        entries.sort_by_key(|entry| std::cmp::Reverse(entry.updated_at));
        entries
    }

    /// Update the summary field for a session (rewrites metadata line).
    pub async fn update_summary(&mut self, key: &SessionKey, summary: String) -> Result<()> {
        let session = self.get_or_create(key).await;
        session.summary = Some(summary);
        self.rewrite(key).await
    }

    /// Set a manual title for a session (rewrites metadata line). Once set,
    /// the title persists across new messages — auto-derivation in
    /// [`add_message_with_seq`] only fires when no manual title exists.
    pub async fn update_title(&mut self, key: &SessionKey, title: String) -> Result<()> {
        let session = self.get_or_create(key).await;
        session.title = Some(title);
        session.title_manual = true;
        self.rewrite(key).await
    }

    /// List sessions for a chat, merging per-user and legacy flat layouts.
    ///
    /// Scans `{data_dir}/users/{base_key}/sessions/` for JSONL files and
    /// also includes any sessions from the legacy flat `{data_dir}/sessions/`
    /// directory that aren't already present in the per-user layout.
    pub fn list_user_sessions(&self, base_key: &str) -> Vec<SessionListEntry> {
        let encoded_base = SessionHandle::encode_path_component(base_key);
        let user_sessions_dir = self
            .sessions_dir
            .parent()
            .unwrap_or(&self.sessions_dir)
            .join("users")
            .join(&encoded_base)
            .join("sessions");

        let mut entries = if user_sessions_dir.is_dir() {
            Self::scan_sessions_dir(&user_sessions_dir)
        } else {
            Vec::new()
        };

        // Merge legacy flat layout sessions that don't exist in per-user dir
        let legacy = self.list_sessions_for_chat(base_key);
        let existing_topics: std::collections::HashSet<Option<String>> =
            entries.iter().map(|e| e.topic.clone()).collect();
        for entry in legacy {
            if !existing_topics.contains(&entry.topic) {
                entries.push(entry);
            }
        }

        entries.sort_by_key(|entry| std::cmp::Reverse(entry.updated_at));
        entries
    }

    /// Scan a sessions directory and return entries sorted by updated_at descending.
    fn scan_sessions_dir(dir: &Path) -> Vec<SessionListEntry> {
        let mut entries = Vec::new();
        let Ok(read_dir) = std::fs::read_dir(dir) else {
            return entries;
        };

        for entry in read_dir.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let Some(name) = path.file_stem().and_then(|n| n.to_str()) else {
                continue;
            };

            if path
                .metadata()
                .map(|m| m.len() > MAX_SESSION_FILE_SIZE)
                .unwrap_or(false)
            {
                continue;
            }

            let Ok(content) = std::fs::read_to_string(&path) else {
                continue;
            };
            let mut lines = content.lines();
            let Some(meta_line) = lines.next() else {
                continue;
            };
            let meta: SessionMeta = match serde_json::from_str(meta_line) {
                Ok(m) => m,
                Err(_) => continue,
            };

            let message_count = lines.filter(|l| !l.trim().is_empty()).count();
            let topic = if name == "default" {
                None
            } else {
                Some(Self::decode_filename(name))
            };

            entries.push(SessionListEntry {
                topic,
                message_count,
                updated_at: meta.updated_at,
                summary: meta.summary,
                title: meta.title,
            });
        }

        entries.sort_by_key(|entry| std::cmp::Reverse(entry.updated_at));
        entries
    }
}

/// Tracks which topic is active per chat, enabling multi-session switching.
///
/// Persisted as JSON in `data_dir/active_sessions.json`.
pub struct ActiveSessionStore {
    path: PathBuf,
    /// base_key → active topic (empty string = default session)
    active: std::collections::HashMap<String, String>,
    /// base_key → previous topic (for /back command)
    previous: std::collections::HashMap<String, String>,
}

impl ActiveSessionStore {
    /// Open or create the active session store.
    pub fn open(data_dir: &Path) -> Result<Self> {
        let path = data_dir.join("active_sessions.json");
        let (active, previous) = if path.exists() {
            let data = std::fs::read_to_string(&path)?;
            let stored: StoredActiveSessions = serde_json::from_str(&data).unwrap_or_default();
            (stored.active, stored.previous)
        } else {
            (Default::default(), Default::default())
        };
        Ok(Self {
            path,
            active,
            previous,
        })
    }

    /// Resolve the full SessionKey for a base key, applying the active topic.
    pub fn resolve_session_key(&self, base_key: &str) -> SessionKey {
        let topic = self.active.get(base_key).map(|s| s.as_str()).unwrap_or("");
        if topic.is_empty() {
            SessionKey(base_key.to_string())
        } else {
            SessionKey(format!("{base_key}#{topic}"))
        }
    }

    /// Get the active topic for a base key (empty string = default).
    pub fn get_active_topic(&self, base_key: &str) -> &str {
        self.active.get(base_key).map(|s| s.as_str()).unwrap_or("")
    }

    /// Switch to a new topic. Records the previous topic for /back.
    pub fn switch_to(&mut self, base_key: &str, topic: &str) -> Result<()> {
        let prev = self.active.get(base_key).cloned().unwrap_or_default();
        self.previous.insert(base_key.to_string(), prev);
        self.active.insert(base_key.to_string(), topic.to_string());
        self.save()
    }

    /// Switch back to the previous topic. Returns the topic switched to.
    pub fn go_back(&mut self, base_key: &str) -> Result<Option<String>> {
        let prev = self.previous.remove(base_key);
        if let Some(ref topic) = prev {
            let current = self.active.get(base_key).cloned().unwrap_or_default();
            self.previous.insert(base_key.to_string(), current);
            self.active.insert(base_key.to_string(), topic.clone());
            self.save()?;
        }
        Ok(prev)
    }

    /// Remove tracking for a topic (e.g. when deleted).
    /// If the deleted topic was active, switches to default.
    pub fn remove_topic(&mut self, base_key: &str, topic: &str) -> Result<()> {
        if self.get_active_topic(base_key) == topic {
            self.active.insert(base_key.to_string(), String::new());
        }
        if self.previous.get(base_key).map(|s| s.as_str()) == Some(topic) {
            self.previous.remove(base_key);
        }
        self.save()
    }

    fn save(&self) -> Result<()> {
        let stored = StoredActiveSessions {
            active: self.active.clone(),
            previous: self.previous.clone(),
        };
        let json = serde_json::to_string_pretty(&stored)?;

        // Atomic write-then-rename
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, &json)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct StoredActiveSessions {
    #[serde(default)]
    active: std::collections::HashMap<String, String>,
    #[serde(default)]
    previous: std::collections::HashMap<String, String>,
}

/// Validate a topic name. Returns Err with a message if invalid.
///
/// Rejects the literal `"default"` because the per-user storage layout
/// uses `default.jsonl` as the no-topic filename — a user-named `"default"`
/// topic would silently collide with the topic-less mapping.
pub fn validate_topic_name(topic: &str) -> std::result::Result<(), &'static str> {
    if topic.is_empty() {
        return Err("topic name cannot be empty");
    }
    if topic.len() > 50 {
        return Err("topic name too long (max 50 characters)");
    }
    if topic.contains('#') || topic.contains(':') || topic.contains('/') {
        return Err("topic name cannot contain #, :, or /");
    }
    if topic.chars().any(|c| c.is_control()) {
        return Err("topic name cannot contain control characters");
    }
    if topic.eq_ignore_ascii_case("default") {
        return Err("topic name 'default' is reserved (used as the no-topic filename in storage)");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use octos_core::MessageRole;
    use tempfile::TempDir;

    fn make_message(role: MessageRole, content: &str) -> Message {
        Message {
            role,
            content: content.into(),
            media: vec![],
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            client_message_id: None,
            timestamp: Utc::now(),
        }
    }

    #[test]
    fn test_session_get_history() {
        let mut session = Session::new(SessionKey::new("cli", "test"));
        for i in 0..10 {
            session
                .messages
                .push(make_message(MessageRole::User, &format!("msg{i}")));
        }
        let history = session.get_history(3);
        assert_eq!(history.len(), 3);
        assert_eq!(history[0].content, "msg7");
        assert_eq!(history[2].content, "msg9");
    }

    #[test]
    fn test_session_get_history_all() {
        let mut session = Session::new(SessionKey::new("cli", "test"));
        session.messages.push(make_message(MessageRole::User, "a"));
        session.messages.push(make_message(MessageRole::User, "b"));
        let history = session.get_history(10);
        assert_eq!(history.len(), 2);
    }

    #[test]
    fn test_sort_by_timestamp_restores_order() {
        use chrono::Duration;
        let mut session = Session::new(SessionKey::new("cli", "test"));
        let t0 = Utc::now();

        // Simulate speculative overflow: primary pre-saved at t0,
        // overflow inserted at t0+15s, primary results saved at t0+45s.
        let mut msg_a = make_message(MessageRole::User, "primary question");
        msg_a.timestamp = t0;

        let mut msg_b_user = make_message(MessageRole::User, "overflow question");
        msg_b_user.timestamp = t0 + Duration::seconds(15);

        let mut msg_b_asst = make_message(MessageRole::Assistant, "overflow answer");
        msg_b_asst.timestamp = t0 + Duration::seconds(16);

        // Primary's tool call happened at t=5s but saved later
        let mut msg_a_tool = make_message(MessageRole::Assistant, "tool_call for primary");
        msg_a_tool.timestamp = t0 + Duration::seconds(5);

        let mut msg_a_result = make_message(MessageRole::User, "tool_result");
        msg_a_result.timestamp = t0 + Duration::seconds(8);

        let mut msg_a_reply = make_message(MessageRole::Assistant, "primary answer");
        msg_a_reply.timestamp = t0 + Duration::seconds(44);

        // Insert in write order (primary pre-save, overflow, primary completion)
        session.messages.push(msg_a); // t0
        session.messages.push(msg_b_user); // t0+15
        session.messages.push(msg_b_asst); // t0+16
        session.messages.push(msg_a_tool); // t0+5 (out of order!)
        session.messages.push(msg_a_result); // t0+8 (out of order!)
        session.messages.push(msg_a_reply); // t0+44

        // Before sort: insertion order
        assert_eq!(session.messages[1].content, "overflow question");
        assert_eq!(session.messages[3].content, "tool_call for primary");

        session.sort_by_timestamp();

        // After sort: chronological order
        assert_eq!(session.messages[0].content, "primary question"); // t0
        assert_eq!(session.messages[1].content, "tool_call for primary"); // t0+5
        assert_eq!(session.messages[2].content, "tool_result"); // t0+8
        assert_eq!(session.messages[3].content, "overflow question"); // t0+15
        assert_eq!(session.messages[4].content, "overflow answer"); // t0+16
        assert_eq!(session.messages[5].content, "primary answer"); // t0+44
    }

    #[tokio::test]
    async fn test_session_manager_create_and_retrieve() {
        let tmp = TempDir::new().unwrap();
        let mut mgr = SessionManager::open(tmp.path()).unwrap();
        let key = SessionKey::new("cli", "default");

        let session = mgr.get_or_create(&key).await;
        assert_eq!(session.messages.len(), 0);

        mgr.add_message(&key, make_message(MessageRole::User, "hello"))
            .await
            .unwrap();
        mgr.add_message(&key, make_message(MessageRole::Assistant, "hi"))
            .await
            .unwrap();

        let session = mgr.get_or_create(&key).await;
        assert_eq!(session.messages.len(), 2);
    }

    #[tokio::test]
    async fn test_session_manager_persistence() {
        let tmp = TempDir::new().unwrap();
        let key = SessionKey::new("cli", "persist");

        // Write session
        {
            let mut mgr = SessionManager::open(tmp.path()).unwrap();
            mgr.add_message(&key, make_message(MessageRole::User, "saved"))
                .await
                .unwrap();
            mgr.add_message(&key, make_message(MessageRole::Assistant, "reply"))
                .await
                .unwrap();
        }

        // New manager should load from disk
        {
            let mut mgr = SessionManager::open(tmp.path()).unwrap();
            let session = mgr.get_or_create(&key).await;
            assert_eq!(session.messages.len(), 2);
            assert_eq!(session.messages[0].content, "saved");
            assert_eq!(session.messages[1].content, "reply");
        }
    }

    #[tokio::test]
    async fn test_session_manager_clear() {
        let tmp = TempDir::new().unwrap();
        let key = SessionKey::new("cli", "clear-me");
        let mut mgr = SessionManager::open(tmp.path()).unwrap();

        mgr.add_message(&key, make_message(MessageRole::User, "temp"))
            .await
            .unwrap();
        assert_eq!(mgr.get_or_create(&key).await.messages.len(), 1);

        mgr.clear(&key).await.unwrap();

        // After clear, should be empty
        let session = mgr.get_or_create(&key).await;
        assert_eq!(session.messages.len(), 0);
    }

    #[tokio::test]
    async fn test_concurrent_sessions() {
        let tmp = TempDir::new().unwrap();
        let mut mgr = SessionManager::open(tmp.path()).unwrap();

        let k1 = SessionKey::new("telegram", "chat1");
        let k2 = SessionKey::new("telegram", "chat2");

        mgr.add_message(&k1, make_message(MessageRole::User, "from chat1"))
            .await
            .unwrap();
        mgr.add_message(&k2, make_message(MessageRole::User, "from chat2"))
            .await
            .unwrap();

        assert_eq!(mgr.get_or_create(&k1).await.messages.len(), 1);
        assert_eq!(mgr.get_or_create(&k2).await.messages.len(), 1);
        assert_eq!(
            mgr.get_or_create(&k1).await.messages[0].content,
            "from chat1"
        );
        assert_eq!(
            mgr.get_or_create(&k2).await.messages[0].content,
            "from chat2"
        );
    }

    #[tokio::test]
    async fn test_session_rewrite() {
        let tmp = TempDir::new().unwrap();
        let key = SessionKey::new("cli", "rewrite");
        let mut mgr = SessionManager::open(tmp.path()).unwrap();

        // Add 5 messages
        for i in 0..5 {
            mgr.add_message(&key, make_message(MessageRole::User, &format!("msg{i}")))
                .await
                .unwrap();
        }

        // Mutate in-memory: keep only last 2
        let session = mgr.get_or_create(&key).await;
        session.messages.drain(0..3);
        assert_eq!(session.messages.len(), 2);

        // Rewrite to disk
        mgr.rewrite(&key).await.unwrap();

        // Load fresh from disk — should have only 2 messages
        let mut mgr2 = SessionManager::open(tmp.path()).unwrap();
        let session2 = mgr2.get_or_create(&key).await;
        assert_eq!(session2.messages.len(), 2);
        assert_eq!(session2.messages[0].content, "msg3");
        assert_eq!(session2.messages[1].content, "msg4");
    }

    #[tokio::test]
    async fn concurrent_rewrites_of_same_session_dont_collide_on_tmp_path() {
        // Regression: prior to using a unique-per-call tmp suffix, two writers
        // racing the same session file (e.g. fanout children of one parent
        // calling parent.rewrite() in the same millisecond) shared a single
        // `<file>.jsonl.tmp` path. Both `File::create` would clobber the same
        // tmp; one rename succeeded, the other got ENOENT and surfaced as a
        // failed rewrite — manifested in spawn lifecycle as `Orphaned` instead
        // of `Joined` for the unlucky child. Asserts the rewrite race no
        // longer drops state.
        let tmp = TempDir::new().unwrap();
        let key = SessionKey::new("cli", "rewrite-race");
        let mut mgr = SessionManager::open(tmp.path()).unwrap();
        for i in 0..3 {
            mgr.add_message(&key, make_message(MessageRole::User, &format!("seed{i}")))
                .await
                .unwrap();
        }
        let mgr = std::sync::Arc::new(tokio::sync::Mutex::new(mgr));

        // Spawn N concurrent rewrites of the same session. Without the unique
        // suffix, several would race on the shared `<file>.jsonl.tmp` path and
        // ~1 in N would fail with ENOENT.
        let mut handles = Vec::new();
        for _ in 0..16 {
            let mgr = mgr.clone();
            let key = key.clone();
            handles.push(tokio::spawn(async move {
                let mgr = mgr.lock().await;
                mgr.rewrite(&key).await
            }));
        }
        for h in handles {
            let result = h.await.expect("join");
            assert!(
                result.is_ok(),
                "concurrent rewrite must not lose to tmp-file collision: {result:?}"
            );
        }

        // Disk state should still be parseable.
        let mut reload = SessionManager::open(tmp.path()).unwrap();
        let session = reload.get_or_create(&key).await;
        assert_eq!(session.messages.len(), 3);
    }

    #[test]
    fn rewrite_tmp_path_is_unique_per_call() {
        let target = std::path::PathBuf::from("/tmp/some/session.jsonl");
        let a = rewrite_tmp_path(&target);
        let b = rewrite_tmp_path(&target);
        assert_ne!(a, b, "successive calls must produce distinct tmp paths");
        assert!(
            a.to_string_lossy().contains(".tmp"),
            "tmp path keeps a .tmp suffix: {}",
            a.display()
        );
        // Suffix encodes both PID and counter so cross-process races don't
        // collide either.
        let pid = std::process::id().to_string();
        assert!(
            a.to_string_lossy().contains(&pid),
            "tmp path includes the pid for cross-process disambiguation: {}",
            a.display()
        );
    }

    #[tokio::test]
    async fn test_fork_creates_child() {
        let tmp = TempDir::new().unwrap();
        let mut mgr = SessionManager::open(tmp.path()).unwrap();
        let parent = SessionKey::new("telegram", "chat1");

        for i in 0..5 {
            mgr.add_message(&parent, make_message(MessageRole::User, &format!("msg{i}")))
                .await
                .unwrap();
        }

        let child_key = mgr.fork(&parent, "chat1_fork", 3).await.unwrap();
        assert_eq!(child_key, SessionKey::new("telegram", "chat1_fork"));

        let child = mgr.get_or_create(&child_key).await;
        assert_eq!(child.parent_key, Some(parent.clone()));
        assert_eq!(child.messages.len(), 3);
        assert_eq!(child.messages[0].content, "msg2");
        assert_eq!(child.messages[2].content, "msg4");
    }

    #[tokio::test]
    async fn test_eviction_keeps_max_sessions() {
        let tmp = TempDir::new().unwrap();
        let mut mgr = SessionManager::open(tmp.path())
            .unwrap()
            .with_max_sessions(3);

        // Create 5 sessions
        for i in 0..5 {
            let key = SessionKey::new("cli", &format!("s{i}"));
            mgr.add_message(&key, make_message(MessageRole::User, &format!("msg{i}")))
                .await
                .unwrap();
            // Small delay so last_accessed ordering is deterministic
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }

        // Should have at most 3 in memory
        assert_eq!(mgr.cache_len(), 3);
    }

    #[tokio::test]
    async fn test_evicted_session_reloads_from_disk() {
        let tmp = TempDir::new().unwrap();
        let mut mgr = SessionManager::open(tmp.path())
            .unwrap()
            .with_max_sessions(2);

        let k0 = SessionKey::new("cli", "oldest");
        mgr.add_message(&k0, make_message(MessageRole::User, "hello from oldest"))
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;

        let k1 = SessionKey::new("cli", "middle");
        mgr.add_message(&k1, make_message(MessageRole::User, "hello from middle"))
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;

        let k2 = SessionKey::new("cli", "newest");
        mgr.add_message(&k2, make_message(MessageRole::User, "hello from newest"))
            .await
            .unwrap();

        // k0 should have been evicted from memory
        assert_eq!(mgr.cache_len(), 2);

        // But accessing k0 should reload it from disk
        let session = mgr.get_or_create(&k0).await;
        assert_eq!(session.messages.len(), 1);
        assert_eq!(session.messages[0].content, "hello from oldest");
    }

    #[test]
    fn test_with_max_sessions_clamps_zero() {
        let tmp = TempDir::new().unwrap();
        let mgr = SessionManager::open(tmp.path())
            .unwrap()
            .with_max_sessions(0);
        assert_eq!(mgr.capacity(), 1);
    }

    /// Integration test: concurrent session processing via multiple tasks.
    /// Verifies that sessions created from parallel tasks don't corrupt each other
    /// and the LRU cache correctly evicts and reloads.
    #[tokio::test]
    async fn test_concurrent_session_processing() {
        use std::sync::Arc;
        use tokio::sync::Mutex;

        let tmp = TempDir::new().unwrap();
        let mgr = Arc::new(Mutex::new(
            SessionManager::open(tmp.path())
                .unwrap()
                .with_max_sessions(5),
        ));

        // Spawn 10 tasks that each create a session and add messages
        let mut handles = Vec::new();
        for i in 0..10 {
            let mgr = mgr.clone();
            handles.push(tokio::spawn(async move {
                let key = SessionKey::new("test", &format!("session-{i}"));
                let mut mgr = mgr.lock().await;
                mgr.add_message(
                    &key,
                    make_message(MessageRole::User, &format!("hello from {i}")),
                )
                .await
                .unwrap();
                mgr.add_message(
                    &key,
                    make_message(MessageRole::Assistant, &format!("reply to {i}")),
                )
                .await
                .unwrap();
            }));
        }

        for h in handles {
            h.await.unwrap();
        }

        // Cache should be capped at 5
        let mgr = mgr.lock().await;
        assert!(mgr.cache_len() <= 5);

        // But all 10 sessions should be loadable from disk
        drop(mgr);
        let mut fresh = SessionManager::open(tmp.path()).unwrap();
        for i in 0..10 {
            let key = SessionKey::new("test", &format!("session-{i}"));
            let session = fresh.get_or_create(&key).await;
            assert_eq!(
                session.messages.len(),
                2,
                "session-{i} should have 2 messages"
            );
            assert_eq!(session.messages[0].content, format!("hello from {i}"));
        }
    }

    #[tokio::test]
    async fn test_fork_persists_to_disk() {
        let tmp = TempDir::new().unwrap();
        let parent = SessionKey::new("cli", "main");

        {
            let mut mgr = SessionManager::open(tmp.path()).unwrap();
            mgr.add_message(&parent, make_message(MessageRole::User, "hello"))
                .await
                .unwrap();
            mgr.fork(&parent, "branch", 1).await.unwrap();
        }

        // Reload from disk
        let mut mgr2 = SessionManager::open(tmp.path()).unwrap();
        let child_key = SessionKey::new("cli", "branch");
        let child = mgr2.get_or_create(&child_key).await;
        assert_eq!(child.parent_key, Some(parent));
        assert_eq!(child.messages.len(), 1);
        assert_eq!(child.messages[0].content, "hello");
    }

    #[tokio::test]
    async fn test_session_handle_fork_from_parent_if_missing_copies_recent_history() {
        let tmp = TempDir::new().unwrap();
        let parent = SessionKey::new("api", "web-parent");
        let child = child_session_key(&parent, "task-123");

        {
            let mut parent_handle = SessionHandle::open(tmp.path(), &parent);
            parent_handle
                .add_message(make_message(MessageRole::User, "msg0"))
                .await
                .unwrap();
            parent_handle
                .add_message(make_message(MessageRole::Assistant, "msg1"))
                .await
                .unwrap();
            parent_handle
                .add_message(make_message(MessageRole::User, "msg2"))
                .await
                .unwrap();
        }

        SessionHandle::fork_from_parent_if_missing(tmp.path(), &parent, &child, 2)
            .await
            .unwrap();

        let child_handle = SessionHandle::open(tmp.path(), &child);
        let child_session = child_handle.session();
        assert_eq!(child_session.parent_key, Some(parent.clone()));
        assert_eq!(child_session.messages.len(), 2);
        assert_eq!(child_session.messages[0].content, "msg1");
        assert_eq!(child_session.messages[1].content, "msg2");
    }

    #[tokio::test]
    async fn test_session_handle_fork_from_parent_if_missing_is_idempotent() {
        let tmp = TempDir::new().unwrap();
        let parent = SessionKey::new("api", "web-parent");
        let child = child_session_key(&parent, "task-123");

        {
            let mut parent_handle = SessionHandle::open(tmp.path(), &parent);
            parent_handle
                .add_message(make_message(MessageRole::User, "msg0"))
                .await
                .unwrap();
        }

        SessionHandle::fork_from_parent_if_missing(tmp.path(), &parent, &child, 1)
            .await
            .unwrap();

        {
            let mut child_handle = SessionHandle::open(tmp.path(), &child);
            child_handle
                .add_message(make_message(MessageRole::Assistant, "child-result"))
                .await
                .unwrap();
        }

        SessionHandle::fork_from_parent_if_missing(tmp.path(), &parent, &child, 1)
            .await
            .unwrap();

        let child_handle = SessionHandle::open(tmp.path(), &child);
        let child_session = child_handle.session();
        assert_eq!(child_session.messages.len(), 2);
        assert_eq!(child_session.messages[1].content, "child-result");
    }

    #[tokio::test]
    async fn test_child_session_contract_round_trips_through_disk() {
        let tmp = TempDir::new().unwrap();
        let parent = SessionKey::new("api", "parent");
        let child = child_session_key(&parent, "task-contract");

        {
            let mut parent_handle = SessionHandle::open(tmp.path(), &parent);
            parent_handle
                .add_message(make_message(MessageRole::User, "seed"))
                .await
                .unwrap();
        }

        {
            let mut child_handle = SessionHandle::open(tmp.path(), &child);
            child_handle
                .upsert_child_contract(ChildSessionContract {
                    task_id: "task-123".to_string(),
                    task_label: "Research".to_string(),
                    parent_session_key: parent.to_string(),
                    child_session_key: child.to_string(),
                    workflow_kind: Some("deep_research".to_string()),
                    current_phase: Some("research".to_string()),
                    terminal_state: None,
                    join_state: None,
                    joined_at: None,
                    failure_action: None,
                    error: None,
                    output_files: vec![],
                })
                .await
                .unwrap();
            child_handle
                .upsert_child_contract(ChildSessionContract {
                    task_id: "task-123".to_string(),
                    task_label: "Research".to_string(),
                    parent_session_key: parent.to_string(),
                    child_session_key: child.to_string(),
                    workflow_kind: Some("deep_research".to_string()),
                    current_phase: Some("deliver_result".to_string()),
                    terminal_state: Some(ChildSessionTerminalState::Completed),
                    join_state: Some(ChildSessionJoinState::Joined),
                    joined_at: Some(Utc::now()),
                    failure_action: None,
                    error: None,
                    output_files: vec!["/tmp/report.md".to_string()],
                })
                .await
                .unwrap();
        }

        assert!(SessionHandle::session_exists(tmp.path(), &child));

        let child_handle = SessionHandle::open(tmp.path(), &child);
        let child_session = child_handle.session();
        assert_eq!(child_session.child_contracts.len(), 1);
        let contract = &child_session.child_contracts[0];
        assert_eq!(contract.task_id, "task-123");
        assert_eq!(
            contract.terminal_state,
            Some(ChildSessionTerminalState::Completed)
        );
        assert_eq!(contract.join_state, Some(ChildSessionJoinState::Joined));
        assert_eq!(contract.output_files, vec!["/tmp/report.md"]);
        assert!(contract.joined_at.is_some());
    }

    #[tokio::test]
    async fn test_session_handle_fork_from_parent_if_missing_links_existing_child_history() {
        let tmp = TempDir::new().unwrap();
        let parent = SessionKey::new("api", "web-parent");
        let child = child_session_key(&parent, "task-linked");

        {
            let mut parent_handle = SessionHandle::open(tmp.path(), &parent);
            parent_handle
                .add_message(make_message(MessageRole::User, "parent-msg"))
                .await
                .unwrap();
        }

        {
            let mut child_handle = SessionHandle::open(tmp.path(), &child);
            child_handle
                .add_message(make_message(MessageRole::Assistant, "existing-child-msg"))
                .await
                .unwrap();
            assert_eq!(child_handle.session().parent_key, None);
        }

        SessionHandle::fork_from_parent_if_missing(tmp.path(), &parent, &child, 1)
            .await
            .unwrap();

        let child_handle = SessionHandle::open(tmp.path(), &child);
        let child_session = child_handle.session();
        assert_eq!(child_session.parent_key, Some(parent));
        assert_eq!(child_session.messages.len(), 1);
        assert_eq!(child_session.messages[0].content, "existing-child-msg");
    }

    #[tokio::test]
    async fn test_load_rejects_oversized_file() {
        let tmp = TempDir::new().unwrap();
        let mut mgr = SessionManager::open(tmp.path()).unwrap();
        let key = SessionKey::new("cli", "huge");

        // Write a normal message so the file exists
        mgr.add_message(&key, make_message(MessageRole::User, "seed"))
            .await
            .unwrap();

        // Evict from cache so next access must load from disk
        mgr.cache.pop(&key.0);

        // Overwrite the file with junk exceeding the size limit
        let path = mgr.session_path(&key);
        let junk = "x".repeat((MAX_SESSION_FILE_SIZE as usize) + 1);
        std::fs::write(&path, junk).unwrap();

        // load_from_disk should return None for oversized file
        assert!(mgr.load_from_disk(&key).await.is_none());
    }

    #[test]
    fn test_truncated_session_keys_no_collision() {
        let tmp = TempDir::new().unwrap();
        let mgr = SessionManager::open(tmp.path()).unwrap();

        // Create two keys that share the same 200-char prefix but differ after
        let prefix = "a".repeat(200);
        let key1 = SessionKey(format!("{prefix}_suffix1"));
        let key2 = SessionKey(format!("{prefix}_suffix2"));

        let path1 = mgr.session_path(&key1);
        let path2 = mgr.session_path(&key2);
        assert_ne!(
            path1, path2,
            "truncated keys with different suffixes must produce different paths"
        );
    }

    #[test]
    fn test_decode_filename() {
        assert_eq!(
            SessionManager::decode_filename("feishu%3Aoc_abc123"),
            "feishu:oc_abc123"
        );
        assert_eq!(
            SessionManager::decode_filename("cli%3Adefault"),
            "cli:default"
        );
        assert_eq!(SessionManager::decode_filename("plain-name"), "plain-name");
        // Double-byte UTF-8 round-trip
        assert_eq!(
            SessionManager::decode_filename("hello%E4%B8%96%E7%95%8C"),
            "hello\u{4e16}\u{754c}" // hello世界
        );
    }

    #[tokio::test]
    async fn test_list_sessions_returns_decoded_keys() {
        let tmp = TempDir::new().unwrap();
        let mut mgr = SessionManager::open(tmp.path()).unwrap();
        let key = SessionKey::new("feishu", "oc_abc123");

        mgr.add_message(&key, make_message(MessageRole::User, "hello"))
            .await
            .unwrap();

        let sessions = mgr.list_sessions();
        assert_eq!(sessions.len(), 1);
        // Should return decoded key, not percent-encoded filename
        assert_eq!(sessions[0].0, "feishu:oc_abc123");
    }

    /// Issue #607 §D: `/api/sessions` hung 30 s+ on a user dir with 65 535
    /// `child-*.jsonl` siblings because the listing iterated every JSONL.
    /// `list_top_level_sessions` must skip `child-*` and `*.tasks` files at
    /// the directory walk so the cost stays O(top-level sessions).
    #[tokio::test]
    async fn list_top_level_sessions_skips_child_jsonl() {
        let tmp = TempDir::new().unwrap();
        let mut mgr = SessionManager::open(tmp.path()).unwrap();
        let parent = SessionKey::new("api", "web-parent");

        // 1 top-level session.
        mgr.add_message(&parent, make_message(MessageRole::User, "parent"))
            .await
            .unwrap();

        // 100 child sessions written via the same canonical handle code path
        // production uses (so the test exercises real filename encoding).
        for i in 0..100 {
            let child = child_session_key(&parent, &format!("task-{i:03}"));
            mgr.add_message(&child, make_message(MessageRole::Assistant, "child"))
                .await
                .unwrap();
        }

        // The all-inclusive walk reflects every jsonl on disk (1 parent +
        // 100 children).
        let all = mgr.list_sessions();
        assert_eq!(all.len(), 101, "internal walk should include children");

        // The user-facing listing must surface only the top-level session.
        let top = mgr.list_top_level_sessions();
        assert_eq!(
            top.len(),
            1,
            "list_top_level_sessions must skip child-* fanouts; got {top:?}"
        );
        assert_eq!(top[0].0, "api:web-parent");
    }

    /// Sidecar `*.tasks.jsonl` ledgers (e.g. `default.tasks.jsonl`) are an
    /// internal runtime detail and must never appear in the user-facing
    /// listing.
    #[test]
    fn list_top_level_sessions_skips_tasks_sidecar_jsonl() {
        let tmp = TempDir::new().unwrap();
        let mgr = SessionManager::open(tmp.path()).unwrap();

        // Construct a per-user dir directly so we can drop in both a
        // top-level `default.jsonl` and an internal `default.tasks.jsonl`
        // without having to drive the task-ledger writers in this unit.
        let user_dir = tmp.path().join("users/api%3Aweb-tasks/sessions");
        std::fs::create_dir_all(&user_dir).unwrap();

        let meta = serde_json::json!({
            "schema_version": 1,
            "session_key": "api:web-tasks",
            "created_at": Utc::now(),
            "updated_at": Utc::now(),
        });
        std::fs::write(
            user_dir.join("default.jsonl"),
            format!("{}\n", serde_json::to_string(&meta).unwrap()),
        )
        .unwrap();
        // Sidecar — must be ignored.
        std::fs::write(
            user_dir.join("default.tasks.jsonl"),
            "{\"task_id\":\"t-1\",\"state\":\"queued\"}\n",
        )
        .unwrap();

        let top = mgr.list_top_level_sessions();
        let ids: Vec<&str> = top.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(ids, vec!["api:web-tasks"], "got {top:?}");
    }

    /// Regression guard for the O(N) hang. With 5 000 synthetic
    /// `child-*.jsonl` files on disk, `list_top_level_sessions` must stay
    /// well under the 500 ms bound — the original `list_sessions`
    /// `count_lines`-per-file loop blew past 30 s in the wild on a dir
    /// 13× larger.
    #[test]
    fn list_top_level_sessions_is_fast_with_many_child_jsonls() {
        let tmp = TempDir::new().unwrap();
        let mgr = SessionManager::open(tmp.path()).unwrap();

        let user_dir = tmp.path().join("users/api%3Aweb-river/sessions");
        std::fs::create_dir_all(&user_dir).unwrap();

        // Top-level session.
        std::fs::write(
            user_dir.join("default.jsonl"),
            "{\"schema_version\":1,\"session_key\":\"api:web-river\",\
             \"created_at\":\"2024-01-01T00:00:00Z\",\
             \"updated_at\":\"2024-01-01T00:00:00Z\"}\n",
        )
        .unwrap();

        // Synthetic spawn fanout.
        const FANOUT: usize = 5_000;
        for i in 0..FANOUT {
            std::fs::write(
                user_dir.join(format!("child-task-{i:05}.jsonl")),
                "{\"schema_version\":1}\n{\"role\":\"assistant\",\"content\":\"x\"}\n",
            )
            .unwrap();
        }

        let start = std::time::Instant::now();
        let top = mgr.list_top_level_sessions();
        let elapsed = start.elapsed();

        assert_eq!(top.len(), 1, "only top-level session should surface");
        assert!(
            elapsed < std::time::Duration::from_millis(500),
            "list_top_level_sessions took {elapsed:?} for {FANOUT} child files; \
             the per-file count_lines fallback regressed",
        );
    }

    #[test]
    fn test_short_key_no_hash_suffix() {
        let tmp = TempDir::new().unwrap();
        let mgr = SessionManager::open(tmp.path()).unwrap();

        let key = SessionKey::new("cli", "short");
        let path = mgr.session_path(&key);
        let name = path.file_stem().unwrap().to_str().unwrap();
        // Short keys should not have hash suffix (no underscore + hex)
        assert!(!name.contains('_') || name.len() < 200);
    }

    #[tokio::test]
    async fn test_list_sessions_for_chat() {
        let tmp = TempDir::new().unwrap();
        let mut mgr = SessionManager::open(tmp.path()).unwrap();

        // Create default session + two topic sessions
        let base = SessionKey::new("telegram", "12345");
        let research = SessionKey::with_topic("telegram", "12345", "research");
        let code = SessionKey::with_topic("telegram", "12345", "code");
        // Unrelated session
        let other = SessionKey::new("telegram", "99999");

        mgr.add_message(&base, make_message(MessageRole::User, "hello default"))
            .await
            .unwrap();
        mgr.add_message(&research, make_message(MessageRole::User, "hello research"))
            .await
            .unwrap();
        mgr.add_message(&code, make_message(MessageRole::User, "hello code"))
            .await
            .unwrap();
        mgr.add_message(&other, make_message(MessageRole::User, "unrelated"))
            .await
            .unwrap();

        let entries = mgr.list_sessions_for_chat("telegram:12345");
        assert_eq!(entries.len(), 3);

        let topics: Vec<Option<String>> = entries.iter().map(|e| e.topic.clone()).collect();
        assert!(topics.contains(&None)); // default
        assert!(topics.contains(&Some("research".into())));
        assert!(topics.contains(&Some("code".into())));

        // Each has 1 message
        for e in &entries {
            assert_eq!(e.message_count, 1);
        }
    }

    #[tokio::test]
    async fn test_session_topic_persists() {
        let tmp = TempDir::new().unwrap();
        let key = SessionKey::with_topic("telegram", "12345", "research");

        {
            let mut mgr = SessionManager::open(tmp.path()).unwrap();
            mgr.add_message(&key, make_message(MessageRole::User, "topic data"))
                .await
                .unwrap();
        }

        // Reload and verify topic
        let mut mgr = SessionManager::open(tmp.path()).unwrap();
        let session = mgr.get_or_create(&key).await;
        assert_eq!(session.topic.as_deref(), Some("research"));
    }

    #[tokio::test]
    async fn test_update_summary() {
        let tmp = TempDir::new().unwrap();
        let key = SessionKey::new("telegram", "12345");

        let mut mgr = SessionManager::open(tmp.path()).unwrap();
        mgr.add_message(&key, make_message(MessageRole::User, "hello"))
            .await
            .unwrap();
        mgr.update_summary(&key, "A test session".into())
            .await
            .unwrap();

        // Reload and verify summary
        let mut mgr2 = SessionManager::open(tmp.path()).unwrap();
        let session = mgr2.get_or_create(&key).await;
        assert_eq!(session.summary.as_deref(), Some("A test session"));
    }

    #[tokio::test]
    async fn should_persist_title_separately_from_summary_when_renamed() {
        let tmp = TempDir::new().unwrap();
        let key = SessionKey::new("telegram", "12345");

        let mut mgr = SessionManager::open(tmp.path()).unwrap();
        mgr.add_message(&key, make_message(MessageRole::User, "hello"))
            .await
            .unwrap();
        mgr.update_title(&key, "Custom title".into()).await.unwrap();
        mgr.update_summary(&key, "Long-form summary".into())
            .await
            .unwrap();

        // Reload from disk and verify title + summary are independent.
        let mut mgr2 = SessionManager::open(tmp.path()).unwrap();
        let session = mgr2.get_or_create(&key).await;
        assert_eq!(session.title.as_deref(), Some("Custom title"));
        assert_eq!(session.summary.as_deref(), Some("Long-form summary"));
    }

    #[tokio::test]
    async fn should_auto_derive_title_from_first_user_message_when_unset() {
        let tmp = TempDir::new().unwrap();
        let key = SessionKey::new("telegram", "12345");

        let mut mgr = SessionManager::open(tmp.path()).unwrap();
        mgr.add_message(
            &key,
            make_message(MessageRole::User, "What is the weather today?"),
        )
        .await
        .unwrap();

        let session = mgr.get_or_create(&key).await;
        assert_eq!(
            session.title.as_deref(),
            Some("What is the weather today?"),
            "first user message should auto-populate title"
        );
    }

    #[tokio::test]
    async fn should_not_overwrite_manual_title_when_subsequent_messages_arrive() {
        let tmp = TempDir::new().unwrap();
        let key = SessionKey::new("telegram", "12345");

        let mut mgr = SessionManager::open(tmp.path()).unwrap();
        mgr.update_title(&key, "Manual title".into()).await.unwrap();
        mgr.add_message(&key, make_message(MessageRole::User, "first user message"))
            .await
            .unwrap();
        mgr.add_message(&key, make_message(MessageRole::User, "second user message"))
            .await
            .unwrap();

        let session = mgr.get_or_create(&key).await;
        assert_eq!(
            session.title.as_deref(),
            Some("Manual title"),
            "manual title must be preserved across new messages"
        );
    }

    #[tokio::test]
    async fn should_truncate_auto_derived_title_to_50_chars() {
        let tmp = TempDir::new().unwrap();
        let key = SessionKey::new("telegram", "12345");

        let mut mgr = SessionManager::open(tmp.path()).unwrap();
        let long_message = "a".repeat(200);
        mgr.add_message(&key, make_message(MessageRole::User, &long_message))
            .await
            .unwrap();

        let session = mgr.get_or_create(&key).await;
        let title = session.title.as_deref().unwrap();
        assert!(
            title.chars().count() <= 50,
            "title should be at most 50 chars, got {}",
            title.chars().count()
        );
    }

    #[test]
    fn test_active_session_store() {
        let tmp = TempDir::new().unwrap();
        let mut store = ActiveSessionStore::open(tmp.path()).unwrap();

        // Default: no topic
        assert_eq!(store.get_active_topic("telegram:12345"), "");
        let key = store.resolve_session_key("telegram:12345");
        assert_eq!(key.0, "telegram:12345");

        // Switch to "research"
        store.switch_to("telegram:12345", "research").unwrap();
        assert_eq!(store.get_active_topic("telegram:12345"), "research");
        let key = store.resolve_session_key("telegram:12345");
        assert_eq!(key.0, "telegram:12345#research");

        // Switch to "code"
        store.switch_to("telegram:12345", "code").unwrap();
        assert_eq!(store.get_active_topic("telegram:12345"), "code");

        // Go back -> should return "research"
        let prev = store.go_back("telegram:12345").unwrap();
        assert_eq!(prev, Some("research".into()));
        assert_eq!(store.get_active_topic("telegram:12345"), "research");
    }

    #[test]
    fn test_active_session_store_persistence() {
        let tmp = TempDir::new().unwrap();

        {
            let mut store = ActiveSessionStore::open(tmp.path()).unwrap();
            store.switch_to("telegram:12345", "research").unwrap();
        }

        // Reload
        let store = ActiveSessionStore::open(tmp.path()).unwrap();
        assert_eq!(store.get_active_topic("telegram:12345"), "research");
    }

    #[test]
    fn test_validate_topic_name() {
        assert!(validate_topic_name("research").is_ok());
        assert!(validate_topic_name("my-code").is_ok());
        assert!(validate_topic_name("work_notes").is_ok());
        assert!(validate_topic_name("").is_err());
        assert!(validate_topic_name("a#b").is_err());
        assert!(validate_topic_name("a:b").is_err());
        assert!(validate_topic_name("a/b").is_err());
        assert!(validate_topic_name(&"x".repeat(51)).is_err());

        // Reserved: "default" is the no-topic filename in the per-user
        // layout. Allowing a user-named "default" topic would silently
        // collide with the topic-less mapping.
        assert!(validate_topic_name("default").is_err());
        assert!(validate_topic_name("DEFAULT").is_err());
        assert!(validate_topic_name("Default").is_err());
    }

    #[tokio::test]
    async fn test_append_respects_file_size_limit() {
        let tmp = TempDir::new().unwrap();
        let mut mgr = SessionManager::open(tmp.path()).unwrap();
        let key = SessionKey::new("cli", "big");

        // Write a seed message
        mgr.add_message(&key, make_message(MessageRole::User, "seed"))
            .await
            .unwrap();

        // Manually inflate the file to just under the limit
        let path = mgr.session_path(&key);
        let padding = "x".repeat((MAX_SESSION_FILE_SIZE as usize) - 10);
        std::fs::write(&path, padding).unwrap();

        // Append should silently skip (file is at limit)
        mgr.add_message(&key, make_message(MessageRole::User, "should not append"))
            .await
            .unwrap();

        // File should not have grown significantly
        let size = std::fs::metadata(&path).unwrap().len();
        assert!(size < MAX_SESSION_FILE_SIZE + 1000);
    }

    #[tokio::test]
    async fn test_load_rejects_future_schema_version() {
        let tmp = TempDir::new().unwrap();
        let mgr = SessionManager::open(tmp.path()).unwrap();
        let key = SessionKey::new("cli", "future");

        // Write a session file with schema version 999
        let meta = serde_json::json!({
            "schema_version": 999,
            "session_key": "cli:future",
            "created_at": "2025-01-01T00:00:00Z",
            "updated_at": "2025-01-01T00:00:00Z"
        });
        let path = mgr.session_path(&key);
        let content = format!("{}\n", serde_json::to_string(&meta).unwrap());
        std::fs::write(&path, content).unwrap();

        // Should refuse to load
        assert!(mgr.load_from_disk(&key).await.is_none());
    }

    #[tokio::test]
    async fn test_load_from_disk_merges_flat_and_per_user_histories() {
        let tmp = TempDir::new().unwrap();
        let mgr = SessionManager::open(tmp.path()).unwrap();
        let key = SessionKey::with_profile("dspfac", "api", "slides-123");

        let older = chrono::Utc::now() - chrono::Duration::minutes(2);
        let newer = older + chrono::Duration::minutes(1);

        let flat_meta = serde_json::json!({
            "schema_version": 1,
            "session_key": key.0,
            "created_at": older,
            "updated_at": newer
        });
        std::fs::create_dir_all(tmp.path().join("sessions")).unwrap();
        std::fs::write(
            mgr.session_path(&key),
            format!(
                "{}\n{}\n",
                serde_json::to_string(&flat_meta).unwrap(),
                serde_json::to_string(&Message {
                    role: MessageRole::Assistant,
                    content: "artifact".into(),
                    media: vec!["/tmp/file.png".into()],
                    tool_calls: None,
                    tool_call_id: None,
                    reasoning_content: None,
                    client_message_id: None,
                    timestamp: newer,
                })
                .unwrap()
            ),
        )
        .unwrap();

        let encoded_base = encode_path_component(key.base_key());
        let per_user_dir = tmp.path().join("users").join(encoded_base).join("sessions");
        std::fs::create_dir_all(&per_user_dir).unwrap();
        let per_user_meta = serde_json::json!({
            "schema_version": 1,
            "session_key": key.0,
            "created_at": older,
            "updated_at": older
        });
        std::fs::write(
            per_user_dir.join("default.jsonl"),
            format!(
                "{}\n{}\n",
                serde_json::to_string(&per_user_meta).unwrap(),
                serde_json::to_string(&Message {
                    role: MessageRole::User,
                    content: "make slides".into(),
                    media: vec![],
                    tool_calls: None,
                    tool_call_id: None,
                    reasoning_content: None,
                    client_message_id: None,
                    timestamp: older,
                })
                .unwrap()
            ),
        )
        .unwrap();

        let session = mgr
            .load_from_disk(&key)
            .await
            .expect("expected merged session");
        assert_eq!(session.messages.len(), 2);
        assert_eq!(session.messages[0].content, "make slides");
        assert_eq!(session.messages[1].content, "artifact");
        assert_eq!(session.updated_at, newer);
    }

    #[tokio::test]
    async fn test_purge_stale_sessions() {
        let tmp = TempDir::new().unwrap();
        let mut mgr = SessionManager::open(tmp.path()).unwrap();

        // Create a session
        let key = SessionKey::new("cli", "old-session");
        mgr.add_message(&key, make_message(MessageRole::User, "old"))
            .await
            .unwrap();

        // Manually backdate the session metadata to 100 days ago
        let path = mgr.session_path(&key);
        let content = std::fs::read_to_string(&path).unwrap();
        let mut lines: Vec<&str> = content.lines().collect();
        let mut meta: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        let old_date = (Utc::now() - chrono::Duration::days(100))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string();
        meta["updated_at"] = serde_json::Value::String(old_date);
        lines[0] = &serde_json::to_string(&meta).unwrap();
        // Need to own the string for lines[0]
        let meta_str = serde_json::to_string(&meta).unwrap();
        let new_content = format!(
            "{}\n{}\n",
            meta_str,
            content.lines().skip(1).collect::<Vec<_>>().join("\n")
        );
        std::fs::write(&path, new_content).unwrap();

        // Purge sessions older than 90 days
        let removed = mgr.purge_stale(90);
        assert_eq!(removed, 1);

        // File should be gone
        assert!(!path.exists());
    }

    #[test]
    fn test_list_user_sessions_merges_both_layouts() {
        let tmp = TempDir::new().unwrap();
        let mgr = SessionManager::open(tmp.path()).unwrap();

        let now = Utc::now();
        let older = now - chrono::Duration::hours(2);
        let old = now - chrono::Duration::hours(1);

        // --- Legacy flat layout ---
        // Default session (older timestamp — should be superseded by per-user default)
        let legacy_default_meta = serde_json::json!({
            "schema_version": 1,
            "session_key": "telegram:12345",
            "created_at": older,
            "updated_at": older
        });
        let legacy_default_path = tmp.path().join("sessions/telegram%3A12345.jsonl");
        std::fs::write(
            &legacy_default_path,
            format!(
                "{}\n{}\n",
                serde_json::to_string(&legacy_default_meta).unwrap(),
                serde_json::to_string(&make_message(MessageRole::User, "legacy default")).unwrap()
            ),
        )
        .unwrap();

        // "research" topic — only exists in legacy
        let legacy_research_meta = serde_json::json!({
            "schema_version": 1,
            "session_key": "telegram:12345#research",
            "topic": "research",
            "created_at": old,
            "updated_at": old
        });
        let legacy_research_path = tmp
            .path()
            .join("sessions/telegram%3A12345%23research.jsonl");
        std::fs::write(
            &legacy_research_path,
            format!(
                "{}\n{}\n",
                serde_json::to_string(&legacy_research_meta).unwrap(),
                serde_json::to_string(&make_message(MessageRole::User, "legacy research")).unwrap()
            ),
        )
        .unwrap();

        // --- Per-user layout ---
        let user_sessions_dir = tmp.path().join("users/telegram%3A12345/sessions");
        std::fs::create_dir_all(&user_sessions_dir).unwrap();

        // Default session (newer — should win over legacy default)
        let peruser_default_meta = serde_json::json!({
            "schema_version": 1,
            "session_key": "telegram:12345",
            "created_at": old,
            "updated_at": now
        });
        std::fs::write(
            user_sessions_dir.join("default.jsonl"),
            format!(
                "{}\n{}\n",
                serde_json::to_string(&peruser_default_meta).unwrap(),
                serde_json::to_string(&make_message(MessageRole::User, "peruser default")).unwrap()
            ),
        )
        .unwrap();

        // "coding" topic — only exists in per-user
        let peruser_coding_meta = serde_json::json!({
            "schema_version": 1,
            "session_key": "telegram:12345#coding",
            "topic": "coding",
            "created_at": old,
            "updated_at": old
        });
        std::fs::write(
            user_sessions_dir.join("coding.jsonl"),
            format!(
                "{}\n{}\n",
                serde_json::to_string(&peruser_coding_meta).unwrap(),
                serde_json::to_string(&make_message(MessageRole::User, "peruser coding")).unwrap()
            ),
        )
        .unwrap();

        // --- Call list_user_sessions and assert ---
        let entries = mgr.list_user_sessions("telegram:12345");

        // Should have 3 entries: default (per-user), research (legacy), coding (per-user)
        assert_eq!(entries.len(), 3, "expected 3 entries, got: {entries:?}");

        // Sorted by updated_at descending: default(now) > research(old) >= coding(old)
        let topics: Vec<Option<&str>> = entries.iter().map(|e| e.topic.as_deref()).collect();

        // Default session (from per-user, not legacy) should be first (newest)
        assert_eq!(
            entries[0].topic, None,
            "first entry should be default session"
        );
        assert_eq!(
            entries[0].updated_at, now,
            "default session should come from per-user layout (newer timestamp)"
        );

        // "research" and "coding" should both be present
        assert!(
            topics.contains(&Some("research")),
            "research topic should be included from legacy"
        );
        assert!(
            topics.contains(&Some("coding")),
            "coding topic should be included from per-user"
        );

        // Each entry should have 1 message
        for e in &entries {
            assert_eq!(e.message_count, 1, "each session should have 1 message");
        }
    }

    #[tokio::test]
    async fn test_session_handle_add_message_with_seq_returns_committed_index() {
        let tmp = TempDir::new().unwrap();
        let key = SessionKey::new("api", "web-seq-test");
        let mut handle = SessionHandle::open(tmp.path(), &key);

        let first = handle
            .add_message_with_seq(make_message(MessageRole::User, "hello"))
            .await
            .unwrap();
        let second = handle
            .add_message_with_seq(make_message(MessageRole::Assistant, "world"))
            .await
            .unwrap();

        assert_eq!(first, 0);
        assert_eq!(second, 1);
        assert_eq!(handle.get_history(10).len(), 2);
    }

    #[tokio::test]
    async fn add_message_preserves_client_message_id_through_jsonl_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let key = SessionKey::new("api", "web-cmid-test");

        // First handle: persist a user message tagged with a client_message_id.
        {
            let mut handle = SessionHandle::open(tmp.path(), &key);
            let user_msg = Message::user("hi there").with_client_message_id("cmid-xyz");
            let seq = handle.add_message_with_seq(user_msg).await.unwrap();
            assert_eq!(seq, 0);
        }

        // Reopen the handle: it should reload from JSONL and the
        // client_message_id field must survive the disk round-trip.
        {
            let handle = SessionHandle::open(tmp.path(), &key);
            let history = handle.get_history(10);
            assert_eq!(history.len(), 1);
            assert_eq!(history[0].content, "hi there");
            assert_eq!(
                history[0].client_message_id.as_deref(),
                Some("cmid-xyz"),
                "client_message_id must survive append-and-reload"
            );
        }
    }

    #[test]
    fn test_session_handle_task_state_path_uses_sidecar_file() {
        let tmp = TempDir::new().unwrap();
        let key = SessionKey::new("api", "web-task-ledger");
        let handle = SessionHandle::open(tmp.path(), &key);

        let path = handle.task_state_path();
        assert!(path.ends_with("default.tasks.jsonl"));
        assert_eq!(
            path.parent().unwrap(),
            handle.session_path().parent().unwrap()
        );
    }

    #[test]
    fn test_child_session_key_derivation_is_stable() {
        let parent = SessionKey::new("api", "web-task-ledger");
        let child = child_session_key(&parent, "spawn-01/alpha beta");

        assert_eq!(child.0, "api:web-task-ledger#child-spawn-01%2Falpha%20beta");
        assert_eq!(child.base_key(), "api:web-task-ledger");
        assert_eq!(child.topic(), Some("child-spawn-01%2Falpha%20beta"));
    }

    /// M8.6: `sanitize_loaded_messages` replaces the session's in-memory
    /// transcript with the cleaned-up version and returns the report. No
    /// disk state is touched until the caller rewrites.
    #[test]
    fn should_sanitize_loaded_messages_in_place() {
        use octos_core::ToolCall;

        let tmp = TempDir::new().unwrap();
        let key = SessionKey::new("api", "resume-test");
        let mut handle = SessionHandle::open(tmp.path(), &key);

        // Load an unresolved tool_call + a whitespace-only assistant
        // message into the handle directly.
        handle
            .session
            .messages
            .push(make_message(MessageRole::User, "hi"));
        handle.session.messages.push(Message {
            role: MessageRole::Assistant,
            content: String::new(),
            media: vec![],
            tool_calls: Some(vec![ToolCall {
                id: "unresolved-1".into(),
                name: "shell".into(),
                arguments: serde_json::json!({}),
                metadata: None,
            }]),
            tool_call_id: None,
            reasoning_content: None,
            client_message_id: None,
            timestamp: chrono::Utc::now(),
        });
        handle.session.messages.push(Message {
            role: MessageRole::Assistant,
            content: "   ".into(),
            media: vec![],
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            client_message_id: None,
            timestamp: chrono::Utc::now(),
        });

        let before = handle.session.messages.len();
        let (report, refs) = handle
            .sanitize_loaded_messages(None, None)
            .expect("clean outcome — no workspace root");

        assert_eq!(report.input_len, before);
        assert_eq!(report.unresolved_tool_uses_dropped, 1);
        assert_eq!(report.whitespace_only_dropped, 1);
        assert_eq!(report.output_len, 1);
        assert!(refs.is_empty());
        // Handle was mutated in place.
        assert_eq!(handle.session.messages.len(), 1);
        assert_eq!(handle.session.messages[0].content, "hi");
    }

    /// M8.6: a missing worktree surfaces as `Err` and DOES NOT mutate the
    /// session's in-memory transcript — callers can still log what was
    /// loaded before deciding to refuse resume.
    #[test]
    fn should_preserve_messages_when_worktree_missing() {
        let tmp = TempDir::new().unwrap();
        let key = SessionKey::new("api", "resume-no-worktree");
        let mut handle = SessionHandle::open(tmp.path(), &key);

        handle
            .session
            .messages
            .push(make_message(MessageRole::User, "hi"));
        handle
            .session
            .messages
            .push(make_message(MessageRole::Assistant, "there"));

        let gone = tmp.path().join("ghost-worktree");
        let before_count = handle.session.messages.len();

        let outcome = handle.sanitize_loaded_messages(None, Some(&gone));

        match outcome {
            Err(crate::SanitizeError::WorktreeMissing { path, .. }) => {
                assert_eq!(path, gone);
            }
            other => panic!("expected WorktreeMissing, got {other:?}"),
        }
        // Transcript is preserved.
        assert_eq!(handle.session.messages.len(), before_count);
    }

    // ----------------------------------------------------------------------
    // Item 3 of OCTOS_M8_FIX_FIRST_CHECKLIST_2026-04-24:
    // worktree-missing must be a hard resume refusal. The session actor
    // calls `clear_messages_for_unsafe_resume()` on Err so the in-memory
    // transcript cannot be silently consumed by the first LLM call.
    // ----------------------------------------------------------------------

    #[test]
    fn session_actor_refuses_resume_when_worktree_missing() {
        // Top-level session whose worktree was cleaned up. After the actor
        // clears the in-memory transcript, the handle must look like a
        // fresh session.
        let tmp = TempDir::new().unwrap();
        let key = SessionKey::new("api", "top-level-refusal");
        let mut handle = SessionHandle::open(tmp.path(), &key);
        handle
            .session
            .messages
            .push(make_message(MessageRole::User, "do thing"));
        handle.session.messages.push(make_message(
            MessageRole::Assistant,
            "I'll start working on it",
        ));

        // Step 1: sanitize sees a missing worktree and returns Err.
        let gone = tmp.path().join("ghost-worktree");
        let outcome = handle.sanitize_loaded_messages(None, Some(&gone));
        assert!(matches!(
            outcome,
            Err(crate::SanitizeError::WorktreeMissing { .. })
        ));

        // Step 2: session_actor responds with a hard refusal.
        assert!(
            !handle.is_child_session(),
            "test fixture is top-level (no parent_key)"
        );
        handle.clear_messages_for_unsafe_resume();
        assert_eq!(
            handle.session.messages.len(),
            0,
            "top-level worktree-missing refusal must drop the in-memory transcript"
        );
    }

    #[test]
    fn session_actor_does_not_continue_with_unsanitized_transcript_on_worktree_missing() {
        // The legacy "warn and continue" branch left the original
        // transcript in `handle.session.messages` so the next LLM call
        // would see unresolved tool_calls / orphan thinking. Verify the
        // post-clear state is empty.
        let tmp = TempDir::new().unwrap();
        let key = SessionKey::new("api", "no-unsafe-llm-call");
        let mut handle = SessionHandle::open(tmp.path(), &key);
        // Add a transcript that previously would have been consumed unsafely:
        //   user → assistant with unresolved tool_call (no matching Tool result).
        handle
            .session
            .messages
            .push(make_message(MessageRole::User, "go"));
        handle.session.messages.push(Message {
            role: MessageRole::Assistant,
            content: String::new(),
            media: vec![],
            tool_calls: Some(vec![octos_core::ToolCall {
                id: "unresolved-1".into(),
                name: "shell".into(),
                arguments: serde_json::json!({}),
                metadata: None,
            }]),
            tool_call_id: None,
            reasoning_content: None,
            client_message_id: None,
            timestamp: Utc::now(),
        });

        let gone = tmp.path().join("ghost-worktree");
        let outcome = handle.sanitize_loaded_messages(None, Some(&gone));
        assert!(matches!(
            outcome,
            Err(crate::SanitizeError::WorktreeMissing { .. })
        ));
        // Even though the sanitizer DID NOT mutate, the actor's hard
        // refusal must clear before any consumer reads `messages()`.
        handle.clear_messages_for_unsafe_resume();
        assert!(
            handle.session.messages.is_empty(),
            "no first LLM call must be made using the unsafe transcript"
        );
    }

    #[test]
    fn background_child_session_marks_failed_when_worktree_missing() {
        // A child session has parent_key set. The session actor uses
        // `is_child_session()` to drive a "mark task failed" decision in
        // the supervisor (top-level decision is "drop transcript and
        // start fresh"). The state-clear is the same on both branches —
        // the difference is the operator-visible signal. We verify the
        // parent linkage flows through here so the actor can branch on it.
        let tmp = TempDir::new().unwrap();
        let parent = SessionKey::new("api", "parent-task");
        let child = SessionKey::new("api", "parent-task#child-job-01");
        let mut child_handle = SessionHandle::open(tmp.path(), &child);
        child_handle.session.parent_key = Some(parent.clone());
        child_handle
            .session
            .messages
            .push(make_message(MessageRole::User, "run"));

        assert!(
            child_handle.is_child_session(),
            "child session must report is_child_session=true"
        );

        let gone = tmp.path().join("ghost-child-worktree");
        let outcome = child_handle.sanitize_loaded_messages(None, Some(&gone));
        assert!(matches!(
            outcome,
            Err(crate::SanitizeError::WorktreeMissing { .. })
        ));
        child_handle.clear_messages_for_unsafe_resume();
        assert_eq!(
            child_handle.session.messages.len(),
            0,
            "child worktree-missing refusal must also clear the unsafe transcript"
        );
        // Parent linkage survives the clear so the supervisor can find
        // the parent on its mark-failed lookup.
        assert_eq!(child_handle.session.parent_key, Some(parent));
    }

    /// Helper to build the legacy-flat path for a key.
    fn legacy_session_path(data_dir: &Path, key: &SessionKey) -> PathBuf {
        SessionManager::session_path_static(&data_dir.join("sessions"), key)
    }

    /// Helper to build the per-user session path for a key.
    fn per_user_session_path(data_dir: &Path, key: &SessionKey) -> PathBuf {
        let encoded_base = encode_path_component(key.base_key());
        let topic = key.topic().unwrap_or("default");
        let encoded_topic = encode_path_component(topic);
        data_dir
            .join("users")
            .join(&encoded_base)
            .join("sessions")
            .join(format!("{encoded_topic}.jsonl"))
    }

    /// Helper to build the migration marker path for a key.
    fn migration_marker_path_for(data_dir: &Path, key: &SessionKey) -> PathBuf {
        let encoded_base = encode_path_component(key.base_key());
        let topic = key.topic().unwrap_or("default");
        let encoded_topic = encode_path_component(topic);
        data_dir
            .join("users")
            .join(&encoded_base)
            .join("sessions")
            .join(format!(".migrated.{encoded_topic}"))
    }

    /// Write a minimal JSONL with one user message at `path`.
    fn write_jsonl_with_one_user_message(path: &Path, key: &SessionKey, content: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let meta = serde_json::json!({
            "schema_version": 1,
            "session_key": key.0,
            "topic": key.topic(),
            "created_at": Utc::now(),
            "updated_at": Utc::now(),
        });
        let msg = make_message(MessageRole::User, content);
        let body = format!(
            "{}\n{}\n",
            serde_json::to_string(&meta).unwrap(),
            serde_json::to_string(&msg).unwrap()
        );
        std::fs::write(path, body).unwrap();
    }

    #[test]
    fn migration_marker_skips_redundant_migration_when_present() {
        // Pre-condition: a per-user JSONL exists, the legacy flat file ALSO
        // exists (e.g. from a stale prior boot), and the per-key migration
        // marker is present — meaning a previous open already migrated and
        // confirmed remove. On `SessionHandle::open` we must skip the legacy
        // load AND the legacy delete entirely: the marker is the authoritative
        // signal that migration completed, so the per-user file wins and the
        // stale legacy file is left untouched (a separate operator cleanup
        // can remove it).
        let tmp = TempDir::new().unwrap();
        let key = SessionKey::new("api", "marker-skip");

        // 1) Per-user JSONL with the canonical content.
        let per_user_path = per_user_session_path(tmp.path(), &key);
        write_jsonl_with_one_user_message(&per_user_path, &key, "canonical");

        // 2) Stale legacy file with DIFFERENT content. If migration runs
        //    redundantly it would overwrite the per-user file with this
        //    legacy content — the test catches that.
        let legacy_path = legacy_session_path(tmp.path(), &key);
        write_jsonl_with_one_user_message(&legacy_path, &key, "STALE-LEGACY");

        // 3) Migration marker present.
        let marker = migration_marker_path_for(tmp.path(), &key);
        std::fs::create_dir_all(marker.parent().unwrap()).unwrap();
        std::fs::write(&marker, b"migrated-from-flat\n").unwrap();

        // Open the handle — must skip legacy entirely.
        let handle = SessionHandle::open(tmp.path(), &key);
        let session = handle.session();

        assert_eq!(
            session.messages.len(),
            1,
            "marker present: must load per-user only, not merge legacy"
        );
        assert_eq!(
            session.messages[0].content, "canonical",
            "marker present: per-user content must win, legacy must NOT overwrite"
        );

        // Stale legacy file must remain untouched (we didn't delete it).
        assert!(
            legacy_path.exists(),
            "marker present + stale legacy: legacy file must remain (no redundant remove)"
        );
        // Marker still present.
        assert!(marker.exists(), "marker must remain after open");
    }

    #[test]
    fn migration_retries_legacy_remove_when_marker_absent_but_per_user_exists() {
        // Pre-condition: a previous open succeeded `rewrite_blocking` (per-user
        // file written) but `remove_file(legacy)` failed (transient errno) —
        // so we ended up with both files on disk and NO marker. The next open
        // must detect this partial-migration shape and best-effort RETRY the
        // legacy removal. On success the marker is written.
        let tmp = TempDir::new().unwrap();
        let key = SessionKey::new("api", "marker-retry");

        // 1) Per-user JSONL exists — canonical state.
        let per_user_path = per_user_session_path(tmp.path(), &key);
        write_jsonl_with_one_user_message(&per_user_path, &key, "canonical");

        // 2) Legacy file ALSO exists (the failed-remove leftover).
        let legacy_path = legacy_session_path(tmp.path(), &key);
        write_jsonl_with_one_user_message(&legacy_path, &key, "legacy-leftover");

        // 3) Marker is ABSENT.
        let marker = migration_marker_path_for(tmp.path(), &key);
        assert!(
            !marker.exists(),
            "precondition: marker must not exist for this case"
        );

        // Open the handle — must retry the legacy removal.
        let _handle = SessionHandle::open(tmp.path(), &key);

        assert!(
            !legacy_path.exists(),
            "legacy file must be removed by the retry-on-open path"
        );
        assert!(
            marker.exists(),
            "marker must be written after the retry succeeds"
        );
    }
}
