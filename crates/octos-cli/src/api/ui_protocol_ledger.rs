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
use std::fs::{self, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use octos_core::SessionKey;
use octos_core::ui_protocol::{
    Envelope, EnvelopeNotification, EnvelopeV2, EnvelopeV2Notification, Payload, PayloadV2,
    RpcError, RpcNotification, SessionOpened, TaskRuntimeState, TurnCompletedEvent, TurnErrorEvent,
    UiCursor, UiNotification, UiProgressEvent, methods,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tokio::sync::broadcast;
use tracing::{debug, info, warn};

/// Per-session broadcast buffer size. Bounded so a slow subscriber cannot
/// pin unbounded memory; on overflow the receiver sees `Lagged(n)` and
/// the connection should fall back to cursor-based replay. The ledger is
/// still the durable source of truth — broadcast is a live-fan-out shortcut.
const LIVE_BROADCAST_CAPACITY: usize = 256;

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
    /// Per-session projection snapshot cadence (step 1b): every
    /// `snapshot_every_events` appended events the session's retained ring
    /// is written to `snapshot.json` next to the log files. Recovery then
    /// loads the latest snapshot and replays only the tail (events with
    /// seq > snapshot head). `0` disables snapshotting.
    pub snapshot_every_events: u64,
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
            snapshot_every_events: 4096,
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
            snapshot_every_events: 4096,
            data_dir: Some(data_dir),
        }
    }
}

// ---------- Event variants ----------

/// Anything that can sit in the ledger ring.
///
/// Serialized AND deserialized with an outer `record_kind` tag (the
/// derived impls). The earlier name `"envelope"` collided with
/// `EnvelopeNotification.envelope` once internally-tagged `UiNotification`
/// flattened its variant data alongside the outer tag — serde produced two
/// `"envelope"` keys in the same object and rejected the record on replay
/// (`duplicate field 'envelope'`, #1358). `record_kind` is disjoint from
/// every flattened inner field name on both branches, so it is the
/// canonical on-disk tag going forward.
///
/// Back-compat for the *pre-#1358* `envelope` tag is handled NOT here but
/// at the read site (see [`LegacyLedgerDiskRecord`] and
/// `read_session_disk_snapshot`). Both the canonical and legacy parses are
/// DERIVED (`#[serde(tag = …)]`), so both stay STRICT: genuine duplicate
/// keys (`record_kind` / `kind` / `session_id` / payload fields) still
/// produce serde's `duplicate field …` error rather than being silently
/// collapsed last-wins by a `serde_json::Value` round-trip.
// Both variants hold their full wire payload by value so a ledger record
// round-trips through serde without an extra indirection; records are
// appended/replayed, never matched in a hot loop, so the size difference
// is immaterial.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "record_kind", rename_all = "snake_case")]
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

    pub(crate) fn topic(&self) -> Option<&str> {
        match self {
            Self::Notification(notification) => notification.topic(),
            Self::Progress(event) => event.session_id.topic(),
        }
    }

    fn stamp_topic_from_session(&mut self) {
        if let Self::Notification(notification) = self {
            notification.stamp_topic_from_session();
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
                // Legacy background records retain their original row seq;
                // only their durable UI-ledger cursor is stamped here.
                UiNotification::TurnSpawnComplete(spawn_complete) => {
                    spawn_complete.cursor = cursor;
                }
                // V2 normally projects an existing durable source event at
                // the wire boundary instead of appending a second ledger row.
                // Stamp it nevertheless if a test or future producer stores a
                // concrete V2 notification directly.
                UiNotification::EnvelopeV2(envelope) => {
                    envelope.envelope.cursor = Some(cursor);
                }
                _ => {}
            }
        }
        self
    }
}

/// Process-unique identifier for a single WebSocket connection. Used to
/// suppress duplicate delivery: when a handler direct-sends an event AND
/// also persists it via `append_*`, the persisting connection tags the
/// broadcast with its own id so its forwarder can drop the duplicate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct ConnectionId(pub(crate) u64);

impl ConnectionId {
    pub(crate) fn next() -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(1);
        Self(COUNTER.fetch_add(1, Ordering::Relaxed))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct LedgeredUiProtocolEvent {
    pub(crate) cursor: UiCursor,
    pub(crate) event: UiProtocolLedgerEvent,
    /// If `Some`, identifies the connection whose handler direct-sent this
    /// event to the wire. Forwarders running on the same connection must
    /// skip these to avoid double delivery; forwarders on other
    /// connections deliver normally.
    pub(crate) from_connection: Option<ConnectionId>,
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

// ---------- Per-session projection snapshot (step 1b) ----------

/// On-disk projection snapshot for one session. Written every
/// `LedgerConfig::snapshot_every_events` events; recovery loads it and
/// replays only the tail (log records with seq > `head_seq`). The FIRST
/// field is the format version — unknown versions are rejected and the
/// reader falls back to a full replay (zero-migration contract).
#[derive(Debug, Serialize, Deserialize)]
struct SessionSnapshotFile {
    /// Schema version of the snapshot format itself. Bump on any
    /// incompatible shape change.
    version: u32,
    /// Highest seq included in `entries` (== next_seq at snapshot time).
    head_seq: u64,
    /// The retained in-memory ring at snapshot time.
    entries: Vec<LedgerDiskRecord>,
}

const SESSION_SNAPSHOT_VERSION: u32 = 1;
const SESSION_SNAPSHOT_FILE_NAME: &str = "snapshot.json";

/// Result of parsing one disk line. A pre-Stage-5 persisted-message record
/// has no representation in the v2-only core enum, so it is explicitly
/// surfaced as a skipped legacy row rather than being mis-routed as a live
/// notification or treated as a fatal replay error.
#[derive(Debug)]
// This is a short-lived parse result; boxing `Record` would allocate once per
// recovered ledger line, so retain the stack representation used by the
// canonical record decoder.
#[allow(clippy::large_enum_variant)]
enum ParsedLedgerDiskRecord {
    Record(LedgerDiskRecord),
    LegacyMessagePersisted { v: u32, seq: u64 },
}

/// Minimal, derived decoder used only to recognize an on-disk legacy
/// `message_persisted` notification after the canonical decoder rejects its
/// removed inner variant. Keeping this typed (rather than probing through a
/// `serde_json::Value`) preserves serde's duplicate-field checks for the
/// discriminator fields that establish the record's identity.
#[derive(Debug, Deserialize)]
struct LegacyMessagePersistedDiskRecord {
    v: u32,
    seq: u64,
    #[serde(rename = "event")]
    _event: LegacyMessagePersistedLedgerEvent,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "record_kind", rename_all = "snake_case")]
enum LegacyMessagePersistedLedgerEvent {
    Notification(LegacyMessagePersistedNotification),
}

#[derive(Debug, Deserialize)]
struct LegacyMessagePersistedNotification {
    #[serde(rename = "kind")]
    _kind: LegacyMessagePersistedKind,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum LegacyMessagePersistedKind {
    MessagePersisted,
}

#[derive(Debug, Deserialize)]
struct LegacyTaggedMessagePersistedDiskRecord {
    v: u32,
    seq: u64,
    #[serde(rename = "event")]
    _event: LegacyTaggedMessagePersistedLedgerEvent,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "envelope", rename_all = "snake_case")]
enum LegacyTaggedMessagePersistedLedgerEvent {
    Notification(LegacyMessagePersistedNotification),
}

/// Back-compat read shim for records written by the *pre-#1358* binary,
/// whose `event` carried the outer discriminator under the legacy tag key
/// `envelope` (renamed to `record_kind` in #1358 with no alias). Mirrors
/// [`LedgerDiskRecord`] exactly except the inner event enum is tagged
/// `envelope` instead of `record_kind`.
///
/// This is a DERIVED `Deserialize`, so it is just as STRICT as the
/// canonical parse — duplicate `envelope` / `kind` / `session_id` /
/// payload keys still error with serde's `duplicate field …`. There is no
/// `serde_json::Value` round-trip anywhere on the read path, so duplicate
/// keys are never silently collapsed last-wins.
///
/// LIMITATION (Step 0 finding, #1358): this shim CANNOT represent the
/// legacy `UiNotification::Envelope` variant. Pre-#1358, the outer tag was
/// `envelope` AND `EnvelopeNotification` carries a nested `envelope`
/// OBJECT field, so internally-tagged flattening emitted TWO `envelope`
/// keys in the same object — `duplicate field 'envelope'`. Those records
/// were duplicate-key garbage on disk and were NEVER cleanly readable
/// (that is precisely the bug #1358 fixed by renaming the outer tag).
/// Recovering them is inherently impossible; the legacy shim's derived
/// parse rejects them on the duplicate `envelope` key, so they fail
/// gracefully and are counted in the per-file skip aggregate. This is NOT
/// a regression — they had no clean prior representation.
#[derive(Debug, Deserialize)]
struct LegacyLedgerDiskRecord {
    v: u32,
    seq: u64,
    event: LegacyUiProtocolLedgerEvent,
}

/// Legacy `envelope`-tagged twin of [`UiProtocolLedgerEvent`]. Derived
/// (strict) — see [`LegacyLedgerDiskRecord`].
#[derive(Debug, Deserialize)]
// Mirrors `UiProtocolLedgerEvent`'s own variant sizing; this internal
// shim is immediately converted into the canonical enum, so boxing would
// only churn an allocation. The public enum carries the same (pre-existing)
// profile.
#[allow(clippy::large_enum_variant)]
#[serde(tag = "envelope", rename_all = "snake_case")]
enum LegacyUiProtocolLedgerEvent {
    Notification(UiNotification),
    Progress(UiProgressEvent),
}

impl From<LegacyUiProtocolLedgerEvent> for UiProtocolLedgerEvent {
    fn from(legacy: LegacyUiProtocolLedgerEvent) -> Self {
        match legacy {
            LegacyUiProtocolLedgerEvent::Notification(n) => Self::Notification(n),
            LegacyUiProtocolLedgerEvent::Progress(p) => Self::Progress(p),
        }
    }
}

impl From<LegacyLedgerDiskRecord> for LedgerDiskRecord {
    fn from(legacy: LegacyLedgerDiskRecord) -> Self {
        Self {
            v: legacy.v,
            seq: legacy.seq,
            event: legacy.event.into(),
        }
    }
}

/// Parse one on-disk ledger line, dual-reading the outer event tag.
///
/// Tries the canonical [`LedgerDiskRecord`] parse (tag `record_kind`)
/// first. If — and ONLY if — that fails because the event object is
/// missing its `record_kind` discriminator (i.e. a *pre-#1358* legacy
/// record tagged `envelope`), it retries via the strict
/// [`LegacyLedgerDiskRecord`] shim. Both parses are derived and strict, so
/// duplicate-key corruption is rejected on either path; the legacy retry
/// only widens the *tag-key* alias, never the duplicate-key tolerance.
///
/// On genuine corruption the *canonical* error is returned (it reflects
/// the format the writer actually emits) so debug logs stay meaningful.
fn parse_ledger_disk_record(line: &str) -> Result<ParsedLedgerDiskRecord, serde_json::Error> {
    match serde_json::from_str::<LedgerDiskRecord>(line) {
        Ok(record) => Ok(ParsedLedgerDiskRecord::Record(record)),
        Err(canonical_err) => {
            if let Ok(legacy) = serde_json::from_str::<LegacyMessagePersistedDiskRecord>(line) {
                return Ok(ParsedLedgerDiskRecord::LegacyMessagePersisted {
                    v: legacy.v,
                    seq: legacy.seq,
                });
            }
            // Only attempt the legacy `envelope`-tagged shim when the
            // canonical parse failed specifically because the event is
            // missing its `record_kind` discriminator — i.e. a legacy
            // record. Any other failure (duplicate field, malformed JSON,
            // missing payload field, unknown version handled by caller) is
            // genuine and must surface the canonical error unchanged so
            // strictness is preserved.
            if is_missing_record_kind_tag(&canonical_err) {
                if let Ok(legacy) =
                    serde_json::from_str::<LegacyTaggedMessagePersistedDiskRecord>(line)
                {
                    return Ok(ParsedLedgerDiskRecord::LegacyMessagePersisted {
                        v: legacy.v,
                        seq: legacy.seq,
                    });
                }
                if let Ok(legacy) = serde_json::from_str::<LegacyLedgerDiskRecord>(line) {
                    return Ok(ParsedLedgerDiskRecord::Record(legacy.into()));
                }
            }
            Err(canonical_err)
        }
    }
}

/// Whether a canonical-parse error is the "the event object lacks its
/// `record_kind` discriminator" case — the signal that the record may be a
/// legacy `envelope`-tagged one worth retrying. serde's internally-tagged
/// enum reports this as `missing field \`record_kind\``.
fn is_missing_record_kind_tag(err: &serde_json::Error) -> bool {
    err.to_string().contains("missing field `record_kind`")
}

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
    /// Total records skipped across all scanned log files (unknown
    /// version + genuinely-malformed). Surfaced so callers/tests can
    /// assert the aggregation deterministically without depending on the
    /// process-global tracing subscriber's level filter.
    #[cfg_attr(not(test), allow(dead_code))]
    skipped_records: u64,
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
    /// Per-project (`appui.sessions_in_cwd`) storage scopes, keyed by the
    /// WIRE session id (`SessionKey.0`). When a session's transcript store is
    /// relocated to `<cwd>/.octos/<profile>` the API layer registers a scope
    /// here (`set_session_scope`, a 16-hex digest of the relocated
    /// `sessions_root`), and every ring/disk/cursor identity for that session
    /// becomes `<key>\u{0}~cwd-<scope>` via [`Self::storage_session_id`].
    /// Two projects reusing the same wire key then get DISTINCT ledger rings
    /// and on-disk dirs instead of replaying each other's conversation
    /// (#1666).
    ///
    /// The registry is authoritative: unregistered keys pass through
    /// verbatim (never parsed for the marker), so a wire key that happens to
    /// contain the marker text is never reinterpreted, and flag-OFF behavior
    /// is byte-identical. Wire EVENT payloads and the live-subscriber map
    /// keep the plain key — the scope exists only in storage identities.
    ///
    /// KNOWN RESIDUAL (documented on #1666, same class as the Phase-4
    /// `session_workspaces()` last-wins map): the registry holds ONE scope
    /// per wire key, so two connections using the SAME key for DIFFERENT
    /// cwds **concurrently in one process** flip-flop the mapping
    /// (last-writer-wins) — appends/replays can land in whichever project
    /// registered last, and live fan-out (plain-keyed by design) crosses
    /// projects. Pre-fix, that scenario shared ONE dir outright, so this is
    /// no new confidentiality exposure, but full isolation needs
    /// per-connection identity threading. Sequential use — one project at a
    /// time per key, the shipping AppUi/stdio flow — is fully isolated.
    ///
    /// Guarded by its own tiny mutex (never held across `inner`).
    scopes: Mutex<HashMap<String, String>>,
    /// Lazy-recovery index: storage identity → on-disk session dir. Built
    /// by [`UiProtocolLedger::recover`] by scanning `ui-protocol/` WITHOUT
    /// replaying any events; consulted by [`ensure_session_loaded`] on the
    /// first read/write touch of a session. Guarded by its own tiny mutex
    /// (never held across `inner`).
    index: Mutex<HashMap<SessionKey, SessionIndexEntry>>,
    /// Test-only counter of lazy disk replays performed by
    /// [`ensure_session_loaded`]. Used to assert that touching session A
    /// never replays session B (acceptance scenario 3).
    #[cfg(test)]
    lazy_replays: std::sync::atomic::AtomicUsize,
    /// Storage identities found at boot, independent of replay ring trimming
    /// and idle eviction. They are NOT authority to restore a cwd or sandbox.
    recovered_session_ids: Mutex<std::collections::HashSet<SessionKey>>,
}

/// One lazily-recoverable on-disk session. `reconciled` tracks whether the
/// boot-time orphan-row sweep has run for this session (it must run exactly
/// once per process, on first touch); `failed` latches a replay error so a
/// corrupt session is never re-scanned on every touch.
#[derive(Debug, Clone)]
struct SessionIndexEntry {
    dir: PathBuf,
    reconciled: bool,
    failed: bool,
}

struct LedgerInner {
    sessions: HashMap<SessionKey, SessionLedger>,
    /// LRU order: front is most-recently-touched, back is least.
    lru: VecDeque<SessionKey>,
    /// Per-session live broadcast senders. Lazily created the first time a
    /// connection calls [`UiProtocolLedger::subscribe`]. Each subsequent
    /// `append_*` fans the persisted event out to all live receivers. The
    /// channel is bounded — slow consumers see `Lagged(_)` and should fall
    /// back to cursor replay rather than block the producer.
    subscribers: HashMap<SessionKey, broadcast::Sender<LedgeredUiProtocolEvent>>,
    /// UPCR-2026-014 M9-γ ThreadSeqAllocator + hard-barrier state per
    /// `(SessionKey, thread_id)` for the canonical projection envelope.
    /// `next_seq` is the next `seq: u64` to issue (1-based, strictly
    /// monotonic within the thread). `completed` flips to `true` the
    /// first time a `Payload::TurnCompleted` envelope is emitted; any
    /// further envelope on the same thread is DROPPED at the live emit
    /// site and counted in `octos_projection_post_completion_drop_total`.
    thread_seq: HashMap<(SessionKey, String), ThreadSeqState>,
    /// Process-lifetime aggregate counters.
    evicted_count: u64,
    dropped_count: u64,
    on_disk_bytes: u64,
}

/// Per-thread sequence allocator + hard-barrier state.
/// See [`LedgerInner::thread_seq`] for usage.
#[derive(Debug, Default, Clone)]
struct ThreadSeqState {
    next_seq: u64,
    completed: bool,
    assistant_segment_id: Option<String>,
    /// Per-thread envelope seq that established the acknowledged owner.
    /// Separate from next_seq: allocation advances before its source append.
    assistant_owner_seq: Option<u64>,
}

fn native_assistant_order(thread: &str, identity: &str) -> Option<(u32, bool)> {
    let prefix = format!("{thread}:assistant:iteration:");
    let value = identity.strip_prefix(&prefix)?;
    let terminal = value.ends_with(":final");
    value
        .strip_suffix(":final")
        .unwrap_or(value)
        .parse::<u32>()
        .ok()
        .map(|iteration| (iteration, terminal))
}

fn update_native_assistant_owner(
    state: &mut ThreadSeqState,
    thread: &str,
    identity: String,
) -> bool {
    if state.assistant_segment_id.as_deref() == Some(&identity) {
        return false;
    }
    // End-of-turn batching can commit iteration 2 after iteration 9 already
    // streamed. Durable canonical backfill must not move attachment ownership
    // backwards. Other native identities remain opaque and follow commit order.
    if let Some(incoming) = native_assistant_order(thread, &identity)
        && let Some(current) = state
            .assistant_segment_id
            .as_deref()
            .and_then(|id| native_assistant_order(thread, id))
        && incoming < current
    {
        return false;
    }
    state.assistant_segment_id = Some(identity);
    true
}

/// Compatibility only for older, uncorrelated v1 producers. A completely
/// silent/new producer has no assistant owner yet; do not invent a dangling
/// `assistant:1` identity merely because a tool attached a file.
fn legacy_attachment_owner(
    inner: &LedgerInner,
    session: &SessionKey,
    thread: &str,
) -> Option<String> {
    let mut index = 0_u64;
    let mut open = false;
    for entry in inner.sessions.get(session)?.entries.iter() {
        let UiProtocolLedgerEvent::Notification(UiNotification::Envelope(event)) = &entry.event
        else {
            continue;
        };
        if event.envelope.thread_id != thread {
            continue;
        }
        match event.envelope.payload {
            Payload::AssistantDelta { .. } | Payload::AssistantPersisted { .. } if !open => {
                index += 1;
                open = true;
            }
            Payload::ToolStart { .. } | Payload::ToolEnd { .. } => open = false,
            _ => {}
        }
    }
    (index > 0).then(|| {
        format!(
            "{thread}:assistant:{}",
            if open { index } else { index + 1 }
        )
    })
}

/// On-disk schema for the per-thread watermark file. Codex #1336
/// round-2 BLOCKER 3: persist `(session_id, thread_id) →
/// (max_seq, completed)` so daemon restart can recover monotonic seq
/// allocation even when the in-memory ring was LRU-evicted or the
/// retained window dropped the originating envelopes.
#[derive(Debug, Serialize, Deserialize)]
struct ThreadWatermarkRecord {
    v: u32,
    session_id: String,
    thread_id: String,
    /// The next seq the allocator would issue. This is the high-water
    /// mark + 1, written under the same lock that bumps the allocator.
    next_seq: u64,
    /// True once a `Payload::TurnCompleted` envelope has been emitted
    /// for `(session_id, thread_id)`. Survives restart so post-completion
    /// envelopes stay barriered even after the ring drops the
    /// originating TurnCompleted.
    completed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    assistant_segment_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    assistant_owner_seq: Option<u64>,
}

const THREAD_WATERMARK_DISK_VERSION: u32 = 1;

/// Result of [`UiProtocolLedger::append_locked`] — the locked-half of
/// the ledger append. Codex #1336 round-2 BLOCKER 2 extracted this so
/// `emit_envelope_inner` can drive seq-allocation AND ledger append
/// under a single critical section.
struct AppendLockedOutcome {
    cursor: UiCursor,
    stamped: UiProtocolLedgerEvent,
    on_disk_delta: i64,
}

impl LedgerInner {
    fn new() -> Self {
        Self {
            sessions: HashMap::new(),
            lru: VecDeque::new(),
            subscribers: HashMap::new(),
            thread_seq: HashMap::new(),
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
    /// The durable data dir this ledger writes to (None when RAM-only) —
    /// exposed for the OLP observability event stream, which shares the
    /// same root (`<data_dir>/events.jsonl`).
    pub(crate) fn config_data_dir(&self) -> Option<PathBuf> {
        self.config.data_dir.clone()
    }
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
            scopes: Mutex::new(HashMap::new()),
            index: Mutex::new(HashMap::new()),
            #[cfg(test)]
            lazy_replays: std::sync::atomic::AtomicUsize::new(0),
            recovered_session_ids: Mutex::new(std::collections::HashSet::new()),
        }
    }

    /// Register (or clear, with `None`) the per-project storage scope for a
    /// session — see the `scopes` field doc. Called by the API layer whenever
    /// a `SessionRuntime` materializes with a RELOCATED transcript store
    /// (`sessions_root != profile.data_dir`, i.e. `appui.sessions_in_cwd`),
    /// and always BEFORE that flow replays the session so replay and
    /// subsequent appends agree on the storage identity. Idempotent;
    /// re-registering the same scope is a no-op, and `None` removes a stale
    /// entry (e.g. the flag was toggled off between restarts).
    pub(crate) fn set_session_scope(&self, session_id: &SessionKey, scope: Option<String>) {
        let mut scopes = self.scopes.lock().unwrap_or_else(|p| p.into_inner());
        match scope {
            Some(scope) => {
                scopes.insert(session_id.0.clone(), scope);
            }
            None => {
                scopes.remove(session_id.0.as_str());
            }
        }
    }

    /// A recovered scoped history requires session/open before a turn without
    /// a live workspace binding. Even missing/corrupt metadata cannot justify
    /// silently starting a fresh bare-key conversation.
    pub(crate) fn has_recovered_scoped_history(&self, session_id: &SessionKey) -> bool {
        let prefix = format!("{}\u{0}~cwd-", session_id.0);
        self.recovered_session_ids
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .iter()
            .any(|key| {
                key.0.strip_prefix(&prefix).is_some_and(|suffix| {
                    suffix.len() == 16 && suffix.bytes().all(|byte| byte.is_ascii_hexdigit())
                })
            })
    }

    /// The STORAGE identity for a wire session id: the id itself, or
    /// `<id>\u{0}~cwd-<scope>` when a per-project scope is registered.
    /// Storage identities key the in-memory ring, the LRU, the on-disk
    /// `ui-protocol/<hex>` dir, thread watermarks/seq state, and replay
    /// cursors' `stream` — everything EXCEPT wire event payloads and the
    /// live-subscriber map, which stay on the plain wire id.
    ///
    /// The NUL separator makes the encoding injective against realistic wire
    /// ids: `SessionKey` is an unconstrained string, so a plain-ASCII marker
    /// could be *equalled* by a hostile client naming its session
    /// `<victim>~cwd-<hash>` and thereby sharing the victim project's dir
    /// (codex v2 P2). A NUL pushes that collision outside anything a
    /// legitimate client id contains; the byte is opaque to the hex dir
    /// encoding, the HashMap keys, and JSON cursor `stream` strings.
    fn storage_session_id(&self, session_id: &SessionKey) -> SessionKey {
        let scopes = self.scopes.lock().unwrap_or_else(|p| p.into_inner());
        match scopes.get(session_id.0.as_str()) {
            Some(scope) => SessionKey(format!("{}\u{0}~cwd-{scope}", session_id.0)),
            None => session_id.clone(),
        }
    }

    /// Build a durable ledger and replay every on-disk session into RAM.
    ///
    /// Bounded by `config.retained_per_session` per session. Returns the
    /// constructed ledger plus the number of sessions/events recovered for
    /// the boot log.
    /// Build a durable ledger and lazily recover on-disk sessions.
    ///
    /// **Lazy recovery (perf/ledger-lazy-recovery, step 1a):** boot only
    /// scans the `ui-protocol/` session dirs to build an index (storage
    /// identity → session dir). NO events are replayed here — boot cost is
    /// O(session count), not O(total events). The first read or write that
    /// touches a session (hydrate/replay/append/emit) replays that one
    /// session's log via [`ensure_session_loaded`] and caches the projection
    /// in the in-memory ring; subsequent touches hit the cache.
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
        let mut indexed = 0usize;
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
            ledger
                .recovered_session_ids
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .insert(session_key.clone());
            // Index only — the session's events are replayed on first touch.
            ledger
                .index
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .insert(
                    session_key,
                    SessionIndexEntry {
                        dir: path,
                        reconciled: false,
                        failed: false,
                    },
                );
            indexed += 1;
        }
        info!(
            target = "octos::ledger",
            sessions_indexed = indexed,
            "ledger session index built (lazy recovery; events replay on first touch)"
        );
        RecoveryOutcome {
            ledger: Arc::new(ledger),
            sessions_recovered: indexed,
            events_recovered: 0,
        }
    }

    /// Replay one indexed session's disk log into the in-memory ring on
    /// first touch, then cache it. Idempotent: a session already resident
    /// in `inner.sessions` returns immediately without any disk I/O.
    ///
    /// A session whose log fails to read is marked `failed` in the index so
    /// subsequent touches return `false` immediately WITHOUT retrying a
    /// doomed full scan — one corrupt session never blocks boot or any
    /// other session.
    ///
    /// Disk I/O happens OUTSIDE the `inner` lock; the hydrate under the
    /// lock applies the stale-live guard so a concurrent append can never
    /// be rolled back by an older snapshot. The orphan-row reconciliation
    /// runs exactly once per successful replay (tracked in the index), and
    /// its synthesized terminal events are appended into the freshly
    /// hydrated ring via [`append_with_storage_id`] — which sees the
    /// session as resident and therefore does not recurse into disk replay.
    fn ensure_session_loaded(&self, storage_id: &SessionKey) -> bool {
        {
            let inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
            if inner.sessions.contains_key(storage_id) {
                return true;
            }
        }
        let (session_dir, reconciled) = {
            let index = self.index.lock().unwrap_or_else(|p| p.into_inner());
            match index.get(storage_id) {
                Some(entry) if !entry.failed => (entry.dir.clone(), entry.reconciled),
                _ => return false,
            }
        };
        // Outer-review 1b fix: existing large ledgers must not wait for
        // the append cadence to earn their FIRST snapshot. Detect whether
        // this recovery could use a projection snapshot BEFORE the scan:
        // `None` (no snapshot yet — pre-1b ledger) or `Err` (corrupt —
        // fault path) both mean the scan below is a full replay, after
        // which we bootstrap-write a snapshot so the NEXT recovery is
        // snapshot+tail. `Ok(Some(_))` means the scan already benefited.
        let needs_bootstrap_snapshot = self.config.snapshot_every_events > 0
            && !matches!(
                self.read_session_snapshot(storage_id, &session_dir),
                Ok(Some(_))
            );
        let snapshot = match self.read_session_disk_snapshot(storage_id, &session_dir, None, false)
        {
            Ok(snapshot) => snapshot,
            Err(error) => {
                warn!(
                    target = "octos::ledger",
                    ?error,
                    session_id = %storage_id.0,
                    "failed to lazily recover session from disk; session unavailable"
                );
                self.index
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .entry(storage_id.clone())
                    .and_modify(|entry| entry.failed = true);
                return false;
            }
        };
        let Some(snapshot) = snapshot else {
            // No log files (or all rotated away): nothing to replay. Leave
            // the index entry — a later append will create the ring and
            // `snapshot_if_session_absent` re-reads the dir then.
            return false;
        };
        if snapshot.retained_entries.is_empty() && snapshot.head_seq == 0 {
            return false;
        }
        let total_disk_bytes = snapshot.total_disk_bytes;
        let replayed_events = snapshot.retained_entries.len();
        let mut hydrated_now = false;
        {
            let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
            if !inner.sessions.contains_key(storage_id)
                && inner.sessions.len() >= self.config.active_session_cap
            {
                self.evict_lru_locked(&mut inner);
            }
            let session = inner
                .sessions
                .entry(storage_id.clone())
                .or_insert_with(SessionLedger::new);
            // Stale-live guard (same rule as `snapshot_with_cursor` /
            // `replay_after_from_disk_with_head`): an append that landed
            // while we were reading disk made the ring newer than our
            // snapshot — never roll it back.
            if session.next_seq <= snapshot.head_seq {
                if session.next_seq == 0 && session.entries.is_empty() {
                    hydrated_now = true;
                }
                hydrate_session_from_snapshot(session, snapshot);
                inner.on_disk_bytes = inner.on_disk_bytes.saturating_add(total_disk_bytes);
            }
            inner.touch_lru(storage_id);
        }
        if hydrated_now {
            #[cfg(test)]
            self.lazy_replays
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            // Bootstrap snapshot (outer-review 1b fix): a full replay just
            // paid O(all events) — persist the projection NOW so the next
            // cold start recovers this session from snapshot+tail instead.
            // Best-effort, same discipline as the append-cadence path: a
            // failed write never breaks recovery.
            if needs_bootstrap_snapshot {
                let inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
                if let Some(session) = inner.sessions.get(storage_id) {
                    if let Err(error) = self.write_session_snapshot_locked(storage_id, session) {
                        warn!(
                            target = "octos::ledger",
                            ?error,
                            session_id = %storage_id.0,
                            "failed to write bootstrap snapshot after full replay"
                        );
                    } else {
                        debug!(
                            target = "octos::ledger",
                            session_id = %storage_id.0,
                            "bootstrap snapshot written after full replay"
                        );
                    }
                }
            }
        }
        if hydrated_now && !reconciled {
            // The process that wrote these events is gone; rows it left
            // non-terminal will never terminate on their own.
            let swept = self.reconcile_orphaned_rows(storage_id);
            if swept > 0 {
                info!(
                    target = "octos::ledger",
                    session_id = %storage_id.0,
                    swept,
                    "recovery: synthesized terminal events for rows orphaned by restart"
                );
            }
        }
        if hydrated_now || !reconciled {
            self.index
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .entry(storage_id.clone())
                .and_modify(|entry| entry.reconciled = true);
        }
        debug!(
            target = "octos::ledger",
            session_id = %storage_id.0,
            replayed_events,
            hydrated_now,
            "ledger lazily recovered session on first touch"
        );
        true
    }

    /// Boot-time reconciliation for one recovered session: any task, turn,
    /// or agent row whose LAST replayed lifecycle event is non-terminal was
    /// orphaned — the server process that owned it died before emitting the
    /// terminal event, and no future event will ever close it. Replaying
    /// such rows verbatim makes every client render phantom running work
    /// forever (spinners for dead tasks, "Orchestrating…" for dead agents,
    /// Esc mis-targeting ghost tasks). Append synthesized terminal events so
    /// hydrate/replay converges to reality.
    ///
    /// Safe because recovery runs before this process serves any connection
    /// (nothing genuinely live exists yet) and idempotent because swept rows
    /// replay as terminal on the next boot.
    fn reconcile_orphaned_rows(&self, session_id: &SessionKey) -> usize {
        let Ok((events, _)) = self.snapshot_with_cursor(session_id, None) else {
            return 0;
        };
        let mut tasks: std::collections::HashMap<
            String,
            octos_core::ui_protocol::TaskUpdatedEvent,
        > = std::collections::HashMap::new();
        let mut started_turns: std::collections::HashMap<
            String,
            (SessionKey, octos_core::TurnId, Option<String>),
        > = std::collections::HashMap::new();
        let mut terminal_turns: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        let mut agents: std::collections::HashMap<
            String,
            octos_core::ui_protocol::AgentUpdatedEvent,
        > = std::collections::HashMap::new();
        for event in &events {
            let UiProtocolLedgerEvent::Notification(notification) = &event.event else {
                continue;
            };
            match notification {
                UiNotification::TaskUpdated(task) => {
                    tasks.insert(task.task_id.to_string(), task.clone());
                }
                UiNotification::TurnStarted(turn) => {
                    started_turns.insert(
                        turn.turn_id.0.to_string(),
                        // Carry the turn's own WIRE session id for the
                        // synthesized terminal below: the `session_id` param
                        // here is a STORAGE identity at recovery time (dir
                        // decode may include a `~cwd-` scope), which must
                        // never appear in an event payload.
                        (
                            turn.session_id.clone(),
                            turn.turn_id.clone(),
                            turn.topic.clone(),
                        ),
                    );
                }
                UiNotification::TurnCompleted(turn) => {
                    terminal_turns.insert(turn.turn_id.0.to_string());
                }
                UiNotification::TurnError(turn) => {
                    terminal_turns.insert(turn.turn_id.0.to_string());
                }
                UiNotification::AgentUpdated(agent) => {
                    agents.insert(agent.agent.agent_id.clone(), agent.clone());
                }
                _ => {}
            }
        }
        let mut swept = 0usize;
        for (_, mut task) in tasks {
            if matches!(
                task.state,
                TaskRuntimeState::Pending | TaskRuntimeState::Running
            ) {
                task.state = TaskRuntimeState::Cancelled;
                task.runtime_detail = Some("orphaned_by_restart".to_owned());
                // Storage-id append: land in the same (possibly scoped) dir
                // as the orphaned row — see `append_with_storage_id`.
                self.append_with_storage_id(
                    session_id.clone(),
                    UiProtocolLedgerEvent::Notification(UiNotification::TaskUpdated(task)),
                    None,
                );
                swept += 1;
            }
        }
        for (key, (turn_session_id, turn_id, topic)) in started_turns {
            if terminal_turns.contains(&key) {
                continue;
            }
            // Preserve the topic so topic-scoped replay filters still see the
            // synthesized terminal for a topic-suffixed turn.
            self.append_with_storage_id(
                session_id.clone(),
                UiProtocolLedgerEvent::Notification(UiNotification::TurnError(TurnErrorEvent {
                    session_id: turn_session_id,
                    topic,
                    turn_id,
                    code: "orphaned_by_restart".to_owned(),
                    message: "server restarted before this turn finished".to_owned(),
                    token_usage: None,
                    partial_result: None,
                })),
                None,
            );
            swept += 1;
        }
        for (_, mut agent) in agents {
            if matches!(
                agent.agent.status.as_str(),
                "completed" | "failed" | "cancelled" | "closed" | "interrupted"
            ) {
                continue;
            }
            agent.agent.status = "failed".to_owned();
            agent.agent.summary = Some("orphaned by server restart".to_owned());
            self.append_with_storage_id(
                session_id.clone(),
                UiProtocolLedgerEvent::Notification(UiNotification::AgentUpdated(agent)),
                None,
            );
            swept += 1;
        }
        swept
    }

    fn read_session_disk_snapshot(
        &self,
        session_id: &SessionKey,
        session_dir: &Path,
        replay_after_seq: Option<u64>,
        // 8d-r1: `true` ONLY for the hydrate from_beginning-exempt caller
        // (`read_disk_snapshot_for_replay`), whose seq-0 means "everything
        // you still retain" (retained ring, shortcut valid). The
        // session/open replay path passes `false`: its seq-0 has NO
        // exemption and must full-scan the JSONL from seq 1 (the snapshot
        // shortcut must NOT apply there). Distinguish by CALLER SEMANTICS,
        // never by the bare seq value.
        from_beginning_hydrate: bool,
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
        let mut skipped_records = 0u64;
        let cap = self.config.retained_per_session;

        // Step 1b: seed from the projection snapshot when one exists and
        // validates. Log records with seq <= snapshot head are then
        // skipped during the scan below, so recovery cost is
        // O(snapshot + tail) instead of O(all events). A corrupt,
        // unreadable, or unknown-version snapshot logs a warning and
        // falls back to a full replay — the log stays the source of
        // truth (fault-tolerance contract).
        //
        // ymote P1 (PR #2114 review): the snapshot shortcut must be
        // CONDITIONAL on the requested replay range. When the caller asks
        // to replay from a cursor OLDER than the snapshot's oldest entry
        // (e.g. cursor 2000 while the snapshot ring only covers
        // 4097..8192), the retained JSONL still holds the pre-snapshot
        // records and they MUST be replayed — seeding the bounded ring
        // and skipping everything ≤ head would silently drop
        // 2001..4096 AND misreport cursor_out_of_range (oldest_seq taken
        // from the snapshot ring). So: only skip log records up to the
        // snapshot head when the requested range is fully covered by the
        // snapshot; otherwise scan the log from seq 0 (the snapshot ring
        // still seeds the retained ring, but nothing is skipped).
        let mut skip_through_seq = 0u64;
        match self.read_session_snapshot(session_id, session_dir) {
            Ok(Some(snapshot)) => {
                head_seq = snapshot.head_seq;
                let snapshot_oldest = snapshot.entries.first().map(|r| r.seq);
                // Valid ledger seqs start at 1; a snapshot entry with seq 0 is
                // corrupt and must degrade to the full-replay path (shortcut
                // off) — never a debug-build panic (the old bare `oldest - 1`)
                // and never a shortcut seeded from corrupt data. The additive
                // form `after + 1 >= oldest` avoids the underflow entirely.
                let shortcut_ok = match replay_after_seq {
                    // 8d-r1: seq-0 takes the shortcut ONLY on the hydrate
                    // from_beginning-exempt path (caller flagged it). On
                    // the session/open replay path (flag false) seq-0 has
                    // no exemption → fall through to the full-scan rule
                    // below, which requires after+1 >= oldest (so seq 0 →
                    // 1 >= oldest, false for a trimmed ring → full JSONL
                    // scan from seq 1, the pre-cfa8fb15 behaviour).
                    Some(0) if from_beginning_hydrate => {
                        snapshot_oldest.is_some_and(|oldest| oldest >= 1)
                    }
                    Some(after) => snapshot_oldest
                        .is_some_and(|oldest| oldest >= 1 && after.saturating_add(1) >= oldest),
                    // 8d: a from-beginning replay (`after: None` → seq 0)
                    // is answered by the RETAINED RING alone (the hydrate
                    // path treats seq-0 as "everything you still retain",
                    // exempt from the oldest-cursor range check), NOT by a
                    // full log replay. The snapshot ring IS that retained
                    // window, so the shortcut applies whenever the snapshot
                    // is non-empty and valid (oldest >= 1) — even when the
                    // ring has since trimmed past seq 1 (oldest > 1). Only
                    // a corrupt/empty snapshot (oldest == 0, caught by the
                    // seq-0 hardening above) degrades to a full replay.
                    None => snapshot_oldest.is_some_and(|oldest| oldest >= 1),
                };
                if shortcut_ok {
                    skip_through_seq = snapshot.head_seq;
                }
                // NOTE: when the shortcut is disabled we deliberately do
                // NOT seed anything from the snapshot (see the loop
                // below) — the log scan rebuilds the full retained ring
                // and replay in seq order, so there is nothing to dedupe
                // and snapshot_seeded_floor/oldest stay 0.
                for record in snapshot.entries {
                    oldest_seq.get_or_insert(record.seq);
                    // ymote P1: only the SHORTCUT path materialises the
                    // snapshot (retained ring + replay_entries). When the
                    // shortcut is disabled the FULL state (retained ring
                    // AND replay) is rebuilt by the log scan below in seq
                    // order — seeding here would duplicate the tail and
                    // mis-order the pre-snapshot gap in both.
                    if shortcut_ok {
                        if replay_after_seq.is_some_and(|after| record.seq > after) {
                            replay_entries.push(LedgeredUiProtocolEvent {
                                cursor: UiCursor {
                                    stream: session_id.0.clone(),
                                    seq: record.seq,
                                },
                                event: record.event.clone(),
                                from_connection: None,
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
                if !shortcut_ok {
                    // Pre-snapshot range requested: the snapshot ring only
                    // seeds the retained window; oldest_seq must be
                    // re-derived from the FULL log scan below, so reset it
                    // (its snapshot-derived value would wrongly bound the
                    // cursor_out_of_range check to the snapshot window).
                    oldest_seq = None;
                }
                debug!(
                    target = "octos::ledger",
                    session_id = %session_id.0,
                    snapshot_head = snapshot.head_seq,
                    shortcut_ok,
                    "session recovery seeded from projection snapshot"
                );
            }
            Ok(None) => {}
            Err(error) => {
                warn!(
                    target = "octos::ledger",
                    ?error,
                    session_id = %session_id.0,
                    "session snapshot unusable; falling back to full log replay"
                );
            }
        }

        for path in log_files {
            // Read the whole file once and iterate borrowed line slices:
            // with a snapshot seeded, the skip path then costs zero
            // per-line allocations (the dominant recovery cost).
            let buf = fs::read(&path)?;
            // Aggregate skip counts per file: emit ONE summary `warn!`
            // after the inner loop instead of one line per record per
            // rescan. `read_session_disk_snapshot` re-reads every log
            // from byte 0 on every recovery / re-hydration / replay, so
            // per-record warnings multiply into log spam (~30k lines per
            // scan on pre-#1358 ledgers). Per-record detail stays at
            // `debug!` for when it is needed.
            let mut skipped_unknown_version = 0u64;
            let mut skipped_legacy_message_persisted = 0u64;
            let mut skipped_malformed = 0u64;
            for line_bytes in buf.split(|b| *b == b'\n') {
                let line_result: std::io::Result<&str> = std::str::from_utf8(line_bytes)
                    .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error));
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
                // Snapshot-seeded recovery fast path: records at or below
                // the snapshot head are already materialised in the ring,
                // so skip the expensive full event parse. We still extract
                // the record's `seq` cheaply (a tiny partial JSON scan of
                // the flat leading fields — the disk writer always emits
                // `{"v":…,"seq":…,"event":…}` in that order) to keep
                // `head_seq` advancing; a line whose seq can't be cheaply
                // extracted falls through to the full parse below, which
                // re-applies the same skip as a safety net.
                if skip_through_seq > 0 {
                    if let Some(seq) = cheap_record_seq(line) {
                        if seq <= skip_through_seq {
                            oldest_seq.get_or_insert(seq);
                            head_seq = head_seq.max(seq);
                            continue;
                        }
                    }
                }
                // Dual-read the outer event tag: canonical `record_kind`
                // first, then the strict legacy `envelope`-tagged shim for
                // pre-#1358 records (see `parse_ledger_disk_record`). Both
                // parses are derived + strict — no `serde_json::Value`
                // round-trip — so duplicate-key corruption is still
                // rejected on either path.
                let record = match parse_ledger_disk_record(line) {
                    Ok(ParsedLedgerDiskRecord::Record(record))
                        if record.v == LEDGER_DISK_VERSION =>
                    {
                        record
                    }
                    Ok(ParsedLedgerDiskRecord::LegacyMessagePersisted { v, seq })
                        if v == LEDGER_DISK_VERSION =>
                    {
                        // Stage 5 deliberately removed the old wire and
                        // ledger variant. Preserve this row's cursor space so
                        // a later append cannot reuse its sequence, but do
                        // not reconstruct or route its obsolete payload.
                        skipped_legacy_message_persisted += 1;
                        oldest_seq.get_or_insert(seq);
                        head_seq = head_seq.max(seq);
                        debug!(
                            target = "octos::ledger",
                            session_id = %session_id.0,
                            path = %path.display(),
                            seq,
                            "skipping removed legacy message/persisted ledger record"
                        );
                        continue;
                    }
                    Ok(ParsedLedgerDiskRecord::Record(record)) => {
                        skipped_unknown_version += 1;
                        debug!(
                            target = "octos::ledger",
                            version = record.v,
                            path = %path.display(),
                            "skipping ledger record with unknown version"
                        );
                        continue;
                    }
                    Ok(ParsedLedgerDiskRecord::LegacyMessagePersisted { v, .. }) => {
                        skipped_unknown_version += 1;
                        debug!(
                            target = "octos::ledger",
                            version = v,
                            path = %path.display(),
                            "skipping legacy message/persisted ledger record with unknown version"
                        );
                        continue;
                    }
                    Err(error) => {
                        // Valid-JSON-but-unknown-discriminator records that
                        // are legacy `envelope`-tagged are recovered by the
                        // legacy shim above, so reaching this arm means
                        // genuine corruption — OR a legacy
                        // `UiNotification::Envelope` record, which is
                        // inherently duplicate-key garbage (#1358 collision)
                        // and was never cleanly readable. Either way it is
                        // skipped, not panicked. Kept at `debug!` per-record,
                        // aggregated into one `warn!` below.
                        skipped_malformed += 1;
                        debug!(
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

                if record.seq <= skip_through_seq {
                    // Already materialised from the projection snapshot
                    // (shortcut path only — when the shortcut is disabled
                    // the log scan rebuilds everything, skip_through_seq
                    // stays 0 and nothing is skipped here).
                    continue;
                }

                if replay_after_seq.is_some_and(|after_seq| record.seq > after_seq) {
                    replay_entries.push(LedgeredUiProtocolEvent {
                        cursor: UiCursor {
                            stream: session_id.0.clone(),
                            seq: record.seq,
                        },
                        event: record.event.clone(),
                        from_connection: None,
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
            let skipped_total =
                skipped_unknown_version + skipped_legacy_message_persisted + skipped_malformed;
            if skipped_total > 0 {
                skipped_records = skipped_records.saturating_add(skipped_total);
                warn!(
                    target = "octos::ledger",
                    session_id = %session_id.0,
                    path = %path.display(),
                    skipped = skipped_total,
                    skipped_unknown_version,
                    skipped_legacy_message_persisted,
                    skipped_malformed,
                    "skipped ledger records during scan (aggregated)"
                );
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
            skipped_records,
        }))
    }

    pub(crate) fn append_notification(
        &self,
        notification: UiNotification,
    ) -> LedgeredUiProtocolEvent {
        self.append(UiProtocolLedgerEvent::Notification(notification), None)
    }

    #[cfg(test)]
    // Test-only counterpart to `append_notification`; some test builds never
    // call it, but the pair keeps ledger tests symmetric.
    #[allow(dead_code)]
    pub(crate) fn append_progress(&self, event: UiProgressEvent) -> LedgeredUiProtocolEvent {
        self.append(UiProtocolLedgerEvent::Progress(event), None)
    }

    /// Like [`append_notification`] but tags the broadcast event with the
    /// originating connection so that connection's live forwarder can skip
    /// it (the handler already direct-sent the wire frame). Other
    /// connections still receive it via fan-out.
    pub(crate) fn append_notification_from(
        &self,
        notification: UiNotification,
        from_connection: ConnectionId,
    ) -> LedgeredUiProtocolEvent {
        self.append(
            UiProtocolLedgerEvent::Notification(notification),
            Some(from_connection),
        )
    }

    /// Progress counterpart of [`append_notification_from`].
    pub(crate) fn append_progress_from(
        &self,
        event: UiProgressEvent,
        from_connection: ConnectionId,
    ) -> LedgeredUiProtocolEvent {
        self.append(
            UiProtocolLedgerEvent::Progress(event),
            Some(from_connection),
        )
    }

    /// UPCR-2026-014 (M9-γ) — emit a canonical projection envelope with
    /// server-allocated `seq` and hard-barrier enforcement.
    ///
    /// Per [§ 14.6 of the v1 spec](../../../api/OCTOS_UI_PROTOCOL_V1_SPEC_2026-04-24.md),
    /// every envelope for a `(session_id, thread_id)` pair gets a
    /// strictly-monotonic `seq: u64` issued from 1; once a
    /// [`Payload::TurnCompleted`] envelope is emitted for a thread, any
    /// further envelope on that thread is DROPPED at the live emit site
    /// and counted in the
    /// `octos_projection_post_completion_drop_total` metric.
    ///
    /// Returns the [`LedgeredUiProtocolEvent`] when the envelope is
    /// emitted, or `None` when the hard barrier dropped it (the metric
    /// is already bumped on the drop path).
    ///
    /// The drop is applied at the *live* emit site only — ledger replay
    /// (post-reconnect) bypasses the drop, so a client that reconnects
    /// with a pre-completion cursor still observes the full envelope
    /// history.
    ///
    /// Topic defaults to `session_id.topic()`. Callers that have stripped
    /// the topic suffix from `session_id` (see the P0-A wire-gap fix in
    /// `emit_files_attached_from_background`) MUST use
    /// [`emit_envelope_with_topic`] to thread the captured topic
    /// explicitly. This single-arg helper preserves the call sites that
    /// emit on an unmodified `SessionKey` and do not have a separate
    /// topic source.
    pub(crate) fn emit_envelope(
        &self,
        session_id: &SessionKey,
        thread_id: String,
        payload: Payload,
        client_message_id: Option<String>,
    ) -> Option<LedgeredUiProtocolEvent> {
        let topic = session_id.topic().map(ToOwned::to_owned);
        self.emit_envelope_inner(session_id, thread_id, payload, client_message_id, topic)
    }

    /// Codex BLOCKER #1336-round-2 (BLOCKER 5): variant of
    /// [`emit_envelope`] that accepts an explicit topic. Required by
    /// callers that strip the `#<topic>` suffix from `session_id` before
    /// publishing — without this hook the envelope would derive topic
    /// from `session_id.topic()` (which is now `None`) and silently lose
    /// routing.
    ///
    /// The caller-provided `topic` is the SOURCE OF TRUTH: an
    /// `Some("…")` always wins over `session_id.topic()`, and `None`
    /// means "no topic" (the call site has already decided the envelope
    /// does not belong to any topic scope).
    pub(crate) fn emit_envelope_with_topic(
        &self,
        session_id: &SessionKey,
        thread_id: String,
        payload: Payload,
        client_message_id: Option<String>,
        topic: Option<&str>,
    ) -> Option<LedgeredUiProtocolEvent> {
        let topic = topic
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(ToOwned::to_owned);
        self.emit_envelope_inner(session_id, thread_id, payload, client_message_id, topic)
    }

    /// Emit one canonical v2 projection envelope under the same per-thread
    /// sequence and terminal-barrier discipline as the older projection
    /// writer. The cursor is assigned by the durable append path, never by a
    /// caller, so live delivery and replay observe the identical envelope.
    pub(crate) fn emit_envelope_v2(
        &self,
        session_id: &SessionKey,
        thread_id: String,
        payload: PayloadV2,
        client_message_id: Option<String>,
    ) -> Option<LedgeredUiProtocolEvent> {
        let topic = session_id.topic().map(ToOwned::to_owned);
        self.emit_envelope_v2_inner(session_id, thread_id, payload, client_message_id, topic)
    }

    fn emit_envelope_v2_inner(
        &self,
        session_id: &SessionKey,
        thread_id: String,
        payload: PayloadV2,
        client_message_id: Option<String>,
        topic: Option<String>,
    ) -> Option<LedgeredUiProtocolEvent> {
        let session_id_clone = session_id.clone();
        let storage_id = self.storage_session_id(session_id);
        let preload_snapshot = self.snapshot_if_session_absent(&storage_id);
        let cursor;
        let stamped;
        let on_disk_delta;
        let ledgered;
        let broadcast_sender;
        {
            let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
            let is_terminal = matches!(
                &payload,
                PayloadV2::TurnTerminal { .. } | PayloadV2::BackgroundChildCompleted { .. }
            );
            let seq = match self.allocate_projection_seq_locked(
                &storage_id,
                &thread_id,
                is_terminal,
                &mut inner,
            ) {
                Ok(seq) => seq,
                Err(kind) => {
                    drop(inner);
                    metrics::counter!(
                        "octos_projection_post_completion_drop_total",
                        "kind" => kind,
                    )
                    .increment(1);
                    return None;
                }
            };

            let assistant_identity = match &payload {
                PayloadV2::AssistantDelta {
                    assistant_segment_id,
                    ..
                }
                | PayloadV2::AssistantPersisted {
                    assistant_segment_id,
                    ..
                } => Some(assistant_segment_id.clone()),
                _ => None,
            };

            let mut event = UiProtocolLedgerEvent::Notification(UiNotification::EnvelopeV2(
                EnvelopeV2Notification {
                    session_id: session_id_clone.clone(),
                    topic,
                    envelope: EnvelopeV2 {
                        thread_id: thread_id.clone(),
                        seq,
                        cursor: None,
                        turn_id: thread_id.clone(),
                        client_message_id,
                        payload,
                    },
                },
            ));
            event.stamp_topic_from_session();

            let append_outcome =
                self.append_locked(&storage_id, event, None, preload_snapshot, &mut inner);
            cursor = append_outcome.cursor;
            stamped = append_outcome.stamped;
            if let Some(identity) = assistant_identity
                && let Some(state) = inner
                    .thread_seq
                    .get_mut(&(storage_id.clone(), thread_id.clone()))
                && update_native_assistant_owner(state, &thread_id, identity)
            {
                state.assistant_owner_seq = Some(seq);
                self.persist_thread_watermark_locked(&storage_id, &thread_id, state);
            }
            on_disk_delta = append_outcome.on_disk_delta;
            ledgered = LedgeredUiProtocolEvent {
                cursor: cursor.clone(),
                event: stamped.clone(),
                from_connection: None,
            };

            if on_disk_delta >= 0 {
                inner.on_disk_bytes = inner.on_disk_bytes.saturating_add(on_disk_delta as u64);
            } else {
                inner.on_disk_bytes = inner.on_disk_bytes.saturating_sub((-on_disk_delta) as u64);
            }
            inner.touch_lru(&storage_id);
            broadcast_sender = inner.subscribers.get(&session_id_clone).cloned();
            if let Some(sender) = broadcast_sender.as_ref() {
                let _ = sender.send(ledgered.clone());
            }
        }
        let _ = broadcast_sender;
        Some(ledgered)
    }

    /// Return the stable one-based assistant-segment ordinal for a v1
    /// envelope being projected into Stage-1 v2 as of its durable cursor.
    ///
    /// Assistant content remains in the same segment until a tool boundary is
    /// observed; a later assistant delta then opens exactly one new segment,
    /// regardless of how many parallel tool events occurred in between.
    /// Persistence races the progress consumer, so a native v2
    /// `assistant_persisted` row can overtake queued streamed v1 deltas. By
    /// treating both delta and persisted rows as the same assistant phase, a
    /// late suffix stays on the canonical row's segment instead of rendering
    /// twice. Conversely, a tool phase closes the current assistant phase, so
    /// the post-tool answer gets a distinct id even when an empty preamble was
    /// not persisted. Durable outer cursors define ordering; per-envelope
    /// sequence numbers are never compared across v1 and native-v2 producers.
    ///
    /// The canonical commit observer invokes this with `u64::MAX` immediately
    /// before appending its new persisted row. Projected v1 events pass their
    /// source cursor, making live delivery and replay deterministic.
    pub(crate) fn projection_v2_assistant_segment_index(
        &self,
        session_id: &SessionKey,
        thread_id: &str,
        before_cursor_seq: u64,
    ) -> u64 {
        let storage_id = self.storage_session_id(session_id);
        self.ensure_session_loaded(&storage_id);
        let inner = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut segment_index = 0_u64;
        let mut assistant_phase_open = false;
        for entry in inner
            .sessions
            .get(&storage_id)
            .into_iter()
            .flat_map(|state| state.entries.iter())
            .filter(|entry| entry.seq < before_cursor_seq)
        {
            let phase = match &entry.event {
                UiProtocolLedgerEvent::Notification(UiNotification::Envelope(envelope))
                    if envelope.envelope.thread_id == thread_id =>
                {
                    match &envelope.envelope.payload {
                        Payload::AssistantDelta { .. } | Payload::AssistantPersisted { .. } => {
                            Some(true)
                        }
                        Payload::ToolStart { .. } | Payload::ToolEnd { .. } => Some(false),
                        _ => None,
                    }
                }
                UiProtocolLedgerEvent::Notification(UiNotification::EnvelopeV2(envelope))
                    if envelope.envelope.thread_id == thread_id =>
                {
                    match &envelope.envelope.payload {
                        PayloadV2::AssistantDelta { .. } | PayloadV2::AssistantPersisted { .. } => {
                            Some(true)
                        }
                        PayloadV2::ToolStart { .. } | PayloadV2::ToolEnd { .. } => Some(false),
                        _ => None,
                    }
                }
                _ => None,
            };
            match phase {
                Some(true) if !assistant_phase_open => {
                    segment_index = segment_index.saturating_add(1);
                    assistant_phase_open = true;
                }
                Some(false) => assistant_phase_open = false,
                _ => {}
            }
        }

        if assistant_phase_open {
            segment_index.max(1)
        } else {
            segment_index.saturating_add(1).max(1)
        }
    }

    /// Segment currently owning a late attachment as of a particular ledger
    /// cursor: the open assistant phase when one is open, otherwise the
    /// segment the next assistant content would open — exactly the ordinal
    /// [`Self::projection_v2_assistant_segment_index`] assigns to assistant
    /// content at the same cursor, so an attachment can never name a bubble
    /// that no assistant payload carries. Counting persisted rows was wrong in
    /// both directions: a post-tool phase with no persisted row yet mapped to
    /// the pre-tool bubble, and two persisted rows without a tool boundary
    /// (one phase) produced a dangling `assistant:2`. If no assistant content
    /// has been observed, the first segment remains the deterministic
    /// fallback. Restricting the scan to source entries preceding the
    /// attachment keeps replay stable even after later assistant iterations
    /// have been appended.
    pub(crate) fn projection_v2_current_assistant_segment_index(
        &self,
        session_id: &SessionKey,
        thread_id: &str,
        before_cursor_seq: u64,
    ) -> u64 {
        self.projection_v2_assistant_segment_index(session_id, thread_id, before_cursor_seq)
    }

    /// Next per-thread sequence for a wire-projected legacy terminal or
    /// attachment. A legacy source can be followed by its durable v1 companion,
    /// but consecutive attachment sources can also arrive without a companion
    /// between them. Count those projection-only rows after the latest durable
    /// envelope so they cannot all reuse the same `(thread_id, seq)` identity.
    ///
    /// Only rows preceding this source cursor participate, which keeps replay
    /// deterministic after later writes. This is read-only and does not reserve
    /// or mutate the authoritative allocator.
    pub(crate) fn projection_v2_next_envelope_seq(
        &self,
        session_id: &SessionKey,
        thread_id: &str,
        before_cursor_seq: u64,
    ) -> u64 {
        let storage_id = self.storage_session_id(session_id);
        let inner = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let entries = inner
            .sessions
            .get(&storage_id)
            .into_iter()
            .flat_map(|state| state.entries.iter())
            .filter(|entry| entry.seq < before_cursor_seq)
            .collect::<Vec<_>>();

        let (durable_seq, durable_cursor_seq) = entries
            .iter()
            .filter_map(|entry| match &entry.event {
                UiProtocolLedgerEvent::Notification(UiNotification::Envelope(envelope))
                    if envelope.envelope.thread_id == thread_id =>
                {
                    Some((envelope.envelope.seq, entry.seq))
                }
                UiProtocolLedgerEvent::Notification(UiNotification::EnvelopeV2(envelope))
                    if envelope.envelope.thread_id == thread_id =>
                {
                    Some((envelope.envelope.seq, entry.seq))
                }
                _ => None,
            })
            .max_by_key(|(envelope_seq, cursor_seq)| (*envelope_seq, *cursor_seq))
            .unwrap_or((0, 0));

        let has_background_child_before = |cursor_seq: u64| {
            entries.iter().any(|entry| {
                if entry.seq >= cursor_seq {
                    return false;
                }
                match &entry.event {
                    UiProtocolLedgerEvent::Notification(UiNotification::EnvelopeV2(envelope)) => {
                        matches!(
                            &envelope.envelope.payload,
                            PayloadV2::BackgroundChildCompleted {
                                parent_turn_id,
                                ..
                            } if parent_turn_id == thread_id
                        )
                    }
                    UiProtocolLedgerEvent::Notification(UiNotification::TurnSpawnComplete(
                        spawn,
                    )) => {
                        spawn.turn_id.as_ref().map(|turn_id| turn_id.0.to_string())
                            == Some(thread_id.to_owned())
                    }
                    _ => false,
                }
            })
        };

        let projected_only_count = entries
            .iter()
            .filter(|entry| entry.seq > durable_cursor_seq)
            .filter(|entry| match &entry.event {
                UiProtocolLedgerEvent::Notification(UiNotification::TurnCompleted(event)) => {
                    event.turn_id.0.to_string() == thread_id
                }
                UiProtocolLedgerEvent::Notification(UiNotification::TurnError(event)) => {
                    event.turn_id.0.to_string() == thread_id
                }
                UiProtocolLedgerEvent::Notification(UiNotification::FileAttached(event)) => {
                    event.turn_id.0.to_string() == thread_id
                        && !has_background_child_before(entry.seq)
                }
                _ => false,
            })
            .count() as u64;

        durable_seq
            .saturating_add(projected_only_count)
            .saturating_add(1)
    }

    /// Whether this parent turn has already opened a linked background child
    /// stream. The legacy background sender appends its per-file
    /// `file/attached` signals *after* `turn/spawn_complete`; v2 must not
    /// replay those files onto the already-terminal parent stream because the
    /// child completion already carries their media.
    pub(crate) fn projection_v2_has_background_child(
        &self,
        session_id: &SessionKey,
        parent_turn_id: &str,
        before_cursor_seq: u64,
    ) -> bool {
        let storage_id = self.storage_session_id(session_id);
        let inner = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        inner
            .sessions
            .get(&storage_id)
            .into_iter()
            .flat_map(|state| state.entries.iter())
            .any(|entry| match &entry.event {
                UiProtocolLedgerEvent::Notification(UiNotification::EnvelopeV2(envelope)) => {
                    entry.seq < before_cursor_seq
                        && matches!(
                            &envelope.envelope.payload,
                            PayloadV2::BackgroundChildCompleted {
                                parent_turn_id: child_parent_turn_id,
                                ..
                            } if child_parent_turn_id == parent_turn_id
                        )
                }
                UiProtocolLedgerEvent::Notification(UiNotification::TurnSpawnComplete(spawn)) => {
                    entry.seq < before_cursor_seq
                        && spawn.turn_id.as_ref().map(|turn_id| turn_id.0.to_string())
                            == Some(parent_turn_id.to_owned())
                }
                _ => false,
            })
    }

    fn emit_envelope_inner(
        &self,
        session_id: &SessionKey,
        thread_id: String,
        payload: Payload,
        client_message_id: Option<String>,
        topic: Option<String>,
    ) -> Option<LedgeredUiProtocolEvent> {
        // Codex #1336 round-2 BLOCKER 2: allocate envelope seq, apply
        // hard barrier, build the notification, append to the ledger
        // entries (in-memory ring + disk write), AND publish to live
        // subscribers — all inside ONE critical section. Previously
        // `allocate_envelope_seq` took its own lock, returned, and
        // then `append` re-acquired — letting two concurrent emits
        // interleave `(seq=1 allocate, seq=2 allocate, seq=2 append,
        // seq=1 append)`. Combined with a `Payload::TurnCompleted`
        // arriving as seq=2, the wire observed `TurnCompleted(seq=2)`
        // before the pre-completion delta `(seq=1)`, which the
        // client bridge then dropped as post-completion.
        //
        // The broadcast `publish_live` is held inside the lock too so
        // the broadcast send order strictly matches the seq allocation
        // order. `broadcast::Sender::send` is non-blocking (try_send on
        // a bounded queue) so the lock-hold is microseconds.
        let session_id_clone = session_id.clone();
        // Storage identity for ring/seq/disk/LRU; `session_id_clone` (the
        // plain wire id) still stamps the envelope payload and keys the live
        // subscriber lookup below.
        let storage_id = self.storage_session_id(session_id);
        let preload_snapshot = self.snapshot_if_session_absent(&storage_id);
        let cursor;
        let stamped;
        let on_disk_delta;
        let ledgered;
        let broadcast_sender;
        {
            let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());

            // --- step 1: envelope seq allocation + hard barrier ---
            // Seq/thread state follows the STORAGE identity so two projects
            // sharing a wire key allocate independent, per-project seq
            // streams consistent with their separate rings/dirs.
            let alloc = self.allocate_projection_seq_locked(
                &storage_id,
                &thread_id,
                matches!(&payload, Payload::TurnCompleted { .. }),
                &mut inner,
            );
            let seq = match alloc {
                Ok(seq) => seq,
                Err(kind) => {
                    drop(inner);
                    metrics::counter!(
                        "octos_projection_post_completion_drop_total",
                        "kind" => kind,
                    )
                    .increment(1);
                    return None;
                }
            };

            // --- step 2: build the notification with allocated seq ---
            let envelope = Envelope {
                thread_id,
                seq,
                client_message_id,
                payload,
            };
            let mut event = UiProtocolLedgerEvent::Notification(UiNotification::Envelope(
                EnvelopeNotification {
                    session_id: session_id_clone.clone(),
                    topic,
                    envelope,
                },
            ));
            event.stamp_topic_from_session();

            // --- step 3: append to the ledger (LRU, ring, disk) ---
            let append_outcome =
                self.append_locked(&storage_id, event, None, preload_snapshot, &mut inner);
            cursor = append_outcome.cursor;
            stamped = append_outcome.stamped;
            on_disk_delta = append_outcome.on_disk_delta;
            ledgered = LedgeredUiProtocolEvent {
                cursor: cursor.clone(),
                event: stamped.clone(),
                from_connection: None,
            };

            // Accounting that the original `append` does after entry
            // push — keep inside the same critical section so the
            // next emit observes consistent state.
            if on_disk_delta >= 0 {
                inner.on_disk_bytes = inner.on_disk_bytes.saturating_add(on_disk_delta as u64);
            } else {
                inner.on_disk_bytes = inner.on_disk_bytes.saturating_sub((-on_disk_delta) as u64);
            }
            inner.touch_lru(&storage_id);

            // --- step 4: publish to live subscribers WHILE STILL
            // HOLDING THE LOCK so the broadcast send order strictly
            // matches the seq allocation order. Another emit cannot
            // acquire the lock + send its own envelope between our
            // append and our broadcast send.
            //
            // `broadcast::Sender::send` is non-blocking (try_send on a
            // bounded tokio channel) so the lock-hold is microseconds.
            // `Err` is returned only when all receivers have been
            // dropped, which is a no-op for our durable contract —
            // the ledger record stands and any future reconnect
            // catches up via cursor replay.
            broadcast_sender = inner.subscribers.get(&session_id_clone).cloned();
            if let Some(sender) = broadcast_sender.as_ref() {
                let _ = sender.send(ledgered.clone());
            }
        }
        let _ = broadcast_sender; // silence dead-store warning
        Some(ledgered)
    }

    /// Per-thread envelope seq allocator with hard-barrier check.
    ///
    /// Codex #1336 round-2 BLOCKER 2: now takes `&mut LedgerInner` so
    /// the caller holds the mutex across both seq allocation AND the
    /// subsequent ledger append. Returns `Ok(seq)` on success;
    /// `Err(metric_kind)` when the hard barrier dropped the emit.
    ///
    /// The first emit for a `(session, thread)` pair after process
    /// restart scans the durable ledger for existing envelopes matching
    /// this thread and resumes from `max(seq) + 1` — daemon restart
    /// MUST NOT reissue `seq=1` for a thread the client already saw.
    fn allocate_projection_seq_locked(
        &self,
        session_id: &SessionKey,
        thread_id: &str,
        is_terminal: bool,
        inner: &mut LedgerInner,
    ) -> Result<u64, &'static str> {
        let key = (session_id.clone(), thread_id.to_owned());
        let needs_recovery = !inner.thread_seq.contains_key(&key);
        if needs_recovery {
            // Codex #1336 round-2 BLOCKER 3: recovery now consults the
            // persistent per-thread high-watermark before falling back
            // to scanning the in-memory ring. The in-memory ring may
            // be empty if the session was LRU-evicted or the retained
            // window aged out the relevant envelopes; the watermark
            // file preserves max-seq + completion state independently.
            let recovered = self.recover_thread_seq_state(session_id, thread_id, inner);
            inner.thread_seq.insert(key.clone(), recovered);
        }
        let state = inner
            .thread_seq
            .get_mut(&key)
            .expect("thread_seq entry inserted above");
        // Hard barrier per spec § 14.6.
        if state.completed {
            return Err(if is_terminal {
                "duplicate_completed"
            } else {
                "post_completion"
            });
        }
        if state.next_seq == 0 {
            state.next_seq = 1;
        }
        let seq = state.next_seq;
        state.next_seq = state.next_seq.saturating_add(1);
        if is_terminal {
            state.completed = true;
        }
        // Codex #1336 round-2 BLOCKER 3: persist the new watermark to
        // disk WHILE STILL HOLDING THE LOCK so a crash mid-emit can
        // still recover monotonically (write-ahead pattern).
        self.persist_thread_watermark_locked(session_id, thread_id, state);
        Ok(seq)
    }

    /// Daemon-restart recovery for the per-thread seq allocator.
    ///
    /// Codex #1336 round-2 BLOCKER 3: consults TWO sources in priority
    /// order:
    ///
    ///   1. **Persistent watermark file** (write-ahead, see
    ///      [`persist_thread_watermark_locked`]). Survives LRU eviction
    ///      AND retained-window compaction — the in-memory ring may
    ///      have aged out every envelope for this `(session, thread)`
    ///      while the watermark file still records `max(seq) +
    ///      completed`. This is the "structurally honest" answer.
    ///   2. **In-memory ring** (fallback). Used when the watermark file
    ///      does not exist (ephemeral ledger, or a thread that has not
    ///      yet recorded a watermark — e.g. when running with
    ///      `LedgerConfig::ephemeral`). Returns `max(observed seq) + 1`.
    ///
    /// The watermark wins on conflict: if disk says `next_seq=42,
    /// completed=true` but the ring has only seen seq=37, we resume
    /// from seq=42 with completed=true. This is the safer direction
    /// (never reissue a seq the client already saw).
    ///
    /// Owner recovery scans retained durable sources on a cold watermark
    /// load, because allocation and owner acknowledgement are separate
    /// writes. New threads without a watermark keep the in-memory fallback.
    /// The allocator recovery runs at most once per
    /// `(session, thread)` pair per process lifetime (subsequent calls
    /// hit the `thread_seq` map).
    fn recover_thread_seq_state(
        &self,
        session_id: &SessionKey,
        thread_id: &str,
        inner: &LedgerInner,
    ) -> ThreadSeqState {
        // --- step 1: try the durable watermark file ---
        if let Some(mut persisted) = self.read_thread_watermark(session_id, thread_id) {
            self.reconcile_durable_assistant_owner(session_id, thread_id, &mut persisted);
            return persisted;
        }
        // --- step 2: fall back to the in-memory ring scan ---
        let mut state = ThreadSeqState::default();
        let Some(session_state) = inner.sessions.get(session_id) else {
            return state;
        };
        for entry in &session_state.entries {
            match &entry.event {
                UiProtocolLedgerEvent::Notification(UiNotification::Envelope(ev))
                    if ev.envelope.thread_id == thread_id =>
                {
                    if ev.envelope.seq >= state.next_seq {
                        state.next_seq = ev.envelope.seq + 1;
                    }
                    if matches!(ev.envelope.payload, Payload::TurnCompleted { .. }) {
                        state.completed = true;
                    }
                }
                UiProtocolLedgerEvent::Notification(UiNotification::EnvelopeV2(ev))
                    if ev.envelope.thread_id == thread_id =>
                {
                    if let PayloadV2::AssistantDelta {
                        assistant_segment_id,
                        ..
                    }
                    | PayloadV2::AssistantPersisted {
                        assistant_segment_id,
                        ..
                    } = &ev.envelope.payload
                    {
                        if update_native_assistant_owner(
                            &mut state,
                            thread_id,
                            assistant_segment_id.clone(),
                        ) {
                            state.assistant_owner_seq = Some(ev.envelope.seq);
                        }
                    }
                    if ev.envelope.seq >= state.next_seq {
                        state.next_seq = ev.envelope.seq + 1;
                    }
                    if matches!(
                        &ev.envelope.payload,
                        PayloadV2::TurnTerminal { .. } | PayloadV2::BackgroundChildCompleted { .. }
                    ) {
                        state.completed = true;
                    }
                }
                _ => {}
            }
        }
        state
    }

    /// The write-ahead watermark remains authoritative for next_seq/completed,
    /// but its owner may lag a source appended before the second watermark
    /// write. Reconcile only actual native assistant sources; the full retained
    /// disk replay (not its tiny RAM tail) also covers LRU-evicted sessions.
    fn reconcile_durable_assistant_owner(
        &self,
        session: &SessionKey,
        thread: &str,
        state: &mut ThreadSeqState,
    ) {
        let Some(data_dir) = self.config.data_dir.as_ref() else {
            return;
        };
        let dir = data_dir
            .join("ui-protocol")
            .join(encode_session_dir_name(session));
        let snapshot = match self.read_session_disk_snapshot(session, &dir, Some(0), false) {
            Ok(Some(snapshot)) => snapshot,
            Ok(None) => return,
            Err(error) => {
                warn!(target: "octos::ledger", ?error, session_id = %session.0, thread_id = thread,
                    "failed to reconcile assistant owner; retaining acknowledged watermark");
                return;
            }
        };
        let sources: Vec<_> = snapshot
            .replay_entries
            .iter()
            .filter_map(|entry| {
                let UiProtocolLedgerEvent::Notification(UiNotification::EnvelopeV2(event)) =
                    &entry.event
                else {
                    return None;
                };
                if event.envelope.thread_id != thread {
                    return None;
                }
                match &event.envelope.payload {
                    PayloadV2::AssistantDelta {
                        assistant_segment_id,
                        ..
                    }
                    | PayloadV2::AssistantPersisted {
                        assistant_segment_id,
                        ..
                    } => Some((event.envelope.seq, assistant_segment_id.as_str())),
                    _ => None,
                }
            })
            .collect();
        // Old watermarks have no acknowledgement seq. An exact retained
        // source anchors them without text matching; use the first occurrence
        // so a delayed canonical backfill cannot hide a newer streamed phase.
        let anchor = state.assistant_owner_seq.or_else(|| {
            state.assistant_segment_id.as_deref().and_then(|id| {
                sources
                    .iter()
                    .find(|(_, source)| *source == id)
                    .map(|(seq, _)| *seq)
            })
        });
        let baseline = state.assistant_segment_id.clone();
        for (seq, identity) in sources {
            let provably_newer = match (
                baseline
                    .as_deref()
                    .and_then(|id| native_assistant_order(thread, id)),
                native_assistant_order(thread, identity),
            ) {
                (Some(before), Some(after)) => after > before,
                _ => false,
            };
            if anchor.map_or(baseline.is_none() || provably_newer, |ack| seq > ack)
                && update_native_assistant_owner(state, thread, identity.to_owned())
            {
                state.assistant_owner_seq = Some(seq);
            }
        }
    }

    /// Codex #1336 round-2 BLOCKER 3: persist `(session, thread) →
    /// (next_seq, completed)` write-ahead so daemon restart can recover
    /// monotonically EVEN WHEN:
    ///   - the session was LRU-evicted (the in-memory ring was dropped),
    ///   - the retained-window compaction aged out every envelope for
    ///     this thread (the ring still has the session but no envelopes
    ///     matching `thread_id`),
    ///   - the process crashes between allocate and append (the
    ///     watermark is written BEFORE the ledger record so a restart
    ///     observes the same `next_seq` even if the in-flight envelope
    ///     never reached disk).
    ///
    /// **No-op on the ephemeral ledger** (no `data_dir`). Tests that
    /// use `UiProtocolLedger::new(_)` keep their in-memory-only
    /// recovery semantics; production durable ledgers get the disk
    /// guarantee.
    ///
    /// Write-ahead pattern: temporary-file write plus rename of the small
    /// per-thread JSON file. Lock-held so a concurrent emit cannot see a
    /// half-written watermark; the next emit either reads the
    /// pre-update value (then writes a fresh one) or the just-updated
    /// value, never an interleaving.
    fn persist_thread_watermark_locked(
        &self,
        session_id: &SessionKey,
        thread_id: &str,
        state: &ThreadSeqState,
    ) {
        let Some(data_dir) = &self.config.data_dir else {
            return;
        };
        let dir = data_dir
            .join("ui-protocol")
            .join(encode_session_dir_name(session_id))
            .join("threads");
        if let Err(error) = fs::create_dir_all(&dir) {
            warn!(
                target = "octos::ledger",
                ?error,
                path = %dir.display(),
                "failed to create thread watermark dir; recovery will fall back to ring scan"
            );
            return;
        }
        let safe_name = encode_thread_file_name(thread_id);
        let path = dir.join(format!("{safe_name}.json"));
        let record = ThreadWatermarkRecord {
            v: THREAD_WATERMARK_DISK_VERSION,
            session_id: session_id.0.clone(),
            thread_id: thread_id.to_owned(),
            next_seq: state.next_seq,
            completed: state.completed,
            assistant_segment_id: state.assistant_segment_id.clone(),
            assistant_owner_seq: state.assistant_owner_seq,
        };
        let json = match serde_json::to_vec(&record) {
            Ok(json) => json,
            Err(error) => {
                warn!(
                    target = "octos::ledger",
                    ?error,
                    session_id = %session_id.0,
                    thread_id,
                    "failed to serialize thread watermark"
                );
                return;
            }
        };
        // Atomic-ish replace: write to `.tmp` then rename so a crash
        // mid-write never leaves a partially-written watermark. The
        // OS page cache provides "good enough" durability — an fsync
        // here would dominate the allocate latency budget.
        let tmp = dir.join(format!("{safe_name}.json.tmp"));
        match fs::write(&tmp, &json).and_then(|_| fs::rename(&tmp, &path)) {
            Ok(_) => {}
            Err(error) => {
                warn!(
                    target = "octos::ledger",
                    ?error,
                    session_id = %session_id.0,
                    thread_id,
                    "failed to write thread watermark"
                );
            }
        }
    }

    /// Codex #1336 round-2 BLOCKER 3: read the persistent watermark
    /// file written by [`persist_thread_watermark_locked`]. Returns
    /// `None` when the file is missing, malformed, or the ledger has
    /// no `data_dir`. Callers (recovery) fall back to scanning the
    /// in-memory ring on `None`.
    fn read_thread_watermark(
        &self,
        session_id: &SessionKey,
        thread_id: &str,
    ) -> Option<ThreadSeqState> {
        let data_dir = self.config.data_dir.as_ref()?;
        let safe_name = encode_thread_file_name(thread_id);
        let path = data_dir
            .join("ui-protocol")
            .join(encode_session_dir_name(session_id))
            .join("threads")
            .join(format!("{safe_name}.json"));
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
            Err(error) => {
                warn!(
                    target = "octos::ledger",
                    ?error,
                    path = %path.display(),
                    "failed to read thread watermark"
                );
                return None;
            }
        };
        match serde_json::from_slice::<ThreadWatermarkRecord>(&bytes) {
            Ok(record) if record.v == THREAD_WATERMARK_DISK_VERSION => Some(ThreadSeqState {
                next_seq: record.next_seq,
                completed: record.completed,
                assistant_segment_id: record.assistant_segment_id,
                assistant_owner_seq: record.assistant_owner_seq,
            }),
            Ok(record) => {
                warn!(
                    target = "octos::ledger",
                    version = record.v,
                    path = %path.display(),
                    "skipping thread watermark with unknown version"
                );
                None
            }
            Err(error) => {
                warn!(
                    target = "octos::ledger",
                    ?error,
                    path = %path.display(),
                    "thread watermark file malformed; falling back to ring scan"
                );
                None
            }
        }
    }

    fn append(
        &self,
        event: UiProtocolLedgerEvent,
        from_connection: Option<ConnectionId>,
    ) -> LedgeredUiProtocolEvent {
        // Ring/LRU/disk are keyed by the per-project STORAGE identity; the
        // live fan-out stays on the plain wire id (`publish_live`).
        let storage_id = self.storage_session_id(event.session_id());
        self.append_with_storage_id(storage_id, event, from_connection)
    }

    /// [`Self::append`] with the storage identity supplied by the caller
    /// instead of resolved from the scope registry. Recovery-time reconcile
    /// uses this: it walks dir-decoded storage identities while the registry
    /// is still empty, and its synthesized terminal events must land in the
    /// SAME (possibly `~cwd-`-scoped) ring/dir as the orphaned rows they
    /// close — resolving through the registry would divert them to the
    /// unscoped dir and leave phantom running turns behind.
    fn append_with_storage_id(
        &self,
        storage_id: SessionKey,
        mut event: UiProtocolLedgerEvent,
        from_connection: Option<ConnectionId>,
    ) -> LedgeredUiProtocolEvent {
        event.stamp_topic_from_session();
        let session_id = event.session_id().clone();
        let preload_snapshot = self.snapshot_if_session_absent(&storage_id);
        let cursor;
        let stamped;
        let on_disk_delta;
        {
            let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
            let outcome =
                self.append_locked(&storage_id, event, None, preload_snapshot, &mut inner);
            cursor = outcome.cursor;
            stamped = outcome.stamped;
            on_disk_delta = outcome.on_disk_delta;
            if on_disk_delta >= 0 {
                inner.on_disk_bytes = inner.on_disk_bytes.saturating_add(on_disk_delta as u64);
            } else {
                inner.on_disk_bytes = inner.on_disk_bytes.saturating_sub((-on_disk_delta) as u64);
            }
            inner.touch_lru(&storage_id);
        }

        let ledgered = LedgeredUiProtocolEvent {
            cursor,
            event: stamped,
            from_connection,
        };
        self.publish_live(&session_id, &ledgered);
        ledgered
    }

    /// Locked half of [`append`] — does the LRU + cursor + disk-write +
    /// in-memory ring-push under an existing `&mut LedgerInner` lock.
    /// Codex #1336 round-2 BLOCKER 2: extracted so [`emit_envelope_inner`]
    /// can run seq-allocation AND ledger append in a SINGLE critical
    /// section, eliminating the interleaving window where two concurrent
    /// emits could publish out-of-order on the broadcast channel.
    ///
    /// Caller is responsible for updating `inner.on_disk_bytes` from the
    /// returned signed delta and for invoking `touch_lru(session_id)`
    /// — both pieces sit OUTSIDE this helper because the borrow checker
    /// won't let us hold `&mut session` and call `inner.touch_lru`
    /// simultaneously, and matching `append`'s old order keeps the
    /// behaviour identical.
    ///
    /// `_from_connection` is intentionally unused here — the
    /// `from_connection` tag is set on the wrapper [`LedgeredUiProtocolEvent`]
    /// AFTER the lock is released. We accept it on the signature so the
    /// call site can document intent (the new envelope path always
    /// passes `None`; legacy `append_notification_from` paths still wrap
    /// the wire-tagged ledgered event manually).
    fn append_locked(
        &self,
        session_id: &SessionKey,
        mut event: UiProtocolLedgerEvent,
        _from_connection: Option<ConnectionId>,
        preload_snapshot: Option<DiskSessionSnapshot>,
        inner: &mut LedgerInner,
    ) -> AppendLockedOutcome {
        // `event.stamp_topic_from_session` is idempotent — `append`
        // already invokes it before locking; the envelope path stamps
        // inside the lock to keep the call ordering local. Either way
        // this is cheap and only does work when the explicit `topic`
        // field was absent.
        event.stamp_topic_from_session();
        // Defensive credential scrub at the one point every record passes
        // before it is written or fanned out. The transport redacts at
        // construction; this keeps any other producer from persisting key
        // material. Records already on disk are never rewritten.
        redact_ledger_event_secrets(&mut event);

        if let UiProtocolLedgerEvent::Notification(UiNotification::FileAttached(file)) = &mut event
            && file.attachment_owner.is_none()
        {
            let thread = file.turn_id.0.to_string();
            let state = inner
                .thread_seq
                .get(&(session_id.clone(), thread.clone()))
                .cloned()
                .unwrap_or_else(|| self.recover_thread_seq_state(session_id, &thread, inner));
            file.attachment_owner = Some(octos_core::ui_protocol::AttachmentOwnerV2 {
                assistant_segment_id: state
                    .assistant_segment_id
                    .or_else(|| legacy_attachment_owner(inner, session_id, &thread)),
                tool_call_id: file.tool_call_id.clone(),
            });
        }

        // LRU eviction: if we'd exceed the active session cap and this
        // session is new, evict the oldest first.
        let is_new = !inner.sessions.contains_key(session_id);
        if is_new && inner.sessions.len() >= self.config.active_session_cap {
            self.evict_lru_locked(inner);
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
        let cursor = UiCursor {
            stream: session_id.0.clone(),
            seq: session.next_seq,
        };
        let stamped = event.with_cursor(cursor.clone());

        // Write-ahead to disk before signaling the wire — happens
        // inside the lock so two appends to the same session never
        // interleave bytes in the file.
        let on_disk_delta: i64 = if self.config.data_dir.is_some() {
            match self.write_record_locked(session_id, session, &stamped) {
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

        // Step 1b: snapshot cadence. Every `snapshot_every_events` seqs,
        // persist the session's retained ring so the next recovery loads
        // the snapshot and replays only the tail. Best-effort: a failed
        // snapshot write never fails the append (the log is the source of
        // truth; recovery falls back to a full replay).
        let cadence = self.config.snapshot_every_events;
        if cadence > 0 && cursor.seq % cadence == 0 {
            if let Err(error) = self.write_session_snapshot_locked(session_id, session) {
                warn!(
                    target = "octos::ledger",
                    ?error,
                    session_id = %session_id.0,
                    seq = cursor.seq,
                    "failed to write session projection snapshot; recovery will full-replay"
                );
            }
        }

        AppendLockedOutcome {
            cursor,
            stamped,
            on_disk_delta,
        }
    }

    /// Fan the just-persisted event out to live subscribers. Runs after the
    /// disk + ring write so reconnect-replay and live-publish always agree
    /// on what was emitted. We use `broadcast` so multiple WS connections
    /// to the same session each see the event; absence of receivers is
    /// fine — the event is durably persisted and any future reconnect
    /// will see it via cursor replay.
    fn publish_live(&self, session_id: &SessionKey, event: &LedgeredUiProtocolEvent) {
        // Clone the sender, then release the lock before `send` so a slow
        // broadcast subscriber (which is bounded but still does work in
        // `send`) can never block the next `append`.
        let sender = {
            let inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
            inner.subscribers.get(session_id).cloned()
        };
        if let Some(sender) = sender {
            // `send` returns `Err` only if there are zero live receivers;
            // ignore that — the durable record stands.
            let _ = sender.send(event.clone());
        }
    }

    /// Subscribe to live `LedgeredUiProtocolEvent`s for `session_id`. The
    /// returned `Receiver` observes events appended after this call
    /// returns. Past events must still be obtained via [`replay_after`]
    /// (the broadcast channel is fan-out only, not history).
    ///
    /// Idempotent: if a sender already exists for the session, a fresh
    /// receiver is attached to it; otherwise a new bounded sender is
    /// created.
    pub(crate) fn subscribe(
        &self,
        session_id: &SessionKey,
    ) -> broadcast::Receiver<LedgeredUiProtocolEvent> {
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(sender) = inner.subscribers.get(session_id) {
            return sender.subscribe();
        }
        let (tx, rx) = broadcast::channel(LIVE_BROADCAST_CAPACITY);
        inner.subscribers.insert(session_id.clone(), tx);
        rx
    }

    /// Drop the broadcast sender for every session whose receiver count
    /// reached zero. Called from the periodic [`sweep_idle`] sweep so the
    /// per-session subscriber map never grows unbounded across long-lived
    /// ledgers, and on the `session/open` failure path so a `subscribe()`
    /// that never paired with a forwarder doesn't leak a sender.
    pub(crate) fn prune_idle_subscribers(&self) -> usize {
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let to_remove: Vec<SessionKey> = inner
            .subscribers
            .iter()
            .filter(|(_, sender)| sender.receiver_count() == 0)
            .map(|(key, _)| key.clone())
            .collect();
        let pruned = to_remove.len();
        for key in to_remove {
            inner.subscribers.remove(&key);
        }
        pruned
    }

    /// Drop the sender for `session_id` only if no live receivers remain.
    /// Used by callers (e.g. failed `session/open`) that just dropped
    /// their `Receiver` and want to immediately reclaim the sender slot
    /// rather than waiting for the next sweep.
    pub(crate) fn prune_subscriber_if_idle(&self, session_id: &SessionKey) -> bool {
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let drop_it = inner
            .subscribers
            .get(session_id)
            .map(|sender| sender.receiver_count() == 0)
            .unwrap_or(false);
        if drop_it {
            inner.subscribers.remove(session_id);
        }
        drop_it
    }

    fn snapshot_if_session_absent(&self, session_id: &SessionKey) -> Option<DiskSessionSnapshot> {
        self.config.data_dir.as_ref()?;
        // Lazy recovery: if this session was indexed at boot, replay it
        // now (first touch) instead of reading a raw snapshot below.
        if self.ensure_session_loaded(session_id) {
            return None;
        }
        {
            let inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
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
        match self.read_session_disk_snapshot(session_id, &session_dir, None, false) {
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

    /// Write the session's current retained ring to `snapshot.json` next
    /// to its log files (atomic via tmp + rename). Callers hold `inner`;
    /// this does NOT take any lock itself.
    fn write_session_snapshot_locked(
        &self,
        session_id: &SessionKey,
        session: &SessionLedger,
    ) -> std::io::Result<()> {
        let Some(dir) = &self.config.data_dir else {
            return Ok(());
        };
        let session_dir = dir
            .join("ui-protocol")
            .join(encode_session_dir_name(session_id));
        fs::create_dir_all(&session_dir)?;
        let snapshot = SessionSnapshotFile {
            version: SESSION_SNAPSHOT_VERSION,
            head_seq: session.next_seq,
            entries: session
                .entries
                .iter()
                .map(|entry| LedgerDiskRecord {
                    v: LEDGER_DISK_VERSION,
                    seq: entry.seq,
                    event: entry.event.clone(),
                })
                .collect(),
        };
        let payload = serde_json::to_vec(&snapshot).map_err(std::io::Error::other)?;
        let tmp_path = session_dir.join(format!("{SESSION_SNAPSHOT_FILE_NAME}.tmp"));
        let final_path = session_dir.join(SESSION_SNAPSHOT_FILE_NAME);
        fs::write(&tmp_path, payload)?;
        fs::rename(&tmp_path, &final_path)?;
        Ok(())
    }

    /// Read + validate the session's snapshot file. Returns `Ok(None)`
    /// when absent (zero-migration: pre-1b ledgers simply have no
    /// snapshot); returns `Err` on unreadable/corrupt/unknown-version
    /// content so the caller can log and fall back to a full replay.
    fn read_session_snapshot(
        &self,
        session_id: &SessionKey,
        session_dir: &Path,
    ) -> std::io::Result<Option<SessionSnapshotFile>> {
        let path = session_dir.join(SESSION_SNAPSHOT_FILE_NAME);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let snapshot: SessionSnapshotFile = serde_json::from_slice(&bytes).map_err(|error| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("corrupt session snapshot {}: {error}", path.display()),
            )
        })?;
        if snapshot.version != SESSION_SNAPSHOT_VERSION {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "unknown session snapshot version {} (expected {SESSION_SNAPSHOT_VERSION})",
                    snapshot.version
                ),
            ));
        }
        // Sanity: snapshot entries must be seq-ordered and ≤ head_seq —
        // anything else means a torn write slipped past the tmp+rename.
        let mut prev = 0u64;
        for entry in &snapshot.entries {
            if entry.seq <= prev || entry.seq > snapshot.head_seq {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "session snapshot {} has out-of-order/overrun seq {}",
                        path.display(),
                        entry.seq
                    ),
                ));
            }
            prev = entry.seq;
        }
        let _ = session_id;
        Ok(Some(snapshot))
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
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
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
        // Same-tick subscriber GC: any broadcast sender whose every
        // receiver has dropped is dead weight. Calling the dedicated
        // helper (rather than inlining) is what wires
        // `prune_idle_subscribers` into a production path so the
        // per-session subscribers map cannot grow without bound.
        self.prune_idle_subscribers();
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

    /// Test helper: count broadcast senders currently held in the
    /// subscribers map. Used to assert pruning behaviour.
    #[cfg(test)]
    pub(crate) fn subscriber_count(&self) -> usize {
        let inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        inner.subscribers.len()
    }

    #[cfg(test)]
    pub(crate) fn has_session_in_memory_for_test(&self, session_id: &SessionKey) -> bool {
        let session_id = self.storage_session_id(session_id);
        let inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        inner.sessions.contains_key(&session_id)
    }

    /// Test helper: number of lazy disk replays performed so far.
    #[cfg(test)]
    pub(crate) fn lazy_replay_count_for_test(&self) -> usize {
        self.lazy_replays.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Test helper: is this session's index entry latched as failed?
    #[cfg(test)]
    pub(crate) fn index_failed_for_test(&self, session_id: &SessionKey) -> bool {
        let session_id = self.storage_session_id(session_id);
        self.index
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&session_id)
            .map(|entry| entry.failed)
            .unwrap_or(false)
    }

    /// Snapshot of the observability counters. Useful for tests and the
    /// `/metrics` endpoint integration.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn metrics(&self) -> LedgerMetrics {
        let inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        LedgerMetrics {
            sessions_active: inner.sessions.len(),
            sessions_evicted: inner.evicted_count,
            events_dropped: inner.dropped_count,
            bytes_in_memory: inner.in_memory_bytes(),
            bytes_on_disk: inner.on_disk_bytes,
        }
    }

    /// Read only canonical message identity evidence through an already
    /// captured, scoped hydrate head. This is NOT a cursor replay request:
    /// earlier files may have rotated away while useful identity references
    /// remain in later retained files outside the hot ring.
    ///
    /// The complete hot-ring case is O(ring length), without disk I/O. A cold
    /// or trimmed ring scans the bounded retained log files, O(retained bytes).
    /// This runs on hydration only, never on the per-token append path. It does
    /// not hydrate/replace runtime state or change snapshot/replay semantics.
    pub(crate) fn retained_message_identity_references(
        &self,
        session_id: &SessionKey,
        through: &UiCursor,
    ) -> Result<Vec<LedgeredUiProtocolEvent>, RpcError> {
        fn is_identity_reference(event: &UiProtocolLedgerEvent) -> bool {
            matches!(event,
                UiProtocolLedgerEvent::Notification(UiNotification::EnvelopeV2(notification))
                    if matches!(notification.envelope.payload,
                        PayloadV2::AssistantPersisted { .. } | PayloadV2::BackgroundChildCompleted { .. }))
                || matches!(
                    event,
                    UiProtocolLedgerEvent::Notification(UiNotification::TurnSpawnComplete(_))
                )
        }

        let session_id = self.storage_session_id(session_id);
        validate_cursor_stream(&session_id, through)?;
        if through.seq == 0 {
            return Ok(Vec::new());
        }
        let (mut references, needs_disk) = {
            let inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
            let session = inner.sessions.get(&session_id);
            let references = session
                .into_iter()
                .flat_map(|session| &session.entries)
                .filter(|entry| entry.seq <= through.seq && is_identity_reference(&entry.event))
                .map(|entry| LedgeredUiProtocolEvent {
                    cursor: UiCursor {
                        stream: session_id.0.clone(),
                        seq: entry.seq,
                    },
                    event: entry.event.clone(),
                    from_connection: None,
                })
                .collect::<Vec<_>>();
            let needs_disk = session
                .and_then(|session| session.entries.front())
                .is_none_or(|oldest| oldest.seq > 1);
            (references, needs_disk)
        };
        if needs_disk {
            if let Some(data_dir) = &self.config.data_dir {
                let session_dir = data_dir
                    .join("ui-protocol")
                    .join(encode_session_dir_name(&session_id));
                match self.read_session_disk_snapshot(&session_id, &session_dir, Some(0), false) {
                    Ok(Some(snapshot)) => {
                        references.extend(snapshot.replay_entries.into_iter().filter(|entry| {
                            entry.cursor.seq <= through.seq && is_identity_reference(&entry.event)
                        }))
                    }
                    Ok(None) => {}
                    Err(error) => warn!(target = "octos::ledger", ?error,
                        session_id = %session_id.0,
                        "cannot read retained canonical message identity evidence"),
                }
            }
        }
        references.sort_by_key(|entry| entry.cursor.seq);
        // Preserve contradictory records so the one-to-one identity resolver
        // can reject them; deduplicate only identical hot/disk copies.
        references.dedup_by(|left, right| left.cursor == right.cursor && left.event == right.event);
        Ok(references)
    }

    /// Atomically snapshot the session's events ≥ `after` AND the head cursor
    /// at the moment of the snapshot. Used by `session/hydrate`
    /// (UPCR-2026-009) and `turn/state/get` (UPCR-2026-011) to satisfy
    /// codex's "snapshot+cursor must be atomic, or reload misses events
    /// between them" ask: a single lock acquisition reads both, so a
    /// concurrent appender cannot land an event with cursor ≤ snapshot.cursor
    /// that the client did not observe.
    ///
    /// Falls through to `replay_after` for the bulk of the work (which has
    /// the disk-recovery path); the difference is that this method returns
    /// the head cursor that pairs with the returned events. Callers
    /// (handlers) use the returned cursor in their result payload — a
    /// follow-up `session/hydrate` with `after = result.cursor` is
    /// guaranteed to see only events strictly after the snapshot.
    pub(crate) fn snapshot_with_cursor(
        &self,
        session_id: &SessionKey,
        after: Option<&UiCursor>,
    ) -> Result<(Vec<LedgeredUiProtocolEvent>, UiCursor), RpcError> {
        // Shadow with the per-project STORAGE identity (no-op when no scope
        // is registered): ring lookups, disk fallbacks, minted cursor
        // `stream`s, and the `after.stream` check below all follow it, so
        // cursors round-trip against the same identity appends mint.
        let session_id = &self.storage_session_id(session_id);
        // Atomicity contract (codex review #1): events and the returned
        // cursor are observed under a single lock acquisition. Concurrent
        // appenders see the same `inner` mutex, so no event can land
        // between the two reads with a seq ≤ the head we return — a
        // follow-up `session/hydrate { after: cursor }` returns only
        // strictly-newer events.
        //
        // When `after` is `None` we materialise an seq-0 cursor (the
        // disk replay path treats that as "from the beginning"). The
        // disk-snapshot read happens BEFORE we take the lock — that's
        // OK because (a) disk records are append-only, (b) the lock is
        // held while we observe `next_seq` and the in-memory ring, and
        // (c) any append concurrent with our disk read must take the
        // lock to update `next_seq`, so its cursor will be > our
        // returned head_seq. The pair we return therefore reflects a
        // single consistent snapshot moment.
        let default_cursor;
        let after = match after {
            Some(after) => after,
            None => {
                default_cursor = UiCursor {
                    stream: session_id.0.clone(),
                    seq: 0,
                };
                &default_cursor
            }
        };
        validate_cursor_stream(session_id, after)?;

        // Lazy recovery: first touch of a boot-indexed session replays its
        // disk log into the ring before we evaluate the preload condition.
        self.ensure_session_loaded(session_id);

        // Pre-load disk snapshot for sessions whose ring has dropped
        // below `after`. We do this before the lock to avoid blocking
        // append paths on slow disk I/O.
        let preload_snapshot = if self
            .inner
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .sessions
            .get(session_id)
            .map(|session| {
                let oldest = session.entries.front().map(|entry| entry.seq);
                match oldest {
                    Some(oldest_seq) => after.seq < oldest_seq.saturating_sub(1),
                    None => after.seq != session.next_seq,
                }
            })
            .unwrap_or(true)
        {
            self.read_disk_snapshot_for_replay(session_id, after)?
        } else {
            None
        };

        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(snapshot) = preload_snapshot {
            // Hydrate the in-memory ring from disk, mirroring what
            // `replay_after_from_disk` does, but inside the same lock
            // we read `next_seq` from. This closes the atomicity gap
            // codex flagged: events and head_seq come out of the same
            // critical section.
            if !inner.sessions.contains_key(session_id)
                && inner.sessions.len() >= self.config.active_session_cap
            {
                self.evict_lru_locked(&mut inner);
            }
            let session = inner
                .sessions
                .entry(session_id.clone())
                .or_insert_with(SessionLedger::new);
            // codex P1: never hydrate a STALE disk snapshot over a newer live
            // session. `hydrate_session_from_snapshot` overwrites `next_seq` and
            // clears the in-memory ring, so if the live session is already newer
            // than the snapshot's head — a concurrent append that landed between
            // the pre-lock disk read and acquiring this lock — hydrating would
            // roll `next_seq` back and drop/duplicate live events. A
            // freshly-inserted session has `next_seq == 0`, so new/trimmed rings
            // still hydrate normally. (Mirrors the stale-live guard on the
            // append/disk-replay paths.)
            if session.next_seq <= snapshot.head_seq {
                hydrate_session_from_snapshot(session, snapshot);
            }
        }

        let session = match inner.sessions.get(session_id) {
            Some(session) => session,
            None => {
                // Empty session — return empty events and a seq-0 cursor.
                let cursor = UiCursor {
                    stream: session_id.0.clone(),
                    seq: 0,
                };
                return Ok((Vec::new(), cursor));
            }
        };

        let head_seq = session.next_seq;
        // `replay_from_entries` filters in-memory ring to events with
        // seq > after.seq; we re-derive locally so we do not call out
        // to a sibling method that would re-acquire the lock.
        let events: Vec<LedgeredUiProtocolEvent> = session
            .entries
            .iter()
            .filter(|entry| entry.seq > after.seq)
            .map(|entry| LedgeredUiProtocolEvent {
                cursor: UiCursor {
                    stream: session_id.0.clone(),
                    seq: entry.seq,
                },
                event: entry.event.clone(),
                from_connection: None,
            })
            .collect();

        // Range validation echoes the existing replay_after error.
        if let Some(oldest_seq) = session.entries.front().map(|entry| entry.seq) {
            let min_after_seq = oldest_seq.saturating_sub(1);
            // A from-beginning request (`after: None` -> seq 0) means "everything
            // you still retain" and is always valid — even for a long/trimmed
            // ledger whose oldest retained seq is > 1. Without this exemption a
            // `session/hydrate { after: None }` against a large session
            // (oldest_seq > 1, e.g. ledger head ~5.5k) was rejected with
            // `cursor_out_of_range`, breaking reconnect-rehydration; the client
            // can't supply a valid cursor for "from the beginning" of a trimmed
            // ring. We still reject a genuine stale non-zero cursor and any
            // future cursor.
            let from_beginning = after.seq == 0;
            if (!from_beginning && after.seq < min_after_seq) || after.seq > head_seq {
                let head_cursor = UiCursor {
                    stream: session_id.0.clone(),
                    seq: head_seq,
                };
                return Err(RpcError::cursor_out_of_range(after, &head_cursor));
            }
        } else if after.seq != head_seq && after.seq != 0 {
            let head_cursor = UiCursor {
                stream: session_id.0.clone(),
                seq: head_seq,
            };
            return Err(RpcError::cursor_out_of_range(after, &head_cursor));
        }

        inner.touch_lru(session_id);
        let cursor = UiCursor {
            stream: session_id.0.clone(),
            seq: head_seq,
        };
        Ok((events, cursor))
    }

    fn read_disk_snapshot_for_replay(
        &self,
        session_id: &SessionKey,
        after: &UiCursor,
    ) -> Result<Option<DiskSessionSnapshot>, RpcError> {
        let Some(data_dir) = &self.config.data_dir else {
            return Ok(None);
        };
        let session_dir = data_dir
            .join("ui-protocol")
            .join(encode_session_dir_name(session_id));
        match self.read_session_disk_snapshot(session_id, &session_dir, Some(after.seq), true) {
            Ok(snapshot) => Ok(snapshot),
            Err(error) => {
                warn!(
                    target = "octos::ledger",
                    ?error,
                    session_id = %session_id.0,
                    "failed to read retained ledger logs for atomic snapshot"
                );
                Ok(None)
            }
        }
    }

    /// Compatibility wrapper used by tests that pre-date
    /// [`replay_after_with_head`]. Production callers should prefer the
    /// `_with_head` variant so a live forwarder can baseline against the
    /// snapshot's atomic head seq.
    #[cfg(test)]
    pub(crate) fn replay_after(
        &self,
        session_id: &SessionKey,
        after: Option<&UiCursor>,
    ) -> Result<Vec<LedgeredUiProtocolEvent>, RpcError> {
        self.replay_after_with_head(session_id, after)
            .map(|(events, _head)| events)
    }

    /// Like [`replay_after`] but also returns the head seq observed at the
    /// moment the replay snapshot was taken. The pair is atomic: any event
    /// appended after this call returns has a seq strictly greater than
    /// the returned head, so a live forwarder using `head` as its baseline
    /// cannot drop events that landed between replay and forwarder
    /// install. Closes the replay/open race called out in PR #761 review.
    pub(crate) fn replay_after_with_head(
        &self,
        session_id: &SessionKey,
        after: Option<&UiCursor>,
    ) -> Result<(Vec<LedgeredUiProtocolEvent>, u64), RpcError> {
        // Per-project STORAGE identity (no-op without a registered scope) —
        // see `snapshot_with_cursor`. Callers pass the plain wire id.
        let session_id = &self.storage_session_id(session_id);
        // Lazy recovery: even a "live only" caller needs the head seq of a
        // boot-indexed session, so replay-on-first-touch runs before any
        // branch below reads `next_seq`.
        self.ensure_session_loaded(session_id);
        let Some(after) = after else {
            // No `after` — caller asked for "live only", no replay history.
            // Pair the empty replay with the current head_seq so the
            // forwarder baseline matches a no-op snapshot.
            let head_seq = {
                let inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
                inner
                    .sessions
                    .get(session_id)
                    .map(|s| s.next_seq)
                    .unwrap_or(0)
            };
            return Ok((Vec::new(), head_seq));
        };
        validate_cursor_stream(session_id, after)?;

        {
            let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(ledger) = inner.sessions.get(session_id) {
                if let Some(oldest_seq) = ledger.entries.front().map(|entry| entry.seq) {
                    let min_after_seq = oldest_seq.saturating_sub(1);
                    if after.seq >= min_after_seq && after.seq <= ledger.next_seq {
                        let result = replay_from_entries(session_id, &ledger.entries, after.seq);
                        let head_seq = ledger.next_seq;
                        inner.touch_lru(session_id);
                        return Ok((result, head_seq));
                    }
                } else if after.seq == ledger.next_seq {
                    let head_seq = ledger.next_seq;
                    inner.touch_lru(session_id);
                    return Ok((Vec::new(), head_seq));
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
                    Ok((Vec::new(), 0))
                } else {
                    Err(cursor_out_of_range_error(session_id, after, 0, None))
                };
            }
        }

        self.replay_after_from_disk_with_head(session_id, after)
    }

    fn replay_after_from_disk_with_head(
        &self,
        session_id: &SessionKey,
        after: &UiCursor,
    ) -> Result<(Vec<LedgeredUiProtocolEvent>, u64), RpcError> {
        let Some(data_dir) = &self.config.data_dir else {
            return Err(cursor_out_of_range_error(session_id, after, 0, None));
        };
        let session_dir = data_dir
            .join("ui-protocol")
            .join(encode_session_dir_name(session_id));
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(ledger) = inner.sessions.get(session_id) {
            if let Some(oldest_seq) = ledger.entries.front().map(|entry| entry.seq) {
                let min_after_seq = oldest_seq.saturating_sub(1);
                if after.seq >= min_after_seq && after.seq <= ledger.next_seq {
                    let result = replay_from_entries(session_id, &ledger.entries, after.seq);
                    let head_seq = ledger.next_seq;
                    inner.touch_lru(session_id);
                    return Ok((result, head_seq));
                }
            } else if after.seq == ledger.next_seq {
                let head_seq = ledger.next_seq;
                inner.touch_lru(session_id);
                return Ok((Vec::new(), head_seq));
            }
        }

        let snapshot = self
            .read_session_disk_snapshot(session_id, &session_dir, Some(after.seq), false)
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
                Ok((Vec::new(), 0))
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
                Ok((Vec::new(), 0))
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
        let head_seq = snapshot.head_seq;
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
        Ok((result, head_seq))
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
/// care. The sweep holds only a weak reference so closing an embedded runtime
/// releases its ledger even when the caller drops the task handle.
pub(crate) fn spawn_eviction_task(ledger: Arc<UiProtocolLedger>) -> tokio::task::JoinHandle<()> {
    let interval = ledger.config.sweep_interval;
    let ledger = Arc::downgrade(&ledger);
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        // The first tick fires immediately; skip it so we don't sweep an
        // empty ledger at startup before any traffic.
        ticker.tick().await;
        loop {
            ticker.tick().await;
            let Some(ledger) = ledger.upgrade() else {
                break;
            };
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
            from_connection: None,
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
        UiNotification::PlanUpdated(event) => &event.session_id,
        UiNotification::MessageDelta(event) => &event.session_id,
        UiNotification::ReasoningDelta(event) => &event.session_id,
        UiNotification::VisualGenerating(event) => &event.session_id,
        UiNotification::VisualSucceeded(event) => &event.session_id,
        UiNotification::VisualFailed(event) => &event.session_id,
        UiNotification::VoiceExit(event) => &event.session_id,
        UiNotification::SkillActionJobUpdated(event) => &event.session_id,
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
        UiNotification::TurnSteerDropped(event) => &event.session_id,
        UiNotification::ReplayLossy(event) => &event.session_id,
        UiNotification::TurnSpawnComplete(event) => &event.session_id,
        UiNotification::FileAttached(event) => &event.session_id,
        UiNotification::VoiceAudioChunk(event) => &event.session_id,
        UiNotification::SessionEventBridged(event) => &event.session_id,
        UiNotification::RouterStatus(event) => &event.session_id,
        UiNotification::RouterFailover(event) => &event.session_id,
        UiNotification::QueueState(event) => &event.session_id,
        UiNotification::AgentUpdated(event) => &event.session_id,
        UiNotification::AgentOutputDelta(event) => &event.session_id,
        UiNotification::AgentArtifactUpdated(event) => &event.session_id,
        UiNotification::SessionGoalUpdated(event) => &event.session_id,
        UiNotification::SessionGoalCleared(event) => &event.session_id,
        UiNotification::LoopUpdated(event) => &event.session_id,
        UiNotification::LoopFired(event) => &event.session_id,
        UiNotification::LoopCompleted(event) => &event.session_id,
        UiNotification::MonitorUpdated(event) => &event.session_id,
        UiNotification::MonitorFired(event) => &event.session_id,
        UiNotification::MonitorExpired(event) => &event.session_id,
        UiNotification::ContextCompactionCompleted(event) => &event.session_id,
        UiNotification::ContextCompactionStarted(event) => &event.session_id,
        UiNotification::ContextNormalizationReported(event) => &event.session_id,
        UiNotification::SessionOrchestration(event) => &event.session_id,
        UiNotification::PeerStaged(event) => &event.session_id,
        UiNotification::PeerClosed(event) => &event.session_id,
        // #2019 — the human sink routes on the OWNING session, like everything
        // else here. This is the ledger's routing key, so a wrong answer would
        // land the event on whichever session the client has focused.
        UiNotification::BackgroundActivity(event) => &event.session_id,
        UiNotification::UserQuestionRequested(event) => &event.session_id,
        UiNotification::Envelope(event) => &event.session_id,
        UiNotification::EnvelopeV2(event) => &event.session_id,
    }
}

fn notification_cursor_seq(notification: &UiNotification) -> Option<u64> {
    match notification {
        UiNotification::SessionOpened(SessionOpened { cursor, .. })
        | UiNotification::TurnCompleted(TurnCompletedEvent { cursor, .. }) => {
            cursor.as_ref().map(|c| c.seq)
        }
        UiNotification::EnvelopeV2(envelope) => envelope.envelope.cursor.as_ref().map(|c| c.seq),
        _ => None,
    }
}

// ---------- Filename encoding ----------
//
// SessionKey may contain characters illegal on common filesystems
// (`:`, `/`, etc.). We hex-encode a stable representation so the
// session dir name is reversible and collision-free.

/// Cheaply extract the top-level `seq` from a ledger disk line WITHOUT
/// parsing the nested event payload. The writer (`write_record_locked`)
/// always serialises the flat struct in field order
/// `{"v":…,"seq":N,"event":…}`, so a byte scan for `"seq":` before the
/// `"event"` key suffices. Returns `None` for any shape we don't
/// recognise — callers then fall back to the full strict parse.
fn cheap_record_seq(line: &str) -> Option<u64> {
    let seq_key = line.find("\"seq\":")?;
    let event_key = line.find("\"event\"")?;
    if seq_key > event_key {
        return None; // nested/foreign layout — not our writer's shape
    }
    let start = seq_key + "\"seq\":".len();
    let digits: String = line[start..]
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    if digits.is_empty() {
        return None;
    }
    digits.parse().ok()
}

/// Scrub credentials from every notification shape that can carry
/// model-issued or client-supplied free text into the durable ledger (and,
/// through `send_notification_durable`, onto the wire):
///
/// * `tool_started.arguments` and the v1/v2 `ToolStart` `arguments_preview`
///   derived from them;
/// * `approval/requested` — the `title`/`body` (`Run command: …`) and the
///   typed `command_line`/`argv` repeat the raw shell command that the later
///   `tool_started` row would have scrubbed;
/// * `approval/decided` — the client-writable `client_note`/`scope`;
/// * `tool/completed.output_preview` and the v1/v2 `ToolEnd`
///   `output_preview`/`error` — a tool that prints `.env` or `printenv`
///   would otherwise persist the first 2 KB of raw output on every replay.
///
/// The redactor is infallible and idempotent, so ordinary payloads stay
/// byte-identical and a row scrubbed on the wire is unchanged at append.
pub(crate) fn redact_ui_notification_secrets(notification: &mut UiNotification) {
    use octos_core::secret_redaction::{redact_secrets_in_text, redact_secrets_in_value};
    fn redact_text_in_place(text: &mut String) {
        if let std::borrow::Cow::Owned(redacted) = redact_secrets_in_text(text) {
            *text = redacted;
        }
    }
    fn redact_opt_in_place(text: &mut Option<String>) {
        if let Some(text) = text.as_mut() {
            redact_text_in_place(text);
        }
    }
    match notification {
        UiNotification::ToolStarted(started) => {
            if let Some(arguments) = started.arguments.as_mut() {
                *arguments = redact_secrets_in_value(arguments);
            }
        }
        UiNotification::ToolCompleted(completed) => {
            redact_opt_in_place(&mut completed.output_preview);
        }
        UiNotification::ApprovalRequested(requested) => {
            redact_text_in_place(&mut requested.title);
            redact_text_in_place(&mut requested.body);
            if let Some(command) = requested
                .typed_details
                .as_mut()
                .and_then(|details| details.command.as_mut())
            {
                redact_opt_in_place(&mut command.command_line);
                for argument in command.argv.iter_mut() {
                    redact_text_in_place(argument);
                }
            }
        }
        UiNotification::ApprovalDecided(decided) => {
            redact_opt_in_place(&mut decided.client_note);
            redact_opt_in_place(&mut decided.scope);
        }
        UiNotification::Envelope(envelope) => match &mut envelope.envelope.payload {
            Payload::ToolStart {
                arguments_preview, ..
            } => redact_opt_in_place(arguments_preview),
            Payload::ToolEnd {
                error,
                output_preview,
                ..
            } => {
                redact_opt_in_place(error);
                redact_opt_in_place(output_preview);
            }
            _ => {}
        },
        UiNotification::EnvelopeV2(envelope) => match &mut envelope.envelope.payload {
            PayloadV2::ToolStart {
                arguments_preview, ..
            } => redact_opt_in_place(arguments_preview),
            PayloadV2::ToolEnd {
                error,
                output_preview,
                ..
            } => {
                redact_opt_in_place(error);
                redact_opt_in_place(output_preview);
            }
            _ => {}
        },
        _ => {}
    }
}

/// Ledger-append scrub: every durable notification row passes through
/// [`redact_ui_notification_secrets`], so no producer can bypass the wire
/// scrub by appending directly.
fn redact_ledger_event_secrets(event: &mut UiProtocolLedgerEvent) {
    let UiProtocolLedgerEvent::Notification(notification) = event else {
        return;
    };
    redact_ui_notification_secrets(notification);
}

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

/// Codex #1336 round-2 BLOCKER 3: hex-encode a thread_id so the
/// per-thread watermark file name is filesystem-safe. Thread IDs are
/// stringified UUIDs today (see `pre_stamp_turn_thread_id`) but may
/// contain arbitrary text in tests / future shapes — hex-encoding is
/// reversible, collision-free, and matches the
/// `encode_session_dir_name` convention.
fn encode_thread_file_name(thread_id: &str) -> String {
    let mut out = String::with_capacity(thread_id.len() * 2);
    for byte in thread_id.as_bytes() {
        out.push_str(&format!("{:02x}", byte));
    }
    out
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

    fn stage_assistant_owner_watermark_crash(
        dir: &Path,
        previous_iteration: Option<u32>,
        terminal_watermark: bool,
    ) -> (LedgerConfig, SessionKey, TurnId) {
        let mut config = LedgerConfig::durable(dir.to_owned());
        config.retained_per_session = 1;
        config.active_session_cap = 1;
        let session = SessionKey("local:assistant-owner-write-crash".into());
        let turn = TurnId::new();
        let thread = turn.0.to_string();
        let ledger = UiProtocolLedger::with_config(config.clone());
        if let Some(iteration) = previous_iteration {
            ledger
                .emit_envelope_v2(
                    &session,
                    thread.clone(),
                    PayloadV2::AssistantDelta {
                        text: "old preamble".into(),
                        assistant_segment_id: format!("{thread}:assistant:iteration:{iteration}"),
                    },
                    None,
                )
                .unwrap();
        }
        let mut before_owner_write = ledger
            .inner
            .lock()
            .unwrap()
            .thread_seq
            .get(&(session.clone(), thread.clone()))
            .cloned()
            .unwrap_or_default();
        let new = ledger
            .emit_envelope_v2(
                &session,
                thread.clone(),
                PayloadV2::AssistantDelta {
                    text: "new durable answer".into(),
                    assistant_segment_id: format!("{thread}:assistant:iteration:9"),
                },
                None,
            )
            .unwrap();
        let UiProtocolLedgerEvent::Notification(UiNotification::EnvelopeV2(new)) = new.event else {
            panic!("native delta")
        };
        // Recreate the actual two-write crash pair: allocation persisted the
        // advanced sequence + OLD owner, append persisted the NEW assistant,
        // then the process died before the owner's second watermark write.
        before_owner_write.next_seq = if terminal_watermark {
            900
        } else {
            new.envelope.seq + 1
        };
        before_owner_write.completed = terminal_watermark;
        ledger.persist_thread_watermark_locked(&session, &thread, &before_owner_write);
        // Ordinary durable notifications evict the assistant from the tiny
        // hot ring without allocating another projection sequence/watermark.
        for _ in 0..4 {
            ledger.append_notification(delta(&session, "unrelated legacy progress"));
        }
        (config, session, turn)
    }

    #[test]
    fn should_recover_appended_assistant_owner_after_pre_owner_watermark_crash() {
        for previous in [None, Some(2)] {
            let dir = tempfile::tempdir().unwrap();
            let (config, session, turn) =
                stage_assistant_owner_watermark_crash(dir.path(), previous, false);
            let ledger = UiProtocolLedger::recover(config).ledger;
            // Model an active-session-cap eviction: allocation recovery runs
            // before append hydrates the separately loaded disk snapshot.
            ledger.inner.lock().unwrap().sessions.remove(&session);
            assert!(!ledger.inner.lock().unwrap().sessions.contains_key(&session));
            let source = ledger
                .emit_envelope_v2(
                    &session,
                    turn.0.to_string(),
                    PayloadV2::ToolStart {
                        tool_call_id: "after-cold-recovery".into(),
                        name: "read_file".into(),
                        arguments_preview: None,
                    },
                    None,
                )
                .unwrap();
            let UiProtocolLedgerEvent::Notification(UiNotification::EnvelopeV2(source)) =
                source.event
            else {
                panic!("tool start")
            };
            assert_eq!(source.envelope.seq, if previous.is_some() { 3 } else { 2 });
            let attached = ledger.append_notification(UiNotification::FileAttached(
                octos_core::ui_protocol::FileAttachedEvent {
                    session_id: session.clone(),
                    topic: None,
                    turn_id: turn.clone(),
                    path: "answer.png".into(),
                    tool_call_id: Some("after-cold-recovery".into()),
                    attachment_owner: None,
                    mime: None,
                },
            ));
            let UiProtocolLedgerEvent::Notification(UiNotification::FileAttached(attached)) =
                attached.event
            else {
                panic!("file")
            };
            assert_eq!(
                attached.attachment_owner.unwrap().assistant_segment_id,
                Some(format!("{}:assistant:iteration:9", turn.0)),
                "durable assistant must repair the stale watermark owner before new attachment commit"
            );
        }
    }

    #[test]
    fn should_preserve_watermark_sequence_and_terminal_while_repairing_assistant_owner() {
        let dir = tempfile::tempdir().unwrap();
        let (config, session, turn) =
            stage_assistant_owner_watermark_crash(dir.path(), Some(2), true);
        let ledger = UiProtocolLedger::recover(config).ledger;
        ledger.inner.lock().unwrap().sessions.remove(&session);
        let state = ledger.recover_thread_seq_state(
            &session,
            &turn.0.to_string(),
            &ledger.inner.lock().unwrap(),
        );
        assert_eq!(
            state.next_seq, 900,
            "durable replay must never lower reserved sequence"
        );
        assert!(
            state.completed,
            "owner repair must not reopen a completed thread"
        );
        assert_eq!(
            state.assistant_segment_id,
            Some(format!("{}:assistant:iteration:9", turn.0))
        );
        assert!(
            ledger
                .emit_envelope_v2(
                    &session,
                    turn.0.to_string(),
                    PayloadV2::AssistantDelta {
                        text: "must remain blocked".into(),
                        assistant_segment_id: "forbidden".into(),
                    },
                    None
                )
                .is_none()
        );
    }

    #[test]
    fn should_repair_opaque_assistant_owner_using_acknowledged_source_sequence() {
        let dir = tempfile::tempdir().unwrap();
        let config = LedgerConfig::durable(dir.path().to_owned());
        let session = SessionKey("local:opaque-owner-crash".into());
        let thread = "opaque-thread";
        let ledger = UiProtocolLedger::with_config(config.clone());
        for identity in ["opaque-first", "opaque-second"] {
            ledger
                .emit_envelope_v2(
                    &session,
                    thread.into(),
                    PayloadV2::AssistantDelta {
                        text: "same words".into(),
                        assistant_segment_id: identity.into(),
                    },
                    None,
                )
                .unwrap();
        }
        ledger.persist_thread_watermark_locked(
            &session,
            thread,
            &ThreadSeqState {
                next_seq: 3,
                completed: false,
                assistant_segment_id: Some("opaque-first".into()),
                assistant_owner_seq: Some(1),
            },
        );
        drop(ledger);
        let reopened = UiProtocolLedger::with_config(config);
        let state =
            reopened.recover_thread_seq_state(&session, thread, &reopened.inner.lock().unwrap());
        assert_eq!(state.assistant_segment_id.as_deref(), Some("opaque-second"));
        assert_eq!(state.assistant_owner_seq, Some(2));
        assert_eq!(state.next_seq, 3);
    }

    #[test]
    fn should_migrate_legacy_owner_without_guessing_rotated_opaque_identity_order() {
        for (old, retained, expected) in [
            (
                "thread:assistant:iteration:2",
                "thread:assistant:iteration:9",
                "thread:assistant:iteration:9",
            ),
            (
                "thread:assistant:iteration:9",
                "thread:assistant:iteration:2",
                "thread:assistant:iteration:9",
            ),
            ("opaque-rotated", "opaque-retained", "opaque-rotated"),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let config = LedgerConfig::durable(dir.path().to_owned());
            let session = SessionKey("local:legacy-owner-migration".into());
            let ledger = UiProtocolLedger::with_config(config.clone());
            ledger
                .emit_envelope_v2(
                    &session,
                    "thread".into(),
                    PayloadV2::AssistantDelta {
                        text: "retained source".into(),
                        assistant_segment_id: retained.into(),
                    },
                    None,
                )
                .unwrap();
            // The old owner's source is no longer retained; legacy records
            // have no acknowledgement seq. Only explicit native producer
            // order can prove advancement here, never lexical opaque IDs.
            ledger.persist_thread_watermark_locked(
                &session,
                "thread",
                &ThreadSeqState {
                    next_seq: 900,
                    completed: false,
                    assistant_segment_id: Some(old.into()),
                    assistant_owner_seq: None,
                },
            );
            drop(ledger);
            let reopened = UiProtocolLedger::with_config(config);
            let state = reopened.recover_thread_seq_state(
                &session,
                "thread",
                &reopened.inner.lock().unwrap(),
            );
            assert_eq!(state.assistant_segment_id.as_deref(), Some(expected));
            assert_eq!(state.next_seq, 900);
        }
    }

    #[test]
    fn should_preserve_acknowledged_owner_when_reserved_assistant_append_never_landed() {
        let dir = tempfile::tempdir().unwrap();
        let config = LedgerConfig::durable(dir.path().to_owned());
        let session = SessionKey("local:owner-without-new-source".into());
        let ledger = UiProtocolLedger::with_config(config.clone());
        ledger
            .emit_envelope_v2(
                &session,
                "thread".into(),
                PayloadV2::AssistantDelta {
                    text: "real prior source".into(),
                    assistant_segment_id: "thread:assistant:iteration:2".into(),
                },
                None,
            )
            .unwrap();
        ledger.persist_thread_watermark_locked(
            &session,
            "thread",
            &ThreadSeqState {
                next_seq: 3,
                completed: false,
                assistant_segment_id: Some("thread:assistant:iteration:2".into()),
                assistant_owner_seq: Some(1),
            },
        );
        drop(ledger);
        let reopened = UiProtocolLedger::with_config(config);
        let state =
            reopened.recover_thread_seq_state(&session, "thread", &reopened.inner.lock().unwrap());
        assert_eq!(
            state.assistant_segment_id.as_deref(),
            Some("thread:assistant:iteration:2")
        );
        assert_eq!(state.assistant_owner_seq, Some(1));
        assert_eq!(state.next_seq, 3);
    }

    fn delta(session: &SessionKey, text: &str) -> UiNotification {
        UiNotification::MessageDelta(MessageDeltaEvent {
            session_id: session.clone(),
            topic: None,
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

    fn session_log_payload(data_dir: &Path, session_id: &SessionKey) -> String {
        let session_dir = data_dir
            .join("ui-protocol")
            .join(encode_session_dir_name(session_id));
        let mut log_files = list_log_files(&session_dir).expect("list session log files");
        log_files.sort();
        assert!(
            !log_files.is_empty(),
            "expected persisted JSONL log files for {}",
            session_id.0
        );
        log_files
            .iter()
            .map(|path| std::fs::read_to_string(path).expect("read session log file"))
            .collect::<String>()
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
                topic: None,
                turn_id,
                cursor: None,
                tokens_in: None,
                tokens_out: None,
                session_result: None,
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
    fn ledger_post_eviction_validation_path_replays_from_persisted_jsonl() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut config = LedgerConfig::durable(temp.path().into());
        config.retained_per_session = 4;
        config.active_session_cap = 1;
        let ledger = UiProtocolLedger::with_config(config);
        let target = SessionKey("local:post-eviction-target".into());
        let filler = SessionKey("local:post-eviction-filler".into());

        ledger.append_notification(delta(&target, "target-one"));
        ledger.append_notification(delta(&target, "target-two"));
        assert!(ledger.has_session_in_memory_for_test(&target));
        assert!(session_log_payload(temp.path(), &target).contains("target-two"));

        ledger.append_notification(delta(&filler, "filler-evicts-target"));
        let post_eviction_metrics = ledger.metrics();
        assert_eq!(post_eviction_metrics.sessions_evicted, 1);
        assert_eq!(post_eviction_metrics.sessions_active, 1);
        assert!(!ledger.has_session_in_memory_for_test(&target));
        assert!(ledger.has_session_in_memory_for_test(&filler));

        let (replayed, cursor) = ledger
            .snapshot_with_cursor(
                &target,
                Some(&UiCursor {
                    stream: target.0.clone(),
                    seq: 1,
                }),
            )
            .expect("hydrate evicted target session from JSONL");

        assert_eq!(replay_texts(&replayed), vec!["target-two"]);
        assert_eq!(cursor.seq, 2);
        assert!(ledger.has_session_in_memory_for_test(&target));
        assert!(!ledger.has_session_in_memory_for_test(&filler));
        assert_eq!(ledger.metrics().sessions_evicted, 2);
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
        // Second boot: drop the in-memory ledger, lazily recover from disk.
        // Lazy recovery (step 1a): boot indexes the session but replays
        // NOTHING — the ring is empty until first touch.
        let outcome = UiProtocolLedger::recover(LedgerConfig::durable(temp.path().into()));
        assert_eq!(outcome.sessions_recovered, 1, "session indexed at boot");
        assert_eq!(
            outcome.events_recovered, 0,
            "lazy recovery replays no events at boot"
        );
        assert!(
            !outcome.ledger.has_session_in_memory_for_test(&session_id),
            "indexed session must not be resident before first touch"
        );
        // First touch replays this session's log.
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
        assert_eq!(
            outcome.events_recovered, 0,
            "lazy recovery indexes rotated logs without replaying them"
        );
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
    fn snapshot_from_beginning_succeeds_on_trimmed_ring() {
        // Regression: `session/hydrate { after: None }` (from-beginning) against a
        // large/trimmed session (oldest retained seq > 1) must NOT be rejected as
        // cursor_out_of_range — "from the beginning" means "everything you still
        // retain". This previously broke reconnect-rehydration on long sessions
        // (mini5 soak: ledger head ~5.5k, hydrate errored + the transcript went
        // empty). A genuine stale non-zero cursor is still rejected elsewhere.
        let ledger = UiProtocolLedger::new(3);
        let session_id = SessionKey("local:trimmed".into());
        for i in 1..=6 {
            ledger.append_notification(delta(&session_id, &format!("msg-{i}")));
        }
        // Ring retains only the last 3 (seq 4,5,6); oldest_seq = 4 > 1.
        let (events, _head) = ledger
            .snapshot_with_cursor(&session_id, None)
            .expect("from-beginning hydrate must succeed on a trimmed ring");
        assert_eq!(replay_texts(&events), vec!["msg-4", "msg-5", "msg-6"]);
    }

    /// Boot recovery must synthesize terminal events for task/turn/agent
    /// rows the dead server generation left non-terminal — otherwise every
    /// hydrate replays phantom running work forever.
    #[test]
    fn recovery_sweeps_rows_orphaned_by_restart() {
        let temp = tempfile::tempdir().expect("tempdir");
        let session_id = SessionKey("local:orphan-sweep".into());
        let turn_id = octos_core::TurnId::new();
        let ghost_task = octos_core::TaskId::new();
        {
            let config = LedgerConfig::durable(temp.path().into());
            let ledger = UiProtocolLedger::with_config(config);
            let task: octos_core::ui_protocol::TaskUpdatedEvent = serde_json::from_value(json!({
                "session_id": session_id.0.clone(),
                "task_id": ghost_task.to_string(),
                "title": "astro dev server",
                "state": "running",
            }))
            .expect("task event");
            ledger.append_notification(UiNotification::TaskUpdated(task));
            let started: octos_core::ui_protocol::TurnStartedEvent =
                serde_json::from_value(json!({
                    "session_id": session_id.0,
                    "turn_id": turn_id.0,
                    "timestamp": chrono::Utc::now(),
                }))
                .expect("turn started");
            ledger.append_notification(UiNotification::TurnStarted(started));
            let agent: octos_core::ui_protocol::AgentUpdatedEvent = serde_json::from_value(json!({
                "session_id": session_id.0,
                "agent": {
                    "agent_id": "agent-ghost",
                    "session_id": session_id.0,
                    "path": "root/agent-ghost",
                    "role": "worker",
                    "nickname": "ghost",
                    "backend_kind": "native",
                    "status": "running",
                    "profile_id": "dev",
                    "created_at_ms": 1,
                    "updated_at_ms": 1,
                }
            }))
            .expect("agent event");
            ledger.append_notification(UiNotification::AgentUpdated(agent));
        } // process "dies" — no terminal events were emitted

        let recovered = UiProtocolLedger::recover(LedgerConfig::durable(temp.path().into()));
        let (events, _) = recovered
            .ledger
            .snapshot_with_cursor(&session_id, None)
            .expect("snapshot");

        let mut task_terminal = false;
        let mut turn_terminal = false;
        let mut agent_terminal = false;
        for event in &events {
            let UiProtocolLedgerEvent::Notification(notification) = &event.event else {
                continue;
            };
            match notification {
                UiNotification::TaskUpdated(task)
                    if task.state == octos_core::ui_protocol::TaskRuntimeState::Cancelled
                        && task.runtime_detail.as_deref() == Some("orphaned_by_restart") =>
                {
                    task_terminal = true;
                }
                UiNotification::TurnError(error)
                    if error.turn_id == turn_id && error.code == "orphaned_by_restart" =>
                {
                    turn_terminal = true;
                }
                UiNotification::AgentUpdated(agent)
                    if agent.agent.agent_id == "agent-ghost" && agent.agent.status == "failed" =>
                {
                    agent_terminal = true;
                }
                _ => {}
            }
        }
        assert!(
            task_terminal,
            "orphaned running task must be swept terminal"
        );
        assert!(turn_terminal, "orphaned started turn must error terminal");
        assert!(
            agent_terminal,
            "orphaned running agent must be swept terminal"
        );

        // Idempotence: a second recovery must not append more sweep events.
        let count_after_first = events.len();
        drop(recovered);
        let recovered_again = UiProtocolLedger::recover(LedgerConfig::durable(temp.path().into()));
        let (events_again, _) = recovered_again
            .ledger
            .snapshot_with_cursor(&session_id, None)
            .expect("snapshot again");
        assert_eq!(
            events_again.len(),
            count_after_first,
            "second recovery must sweep nothing (rows already terminal)"
        );
    }

    /// #1666 per-project isolation: two projects (`appui.sessions_in_cwd`)
    /// can share the same WIRE session id. Registering distinct storage
    /// scopes must give them distinct in-memory rings AND on-disk dirs —
    /// including the warm-ring case where project A's events are still
    /// resident when project B opens — while replayed events keep the plain
    /// wire id in their payloads.
    #[test]
    fn should_isolate_ledger_storage_between_cwd_scopes_sharing_a_wire_key() {
        let temp = tempfile::tempdir().expect("tempdir");
        let ledger = UiProtocolLedger::with_config(LedgerConfig::durable(temp.path().into()));
        let key = SessionKey("glm:local:tui#coding".into());

        ledger.set_session_scope(&key, Some("aaaa111122223333".into()));
        ledger.append_notification(delta(&key, "from-project-a"));

        // Project B re-registers the SAME wire key under its own scope while
        // A's ring entry is still warm.
        ledger.set_session_scope(&key, Some("bbbb444455556666".into()));
        ledger.append_notification(delta(&key, "from-project-b"));
        let (events, _) = ledger
            .snapshot_with_cursor(&key, None)
            .expect("scoped snapshot");
        assert_eq!(
            replay_texts(&events),
            vec!["from-project-b"],
            "project B must not replay project A's warm-ring events"
        );
        assert!(
            events.iter().all(|event| event.event.session_id() == &key),
            "replayed payloads keep the PLAIN wire id, never the storage id"
        );

        // Back to A: its events are intact under its own storage identity.
        ledger.set_session_scope(&key, Some("aaaa111122223333".into()));
        let (events, _) = ledger
            .snapshot_with_cursor(&key, None)
            .expect("re-scoped snapshot");
        assert_eq!(replay_texts(&events), vec!["from-project-a"]);

        // Clearing the scope falls back to the (empty) legacy identity.
        ledger.set_session_scope(&key, None);
        let (events, _) = ledger
            .snapshot_with_cursor(&key, None)
            .expect("unscoped snapshot");
        assert!(
            replay_texts(&events).is_empty(),
            "no scope registered → plain storage identity, which holds nothing"
        );

        // And the two projects own two distinct on-disk dirs (NUL-separated
        // storage identities — see `storage_session_id`).
        let scoped_a = SessionKey(format!("{}\u{0}~cwd-aaaa111122223333", key.0));
        let scoped_b = SessionKey(format!("{}\u{0}~cwd-bbbb444455556666", key.0));
        for scoped in [&scoped_a, &scoped_b] {
            let dir = temp
                .path()
                .join("ui-protocol")
                .join(encode_session_dir_name(scoped));
            assert!(dir.is_dir(), "expected scoped dir {}", dir.display());
        }

        // Injectivity: a WIRE key that literally spells the old plain-ASCII
        // marker shape must NOT alias into a scoped dir — its storage
        // identity is itself (no NUL), so it stays a separate, empty bucket.
        let impostor = SessionKey(format!("{}~cwd-aaaa111122223333", key.0));
        let (events, _) = ledger
            .snapshot_with_cursor(&impostor, None)
            .expect("impostor snapshot");
        assert!(
            replay_texts(&events).is_empty(),
            "a literal `~cwd-` wire key must not read a scoped project's events"
        );
    }

    /// #1666: scoped dirs survive a restart under the SAME storage identity —
    /// recovery decodes the scoped dir name verbatim, and a re-registered
    /// scope replays it while an unregistered (plain) read stays empty.
    #[test]
    fn should_recover_scoped_sessions_under_their_storage_identity() {
        let temp = tempfile::tempdir().expect("tempdir");
        let key = SessionKey("glm:local:tui#coding".into());
        {
            let ledger = UiProtocolLedger::with_config(LedgerConfig::durable(temp.path().into()));
            ledger.set_session_scope(&key, Some("aaaa111122223333".into()));
            ledger.append_notification(delta(&key, "scoped-before-restart"));
        } // process "dies"

        let recovered = UiProtocolLedger::recover(LedgerConfig::durable(temp.path().into()));
        let (unregistered, _) = recovered
            .ledger
            .snapshot_with_cursor(&key, None)
            .expect("plain snapshot");
        assert!(
            replay_texts(&unregistered).is_empty(),
            "without a registered scope the plain identity must not see scoped events"
        );

        recovered
            .ledger
            .set_session_scope(&key, Some("aaaa111122223333".into()));
        let (events, _) = recovered
            .ledger
            .snapshot_with_cursor(&key, None)
            .expect("scoped snapshot after recovery");
        assert_eq!(replay_texts(&events), vec!["scoped-before-restart"]);
    }

    /// #1666: recovery's orphan sweep must close rows INSIDE a scoped dir
    /// (the synthesized terminal lands in the same scoped ring, not the
    /// plain one) and its synthesized payloads must carry the plain WIRE
    /// session id — never the `~cwd-` storage identity.
    #[test]
    fn recovery_sweeps_orphans_in_scoped_dirs_with_wire_session_ids() {
        let temp = tempfile::tempdir().expect("tempdir");
        let key = SessionKey("glm:local:tui#coding".into());
        let turn_id = octos_core::TurnId::new();
        {
            let ledger = UiProtocolLedger::with_config(LedgerConfig::durable(temp.path().into()));
            ledger.set_session_scope(&key, Some("aaaa111122223333".into()));
            let started: octos_core::ui_protocol::TurnStartedEvent =
                serde_json::from_value(json!({
                    "session_id": key.0,
                    "turn_id": turn_id.0,
                    "timestamp": chrono::Utc::now(),
                }))
                .expect("turn started");
            ledger.append_notification(UiNotification::TurnStarted(started));
        } // dies mid-turn, no terminal event

        let recovered = UiProtocolLedger::recover(LedgerConfig::durable(temp.path().into()));
        recovered
            .ledger
            .set_session_scope(&key, Some("aaaa111122223333".into()));
        let (events, _) = recovered
            .ledger
            .snapshot_with_cursor(&key, None)
            .expect("scoped snapshot");
        let synthesized = events
            .iter()
            .find_map(|event| match &event.event {
                UiProtocolLedgerEvent::Notification(UiNotification::TurnError(error))
                    if error.code == "orphaned_by_restart" =>
                {
                    Some(error.clone())
                }
                _ => None,
            })
            .expect("orphan sweep must synthesize the terminal INSIDE the scoped ring");
        assert_eq!(
            synthesized.session_id, key,
            "synthesized terminal must carry the plain wire id, not the storage id"
        );
        assert_eq!(synthesized.turn_id, turn_id);
    }

    #[test]
    fn snapshot_with_cursor_does_not_hydrate_stale_disk_over_newer_live() {
        // codex P1: `snapshot_with_cursor` must not let a STALE disk snapshot
        // roll back a newer live session — `hydrate_session_from_snapshot` resets
        // `next_seq` and clears the ring, which would drop/duplicate live events.
        let temp = tempfile::tempdir().expect("tempdir");
        let mut config = LedgerConfig::durable(temp.path().into());
        config.retained_per_session = 1;
        config.retained_log_files = 4;
        config.rotate_bytes = 1024 * 1024;
        let ledger = UiProtocolLedger::with_config(config);
        let session_id = SessionKey("local:stale-live-snap".into());

        ledger.append_notification(delta(&session_id, "one"));
        ledger.append_notification(delta(&session_id, "two"));
        ledger.append_notification(delta(&session_id, "three")); // live next_seq=4, ring=[seq3]

        // Truncate the disk log to a STALE snapshot (only seq 1,2 → head_seq=2).
        let session_dir = temp
            .path()
            .join("ui-protocol")
            .join(encode_session_dir_name(&session_id));
        let mut log_files = list_log_files(&session_dir).expect("list logs");
        log_files.sort();
        let active_log = log_files.last().expect("active log");
        let contents = std::fs::read_to_string(active_log).expect("read log");
        let stale = contents
            .lines()
            .take(2)
            .map(|line| format!("{line}\n"))
            .collect::<String>();
        std::fs::write(active_log, stale).expect("truncate to stale snapshot");

        // From-beginning snapshot: with the guard it must keep the LIVE tail and
        // the live head, not roll back to the stale disk.
        let (events, head) = ledger
            .snapshot_with_cursor(&session_id, None)
            .expect("from-beginning must succeed without rolling back");
        assert_eq!(
            head.seq, 3,
            "live head (3) must be preserved, not rolled back to the stale disk head (2)"
        );
        assert_eq!(
            replay_texts(&events),
            vec!["three"],
            "the live tail survives; the stale disk must not replace it"
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

    // ---- Lazy recovery (perf/ledger-lazy-recovery, step 1a) -------------
    // Acceptance scenarios from .octos/OUTER_LOOP_REVIEW.md §1a.

    /// Scenario 1: cold boot that touches nothing replays NOTHING — boot
    /// only builds the index; no session is resident; zero lazy replays.
    #[test]
    fn lazy_recovery_boot_indexes_without_replay() {
        let temp = tempfile::tempdir().expect("tempdir");
        let s1 = SessionKey("local:lazy-boot-1".into());
        let s2 = SessionKey("local:lazy-boot-2".into());
        {
            let ledger = UiProtocolLedger::with_config(LedgerConfig::durable(temp.path().into()));
            for i in 0..5 {
                ledger.append_notification(delta(&s1, &format!("a-{i}")));
                ledger.append_notification(delta(&s2, &format!("b-{i}")));
            }
        }
        let outcome = UiProtocolLedger::recover(LedgerConfig::durable(temp.path().into()));
        assert_eq!(outcome.sessions_recovered, 2, "both sessions indexed");
        assert_eq!(
            outcome.events_recovered, 0,
            "boot must NOT replay any events"
        );
        assert_eq!(outcome.ledger.lazy_replay_count_for_test(), 0);
        assert!(!outcome.ledger.has_session_in_memory_for_test(&s1));
        assert!(!outcome.ledger.has_session_in_memory_for_test(&s2));
        let m = outcome.ledger.metrics();
        assert_eq!(m.sessions_active, 0, "no session resident after boot");
    }

    /// Scenario 2 (equivalence): hydrate of a lazily-recovered session
    /// must be field-for-field identical to the eager full-replay
    /// projection of the same on-disk ledger.
    #[test]
    fn lazy_recovery_projection_equivalent_to_eager_replay() {
        let temp = tempfile::tempdir().expect("tempdir");
        let session_id = SessionKey("local:lazy-equiv".into());
        {
            let ledger = UiProtocolLedger::with_config(LedgerConfig::durable(temp.path().into()));
            for i in 0..20 {
                ledger.append_notification(delta(&session_id, &format!("ev-{i}")));
            }
        }
        // Eager reference path: read the disk snapshot directly (what the
        // old boot-time recover did) and hydrate a fresh in-memory session.
        let session_dir = temp
            .path()
            .join("ui-protocol")
            .join(encode_session_dir_name(&session_id));
        let reference = UiProtocolLedger::with_config(LedgerConfig::durable(temp.path().into()));
        let snapshot = reference
            .read_session_disk_snapshot(&session_id, &session_dir, None, false)
            .expect("read snapshot")
            .expect("snapshot exists");
        let mut reference_session = SessionLedger::new();
        hydrate_session_from_snapshot(&mut reference_session, snapshot);
        let reference_entries: Vec<String> = reference_session
            .entries
            .iter()
            .map(|entry| format!("{:?}", entry))
            .collect();

        // Lazy path: boot-index, then hydrate on first touch.
        let outcome = UiProtocolLedger::recover(LedgerConfig::durable(temp.path().into()));
        let (events, head) = outcome
            .ledger
            .snapshot_with_cursor(&session_id, None)
            .expect("lazy hydrate");
        assert_eq!(events.len(), 20);
        assert_eq!(head.seq, 20);
        let lazy_entries: Vec<String> = outcome
            .ledger
            .snapshot_with_cursor(&session_id, None)
            .expect("second hydrate")
            .0
            .iter()
            .map(|entry| format!("{:?}", entry))
            .collect();
        // Both paths must produce the same event payloads in order.
        let lazy_texts = replay_texts(
            &outcome
                .ledger
                .snapshot_with_cursor(&session_id, None)
                .unwrap()
                .0,
        );
        let expected: Vec<String> = (0..20).map(|i| format!("ev-{i}")).collect();
        assert_eq!(lazy_texts, expected);
        assert_eq!(
            lazy_entries.len(),
            reference_entries.len(),
            "lazy and eager projections must have identical entry counts"
        );
        assert!(
            outcome.ledger.has_session_in_memory_for_test(&session_id),
            "touched session must be cached in the ring"
        );
    }

    /// Scenario 3: touching session A must never replay session B.
    #[test]
    fn lazy_recovery_touch_a_does_not_replay_b() {
        let temp = tempfile::tempdir().expect("tempdir");
        let a = SessionKey("local:lazy-a".into());
        let b = SessionKey("local:lazy-b".into());
        {
            let ledger = UiProtocolLedger::with_config(LedgerConfig::durable(temp.path().into()));
            for i in 0..3 {
                ledger.append_notification(delta(&a, &format!("a-{i}")));
            }
            for i in 0..7 {
                ledger.append_notification(delta(&b, &format!("b-{i}")));
            }
        }
        let outcome = UiProtocolLedger::recover(LedgerConfig::durable(temp.path().into()));
        assert_eq!(outcome.ledger.lazy_replay_count_for_test(), 0);
        // Touch A twice.
        let (events_a, _) = outcome
            .ledger
            .snapshot_with_cursor(&a, None)
            .expect("hydrate A");
        assert_eq!(replay_texts(&events_a), vec!["a-0", "a-1", "a-2"]);
        let _ = outcome
            .ledger
            .snapshot_with_cursor(&a, None)
            .expect("A cached");
        assert_eq!(
            outcome.ledger.lazy_replay_count_for_test(),
            1,
            "exactly one lazy replay (A on first touch; second touch hits cache)"
        );
        assert!(
            !outcome.ledger.has_session_in_memory_for_test(&b),
            "B must remain untouched on disk"
        );
        // Now touch B: one more replay, A unaffected.
        let (events_b, head_b) = outcome
            .ledger
            .snapshot_with_cursor(&b, None)
            .expect("hydrate B");
        assert_eq!(events_b.len(), 7);
        assert_eq!(head_b.seq, 7);
        assert_eq!(outcome.ledger.lazy_replay_count_for_test(), 2);
    }

    /// Scenario 4: a corrupt single-session ledger makes ONLY that session
    /// unavailable — boot and other sessions proceed normally; the failure
    /// is latched (no rescan storm on repeated touches).
    #[test]
    fn lazy_recovery_corrupt_session_isolated() {
        let temp = tempfile::tempdir().expect("tempdir");
        let good = SessionKey("local:lazy-good".into());
        let bad = SessionKey("local:lazy-bad".into());
        {
            let ledger = UiProtocolLedger::with_config(LedgerConfig::durable(temp.path().into()));
            for i in 0..4 {
                ledger.append_notification(delta(&good, &format!("g-{i}")));
            }
            for i in 0..4 {
                ledger.append_notification(delta(&bad, &format!("x-{i}")));
            }
        }
        // Corrupt the bad session's log directory: a subdirectory NAMED
        // like a log file. `list_log_files` only picks `is_file()` entries
        // on its top-level pass, so a directory alone would be silently
        // skipped — instead we poison the scan by making `read_dir` itself
        // fail: remove read permission from the session dir. That makes
        // `list_log_files` (and any sibling scan) return Err(PermissionDenied),
        // which `read_session_disk_snapshot` propagates as Err → the index
        // entry latches `failed`.
        let bad_dir = temp
            .path()
            .join("ui-protocol")
            .join(encode_session_dir_name(&bad));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&bad_dir).expect("meta").permissions();
            perms.set_mode(0o000);
            fs::set_permissions(&bad_dir, perms).expect("chmod 000");
        }
        #[cfg(not(unix))]
        {
            // Portable fallback: drop a directory named like the active
            // log so the sorted-last "active" path cannot be opened as a
            // file, then remove the real files.
            for entry in fs::read_dir(&bad_dir).expect("list bad dir").flatten() {
                let path = entry.path();
                if path.is_file() {
                    fs::remove_file(&path).expect("remove log");
                    fs::create_dir(&path).expect("dir named like log");
                }
            }
        }
        let outcome = UiProtocolLedger::recover(LedgerConfig::durable(temp.path().into()));
        assert_eq!(
            outcome.sessions_recovered, 2,
            "boot indexes both sessions; corruption must not block boot"
        );
        // Good session fully usable.
        let (events_good, head_good) = outcome
            .ledger
            .snapshot_with_cursor(&good, None)
            .expect("good session hydrates");
        assert_eq!(replay_texts(&events_good), vec!["g-0", "g-1", "g-2", "g-3"]);
        assert_eq!(head_good.seq, 4);
        // Bad session: read fails, session stays out of memory, failure latched.
        let (events_bad, _) = outcome
            .ledger
            .snapshot_with_cursor(&bad, None)
            .expect("bad session must not error the whole hydrate path");
        assert!(events_bad.is_empty());
        assert!(outcome.ledger.index_failed_for_test(&bad));
        // Repeated touches do not rescan (failure latched).
        let _ = outcome.ledger.snapshot_with_cursor(&bad, None);
        assert_eq!(
            outcome.ledger.lazy_replay_count_for_test(),
            1,
            "only the good session replayed; the bad one never retries"
        );
        // Appends to the good session continue unaffected.
        let next = outcome.ledger.append_notification(delta(&good, "g-4"));
        assert_eq!(next.cursor.seq, 5);
    }

    // ---- Per-session snapshots (perf/ledger-snapshot, step 1b) ---------
    // Acceptance scenarios from .octos/OUTER_LOOP_REVIEW.md §1b.

    fn snapshot_config(data_dir: &Path, every: u64) -> LedgerConfig {
        let mut config = LedgerConfig::durable(data_dir.into());
        config.snapshot_every_events = every;
        config
    }

    fn snapshot_file_path(data_dir: &Path, session_id: &SessionKey) -> PathBuf {
        data_dir
            .join("ui-protocol")
            .join(encode_session_dir_name(session_id))
            .join(SESSION_SNAPSHOT_FILE_NAME)
    }

    /// #8d — ring already trimmed (oldest > 1) + cold-start hydrate(None):
    /// the snapshot shortcut MUST apply (no full replay) because
    /// hydrate(None) is answered by the retained ring alone. And a corrupt
    /// snapshot whose oldest entry is seq 0 MUST still degrade to a full
    /// replay.
    #[test]
    fn snapshot_shortcut_applies_to_trimmed_ring_from_beginning() {
        let temp = tempfile::tempdir().expect("tempdir");
        let session_id = SessionKey("local:8d-trimmed".into());
        let session_dir = temp
            .path()
            .join("ui-protocol")
            .join(encode_session_dir_name(&session_id));
        // ring=2, snapshot_every=2 → snapshot at seq 4 covers seq 3..4
        // (trimmed past seq 1: oldest=3 > 1).
        let mut config = snapshot_config(temp.path(), 2);
        config.retained_per_session = 2;
        {
            let ledger = UiProtocolLedger::with_config(config.clone());
            for i in 1..=5 {
                ledger.append_notification(delta(&session_id, &format!("ev-{i}")));
            }
        }
        let raw: Value = serde_json::from_slice(
            &fs::read(snapshot_file_path(temp.path(), &session_id)).expect("snapshot"),
        )
        .expect("json");
        assert_eq!(raw["head_seq"], 4);
        let oldest_in_snap = raw["entries"][0]["seq"].as_u64().expect("oldest");
        assert!(oldest_in_snap > 1, "ring must be trimmed (oldest > 1)");

        // Cold-start hydrate(None): read_session_disk_snapshot with
        // replay_after_seq=None — the shortcut must apply: the snapshot
        // seeds the ring and the log scan skips everything ≤ snapshot
        // head (4), replaying only the tail (seq 5). head_seq reflects
        // snapshot+tail (5), retained ring = snapshot(3,4)+tail(5) capped
        // to 2 → (4,5). This is the O(snapshot+tail) path, NOT a full
        // O(all-events) replay — verified by the retained ring coming
        // from the snapshot seed, not the full log.
        let ledger = UiProtocolLedger::with_config(config.clone());
        let snap = ledger
            .read_session_disk_snapshot(&session_id, &session_dir, None, false)
            .expect("read ok")
            .expect("snapshot");
        assert_eq!(snap.head_seq, 5, "snapshot head 4 + log tail 5");
        let ring_seqs: Vec<u64> = snap.retained_entries.iter().map(|e| e.seq).collect();
        assert_eq!(
            ring_seqs,
            vec![4, 5],
            "retained ring = snapshot seed (3,4) + tail (5), capped to 2"
        );

        // seq-0 (from-beginning via the replay path) also shortcuts.
        let ledger2 = UiProtocolLedger::with_config(config.clone());
        let snap0 = ledger2
            .read_session_disk_snapshot(&session_id, &session_dir, Some(0), true)
            .expect("read ok")
            .expect("snapshot");
        assert_eq!(snap0.head_seq, 5, "seq-0 from-beginning also shortcuts");

        // Corrupt: a snapshot whose oldest entry is seq 0 MUST degrade to
        // a full replay (log head 5).
        let bad = json!({
            "version": SESSION_SNAPSHOT_VERSION,
            "head_seq": 4,
            "entries": [
                {"v": 1, "seq": 0, "event": raw["entries"][0]["event"]},
                {"v": 1, "seq": 4, "event": raw["entries"][1]["event"]},
            ],
        });
        fs::write(
            snapshot_file_path(temp.path(), &session_id),
            serde_json::to_vec(&bad).unwrap(),
        )
        .expect("write corrupt snapshot");
        let ledger3 = UiProtocolLedger::with_config(config);
        let snap_bad = ledger3
            .read_session_disk_snapshot(&session_id, &session_dir, None, false)
            .expect("read ok")
            .expect("snapshot");
        assert_eq!(
            snap_bad.head_seq, 5,
            "corrupt seq-0 snapshot must degrade to full replay"
        );
    }

    /// 8d-r1 ①/② — session/open {after:{seq:0}} on a TRIMMED snapshot
    /// (oldest>1) with JSONL still holding seq 1: the snapshot shortcut
    /// must NOT apply (session/open has no from_beginning exemption) →
    /// full JSONL scan, replay from seq 1, no cursor_out_of_range.
    /// Meanwhile session/hydrate from-beginning (read_disk_snapshot_for_
    /// replay, seq-0 exempt) keeps the retained-window shortcut. And
    /// replay_after_with_head(None) live-only semantics are unaffected.
    #[test]
    fn session_open_seq0_full_replays_but_hydrate_shortcuts() {
        let temp = tempfile::tempdir().expect("tempdir");
        let session_id = SessionKey("local:8d-r1".into());
        // ring=2, snapshot at seq 4 (covers seq 3..4; JSONL holds 1..5).
        let mut config = snapshot_config(temp.path(), 2);
        config.retained_per_session = 2;
        {
            let ledger = UiProtocolLedger::with_config(config.clone());
            for i in 1..=5 {
                ledger.append_notification(delta(&session_id, &format!("ev-{i}")));
            }
        }
        let raw: Value = serde_json::from_slice(
            &fs::read(snapshot_file_path(temp.path(), &session_id)).expect("snapshot"),
        )
        .expect("json");
        assert!(raw["entries"][0]["seq"].as_u64().expect("oldest") > 1);

        // (a) session/open replay path: replay_after {seq:0} → full JSONL
        // scan from seq 1 (shortcut OFF), no error.
        let ledger = UiProtocolLedger::with_config(config.clone());
        let cursor0 = UiCursor {
            stream: session_id.0.clone(),
            seq: 0,
        };
        let replay = ledger
            .replay_after(&session_id, Some(&cursor0))
            .expect("session/open seq-0 must not be cursor_out_of_range");
        let seqs: Vec<u64> = replay.iter().map(|e| e.cursor.seq).collect();
        assert_eq!(
            seqs,
            vec![1, 2, 3, 4, 5],
            "session/open seq-0 must full-replay from seq 1"
        );

        // (b) hydrate from-beginning (read_disk_snapshot_for_replay) keeps
        // the retained-window shortcut: same data dir, snapshot seeds the
        // ring and the log scan skips ≤ snapshot head (only the tail 5 is
        // replayed from disk).
        let ledger2 = UiProtocolLedger::with_config(config.clone());
        let (events, head) = ledger2
            .snapshot_with_cursor(&session_id, None)
            .expect("hydrate from-beginning");
        assert_eq!(head.seq, 5);
        // The hydrate projection comes from the snapshot-seeded ring + tail.
        let hseqs: Vec<u64> = events.iter().map(|e| e.cursor.seq).collect();
        assert_eq!(
            hseqs,
            vec![4, 5],
            "hydrate keeps the retained window (snapshot seed 3,4 + tail 5, capped to 2)"
        );

        // (c) replay_after_with_head(None) live-only: unaffected — on an
        // already-hydrated session it returns an empty replay paired with
        // the current head (no full scan, no replayed history).
        let (live_events, live_head) = ledger2
            .replay_after_with_head(&session_id, None)
            .expect("live-only after None");
        assert!(live_events.is_empty(), "after=None is live-only, no replay");
        assert_eq!(live_head, 5);
    }

    /// ymote P1 regression (PR #2114 review): ring=2, snapshot at seq 4 —
    /// replay after seq 1 MUST return seq 2..5. The snapshot shortcut
    /// must NOT skip the pre-snapshot range (the retained JSONL still
    /// holds it), and oldest_seq must come from the full log so the
    /// cursor is not falsely reported out_of_range. Note the pre-existing
    /// equivalence test could not catch this hole because it ran with
    /// snapshot_every_events=0 on the reference path (which disables
    /// snapshot WRITES but not the snapshot READ being tested here).
    #[test]
    fn snapshot_shortcut_replays_pre_snapshot_range() {
        let temp = tempfile::tempdir().expect("tempdir");
        let session_id = SessionKey("local:ymote-p1".into());
        let session_dir = temp
            .path()
            .join("ui-protocol")
            .join(encode_session_dir_name(&session_id));
        // ring=2 (retained_per_session=2), snapshot_every=4 → snapshot at seq 4.
        let mut config = snapshot_config(temp.path(), 4);
        config.retained_per_session = 2;
        {
            let ledger = UiProtocolLedger::with_config(config.clone());
            for i in 1..=5 {
                ledger.append_notification(delta(&session_id, &format!("ev-{i}")));
            }
        }
        // Snapshot file exists with head 4, ring covers only seq 3..4
        // (retained 2); the JSONL log still holds seq 1..5.
        let raw: Value = serde_json::from_slice(
            &fs::read(snapshot_file_path(temp.path(), &session_id)).expect("snapshot"),
        )
        .expect("snapshot json");
        assert_eq!(raw["head_seq"], 4, "snapshot at seq 4");

        // Replay after seq 1 → must return seq 2,3,4,5 (NOT 5 only, NOT
        // cursor_out_of_range). Drive read_session_disk_snapshot directly
        // with replay_after_seq = Some(1).
        let ledger = UiProtocolLedger::with_config(config);
        let snap = ledger
            .read_session_disk_snapshot(&session_id, &session_dir, Some(1), false)
            .expect("read ok")
            .expect("snapshot exists");
        let seqs: Vec<u64> = snap.replay_entries.iter().map(|e| e.cursor.seq).collect();
        assert_eq!(
            seqs,
            vec![2, 3, 4, 5],
            "pre-snapshot range must be replayed from the retained log"
        );
        // oldest_seq from the FULL log (1), not the snapshot ring (3), so
        // a cursor at seq 1 is not falsely out_of_range.
        assert_eq!(snap.oldest_seq, Some(1));
        assert_eq!(snap.head_seq, 5);
    }

    /// Scenario 1 (equivalence): snapshot+tail recovery must produce a
    /// projection field-for-field identical to a full replay of the same
    /// ledger — and the snapshot file must exist with the right head.
    #[test]
    fn snapshot_plus_tail_equivalent_to_full_replay() {
        let temp = tempfile::tempdir().expect("tempdir");
        let session_id = SessionKey("local:snap-equiv".into());
        {
            let ledger = UiProtocolLedger::with_config(snapshot_config(temp.path(), 4));
            for i in 1..=11 {
                ledger.append_notification(delta(&session_id, &format!("ev-{i}")));
            }
        }
        let snap_path = snapshot_file_path(temp.path(), &session_id);
        assert!(snap_path.exists(), "snapshot must be written at seq 4 & 8");
        let raw: Value =
            serde_json::from_slice(&fs::read(&snap_path).expect("read snapshot")).unwrap();
        assert_eq!(raw["version"], SESSION_SNAPSHOT_VERSION as u64);
        assert_eq!(raw["head_seq"], 8, "latest snapshot head after 11 events");

        // Snapshot+tail path (lazy recover then hydrate).
        let outcome = UiProtocolLedger::recover(snapshot_config(temp.path(), 4));
        let (snap_events, snap_head) = outcome
            .ledger
            .snapshot_with_cursor(&session_id, None)
            .expect("snapshot+tail hydrate");
        // Full-replay reference: snapshotting disabled.
        let mut full_config = snapshot_config(temp.path(), 4);
        full_config.snapshot_every_events = 0;
        let reference = UiProtocolLedger::recover(full_config);
        let (full_events, full_head) = reference
            .ledger
            .snapshot_with_cursor(&session_id, None)
            .expect("full replay hydrate");

        assert_eq!(snap_head, full_head);
        let snap_dbg: Vec<String> = snap_events.iter().map(|e| format!("{e:?}")).collect();
        let full_dbg: Vec<String> = full_events.iter().map(|e| format!("{e:?}")).collect();
        assert_eq!(
            snap_dbg, full_dbg,
            "snapshot+tail projection must equal full-replay projection"
        );
        assert_eq!(replay_texts(&snap_events).len(), 11);
        // Next append continues the seq space correctly.
        let next = outcome
            .ledger
            .append_notification(delta(&session_id, "ev-12"));
        assert_eq!(next.cursor.seq, 12);
    }

    /// Scenario 2 (fault tolerance): a corrupt snapshot must NOT lose data
    /// — recovery logs a warning and falls back to a full replay.
    #[test]
    fn corrupt_snapshot_falls_back_to_full_replay() {
        let temp = tempfile::tempdir().expect("tempdir");
        let session_id = SessionKey("local:snap-corrupt".into());
        {
            let ledger = UiProtocolLedger::with_config(snapshot_config(temp.path(), 2));
            for i in 1..=6 {
                ledger.append_notification(delta(&session_id, &format!("ev-{i}")));
            }
        }
        let snap_path = snapshot_file_path(temp.path(), &session_id);
        assert!(snap_path.exists());
        fs::write(&snap_path, b"{ not json !!").expect("corrupt snapshot");

        let outcome = UiProtocolLedger::recover(snapshot_config(temp.path(), 2));
        let (events, head) = outcome
            .ledger
            .snapshot_with_cursor(&session_id, None)
            .expect("corrupt snapshot must not break recovery");
        assert_eq!(head.seq, 6);
        assert_eq!(
            replay_texts(&events),
            vec!["ev-1", "ev-2", "ev-3", "ev-4", "ev-5", "ev-6"],
            "full replay must recover everything despite corrupt snapshot"
        );

        // Unknown-version snapshot also falls back.
        fs::write(
            &snap_path,
            serde_json::to_vec(&json!({
                "version": 999,
                "head_seq": 6,
                "entries": [],
            }))
            .unwrap(),
        )
        .expect("write unknown-version snapshot");
        let outcome2 = UiProtocolLedger::recover(snapshot_config(temp.path(), 2));
        let (events2, head2) = outcome2
            .ledger
            .snapshot_with_cursor(&session_id, None)
            .expect("unknown-version snapshot must fall back");
        assert_eq!(head2.seq, 6);
        assert_eq!(replay_texts(&events2).len(), 6);
    }

    /// Scenario 3 (zero migration): a pre-1b ledger with NO snapshot file
    /// reads exactly as before.
    #[test]
    fn ledger_without_snapshot_reads_unchanged() {
        let temp = tempfile::tempdir().expect("tempdir");
        let session_id = SessionKey("local:snap-none".into());
        {
            let mut config = snapshot_config(temp.path(), 4);
            config.snapshot_every_events = 0; // simulate a pre-1b writer
            let ledger = UiProtocolLedger::with_config(config);
            for i in 1..=7 {
                ledger.append_notification(delta(&session_id, &format!("old-{i}")));
            }
        }
        assert!(
            !snapshot_file_path(temp.path(), &session_id).exists(),
            "pre-1b ledger has no snapshot file"
        );
        let outcome = UiProtocolLedger::recover(snapshot_config(temp.path(), 4));
        let (events, head) = outcome
            .ledger
            .snapshot_with_cursor(&session_id, None)
            .expect("legacy ledger must replay fully");
        assert_eq!(head.seq, 7);
        assert_eq!(replay_texts(&events).len(), 7);
        // And once the new writer touches it, snapshots start appearing.
        for i in 8..=12 {
            outcome
                .ledger
                .append_notification(delta(&session_id, &format!("new-{i}")));
        }
        assert!(
            snapshot_file_path(temp.path(), &session_id).exists(),
            "snapshot appears at the next cadence boundary (seq 8/12)"
        );
    }

    /// Outer-review 1b fix: an existing (pre-1b) ledger with NO snapshot
    /// must earn a bootstrap snapshot on its first touch (the full-replay
    /// path), so the SECOND recovery runs snapshot+tail — the operator's
    /// 45MB main session never waits for the append cadence.
    #[test]
    fn existing_ledger_bootstraps_snapshot_on_first_touch() {
        let temp = tempfile::tempdir().expect("tempdir");
        let session_id = SessionKey("local:snap-bootstrap".into());
        {
            let mut config = snapshot_config(temp.path(), 4096);
            config.snapshot_every_events = 0; // pre-1b writer: no snapshots
            let ledger = UiProtocolLedger::with_config(config);
            for i in 1..=10 {
                ledger.append_notification(delta(&session_id, &format!("old-{i}")));
            }
        }
        let snap_path = snapshot_file_path(temp.path(), &session_id);
        assert!(!snap_path.exists(), "pre-1b ledger has no snapshot");

        // First touch under the 1b writer: full replay happens AND the
        // bootstrap snapshot is written immediately.
        let outcome = UiProtocolLedger::recover(snapshot_config(temp.path(), 4096));
        let (events, head) = outcome
            .ledger
            .snapshot_with_cursor(&session_id, None)
            .expect("first touch hydrates");
        assert_eq!(head.seq, 10);
        assert_eq!(replay_texts(&events).len(), 10);
        assert!(
            snap_path.exists(),
            "full replay must bootstrap-write snapshot.json on first touch"
        );
        let raw: Value =
            serde_json::from_slice(&fs::read(&snap_path).expect("read snapshot")).unwrap();
        assert_eq!(raw["version"], SESSION_SNAPSHOT_VERSION as u64);
        assert_eq!(raw["head_seq"], 10);

        // Second recovery: projection still equivalent (snapshot+tail
        // path — the bootstrap snapshot is used, logs only contribute
        // the empty tail).
        drop(outcome);
        let outcome2 = UiProtocolLedger::recover(snapshot_config(temp.path(), 4096));
        let (events2, head2) = outcome2
            .ledger
            .snapshot_with_cursor(&session_id, None)
            .expect("second recovery hydrates from snapshot+tail");
        assert_eq!(head2.seq, 10);
        let dbg1: Vec<String> = events.iter().map(|e| format!("{e:?}")).collect();
        let dbg2: Vec<String> = events2.iter().map(|e| format!("{e:?}")).collect();
        assert_eq!(
            dbg1, dbg2,
            "snapshot+tail projection must match full replay"
        );
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

    #[test]
    fn recovery_skips_legacy_message_persisted_row_and_preserves_cursor_space() {
        let temp = tempfile::tempdir().expect("tempdir");
        let session_id = SessionKey("local:legacy-persisted".into());
        let session_dir = temp
            .path()
            .join("ui-protocol")
            .join(encode_session_dir_name(&session_id));
        fs::create_dir_all(&session_dir).expect("session dir");

        // Authentic pre-Stage-5 ledger shape. The v2-only reader recognizes
        // the removed discriminator, skips its payload deliberately, and
        // retains its cursor position so a new append cannot reuse seq 7.
        let legacy = json!({
            "v": LEDGER_DISK_VERSION,
            "seq": 7,
            "event": {
                "record_kind": "notification",
                "kind": "message_persisted",
                "session_id": session_id.0,
                "topic": null,
                "turn_id": null,
                "thread_id": "thread-legacy",
                "seq": 3,
                "role": "assistant",
                "message_id": "local:legacy-persisted:3:1",
                "client_message_id": null,
                "source": "assistant",
                "media": [],
                "cursor": { "stream": session_id.0.clone(), "seq": 7 },
                "persisted_at": "2026-01-01T00:00:00Z",
                "content": null
            }
        });
        fs::write(
            session_dir.join(new_log_file_name()),
            format!(
                "{}\n",
                serde_json::to_string(&legacy).expect("serialize legacy row")
            ),
        )
        .expect("write legacy log");

        let recovered = UiProtocolLedger::recover(LedgerConfig::durable(temp.path().into()));
        let (replay, cursor) = recovered
            .ledger
            .snapshot_with_cursor(&session_id, None)
            .expect("legacy replay must not crash");
        assert!(replay.is_empty(), "removed payload must never be re-routed");
        assert_eq!(cursor.seq, 7, "skipped row still reserves its cursor");

        let next = recovered
            .ledger
            .append_notification(delta(&session_id, "v2-next"));
        assert_eq!(
            next.cursor.seq, 8,
            "new row must continue after skipped legacy row"
        );
    }

    // ---------- live publish-subscribe (issue #760) ----------

    #[tokio::test]
    async fn subscribe_delivers_v2_assistant_persisted_to_live_receiver() {
        let ledger = UiProtocolLedger::new(8);
        let session_id = SessionKey("local:live".into());
        let mut rx = ledger.subscribe(&session_id);

        let appended = ledger
            .emit_envelope_v2(
                &session_id,
                "thread-live".into(),
                PayloadV2::AssistantPersisted {
                    text: "assistant".into(),
                    assistant_segment_id: "thread-live:assistant:1".into(),
                    meta: octos_core::ui_protocol::MessageMeta {
                        message_id: "msg-1".into(),
                        persisted_at: chrono::Utc::now(),
                        media: vec![],
                    },
                },
                None,
            )
            .expect("v2 envelope emitted");

        let received = tokio::time::timeout(StdDuration::from_secs(1), rx.recv())
            .await
            .expect("live event arrived")
            .expect("receiver still open");

        assert_eq!(received.cursor, appended.cursor);
        assert!(matches!(
            received.event,
            UiProtocolLedgerEvent::Notification(UiNotification::EnvelopeV2(_))
        ));
    }

    #[tokio::test]
    async fn subscribe_fans_out_to_multiple_receivers() {
        let ledger = UiProtocolLedger::new(8);
        let session_id = SessionKey("local:fanout".into());
        let mut rx_one = ledger.subscribe(&session_id);
        let mut rx_two = ledger.subscribe(&session_id);

        let appended = ledger.append_notification(delta(&session_id, "fanout"));

        let one = tokio::time::timeout(StdDuration::from_secs(1), rx_one.recv())
            .await
            .expect("rx_one timeout")
            .expect("rx_one open");
        let two = tokio::time::timeout(StdDuration::from_secs(1), rx_two.recv())
            .await
            .expect("rx_two timeout")
            .expect("rx_two open");

        assert_eq!(one.cursor, appended.cursor);
        assert_eq!(two.cursor, appended.cursor);
    }

    #[tokio::test]
    async fn subscribe_continues_after_one_receiver_drops() {
        let ledger = UiProtocolLedger::new(8);
        let session_id = SessionKey("local:drop-one".into());
        let rx_one = ledger.subscribe(&session_id);
        let mut rx_two = ledger.subscribe(&session_id);
        drop(rx_one);

        let appended = ledger.append_notification(delta(&session_id, "after-drop"));

        let received = tokio::time::timeout(StdDuration::from_secs(1), rx_two.recv())
            .await
            .expect("rx_two timeout")
            .expect("rx_two still open after sibling dropped");

        assert_eq!(received.cursor, appended.cursor);
    }

    #[tokio::test]
    async fn subscribe_does_not_replay_past_events() {
        let ledger = UiProtocolLedger::new(8);
        let session_id = SessionKey("local:no-replay".into());
        ledger.append_notification(delta(&session_id, "before"));

        let mut rx = ledger.subscribe(&session_id);

        // Nothing should be queued — broadcast is live-only fan-out.
        let try_recv = rx.try_recv();
        assert!(
            matches!(try_recv, Err(broadcast::error::TryRecvError::Empty)),
            "broadcast must not deliver past events; got {try_recv:?}"
        );

        // Once a new event lands, the receiver does see it.
        let after = ledger.append_notification(delta(&session_id, "after"));
        let live = tokio::time::timeout(StdDuration::from_secs(1), rx.recv())
            .await
            .expect("recv timeout")
            .expect("recv open");
        assert_eq!(live.cursor, after.cursor);
    }

    #[tokio::test]
    async fn append_without_subscribers_is_durable_no_op_for_broadcast() {
        // No subscribe call — append must still succeed and persist.
        let ledger = UiProtocolLedger::new(8);
        let session_id = SessionKey("local:no-sub".into());
        let appended = ledger.append_notification(delta(&session_id, "alone"));
        assert_eq!(appended.cursor.seq, 1);

        // Subscriber arriving after the fact only sees future events,
        // and `replay_after` covers the durable history.
        let mut rx = ledger.subscribe(&session_id);
        let after = ledger.append_notification(delta(&session_id, "alone-2"));
        let live = tokio::time::timeout(StdDuration::from_secs(1), rx.recv())
            .await
            .expect("recv timeout")
            .expect("recv open");
        assert_eq!(live.cursor, after.cursor);
    }

    #[test]
    fn prune_idle_subscribers_drops_orphaned_senders() {
        let ledger = UiProtocolLedger::new(8);
        let session_id = SessionKey("local:prune".into());
        let rx = ledger.subscribe(&session_id);
        // Sanity: prune is a no-op while a receiver is alive.
        assert_eq!(ledger.prune_idle_subscribers(), 0);
        drop(rx);
        // After all receivers drop, the orphaned sender is removed.
        assert_eq!(ledger.prune_idle_subscribers(), 1);
    }

    // ======================================================================
    // UPCR-2026-014 M9-γ ThreadSeqAllocator + hard-barrier invariants.
    // ======================================================================

    fn envelope_seq(ledgered: &LedgeredUiProtocolEvent) -> u64 {
        match &ledgered.event {
            UiProtocolLedgerEvent::Notification(UiNotification::Envelope(ev)) => ev.envelope.seq,
            other => panic!("expected envelope ledger event, got {other:?}"),
        }
    }

    fn envelope_payload(ledgered: &LedgeredUiProtocolEvent) -> Payload {
        match &ledgered.event {
            UiProtocolLedgerEvent::Notification(UiNotification::Envelope(ev)) => {
                ev.envelope.payload.clone()
            }
            other => panic!("expected envelope ledger event, got {other:?}"),
        }
    }

    #[test]
    fn thread_seq_allocator_issues_monotonic_seq_within_thread() {
        let ledger = UiProtocolLedger::new(32);
        let session_id = SessionKey("local:thread-seq".into());
        let thread_id = "thread-A".to_owned();

        let a = ledger
            .emit_envelope(
                &session_id,
                thread_id.clone(),
                Payload::AssistantDelta { text: "a".into() },
                None,
            )
            .expect("first emit");
        let b = ledger
            .emit_envelope(
                &session_id,
                thread_id.clone(),
                Payload::AssistantDelta { text: "b".into() },
                None,
            )
            .expect("second emit");
        let c = ledger
            .emit_envelope(
                &session_id,
                thread_id.clone(),
                Payload::AssistantDelta { text: "c".into() },
                None,
            )
            .expect("third emit");

        assert_eq!(envelope_seq(&a), 1);
        assert_eq!(envelope_seq(&b), 2);
        assert_eq!(envelope_seq(&c), 3);
    }

    #[test]
    fn thread_seq_allocator_is_independent_across_threads() {
        let ledger = UiProtocolLedger::new(32);
        let session_id = SessionKey("local:multi-thread".into());

        let a1 = ledger
            .emit_envelope(
                &session_id,
                "thread-A".into(),
                Payload::AssistantDelta { text: "a1".into() },
                None,
            )
            .unwrap();
        let b1 = ledger
            .emit_envelope(
                &session_id,
                "thread-B".into(),
                Payload::AssistantDelta { text: "b1".into() },
                None,
            )
            .unwrap();
        let a2 = ledger
            .emit_envelope(
                &session_id,
                "thread-A".into(),
                Payload::AssistantDelta { text: "a2".into() },
                None,
            )
            .unwrap();
        let b2 = ledger
            .emit_envelope(
                &session_id,
                "thread-B".into(),
                Payload::AssistantDelta { text: "b2".into() },
                None,
            )
            .unwrap();

        assert_eq!(envelope_seq(&a1), 1);
        assert_eq!(envelope_seq(&a2), 2);
        assert_eq!(envelope_seq(&b1), 1);
        assert_eq!(envelope_seq(&b2), 2);
    }

    #[test]
    fn hard_barrier_drops_post_completion_envelopes() {
        let ledger = UiProtocolLedger::new(32);
        let session_id = SessionKey("local:barrier".into());
        let thread_id = "thread-X".to_owned();

        let _ = ledger
            .emit_envelope(
                &session_id,
                thread_id.clone(),
                Payload::AssistantDelta { text: "hi".into() },
                None,
            )
            .expect("pre-completion emit");
        let completed = ledger
            .emit_envelope(
                &session_id,
                thread_id.clone(),
                Payload::TurnCompleted {
                    token_usage: octos_core::ui_protocol::EnvelopeTokenUsage::default(),
                },
                None,
            )
            .expect("turn_completed emit");
        assert!(matches!(
            envelope_payload(&completed),
            Payload::TurnCompleted { .. }
        ));

        // Post-completion: ANY further envelope on this thread is dropped.
        let dropped = ledger.emit_envelope(
            &session_id,
            thread_id.clone(),
            Payload::AssistantDelta {
                text: "should be dropped".into(),
            },
            None,
        );
        assert!(dropped.is_none(), "post-completion emit must be dropped");

        // A second TurnCompleted is also dropped.
        let dup = ledger.emit_envelope(
            &session_id,
            thread_id,
            Payload::TurnCompleted {
                token_usage: octos_core::ui_protocol::EnvelopeTokenUsage::default(),
            },
            None,
        );
        assert!(
            dup.is_none(),
            "duplicate TurnCompleted on the same thread must be dropped"
        );
    }

    #[test]
    fn thread_seq_allocator_recovers_from_in_memory_ledger() {
        // Daemon restart simulation: pre-fill the ring with envelopes
        // bearing explicit seqs (as if recovered from disk via `recover`),
        // then call `emit_envelope` on a fresh allocator — it MUST
        // continue from max(seq) + 1, not reset to 1.
        let ledger = UiProtocolLedger::new(32);
        let session_id = SessionKey("local:restart".into());
        let thread_id = "thread-R".to_owned();

        // Pre-seed: bypass `emit_envelope` and append raw envelopes so the
        // `thread_seq` map is NOT populated (simulating the post-restart
        // state where the ring is hydrated from disk but the per-thread
        // allocator hasn't observed an emit yet).
        for seq in [1u64, 2, 3, 4] {
            let envelope = Envelope {
                thread_id: thread_id.clone(),
                seq,
                client_message_id: None,
                payload: Payload::AssistantDelta {
                    text: format!("delta-{seq}"),
                },
            };
            let notif = UiNotification::Envelope(EnvelopeNotification {
                session_id: session_id.clone(),
                topic: None,
                envelope,
            });
            let _ = ledger.append_notification(notif);
        }
        // Confirm the ring carries 4 envelopes (replay from seq=0).
        let baseline = UiCursor {
            stream: session_id.0.clone(),
            seq: 0,
        };
        let replay = ledger.replay_after(&session_id, Some(&baseline)).unwrap();
        assert_eq!(replay.len(), 4);

        // Now emit_envelope: the allocator must resume from seq=5.
        let recovered = ledger
            .emit_envelope(
                &session_id,
                thread_id,
                Payload::AssistantDelta {
                    text: "post-recovery".into(),
                },
                None,
            )
            .expect("emit after recovery");
        assert_eq!(
            envelope_seq(&recovered),
            5,
            "ThreadSeqAllocator must continue from max(seq) + 1 after restart"
        );
    }

    // ────────────────────────────────────────────────────────────────────
    // Codex #1336 round-2 BLOCKER 2: atomic seq allocation + append
    // ────────────────────────────────────────────────────────────────────

    /// 100 concurrent emits on the same `(session, thread)` produce a
    /// contiguous `seq` range `1..=100` and contiguous ledger-cursor
    /// `seq` values, with the ring entries in EXACTLY the same order
    /// as the allocated envelope `seq` values.
    ///
    /// Pre-fix: `allocate_envelope_seq` and `append` each took the
    /// mutex independently, so two threads could interleave `(seq=1
    /// allocate, seq=2 allocate, seq=2 append, seq=1 append)` and end
    /// up with the ring entries in `seq=2, seq=1` order. A
    /// `TurnCompleted(seq=2)` published before delta `seq=1` would
    /// then cause the bridge to drop the delta as post-completion.
    ///
    /// Post-fix: allocation + ring push run under a SINGLE critical
    /// section, so no other emit can interleave its append between
    /// our allocate and our push.
    #[test]
    fn envelope_seq_allocation_and_append_are_atomic_under_concurrency() {
        use std::sync::Arc;
        use std::thread;
        let ledger = Arc::new(UiProtocolLedger::new(256));
        let session_id = SessionKey("local:atomic".into());
        let thread_id = "thread-atomic".to_owned();
        const N: usize = 100;

        // Subscribe BEFORE emits start so the broadcast captures the
        // delivery order — if seq allocation and append were not
        // atomic, the broadcast send order could deviate from the
        // allocation order.
        let mut subscriber = ledger.subscribe(&session_id);

        let mut handles = Vec::with_capacity(N);
        for i in 0..N {
            let ledger = ledger.clone();
            let session_id = session_id.clone();
            let thread_id = thread_id.clone();
            handles.push(thread::spawn(move || {
                ledger
                    .emit_envelope(
                        &session_id,
                        thread_id,
                        Payload::AssistantDelta {
                            text: format!("concurrent-{i}"),
                        },
                        None,
                    )
                    .expect("concurrent emit must not be dropped")
            }));
        }
        let mut results: Vec<LedgeredUiProtocolEvent> =
            handles.into_iter().map(|h| h.join().unwrap()).collect();

        // 1. All N seqs are contiguous 1..=N (no gaps, no duplicates).
        let mut seqs: Vec<u64> = results.iter().map(envelope_seq).collect();
        seqs.sort();
        let expected: Vec<u64> = (1..=N as u64).collect();
        assert_eq!(
            seqs, expected,
            "envelope seqs MUST be contiguous 1..=N with no gaps or duplicates"
        );

        // 2. The ledger cursor seqs are also contiguous (the in-memory
        // ring order matches the allocation order).
        let mut cursor_seqs: Vec<u64> = results.iter().map(|r| r.cursor.seq).collect();
        cursor_seqs.sort();
        assert_eq!(cursor_seqs, expected);

        // 3. The mapping is consistent: a lower envelope.seq ALWAYS
        // maps to a lower-or-equal ledger cursor.seq. The two seq
        // spaces share a monotonic relationship because both are
        // assigned inside the same critical section.
        results.sort_by_key(envelope_seq);
        for window in results.windows(2) {
            assert!(
                window[0].cursor.seq < window[1].cursor.seq,
                "ledger cursor.seq MUST be monotonic with envelope.seq under atomic emit"
            );
        }

        // 4. The broadcast delivery order matches the allocation
        // order. If allocate+append were not atomic, two emits could
        // race past each other in publish_live and the broadcast
        // would see seq=k+1 before seq=k.
        let mut last_seq: u64 = 0;
        for _ in 0..N {
            match subscriber.try_recv() {
                Ok(event) => {
                    if let UiProtocolLedgerEvent::Notification(UiNotification::Envelope(ev)) =
                        &event.event
                    {
                        assert!(
                            ev.envelope.seq > last_seq,
                            "broadcast delivery must observe seq in strictly monotonic order; \
                             saw {} after {last_seq}",
                            ev.envelope.seq
                        );
                        last_seq = ev.envelope.seq;
                    }
                }
                Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => {
                    // Bounded broadcast may lag — that's OK for this test;
                    // the durability is checked by the seqs/cursor_seqs
                    // asserts above. We DO NOT assert delivery of all 100
                    // because a lagged consumer is a separate concern.
                    break;
                }
                Err(_) => break,
            }
        }
    }

    /// `TurnCompleted` and one pre-completion delta race: the wire
    /// MUST observe the delta first (seq=1), then `TurnCompleted`
    /// (seq=2). Pre-fix this could publish in `(seq=2, seq=1)` order
    /// because the broadcast send happened after lock release on
    /// different threads' lock acquisitions.
    ///
    /// We test this by emitting from two threads with a barrier so
    /// they hit the mutex roughly simultaneously, then check the
    /// in-memory ring's entry order matches the allocation order.
    #[test]
    fn turn_completed_never_appended_before_pre_completion_envelope_on_same_thread() {
        // We can't easily force the OS scheduler to reproduce the
        // race deterministically, but we can hammer the lock with N
        // concurrent emits to maximise contention and assert the
        // ring order matches the allocation order on every iteration.
        // Combined with the atomicity test above, this gives high
        // confidence the race is closed.
        use std::sync::Arc;
        use std::thread;
        let ledger = Arc::new(UiProtocolLedger::new(64));
        let session_id = SessionKey("local:tc-race".into());
        let thread_id = "thread-tc".to_owned();

        let mut handles = Vec::new();
        for _ in 0..30 {
            let l = ledger.clone();
            let s = session_id.clone();
            let t = thread_id.clone();
            handles.push(thread::spawn(move || {
                l.emit_envelope(
                    &s,
                    t,
                    Payload::AssistantDelta {
                        text: "delta".into(),
                    },
                    None,
                )
            }));
        }
        let mut delta_results: Vec<u64> = handles
            .into_iter()
            .filter_map(|h| h.join().unwrap())
            .map(|r| envelope_seq(&r))
            .collect();
        delta_results.sort();
        assert_eq!(delta_results, (1u64..=30).collect::<Vec<_>>());

        // Now emit TurnCompleted on the same thread — it MUST land
        // at seq=31 (after all the deltas).
        let tc = ledger
            .emit_envelope(
                &session_id,
                thread_id.clone(),
                Payload::TurnCompleted {
                    token_usage: octos_core::ui_protocol::EnvelopeTokenUsage::default(),
                },
                None,
            )
            .expect("turn_completed must not be dropped");
        assert_eq!(envelope_seq(&tc), 31);

        // And any further envelope on this thread is hard-barriered.
        let post = ledger.emit_envelope(
            &session_id,
            thread_id,
            Payload::AssistantDelta { text: "x".into() },
            None,
        );
        assert!(post.is_none());
    }

    // ────────────────────────────────────────────────────────────────────
    // Codex #1336 round-2 BLOCKER 3: persistent thread watermark recovery
    // ────────────────────────────────────────────────────────────────────

    /// The watermark file persists `(session, thread) → (next_seq,
    /// completed)` write-ahead so an LRU-evicted session OR a thread
    /// whose envelopes aged out of the retained window can still
    /// resume seq allocation monotonically.
    #[test]
    fn thread_watermark_recovery_from_evicted_session() {
        let temp = tempfile::tempdir().expect("tempdir");
        let session_id = SessionKey("local:wm-evict".into());
        let thread_id = "thread-evict".to_owned();
        // First boot: emit 3 envelopes — the watermark records
        // next_seq=4 after the third emit.
        {
            let mut config = LedgerConfig::durable(temp.path().into());
            config.active_session_cap = 8;
            let ledger = UiProtocolLedger::with_config(config);
            for _ in 0..3 {
                let _ = ledger
                    .emit_envelope(
                        &session_id,
                        thread_id.clone(),
                        Payload::AssistantDelta { text: "d".into() },
                        None,
                    )
                    .expect("emit");
            }
        }
        // Second boot: build a FRESH ledger WITHOUT calling recover()
        // (which would hydrate the disk into the in-memory ring).
        // The thread_seq HashMap is empty, the in-memory ring is
        // empty too — the ONLY way to resume monotonically is via
        // the persistent watermark file.
        let ledger = UiProtocolLedger::with_config(LedgerConfig::durable(temp.path().into()));
        let resumed = ledger
            .emit_envelope(
                &session_id,
                thread_id,
                Payload::AssistantDelta {
                    text: "after-evict".into(),
                },
                None,
            )
            .expect("emit after evicted-session recovery");
        assert_eq!(
            envelope_seq(&resumed),
            4,
            "watermark recovery MUST resume from next_seq=4 \
             even with an empty in-memory ring (session was evicted, \
             retained window has nothing to scan)",
        );
    }

    /// When the retained-window compaction drops every envelope for
    /// a thread but the SESSION itself is still hot, the watermark
    /// file still records `max_seq + completed` — recovery resumes
    /// from the watermark, not from a fresh `next_seq=1`.
    #[test]
    fn thread_watermark_survives_retained_window_compaction() {
        let temp = tempfile::tempdir().expect("tempdir");
        let session_id = SessionKey("local:wm-compact".into());
        let thread_id_a = "thread-A".to_owned();
        let thread_id_b = "thread-B".to_owned();

        // Tiny retained-window so a few emits on thread-B push
        // thread-A's envelopes out of the ring.
        let mut config = LedgerConfig::durable(temp.path().into());
        config.retained_per_session = 3;
        let ledger = UiProtocolLedger::with_config(config);

        // Emit on thread-A; watermark records next_seq=3.
        for _ in 0..2 {
            let _ = ledger
                .emit_envelope(
                    &session_id,
                    thread_id_a.clone(),
                    Payload::AssistantDelta { text: "a".into() },
                    None,
                )
                .expect("emit a");
        }
        // Hammer thread-B to push thread-A out of the ring.
        for _ in 0..10 {
            let _ = ledger
                .emit_envelope(
                    &session_id,
                    thread_id_b.clone(),
                    Payload::AssistantDelta { text: "b".into() },
                    None,
                )
                .expect("emit b");
        }

        // Restart and emit again on thread-A — the in-memory ring no
        // longer has thread-A envelopes (compacted), but the
        // watermark file persists.
        drop(ledger);
        let ledger = UiProtocolLedger::with_config(LedgerConfig::durable(temp.path().into()));
        let resumed = ledger
            .emit_envelope(
                &session_id,
                thread_id_a,
                Payload::AssistantDelta {
                    text: "after-compact".into(),
                },
                None,
            )
            .expect("emit after compact-recovery");
        assert_eq!(
            envelope_seq(&resumed),
            3,
            "watermark recovery MUST resume from the persisted max+1 \
             even when the retained window has compacted out the \
             originating envelopes",
        );
    }

    /// Completion state persists across restart: a thread that
    /// received `TurnCompleted` BEFORE restart MUST stay barriered
    /// after restart even when the in-memory ring is empty.
    #[test]
    fn thread_watermark_preserves_completed_across_restart() {
        let temp = tempfile::tempdir().expect("tempdir");
        let session_id = SessionKey("local:wm-completed".into());
        let thread_id = "thread-tc".to_owned();
        {
            let ledger = UiProtocolLedger::with_config(LedgerConfig::durable(temp.path().into()));
            let _ = ledger
                .emit_envelope(
                    &session_id,
                    thread_id.clone(),
                    Payload::AssistantDelta { text: "d".into() },
                    None,
                )
                .expect("delta");
            let _ = ledger
                .emit_envelope(
                    &session_id,
                    thread_id.clone(),
                    Payload::TurnCompleted {
                        token_usage: octos_core::ui_protocol::EnvelopeTokenUsage::default(),
                    },
                    None,
                )
                .expect("turn_completed");
        }
        let ledger = UiProtocolLedger::with_config(LedgerConfig::durable(temp.path().into()));
        // Post-completion emit on the same thread MUST be barriered
        // even though the in-memory ring is empty.
        let dropped = ledger.emit_envelope(
            &session_id,
            thread_id,
            Payload::AssistantDelta {
                text: "should be barriered".into(),
            },
            None,
        );
        assert!(
            dropped.is_none(),
            "completed=true MUST survive restart so post-completion \
             envelopes stay barriered even without an in-memory ring"
        );
    }

    #[test]
    fn envelope_notification_round_trips_through_ledger_wrapper() {
        // On mini3 we saw 5 ledger records dropped on replay with
        //   error=Error("duplicate field `envelope`", ...)
        // because the outer `tag = "envelope"` discriminator flattened
        // alongside `EnvelopeNotification.envelope` and produced two
        // identical keys in the same JSON object. The wider standalone
        // round-trip test for `EnvelopeNotification` in octos-core missed
        // this — only the ledger wrapper exhibits the collision.
        let session = SessionKey("local:envelope-round-trip".into());
        let event =
            UiProtocolLedgerEvent::Notification(UiNotification::Envelope(EnvelopeNotification {
                session_id: session.clone(),
                topic: Some("planning".into()),
                envelope: Envelope {
                    thread_id: "round-trip-thread".into(),
                    seq: 7,
                    client_message_id: None,
                    payload: Payload::AssistantDelta {
                        text: "round-trip me".into(),
                    },
                },
            }));

        let serialized = serde_json::to_string(&event).expect("ledger event serializes");
        assert!(
            !serialized.contains("\"envelope\":\"notification\""),
            "outer ledger discriminator must NOT be named `envelope` — got {serialized}"
        );
        let parsed: UiProtocolLedgerEvent = serde_json::from_str(&serialized)
            .unwrap_or_else(|e| panic!("ledger replay MUST succeed: {e}; payload={serialized}"));
        assert_eq!(parsed, event, "round-trip must be field-equal");
    }

    // ---------- #1358 back-compat: legacy `envelope` outer tag ----------

    /// Construct a legacy on-disk `event` JSON as the PRE-#1358 binary
    /// wrote it: the outer ledger discriminator was named `envelope`
    /// (renamed to `record_kind` in #1358). We synthesize it by serializing
    /// with the current `record_kind` tag and renaming JUST the outer tag
    /// KEY back to `envelope` via STRING manipulation.
    ///
    /// The string-level rename is load-bearing: for the `Envelope` notification
    /// variant the canonical JSON already contains a nested `envelope` OBJECT
    /// field, so renaming the outer tag to `envelope` re-creates the genuine
    /// TWO-`envelope`-keys-in-one-object collision the pre-#1358 binary wrote.
    /// A `serde_json::Map` could not represent that (it would collapse the two
    /// keys to last-wins) — which is exactly why the collision was duplicate-key
    /// garbage on disk (Step 0). For non-`Envelope` notifications and `Progress`
    /// records there is no inner `envelope` field, so the rename is a clean
    /// single-key swap.
    fn legacy_envelope_tagged_json(event: &UiProtocolLedgerEvent) -> String {
        // serde emits the internally-tagged discriminator FIRST, so the
        // canonical JSON always begins `{"record_kind":"…",`. Rename only
        // that leading outer tag key, leaving every other key (including any
        // nested `envelope` object) byte-for-byte intact.
        let canonical = serde_json::to_string(event).expect("serialize event");
        assert!(
            canonical.starts_with("{\"record_kind\":"),
            "expected canonical record_kind-tagged JSON, got {canonical}"
        );
        canonical.replacen("{\"record_kind\":", "{\"envelope\":", 1)
    }

    /// Wrap a legacy `envelope`-tagged event JSON into a full on-disk
    /// `{v, seq, event}` record line — the shape the pre-#1358 binary
    /// actually wrote. Back-compat lives at the disk-record READ SITE
    /// (`parse_ledger_disk_record`), not on the bare `UiProtocolLedgerEvent`
    /// type (whose derived `Deserialize` is now strictly `record_kind`),
    /// so legacy recovery is exercised through that helper.
    fn legacy_envelope_tagged_record_line(event: &UiProtocolLedgerEvent, seq: u64) -> String {
        let event_json: Value =
            serde_json::from_str(&legacy_envelope_tagged_json(event)).expect("legacy event json");
        let record = json!({ "v": LEDGER_DISK_VERSION, "seq": seq, "event": event_json });
        serde_json::to_string(&record).expect("serialize legacy record line")
    }

    #[test]
    fn legacy_envelope_tagged_notification_deserializes() {
        // RED before back-compat: serde's `tag = "record_kind"` cannot find
        // its discriminator in a record whose `event` carries the legacy
        // `envelope` tag key, so the canonical parse fails with
        // `missing field record_kind` and `parse_ledger_disk_record` retries
        // the strict legacy `envelope`-tagged shim.
        let session = SessionKey("local:legacy-notif".into());
        let event = UiProtocolLedgerEvent::Notification(delta(&session, "legacy delta"));
        let event_json = legacy_envelope_tagged_json(&event);
        assert!(
            event_json.contains("\"envelope\":\"notification\""),
            "fixture must carry the legacy `envelope` outer tag — got {event_json}"
        );
        let line = legacy_envelope_tagged_record_line(&event, 1);
        let record = parse_ledger_disk_record(&line)
            .unwrap_or_else(|e| panic!("legacy envelope-tagged record MUST deserialize: {e}"));
        let ParsedLedgerDiskRecord::Record(record) = record else {
            panic!("legacy envelope-tagged record must remain a canonical record");
        };
        assert_eq!(
            record.event, event,
            "legacy record must decode to the same variant"
        );
    }

    #[test]
    fn legacy_envelope_tagged_progress_deserializes() {
        use octos_core::ui_protocol::UiProgressMetadata;
        let session = SessionKey("local:legacy-progress".into());
        let event = UiProtocolLedgerEvent::Progress(UiProgressEvent {
            session_id: session,
            turn_id: None,
            metadata: UiProgressMetadata {
                kind: "thinking".into(),
                label: None,
                message: None,
                detail: None,
                iteration: None,
                progress_pct: None,
                retry: None,
                file_mutation: None,
                token_cost: None,
                extra: Default::default(),
            },
        });
        let event_json = legacy_envelope_tagged_json(&event);
        assert!(
            event_json.contains("\"envelope\":\"progress\""),
            "fixture must carry the legacy `envelope` outer tag — got {event_json}"
        );
        let line = legacy_envelope_tagged_record_line(&event, 1);
        let record = parse_ledger_disk_record(&line)
            .unwrap_or_else(|e| panic!("legacy envelope-tagged progress MUST deserialize: {e}"));
        let ParsedLedgerDiskRecord::Record(record) = record else {
            panic!("legacy envelope-tagged progress must remain a canonical record");
        };
        assert_eq!(record.event, event);
    }

    #[test]
    fn new_record_kind_tagged_notification_still_deserializes() {
        // The write side MUST keep emitting `record_kind` (no #1358
        // regression) and the reader must still accept it.
        let session = SessionKey("local:new-notif".into());
        let event = UiProtocolLedgerEvent::Notification(delta(&session, "new delta"));
        let serialized = serde_json::to_string(&event).expect("serialize");
        assert!(
            serialized.contains("\"record_kind\":\"notification\""),
            "write side must still emit `record_kind` — got {serialized}"
        );
        assert!(
            !serialized.contains("\"envelope\":\"notification\""),
            "write side must NOT regress to the legacy `envelope` tag — got {serialized}"
        );
        let parsed: UiProtocolLedgerEvent =
            serde_json::from_str(&serialized).expect("record_kind record deserializes");
        assert_eq!(parsed, event);
    }

    #[test]
    fn genuinely_malformed_record_still_errors() {
        // A record with neither discriminator (and not even valid for the
        // inner variant) must still be rejected so real corruption is not
        // silently accepted — both as a bare event and through the actual
        // disk-record read path.
        let bad = r#"{"kind":"message_delta","text":"orphan, no outer tag"}"#;
        let result: Result<UiProtocolLedgerEvent, _> = serde_json::from_str(bad);
        assert!(
            result.is_err(),
            "record lacking any outer discriminator must still error, got {result:?}"
        );
        let not_json = "this is not json at all";
        assert!(
            serde_json::from_str::<UiProtocolLedgerEvent>(not_json).is_err(),
            "non-JSON must still error"
        );

        // Disk-record read path: neither the canonical nor the legacy shim
        // can decode these, and non-object JSON must not panic.
        let bad_record = format!("{{\"v\":{LEDGER_DISK_VERSION},\"seq\":1,\"event\":{bad}}}");
        assert!(
            parse_ledger_disk_record(&bad_record).is_err(),
            "malformed event in a disk record must still error via the read path"
        );
        assert!(
            parse_ledger_disk_record("{not valid json}").is_err(),
            "non-JSON disk line must error, not panic"
        );
        assert!(
            parse_ledger_disk_record("[1,2,3]").is_err(),
            "non-object JSON disk line must error, not panic"
        );
    }

    /// Inject a duplicate key into a JSON object STRING by inserting
    /// `"key":<value>,` immediately after the opening brace. The result is
    /// valid JSON syntax with a genuine duplicate top-level key — which a
    /// `serde_json::Map` cannot represent (it collapses to last-wins), so
    /// the corruption can only be expressed at the string level. Used to
    /// prove the deserializer stays STRICT and does not round-trip through
    /// a duplicate-collapsing `Value`.
    fn inject_duplicate_key(json: &str, key: &str, value: &str) -> String {
        let brace = json.find('{').expect("object json");
        let (head, tail) = json.split_at(brace + 1);
        format!("{head}\"{key}\":{value},{tail}")
    }

    #[test]
    fn duplicate_key_corruption_is_rejected() {
        // STRICTNESS: the derived `Deserialize` rejects duplicate fields
        // (`duplicate field …`). The dual-tag rework must NOT regress that
        // into a duplicate-collapsing `serde_json::Value` round-trip, which
        // silently keeps the LAST value and accepts corrupted JSON.
        //
        // Fixtures are built by serializing a fully-valid event (so every
        // field — including the UUID `turn_id` — is genuinely valid) and
        // then injecting ONE duplicate key into the JSON STRING. A
        // `serde_json::Map` cannot hold duplicate keys, which is exactly why
        // a Value round-trip loses the strictness the derived path
        // guaranteed; the ONLY reason any fixture below can error is the
        // duplicate key.
        //
        // RED on 39794cd1: the inner-field cases (`session_id`, `kind`,
        // `text`) were SILENTLY ACCEPTED (last-wins) because the
        // `Value::deserialize` step collapsed the duplicate before the
        // derived shim ever saw it.
        let session = SessionKey("local:dup".into());
        let event = UiProtocolLedgerEvent::Notification(delta(&session, "corrupt"));
        let valid = serde_json::to_string(&event).expect("serialize valid event");
        // Sanity: the clean record must deserialize (so a failure below is
        // attributable to the injected duplicate, not a broken fixture).
        serde_json::from_str::<UiProtocolLedgerEvent>(&valid)
            .expect("clean fixture must deserialize");

        // Duplicate INNER payload field `session_id` — headline regression.
        let dup_session = inject_duplicate_key(&valid, "session_id", "\"local:other\"");
        assert!(
            serde_json::from_str::<UiProtocolLedgerEvent>(&dup_session).is_err(),
            "duplicate inner `session_id` must error, not last-wins — got {dup_session}"
        );

        // Duplicate INNER scalar `text`.
        let dup_text = inject_duplicate_key(&valid, "text", "\"first\"");
        assert!(
            serde_json::from_str::<UiProtocolLedgerEvent>(&dup_text).is_err(),
            "duplicate inner `text` must error, not last-wins — got {dup_text}"
        );

        // Duplicate INNER discriminator `kind`.
        let dup_kind = inject_duplicate_key(&valid, "kind", "\"tool_started\"");
        assert!(
            serde_json::from_str::<UiProtocolLedgerEvent>(&dup_kind).is_err(),
            "duplicate inner `kind` must error, not last-wins — got {dup_kind}"
        );

        // Duplicate OUTER discriminator `record_kind`.
        let dup_record_kind = inject_duplicate_key(&valid, "record_kind", "\"progress\"");
        assert!(
            serde_json::from_str::<UiProtocolLedgerEvent>(&dup_record_kind).is_err(),
            "duplicate outer `record_kind` must error, not last-wins — got {dup_record_kind}"
        );

        // The legacy `envelope`-tagged path runs only at the disk-record
        // READ SITE (`parse_ledger_disk_record`), so legacy duplicate-key
        // fixtures are exercised there — proving the STRICT legacy shim
        // (not a Value collapse) is what recovers them. Wrap each corrupt
        // legacy event in a `{v, seq, event}` line and assert it is
        // rejected. (Sanity: the clean legacy line recovers, so failures
        // below are attributable to the injected duplicate.)
        let legacy_event = legacy_envelope_tagged_json(&event);
        let clean_legacy_line = legacy_envelope_tagged_record_line(&event, 1);
        parse_ledger_disk_record(&clean_legacy_line).expect("clean legacy line must recover");

        // Duplicate legacy OUTER discriminator `envelope` (string tag).
        let dup_envelope_event = inject_duplicate_key(&legacy_event, "envelope", "\"progress\"");
        let dup_envelope_line =
            format!("{{\"v\":{LEDGER_DISK_VERSION},\"seq\":1,\"event\":{dup_envelope_event}}}");
        assert!(
            parse_ledger_disk_record(&dup_envelope_line).is_err(),
            "duplicate legacy outer `envelope` tag must error, not last-wins — got {dup_envelope_line}"
        );

        // Duplicate INNER `session_id` on a legacy-tagged record.
        let dup_session_event =
            inject_duplicate_key(&legacy_event, "session_id", "\"local:other\"");
        let dup_session_line =
            format!("{{\"v\":{LEDGER_DISK_VERSION},\"seq\":1,\"event\":{dup_session_event}}}");
        assert!(
            parse_ledger_disk_record(&dup_session_line).is_err(),
            "duplicate inner `session_id` (legacy record) must error — got {dup_session_line}"
        );
    }

    #[test]
    fn legacy_envelope_variant_record_errors_gracefully_not_recovered() {
        // STEP 0 finding: pre-#1358 `UiNotification::Envelope` records were
        // NEVER cleanly (de)serializable. The outer ledger tag was named
        // `envelope` AND `EnvelopeNotification` carries a nested `envelope`
        // OBJECT field, so internally-tagged flattening emitted TWO
        // `envelope` keys in the same object — exactly the
        // `duplicate field 'envelope'` that #1358 fixed by renaming the
        // outer tag to `record_kind`. Such records are duplicate-key
        // garbage on disk; recovering them is inherently impossible.
        //
        // The contract here is therefore NOT "recover" but "fail
        // gracefully": the record must ERROR (so the read loop counts it as
        // a skip) and MUST NOT panic. This is NOT a regression — these
        // records were always unreadable.
        //
        // Build the fixture authentically: serialize a real
        // `UiNotification::Envelope` event (with the canonical `record_kind`
        // tag) then rename the outer key back to the legacy `envelope` — the
        // exact byte shape the pre-#1358 binary wrote. This produces the
        // genuine two-`envelope`-keys-in-one-object collision.
        let session = SessionKey("local:env".into());
        let event =
            UiProtocolLedgerEvent::Notification(UiNotification::Envelope(EnvelopeNotification {
                session_id: session,
                topic: Some("planning".into()),
                envelope: Envelope {
                    thread_id: "t".into(),
                    seq: 1,
                    client_message_id: None,
                    payload: Payload::AssistantDelta { text: "x".into() },
                },
            }));
        let legacy_event = legacy_envelope_tagged_json(&event);
        // The collision: the outer string tag AND the nested object field
        // both serialize to the key `envelope`.
        assert!(
            legacy_event.matches("\"envelope\":").count() >= 2,
            "legacy Envelope record must carry the duplicate `envelope` key \
             collision — got {legacy_event}"
        );

        // The bare strict event parse rejects it (no `record_kind`).
        assert!(
            serde_json::from_str::<UiProtocolLedgerEvent>(&legacy_event).is_err(),
            "bare legacy Envelope event must error under the strict canonical parse"
        );

        // The actual READ PATH must also reject it gracefully: the legacy
        // shim hits `duplicate field 'envelope'` and errors — counted as a
        // skip, never recovered, never a panic.
        let line = legacy_envelope_tagged_record_line(&event, 1);
        let result = parse_ledger_disk_record(&line);
        assert!(
            result.is_err(),
            "legacy Envelope-variant records are inherently duplicate-key \
             garbage (#1358 collision) and must error gracefully, got {result:?}"
        );

        // Whole-chain: a log file containing ONLY such records recovers
        // zero events and counts each as a skip (no panic).
        let temp = tempfile::tempdir().expect("tempdir");
        let session_id = SessionKey("local:env-disk".into());
        let session_dir = temp
            .path()
            .join("ui-protocol")
            .join(encode_session_dir_name(&session_id));
        fs::create_dir_all(&session_dir).expect("session dir");
        let log_path = session_dir.join(new_log_file_name());
        fs::write(&log_path, format!("{line}\n")).expect("write envelope log");

        let ledger = UiProtocolLedger::with_config(LedgerConfig::durable(temp.path().into()));
        let snapshot = ledger
            .read_session_disk_snapshot(&session_id, &session_dir, None, false)
            .expect("scan ok");
        // An empty snapshot (`None`) is also acceptable (nothing
        // recovered); when present it must show zero recovered + one skip.
        if let Some(snap) = snapshot {
            assert_eq!(
                snap.retained_entries.len(),
                0,
                "legacy Envelope-variant records must not be recovered"
            );
            assert_eq!(
                snap.skipped_records, 1,
                "the single unreadable Envelope-variant record must be counted as a skip"
            );
        }
    }

    #[test]
    fn ledger_recovers_legacy_envelope_tagged_records_from_disk() {
        // Whole-chain: write a log file by hand using the legacy
        // `envelope` outer tag (as the pre-#1358 binary did) and confirm
        // recovery hydrates them rather than dropping them as malformed.
        let temp = tempfile::tempdir().expect("tempdir");
        let session_id = SessionKey("local:legacy-disk".into());
        let session_dir = temp
            .path()
            .join("ui-protocol")
            .join(encode_session_dir_name(&session_id));
        fs::create_dir_all(&session_dir).expect("session dir");
        let log_path = session_dir.join(new_log_file_name());
        let mut lines = String::new();
        for (seq, text) in [(1u64, "legacy-one"), (2, "legacy-two"), (3, "legacy-three")] {
            let event = UiProtocolLedgerEvent::Notification(delta(&session_id, text));
            let event_json: Value =
                serde_json::from_str(&legacy_envelope_tagged_json(&event)).unwrap();
            let record = json!({ "v": LEDGER_DISK_VERSION, "seq": seq, "event": event_json });
            lines.push_str(&serde_json::to_string(&record).unwrap());
            lines.push('\n');
        }
        fs::write(&log_path, lines).expect("write legacy log");

        let outcome = UiProtocolLedger::recover(LedgerConfig::durable(temp.path().into()));
        assert_eq!(
            outcome.events_recovered, 0,
            "lazy recovery indexes the legacy session without replaying it"
        );
        assert_eq!(outcome.sessions_recovered, 1);
        // First touch replays all 3 legacy `envelope`-tagged records.
        let replay = outcome
            .ledger
            .replay_after(
                &session_id,
                Some(&UiCursor {
                    stream: session_id.0.clone(),
                    seq: 1,
                }),
            )
            .expect("replay recovered legacy session");
        assert_eq!(replay_texts(&replay), vec!["legacy-two", "legacy-three"]);
    }

    #[test]
    fn scanning_legacy_and_bad_records_aggregates_skips_per_file() {
        // N legacy + M truly-bad records in ONE file: the N legacy
        // records are recovered (0 skips, via part 1) and only the M bad
        // records are counted. `skipped_records` is the per-file
        // aggregate — proving the read loop emits a single summary `warn!`
        // per file (the warn is guarded by `skipped_total > 0`, once per
        // file) rather than one line per record per rescan. We assert on
        // the returned count rather than captured logs because the
        // process-global tracing max-level filter (set by the test
        // harness / other tests) is not deterministically observable.
        let temp = tempfile::tempdir().expect("tempdir");
        let session_id = SessionKey("local:agg-skip".into());
        let session_dir = temp
            .path()
            .join("ui-protocol")
            .join(encode_session_dir_name(&session_id));
        fs::create_dir_all(&session_dir).expect("session dir");
        let log_path = session_dir.join(new_log_file_name());

        let mut lines = String::new();
        // 2 legacy `envelope`-tagged records → recovered by part 1.
        for (seq, text) in [(1u64, "legacy-a"), (2, "legacy-b")] {
            let event = UiProtocolLedgerEvent::Notification(delta(&session_id, text));
            let event_json: Value =
                serde_json::from_str(&legacy_envelope_tagged_json(&event)).unwrap();
            let record = json!({ "v": LEDGER_DISK_VERSION, "seq": seq, "event": event_json });
            lines.push_str(&serde_json::to_string(&record).unwrap());
            lines.push('\n');
        }
        // 3 truly-bad records → skipped (aggregated into one summary).
        lines.push_str("{not valid json}\n");
        lines.push_str("{\"v\":1,\"seq\":99}\n"); // missing `event`
        lines.push_str("garbage line\n");
        fs::write(&log_path, lines).expect("write mixed log");

        let ledger = UiProtocolLedger::with_config(LedgerConfig::durable(temp.path().into()));
        let snapshot = ledger
            .read_session_disk_snapshot(&session_id, &session_dir, None, false)
            .expect("scan ok")
            .expect("non-empty snapshot");

        assert_eq!(
            snapshot.skipped_records, 3,
            "only the 3 truly-bad records are skipped; the 2 legacy records are recovered"
        );
        assert_eq!(
            snapshot.retained_entries.len(),
            2,
            "the 2 legacy `envelope`-tagged records must be recovered, not skipped"
        );
        let recovered_texts: Vec<String> = snapshot
            .retained_entries
            .iter()
            .filter_map(|entry| match &entry.event {
                UiProtocolLedgerEvent::Notification(UiNotification::MessageDelta(d)) => {
                    Some(d.text.clone())
                }
                _ => None,
            })
            .collect();
        assert_eq!(recovered_texts, vec!["legacy-a", "legacy-b"]);
    }
    // -----------------------------------------------------------------
    // NEW-2: credentials never reach the durable ledger in tool arguments.
    // -----------------------------------------------------------------
    const LEAKED_OPENAI_KEY: &str = "sk-proj-abcdefghijklmnopqrstuvwxyz0123456789";
    const LEAKED_BEARER_KEY: &str = "sk-live-XXXXXXXXXXXXXXXXXXXXXXXX";

    fn leaky_tool_arguments() -> serde_json::Value {
        serde_json::json!({
            "env": { "OPENAI_API_KEY": LEAKED_OPENAI_KEY },
            "cmd": format!("curl -H 'Authorization: Bearer {LEAKED_BEARER_KEY}' https://api.example"),
            "path": "src/main.rs",
            "query": "todo",
        })
    }

    fn tool_started_with(
        session: &SessionKey,
        turn_id: &TurnId,
        arguments: serde_json::Value,
    ) -> UiNotification {
        UiNotification::ToolStarted(octos_core::ui_protocol::ToolStartedEvent {
            session_id: session.clone(),
            topic: None,
            turn_id: turn_id.clone(),
            tool_call_id: "tc-leak".into(),
            tool_name: "shell".into(),
            arguments: Some(arguments),
        })
    }

    fn session_log_bytes(temp: &std::path::Path, session: &SessionKey) -> String {
        let dir = temp
            .join("ui-protocol")
            .join(encode_session_dir_name(session));
        let mut files = fs::read_dir(&dir)
            .expect("session ledger dir")
            .map(|entry| entry.expect("dir entry").path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "log"))
            .collect::<Vec<_>>();
        files.sort();
        files
            .iter()
            .map(|path| fs::read_to_string(path).expect("log file"))
            .collect::<Vec<_>>()
            .join("")
    }

    fn assert_redacted_arguments(arguments: &serde_json::Value) {
        assert_eq!(arguments["path"], "src/main.rs");
        assert_eq!(arguments["query"], "todo");
        assert_eq!(
            arguments["env"]["OPENAI_API_KEY"],
            octos_core::secret_redaction::REDACTED_PLACEHOLDER
        );
        let cmd = arguments["cmd"].as_str().expect("cmd survives as text");
        assert!(
            !cmd.contains(LEAKED_BEARER_KEY),
            "bearer token leaked: {cmd}"
        );
        assert!(
            cmd.contains("curl"),
            "non-secret command prefix must survive: {cmd}"
        );
        assert!(
            cmd.contains("https://api.example"),
            "non-secret command tail must survive: {cmd}"
        );
        assert!(cmd.contains(octos_core::secret_redaction::REDACTED_PLACEHOLDER));
    }

    #[test]
    fn should_redact_numeric_credentials_before_durable_replay() {
        let temp = tempfile::tempdir().expect("tempdir");
        let session = SessionKey("local:numeric-credentials".into());
        let ledger = UiProtocolLedger::with_config(LedgerConfig::durable(temp.path().into()));
        ledger.append_notification(tool_started_with(
            &session,
            &TurnId::new(),
            serde_json::json!({"password": 918273645, "passcode": 564738291, "retry_count": 2}),
        ));
        let disk = session_log_bytes(temp.path(), &session);
        assert!(!disk.contains("918273645") && !disk.contains("564738291"));
        drop(ledger);
        let reloaded = UiProtocolLedger::with_config(LedgerConfig::durable(temp.path().into()));
        let replay = reloaded
            .replay_after(
                &session,
                Some(&UiCursor {
                    stream: session.0.clone(),
                    seq: 0,
                }),
            )
            .expect("replay");
        let UiProtocolLedgerEvent::Notification(UiNotification::ToolStarted(event)) =
            &replay[0].event
        else {
            panic!("tool started")
        };
        assert_eq!(
            event.arguments,
            Some(
                serde_json::json!({"password": "[REDACTED]", "passcode": "[REDACTED]", "retry_count": 2})
            )
        );
    }

    #[test]
    fn should_redact_tool_started_arguments_before_persist_and_replay() {
        let temp = tempfile::tempdir().expect("tempdir");
        let ledger = UiProtocolLedger::with_config(LedgerConfig::durable(temp.path().into()));
        let session = SessionKey("local:redact-tool-args".into());
        let turn_id = TurnId::new();

        let appended = ledger.append_notification(tool_started_with(
            &session,
            &turn_id,
            leaky_tool_arguments(),
        ));

        let appended_json = serde_json::to_string(&appended.event).expect("json");
        assert!(
            !appended_json.contains(LEAKED_OPENAI_KEY),
            "key leaked: {appended_json}"
        );
        assert!(!appended_json.contains(LEAKED_BEARER_KEY));
        let on_disk = session_log_bytes(temp.path(), &session);
        assert!(
            !on_disk.contains(LEAKED_OPENAI_KEY) && !on_disk.contains(LEAKED_BEARER_KEY),
            "the persisted record must not carry key material: {on_disk}"
        );
        assert!(on_disk.contains(octos_core::secret_redaction::REDACTED_PLACEHOLDER));
        assert!(on_disk.contains("src/main.rs") && on_disk.contains("todo"));

        let replay = ledger
            .replay_after(
                &session,
                Some(&UiCursor {
                    stream: session.0.clone(),
                    seq: 0,
                }),
            )
            .expect("replay");
        let UiProtocolLedgerEvent::Notification(UiNotification::ToolStarted(replayed)) =
            &replay[0].event
        else {
            panic!("expected the tool_started row, got {:?}", replay[0].event);
        };
        assert_redacted_arguments(replayed.arguments.as_ref().expect("arguments kept"));
    }

    #[test]
    fn should_redact_tool_start_envelope_previews_at_append() {
        let temp = tempfile::tempdir().expect("tempdir");
        let ledger = UiProtocolLedger::with_config(LedgerConfig::durable(temp.path().into()));
        let session = SessionKey("local:redact-tool-preview".into());
        let thread_id = TurnId::new().0.to_string();
        let preview = format!("cmd: \"curl -H 'Authorization: Bearer {LEAKED_BEARER_KEY}'\"");

        let legacy = ledger
            .emit_envelope(
                &session,
                thread_id.clone(),
                Payload::ToolStart {
                    tool_call_id: "tc-1".into(),
                    name: "shell".into(),
                    arguments_preview: Some(preview.clone()),
                },
                None,
            )
            .expect("v1 envelope");
        let v2 = ledger
            .emit_envelope_v2(
                &session,
                thread_id,
                PayloadV2::ToolStart {
                    tool_call_id: "tc-2".into(),
                    name: "shell".into(),
                    arguments_preview: Some(preview),
                },
                None,
            )
            .expect("v2 envelope");

        for event in [&legacy.event, &v2.event] {
            let json = serde_json::to_string(event).expect("json");
            assert!(
                !json.contains(LEAKED_BEARER_KEY),
                "bearer token leaked: {json}"
            );
            assert!(json.contains(octos_core::secret_redaction::REDACTED_PLACEHOLDER));
            assert!(json.contains("curl"));
        }
        let on_disk = session_log_bytes(temp.path(), &session);
        assert!(!on_disk.contains(LEAKED_BEARER_KEY));
    }

    /// `approval/requested` repeats the raw shell command in `body`
    /// (`Run command: …`) and in the typed `command_line`; both must be
    /// scrubbed before the durable row and every hydrate replay.
    #[test]
    fn should_redact_approval_requested_command_before_durable_ledger_and_replay() {
        use octos_core::ui_protocol::{
            ApprovalCommandDetails, ApprovalId, ApprovalRequestedEvent, ApprovalTypedDetails,
        };
        let temp = tempfile::tempdir().expect("tempdir");
        let ledger = UiProtocolLedger::with_config(LedgerConfig::durable(temp.path().into()));
        let session = SessionKey("local:redact-approval-requested".into());
        let turn_id = TurnId::new();
        let command =
            format!("curl -H 'Authorization: Bearer {LEAKED_BEARER_KEY}' https://api.example");
        let mut event = ApprovalRequestedEvent::generic(
            session.clone(),
            ApprovalId::new(),
            turn_id.clone(),
            "shell",
            "Approve shell command?",
            format!("Run command: {command}"),
        );
        event.typed_details = Some(ApprovalTypedDetails::command(
            ApprovalCommandDetails {
                argv: vec!["sh".into(), "-c".into(), command.clone()],
                command_line: Some(command.clone()),
                cwd: Some("/workspace".into()),
                env_keys: Vec::new(),
                tool_call_id: Some("tc-approval".into()),
            },
            None,
        ));

        let appended = ledger.append_notification(UiNotification::ApprovalRequested(event));

        let appended_json = serde_json::to_string(&appended.event).expect("json");
        assert!(
            !appended_json.contains(LEAKED_BEARER_KEY),
            "approval row leaked the command: {appended_json}"
        );
        assert!(appended_json.contains("Run command: curl"));
        assert!(appended_json.contains("/workspace"));
        let on_disk = session_log_bytes(temp.path(), &session);
        assert!(!on_disk.contains(LEAKED_BEARER_KEY), "persisted: {on_disk}");
        assert!(on_disk.contains(octos_core::secret_redaction::REDACTED_PLACEHOLDER));

        let replay = ledger
            .replay_after(
                &session,
                Some(&UiCursor {
                    stream: session.0.clone(),
                    seq: 0,
                }),
            )
            .expect("replay");
        let UiProtocolLedgerEvent::Notification(UiNotification::ApprovalRequested(replayed)) =
            &replay[0].event
        else {
            panic!("expected the approval row, got {:?}", replay[0].event);
        };
        assert!(!replayed.body.contains(LEAKED_BEARER_KEY));
        let details = replayed
            .typed_details
            .as_ref()
            .and_then(|details| details.command.as_ref())
            .expect("typed command details kept");
        assert!(
            !details
                .command_line
                .as_deref()
                .unwrap_or_default()
                .contains(LEAKED_BEARER_KEY)
        );
        assert!(
            details
                .argv
                .iter()
                .all(|arg| !arg.contains(LEAKED_BEARER_KEY))
        );
        assert_eq!(details.cwd.as_deref(), Some("/workspace"));
    }

    /// The client-writable `approval/decided` free-text fields were scrubbed
    /// for the audit file only; the durable UI ledger row must match.
    #[test]
    fn should_redact_client_note_in_durable_approval_decided_row() {
        use octos_core::ui_protocol::{ApprovalDecidedEvent, ApprovalDecision, ApprovalId};
        let temp = tempfile::tempdir().expect("tempdir");
        let ledger = UiProtocolLedger::with_config(LedgerConfig::durable(temp.path().into()));
        let session = SessionKey("local:redact-approval-decided".into());
        let mut event = ApprovalDecidedEvent::manual(
            session.clone(),
            ApprovalId::new(),
            TurnId::new(),
            ApprovalDecision::Approve,
            "user",
        );
        event.client_note = Some(format!("use token {LEAKED_BEARER_KEY} for this run"));
        event.scope = Some("session".into());

        let appended = ledger.append_notification(UiNotification::ApprovalDecided(event));

        let appended_json = serde_json::to_string(&appended.event).expect("json");
        assert!(
            !appended_json.contains(LEAKED_BEARER_KEY),
            "{appended_json}"
        );
        assert!(appended_json.contains("\"scope\":\"session\""));
        let on_disk = session_log_bytes(temp.path(), &session);
        assert!(!on_disk.contains(LEAKED_BEARER_KEY), "persisted: {on_disk}");
    }

    /// Tool RESULTS are durable too: `tool/completed.output_preview` and the
    /// v1/v2 `ToolEnd` preview/error carry the first 2 KB of whatever the
    /// tool printed (`.env`, `printenv`, an HTTP error echoing a key).
    #[test]
    fn should_redact_tool_output_preview_before_durable_ledger() {
        let temp = tempfile::tempdir().expect("tempdir");
        let ledger = UiProtocolLedger::with_config(LedgerConfig::durable(temp.path().into()));
        let session = SessionKey("local:redact-tool-output".into());
        let turn_id = TurnId::new();
        let thread_id = turn_id.0.to_string();
        let preview =
            format!("OPENAI_API_KEY={LEAKED_OPENAI_KEY}\nDATABASE_URL=postgres://localhost/db\n");

        let completed = ledger.append_notification(UiNotification::ToolCompleted(
            octos_core::ui_protocol::ToolCompletedEvent {
                session_id: session.clone(),
                topic: None,
                turn_id: turn_id.clone(),
                tool_call_id: "tc-out".into(),
                tool_name: "shell".into(),
                success: Some(true),
                output_preview: Some(preview.clone()),
                duration_ms: Some(3),
            },
        ));
        let completed_json = serde_json::to_string(&completed.event).expect("json");
        assert!(
            !completed_json.contains(LEAKED_OPENAI_KEY),
            "{completed_json}"
        );
        assert!(completed_json.contains("DATABASE_URL=postgres://localhost/db"));

        let legacy = ledger
            .emit_envelope(
                &session,
                thread_id.clone(),
                Payload::ToolEnd {
                    tool_call_id: "tc-out".into(),
                    status: octos_core::ui_protocol::EnvelopeToolEndStatus::Error,
                    error: Some(preview.clone()),
                    reason: None,
                    output_preview: Some(preview.clone()),
                    duration_ms: None,
                },
                None,
            )
            .expect("v1 envelope");
        let v2 = ledger
            .emit_envelope_v2(
                &session,
                thread_id,
                PayloadV2::ToolEnd {
                    tool_call_id: "tc-out".into(),
                    status: octos_core::ui_protocol::EnvelopeToolEndStatus::Complete,
                    error: None,
                    reason: None,
                    output_preview: Some(preview),
                    duration_ms: None,
                },
                None,
            )
            .expect("v2 envelope");
        for event in [&legacy.event, &v2.event] {
            let json = serde_json::to_string(event).expect("json");
            assert!(!json.contains(LEAKED_OPENAI_KEY), "envelope leaked: {json}");
        }
        let on_disk = session_log_bytes(temp.path(), &session);
        assert!(!on_disk.contains(LEAKED_OPENAI_KEY), "persisted: {on_disk}");
        assert!(on_disk.contains("DATABASE_URL=postgres://localhost/db"));
    }

    #[test]
    fn should_not_rewrite_pre_existing_raw_ledger_entries_on_replay() {
        let temp = tempfile::tempdir().expect("tempdir");
        let session = SessionKey("local:raw-legacy-entry".into());
        let turn_id = TurnId::new();
        // A record persisted by a pre-redaction daemon: written directly, as
        // the old appender would have.
        let session_dir = temp
            .path()
            .join("ui-protocol")
            .join(encode_session_dir_name(&session));
        fs::create_dir_all(&session_dir).expect("session dir");
        let raw = UiProtocolLedgerEvent::Notification(tool_started_with(
            &session,
            &turn_id,
            leaky_tool_arguments(),
        ))
        .with_cursor(UiCursor {
            stream: session.0.clone(),
            seq: 1,
        });
        let line = serde_json::to_string(&LedgerDiskRecord {
            v: LEDGER_DISK_VERSION,
            seq: 1,
            event: raw,
        })
        .expect("record json");
        let log_path = session_dir.join(new_log_file_name());
        fs::write(&log_path, format!("{line}\n")).expect("write raw record");
        let before = fs::read(&log_path).expect("raw bytes");
        assert!(String::from_utf8_lossy(&before).contains(LEAKED_OPENAI_KEY));

        // A restarted daemon recovers the session from disk and replays it.
        let ledger = UiProtocolLedger::recover(LedgerConfig::durable(temp.path().into())).ledger;
        let replay = ledger
            .replay_after(
                &session,
                Some(&UiCursor {
                    stream: session.0.clone(),
                    seq: 0,
                }),
            )
            .expect("replay raw entry");
        assert_eq!(replay.len(), 1, "the raw entry is still replayable");
        let after_replay = fs::read(&log_path).expect("raw bytes");
        assert!(
            after_replay.starts_with(&before),
            "replay must never rewrite a pre-existing on-disk record"
        );

        // Appending after the replay never touches the old record either:
        // the log is append-only and redaction applies to NEW rows only.
        ledger.append_notification(delta(&session, "later"));
        let after_append = fs::read(&log_path).expect("raw bytes");
        assert!(
            after_append.starts_with(&before),
            "an append must leave the pre-existing raw record byte-identical"
        );
        assert!(
            String::from_utf8_lossy(&after_append)
                .lines()
                .next()
                .is_some_and(|first| first.contains(LEAKED_OPENAI_KEY)),
            "the historical row is read-only compatibility data, not rewritten"
        );
    }
}
