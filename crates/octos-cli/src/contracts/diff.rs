//! Pending diff-preview store with optional disk-backed durability.
//!
//! ## Durability model
//!
//! Mirrors the M9.6 ledger durability pattern (`ui_protocol_ledger.rs`,
//! see `docs/M9-LEDGER-DURABILITY-ADR.md`). Each preview is held in RAM
//! for fast `diff/preview/get`, and — when configured with a `data_dir` —
//! also written ahead-of-time to a per-session append-only JSON-Lines
//! log under
//! `<data_dir>/ui-protocol/<safe_session_id>/diff-preview-<epoch_micros>-<pid>.log`.
//!
//! Live insert flow:
//!
//! 1. Caller invokes [`PendingDiffPreviewStore::insert_with_snapshot`]
//!    (or any of the wrappers that funnel into it).
//! 2. The store opens the active per-session log file (creating it on
//!    first use), serializes a [`DiffPreviewDiskRecord`], appends one
//!    line, flushes, then updates the in-memory map. Disk write happens
//!    inside the lock so two concurrent inserts cannot interleave bytes
//!    in the file.
//! 3. The caller is then free to ship the corresponding
//!    `approval/requested` notification on the wire — by the time
//!    `insert_with_snapshot` returns, the preview is durable.
//!
//! Recovery:
//!
//! - At startup, [`PendingDiffPreviewStore::recover`] scans
//!   `<data_dir>/ui-protocol/`. For each session directory it streams
//!   every `diff-preview-*.log` file in lexical (= chronological) order
//!   and replays each record into the in-memory map. Same-`preview_id`
//!   re-inserts collapse to the latest write (HashMap insert semantics).
//!   Malformed lines and unknown schema versions are skipped with a
//!   warning so a single corrupted line does not poison recovery.
//!
//! Eviction:
//!
//! - Per-session entries: capped at `entries_per_session` (default
//!   4096). Oldest insertions are dropped from RAM but remain on disk
//!   until the log is rotated or the directory is purged.
//! - Active sessions: capped at `active_session_cap` (default 1024).
//!   Exceeding the cap evicts the LRU session from RAM (its disk file
//!   stays for replay-after-restart).
//! - Idle TTL is configured but enforced lazily — the v1 cut does not
//!   spawn a background sweep (diff previews are fewer per session than
//!   ledger events, so this is acceptable). Filed as a v2 follow-up.
//!
//! Counters (snapshotted via [`PendingDiffPreviewStore::metrics`]):
//!
//! - `diff_preview.entries.active`
//! - `diff_preview.bytes.in_memory`
//! - `diff_preview.bytes.on_disk`
//! - `diff_preview.recovery.entries_loaded`
//! - `diff_preview.eviction.dropped`

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use octos_core::SessionKey;
use octos_core::ui_protocol::{
    DiffPreview, DiffPreviewFile, DiffPreviewFileStatus, DiffPreviewGetParams,
    DiffPreviewGetResult, DiffPreviewGetStatus, DiffPreviewHunk, DiffPreviewLine,
    DiffPreviewLineKind, DiffPreviewSource, PreviewId, RpcError, TurnId, UiFileMutationNotice,
    file_mutation_operations, methods, rpc_error_codes,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::{debug, info, warn};

/// A pending diff-preview entry. Carries both the parsed `DiffPreview` that
/// is shipped to clients and a raw snapshot of the underlying diff bytes
/// captured *at proposal time*. Storing the snapshot in the entry closes
/// the TOCTOU between proposal and apply: subsequent `diff/preview/get`
/// calls return the proposal-time view even if the file on disk has been
/// rewritten between proposal and approval.
#[derive(Debug, Clone)]
pub(crate) struct PendingDiffEntry {
    preview: DiffPreview,
    /// Raw unified diff captured at proposal time. `None` when the
    /// runtime did not surface a diff at all (e.g. tool emitted no
    /// `diff` and `materialize_file_mutation_diff` could not produce one).
    /// Used by apply-time consistency checks once the apply path is wired
    /// in; today it is persisted to disk and measured by
    /// `approx_entry_bytes` on every insert.
    snapshot_at_proposal: Option<String>,
}

impl PendingDiffEntry {
    fn new(preview: DiffPreview, snapshot: Option<String>) -> Self {
        Self {
            preview,
            snapshot_at_proposal: snapshot,
        }
    }
}

// ---------- Disk record ----------

/// On-disk schema version. Bump when the record shape changes
/// incompatibly. Recovery skips records with unknown versions and logs
/// a warning rather than crashing.
const DIFF_PREVIEW_DISK_VERSION: u32 = 1;

#[derive(Debug, Serialize, Deserialize)]
struct DiffPreviewDiskRecord {
    v: u32,
    /// Microseconds since UNIX_EPOCH at write time. Used for ordering
    /// across log files (the filename also encodes a timestamp, but
    /// this stamp survives clock skew between rotations).
    ts: u128,
    preview: DiffPreview,
    #[serde(skip_serializing_if = "Option::is_none")]
    snapshot_at_proposal: Option<String>,
}

// ---------- Configuration ----------

#[derive(Debug, Clone)]
pub(crate) struct DiffPreviewConfig {
    /// Maximum entries retained in RAM per session. Older entries are
    /// dropped from RAM but stay on disk for replay-after-restart.
    pub entries_per_session: usize,
    /// Maximum active sessions held in RAM. Exceeding this cap evicts
    /// the LRU session.
    pub active_session_cap: usize,
    /// Idle TTL — sessions untouched for this long are eligible for
    /// eviction. v1 does not run an automatic sweep; configured for
    /// observability and v2 use.
    #[allow(dead_code)]
    pub idle_ttl: Duration,
    /// `None` ⇒ RAM-only (legacy behaviour, used by tests and
    /// connections without a sessions manager).
    pub data_dir: Option<PathBuf>,
}

impl DiffPreviewConfig {
    pub(crate) fn ephemeral() -> Self {
        Self {
            entries_per_session: 4096,
            active_session_cap: 1024,
            idle_ttl: Duration::from_secs(60 * 60),
            data_dir: None,
        }
    }

    pub(crate) fn durable(data_dir: PathBuf) -> Self {
        Self {
            entries_per_session: 4096,
            active_session_cap: 1024,
            idle_ttl: Duration::from_secs(60 * 60),
            data_dir: Some(data_dir),
        }
    }
}

impl Default for DiffPreviewConfig {
    fn default() -> Self {
        Self::ephemeral()
    }
}

// ---------- Per-session state ----------

#[derive(Debug)]
struct SessionState {
    /// Insertion order of preview ids in this session — used to enforce
    /// `entries_per_session`. A re-insert of the same id moves it to
    /// the back (treated as a fresh insert). The HashMap holds the
    /// authoritative entry; this deque is just an LRU index.
    insertion_order: VecDeque<PreviewId>,
    /// Approximate JSON byte size of in-memory entries owned by this
    /// session. Used for `diff_preview.bytes.in_memory`.
    in_memory_bytes: usize,
    last_touched_at: Instant,
    /// Active log file for this session (`None` when RAM-only).
    active_log_path: Option<PathBuf>,
}

impl SessionState {
    fn new() -> Self {
        Self {
            insertion_order: VecDeque::new(),
            in_memory_bytes: 0,
            last_touched_at: Instant::now(),
            active_log_path: None,
        }
    }
}

// ---------- Store ----------

struct StoreInner {
    /// `(session_id, preview_id)` is the global key. We keep two views:
    ///
    /// - `entries` for fast `get`-by-`preview_id` (the wire path).
    ///   The session_id check is enforced inside `get()` to keep the
    ///   wire semantics — a known preview_id from the wrong session is
    ///   reported as `unknown_preview` (existing contract).
    /// - `sessions` for per-session bookkeeping, eviction, and disk
    ///   layout.
    entries: HashMap<PreviewId, PendingDiffEntry>,
    /// Reverse index: which session does this preview belong to? Avoids
    /// scanning every session's deque on `entries_per_session`-driven
    /// eviction.
    preview_session: HashMap<PreviewId, SessionKey>,
    sessions: HashMap<SessionKey, SessionState>,
    /// LRU order: front is most-recently-touched, back is least.
    lru: VecDeque<SessionKey>,
    /// Process-lifetime aggregate counters.
    on_disk_bytes: u64,
    dropped_count: u64,
    evicted_count: u64,
    recovery_loaded_count: u64,
    /// Total diff-preview records skipped during recovery (unknown
    /// version + genuinely-malformed + session-id mismatch). Surfaced so
    /// operators/tests can observe the aggregated per-file skip volume
    /// deterministically without depending on the tracing subscriber.
    recovery_skipped_count: u64,
}

impl StoreInner {
    fn new() -> Self {
        Self {
            entries: HashMap::new(),
            preview_session: HashMap::new(),
            sessions: HashMap::new(),
            lru: VecDeque::new(),
            on_disk_bytes: 0,
            dropped_count: 0,
            evicted_count: 0,
            recovery_loaded_count: 0,
            recovery_skipped_count: 0,
        }
    }

    fn touch_lru(&mut self, session_id: &SessionKey) {
        if let Some(idx) = self.lru.iter().position(|key| key == session_id) {
            self.lru.remove(idx);
        }
        self.lru.push_front(session_id.clone());
    }
}

pub(crate) struct PendingDiffPreviewStore {
    config: DiffPreviewConfig,
    inner: Mutex<StoreInner>,
}

impl Default for PendingDiffPreviewStore {
    fn default() -> Self {
        Self::with_config(DiffPreviewConfig::ephemeral())
    }
}

impl PendingDiffPreviewStore {
    pub(crate) fn with_config(config: DiffPreviewConfig) -> Self {
        if let Some(dir) = &config.data_dir {
            let ui_dir = dir.join("ui-protocol");
            if let Err(error) = fs::create_dir_all(&ui_dir) {
                warn!(
                    target = "octos::diff_preview",
                    ?error,
                    path = %ui_dir.display(),
                    "failed to create ui-protocol data dir; falling back to RAM-only writes"
                );
            }
        }
        Self {
            config,
            inner: Mutex::new(StoreInner::new()),
        }
    }

    /// Build a durable store and replay every on-disk session into RAM.
    ///
    /// Bounded by `config.entries_per_session` per session. A missing
    /// `data_dir` directory is treated as a clean boot (no-op recovery).
    pub(crate) fn recover(config: DiffPreviewConfig) -> RecoveryOutcome {
        let store = Self::with_config(config);
        let Some(dir) = store.config.data_dir.clone() else {
            return RecoveryOutcome {
                store,
                sessions_recovered: 0,
                entries_recovered: 0,
            };
        };
        let ui_dir = dir.join("ui-protocol");
        let entries = match fs::read_dir(&ui_dir) {
            Ok(entries) => entries,
            Err(error) => {
                if error.kind() != std::io::ErrorKind::NotFound {
                    warn!(
                        target = "octos::diff_preview",
                        ?error,
                        path = %ui_dir.display(),
                        "failed to read ui-protocol dir during diff-preview recovery"
                    );
                }
                return RecoveryOutcome {
                    store,
                    sessions_recovered: 0,
                    entries_recovered: 0,
                };
            }
        };

        let mut sessions_recovered = 0usize;
        let mut entries_recovered = 0usize;
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let Some(safe_name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let Some(session_key) = decode_session_dir_name(safe_name) else {
                continue;
            };
            match store.recover_one_session(&session_key, &path) {
                Ok(count) => {
                    if count > 0 {
                        sessions_recovered += 1;
                        entries_recovered += count;
                    }
                }
                Err(error) => {
                    warn!(
                        target = "octos::diff_preview",
                        ?error,
                        session_id = %session_key.0,
                        "failed to recover diff-preview session from disk"
                    );
                }
            }
        }
        if let Ok(mut inner) = store.inner.lock() {
            inner.recovery_loaded_count = entries_recovered as u64;
        }
        info!(
            target = "octos::diff_preview",
            sessions_recovered, entries_recovered, "diff preview recovery complete"
        );
        RecoveryOutcome {
            store,
            sessions_recovered,
            entries_recovered,
        }
    }

    fn recover_one_session(
        &self,
        session_id: &SessionKey,
        session_dir: &Path,
    ) -> std::io::Result<usize> {
        let mut log_files = match list_log_files(session_dir) {
            Ok(log_files) => log_files,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(error) => return Err(error),
        };
        if log_files.is_empty() {
            return Ok(0);
        }
        log_files.sort();

        let mut total_disk_bytes = 0u64;
        for path in &log_files {
            if let Ok(metadata) = fs::metadata(path) {
                total_disk_bytes = total_disk_bytes.saturating_add(metadata.len());
            }
        }
        let active_log_path = log_files.last().expect("non-empty after sort").clone();

        // Iterate every record in chronological order. Same-id
        // re-inserts collapse: the latest write wins (matches the live
        // semantics of `insert_with_snapshot`).
        let mut loaded = 0usize;
        let mut skipped_total = 0u64;
        let mut seen_ids: HashSet<PreviewId> = HashSet::new();
        for path in &log_files {
            let file = match File::open(path) {
                Ok(file) => file,
                Err(error) => {
                    warn!(
                        target = "octos::diff_preview",
                        ?error,
                        path = %path.display(),
                        "failed to open diff-preview log; skipping"
                    );
                    continue;
                }
            };
            let reader = BufReader::new(file);
            // Aggregate skip counts per file: emit ONE summary `warn!`
            // after the inner loop instead of one line per record per
            // rescan. Recovery re-reads every log from byte 0, so
            // per-record warnings multiply into log spam on corrupted or
            // legacy logs. Per-record detail stays at `debug!`.
            let mut skipped_unknown_version = 0u64;
            let mut skipped_malformed = 0u64;
            let mut skipped_session_mismatch = 0u64;
            for line_result in reader.lines() {
                let line = match line_result {
                    Ok(line) => line,
                    Err(error) => {
                        warn!(
                            target = "octos::diff_preview",
                            ?error,
                            session_id = %session_id.0,
                            path = %path.display(),
                            "io error reading diff-preview line; truncating this file here"
                        );
                        break;
                    }
                };
                if line.trim().is_empty() {
                    continue;
                }
                let record = match serde_json::from_str::<DiffPreviewDiskRecord>(&line) {
                    Ok(record) if record.v == DIFF_PREVIEW_DISK_VERSION => record,
                    Ok(record) => {
                        skipped_unknown_version += 1;
                        debug!(
                            target = "octos::diff_preview",
                            version = record.v,
                            path = %path.display(),
                            "skipping diff-preview record with unknown version"
                        );
                        continue;
                    }
                    Err(error) => {
                        skipped_malformed += 1;
                        debug!(
                            target = "octos::diff_preview",
                            ?error,
                            session_id = %session_id.0,
                            path = %path.display(),
                            "skipping malformed diff-preview record"
                        );
                        continue;
                    }
                };
                if record.preview.session_id != *session_id {
                    skipped_session_mismatch += 1;
                    debug!(
                        target = "octos::diff_preview",
                        record_session_id = %record.preview.session_id.0,
                        dir_session_id = %session_id.0,
                        path = %path.display(),
                        "skipping diff-preview record whose session_id mismatches its dir"
                    );
                    continue;
                }
                let preview_id = record.preview.preview_id.clone();
                self.hydrate_one(
                    session_id,
                    &active_log_path,
                    PendingDiffEntry::new(record.preview, record.snapshot_at_proposal),
                );
                if seen_ids.insert(preview_id) {
                    loaded += 1;
                }
            }
            let skipped_in_file =
                skipped_unknown_version + skipped_malformed + skipped_session_mismatch;
            if skipped_in_file > 0 {
                skipped_total = skipped_total.saturating_add(skipped_in_file);
                warn!(
                    target = "octos::diff_preview",
                    session_id = %session_id.0,
                    path = %path.display(),
                    skipped = skipped_in_file,
                    skipped_unknown_version,
                    skipped_malformed,
                    skipped_session_mismatch,
                    "skipped diff-preview records during scan (aggregated)"
                );
            }
        }

        if loaded > 0 || skipped_total > 0 {
            let mut inner = self.inner.lock().expect("diff preview store poisoned");
            if loaded > 0 {
                inner.on_disk_bytes = inner.on_disk_bytes.saturating_add(total_disk_bytes);
                inner.touch_lru(session_id);
            }
            inner.recovery_skipped_count =
                inner.recovery_skipped_count.saturating_add(skipped_total);
        }
        Ok(loaded)
    }

    fn hydrate_one(
        &self,
        session_id: &SessionKey,
        active_log_path: &Path,
        entry: PendingDiffEntry,
    ) {
        let bytes = approx_entry_bytes(&entry);
        let preview_id = entry.preview.preview_id.clone();
        let mut inner = self.inner.lock().expect("diff preview store poisoned");

        // Replace any existing entry for this preview_id (latest-wins).
        if let Some(_old) = inner.entries.remove(&preview_id) {
            // Remove from previous session's order, if different.
            if let Some(prev_session) = inner.preview_session.remove(&preview_id) {
                if let Some(state) = inner.sessions.get_mut(&prev_session) {
                    if let Some(pos) = state.insertion_order.iter().position(|p| p == &preview_id) {
                        state.insertion_order.remove(pos);
                    }
                }
            }
        }

        let session = inner
            .sessions
            .entry(session_id.clone())
            .or_insert_with(SessionState::new);
        if session.active_log_path.is_none() {
            session.active_log_path = Some(active_log_path.to_path_buf());
        }
        session.in_memory_bytes = session.in_memory_bytes.saturating_add(bytes);
        session.insertion_order.push_back(preview_id.clone());
        session.last_touched_at = Instant::now();

        inner
            .preview_session
            .insert(preview_id.clone(), session_id.clone());
        inner.entries.insert(preview_id, entry);
    }

    pub(crate) fn get(
        &self,
        params: DiffPreviewGetParams,
    ) -> Result<DiffPreviewGetResult, RpcError> {
        let inner = self.inner.lock().expect("diff preview store poisoned");
        let Some(entry) = inner.entries.get(&params.preview_id) else {
            return Err(diff_preview_not_found_error(&params));
        };

        if entry.preview.session_id != params.session_id {
            return Err(diff_preview_not_found_error(&params));
        }

        Ok(DiffPreviewGetResult {
            status: DiffPreviewGetStatus::Ready,
            source: DiffPreviewSource::PendingStore,
            preview: entry.preview.clone(),
        })
    }

    pub(crate) fn insert_with_snapshot(
        &self,
        preview: DiffPreview,
        snapshot_at_proposal: Option<String>,
    ) {
        let session_id = preview.session_id.clone();
        let preview_id = preview.preview_id.clone();
        let entry = PendingDiffEntry::new(preview, snapshot_at_proposal);

        let mut inner = self.inner.lock().expect("diff preview store poisoned");

        // Active-session LRU eviction: if this is a brand-new session
        // and we'd exceed the cap, evict the LRU first.
        let is_new_session = !inner.sessions.contains_key(&session_id);
        if is_new_session && inner.sessions.len() >= self.config.active_session_cap {
            self.evict_lru_locked(&mut inner);
        }

        // Disk write-ahead. The file write happens BEFORE we update
        // the in-memory map so a crash between disk-commit and
        // RAM-update leaves the entry recoverable. The `&mut session`
        // borrow is scoped to this block so we can later touch
        // `inner.entries` to evict displaced entries.
        let on_disk_delta = if self.config.data_dir.is_some() {
            let session = inner
                .sessions
                .entry(session_id.clone())
                .or_insert_with(SessionState::new);
            match self.write_record_locked(&session_id, session, &entry) {
                Ok(written) => written as i64,
                Err(error) => {
                    warn!(
                        target = "octos::diff_preview",
                        ?error,
                        session_id = %session_id.0,
                        preview_id = %entry.preview.preview_id.0,
                        "failed to append diff preview record to disk; in-memory only"
                    );
                    0
                }
            }
        } else {
            // Ensure the session entry exists so subsequent borrows
            // don't have to re-create it on the RAM-only path.
            inner
                .sessions
                .entry(session_id.clone())
                .or_insert_with(SessionState::new);
            0
        };

        let bytes = approx_entry_bytes(&entry);
        if let Some(state) = inner.sessions.get_mut(&session_id) {
            state.last_touched_at = Instant::now();
        }

        // If the same preview_id already exists, treat this as
        // overwrite-in-place — drop the previous bytes from the
        // session counter and remove the old position in the deque.
        let displaced_bytes = if let Some(old) = inner.entries.remove(&preview_id) {
            // The old entry might belong to a different session; in that
            // (very unlikely) case clean its session bookkeeping too.
            let old_session_key = inner.preview_session.remove(&preview_id);
            let old_bytes = approx_entry_bytes(&old);
            if let Some(prev_session) = old_session_key {
                if prev_session == session_id {
                    if let Some(state) = inner.sessions.get_mut(&prev_session) {
                        if let Some(pos) =
                            state.insertion_order.iter().position(|p| p == &preview_id)
                        {
                            state.insertion_order.remove(pos);
                        }
                        state.in_memory_bytes = state.in_memory_bytes.saturating_sub(old_bytes);
                    }
                    old_bytes
                } else {
                    if let Some(state) = inner.sessions.get_mut(&prev_session) {
                        if let Some(pos) =
                            state.insertion_order.iter().position(|p| p == &preview_id)
                        {
                            state.insertion_order.remove(pos);
                        }
                        state.in_memory_bytes = state.in_memory_bytes.saturating_sub(old_bytes);
                    }
                    0
                }
            } else {
                0
            }
        } else {
            0
        };

        // Re-borrow session after the displacement bookkeeping above
        // and push the new id. We collect any eviction victims into a
        // buffer (rather than walking `session` while we mutate
        // `inner`) so the borrow checker can verify the disjointness.
        let cap = self.config.entries_per_session;
        let mut victims: Vec<PreviewId> = Vec::new();
        {
            let session = inner
                .sessions
                .entry(session_id.clone())
                .or_insert_with(SessionState::new);
            session.in_memory_bytes = session
                .in_memory_bytes
                .saturating_add(bytes)
                .saturating_sub(displaced_bytes.min(bytes));
            session.insertion_order.push_back(preview_id.clone());
            while session.insertion_order.len() > cap {
                if let Some(victim) = session.insertion_order.pop_front() {
                    victims.push(victim);
                } else {
                    break;
                }
            }
        }

        // Per-session entry cap: drop the oldest from RAM (still on
        // disk) until we're within the cap. Done after dropping the
        // `&mut session` borrow above so we can touch `inner.entries`.
        let mut dropped_now = 0u64;
        for victim in victims {
            if let Some(old_entry) = inner.entries.remove(&victim) {
                let old_bytes = approx_entry_bytes(&old_entry);
                if let Some(state) = inner.sessions.get_mut(&session_id) {
                    state.in_memory_bytes = state.in_memory_bytes.saturating_sub(old_bytes);
                }
                inner.preview_session.remove(&victim);
                dropped_now += 1;
            }
        }

        inner
            .preview_session
            .insert(preview_id.clone(), session_id.clone());
        inner.entries.insert(preview_id, entry);
        inner.dropped_count = inner.dropped_count.saturating_add(dropped_now);
        if on_disk_delta >= 0 {
            inner.on_disk_bytes = inner.on_disk_bytes.saturating_add(on_disk_delta as u64);
        }
        inner.touch_lru(&session_id);
    }

    fn write_record_locked(
        &self,
        session_id: &SessionKey,
        session: &mut SessionState,
        entry: &PendingDiffEntry,
    ) -> std::io::Result<u64> {
        let Some(dir) = &self.config.data_dir else {
            return Ok(0);
        };
        let session_dir = dir
            .join("ui-protocol")
            .join(encode_session_dir_name(session_id));
        if session.active_log_path.is_none() {
            fs::create_dir_all(&session_dir)?;
            session.active_log_path = Some(session_dir.join(new_log_file_name()));
        }
        let path = session
            .active_log_path
            .clone()
            .expect("active log path set above");

        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_micros())
            .unwrap_or(0);
        let record = DiffPreviewDiskRecord {
            v: DIFF_PREVIEW_DISK_VERSION,
            ts,
            preview: entry.preview.clone(),
            snapshot_at_proposal: entry.snapshot_at_proposal.clone(),
        };
        let line = serde_json::to_string(&record).map_err(std::io::Error::other)?;
        let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
        let bytes = line.len() as u64 + 1; // newline
        let mut writer = BufWriter::with_capacity(8192, &mut file);
        writer.write_all(line.as_bytes())?;
        writer.write_all(b"\n")?;
        writer.flush()?;
        // We rely on the OS page cache for durability; fsync per-append
        // is too expensive. Same tradeoff as the M9.6 ledger.
        Ok(bytes)
    }

    fn evict_lru_locked(&self, inner: &mut StoreInner) {
        let Some(victim) = inner.lru.pop_back() else {
            return;
        };
        if let Some(state) = inner.sessions.remove(&victim) {
            // Remove every preview_id owned by this session from the
            // global maps. The disk file stays so a future
            // `recover()` (or, when a v2 lazy-rehydrate is added, an
            // on-demand load) can restore it.
            for preview_id in state.insertion_order.iter() {
                inner.entries.remove(preview_id);
                inner.preview_session.remove(preview_id);
            }
            inner.evicted_count = inner.evicted_count.saturating_add(1);
            info!(
                target = "octos::diff_preview",
                session_id = %victim.0,
                cause = "lru_cap",
                evicted_in_memory_bytes = state.in_memory_bytes,
                "diff preview store evicted session from in-memory cache"
            );
        }
    }

    pub(crate) fn upsert_file_mutation(
        &self,
        session_id: SessionKey,
        turn_id: &TurnId,
        notice: &mut UiFileMutationNotice,
        diff: Option<&str>,
    ) -> PreviewId {
        let preview_id = notice
            .preview_id
            .clone()
            .unwrap_or_else(|| preview_id_for_file_mutation(&session_id, turn_id, notice));
        notice.preview_id = Some(preview_id.clone());
        self.insert_with_snapshot(
            preview_from_file_mutation(session_id, preview_id.clone(), notice, diff),
            diff.map(ToOwned::to_owned),
        );
        preview_id
    }
}

/// Outcome of [`PendingDiffPreviewStore::recover`]. The caller wires
/// `store` into the singleton; the counts are useful for the boot log
/// line.
pub(crate) struct RecoveryOutcome {
    pub(crate) store: PendingDiffPreviewStore,
    pub(crate) sessions_recovered: usize,
    pub(crate) entries_recovered: usize,
}

// ---------- Helpers ----------

fn approx_entry_bytes(entry: &PendingDiffEntry) -> usize {
    let preview_bytes = serde_json::to_string(&entry.preview)
        .map(|s| s.len())
        .unwrap_or(0);
    preview_bytes + entry.snapshot_at_proposal.as_ref().map_or(0, |s| s.len())
}

fn new_log_file_name() -> String {
    // Microsecond-precision epoch keeps lexical sort = chronological
    // sort, which the recovery iteration relies on. The pid suffix
    // disambiguates concurrent rotates within the same microsecond.
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let micros = now.as_micros();
    format!(
        "diff-preview-{:020}-{:05}.log",
        micros,
        std::process::id() % 100000
    )
}

fn list_log_files(session_dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for entry in fs::read_dir(session_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_file() {
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if name.starts_with("diff-preview-") && name.ends_with(".log") {
                    out.push(path);
                }
            }
        }
    }
    Ok(out)
}

// ---------- Filename encoding ----------
//
// Mirrors `ui_protocol_ledger.rs::encode_session_dir_name`. SessionKey
// may contain characters illegal on common filesystems (`:`, `/`,
// etc.). Hex-encoding is reversible and collision-free, and lets the
// diff-preview logs co-tenant under the same `<safe_session_id>/` dir
// as the ledger.

fn encode_session_dir_name(session_id: &SessionKey) -> String {
    let mut out = String::with_capacity(session_id.0.len() * 2);
    for byte in session_id.0.as_bytes() {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn decode_session_dir_name(name: &str) -> Option<SessionKey> {
    if name.len() % 2 != 0 || !name.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut bytes = Vec::with_capacity(name.len() / 2);
    for chunk in name.as_bytes().chunks(2) {
        let hi = hex_digit(chunk[0])?;
        let lo = hex_digit(chunk[1])?;
        bytes.push((hi << 4) | lo);
    }
    let s = String::from_utf8(bytes).ok()?;
    Some(SessionKey(s))
}

fn hex_digit(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn preview_id_for_file_mutation(
    session_id: &SessionKey,
    turn_id: &TurnId,
    notice: &UiFileMutationNotice,
) -> PreviewId {
    let mut hash = 0xcbf2_9ce4_8422_2325_u128;
    fn feed(hash: &mut u128, bytes: &[u8]) {
        for byte in bytes {
            *hash ^= u128::from(*byte);
            *hash = hash.wrapping_mul(0x100_0000_01b3);
        }
        *hash ^= 0xff;
        *hash = hash.wrapping_mul(0x100_0000_01b3);
    }

    feed(&mut hash, session_id.0.as_bytes());
    feed(&mut hash, turn_id.0.as_bytes());
    feed(&mut hash, notice.path.as_bytes());
    feed(&mut hash, notice.operation.as_bytes());
    if let Some(tool_call_id) = &notice.tool_call_id {
        feed(&mut hash, tool_call_id.as_bytes());
    }
    PreviewId(uuid::Uuid::from_u128(hash))
}

fn preview_from_file_mutation(
    session_id: SessionKey,
    preview_id: PreviewId,
    notice: &UiFileMutationNotice,
    diff: Option<&str>,
) -> DiffPreview {
    let files = diff
        .and_then(parse_unified_diff_preview_files)
        .filter(|files| !files.is_empty())
        .map(|files| files.into_iter().map(sanitize_preview_file).collect())
        .unwrap_or_else(|| vec![file_from_mutation_notice(notice)]);

    let safe_path = crate::contracts::sanitize::sanitize_display_path(&notice.path);
    DiffPreview {
        session_id,
        preview_id,
        title: Some(format!("{} {}", notice.operation, safe_path)),
        files,
    }
}

fn file_from_mutation_notice(notice: &UiFileMutationNotice) -> DiffPreviewFile {
    DiffPreviewFile {
        path: crate::contracts::sanitize::sanitize_display_path(&notice.path),
        old_path: None,
        status: status_from_operation(&notice.operation),
        hunks: Vec::new(),
    }
}

fn sanitize_preview_file(mut file: DiffPreviewFile) -> DiffPreviewFile {
    file.path = crate::contracts::sanitize::sanitize_display_path(&file.path);
    file.old_path = file
        .old_path
        .map(|path| crate::contracts::sanitize::sanitize_display_path(&path));
    file
}

fn status_from_operation(operation: &str) -> DiffPreviewFileStatus {
    match operation {
        file_mutation_operations::CREATE | file_mutation_operations::WRITE => {
            DiffPreviewFileStatus::Added
        }
        file_mutation_operations::DELETE => DiffPreviewFileStatus::Deleted,
        _ => DiffPreviewFileStatus::Modified,
    }
}

fn parse_unified_diff_preview_files(diff: &str) -> Option<Vec<DiffPreviewFile>> {
    let mut files = Vec::new();
    let mut current: Option<DiffPreviewFile> = None;
    let mut current_hunk: Option<DiffPreviewHunk> = None;
    let mut old_line = 0_u32;
    let mut new_line = 0_u32;
    let mut old_remaining = 0_u32;
    let mut new_remaining = 0_u32;

    for line in diff.lines() {
        // Inside a hunk, consume content strictly by the declared old/new
        // counts: a `--` or `++` line is real content (SQL/Lua/Haskell
        // comments), never a file header. Only when the hunk's declared
        // lines are exhausted may a new file header be recognized (T-0277).
        if current_hunk.is_some() && old_remaining == 0 && new_remaining == 0 {
            push_hunk(&mut current, &mut current_hunk);
        }
        if current_hunk.is_none()
            && let Some(rest) = line.strip_prefix("diff --git ")
        {
            let Some((old_path, new_path)) = parse_diff_git_paths(rest) else {
                tracing::warn!("Unparseable diff file header; showing mutation path without hunks");
                return None;
            };
            push_hunk(&mut current, &mut current_hunk);
            if let Some(file) = current.take() {
                files.push(file);
            }
            current = Some(DiffPreviewFile {
                path: new_path,
                old_path: Some(old_path),
                status: DiffPreviewFileStatus::Modified,
                hunks: Vec::new(),
            });
            continue;
        }

        let Some(file) = current.as_mut() else {
            continue;
        };

        if line.starts_with("new file mode ") {
            file.status = DiffPreviewFileStatus::Added;
        } else if line.starts_with("deleted file mode ") {
            file.status = DiffPreviewFileStatus::Deleted;
        } else if let Some(path) = line.strip_prefix("rename from ") {
            file.old_path = Some(path.to_string());
            file.status = DiffPreviewFileStatus::Renamed;
        } else if let Some(path) = line.strip_prefix("rename to ") {
            file.path = path.to_string();
            file.status = DiffPreviewFileStatus::Renamed;
        } else if current_hunk.is_none() && line.starts_with("@@ ") {
            push_hunk(&mut current, &mut current_hunk);
            let (old_start, new_start, old_count, new_count) =
                parse_hunk_header(line).unwrap_or((1, 1, 1, 1));
            old_line = old_start;
            new_line = new_start;
            old_remaining = old_count;
            new_remaining = new_count;
            current_hunk = Some(DiffPreviewHunk {
                header: line.to_string(),
                lines: Vec::new(),
            });
            if old_remaining == 0 && new_remaining == 0 {
                push_hunk(&mut current, &mut current_hunk);
            }
        } else if let Some(hunk) = current_hunk.as_mut() {
            let Some(first) = line.chars().next() else {
                continue;
            };
            match first {
                '+' => {
                    if new_remaining == 0 {
                        continue;
                    }
                    hunk.lines.push(DiffPreviewLine {
                        kind: DiffPreviewLineKind::Added,
                        content: line[1..].to_string(),
                        old_line: None,
                        new_line: Some(new_line),
                    });
                    new_line += 1;
                    new_remaining -= 1;
                }
                '-' => {
                    if old_remaining == 0 {
                        continue;
                    }
                    hunk.lines.push(DiffPreviewLine {
                        kind: DiffPreviewLineKind::Removed,
                        content: line[1..].to_string(),
                        old_line: Some(old_line),
                        new_line: None,
                    });
                    old_line += 1;
                    old_remaining -= 1;
                }
                ' ' => {
                    if old_remaining == 0 || new_remaining == 0 {
                        continue;
                    }
                    hunk.lines.push(DiffPreviewLine {
                        kind: DiffPreviewLineKind::Context,
                        content: line[1..].to_string(),
                        old_line: Some(old_line),
                        new_line: Some(new_line),
                    });
                    old_line += 1;
                    new_line += 1;
                    old_remaining -= 1;
                    new_remaining -= 1;
                }
                _ => {}
            }
        }
    }

    push_hunk(&mut current, &mut current_hunk);
    if let Some(file) = current {
        files.push(file);
    }
    Some(files)
}

fn parse_diff_git_paths(rest: &str) -> Option<(String, String)> {
    if rest.starts_with('"') {
        let (old_path, rest) = parse_git_quoted_path(rest)?;
        let rest = rest.strip_prefix(' ')?;
        let new_path = if rest.starts_with('"') {
            let (path, tail) = parse_git_quoted_path(rest)?;
            if !tail.is_empty() {
                return None;
            }
            path
        } else {
            rest.to_owned()
        };
        return Some((strip_diff_prefix(&old_path), strip_diff_prefix(&new_path)));
    }

    // Mixed form: an unquoted a/ side (which cannot contain spaces) followed
    // by a C-quoted b/ side. The unquoted split_once below would otherwise
    // chop the quoted path at its first space (T-0249).
    if let Some((old_path, quoted_new)) = rest.split_once(' ')
        && quoted_new.starts_with('"')
        && old_path.starts_with("a/")
    {
        let (new_path, tail) = parse_git_quoted_path(quoted_new)?;
        if !tail.is_empty() {
            return None;
        }
        return Some((strip_diff_prefix(old_path), strip_diff_prefix(&new_path)));
    }

    // Git leaves ordinary spaces unquoted in this header. For the normal
    // non-rename case, select the a/ + b/ split whose two paths are identical;
    // this remains unambiguous even when the path itself contains " b/".
    // Rename headers are corrected by their mandatory rename from/to lines.
    let mut fallback = None;
    for (index, _) in rest.match_indices(" b/") {
        let old_path = &rest[..index];
        let new_path = &rest[index + 1..];
        if !old_path.starts_with("a/") {
            continue;
        }
        let pair = (strip_diff_prefix(old_path), strip_diff_prefix(new_path));
        if pair.0 == pair.1 {
            return Some(pair);
        }
        fallback.get_or_insert(pair);
    }
    fallback
}

/// Decode Git's C-style path quoting, including octal UTF-8 bytes. JSON string
/// decoding is not equivalent: Git emits octal escapes, not `\uXXXX` escapes.
/// Keep control-character paths quoted so display sanitization cannot alias them.
fn parse_git_quoted_path(input: &str) -> Option<(String, &str)> {
    let bytes = input.strip_prefix('"')?.as_bytes();
    let mut decoded = Vec::new();
    let mut index = 0;
    while let Some(&byte) = bytes.get(index) {
        index += 1;
        match byte {
            b'"' => {
                let path = String::from_utf8(decoded).ok()?;
                let path = if path.chars().any(char::is_control) {
                    input[..index + 1].to_owned()
                } else {
                    path
                };
                return Some((path, &input[index + 1..]));
            }
            b'\\' => {
                let escaped = *bytes.get(index)?;
                index += 1;
                decoded.push(match escaped {
                    b'"' | b'\\' => escaped,
                    b'a' => 7,
                    b'b' => 8,
                    b't' => b'\t',
                    b'n' => b'\n',
                    b'v' => 11,
                    b'f' => 12,
                    b'r' => b'\r',
                    b'0'..=b'3' => {
                        let second = *bytes.get(index)?;
                        let third = *bytes.get(index + 1)?;
                        if !(b'0'..=b'7').contains(&second) || !(b'0'..=b'7').contains(&third) {
                            return None;
                        }
                        index += 2;
                        (escaped - b'0') * 64 + (second - b'0') * 8 + (third - b'0')
                    }
                    _ => return None,
                });
            }
            _ => decoded.push(byte),
        }
    }
    None
}

fn strip_diff_prefix(path: &str) -> String {
    path.strip_prefix("a/")
        .or_else(|| path.strip_prefix("b/"))
        .unwrap_or(path)
        .to_string()
}

fn parse_hunk_header(header: &str) -> Option<(u32, u32, u32, u32)> {
    let mut parts = header.split_whitespace();
    parts.next()?;
    let old = parts.next()?.trim_start_matches('-');
    let new = parts.next()?.trim_start_matches('+');
    let (old_start, old_count) = parse_range(old)?;
    let (new_start, new_count) = parse_range(new)?;
    Some((old_start, new_start, old_count, new_count))
}

fn parse_range(range: &str) -> Option<(u32, u32)> {
    let mut parts = range.splitn(2, ',');
    let start = parts.next()?.parse().ok()?;
    let count = match parts.next() {
        Some(c) => c.parse().ok()?,
        None => 1,
    };
    Some((start, count))
}

fn push_hunk(file: &mut Option<DiffPreviewFile>, hunk: &mut Option<DiffPreviewHunk>) {
    if let (Some(file), Some(hunk)) = (file.as_mut(), hunk.take()) {
        file.hunks.push(hunk);
    }
}

fn diff_preview_not_found_error(params: &DiffPreviewGetParams) -> RpcError {
    RpcError::new(
        rpc_error_codes::UNKNOWN_PREVIEW_ID,
        "diff/preview/get target was not found for this session",
    )
    .with_data(json!({
        "kind": "unknown_preview",
        "method": methods::DIFF_PREVIEW_GET,
        "session_id": params.session_id,
        "preview_id": params.preview_id,
        "legacy_kind": "diff_preview_not_found",
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hunk_content_lines_starting_with_dashes_and_pluses_are_not_file_headers() {
        let diff = "diff --git a/f.sql b/f.sql
--- a/f.sql
+++ b/f.sql
@@ -1,2 +1,2 @@
-- drop me
++ new inc
 context
";
        let files = parse_unified_diff_preview_files(diff).expect("parsed");
        assert_eq!(files.len(), 1);
        let file = &files[0];
        assert_eq!(file.path, "f.sql");
        let hunk = &file.hunks[0];
        assert_eq!(hunk.lines.len(), 3, "no content line is swallowed");
        assert_eq!(hunk.lines[0].content, "- drop me");
        assert_eq!(hunk.lines[0].kind, DiffPreviewLineKind::Removed);
        assert_eq!(hunk.lines[0].old_line, Some(1));
        assert_eq!(hunk.lines[1].content, "+ new inc");
        assert_eq!(hunk.lines[1].kind, DiffPreviewLineKind::Added);
        assert_eq!(hunk.lines[1].new_line, Some(1));
        assert_eq!(hunk.lines[2].content, "context");
        assert_eq!(hunk.lines[2].old_line, Some(2));
        assert_eq!(hunk.lines[2].new_line, Some(2));
    }

    #[test]
    fn consecutive_special_lines_and_multi_file_do_not_cross_hunks() {
        let diff = "diff --git a/a.txt b/a.txt
--- a/a.txt
+++ b/a.txt
@@ -1,3 +1,3 @@
-- one
-- two
++ new one
++ new two
 ctx

diff --git a/b.txt b/b.txt
--- a/b.txt
+++ b/b.txt
@@ -1 +1 @@
-- drop
++ keep
";
        let files = parse_unified_diff_preview_files(diff).expect("parsed");
        assert_eq!(files.len(), 2, "real next file header is not swallowed");
        assert_eq!(files[0].path, "a.txt");
        assert_eq!(files[0].hunks[0].lines.len(), 5);
        assert_eq!(files[1].path, "b.txt");
        assert_eq!(files[1].hunks[0].lines.len(), 2);
        assert_eq!(files[1].hunks[0].lines[0].content, "- drop");
        assert_eq!(files[1].hunks[0].lines[1].content, "+ keep");
    }

    #[test]
    fn empty_hunk_is_kept_without_lines() {
        let diff = "diff --git a/e.txt b/e.txt
--- a/e.txt
+++ b/e.txt
@@ -0,0 +1,0 @@
";
        let files = parse_unified_diff_preview_files(diff).expect("parsed");
        assert_eq!(files.len(), 1);
        assert!(files[0].hunks[0].lines.is_empty());
    }

    #[test]
    fn git_diff_paths_preserve_spaces_renames_and_quoted_utf8() {
        // Ordinary spaces in the path must not split at the first space.
        let header = "a/My Project/tracked a.txt b/My Project/tracked a.txt";
        let (old, new) = parse_diff_git_paths(header).expect("space path header");
        assert_eq!(old, "My Project/tracked a.txt");
        assert_eq!(new, "My Project/tracked a.txt");

        // Path containing the literal " b/" separator substring.
        let header = "a/dir b/file.txt b/dir b/file.txt";
        let (old, new) = parse_diff_git_paths(header).expect("b/ in path");
        assert_eq!(old, "dir b/file.txt");
        assert_eq!(new, "dir b/file.txt");

        // Rename header paths are corrected by rename from/to lines.
        let header = "a/old name.txt b/new name.txt";
        let (old, new) = parse_diff_git_paths(header).expect("rename header");
        assert_eq!(old, "old name.txt");
        assert_eq!(new, "new name.txt");
    }

    #[test]
    fn diff_git_paths_decode_quoted_paths_before_stripping_prefixes() {
        let header = r#""a/quoted path.txt" "b/quoted path.txt""#;
        let (old, new) = parse_diff_git_paths(header).expect("quoted header");
        assert_eq!(old, "quoted path.txt");
        assert_eq!(new, "quoted path.txt");

        // Mixed unquoted a/ + quoted b/.
        let header = r#"a/plain.txt "b/plain.txt""#;
        let (old, new) = parse_diff_git_paths(header).expect("mixed header");
        assert_eq!(old, "plain.txt");
        assert_eq!(new, "plain.txt");
    }

    #[test]
    fn diff_git_paths_reject_malformed_quoted_headers() {
        assert!(parse_diff_git_paths(r#""a/x.txt" "b/x.txt" extra"#).is_none());
        assert!(parse_diff_git_paths(r#""a/x.txt"#).is_none());
    }

    #[test]
    fn real_git_preview_preserves_tracked_untracked_and_rename_paths() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(repo)
                .args(args)
                .output()
                .expect("git runs");
            assert!(
                out.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).to_string()
        };
        git(&["init", "-q", "-b", "main"]);
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "t"]);
        std::fs::create_dir_all(repo.join("My Project")).unwrap();
        std::fs::write(repo.join("My Project/tracked a.txt"), "old\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-qm", "one"]);
        std::fs::write(repo.join("My Project/tracked a.txt"), "new\n").unwrap();
        let diff = git(&["diff", "HEAD"]);
        let files = parse_unified_diff_preview_files(&diff).expect("parsed");
        let file = files
            .iter()
            .find(|f| f.path == "My Project/tracked a.txt")
            .expect("space path preserved");
        assert_eq!(file.old_path.as_deref(), Some("My Project/tracked a.txt"));

        // Untracked file with spaces (diff against /dev/null).
        std::fs::write(
            repo.join("My Project/new file.txt"),
            "added
",
        )
        .unwrap();
        git(&["add", "."]);
        git(&["commit", "-qm", "two"]);
        std::fs::write(
            repo.join("My Project/untracked a.txt"),
            "u
",
        )
        .unwrap();
        let diff = git(&["diff", "HEAD"]);
        let files = parse_unified_diff_preview_files(&diff).expect("parsed");
        assert!(files.iter().all(|f| !f.path.is_empty()));

        // Rename with spaces.
        git(&[
            "mv",
            "My Project/new file.txt",
            "My Project/renamed file.txt",
        ]);
        let diff = git(&["diff", "--cached", "-M", "HEAD"]);
        let files = parse_unified_diff_preview_files(&diff).expect("parsed");
        let renamed = files
            .iter()
            .find(|f| f.path == "My Project/renamed file.txt")
            .expect("rename path preserved");
        assert_eq!(renamed.old_path.as_deref(), Some("My Project/new file.txt"));
    }
}
