//! In-memory + on-disk UI Protocol event ledger (M9.6 + M9-FIX-05).
//!
//! ## Durability model — Path A
//!
//! Each session owns a `SessionLedger`. The hot path is an LRU-managed
//! ring buffer in memory; the cold/durable path is a per-session
//! append-only JSON-Lines log under
//! `<data_dir>/ui-protocol/<safe_session_id>/ledger-<epoch_micros>.log`.
//!
//! Live notification flow:
//!
//! 1. Caller invokes [`UiProtocolLedger::append_notification`] or
//!    [`UiProtocolLedger::append_progress`].
//! 2. Ledger assigns the next monotonic `seq`, stamps the cursor into the
//!    payload (where applicable), writes a JSON-Lines record to the active
//!    log file (write-ahead), then pushes the entry into the in-memory
//!    ring buffer and returns the cursor to the caller.
//! 3. The caller (`ui_protocol.rs`) is then free to send the wire frame.
//!    Because the disk write is observed before the function returns, a
//!    crash between disk-commit and wire-emit leaves the event durably
//!    recorded for replay on the next session/open.
//!
//! Eviction:
//!
//! - Per-session ring is bounded by `retained_per_session` (default 4096).
//!   Older entries are dropped from RAM but remain on disk until rotation.
//! - When the active session count exceeds `active_session_cap` (default
//!   1024) the LRU session is evicted from RAM (its disk log stays).
//! - A periodic sweep (every `sweep_interval`, default 60 s) evicts
//!   sessions whose `last_touched_at` is older than `idle_ttl` (default 1
//!   hour).
//!
//! Recovery:
//!
//! - At startup, [`UiProtocolLedger::recover`] scans
//!   `<data_dir>/ui-protocol/`. For each session directory it streams all
//!   retained log files in order and hydrates up to `retained_per_session`
//!   tail entries into the in-memory ring. The next `seq` continues from
//!   the highest retained on-disk seq.
//!
//! Counters (emitted via `tracing::info!` with structured fields):
//!
//! - `ledger.sessions.active`
//! - `ledger.sessions.evicted`
//! - `ledger.events.dropped`
//! - `ledger.bytes.in_memory`
//! - `ledger.bytes.on_disk`
//!
//! See `~/home/octos/docs/M9-LEDGER-DURABILITY-ADR.md` for the full
//! decision record and tradeoffs.

use std::collections::{HashMap, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use octos_core::SessionKey;
use octos_core::ui_protocol::{
    RpcError, RpcNotification, SessionOpened, TurnCompletedEvent, UiCursor, UiNotification,
    UiProgressEvent, methods,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tracing::{info, warn};

// ---------- Public configuration ----------

/// Tunables for [`UiProtocolLedger`].
///
/// Defaults match the M9-FIX-05 spec: 4096 events per session, 1024
/// active sessions, 1 hour idle TTL, 10 MB log rotation, 5 retained
/// log files per session, 60 s sweep interval.
#[derive(Debug, Clone)]
pub(crate) struct LedgerConfig {
    pub retained_per_session: usize,
    pub active_session_cap: usize,
    pub idle_ttl: Duration,
    pub sweep_interval: Duration,
    pub rotate_bytes: u64,
    pub retained_log_files: usize,
    /// When `None`, the ledger is RAM-only (Path B fallback / unit tests).
    pub data_dir: Option<PathBuf>,
}

impl LedgerConfig {
    pub(crate) fn ephemeral(retained_per_session: usize) -> Self {
        Self {
            retained_per_session: retained_per_session.max(1),
            active_session_cap: 1024,
            idle_ttl: Duration::from_secs(60 * 60),
            sweep_interval: Duration::from_secs(60),
            rotate_bytes: 10 * 1024 * 1024,
            retained_log_files: 5,
            data_dir: None,
        }
    }

    pub(crate) fn durable(data_dir: PathBuf) -> Self {
        Self {
            retained_per_session: 4096,
            active_session_cap: 1024,
            idle_ttl: Duration::from_secs(60 * 60),
            sweep_interval: Duration::from_secs(60),
            rotate_bytes: 10 * 1024 * 1024,
            retained_log_files: 5,
            data_dir: Some(data_dir),
        }
    }
}

// ---------- Event variants ----------

/// Anything that can sit in the ledger ring.
///
/// Serialized with an outer `envelope` tag distinct from `UiNotification`'s
/// own `kind` tag so the two enums round-trip cleanly when nested.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "envelope", rename_all = "snake_case")]
pub(crate) enum UiProtocolLedgerEvent {
    Notification(UiNotification),
    Progress(UiProgressEvent),
}

impl UiProtocolLedgerEvent {
    pub(crate) fn session_id(&self) -> &SessionKey {
        match self {
            Self::Notification(notification) => notification_session_id(notification),
            Self::Progress(event) => &event.session_id,
        }
    }

    pub(crate) fn into_rpc_notification(self) -> Result<RpcNotification<Value>, serde_json::Error> {
        match self {
            Self::Notification(notification) => notification.into_rpc_notification(),
            Self::Progress(event) => event.into_rpc_notification(),
        }
    }

    fn with_cursor(mut self, cursor: UiCursor) -> Self {
        if let Self::Notification(notification) = &mut self {
            match notification {
                UiNotification::SessionOpened(SessionOpened {
                    cursor: event_cursor,
                    ..
                })
                | UiNotification::TurnCompleted(TurnCompletedEvent {
                    cursor: event_cursor,
                    ..
                }) => {
                    *event_cursor = Some(cursor);
                }
                _ => {}
            }
        }
        self
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct LedgeredUiProtocolEvent {
    pub(crate) cursor: UiCursor,
    pub(crate) event: UiProtocolLedgerEvent,
}

// ---------- On-disk record ----------

#[derive(Debug, Serialize, Deserialize)]
struct LedgerDiskRecord {
    /// Schema version for the on-disk format. Bump when the record shape
    /// changes incompatibly. Recovery skips records with unknown versions
    /// and logs a warning.
    v: u32,
    seq: u64,
    event: UiProtocolLedgerEvent,
}

const LEDGER_DISK_VERSION: u32 = 1;

// ---------- Per-session state ----------

#[derive(Debug)]
struct LedgerEntry {
    seq: u64,
    event: UiProtocolLedgerEvent,
    /// Approximate bytes for the in-memory representation. Used for
    /// `ledger.bytes.in_memory` accounting; not fsync-precise.
    bytes: usize,
}

struct DiskSessionSnapshot {
    active_log_path: PathBuf,
    active_log_bytes: u64,
    total_disk_bytes: u64,
    oldest_seq: Option<u64>,
    head_seq: u64,
    retained_entries: VecDeque<LedgerEntry>,
    replay_entries: Vec<LedgeredUiProtocolEvent>,
}

/// Per-session state held under the global lock. Disk writers live inside
/// here so two appends to the same session can't interleave bytes.
struct SessionLedger {
    next_seq: u64,
    entries: VecDeque<LedgerEntry>,
    last_touched_at: Instant,
    in_memory_bytes: usize,
    /// Active log file path (None when RAM-only).
    active_log_path: Option<PathBuf>,
    /// Cached size of the active log file in bytes (so we don't `metadata`
    /// on every append).
    active_log_bytes: u64,
}

impl SessionLedger {
    fn new() -> Self {
        Self {
            next_seq: 0,
            entries: VecDeque::new(),
            last_touched_at: Instant::now(),
            in_memory_bytes: 0,
            active_log_path: None,
            active_log_bytes: 0,
        }
    }
}

// ---------- Ledger ----------

pub(crate) struct UiProtocolLedger {
    config: LedgerConfig,
    inner: Mutex<LedgerInner>,
}

struct LedgerInner {
    sessions: HashMap<SessionKey, SessionLedger>,
    /// LRU order: front is most-recently-touched, back is least.
    lru: VecDeque<SessionKey>,
    /// Process-lifetime aggregate counters.
    evicted_count: u64,
    dropped_count: u64,
    on_disk_bytes: u64,
}

impl LedgerInner {
    fn new() -> Self {
        Self {
            sessions: HashMap::new(),
            lru: VecDeque::new(),
            evicted_count: 0,
            dropped_count: 0,
            on_disk_bytes: 0,
        }
    }

    fn touch_lru(&mut self, session_id: &SessionKey) {
        if let Some(idx) = self.lru.iter().position(|key| key == session_id) {
            self.lru.remove(idx);
        }
        self.lru.push_front(session_id.clone());
    }

    fn in_memory_bytes(&self) -> usize {
        self.sessions.values().map(|s| s.in_memory_bytes).sum()
    }
}

impl UiProtocolLedger {
    /// RAM-only ledger. Used for tests and as the no-data-dir fallback.
    #[cfg(test)]
    pub(crate) fn new(retained_per_session: usize) -> Self {
        Self::with_config(LedgerConfig::ephemeral(retained_per_session))
    }

    pub(crate) fn with_config(config: LedgerConfig) -> Self {
        if let Some(dir) = &config.data_dir {
            if let Err(error) = fs::create_dir_all(dir.join("ui-protocol")) {
                warn!(
                    target = "octos::ledger",
                    ?error,
                    path = %dir.join("ui-protocol").display(),
                    "failed to create ui-protocol data dir; falling back to RAM-only"
                );
            }
        }
        Self {
            config,
            inner: Mutex::new(LedgerInner::new()),
        }
    }

    /// Build a durable ledger and replay every on-disk session into RAM.
    ///
    /// Bounded by `config.retained_per_session` per session. Returns the
    /// constructed ledger plus the number of sessions/events recovered for
    /// the boot log.
    pub(crate) fn recover(config: LedgerConfig) -> RecoveryOutcome {
        let ledger = Self::with_config(config);
        let Some(dir) = ledger.config.data_dir.clone() else {
            return RecoveryOutcome {
                ledger: Arc::new(ledger),
                sessions_recovered: 0,
                events_recovered: 0,
            };
        };
        let ui_dir = dir.join("ui-protocol");
        let mut sessions = 0usize;
        let mut events = 0usize;
        let entries = match fs::read_dir(&ui_dir) {
            Ok(entries) => entries,
            Err(error) => {
                if error.kind() != std::io::ErrorKind::NotFound {
                    warn!(
                        target = "octos::ledger",
                        ?error,
                        path = %ui_dir.display(),
                        "failed to read ui-protocol dir during recovery"
                    );
                }
                return RecoveryOutcome {
                    ledger: Arc::new(ledger),
                    sessions_recovered: 0,
                    events_recovered: 0,
                };
            }
        };
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
            match ledger.recover_one_session(&session_key, &path) {
                Ok(count) => {
                    if count > 0 {
                        sessions += 1;
                        events += count;
                    }
                }
                Err(error) => {
                    warn!(
                        target = "octos::ledger",
                        ?error,
                        session_id = %session_key.0,
                        "failed to recover session from disk"
                    );
                }
            }
        }
        info!(
            target = "octos::ledger",
            sessions_recovered = sessions,
            events_recovered = events,
            "ledger recovery complete"
        );
        RecoveryOutcome {
            ledger: Arc::new(ledger),
            sessions_recovered: sessions,
            events_recovered: events,
        }
    }

    fn recover_one_session(
        &self,
        session_id: &SessionKey,
        session_dir: &Path,
    ) -> std::io::Result<usize> {
        let Some(snapshot) = self.read_session_disk_snapshot(session_id, session_dir, None)? else {
            return Ok(0);
        };
        if snapshot.retained_entries.is_empty() {
            return Ok(0);
        }

        let count = snapshot.retained_entries.len();
        let total_disk_bytes = snapshot.total_disk_bytes;
        let mut inner = self.inner.lock().expect("ui protocol ledger lock");
        let session_state = inner
            .sessions
            .entry(session_id.clone())
            .or_insert_with(SessionLedger::new);
        hydrate_session_from_snapshot(session_state, snapshot);
        inner.on_disk_bytes = inner.on_disk_bytes.saturating_add(total_disk_bytes);
        inner.touch_lru(session_id);
        Ok(count)
    }

    fn read_session_disk_snapshot(
        &self,
        session_id: &SessionKey,
        session_dir: &Path,
        replay_after_seq: Option<u64>,
    ) -> std::io::Result<Option<DiskSessionSnapshot>> {
        let mut log_files = match list_log_files(session_dir) {
            Ok(log_files) => log_files,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        if log_files.is_empty() {
            return Ok(None);
        }
        log_files.sort();

        let active_log_path = log_files.last().expect("non-empty after sort").clone();
        let active_log_bytes = fs::metadata(&active_log_path)
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        let mut total_disk_bytes = 0u64;
        for path in &log_files {
            if let Ok(metadata) = fs::metadata(path) {
                total_disk_bytes = total_disk_bytes.saturating_add(metadata.len());
            }
        }

        let mut oldest_seq = None;
        let mut head_seq = 0u64;
        let mut retained_entries = VecDeque::new();
        let mut replay_entries = Vec::new();
        let cap = self.config.retained_per_session;

        for path in log_files {
            let file = File::open(&path)?;
            let reader = BufReader::new(file);
            for line_result in reader.lines() {
                let line = match line_result {
                    Ok(line) => line,
                    Err(error) => {
                        warn!(
                            target = "octos::ledger",
                            ?error,
                            session_id = %session_id.0,
                            path = %path.display(),
                            "io error reading ledger line; truncating this file here"
                        );
                        break;
                    }
                };
                if line.trim().is_empty() {
                    continue;
                }
                let record = match serde_json::from_str::<LedgerDiskRecord>(&line) {
                    Ok(record) if record.v == LEDGER_DISK_VERSION => record,
                    Ok(record) => {
                        warn!(
                            target = "octos::ledger",
                            version = record.v,
                            path = %path.display(),
                            "skipping ledger record with unknown version"
                        );
                        continue;
                    }
                    Err(error) => {
                        warn!(
                            target = "octos::ledger",
                            ?error,
                            session_id = %session_id.0,
                            path = %path.display(),
                            "skipping malformed ledger record"
                        );
                        continue;
                    }
                };

                oldest_seq.get_or_insert(record.seq);
                head_seq = head_seq.max(record.seq);

                if replay_after_seq.is_some_and(|after_seq| record.seq > after_seq) {
                    replay_entries.push(LedgeredUiProtocolEvent {
                        cursor: UiCursor {
                            stream: session_id.0.clone(),
                            seq: record.seq,
                        },
                        event: record.event.clone(),
                    });
                }

                let bytes = approx_event_bytes(&record.event);
                retained_entries.push_back(LedgerEntry {
                    seq: record.seq,
                    event: record.event,
                    bytes,
                });
                while retained_entries.len() > cap {
                    retained_entries.pop_front();
                }
            }
        }

        Ok(Some(DiskSessionSnapshot {
            active_log_path,
            active_log_bytes,
            total_disk_bytes,
            oldest_seq,
            head_seq,
            retained_entries,
            replay_entries,
        }))
    }

    pub(crate) fn append_notification(
        &self,
        notification: UiNotification,
    ) -> LedgeredUiProtocolEvent {
        self.append(UiProtocolLedgerEvent::Notification(notification))
    }

    pub(crate) fn append_progress(&self, event: UiProgressEvent) -> LedgeredUiProtocolEvent {
        self.append(UiProtocolLedgerEvent::Progress(event))
    }

    fn append(&self, event: UiProtocolLedgerEvent) -> LedgeredUiProtocolEvent {
        let session_id = event.session_id().clone();
        let preload_snapshot = self.snapshot_if_session_absent(&session_id);
        let cursor;
        let stamped;
        let on_disk_delta;
        {
            let mut inner = self.inner.lock().expect("ui protocol ledger lock");

            // LRU eviction: if we'd exceed the active session cap and this
            // session is new, evict the oldest first.
            let is_new = !inner.sessions.contains_key(&session_id);
            if is_new && inner.sessions.len() >= self.config.active_session_cap {
                self.evict_lru_locked(&mut inner);
            }

            let session = inner
                .sessions
                .entry(session_id.clone())
                .or_insert_with(SessionLedger::new);
            if is_new {
                if let Some(snapshot) = preload_snapshot {
                    hydrate_session_from_snapshot(session, snapshot);
                }
            }
            session.next_seq += 1;
            session.last_touched_at = Instant::now();
            cursor = UiCursor {
                stream: session_id.0.clone(),
                seq: session.next_seq,
            };
            stamped = event.with_cursor(cursor.clone());

            // Write-ahead to disk before signaling the wire — happens
            // inside the lock so two appends to the same session never
            // interleave bytes in the file.
            on_disk_delta = if self.config.data_dir.is_some() {
                match self.write_record_locked(&session_id, session, &stamped) {
                    Ok((written, reclaimed)) => (written as i64) - (reclaimed as i64),
                    Err(error) => {
                        warn!(
                            target = "octos::ledger",
                            ?error,
                            session_id = %session_id.0,
                            seq = cursor.seq,
                            "failed to append ledger record to disk; in-memory only"
                        );
                        0
                    }
                }
            } else {
                0
            };

            let bytes = approx_event_bytes(&stamped);
            session.in_memory_bytes = session.in_memory_bytes.saturating_add(bytes);
            session.entries.push_back(LedgerEntry {
                seq: cursor.seq,
                event: stamped.clone(),
                bytes,
            });
            // Cap the in-memory ring; older entries remain on disk for
            // cursor replay (within log range). Each over-cap drop bumps
            // the dropped counter (applied after we release the &mut on
            // `session` to satisfy the borrow checker).
            let mut dropped_now = 0u64;
            while session.entries.len() > self.config.retained_per_session {
                if let Some(dropped) = session.entries.pop_front() {
                    session.in_memory_bytes = session.in_memory_bytes.saturating_sub(dropped.bytes);
                    dropped_now += 1;
                }
            }

            inner.dropped_count = inner.dropped_count.saturating_add(dropped_now);
            // `on_disk_delta` is signed: rotation may reclaim more bytes than
            // the new record adds, so a single append can be a net negative
            // for `on_disk_bytes`.
            if on_disk_delta >= 0 {
                inner.on_disk_bytes = inner.on_disk_bytes.saturating_add(on_disk_delta as u64);
            } else {
                inner.on_disk_bytes = inner.on_disk_bytes.saturating_sub((-on_disk_delta) as u64);
            }
            inner.touch_lru(&session_id);
        }

        LedgeredUiProtocolEvent {
            cursor,
            event: stamped,
        }
    }

    fn snapshot_if_session_absent(&self, session_id: &SessionKey) -> Option<DiskSessionSnapshot> {
        self.config.data_dir.as_ref()?;
        {
            let inner = self.inner.lock().expect("ui protocol ledger lock");
            if inner.sessions.contains_key(session_id) {
                return None;
            }
        }

        let session_dir = self
            .config
            .data_dir
            .as_ref()?
            .join("ui-protocol")
            .join(encode_session_dir_name(session_id));
        match self.read_session_disk_snapshot(session_id, &session_dir, None) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                warn!(
                    target = "octos::ledger",
                    ?error,
                    session_id = %session_id.0,
                    "failed to hydrate retained ledger logs before append"
                );
                None
            }
        }
    }

    /// Returns `(bytes_written, bytes_reclaimed_by_rotation)`. The caller
    /// adjusts `inner.on_disk_bytes` with the net delta.
    fn write_record_locked(
        &self,
        session_id: &SessionKey,
        session: &mut SessionLedger,
        event: &UiProtocolLedgerEvent,
    ) -> std::io::Result<(u64, u64)> {
        let Some(dir) = &self.config.data_dir else {
            return Ok((0, 0));
        };
        // Open or rotate the active log file.
        let session_dir = dir
            .join("ui-protocol")
            .join(encode_session_dir_name(session_id));
        let mut reclaimed: u64 = 0;
        if session.active_log_path.is_none() {
            fs::create_dir_all(&session_dir)?;
            let path = session_dir.join(new_log_file_name());
            session.active_log_path = Some(path);
            session.active_log_bytes = 0;
        } else if session.active_log_bytes >= self.config.rotate_bytes {
            reclaimed = self.rotate_locked(session_id, session, &session_dir)?;
        }
        let path = session
            .active_log_path
            .clone()
            .expect("active log path set above");

        let record = LedgerDiskRecord {
            v: LEDGER_DISK_VERSION,
            seq: 0, // filled in by appender below
            event: event.clone(),
        };
        let cursor_seq = match event {
            UiProtocolLedgerEvent::Notification(notification) => {
                notification_cursor_seq(notification)
            }
            UiProtocolLedgerEvent::Progress(_) => None,
        }
        .unwrap_or(session.next_seq);

        let to_write = LedgerDiskRecord {
            v: record.v,
            seq: cursor_seq,
            event: record.event,
        };
        let line = serde_json::to_string(&to_write).map_err(std::io::Error::other)?;
        let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
        let bytes = line.len() as u64 + 1; // newline
        let mut writer = BufWriter::with_capacity(8192, &mut file);
        writer.write_all(line.as_bytes())?;
        writer.write_all(b"\n")?;
        writer.flush()?;
        // We rely on the OS page cache for durability; an fsync per
        // append is too expensive for the latency budget. The ADR
        // documents this as a deliberate tradeoff.
        session.active_log_bytes = session.active_log_bytes.saturating_add(bytes);
        Ok((bytes, reclaimed))
    }

    /// Rotate the session's active log file and trim retained history.
    ///
    /// Returns total disk-bytes reclaimed by deletions; the caller is
    /// responsible for subtracting that from `inner.on_disk_bytes`. We
    /// don't take `self.inner.lock()` here because callers (`append`)
    /// already hold it — a second `lock()` on `std::sync::Mutex` would
    /// deadlock the same thread.
    fn rotate_locked(
        &self,
        session_id: &SessionKey,
        session: &mut SessionLedger,
        session_dir: &Path,
    ) -> std::io::Result<u64> {
        // Trim oldest BEFORE creating the new active file so the post-
        // rotation file count is exactly `retained_log_files` (the new
        // active file replaces one rotated-out slot). Trimming after
        // would leave `retained_log_files + 1` on disk.
        //
        // Threshold: keep at most `retained_log_files - 1` rotated
        // files; the new active file makes `retained_log_files` total.
        let mut existing = list_log_files(session_dir)?;
        existing.sort();
        let keep_rotated = self.config.retained_log_files.saturating_sub(1);
        let mut reclaimed: u64 = 0;
        while existing.len() > keep_rotated {
            let oldest = existing.remove(0);
            if let Ok(meta) = fs::metadata(&oldest) {
                reclaimed = reclaimed.saturating_add(meta.len());
            }
            if let Err(error) = fs::remove_file(&oldest) {
                warn!(
                    target = "octos::ledger",
                    ?error,
                    session_id = %session_id.0,
                    path = %oldest.display(),
                    "failed to delete rotated ledger log file"
                );
            }
        }
        let new_path = session_dir.join(new_log_file_name());
        session.active_log_path = Some(new_path);
        session.active_log_bytes = 0;
        Ok(reclaimed)
    }

    fn evict_lru_locked(&self, inner: &mut LedgerInner) {
        let Some(victim) = inner.lru.pop_back() else {
            return;
        };
        if let Some(state) = inner.sessions.remove(&victim) {
            inner.evicted_count = inner.evicted_count.saturating_add(1);
            info!(
                target = "octos::ledger",
                session_id = %victim.0,
                cause = "lru_cap",
                evicted_in_memory_bytes = state.in_memory_bytes,
                "ledger evicted session from in-memory cache"
            );
        }
    }

    /// Sweep for idle sessions; called by [`spawn_eviction_task`] on the
    /// `sweep_interval`. Public so tests can drive eviction deterministically.
    pub(crate) fn sweep_idle(&self) -> usize {
        let cutoff = Instant::now() - self.config.idle_ttl;
        let mut evicted = 0usize;
        let mut inner = self.inner.lock().expect("ui protocol ledger lock");
        let victims: Vec<SessionKey> = inner
            .sessions
            .iter()
            .filter(|(_, state)| state.last_touched_at < cutoff)
            .map(|(key, _)| key.clone())
            .collect();
        for key in victims {
            if let Some(state) = inner.sessions.remove(&key) {
                inner.evicted_count = inner.evicted_count.saturating_add(1);
                if let Some(idx) = inner.lru.iter().position(|k| k == &key) {
                    inner.lru.remove(idx);
                }
                info!(
                    target = "octos::ledger",
                    session_id = %key.0,
                    cause = "idle_ttl",
                    evicted_in_memory_bytes = state.in_memory_bytes,
                    "ledger evicted idle session from in-memory cache"
                );
                evicted += 1;
            }
        }
        let active = inner.sessions.len();
        let in_memory_bytes = inner.in_memory_bytes();
        let on_disk_bytes = inner.on_disk_bytes;
        let evicted_total = inner.evicted_count;
        let dropped_total = inner.dropped_count;
        drop(inner);
        info!(
            target = "octos::ledger",
            ledger.sessions.active = active,
            ledger.sessions.evicted = evicted_total,
            ledger.events.dropped = dropped_total,
            ledger.bytes.in_memory = in_memory_bytes,
            ledger.bytes.on_disk = on_disk_bytes,
            "ledger sweep tick"
        );
        evicted
    }

    /// Snapshot of the observability counters. Useful for tests and the
    /// `/metrics` endpoint integration.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn metrics(&self) -> LedgerMetrics {
        let inner = self.inner.lock().expect("ui protocol ledger lock");
        LedgerMetrics {
            sessions_active: inner.sessions.len(),
            sessions_evicted: inner.evicted_count,
            events_dropped: inner.dropped_count,
            bytes_in_memory: inner.in_memory_bytes(),
            bytes_on_disk: inner.on_disk_bytes,
        }
    }

    pub(crate) fn replay_after(
        &self,
        session_id: &SessionKey,
        after: Option<&UiCursor>,
    ) -> Result<Vec<LedgeredUiProtocolEvent>, RpcError> {
        let Some(after) = after else {
            return Ok(Vec::new());
        };
        validate_cursor_stream(session_id, after)?;

        {
            let mut inner = self.inner.lock().expect("ui protocol ledger lock");
            if let Some(ledger) = inner.sessions.get(session_id) {
                if let Some(oldest_seq) = ledger.entries.front().map(|entry| entry.seq) {
                    let min_after_seq = oldest_seq.saturating_sub(1);
                    if after.seq >= min_after_seq && after.seq <= ledger.next_seq {
                        let result = replay_from_entries(session_id, &ledger.entries, after.seq);
                        inner.touch_lru(session_id);
                        return Ok(result);
                    }
                } else if after.seq == ledger.next_seq {
                    inner.touch_lru(session_id);
                    return Ok(Vec::new());
                }

                if self.config.data_dir.is_none() {
                    return Err(cursor_out_of_range_error(
                        session_id,
                        after,
                        ledger.next_seq,
                        ledger.entries.front().map(|entry| entry.seq),
                    ));
                }
            } else if self.config.data_dir.is_none() {
                return if after.seq == 0 {
                    Ok(Vec::new())
                } else {
                    Err(cursor_out_of_range_error(session_id, after, 0, None))
                };
            }
        }

        self.replay_after_from_disk(session_id, after)
    }

    fn replay_after_from_disk(
        &self,
        session_id: &SessionKey,
        after: &UiCursor,
    ) -> Result<Vec<LedgeredUiProtocolEvent>, RpcError> {
        let Some(data_dir) = &self.config.data_dir else {
            return Err(cursor_out_of_range_error(session_id, after, 0, None));
        };
        let session_dir = data_dir
            .join("ui-protocol")
            .join(encode_session_dir_name(session_id));
        let mut inner = self.inner.lock().expect("ui protocol ledger lock");
        if let Some(ledger) = inner.sessions.get(session_id) {
            if let Some(oldest_seq) = ledger.entries.front().map(|entry| entry.seq) {
                let min_after_seq = oldest_seq.saturating_sub(1);
                if after.seq >= min_after_seq && after.seq <= ledger.next_seq {
                    let result = replay_from_entries(session_id, &ledger.entries, after.seq);
                    inner.touch_lru(session_id);
                    return Ok(result);
                }
            } else if after.seq == ledger.next_seq {
                inner.touch_lru(session_id);
                return Ok(Vec::new());
            }
        }

        let snapshot = self
            .read_session_disk_snapshot(session_id, &session_dir, Some(after.seq))
            .map_err(|error| {
                warn!(
                    target = "octos::ledger",
                    ?error,
                    session_id = %session_id.0,
                    "failed to read retained ledger logs for replay"
                );
                cursor_out_of_range_error(session_id, after, 0, None)
            })?;
        let Some(mut snapshot) = snapshot else {
            return if after.seq == 0 {
                Ok(Vec::new())
            } else {
                Err(cursor_out_of_range_error(session_id, after, 0, None))
            };
        };

        if let Some(existing) = inner.sessions.get(session_id) {
            if existing.next_seq > snapshot.head_seq {
                return Err(cursor_out_of_range_error(
                    session_id,
                    after,
                    existing.next_seq,
                    existing.entries.front().map(|entry| entry.seq),
                ));
            }
        }

        let Some(oldest_seq) = snapshot.oldest_seq else {
            return if after.seq == 0 {
                Ok(Vec::new())
            } else {
                Err(cursor_out_of_range_error(session_id, after, 0, None))
            };
        };

        if after.seq > snapshot.head_seq {
            return Err(cursor_out_of_range_error(
                session_id,
                after,
                snapshot.head_seq,
                Some(oldest_seq),
            ));
        }

        if after.seq < oldest_seq.saturating_sub(1) {
            return Err(cursor_out_of_range_error(
                session_id,
                after,
                snapshot.head_seq,
                Some(oldest_seq),
            ));
        }

        let result = std::mem::take(&mut snapshot.replay_entries);
        let is_new = !inner.sessions.contains_key(session_id);
        if is_new && inner.sessions.len() >= self.config.active_session_cap {
            self.evict_lru_locked(&mut inner);
        }
        let session = inner
            .sessions
            .entry(session_id.clone())
            .or_insert_with(SessionLedger::new);
        hydrate_session_from_snapshot(session, snapshot);
        inner.touch_lru(session_id);
        Ok(result)
    }
}

/// Outcome of [`UiProtocolLedger::recover`]. The caller wires `ledger`
/// into the singleton; the counts are useful for the boot log line.
pub(crate) struct RecoveryOutcome {
    pub(crate) ledger: Arc<UiProtocolLedger>,
    pub(crate) sessions_recovered: usize,
    pub(crate) events_recovered: usize,
}

/// Snapshot of the ledger observability counters.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LedgerMetrics {
    pub(crate) sessions_active: usize,
    pub(crate) sessions_evicted: u64,
    pub(crate) events_dropped: u64,
    pub(crate) bytes_in_memory: usize,
    pub(crate) bytes_on_disk: u64,
}

/// Spawn the periodic idle-eviction sweep on the current Tokio runtime.
/// Returns the join handle so callers can abort during shutdown if they
/// care; today the daemon runs until process exit, so the handle is
/// dropped.
pub(crate) fn spawn_eviction_task(ledger: Arc<UiProtocolLedger>) -> tokio::task::JoinHandle<()> {
    let interval = ledger.config.sweep_interval;
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        // The first tick fires immediately; skip it so we don't sweep an
        // empty ledger at startup before any traffic.
        ticker.tick().await;
        loop {
            ticker.tick().await;
            ledger.sweep_idle();
        }
    })
}

// ---------- Helpers ----------

fn approx_event_bytes(event: &UiProtocolLedgerEvent) -> usize {
    // Approximate; we use the JSON serialization length as a stable
    // proxy. Avoids serializing twice when we also write to disk.
    serde_json::to_string(event).map(|s| s.len()).unwrap_or(0)
}

fn replay_from_entries(
    session_id: &SessionKey,
    entries: &VecDeque<LedgerEntry>,
    after_seq: u64,
) -> Vec<LedgeredUiProtocolEvent> {
    entries
        .iter()
        .filter(|entry| entry.seq > after_seq)
        .map(|entry| LedgeredUiProtocolEvent {
            cursor: UiCursor {
                stream: session_id.0.clone(),
                seq: entry.seq,
            },
            event: entry.event.clone(),
        })
        .collect()
}

fn hydrate_session_from_snapshot(session: &mut SessionLedger, snapshot: DiskSessionSnapshot) {
    session.next_seq = snapshot.head_seq;
    session.entries.clear();
    session.in_memory_bytes = 0;
    session.last_touched_at = Instant::now();
    session.active_log_path = Some(snapshot.active_log_path);
    session.active_log_bytes = snapshot.active_log_bytes;
    for entry in snapshot.retained_entries {
        session.in_memory_bytes = session.in_memory_bytes.saturating_add(entry.bytes);
        session.entries.push_back(entry);
    }
}

fn validate_cursor_stream(session_id: &SessionKey, after: &UiCursor) -> Result<(), RpcError> {
    if after.stream == session_id.0 {
        return Ok(());
    }

    Err(
        RpcError::cursor_invalid("session/open after cursor belongs to a different event stream")
            .with_data(json!({
                "kind": "cursor_stream_mismatch",
                "method": methods::SESSION_OPEN,
                "session_id": session_id,
                "expected_stream": session_id.0.as_str(),
                "actual_stream": after.stream.as_str(),
            })),
    )
}

/// `cursor_out_of_range` covers both classic "stale" cursors (older than
/// the retained window) and "future" cursors (seq beyond what we ever
/// emitted). The `kind` field differentiates them in `data`.
///
/// The core helper provides the typed `CURSOR_OUT_OF_RANGE` code. We
/// keep the legacy `kind: "cursor_expired"` value for backward
/// compatibility with existing dashboard clients.
const CURSOR_OUT_OF_RANGE_KIND: &str = "cursor_expired";

fn cursor_out_of_range_error(
    session_id: &SessionKey,
    after: &UiCursor,
    retained_seq: u64,
    oldest_retained_seq: Option<u64>,
) -> RpcError {
    let ledger_head = UiCursor {
        stream: session_id.0.clone(),
        seq: retained_seq,
    };
    let mut data = match RpcError::cursor_out_of_range(after, &ledger_head).data {
        Some(Value::Object(map)) => map,
        _ => Map::new(),
    };
    data.insert("kind".into(), json!(CURSOR_OUT_OF_RANGE_KIND));
    data.insert("method".into(), json!(methods::SESSION_OPEN));
    data.insert("session_id".into(), json!(session_id));
    data.insert("retained_seq".into(), json!(retained_seq));
    data.insert("oldest_retained_seq".into(), json!(oldest_retained_seq));

    RpcError::cursor_out_of_range(after, &ledger_head).with_data(Value::Object(data))
}

fn notification_session_id(notification: &UiNotification) -> &SessionKey {
    match notification {
        UiNotification::SessionOpened(event) => &event.session_id,
        UiNotification::TurnStarted(event) => &event.session_id,
        UiNotification::MessageDelta(event) => &event.session_id,
        UiNotification::ToolStarted(event) => &event.session_id,
        UiNotification::ToolProgress(event) => &event.session_id,
        UiNotification::ToolCompleted(event) => &event.session_id,
        UiNotification::ApprovalRequested(event) => &event.session_id,
        UiNotification::ApprovalAutoResolved(event) => &event.session_id,
        UiNotification::ApprovalDecided(event) => &event.session_id,
        UiNotification::ApprovalCancelled(event) => &event.session_id,
        UiNotification::TaskUpdated(event) => &event.session_id,
        UiNotification::TaskOutputDelta(event) => &event.session_id,
        UiNotification::ProgressUpdated(event) => &event.session_id,
        UiNotification::Warning(event) => &event.session_id,
        UiNotification::TurnCompleted(event) => &event.session_id,
        UiNotification::TurnError(event) => &event.session_id,
        UiNotification::ReplayLossy(event) => &event.session_id,
    }
}

fn notification_cursor_seq(notification: &UiNotification) -> Option<u64> {
    match notification {
        UiNotification::SessionOpened(SessionOpened { cursor, .. })
        | UiNotification::TurnCompleted(TurnCompletedEvent { cursor, .. }) => {
            cursor.as_ref().map(|c| c.seq)
        }
        _ => None,
    }
}

// ---------- Filename encoding ----------
//
// SessionKey may contain characters illegal on common filesystems
// (`:`, `/`, etc.). We hex-encode a stable representation so the
// session dir name is reversible and collision-free.

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

fn new_log_file_name() -> String {
    // Microsecond-precision epoch keeps lexical sort = chronological
    // sort, which the rotation/recovery logic relies on. The pid suffix
    // disambiguates concurrent rotates within the same microsecond.
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let micros = now.as_micros();
    format!(
        "ledger-{:020}-{:05}.log",
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
                if name.starts_with("ledger-") && name.ends_with(".log") {
                    out.push(path);
                }
            }
        }
    }
    Ok(out)
}

// ---------- Tests ----------

#[cfg(test)]
mod tests {
    use super::*;
    use octos_core::ui_protocol::{MessageDeltaEvent, TurnId, rpc_error_codes};
    use std::time::Duration as StdDuration;

    fn delta(session: &SessionKey, text: &str) -> UiNotification {
        UiNotification::MessageDelta(MessageDeltaEvent {
            session_id: session.clone(),
            turn_id: TurnId::new(),
            text: text.into(),
        })
    }

    fn replay_texts(replay: &[LedgeredUiProtocolEvent]) -> Vec<String> {
        replay
            .iter()
            .filter_map(|event| match &event.event {
                UiProtocolLedgerEvent::Notification(UiNotification::MessageDelta(delta)) => {
                    Some(delta.text.clone())
                }
                _ => None,
            })
            .collect()
    }

    #[test]
    fn ledger_replays_notifications_after_cursor_in_order() {
        let ledger = UiProtocolLedger::new(8);
        let session_id = SessionKey("local:test".into());
        let first = ledger.append_notification(delta(&session_id, "one"));
        ledger.append_notification(delta(&session_id, "two"));

        let replay = ledger
            .replay_after(&session_id, Some(&first.cursor))
            .expect("replay after cursor");

        assert_eq!(replay.len(), 1);
        assert_eq!(replay[0].cursor.seq, 2);
        assert!(matches!(
            &replay[0].event,
            UiProtocolLedgerEvent::Notification(UiNotification::MessageDelta(event))
                if event.text == "two"
        ));
    }

    #[test]
    fn ledger_assigns_cursor_to_turn_completed() {
        let ledger = UiProtocolLedger::new(8);
        let session_id = SessionKey("local:test".into());
        let turn_id = TurnId::new();

        let completed =
            ledger.append_notification(UiNotification::TurnCompleted(TurnCompletedEvent {
                session_id,
                turn_id,
                cursor: None,
            }));

        assert!(matches!(
            completed.event,
            UiProtocolLedgerEvent::Notification(UiNotification::TurnCompleted(event))
                if event.cursor == Some(completed.cursor)
        ));
    }

    #[test]
    fn ledger_rejects_wrong_stream_and_stale_cursors() {
        let ledger = UiProtocolLedger::new(1);
        let session_id = SessionKey("local:test".into());
        ledger.append_notification(delta(&session_id, "one"));
        ledger.append_notification(delta(&session_id, "two"));

        let wrong_stream = ledger
            .replay_after(
                &session_id,
                Some(&UiCursor {
                    stream: "local:other".into(),
                    seq: 1,
                }),
            )
            .expect_err("wrong stream");
        assert_eq!(
            wrong_stream.data.as_ref().and_then(|data| data.get("kind")),
            Some(&json!("cursor_stream_mismatch"))
        );
        assert_eq!(wrong_stream.code, rpc_error_codes::CURSOR_INVALID);

        let stale = ledger
            .replay_after(
                &session_id,
                Some(&UiCursor {
                    stream: session_id.0.clone(),
                    seq: 0,
                }),
            )
            .expect_err("stale cursor");
        assert_eq!(
            stale.data.as_ref().and_then(|data| data.get("kind")),
            Some(&json!("cursor_expired"))
        );
        assert_eq!(stale.code, rpc_error_codes::CURSOR_OUT_OF_RANGE);
    }

    // ---------- M9-FIX-05 acceptance tests ----------

    #[test]
    fn ledger_per_session_capacity_enforced() {
        let ledger = UiProtocolLedger::new(4);
        let session_id = SessionKey("local:cap".into());
        for i in 0..10 {
            ledger.append_notification(delta(&session_id, &format!("msg-{i}")));
        }
        let metrics = ledger.metrics();
        assert_eq!(metrics.sessions_active, 1);
        // 10 written, ring cap 4 ⇒ 6 dropped from RAM.
        assert_eq!(metrics.events_dropped, 6);
        // Verify ring contents are the most recent four.
        let replay = ledger
            .replay_after(
                &session_id,
                Some(&UiCursor {
                    stream: session_id.0.clone(),
                    seq: 6,
                }),
            )
            .expect("replay");
        let texts: Vec<_> = replay
            .iter()
            .filter_map(|e| match &e.event {
                UiProtocolLedgerEvent::Notification(UiNotification::MessageDelta(d)) => {
                    Some(d.text.clone())
                }
                _ => None,
            })
            .collect();
        assert_eq!(texts, vec!["msg-6", "msg-7", "msg-8", "msg-9"]);
    }

    #[test]
    fn ledger_idle_session_evicted_after_ttl() {
        let mut config = LedgerConfig::ephemeral(8);
        config.idle_ttl = StdDuration::from_millis(50);
        let ledger = UiProtocolLedger::with_config(config);
        let session_id = SessionKey("local:idle".into());
        ledger.append_notification(delta(&session_id, "hi"));
        assert_eq!(ledger.metrics().sessions_active, 1);
        std::thread::sleep(StdDuration::from_millis(80));
        let evicted = ledger.sweep_idle();
        assert_eq!(evicted, 1);
        let metrics = ledger.metrics();
        assert_eq!(metrics.sessions_active, 0);
        assert_eq!(metrics.sessions_evicted, 1);
    }

    #[test]
    fn ledger_active_session_cap_enforced() {
        let mut config = LedgerConfig::ephemeral(4);
        config.active_session_cap = 3;
        let ledger = UiProtocolLedger::with_config(config);
        for i in 0..5 {
            let session = SessionKey(format!("local:s{i}"));
            ledger.append_notification(delta(&session, "x"));
        }
        let metrics = ledger.metrics();
        assert_eq!(metrics.sessions_active, 3);
        // 5 unique sessions opened, cap 3 ⇒ 2 evicted.
        assert_eq!(metrics.sessions_evicted, 2);
        // The two oldest were evicted; the three newest survive.
        // Use cursor seq=1 (matches each session's single event) so that
        // present sessions resolve cleanly (next_seq=1, replay returns
        // Ok(empty)) and evicted sessions hit the unknown-session
        // cursor_out_of_range branch (after.seq != 0 → Err). With
        // cursor seq=0 a fresh session and an evicted session are
        // indistinguishable by design (both → Ok(empty)).
        for (i, expected_present) in [(2usize, true), (3, true), (4, true), (0, false), (1, false)]
        {
            let session = SessionKey(format!("local:s{i}"));
            let replay = ledger.replay_after(
                &session,
                Some(&UiCursor {
                    stream: session.0.clone(),
                    seq: 1,
                }),
            );
            assert_eq!(
                replay.is_ok(),
                expected_present,
                "session local:s{i} expected_present={expected_present}, replay={:?}",
                replay
            );
        }
    }

    #[test]
    fn ledger_replays_from_disk_after_lru_eviction() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut config = LedgerConfig::durable(temp.path().into());
        config.retained_per_session = 1;
        config.active_session_cap = 1;
        let ledger = UiProtocolLedger::with_config(config);
        let evicted = SessionKey("local:lru-disk".into());
        let other = SessionKey("local:lru-other".into());

        ledger.append_notification(delta(&evicted, "one"));
        ledger.append_notification(delta(&evicted, "two"));
        ledger.append_notification(delta(&evicted, "three"));
        ledger.append_notification(delta(&other, "evict"));
        assert_eq!(ledger.metrics().sessions_evicted, 1);

        let replay = ledger
            .replay_after(
                &evicted,
                Some(&UiCursor {
                    stream: evicted.0.clone(),
                    seq: 1,
                }),
            )
            .expect("replay evicted session from disk");

        assert_eq!(replay_texts(&replay), vec!["two", "three"]);
    }

    #[test]
    fn ledger_replays_from_disk_after_idle_ttl_eviction() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut config = LedgerConfig::durable(temp.path().into());
        config.retained_per_session = 1;
        config.idle_ttl = StdDuration::from_millis(10);
        let ledger = UiProtocolLedger::with_config(config);
        let session_id = SessionKey("local:idle-disk".into());

        ledger.append_notification(delta(&session_id, "one"));
        ledger.append_notification(delta(&session_id, "two"));
        std::thread::sleep(StdDuration::from_millis(30));
        assert_eq!(ledger.sweep_idle(), 1);
        assert_eq!(ledger.metrics().sessions_active, 0);

        let replay = ledger
            .replay_after(
                &session_id,
                Some(&UiCursor {
                    stream: session_id.0.clone(),
                    seq: 1,
                }),
            )
            .expect("replay idle-evicted session from disk");

        assert_eq!(replay_texts(&replay), vec!["two"]);
    }

    #[test]
    fn ledger_recovers_after_simulated_restart() {
        let temp = tempfile::tempdir().expect("tempdir");
        let session_id = SessionKey("local:restart".into());
        // First boot: write 3 events.
        {
            let ledger = UiProtocolLedger::with_config(LedgerConfig::durable(temp.path().into()));
            ledger.append_notification(delta(&session_id, "one"));
            ledger.append_notification(delta(&session_id, "two"));
            ledger.append_notification(delta(&session_id, "three"));
            let metrics = ledger.metrics();
            assert_eq!(metrics.sessions_active, 1);
            assert!(metrics.bytes_on_disk > 0);
        }
        // Second boot: drop the in-memory ledger, recover from disk.
        let outcome = UiProtocolLedger::recover(LedgerConfig::durable(temp.path().into()));
        assert_eq!(outcome.sessions_recovered, 1);
        assert_eq!(outcome.events_recovered, 3);
        let replay = outcome
            .ledger
            .replay_after(
                &session_id,
                Some(&UiCursor {
                    stream: session_id.0.clone(),
                    seq: 1,
                }),
            )
            .expect("replay after restart");
        assert_eq!(replay.len(), 2);
        assert_eq!(replay[0].cursor.seq, 2);
        assert_eq!(replay[1].cursor.seq, 3);
        // Append after recovery continues from seq 4.
        let next = outcome
            .ledger
            .append_notification(delta(&session_id, "four"));
        assert_eq!(next.cursor.seq, 4);
    }

    #[test]
    fn ledger_recovers_tail_across_multiple_rotated_files() {
        let temp = tempfile::tempdir().expect("tempdir");
        let session_id = SessionKey("local:recover-rotated".into());
        {
            let mut config = LedgerConfig::durable(temp.path().into());
            config.retained_per_session = 6;
            config.retained_log_files = 16;
            config.rotate_bytes = 512;
            let ledger = UiProtocolLedger::with_config(config);
            for i in 1..=8 {
                ledger.append_notification(delta(
                    &session_id,
                    &format!("rotated-{i}-{}", "x".repeat(800)),
                ));
                std::thread::sleep(StdDuration::from_millis(1));
            }
        }

        let mut config = LedgerConfig::durable(temp.path().into());
        config.retained_per_session = 6;
        config.retained_log_files = 16;
        config.rotate_bytes = 512;
        let outcome = UiProtocolLedger::recover(config);

        assert_eq!(outcome.sessions_recovered, 1);
        assert_eq!(outcome.events_recovered, 6);
        let replay = outcome
            .ledger
            .replay_after(
                &session_id,
                Some(&UiCursor {
                    stream: session_id.0.clone(),
                    seq: 2,
                }),
            )
            .expect("replay recovered tail");
        assert_eq!(replay.len(), 6);
        assert_eq!(replay[0].cursor.seq, 3);
        assert_eq!(replay[5].cursor.seq, 8);
    }

    #[test]
    fn ledger_disk_log_rotates_on_size_threshold() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut config = LedgerConfig::durable(temp.path().into());
        // Tiny rotate threshold so even a few events trigger a rotation.
        config.rotate_bytes = 256;
        config.retained_log_files = 3;
        let ledger = UiProtocolLedger::with_config(config);
        let session_id = SessionKey("local:rotate".into());
        for i in 0..50 {
            ledger.append_notification(delta(&session_id, &format!("rotate-payload-{i}")));
        }
        let dir = temp
            .path()
            .join("ui-protocol")
            .join(encode_session_dir_name(&session_id));
        let log_files = list_log_files(&dir).expect("list logs");
        assert!(
            log_files.len() > 1,
            "expected rotation, got {} files",
            log_files.len()
        );
        assert!(
            log_files.len() <= 3,
            "expected ≤3 retained files, got {}",
            log_files.len()
        );
    }

    #[test]
    fn ledger_rejects_cursor_older_than_retained_disk_logs() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut config = LedgerConfig::durable(temp.path().into());
        config.retained_per_session = 1;
        config.retained_log_files = 1;
        config.rotate_bytes = 512;
        let ledger = UiProtocolLedger::with_config(config);
        let session_id = SessionKey("local:stale-disk".into());

        for i in 1..=6 {
            ledger.append_notification(delta(
                &session_id,
                &format!("stale-{i}-{}", "x".repeat(800)),
            ));
            std::thread::sleep(StdDuration::from_millis(1));
        }

        let err = ledger
            .replay_after(
                &session_id,
                Some(&UiCursor {
                    stream: session_id.0.clone(),
                    seq: 0,
                }),
            )
            .expect_err("cursor older than retained logs");

        assert_eq!(err.code, rpc_error_codes::CURSOR_OUT_OF_RANGE);
        assert_eq!(
            err.data.as_ref().and_then(|data| data.get("kind")),
            Some(&json!("cursor_expired"))
        );
        assert_eq!(
            err.data
                .as_ref()
                .and_then(|data| data.get("oldest_retained_seq")),
            Some(&json!(6))
        );
    }

    #[test]
    fn ledger_replay_cannot_hydrate_stale_disk_over_newer_live_session() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut config = LedgerConfig::durable(temp.path().into());
        config.retained_per_session = 1;
        config.retained_log_files = 4;
        config.rotate_bytes = 1024 * 1024;
        let ledger = UiProtocolLedger::with_config(config);
        let session_id = SessionKey("local:stale-live".into());

        ledger.append_notification(delta(&session_id, "one"));
        ledger.append_notification(delta(&session_id, "two"));
        ledger.append_notification(delta(&session_id, "three"));

        let session_dir = temp
            .path()
            .join("ui-protocol")
            .join(encode_session_dir_name(&session_id));
        let mut log_files = list_log_files(&session_dir).expect("list logs");
        log_files.sort();
        let active_log = log_files.last().expect("active log");
        let contents = std::fs::read_to_string(active_log).expect("read log");
        let stale_contents = contents
            .lines()
            .take(2)
            .map(|line| {
                let mut line = line.to_owned();
                line.push('\n');
                line
            })
            .collect::<String>();
        std::fs::write(active_log, stale_contents).expect("truncate log to stale snapshot");

        let err = ledger
            .replay_after(
                &session_id,
                Some(&UiCursor {
                    stream: session_id.0.clone(),
                    seq: 1,
                }),
            )
            .expect_err("stale disk snapshot must not replace live state");
        assert_eq!(err.code, rpc_error_codes::CURSOR_OUT_OF_RANGE);
        assert_eq!(
            err.data.as_ref().and_then(|data| data.get("kind")),
            Some(&json!("cursor_expired"))
        );

        let fourth = ledger.append_notification(delta(&session_id, "four"));
        assert_eq!(fourth.cursor.seq, 4);
        let replay = ledger
            .replay_after(
                &session_id,
                Some(&UiCursor {
                    stream: session_id.0.clone(),
                    seq: 3,
                }),
            )
            .expect("replay live tail after stale disk rejection");
        assert_eq!(replay_texts(&replay), vec!["four"]);
    }

    #[test]
    fn ledger_write_ahead_durable_before_wire_signal() {
        // Race-shape test: append commits to disk *before* the function
        // returns. We simulate "wire path killed between disk-write and
        // wire-emit" by recording the cursor returned from append_*
        // (which corresponds to the on-disk record) but never sending a
        // wire frame. Then we restart and verify the event is recovered.
        let temp = tempfile::tempdir().expect("tempdir");
        let session_id = SessionKey("local:wa".into());
        let returned_cursor;
        {
            let ledger = UiProtocolLedger::with_config(LedgerConfig::durable(temp.path().into()));
            let appended = ledger.append_notification(delta(&session_id, "would-be-wire"));
            returned_cursor = appended.cursor.clone();
            // Intentionally drop the ledger here; the wire frame never
            // gets sent in this simulated crash.
        }
        let outcome = UiProtocolLedger::recover(LedgerConfig::durable(temp.path().into()));
        assert_eq!(outcome.sessions_recovered, 1);
        let replay = outcome
            .ledger
            .replay_after(
                &session_id,
                Some(&UiCursor {
                    stream: session_id.0.clone(),
                    seq: 0,
                }),
            )
            .expect("replay after simulated crash");
        assert_eq!(replay.len(), 1);
        assert_eq!(replay[0].cursor, returned_cursor);
    }

    #[test]
    fn session_dir_name_round_trip() {
        let key = SessionKey("local:test:abc/def".into());
        let encoded = encode_session_dir_name(&key);
        let decoded = decode_session_dir_name(&encoded).expect("decode");
        assert_eq!(decoded, key);
    }

    #[test]
    fn metrics_counters_track_active_dropped_evicted() {
        let mut config = LedgerConfig::ephemeral(2);
        config.active_session_cap = 2;
        let ledger = UiProtocolLedger::with_config(config);
        let s1 = SessionKey("local:m1".into());
        let s2 = SessionKey("local:m2".into());
        let s3 = SessionKey("local:m3".into());
        ledger.append_notification(delta(&s1, "a"));
        ledger.append_notification(delta(&s1, "b"));
        ledger.append_notification(delta(&s1, "c")); // drops 1
        ledger.append_notification(delta(&s2, "a"));
        ledger.append_notification(delta(&s3, "a")); // evicts s1 (LRU)
        let m = ledger.metrics();
        assert_eq!(m.sessions_active, 2);
        assert_eq!(m.events_dropped, 1);
        assert_eq!(m.sessions_evicted, 1);
        assert!(m.bytes_in_memory > 0);
    }

    /// Manual soak harness — gated behind `OCTOS_LEDGER_SOAK=1` and
    /// `--ignored` so it doesn't run in CI by default. Spam 10K events
    /// across 10 sessions, restart from disk, verify recovery within
    /// bounds. Reports peak memory + disk usage to stdout.
    #[test]
    #[ignore = "manual soak; enable with OCTOS_LEDGER_SOAK=1 and --nocapture"]
    fn ledger_soak_10k_events_10_sessions() {
        if std::env::var("OCTOS_LEDGER_SOAK").as_deref() != Ok("1") {
            return;
        }
        let temp = tempfile::tempdir().expect("tempdir");
        let sessions: Vec<SessionKey> = (0..10)
            .map(|i| SessionKey(format!("local:soak{i}")))
            .collect();
        let start = std::time::Instant::now();
        {
            let ledger = UiProtocolLedger::with_config(LedgerConfig::durable(temp.path().into()));
            for i in 0..10_000 {
                let session = &sessions[i % sessions.len()];
                ledger.append_notification(delta(session, &format!("soak-{i}")));
            }
            let m = ledger.metrics();
            eprintln!(
                "[soak] write phase: {:?} | active={} dropped={} mem_bytes={} disk_bytes={}",
                start.elapsed(),
                m.sessions_active,
                m.events_dropped,
                m.bytes_in_memory,
                m.bytes_on_disk
            );
        }
        let recover_start = std::time::Instant::now();
        let outcome = UiProtocolLedger::recover(LedgerConfig::durable(temp.path().into()));
        eprintln!(
            "[soak] recovery: {:?} | sessions={} events={}",
            recover_start.elapsed(),
            outcome.sessions_recovered,
            outcome.events_recovered
        );
        assert_eq!(outcome.sessions_recovered, sessions.len());
    }
}
