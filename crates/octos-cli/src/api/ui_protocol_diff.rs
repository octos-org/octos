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
//! Counters (emitted via `tracing::info!` from
//! [`PendingDiffPreviewStore::log_metrics`]):
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
use tracing::{info, warn};

/// A pending diff-preview entry. Carries both the parsed `DiffPreview` that
/// is shipped to clients and a raw snapshot of the underlying diff bytes
/// captured *at proposal time*. Storing the snapshot in the entry closes
/// the TOCTOU between proposal and apply: subsequent `diff/preview/get`
/// calls return the proposal-time view even if the file on disk has been
/// rewritten between proposal and approval.
#[derive(Debug, Clone)]
pub(super) struct PendingDiffEntry {
    preview: DiffPreview,
    /// Raw unified diff captured at proposal time. `None` when the
    /// runtime did not surface a diff at all (e.g. tool emitted no
    /// `diff` and `materialize_file_mutation_diff` could not produce one).
    /// Used by tests today and by apply-time consistency checks once the
    /// apply path is wired in.
    #[allow(dead_code)]
    snapshot_at_proposal: Option<String>,
}

impl PendingDiffEntry {
    fn new(preview: DiffPreview, snapshot: Option<String>) -> Self {
        Self {
            preview,
            snapshot_at_proposal: snapshot,
        }
    }

    #[cfg(test)]
    pub(super) fn snapshot(&self) -> Option<&str> {
        self.snapshot_at_proposal.as_deref()
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
pub(super) struct DiffPreviewConfig {
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
    pub(super) fn ephemeral() -> Self {
        Self {
            entries_per_session: 4096,
            active_session_cap: 1024,
            idle_ttl: Duration::from_secs(60 * 60),
            data_dir: None,
        }
    }

    pub(super) fn durable(data_dir: PathBuf) -> Self {
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
        }
    }

    fn touch_lru(&mut self, session_id: &SessionKey) {
        if let Some(idx) = self.lru.iter().position(|key| key == session_id) {
            self.lru.remove(idx);
        }
        self.lru.push_front(session_id.clone());
    }
}

pub(super) struct PendingDiffPreviewStore {
    config: DiffPreviewConfig,
    inner: Mutex<StoreInner>,
}

impl Default for PendingDiffPreviewStore {
    fn default() -> Self {
        Self::with_config(DiffPreviewConfig::ephemeral())
    }
}

impl PendingDiffPreviewStore {
    pub(super) fn with_config(config: DiffPreviewConfig) -> Self {
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
    pub(super) fn recover(config: DiffPreviewConfig) -> RecoveryOutcome {
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
                        warn!(
                            target = "octos::diff_preview",
                            version = record.v,
                            path = %path.display(),
                            "skipping diff-preview record with unknown version"
                        );
                        continue;
                    }
                    Err(error) => {
                        warn!(
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
                    warn!(
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
        }

        if loaded > 0 {
            let mut inner = self.inner.lock().expect("diff preview store poisoned");
            inner.on_disk_bytes = inner.on_disk_bytes.saturating_add(total_disk_bytes);
            inner.touch_lru(session_id);
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

    pub(super) fn get(
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

    #[allow(dead_code)]
    pub(super) fn insert(&self, preview: DiffPreview) {
        self.insert_with_snapshot(preview, None);
    }

    pub(super) fn insert_with_snapshot(
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

    pub(super) fn upsert_file_mutation(
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

    /// Snapshot of the observability counters. Useful for tests and
    /// the `/metrics` endpoint integration.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) fn metrics(&self) -> DiffPreviewMetrics {
        let inner = self.inner.lock().expect("diff preview store poisoned");
        let in_memory_bytes: usize = inner.sessions.values().map(|s| s.in_memory_bytes).sum();
        DiffPreviewMetrics {
            entries_active: inner.entries.len(),
            sessions_active: inner.sessions.len(),
            sessions_evicted: inner.evicted_count,
            entries_dropped: inner.dropped_count,
            recovery_entries_loaded: inner.recovery_loaded_count,
            bytes_in_memory: in_memory_bytes,
            bytes_on_disk: inner.on_disk_bytes,
        }
    }

    /// Emit a structured tracing line with the current metrics.
    /// Intended for periodic operator-visibility, mirroring the
    /// ledger's sweep-tick log.
    #[allow(dead_code)]
    pub(super) fn log_metrics(&self) {
        let m = self.metrics();
        info!(
            target = "octos::diff_preview",
            diff_preview.entries.active = m.entries_active,
            diff_preview.sessions.active = m.sessions_active,
            diff_preview.eviction.dropped = m.entries_dropped,
            diff_preview.recovery.entries_loaded = m.recovery_entries_loaded,
            diff_preview.bytes.in_memory = m.bytes_in_memory,
            diff_preview.bytes.on_disk = m.bytes_on_disk,
            "diff preview metrics tick"
        );
    }

    #[cfg(test)]
    pub(super) fn snapshot_for(&self, preview_id: &PreviewId) -> Option<String> {
        self.inner
            .lock()
            .expect("diff preview store poisoned")
            .entries
            .get(preview_id)
            .and_then(|entry| entry.snapshot().map(ToOwned::to_owned))
    }
}

/// Outcome of [`PendingDiffPreviewStore::recover`]. The caller wires
/// `store` into the singleton; the counts are useful for the boot log
/// line.
pub(super) struct RecoveryOutcome {
    pub(super) store: PendingDiffPreviewStore,
    pub(super) sessions_recovered: usize,
    pub(super) entries_recovered: usize,
}

/// Snapshot of the diff preview store observability counters.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct DiffPreviewMetrics {
    pub(super) entries_active: usize,
    pub(super) sessions_active: usize,
    pub(super) sessions_evicted: u64,
    pub(super) entries_dropped: u64,
    pub(super) recovery_entries_loaded: u64,
    pub(super) bytes_in_memory: usize,
    pub(super) bytes_on_disk: u64,
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
        out.push_str(&format!("{:02x}", byte));
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

    let safe_path = super::ui_protocol_sanitize::sanitize_display_path(&notice.path);
    DiffPreview {
        session_id,
        preview_id,
        title: Some(format!("{} {}", notice.operation, safe_path)),
        files,
    }
}

fn file_from_mutation_notice(notice: &UiFileMutationNotice) -> DiffPreviewFile {
    DiffPreviewFile {
        path: super::ui_protocol_sanitize::sanitize_display_path(&notice.path),
        old_path: None,
        status: status_from_operation(&notice.operation),
        hunks: Vec::new(),
    }
}

fn sanitize_preview_file(mut file: DiffPreviewFile) -> DiffPreviewFile {
    file.path = super::ui_protocol_sanitize::sanitize_display_path(&file.path);
    file.old_path = file
        .old_path
        .map(|path| super::ui_protocol_sanitize::sanitize_display_path(&path));
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

    for line in diff.lines() {
        if let Some((old_path, new_path)) = line
            .strip_prefix("diff --git ")
            .and_then(parse_diff_git_paths)
        {
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
        } else if line.starts_with("@@ ") {
            push_hunk(&mut current, &mut current_hunk);
            let (old_start, new_start) = parse_hunk_starts(line).unwrap_or((1, 1));
            old_line = old_start;
            new_line = new_start;
            current_hunk = Some(DiffPreviewHunk {
                header: line.to_string(),
                lines: Vec::new(),
            });
        } else if let Some(hunk) = current_hunk.as_mut() {
            if line.starts_with("--- ") || line.starts_with("+++ ") {
                continue;
            }
            let Some(first) = line.chars().next() else {
                continue;
            };
            match first {
                '+' => {
                    hunk.lines.push(DiffPreviewLine {
                        kind: DiffPreviewLineKind::Added,
                        content: line[1..].to_string(),
                        old_line: None,
                        new_line: Some(new_line),
                    });
                    new_line += 1;
                }
                '-' => {
                    hunk.lines.push(DiffPreviewLine {
                        kind: DiffPreviewLineKind::Removed,
                        content: line[1..].to_string(),
                        old_line: Some(old_line),
                        new_line: None,
                    });
                    old_line += 1;
                }
                ' ' => {
                    hunk.lines.push(DiffPreviewLine {
                        kind: DiffPreviewLineKind::Context,
                        content: line[1..].to_string(),
                        old_line: Some(old_line),
                        new_line: Some(new_line),
                    });
                    old_line += 1;
                    new_line += 1;
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
    let (old_path, new_path) = rest.split_once(' ')?;
    Some((strip_diff_prefix(old_path), strip_diff_prefix(new_path)))
}

fn strip_diff_prefix(path: &str) -> String {
    path.strip_prefix("a/")
        .or_else(|| path.strip_prefix("b/"))
        .unwrap_or(path)
        .to_string()
}

fn parse_hunk_starts(header: &str) -> Option<(u32, u32)> {
    let mut parts = header.split_whitespace();
    parts.next()?;
    let old = parts.next()?.trim_start_matches('-');
    let new = parts.next()?.trim_start_matches('+');
    Some((parse_range_start(old)?, parse_range_start(new)?))
}

fn parse_range_start(range: &str) -> Option<u32> {
    range.split(',').next()?.parse().ok()
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
    use octos_core::ui_protocol::{
        DiffPreviewFile, DiffPreviewFileStatus, DiffPreviewHunk, DiffPreviewLine,
        DiffPreviewLineKind, TurnId, UiFileMutationNotice,
    };
    use std::sync::Arc;

    fn sample_preview(session: &SessionKey, preview_id: PreviewId) -> DiffPreview {
        DiffPreview {
            session_id: session.clone(),
            preview_id,
            title: Some("preview".into()),
            files: vec![DiffPreviewFile {
                path: "src/lib.rs".into(),
                old_path: None,
                status: DiffPreviewFileStatus::Modified,
                hunks: vec![DiffPreviewHunk {
                    header: "@@ -1 +1 @@".into(),
                    lines: vec![DiffPreviewLine {
                        kind: DiffPreviewLineKind::Added,
                        content: "new line".into(),
                        old_line: None,
                        new_line: Some(1),
                    }],
                }],
            }],
        }
    }

    #[test]
    fn known_diff_preview_returns_stored_preview() {
        let store = PendingDiffPreviewStore::default();
        let session_id = SessionKey("local:test".into());
        let preview_id = PreviewId::new();
        store.insert(sample_preview(&session_id, preview_id.clone()));

        let result = store
            .get(DiffPreviewGetParams {
                session_id,
                preview_id,
            })
            .expect("preview should exist");

        assert_eq!(result.status, DiffPreviewGetStatus::Ready);
        assert_eq!(result.source, DiffPreviewSource::PendingStore);
        assert_eq!(result.preview.files[0].path, "src/lib.rs");
    }

    #[test]
    fn file_mutation_produces_deterministic_preview_from_diff() {
        let store = PendingDiffPreviewStore::default();
        let session_id = SessionKey("local:test".into());
        let turn_id = TurnId::new();
        let mut notice = UiFileMutationNotice::new("src/lib.rs", file_mutation_operations::MODIFY);
        notice.tool_call_id = Some("tool-1".into());

        let diff = "\
diff --git a/src/lib.rs b/src/lib.rs
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1,2 +1,2 @@
 fn main() {
-    old();
+    new();
 }
";

        let preview_id =
            store.upsert_file_mutation(session_id.clone(), &turn_id, &mut notice, Some(diff));
        let repeated = store.upsert_file_mutation(
            session_id.clone(),
            &turn_id,
            &mut UiFileMutationNotice {
                preview_id: None,
                ..notice.clone()
            },
            Some(diff),
        );

        assert_eq!(repeated, preview_id);
        assert_eq!(notice.preview_id, Some(preview_id.clone()));

        let result = store
            .get(DiffPreviewGetParams {
                session_id,
                preview_id,
            })
            .expect("preview should be produced from mutation");

        assert_eq!(result.source, DiffPreviewSource::PendingStore);
        assert_eq!(result.preview.files[0].path, "src/lib.rs");
        assert_eq!(
            result.preview.files[0].status,
            DiffPreviewFileStatus::Modified
        );
        assert_eq!(
            result.preview.files[0].hunks[0].lines[1].kind,
            DiffPreviewLineKind::Removed
        );
        assert_eq!(
            result.preview.files[0].hunks[0].lines[2].kind,
            DiffPreviewLineKind::Added
        );
    }

    #[test]
    fn missing_diff_preview_is_typed_not_found() {
        let store = PendingDiffPreviewStore::default();
        let error = store
            .get(DiffPreviewGetParams {
                session_id: SessionKey("local:test".into()),
                preview_id: PreviewId::new(),
            })
            .expect_err("missing preview should fail");

        assert_eq!(error.code, rpc_error_codes::UNKNOWN_PREVIEW_ID);
        assert_eq!(
            error.data.as_ref().and_then(|data| data.get("kind")),
            Some(&json!("unknown_preview"))
        );
    }

    // ---------- Durability tests ----------

    #[test]
    fn inserted_preview_survives_simulated_restart() {
        let temp = tempfile::tempdir().expect("tempdir");
        let session_id = SessionKey("local:restart".into());
        let preview_id = PreviewId::new();

        // Boot 1: insert and drop.
        {
            let store = PendingDiffPreviewStore::with_config(DiffPreviewConfig::durable(
                temp.path().into(),
            ));
            store.insert_with_snapshot(
                sample_preview(&session_id, preview_id.clone()),
                Some("raw diff bytes".into()),
            );
            let m = store.metrics();
            assert_eq!(m.entries_active, 1);
            assert!(m.bytes_on_disk > 0);
        }

        // Boot 2: recover and verify the preview is hydrated.
        let outcome =
            PendingDiffPreviewStore::recover(DiffPreviewConfig::durable(temp.path().into()));
        assert_eq!(outcome.sessions_recovered, 1);
        assert_eq!(outcome.entries_recovered, 1);

        let result = outcome
            .store
            .get(DiffPreviewGetParams {
                session_id,
                preview_id: preview_id.clone(),
            })
            .expect("preview should be recovered");
        assert_eq!(result.preview.preview_id, preview_id);
        assert_eq!(result.preview.files[0].path, "src/lib.rs");
        assert_eq!(
            outcome.store.snapshot_for(&preview_id).as_deref(),
            Some("raw diff bytes")
        );
    }

    #[test]
    fn recover_handles_missing_data_dir_as_no_op() {
        let temp = tempfile::tempdir().expect("tempdir");
        // No prior writes — data_dir is empty.
        let outcome =
            PendingDiffPreviewStore::recover(DiffPreviewConfig::durable(temp.path().into()));
        assert_eq!(outcome.sessions_recovered, 0);
        assert_eq!(outcome.entries_recovered, 0);
        let m = outcome.store.metrics();
        assert_eq!(m.entries_active, 0);
        assert_eq!(m.sessions_active, 0);
    }

    #[test]
    fn recover_handles_corrupted_log_entry_by_skipping() {
        let temp = tempfile::tempdir().expect("tempdir");
        let session_id = SessionKey("local:corrupt".into());
        let valid_id = PreviewId::new();

        // Boot 1: write one valid record.
        {
            let store = PendingDiffPreviewStore::with_config(DiffPreviewConfig::durable(
                temp.path().into(),
            ));
            store.insert_with_snapshot(sample_preview(&session_id, valid_id.clone()), None);
        }

        // Append a malformed line + a truncated line to the log file.
        let session_dir = temp
            .path()
            .join("ui-protocol")
            .join(encode_session_dir_name(&session_id));
        let log_files = list_log_files(&session_dir).expect("list logs");
        let log_path = log_files.first().expect("log file").clone();
        {
            let mut f = OpenOptions::new()
                .append(true)
                .open(&log_path)
                .expect("open log for append");
            f.write_all(b"this is not json\n").expect("append junk");
            f.write_all(b"{\"v\":1,\"ts\":0,\"preview\":{\"session_id\":")
                .expect("append truncated"); // truncated mid-record, no newline
        }

        // Boot 2: recover — valid record should be loaded, malformed
        // lines skipped without crashing.
        let outcome =
            PendingDiffPreviewStore::recover(DiffPreviewConfig::durable(temp.path().into()));
        assert_eq!(outcome.sessions_recovered, 1);
        assert_eq!(outcome.entries_recovered, 1);
        let result = outcome
            .store
            .get(DiffPreviewGetParams {
                session_id,
                preview_id: valid_id,
            })
            .expect("valid preview survives corruption");
        assert_eq!(result.status, DiffPreviewGetStatus::Ready);
    }

    #[test]
    fn concurrent_insert_then_get_does_not_race_with_disk_write() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = Arc::new(PendingDiffPreviewStore::with_config(
            DiffPreviewConfig::durable(temp.path().into()),
        ));
        let session_id = SessionKey("local:race".into());

        let handles: Vec<_> = (0..16)
            .map(|i| {
                let store = store.clone();
                let session_id = session_id.clone();
                std::thread::spawn(move || {
                    let preview_id = PreviewId::new();
                    let preview = sample_preview(&session_id, preview_id.clone());
                    store.insert_with_snapshot(preview, Some(format!("snap-{i}")));
                    let result = store
                        .get(DiffPreviewGetParams {
                            session_id,
                            preview_id: preview_id.clone(),
                        })
                        .expect("immediately readable");
                    assert_eq!(result.preview.preview_id, preview_id);
                })
            })
            .collect();
        for h in handles {
            h.join().expect("thread join");
        }
        assert_eq!(store.metrics().entries_active, 16);
    }

    #[test]
    fn same_preview_id_reinsert_overwrites_in_recovery() {
        let temp = tempfile::tempdir().expect("tempdir");
        let session_id = SessionKey("local:dup".into());
        let preview_id = PreviewId::new();

        {
            let store = PendingDiffPreviewStore::with_config(DiffPreviewConfig::durable(
                temp.path().into(),
            ));
            // First insert: title "first"
            let mut p = sample_preview(&session_id, preview_id.clone());
            p.title = Some("first".into());
            store.insert_with_snapshot(p, Some("snap-1".into()));
            // Second insert with same id: title "second"
            let mut p2 = sample_preview(&session_id, preview_id.clone());
            p2.title = Some("second".into());
            store.insert_with_snapshot(p2, Some("snap-2".into()));
        }

        let outcome =
            PendingDiffPreviewStore::recover(DiffPreviewConfig::durable(temp.path().into()));
        assert_eq!(outcome.entries_recovered, 1);
        let result = outcome
            .store
            .get(DiffPreviewGetParams {
                session_id,
                preview_id: preview_id.clone(),
            })
            .expect("recovered");
        assert_eq!(result.preview.title.as_deref(), Some("second"));
        assert_eq!(
            outcome.store.snapshot_for(&preview_id).as_deref(),
            Some("snap-2")
        );
    }

    #[test]
    fn eviction_drops_oldest_session_when_active_cap_exceeded() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut config = DiffPreviewConfig::durable(temp.path().into());
        config.active_session_cap = 2;
        let store = PendingDiffPreviewStore::with_config(config);

        let s1 = SessionKey("local:s1".into());
        let s2 = SessionKey("local:s2".into());
        let s3 = SessionKey("local:s3".into());
        let p1 = PreviewId::new();
        let p2 = PreviewId::new();
        let p3 = PreviewId::new();
        store.insert_with_snapshot(sample_preview(&s1, p1.clone()), None);
        store.insert_with_snapshot(sample_preview(&s2, p2.clone()), None);
        // Inserting s3 should evict s1 (LRU).
        store.insert_with_snapshot(sample_preview(&s3, p3.clone()), None);

        let m = store.metrics();
        assert_eq!(m.sessions_active, 2);
        assert_eq!(m.sessions_evicted, 1);

        // s1's preview is gone from RAM…
        assert!(matches!(
            store.get(DiffPreviewGetParams {
                session_id: s1.clone(),
                preview_id: p1.clone(),
            }),
            Err(_)
        ));
        // …but its disk file is retained for replay-after-restart.
        let s1_dir = temp
            .path()
            .join("ui-protocol")
            .join(encode_session_dir_name(&s1));
        assert!(s1_dir.exists(), "evicted session disk file must remain");
        let log_files = list_log_files(&s1_dir).expect("list logs");
        assert!(
            !log_files.is_empty(),
            "evicted session log files must survive eviction"
        );

        // s2 + s3 are still present.
        assert!(
            store
                .get(DiffPreviewGetParams {
                    session_id: s2,
                    preview_id: p2,
                })
                .is_ok()
        );
        assert!(
            store
                .get(DiffPreviewGetParams {
                    session_id: s3,
                    preview_id: p3,
                })
                .is_ok()
        );

        // Restart-from-disk recovers s1 too (its log file is still
        // there, even though it was evicted from RAM mid-run).
        drop(store);
        let outcome =
            PendingDiffPreviewStore::recover(DiffPreviewConfig::durable(temp.path().into()));
        assert_eq!(outcome.sessions_recovered, 3);
        assert!(
            outcome
                .store
                .get(DiffPreviewGetParams {
                    session_id: s1,
                    preview_id: p1,
                })
                .is_ok()
        );
    }
}
