//! In-memory + on-disk UI Protocol event ledger (M9.6 + M9-FIX-05).
//!
//! ## Durability model — Path A
//!
//! Each session owns a `SessionLedger`. The hot path is an LRU-managed
//! ring buffer in memory; the cold/durable path is a per-session
//! append-only JSON-Lines log under
//! `<data_dir>/ui-protocol/<safe_session_id>/ledger-<epoch_micros>.log`.
//! Short identities keep their legacy hex directory. Longer identities use a
//! bounded SHA-256 directory plus a private, atomically published full identity
//! record, verified before recovery/read/write. Logical session keys are unchanged.
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
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use octos_core::SessionKey;
use octos_core::ui_protocol::{
    EnvelopeV2, EnvelopeV2Notification, PayloadV2, RpcError, RpcNotification, SessionOpened,
    TaskRuntimeState, TurnCompletedEvent, TurnErrorEvent, TurnId, UiCursor, UiFileMutationNotice,
    UiNotification, UiProgressEvent, methods,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::broadcast;
use tracing::{debug, info, warn};

/// Per-session broadcast buffer size. Bounded so a slow subscriber cannot
/// pin unbounded memory; on overflow the receiver sees `Lagged(n)` and
/// the connection should fall back to cursor-based replay. The ledger is
/// still the durable source of truth — broadcast is a live-fan-out shortcut.
const LIVE_BROADCAST_CAPACITY: usize = 256;

#[path = "ui_protocol_retained_references.rs"]
mod retained_references;
pub(crate) use retained_references::{file_mutation, hydrate_references_error};

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
    /// Wall-clock time captured once at durable append. Legacy records have
    /// no value; projections must never replace it with their own clock.
    pub(crate) recorded_at: Option<chrono::DateTime<chrono::Utc>>,
    pub(crate) event: UiProtocolLedgerEvent,
    /// If `Some`, identifies the connection whose handler direct-sent this
    /// event to the wire. Forwarders running on the same connection must
    /// skip these to avoid double delivery; forwarders on other
    /// connections deliver normally.
    pub(crate) from_connection: Option<ConnectionId>,
    /// UPCR-2026-036 (#2625): an approval or question event of a prompt
    /// raised by an external client's turn under `serve --host-managed`.
    /// Durable (see [`LedgerDiskRecord::external_prompt`]), so the prompt
    /// stays hidden from the host after a restart or once the transport's
    /// in-memory owner table has forgotten it.
    pub(crate) external_prompt: bool,
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
    /// Durable append time used by terminal projections. Additive and absent
    /// on old records, so disk/snapshot format versions stay unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    recorded_at: Option<chrono::DateTime<chrono::Utc>>,
    /// UPCR-2026-036 (#2625): set on the approval and question events
    /// (`approval/requested`, `approval/decided`, `approval/cancelled`,
    /// `approval/auto_resolved`, `user_question/requested`) of a prompt
    /// raised by an external client's turn under `serve --host-managed`.
    /// Replay, hydrate and live fan-out show such a record only to the live
    /// connection that owns the prompt, so after a restart to nobody.
    ///
    /// Additive, so [`LEDGER_DISK_VERSION`] and `SESSION_SNAPSHOT_VERSION`
    /// stay at 1: it is written only when `true` and after `event` (the
    /// `{"v","seq","event"}` prefix stays intact), a record without it
    /// reads as `false` (behaviour as before), and an older reader ignores
    /// it (the record is not `deny_unknown_fields`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    external_prompt: bool,
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
            external_prompt: false,
            recorded_at: None,
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
    recorded_at: Option<chrono::DateTime<chrono::Utc>>,
    /// See [`LedgerDiskRecord::external_prompt`].
    external_prompt: bool,
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
    /// The session's unresolved external approvals (see
    /// [`SessionLedger::external_prompts`]), rebuilt from EVERY marked
    /// record of the retained logs and snapshot, not just the ring.
    external_prompts: std::collections::HashSet<String>,
    /// Malformed or unknown-version records. Known compatible legacy rows
    /// are excluded; cold recovery and append must reject this history.
    unrecoverable_records: u64,
    /// Total records skipped across all scanned log files (unknown
    /// version + genuinely-malformed), surfaced as an aggregate runtime
    /// warning after the scan.
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
    /// Committed bytes for rotation; rollback checks the pre-write file length.
    active_log_bytes: u64,
    /// Exact pre-write length if rollback itself failed. No further append is admitted.
    pending_rollback: Option<u64>,
    /// External prompt ids awaiting a terminal notification. Retained even
    /// after the bounded connection-owner table evicts an id, so a later
    /// cancellation stays private. Rebuilt from all retained records on reload.
    /// Answered questions release their in-memory marker in the requester.
    /// Since answers have no ledger event, reload can retain historical ids,
    /// bounded by the retained logs, until this session is evicted.
    external_prompts: std::collections::HashSet<String>,
    /// A write error may be partial/ambiguous. Latch the session unavailable
    /// until restart reconciles the actual log instead of retrying a cursor.
    unavailable: Option<String>,
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
            pending_rollback: None,
            external_prompts: std::collections::HashSet::new(),
            unavailable: None,
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
    index_scan_failed: std::sync::atomic::AtomicBool,
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
    failed: Option<RecoveryFailure>,
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
    /// A failed write-ahead allocation must not become a successful retry
    /// in the same process. Restart reconciles the persisted intent and log.
    failed_projections: HashMap<(SessionKey, String), String>,
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

enum ProjectionSeqAllocationError {
    PostCompletion(&'static str),
    Unavailable(LedgerError),
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
        let UiProtocolLedgerEvent::Notification(UiNotification::EnvelopeV2(event)) = &entry.event
        else {
            continue;
        };
        if event.envelope.thread_id != thread {
            continue;
        }
        match event.envelope.payload {
            PayloadV2::AssistantDelta { .. } | PayloadV2::AssistantPersisted { .. } if !open => {
                index += 1;
                open = true;
            }
            PayloadV2::ToolStart { .. } | PayloadV2::ToolEnd { .. } => open = false,
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
    external_prompt: bool,
}

/// Durable ledger failure with enough context to reject an operation without
/// mistaking existing-but-unavailable history for an empty session.
#[derive(Debug)]
pub(crate) struct LedgerError {
    operation: &'static str,
    session_id: SessionKey,
    seq: Option<u64>,
    source: std::io::Error,
}

impl LedgerError {
    fn new(
        operation: &'static str,
        session_id: &SessionKey,
        seq: Option<u64>,
        source: std::io::Error,
    ) -> Self {
        Self {
            operation,
            session_id: session_id.clone(),
            seq,
            source,
        }
    }
}

impl std::fmt::Display for LedgerError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "ledger operation={} session={}",
            self.operation, self.session_id.0
        )?;
        if let Some(seq) = self.seq {
            write!(formatter, " seq={seq}")?;
        }
        write!(formatter, ": {}", self.source)
    }
}

impl std::error::Error for LedgerError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

impl From<LedgerError> for RpcError {
    fn from(error: LedgerError) -> Self {
        RpcError::internal_error(error.to_string())
    }
}

#[cfg(test)]
struct InjectedLedgerWriteSpec {
    session_dir: PathBuf,
    detail: String,
    after_write: bool,
}

#[cfg(test)]
thread_local! {
    static INJECTED_LEDGER_WRITE_FAILURE: std::cell::RefCell<Option<InjectedLedgerWriteSpec>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
#[cfg(test)]
fn injected_ledger_write_error(session_dir: &Path, after_write: bool) -> Option<std::io::Error> {
    INJECTED_LEDGER_WRITE_FAILURE.with(|slot| {
        slot.borrow().as_ref().and_then(|spec| {
            (spec.session_dir == session_dir && spec.after_write == after_write).then(|| {
                std::io::Error::other(format!("injected ledger write failure: {}", spec.detail))
            })
        })
    })
}

#[cfg(test)]
thread_local! {
    static INJECTED_WATERMARK_WRITE_FAILURE: std::cell::RefCell<Option<(PathBuf, String)>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
#[cfg(test)]
fn injected_watermark_write_error(session_dir: &Path) -> Option<std::io::Error> {
    INJECTED_WATERMARK_WRITE_FAILURE.with(|slot| {
        slot.borrow().as_ref().and_then(|(path, detail)| {
            (path == session_dir).then(|| {
                std::io::Error::other(format!("injected watermark write failure: {detail}"))
            })
        })
    })
}

impl LedgerInner {
    fn new() -> Self {
        Self {
            sessions: HashMap::new(),
            lru: VecDeque::new(),
            subscribers: HashMap::new(),
            thread_seq: HashMap::new(),
            failed_projections: HashMap::new(),
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
                    "failed to create ui-protocol data dir; durable operations will report errors"
                );
            }
        }
        Self {
            config,
            inner: Mutex::new(LedgerInner::new()),
            scopes: Mutex::new(HashMap::new()),
            index: Mutex::new(HashMap::new()),
            index_scan_failed: std::sync::atomic::AtomicBool::new(false),
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
    /// `ui-protocol/<safe_session_id>` dir, thread watermarks/seq state, and replay
    /// cursors' `stream` — everything EXCEPT wire event payloads and the
    /// live-subscriber map, which stay on the plain wire id.
    ///
    /// The NUL separator makes the encoding injective against realistic wire
    /// ids: `SessionKey` is an unconstrained string, so a plain-ASCII marker
    /// could be *equalled* by a hostile client naming its session
    /// `<victim>~cwd-<hash>` and thereby sharing the victim project's dir
    /// (codex v2 P2). A NUL pushes that collision outside anything a
    /// legitimate client id contains; the byte is opaque to the stable dir
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
        let indexed = match ledger.index_session_directories(&dir.join("ui-protocol")) {
            Ok(indexed) => indexed,
            Err(error) => {
                ledger
                    .index_scan_failed
                    .store(true, std::sync::atomic::Ordering::Release);
                warn!(target: "octos::ledger", ?error, "ledger index unavailable; reads will retry");
                0
            }
        };
        RecoveryOutcome {
            ledger: Arc::new(ledger),
            sessions_recovered: indexed,
            events_recovered: 0,
        }
    }

    fn fail_session_recovery(
        &self,
        storage_id: &SessionKey,
        session_dir: &Path,
        error: std::io::Error,
    ) -> LedgerError {
        warn!(
            target = "octos::ledger",
            ?error,
            session_id = %storage_id.0,
            "failed to recover session from disk; session unavailable"
        );
        let detail = error.to_string();
        let mut index = self.index.lock().unwrap_or_else(|p| p.into_inner());
        let entry = index
            .entry(storage_id.clone())
            .or_insert_with(|| SessionIndexEntry {
                dir: session_dir.to_path_buf(),
                reconciled: false,
                failed: None,
            });
        // A read retry must never erase a concurrent durable-write latch.
        if entry.failed.as_ref().is_none_or(RecoveryFailure::retryable) {
            entry.failed = Some(RecoveryFailure::from_read(&error, detail));
        }
        LedgerError::new("recover_session", storage_id, None, error)
    }

    /// Replay one indexed session's disk log into the in-memory ring on
    /// first touch, then cache it. Idempotent: a session already resident
    /// in `inner.sessions` returns immediately without any disk I/O.
    ///
    /// A session whose log fails to read is marked `failed` in the index so
    /// read-only I/O failures can be retried after repair. Malformed history
    /// and ambiguous writes remain unavailable until restart; retries must not
    /// repair away a previously confirmed corrupt record.
    ///
    /// Disk I/O happens OUTSIDE the `inner` lock; the hydrate under the
    /// lock applies the stale-live guard so a concurrent append can never
    /// be rolled back by an older snapshot. The orphan-row reconciliation
    /// runs exactly once per successful replay (tracked in the index), and
    /// its synthesized terminal events are appended into the freshly
    /// hydrated ring via [`append_with_storage_id`] — which sees the
    /// session as resident and therefore does not recurse into disk replay.
    /// `Ok(true)` = Loaded/resident, `Ok(false)` = genuinely Absent, and
    /// `Err` = existing-but-Unavailable. Callers that can create history must
    /// not collapse the last case into Absent.
    fn ensure_session_loaded(&self, storage_id: &SessionKey) -> Result<bool, LedgerError> {
        self.check_recovery_latch(storage_id)?;
        {
            let inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(session) = inner.sessions.get(storage_id) {
                check_session_available(storage_id, session)?;
                return Ok(true);
            }
        }
        let Some(data_dir) = self.config.data_dir.as_ref() else {
            return Ok(false);
        };
        if self
            .index_scan_failed
            .load(std::sync::atomic::Ordering::Acquire)
        {
            match self.index_session_directories(&data_dir.join("ui-protocol")) {
                Ok(_) => self
                    .index_scan_failed
                    .store(false, std::sync::atomic::Ordering::Release),
                Err(error) => {
                    // The index scan failed. If the ledger ROOT itself is not
                    // a readable directory (e.g. replaced by a file), every
                    // session is unavailable — fail closed, because on Windows
                    // the direct read below would otherwise swallow the
                    // resulting NotFound as an empty history (Unix surfaces
                    // ENOTDIR as an error). If the root is a valid directory,
                    // the failure is scoped to some OTHER session: keep the
                    // direct read so a healthy session still hydrates
                    // (per-session isolation). T-0225 Windows merge-group.
                    let root = data_dir.join("ui-protocol");
                    if !std::fs::metadata(&root)
                        .map(|m| m.is_dir())
                        .unwrap_or(false)
                    {
                        return Err(LedgerError::new(
                            "index_session_directories",
                            storage_id,
                            None,
                            error,
                        ));
                    }
                    warn!(target: "octos::ledger", ?error,
                        "ledger index retry failed; reading requested session directly");
                }
            }
        }
        let (session_dir, reconciled) = {
            let mut index = self.index.lock().unwrap_or_else(|p| p.into_inner());
            let entry = index
                .entry(storage_id.clone())
                .or_insert_with(|| SessionIndexEntry {
                    dir: data_dir
                        .join("ui-protocol")
                        .join(encode_session_dir_name(storage_id)),
                    reconciled: !self
                        .index_scan_failed
                        .load(std::sync::atomic::Ordering::Acquire),
                    failed: None,
                });
            if let Some(failure) = &entry.failed {
                failure.check(storage_id)?;
            }
            (entry.dir.clone(), entry.reconciled)
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
        let snapshot = match self
            .repair_session_tail(storage_id, &session_dir)
            .and_then(|()| self.read_session_disk_snapshot(storage_id, &session_dir, None, false))
        {
            Ok(snapshot) => snapshot,
            Err(error) => {
                return Err(self.fail_session_recovery(storage_id, &session_dir, error));
            }
        };
        let Some(snapshot) = snapshot else {
            self.clear_read_failure(storage_id);
            // No log files (or all rotated away): nothing to replay. Leave
            // the index entry — a later append will create the ring and
            // `snapshot_if_session_absent` re-reads the dir then.
            return Ok(false);
        };
        if snapshot.unrecoverable_records > 0 {
            let error = std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "{} unrecoverable ledger record(s) in retained history",
                    snapshot.unrecoverable_records
                ),
            );
            return Err(self.fail_session_recovery(storage_id, &session_dir, error));
        }
        if snapshot.retained_entries.is_empty() && snapshot.head_seq == 0 {
            return Ok(false);
        }
        self.clear_read_failure(storage_id);
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
            check_session_available(storage_id, session)?;
            self.check_recovery_latch(storage_id)?;
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
            let swept = self.reconcile_orphaned_rows(storage_id)?;
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
        Ok(true)
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
    fn reconcile_orphaned_rows(&self, session_id: &SessionKey) -> Result<usize, LedgerError> {
        let events: Vec<LedgeredUiProtocolEvent> = {
            let inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
            let Some(session) = inner.sessions.get(session_id) else {
                return Ok(0);
            };
            session
                .entries
                .iter()
                .map(|entry| LedgeredUiProtocolEvent {
                    cursor: UiCursor {
                        stream: session_id.0.clone(),
                        seq: entry.seq,
                    },
                    event: entry.event.clone(),
                    recorded_at: entry.recorded_at,
                    from_connection: None,
                    external_prompt: entry.external_prompt,
                })
                .collect()
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
                    // A start opens a new generation even when an earlier
                    // generation reused the same wire turn_id. Only a terminal
                    // after this start may close it; otherwise restart must
                    // orphan the new generation instead of inheriting the old
                    // terminal time/state.
                    terminal_turns.remove(&turn.turn_id.0.to_string());
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
                )?;
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
            )?;
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
            )?;
            swept += 1;
        }
        Ok(swept)
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
        match validate_session_directory(session_dir, session_id) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        }
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
        let active_log_bytes = fs::metadata(&active_log_path)?.len();
        let mut total_disk_bytes = 0u64;
        for path in &log_files {
            total_disk_bytes = total_disk_bytes.saturating_add(fs::metadata(path)?.len());
        }

        let mut oldest_seq = None;
        let mut head_seq = 0u64;
        let mut retained_entries = VecDeque::new();
        let mut external_prompts = std::collections::HashSet::new();
        let mut replay_entries = Vec::new();
        let mut skipped_records = 0u64;
        let mut unrecoverable_records = 0u64;
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
                    // Seed the external-prompt set in seq order; the log
                    // scan below re-applies the same records (and any whose
                    // log file rotated away stay covered by this).
                    track_external_prompt(
                        &mut external_prompts,
                        &record.event,
                        record.external_prompt,
                    );
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
                                recorded_at: record.recorded_at,
                                from_connection: None,
                                external_prompt: record.external_prompt,
                            });
                        }
                        let bytes = approx_event_bytes(&record.event);
                        retained_entries.push_back(LedgerEntry {
                            seq: record.seq,
                            event: record.event,
                            recorded_at: record.recorded_at,
                            bytes,
                            external_prompt: record.external_prompt,
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
                let line = line_result?;
                if line.trim().is_empty() {
                    continue;
                }
                // Parse every record strictly before skipping materialization
                // below the projection head; malformed prefix rows must not
                // hide behind the snapshot fast path.
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
                track_external_prompt(&mut external_prompts, &record.event, record.external_prompt);

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
                        recorded_at: record.recorded_at,
                        from_connection: None,
                        external_prompt: record.external_prompt,
                    });
                }

                let bytes = approx_event_bytes(&record.event);
                retained_entries.push_back(LedgerEntry {
                    seq: record.seq,
                    event: record.event,
                    recorded_at: record.recorded_at,
                    bytes,
                    external_prompt: record.external_prompt,
                });
                while retained_entries.len() > cap {
                    retained_entries.pop_front();
                }
            }
            let skipped_unrecoverable = skipped_unknown_version + skipped_malformed;
            unrecoverable_records = unrecoverable_records.saturating_add(skipped_unrecoverable);
            let skipped_total = skipped_unrecoverable + skipped_legacy_message_persisted;
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

        let snapshot = DiskSessionSnapshot {
            active_log_path,
            active_log_bytes,
            total_disk_bytes,
            oldest_seq,
            head_seq,
            retained_entries,
            replay_entries,
            external_prompts,
            unrecoverable_records,
            skipped_records,
        };
        if snapshot.skipped_records > 0 {
            warn!(
                target = "octos::ledger",
                session_id = %session_id.0,
                path = %session_dir.display(),
                skipped_records = snapshot.skipped_records,
                "skipped ledger records during session scan (all files)"
            );
        }
        Ok(Some(snapshot))
    }

    /// No terminal notification follows an answered question. Its requester
    /// releases the marker only after receiving answers, never on cancellation:
    /// a competing cancellation emitter may still need the marker for its append.
    pub(crate) fn forget_answered_external_question(
        &self,
        session_id: &SessionKey,
        question_id: &octos_core::ui_protocol::QuestionId,
    ) {
        let storage_id = self.storage_session_id(session_id);
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(session) = inner.sessions.get_mut(&storage_id) {
            session.external_prompts.remove(&question_id.0.to_string());
        }
    }

    pub(crate) fn ensure_session_available(
        &self,
        session_id: &SessionKey,
    ) -> Result<(), LedgerError> {
        let storage_id = self.storage_session_id(session_id);
        self.snapshot_if_session_absent(&storage_id).map(|_| ())
    }

    pub(crate) fn try_append_notification(
        &self,
        notification: UiNotification,
    ) -> Result<LedgeredUiProtocolEvent, LedgerError> {
        self.append(UiProtocolLedgerEvent::Notification(notification), None)
    }

    /// Test fixture shortcut for an append expected to succeed. Production
    /// callers must use `try_append_notification` and handle the error.
    #[cfg(test)]
    pub(crate) fn append_notification(
        &self,
        notification: UiNotification,
    ) -> LedgeredUiProtocolEvent {
        self.try_append_notification(notification)
            .expect("test ledger append must succeed")
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub(crate) fn append_progress(&self, event: UiProgressEvent) -> LedgeredUiProtocolEvent {
        self.append(UiProtocolLedgerEvent::Progress(event), None)
            .expect("test ledger progress append must succeed")
    }

    /// Like [`try_append_notification`] but tags the broadcast event with the
    /// originating connection so that connection's live forwarder can skip
    /// it (the handler already direct-sent the wire frame). Other
    /// connections still receive it via fan-out.
    pub(crate) fn try_append_notification_from(
        &self,
        notification: UiNotification,
        from_connection: ConnectionId,
    ) -> Result<LedgeredUiProtocolEvent, LedgerError> {
        self.append(
            UiProtocolLedgerEvent::Notification(notification),
            Some(from_connection),
        )
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub(crate) fn append_notification_from(
        &self,
        notification: UiNotification,
        from_connection: ConnectionId,
    ) -> LedgeredUiProtocolEvent {
        self.try_append_notification_from(notification, from_connection)
            .expect("test ledger append must succeed")
    }

    /// Progress counterpart of [`try_append_notification_from`].
    pub(crate) fn try_append_progress_from(
        &self,
        event: UiProgressEvent,
        from_connection: ConnectionId,
    ) -> Result<LedgeredUiProtocolEvent, LedgerError> {
        self.append(
            UiProtocolLedgerEvent::Progress(event),
            Some(from_connection),
        )
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub(crate) fn append_progress_from(
        &self,
        event: UiProgressEvent,
        from_connection: ConnectionId,
    ) -> LedgeredUiProtocolEvent {
        self.try_append_progress_from(event, from_connection)
            .expect("test ledger progress append must succeed")
    }

    /// Emit one canonical v2 projection envelope under the same per-thread
    /// sequence and terminal-barrier discipline. The cursor is assigned by the
    /// durable append path, never by a caller, so live delivery and replay
    /// observe the identical envelope.
    pub(crate) fn try_emit_envelope_v2(
        &self,
        session_id: &SessionKey,
        thread_id: String,
        payload: PayloadV2,
        client_message_id: Option<String>,
    ) -> Result<Option<LedgeredUiProtocolEvent>, LedgerError> {
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
    ) -> Result<Option<LedgeredUiProtocolEvent>, LedgerError> {
        let session_id_clone = session_id.clone();
        let storage_id = self.storage_session_id(session_id);
        let preload_snapshot = self.snapshot_if_session_absent(&storage_id)?;
        let cursor;
        let stamped;
        let on_disk_delta;
        let ledgered;
        let broadcast_sender;
        let recorded_at = chrono::Utc::now();
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
                Err(ProjectionSeqAllocationError::PostCompletion(kind)) => {
                    drop(inner);
                    metrics::counter!(
                        "octos_projection_post_completion_drop_total",
                        "kind" => kind,
                    )
                    .increment(1);
                    return Ok(None);
                }
                Err(ProjectionSeqAllocationError::Unavailable(error)) => return Err(error),
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

            let append_outcome = self.append_locked(
                &storage_id,
                event,
                None,
                recorded_at,
                preload_snapshot,
                &mut inner,
            )?;
            cursor = append_outcome.cursor;
            stamped = append_outcome.stamped;
            if let Some(identity) = assistant_identity
                && let Some(state) = inner
                    .thread_seq
                    .get_mut(&(storage_id.clone(), thread_id.clone()))
                && update_native_assistant_owner(state, &thread_id, identity)
            {
                state.assistant_owner_seq = Some(seq);
                if let Err(error) =
                    self.persist_thread_watermark_locked(&storage_id, &thread_id, state)
                {
                    warn!(
                        target = "octos::ledger",
                        ?error,
                        "assistant owner watermark lagged; durable replay will reconcile it"
                    );
                }
            }
            on_disk_delta = append_outcome.on_disk_delta;
            ledgered = LedgeredUiProtocolEvent {
                cursor: cursor.clone(),
                event: stamped.clone(),
                recorded_at: Some(recorded_at),
                from_connection: None,
                external_prompt: append_outcome.external_prompt,
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
        Ok(Some(ledgered))
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
        if let Err(error) = self.ensure_session_loaded(&storage_id) {
            // Projection ordinals are an auxiliary, visibly degraded view;
            // durable append/replay/hydrate paths propagate this same error.
            warn!(
                target = "octos::ledger",
                error = %error,
                "session unavailable while deriving projection ordinal; using resident entries only"
            );
        }
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
    ) -> Result<u64, ProjectionSeqAllocationError> {
        let key = (session_id.clone(), thread_id.to_owned());
        let needs_recovery = !inner.thread_seq.contains_key(&key);
        if needs_recovery {
            // Codex #1336 round-2 BLOCKER 3: recovery now consults the
            // persistent per-thread high-watermark before falling back
            // to scanning the in-memory ring. The in-memory ring may
            // be empty if the session was LRU-evicted or the retained
            // window aged out the relevant envelopes; the watermark
            // file preserves max-seq + completion state independently.
            let recovered = self
                .recover_thread_seq_state(session_id, thread_id, inner)
                .map_err(ProjectionSeqAllocationError::Unavailable)?;
            inner.thread_seq.insert(key.clone(), recovered);
        }
        if let Some(reason) = inner.failed_projections.get(&key) {
            return Err(ProjectionSeqAllocationError::Unavailable(LedgerError::new(
                "persist_thread_watermark",
                session_id,
                None,
                std::io::Error::other(reason.clone()),
            )));
        }
        let state = inner
            .thread_seq
            .get_mut(&key)
            .expect("thread_seq entry inserted above");
        // Hard barrier per spec § 14.6.
        if state.completed {
            return Err(ProjectionSeqAllocationError::PostCompletion(
                if is_terminal {
                    "duplicate_completed"
                } else {
                    "post_completion"
                },
            ));
        }
        if state.next_seq == 0 {
            state.next_seq = 1;
        }
        let seq = state.next_seq;
        state.next_seq = state.next_seq.saturating_add(1);
        if is_terminal {
            state.completed = true;
        }
        // Persist the allocation intent before the log. A failure rejects
        // the emit instead of leaving a durable row without its high-watermark.
        if let Err(error) = self.persist_thread_watermark_locked(session_id, thread_id, state) {
            inner.failed_projections.insert(key, error.to_string());
            return Err(ProjectionSeqAllocationError::Unavailable(error));
        }
        Ok(seq)
    }

    /// Cold per-thread recovery merges the persisted allocation watermark with the
    /// authoritative retained durable replay. The larger next_seq and latched
    /// completion win, so a stale watermark cannot reuse a sequence or reopen
    /// a terminal. Missing metadata means a new thread; unreadable metadata or
    /// durable history is an error. Ephemeral ledgers scan their resident ring.
    fn recover_thread_seq_state(
        &self,
        session_id: &SessionKey,
        thread_id: &str,
        inner: &LedgerInner,
    ) -> Result<ThreadSeqState, LedgerError> {
        let mut state = self
            .read_thread_watermark(session_id, thread_id)?
            .unwrap_or_default();
        if self.config.data_dir.is_some() {
            self.reconcile_durable_assistant_owner(session_id, thread_id, &mut state)?;
            return Ok(state);
        }
        let Some(session_state) = inner.sessions.get(session_id) else {
            return Ok(state);
        };
        for entry in &session_state.entries {
            if let UiProtocolLedgerEvent::Notification(UiNotification::EnvelopeV2(ev)) =
                &entry.event
                && ev.envelope.thread_id == thread_id
            {
                if let PayloadV2::AssistantDelta {
                    assistant_segment_id,
                    ..
                }
                | PayloadV2::AssistantPersisted {
                    assistant_segment_id,
                    ..
                } = &ev.envelope.payload
                    && update_native_assistant_owner(
                        &mut state,
                        thread_id,
                        assistant_segment_id.clone(),
                    )
                {
                    state.assistant_owner_seq = Some(ev.envelope.seq);
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
        }
        Ok(state)
    }

    /// Merge durable sequence/completion state and repair an assistant owner
    /// whose acknowledgement lagged its source append. Full retained disk replay
    /// (not the RAM tail) also covers LRU-evicted sessions.
    fn reconcile_durable_assistant_owner(
        &self,
        session: &SessionKey,
        thread: &str,
        state: &mut ThreadSeqState,
    ) -> Result<(), LedgerError> {
        let Some(data_dir) = self.config.data_dir.as_ref() else {
            return Ok(());
        };
        let dir = data_dir
            .join("ui-protocol")
            .join(encode_session_dir_name(session));
        let snapshot = match self.read_session_disk_snapshot(session, &dir, Some(0), false) {
            Ok(Some(snapshot)) => snapshot,
            Ok(None) => return Ok(()),
            Err(error) => {
                return Err(LedgerError::new(
                    "recover_thread_history",
                    session,
                    None,
                    error,
                ));
            }
        };
        for entry in &snapshot.replay_entries {
            if let UiProtocolLedgerEvent::Notification(UiNotification::EnvelopeV2(event)) =
                &entry.event
                && event.envelope.thread_id == thread
            {
                state.next_seq = state.next_seq.max(event.envelope.seq.saturating_add(1));
                state.completed |= matches!(
                    event.envelope.payload,
                    PayloadV2::TurnTerminal { .. } | PayloadV2::BackgroundChildCompleted { .. }
                );
            }
        }
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
        Ok(())
    }

    /// Persist the write-ahead `(session, thread)` allocation intent before
    /// its log append. Allocation callers propagate failure; a later owner-only
    /// refresh may stay best-effort because replay reconciles that field.
    fn persist_thread_watermark_locked(
        &self,
        session_id: &SessionKey,
        thread_id: &str,
        state: &ThreadSeqState,
    ) -> Result<(), LedgerError> {
        let Some(data_dir) = &self.config.data_dir else {
            return Ok(());
        };
        let session_dir = data_dir
            .join("ui-protocol")
            .join(encode_session_dir_name(session_id));
        #[cfg(test)]
        if let Some(error) = injected_watermark_write_error(&session_dir) {
            return Err(LedgerError::new(
                "persist_thread_watermark",
                session_id,
                Some(state.next_seq),
                error,
            ));
        }
        let dir = session_dir.join("threads");
        ensure_session_directory(&session_dir, session_id)
            .and_then(|_| fs::create_dir_all(&dir))
            .map_err(|error| {
                LedgerError::new(
                    "persist_thread_watermark",
                    session_id,
                    Some(state.next_seq),
                    error,
                )
            })?;
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
        let json = serde_json::to_vec(&record).map_err(|error| {
            LedgerError::new(
                "persist_thread_watermark",
                session_id,
                Some(state.next_seq),
                std::io::Error::other(error),
            )
        })?;
        let tmp = dir.join(format!("{safe_name}.json.tmp"));
        fs::write(&tmp, &json)
            .and_then(|_| fs::rename(&tmp, &path))
            .map_err(|error| {
                LedgerError::new(
                    "persist_thread_watermark",
                    session_id,
                    Some(state.next_seq),
                    error,
                )
            })?;
        Ok(())
    }

    /// Read the persisted allocation watermark. `Ok(None)` means genuine absence or
    /// an ephemeral ledger; read, parse, and version failures are typed errors.
    fn read_thread_watermark(
        &self,
        session_id: &SessionKey,
        thread_id: &str,
    ) -> Result<Option<ThreadSeqState>, LedgerError> {
        let Some(data_dir) = self.config.data_dir.as_ref() else {
            return Ok(None);
        };
        let safe_name = encode_thread_file_name(thread_id);
        let session_dir = data_dir
            .join("ui-protocol")
            .join(encode_session_dir_name(session_id));
        if let Err(error) = validate_session_directory(&session_dir, session_id) {
            if error.kind() == std::io::ErrorKind::NotFound {
                return Ok(None);
            }
            return Err(LedgerError::new(
                "read_thread_watermark",
                session_id,
                None,
                error,
            ));
        }
        let path = session_dir
            .join("threads")
            .join(format!("{safe_name}.json"));
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(LedgerError::new(
                    "read_thread_watermark",
                    session_id,
                    None,
                    error,
                ));
            }
        };
        let record = serde_json::from_slice::<ThreadWatermarkRecord>(&bytes).map_err(|error| {
            LedgerError::new(
                "read_thread_watermark",
                session_id,
                None,
                std::io::Error::new(std::io::ErrorKind::InvalidData, error),
            )
        })?;
        if record.v != THREAD_WATERMARK_DISK_VERSION {
            return Err(LedgerError::new(
                "read_thread_watermark",
                session_id,
                None,
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("unsupported thread watermark version {}", record.v),
                ),
            ));
        }
        Ok(Some(ThreadSeqState {
            next_seq: record.next_seq,
            completed: record.completed,
            assistant_segment_id: record.assistant_segment_id,
            assistant_owner_seq: record.assistant_owner_seq,
        }))
    }

    fn append(
        &self,
        event: UiProtocolLedgerEvent,
        from_connection: Option<ConnectionId>,
    ) -> Result<LedgeredUiProtocolEvent, LedgerError> {
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
    ) -> Result<LedgeredUiProtocolEvent, LedgerError> {
        event.stamp_topic_from_session();
        let session_id = event.session_id().clone();
        let preload_snapshot = self.snapshot_if_session_absent(&storage_id)?;
        let cursor;
        let stamped;
        let on_disk_delta;
        let external_prompt;
        let recorded_at = chrono::Utc::now();
        {
            let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
            let outcome = self.append_locked(
                &storage_id,
                event,
                None,
                recorded_at,
                preload_snapshot,
                &mut inner,
            )?;
            cursor = outcome.cursor;
            stamped = outcome.stamped;
            on_disk_delta = outcome.on_disk_delta;
            external_prompt = outcome.external_prompt;
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
            recorded_at: Some(recorded_at),
            from_connection,
            external_prompt,
        };
        self.publish_live(&session_id, &ledgered);
        Ok(ledgered)
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
        recorded_at: chrono::DateTime<chrono::Utc>,
        preload_snapshot: Option<DiskSessionSnapshot>,
        inner: &mut LedgerInner,
    ) -> Result<AppendLockedOutcome, LedgerError> {
        self.check_recovery_latch(session_id)?;
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
            let state = match inner
                .thread_seq
                .get(&(session_id.clone(), thread.clone()))
                .cloned()
            {
                Some(state) => state,
                None => self.recover_thread_seq_state(session_id, &thread, inner)?,
            };
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
        if let Some(reason) = &session.unavailable {
            return Err(LedgerError::new(
                "session_unavailable",
                session_id,
                Some(session.next_seq.saturating_add(1)),
                std::io::Error::other(reason.clone()),
            ));
        }
        let next_seq = session.next_seq.saturating_add(1);
        let cursor = UiCursor {
            stream: session_id.0.clone(),
            seq: next_seq,
        };
        let stamped = event.with_cursor(cursor.clone());
        // UPCR-2026-036 (#2625): durably mark an external client's prompt
        // events. The transport registers the owner before the prompt's
        // first event reaches the ledger; the session's own set covers a
        // later event of the same prompt once that table has evicted it.
        let external_prompt = ledger_event_prompt_id(&stamped).is_some_and(|prompt_id| {
            session.external_prompts.contains(&prompt_id)
                || super::host_managed::external_prompt_owner(&prompt_id).is_some()
        });

        // Write-ahead to disk before signaling the wire or consuming the
        // cursor in memory. A durable error aborts before ring/publish/ACK.
        let on_disk_delta: i64 = if self.config.data_dir.is_some() {
            let (written, reclaimed) = match self.write_record_locked(
                session_id,
                session,
                &stamped,
                external_prompt,
                recorded_at,
                next_seq,
            ) {
                Ok(outcome) => outcome,
                Err(error) => {
                    session.unavailable = Some(error.to_string());
                    warn!(
                        target = "octos::ledger",
                        ?error,
                        session_id = %session_id.0,
                        seq = cursor.seq,
                        "failed to append ledger record to disk; session unavailable until restart"
                    );
                    return Err(LedgerError::new(
                        "append_log",
                        session_id,
                        Some(next_seq),
                        error,
                    ));
                }
            };
            (written as i64) - (reclaimed as i64)
        } else {
            0
        };

        session.next_seq = next_seq;
        session.last_touched_at = Instant::now();
        track_external_prompt(&mut session.external_prompts, &stamped, external_prompt);
        let bytes = approx_event_bytes(&stamped);
        session.in_memory_bytes = session.in_memory_bytes.saturating_add(bytes);
        session.entries.push_back(LedgerEntry {
            seq: cursor.seq,
            event: stamped.clone(),
            recorded_at: Some(recorded_at),
            external_prompt,
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

        Ok(AppendLockedOutcome {
            cursor,
            stamped,
            on_disk_delta,
            external_prompt,
        })
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

    fn snapshot_if_session_absent(
        &self,
        session_id: &SessionKey,
    ) -> Result<Option<DiskSessionSnapshot>, LedgerError> {
        let Some(data_dir) = self.config.data_dir.as_ref() else {
            return Ok(None);
        };
        // Lazy recovery: if this session was indexed at boot, replay it
        // now (first touch) instead of reading a raw snapshot below.
        if self.ensure_session_loaded(session_id)? {
            return Ok(None);
        }
        {
            let inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
            if inner.sessions.contains_key(session_id) {
                return Ok(None);
            }
        }

        let session_dir = data_dir
            .join("ui-protocol")
            .join(encode_session_dir_name(session_id));
        let snapshot = self
            .read_session_disk_snapshot(session_id, &session_dir, None, false)
            .map_err(|error| LedgerError::new("hydrate_before_append", session_id, None, error))?;
        if let Some(snapshot) = &snapshot
            && snapshot.unrecoverable_records > 0
        {
            let error = std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "{} unrecoverable ledger record(s) in retained history",
                    snapshot.unrecoverable_records
                ),
            );
            return Err(self.fail_session_recovery(session_id, &session_dir, error));
        }
        Ok(snapshot)
    }

    /// Returns `(bytes_written, bytes_reclaimed_by_rotation)`. The caller
    /// adjusts `inner.on_disk_bytes` with the net delta.
    fn write_record_locked(
        &self,
        session_id: &SessionKey,
        session: &mut SessionLedger,
        event: &UiProtocolLedgerEvent,
        external_prompt: bool,
        recorded_at: chrono::DateTime<chrono::Utc>,
        candidate_seq: u64,
    ) -> std::io::Result<(u64, u64)> {
        let Some(dir) = &self.config.data_dir else {
            return Ok((0, 0));
        };
        // Open or rotate the active log file.
        let session_dir = dir
            .join("ui-protocol")
            .join(encode_session_dir_name(session_id));
        #[cfg(test)]
        if let Some(error) = injected_ledger_write_error(&session_dir, false) {
            return Err(error);
        }
        ensure_session_directory(&session_dir, session_id)?;
        let mut reclaimed: u64 = 0;
        if session.active_log_path.is_none() {
            let (path, len) = repair_latest_log(session_id, &session_dir)?
                .unwrap_or_else(|| (session_dir.join(new_log_file_name()), 0));
            session.active_log_path = Some(path);
            session.active_log_bytes = len;
        } else if session.pending_rollback.is_none()
            && session.active_log_bytes >= self.config.rotate_bytes
        {
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
            external_prompt,
            recorded_at: Some(recorded_at),
        };
        let cursor_seq = match event {
            UiProtocolLedgerEvent::Notification(notification) => {
                notification_cursor_seq(notification)
            }
            UiProtocolLedgerEvent::Progress(_) => None,
        }
        .unwrap_or(candidate_seq);

        let to_write = LedgerDiskRecord {
            v: record.v,
            seq: cursor_seq,
            event: record.event,
            external_prompt: record.external_prompt,
            recorded_at: record.recorded_at,
        };
        let mut line = serde_json::to_vec(&to_write).map_err(std::io::Error::other)?;
        line.push(b'\n');
        let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
        let bytes = append_complete_record(&path, &mut file, &mut session.pending_rollback, &line)?;
        #[cfg(test)]
        if let Some(error) = injected_ledger_write_error(&session_dir, true) {
            return Err(error);
        }
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
        ensure_session_directory(&session_dir, session_id)?;
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
                    external_prompt: entry.external_prompt,
                    recorded_at: entry.recorded_at,
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
        inner
            .thread_seq
            .retain(|(session, _), _| session != &victim);
        if let Some(state) = inner.sessions.remove(&victim) {
            self.preserve_write_failure(&victim, &state);
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
            inner.thread_seq.retain(|(session, _), _| session != &key);
            if let Some(state) = inner.sessions.remove(&key) {
                self.preserve_write_failure(&key, &state);
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
        self.snapshot_with_cursor_filtered(session_id, after, None)
    }

    /// Turn-scoped variant of [`Self::snapshot_with_cursor`]: identical
    /// locking, disk-recovery, and cursor semantics, but the returned events
    /// are only the ones belonging to `turn_id` — exactly the set
    /// `project_turn_from_ledger` consumes — so a `turn/state` poll on a
    /// session with a long ledger no longer deep-clones the whole ring to
    /// project a single turn.
    pub(crate) fn snapshot_events_for_turn(
        &self,
        session_id: &SessionKey,
        turn_id: &TurnId,
    ) -> Result<Vec<LedgeredUiProtocolEvent>, RpcError> {
        let turn_wire_id = turn_id.0.to_string();
        let (events, _) = self.snapshot_with_cursor_filtered(
            session_id,
            None,
            Some(&|event| event_belongs_to_turn(event, turn_id, &turn_wire_id)),
        )?;
        Ok(events)
    }

    fn snapshot_with_cursor_filtered(
        &self,
        session_id: &SessionKey,
        after: Option<&UiCursor>,
        event_filter: Option<&dyn Fn(&UiProtocolLedgerEvent) -> bool>,
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
        self.ensure_session_loaded(session_id)
            .map_err(hydrate_ledger_error)?;

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
        self.check_recovery_latch(session_id)
            .map_err(hydrate_ledger_error)?;
        if let Some(session) = inner.sessions.get(session_id) {
            check_session_available(session_id, session).map_err(hydrate_ledger_error)?;
        }
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
            .filter(|entry| entry.seq > after.seq && event_filter.is_none_or(|f| f(&entry.event)))
            .map(|entry| LedgeredUiProtocolEvent {
                cursor: UiCursor {
                    stream: session_id.0.clone(),
                    seq: entry.seq,
                },
                event: entry.event.clone(),
                recorded_at: entry.recorded_at,
                from_connection: None,
                external_prompt: entry.external_prompt,
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
            Ok(snapshot) => {
                if snapshot
                    .as_ref()
                    .is_some_and(|snapshot| snapshot.unrecoverable_records > 0)
                {
                    let error = std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "unrecoverable ledger records",
                    );
                    return Err(hydrate_ledger_error(self.fail_session_recovery(
                        session_id,
                        &session_dir,
                        error,
                    )));
                }
                Ok(snapshot)
            }
            Err(error) => {
                warn!(
                    target = "octos::ledger",
                    ?error,
                    session_id = %session_id.0,
                    "failed to read retained ledger logs for atomic snapshot"
                );
                Err(hydrate_ledger_error(LedgerError::new(
                    "read_snapshot",
                    session_id,
                    None,
                    error,
                )))
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
        self.ensure_session_loaded(session_id)?;
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
                RpcError::from(LedgerError::new(
                    "replay_log",
                    session_id,
                    Some(after.seq),
                    error,
                ))
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
            recorded_at: entry.recorded_at,
            from_connection: None,
            external_prompt: entry.external_prompt,
        })
        .collect()
}

/// The approval or question id of an event that UPCR-2026-036 restricts to
/// an external prompt's owner (see [`LedgerDiskRecord::external_prompt`]).
pub(crate) fn ledger_event_prompt_id(event: &UiProtocolLedgerEvent) -> Option<String> {
    let UiProtocolLedgerEvent::Notification(notification) = event else {
        return None;
    };
    match notification {
        UiNotification::ApprovalRequested(e) => Some(e.approval_id.0.to_string()),
        UiNotification::ApprovalDecided(e) => Some(e.approval_id.0.to_string()),
        UiNotification::ApprovalCancelled(e) => Some(e.approval_id.0.to_string()),
        UiNotification::ApprovalAutoResolved(e) => Some(e.approval_id.0.to_string()),
        UiNotification::UserQuestionRequested(e) => Some(e.question_id.0.to_string()),
        UiNotification::UserQuestionCancelled(e) => Some(e.question_id.0.to_string()),
        _ => None,
    }
}

/// Apply one record to a session's set of external prompts awaiting a terminal event
/// (see [`SessionLedger::external_prompts`]).
fn track_external_prompt(
    external_prompts: &mut std::collections::HashSet<String>,
    event: &UiProtocolLedgerEvent,
    external_prompt: bool,
) {
    if !external_prompt {
        return;
    }
    let UiProtocolLedgerEvent::Notification(notification) = event else {
        return;
    };
    match notification {
        UiNotification::ApprovalRequested(e) => {
            external_prompts.insert(e.approval_id.0.to_string());
        }
        UiNotification::UserQuestionRequested(e) => {
            external_prompts.insert(e.question_id.0.to_string());
        }
        UiNotification::UserQuestionCancelled(e) => {
            external_prompts.remove(&e.question_id.0.to_string());
        }
        UiNotification::ApprovalDecided(e) => {
            external_prompts.remove(&e.approval_id.0.to_string());
        }
        UiNotification::ApprovalCancelled(e) => {
            external_prompts.remove(&e.approval_id.0.to_string());
        }
        UiNotification::ApprovalAutoResolved(e) => {
            external_prompts.remove(&e.approval_id.0.to_string());
        }
        _ => {}
    }
}

fn hydrate_session_from_snapshot(session: &mut SessionLedger, snapshot: DiskSessionSnapshot) {
    session.next_seq = snapshot.head_seq;
    session.entries.clear();
    session.in_memory_bytes = 0;
    session.last_touched_at = Instant::now();
    session.active_log_path = Some(snapshot.active_log_path);
    session.active_log_bytes = snapshot.active_log_bytes;
    session.external_prompts = snapshot.external_prompts;
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

/// Whether a ledger event belongs to `turn_id` — exactly the set
/// `project_turn_from_ledger` reads (turn lifecycle notifications plus the
/// envelope thread backfill), so a turn-scoped snapshot projects identically
/// to a full one. `turn_wire_id` is `turn_id` pre-rendered to its wire form.
fn event_belongs_to_turn(
    event: &UiProtocolLedgerEvent,
    turn_id: &TurnId,
    turn_wire_id: &str,
) -> bool {
    let UiProtocolLedgerEvent::Notification(notification) = event else {
        return false;
    };
    match notification {
        UiNotification::TurnStarted(e) => e.turn_id == *turn_id,
        UiNotification::TurnCompleted(e) => e.turn_id == *turn_id,
        UiNotification::TurnError(e) => e.turn_id == *turn_id,
        UiNotification::EnvelopeV2(e) => e.envelope.turn_id == turn_wire_id,
        _ => false,
    }
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
        // #1492: user-question cancellation rides the ledger like its
        // approval twin — reconnect replay must show the question was
        // asked and cancelled, not silently drop it.
        UiNotification::UserQuestionCancelled(event) => &event.session_id,
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
        UiNotification::MonitorUpdated(event) => &event.session_id,
        UiNotification::MonitorFired(event) => &event.session_id,
        UiNotification::MonitorExpired(event) => &event.session_id,
        UiNotification::ContextCompactionCompleted(event) => &event.session_id,
        UiNotification::ContextCompactionStarted(event) => &event.session_id,
        UiNotification::ContextNormalizationReported(event) => &event.session_id,
        UiNotification::ContextStateReported(event) => &event.session_id,
        UiNotification::SessionOrchestration(event) => &event.session_id,
        UiNotification::PeerStaged(event) => &event.session_id,
        UiNotification::PeerClosed(event) => &event.session_id,
        // #2019 — the human sink routes on the OWNING session, like everything
        // else here. This is the ledger's routing key, so a wrong answer would
        // land the event on whichever session the client has focused.
        UiNotification::BackgroundActivity(event) => &event.session_id,
        // UPCR-2026-RF506E103 — routed on the RECIPIENT session, whose
        // history holds the record.
        UiNotification::PeerTeamMessageReceived(event) => &event.session_id,
        UiNotification::UserQuestionRequested(event) => &event.session_id,
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
// (`:`, `/`, etc.). Short names retain the legacy reversible hex encoding;
// long names use a bounded hash with verified full identity metadata.

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
        UiNotification::EnvelopeV2(envelope) => match &mut envelope.envelope.payload {
            PayloadV2::ToolStart {
                arguments_preview,
                write_path,
                ..
            } => {
                redact_opt_in_place(arguments_preview);
                redact_opt_in_place(write_path);
            }
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

// Hex is retained for existing short identities. Hash only the filesystem
// component, never the wire/storage key: peer, account and project scopes remain
// distinct. Long names carry an exact identity record for cold discovery.
const SESSION_IDENTITY_FILE: &str = "session-identity.json";
const HASHED_SESSION_PREFIX: &str = "sha256-";
const LEGACY_SESSION_MAX_BYTES: usize = 127;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SessionDirectoryIdentity {
    version: u8,
    session_id: String,
}

fn encode_session_dir_name(session_id: &SessionKey) -> String {
    if session_id.0.len() > LEGACY_SESSION_MAX_BYTES {
        return format!(
            "{HASHED_SESSION_PREFIX}{:x}",
            Sha256::digest(session_id.0.as_bytes())
        );
    }
    let mut out = String::with_capacity(session_id.0.len() * 2);
    for byte in session_id.0.as_bytes() {
        out.push_str(&format!("{:02x}", byte));
    }
    out
}

fn invalid_session_directory() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        "invalid ledger session identity",
    )
}

fn read_directory_identity(dir: &Path) -> std::io::Result<SessionKey> {
    let path = dir.join(SESSION_IDENTITY_FILE);
    let metadata = fs::symlink_metadata(&path)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(invalid_session_directory());
    }
    let record: SessionDirectoryIdentity =
        serde_json::from_reader(fs::File::open(path)?).map_err(|_| invalid_session_directory())?;
    if record.version != 1 || record.session_id.len() <= LEGACY_SESSION_MAX_BYTES {
        return Err(invalid_session_directory());
    }
    let key = SessionKey(record.session_id);
    if dir.file_name().and_then(|n| n.to_str()) != Some(encode_session_dir_name(&key).as_str()) {
        return Err(invalid_session_directory());
    }
    Ok(key)
}

fn validate_session_directory(dir: &Path, key: &SessionKey) -> std::io::Result<()> {
    if key.0.len() <= LEGACY_SESSION_MAX_BYTES {
        return Ok(());
    }
    let metadata = fs::symlink_metadata(dir)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(invalid_session_directory());
    }
    let stored = read_directory_identity(dir).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            // Only an absent directory is an empty ledger. Existing data with
            // missing identity is corruption, never a new caller's namespace.
            invalid_session_directory()
        } else {
            error
        }
    })?;
    if stored != *key {
        return Err(invalid_session_directory());
    }
    Ok(())
}

fn ensure_session_directory(dir: &Path, key: &SessionKey) -> std::io::Result<()> {
    if key.0.len() <= LEGACY_SESSION_MAX_BYTES {
        return fs::create_dir_all(dir);
    }
    fs::create_dir_all(dir.parent().ok_or_else(invalid_session_directory)?)?;
    match fs::create_dir(dir) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let metadata = fs::symlink_metadata(dir)?;
            if !metadata.is_dir() || metadata.file_type().is_symlink() {
                return Err(invalid_session_directory());
            }
            match fs::symlink_metadata(dir.join(SESSION_IDENTITY_FILE)) {
                Ok(_) => return validate_session_directory(dir, key),
                // Recover only a genuinely empty directory from an interrupted
                // first creation. Existing unidentified data cannot be assigned.
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    if fs::read_dir(dir)?.next().is_some() {
                        // A concurrent creator may have published since our read.
                        return validate_session_directory(dir, key);
                    }
                }
                Err(error) => return Err(error),
            }
        }
        Err(error) => return Err(error),
    }
    let record = SessionDirectoryIdentity {
        version: 1,
        session_id: key.0.clone(),
    };
    // NamedTempFile is private (0600 on Unix). No-clobber publish prevents a
    // racing writer from replacing another identity; validate the winner.
    // Stage in the parent so an interrupted write does not leave a temporary
    // file that makes a newly created, otherwise-empty session unrecoverable.
    let mut staged =
        tempfile::NamedTempFile::new_in(dir.parent().ok_or_else(invalid_session_directory)?)?;
    serde_json::to_writer(staged.as_file_mut(), &record).map_err(std::io::Error::other)?;
    staged.as_file_mut().flush()?;
    // The identity must reach stable storage before any log refers to it.
    // This runs once per new long session, not on each ledger append.
    staged.as_file_mut().sync_all()?;
    match staged.persist_noclobber(dir.join(SESSION_IDENTITY_FILE)) {
        Ok(_) => {
            #[cfg(unix)]
            {
                fs::File::open(dir)?.sync_all()?;
                fs::File::open(dir.parent().ok_or_else(invalid_session_directory)?)?.sync_all()?;
            }
            validate_session_directory(dir, key)
        }
        Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
            validate_session_directory(dir, key)
        }
        Err(error) => Err(error.error),
    }
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
        if path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|name| name.starts_with("ledger-") && name.ends_with(".log"))
        {
            // Only read_dir's NotFound means the session directory is absent.
            // A vanished/dangling log is an unavailable history, not an empty one.
            let metadata = fs::metadata(&path).map_err(std::io::Error::other)?;
            if !metadata.is_file() {
                return Err(std::io::Error::other("ledger log is not a file"));
            }
            out.push(path);
        }
    }
    Ok(out)
}

include!("ui_protocol_ledger_recovery.rs");

#[cfg(test)]
mod tests {
    use super::*;

    const LEAKED_OPENAI_KEY: &str = "sk-proj-abcdefghijklmnopqrstuvwxyz0123456789";
    const LEAKED_BEARER_KEY: &str = "sk-live-XXXXXXXXXXXXXXXXXXXXXXXX";

    fn tool_started(session: &SessionKey, turn_id: &TurnId) -> UiNotification {
        UiNotification::ToolStarted(octos_core::ui_protocol::ToolStartedEvent {
            session_id: session.clone(),
            topic: None,
            turn_id: turn_id.clone(),
            tool_call_id: "tc-leak".into(),
            tool_name: "shell".into(),
            arguments: Some(serde_json::json!({
                "env": { "OPENAI_API_KEY": LEAKED_OPENAI_KEY },
                "cmd": format!("curl -H 'Authorization: Bearer {LEAKED_BEARER_KEY}' https://api.example"),
                "path": "src/main.rs",
                "query": "todo",
            })),
        })
    }

    fn session_log_bytes(temp: &Path, session: &SessionKey) -> String {
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
            .collect()
    }

    #[test]
    fn should_redact_tool_started_arguments_before_persist_and_replay() {
        let temp = tempfile::tempdir().unwrap();
        let ledger = UiProtocolLedger::with_config(LedgerConfig::durable(temp.path().into()));
        let session = SessionKey("local:redact-tool-args".into());
        let appended = ledger.append_notification(tool_started(&session, &TurnId::new()));
        let appended_json = serde_json::to_string(&appended.event).unwrap();
        assert!(!appended_json.contains(LEAKED_OPENAI_KEY));
        assert!(!appended_json.contains(LEAKED_BEARER_KEY));

        let on_disk = session_log_bytes(temp.path(), &session);
        assert!(!on_disk.contains(LEAKED_OPENAI_KEY));
        assert!(!on_disk.contains(LEAKED_BEARER_KEY));
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
            panic!("expected tool_started")
        };
        let arguments = replayed.arguments.as_ref().unwrap();
        assert_eq!(arguments["path"], "src/main.rs");
        assert_eq!(arguments["query"], "todo");
        assert_eq!(
            arguments["env"]["OPENAI_API_KEY"],
            octos_core::secret_redaction::REDACTED_PLACEHOLDER
        );
        let command = arguments["cmd"].as_str().unwrap();
        assert!(!command.contains(LEAKED_BEARER_KEY));
        assert!(command.contains("curl") && command.contains("https://api.example"));
        assert!(command.contains(octos_core::secret_redaction::REDACTED_PLACEHOLDER));
    }
}

// ---------- Tests ----------
