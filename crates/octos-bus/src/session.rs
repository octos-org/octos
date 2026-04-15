//! Session management with JSONL persistence and LRU eviction.

use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use eyre::Result;
use lru::LruCache;
use octos_core::{Message, SessionKey};
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

/// Current schema version for session JSONL files.
const CURRENT_SESSION_SCHEMA: u32 = 1;

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

fn default_session_schema() -> u32 {
    CURRENT_SESSION_SCHEMA
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

    /// List all sessions (ID + message count) from disk.
    ///
    /// Scans both the legacy flat layout (`sessions/*.jsonl`) and the per-user
    /// layout (`users/{base_key}/sessions/{topic}.jsonl`).
    /// Counts lines efficiently using `BufRead` to avoid loading entire files.
    pub fn list_sessions(&self) -> Vec<(String, usize)> {
        let mut seen = std::collections::HashSet::new();
        let mut result = Vec::new();

        // 1. Legacy flat layout: data/sessions/*.jsonl
        if let Ok(entries) = std::fs::read_dir(&self.sessions_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                    if let Some(name) = path.file_stem().and_then(|n| n.to_str()) {
                        let count = Self::count_lines(&path);
                        let decoded = Self::decode_filename(name);
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
        self.append_to_disk(key, &message).await?;
        let session = self.get_or_create(key).await;
        session.messages.push(message);
        session.updated_at = Utc::now();
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

                    for message in flat
                        .messages
                        .into_iter()
                        .chain(per_user.messages.into_iter())
                    {
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

        tokio::task::spawn_blocking(move || {
            use std::io::Write;
            let tmp_path = path.with_extension("jsonl.tmp");
            let mut file = std::fs::File::create(&tmp_path)?;
            file.write_all(content.as_bytes())?;
            file.flush()?;
            // Atomic rename (on same filesystem)
            std::fs::rename(&tmp_path, &path)?;
            Ok::<_, eyre::Report>(())
        })
        .await
        .map_err(|e| eyre::eyre!("spawn_blocking join error: {e}"))??;

        debug!(key = %key_display, messages = msg_count, "Rewrote session to disk");
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

    /// Clear a session's history (both in-memory and on disk).
    pub async fn clear(&mut self, key: &SessionKey) -> Result<()> {
        self.cache.pop(&key.0);
        let path = self.session_path(key);
        if path.exists() {
            tokio::fs::remove_file(&path).await?;
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

        // Try loading from new per-user path first
        let topic_filename = Self::topic_filename(key);
        let new_path = user_sessions_dir.join(&topic_filename);

        let session = if new_path.exists() {
            Self::load_from_file(&new_path, key)
        } else {
            // Fall back to legacy flat path for migration
            let legacy_dir = data_dir.join("sessions");
            let legacy_path = SessionManager::session_path_static(&legacy_dir, key);
            if legacy_path.exists() {
                debug!(key = %key, "migrating session from legacy flat layout");
                let session = Self::load_from_file(&legacy_path, key);
                // Migration: if loaded from legacy, we'll write to new path on next save
                if session.is_some() {
                    // Remove legacy file after successful load
                    let _ = std::fs::remove_file(&legacy_path);
                }
                session
            } else {
                None
            }
        }
        .unwrap_or_else(|| Session::new(key.clone()));

        Self {
            sessions_dir: user_sessions_dir,
            session,
        }
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

    /// Get the most recent N messages from history.
    pub fn get_history(&self, max: usize) -> &[Message] {
        self.session.get_history(max)
    }

    /// Get or initialize the session (always returns a reference).
    pub fn get_or_create(&mut self) -> &mut Session {
        &mut self.session
    }

    /// Add a message to the session and persist it.
    pub async fn add_message(&mut self, message: Message) -> Result<()> {
        self.add_message_with_seq(message).await.map(|_| ())
    }

    /// Add a message to the session, persist it, and return its committed sequence.
    pub async fn add_message_with_seq(&mut self, message: Message) -> Result<usize> {
        self.session.messages.push(message.clone());
        self.session.updated_at = Utc::now();
        self.append_to_disk(&message).await?;
        Ok(self.session.messages.len().saturating_sub(1))
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

        tokio::task::spawn_blocking(move || {
            use std::io::Write;
            let tmp_path = path.with_extension("jsonl.tmp");
            let mut file = std::fs::File::create(&tmp_path)?;
            file.write_all(content.as_bytes())?;
            file.flush()?;
            std::fs::rename(&tmp_path, &path)?;
            Ok::<_, eyre::Report>(())
        })
        .await
        .map_err(|e| eyre::eyre!("spawn_blocking join error: {e}"))??;

        debug!(key = %key_display, messages = msg_count, "Rewrote session to disk");
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

    /// Append a single message to the JSONL file.
    async fn append_to_disk(&self, message: &Message) -> Result<()> {
        let path = self.session_path();
        let parent_key = self.session.parent_key.as_ref().map(|k| k.0.clone());
        let topic = self.session.topic.clone();
        let summary = self.session.summary.clone();
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
            });
        }

        entries.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        entries
    }

    /// Update the summary field for a session (rewrites metadata line).
    pub async fn update_summary(&mut self, key: &SessionKey, summary: String) -> Result<()> {
        let session = self.get_or_create(key).await;
        session.summary = Some(summary);
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

        entries.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
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
            });
        }

        entries.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
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
}
