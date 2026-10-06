//! Structured harness event ABI and local sink transport.
//!
//! Child tools/workflows write newline-delimited JSON events to the local
//! transport URI exposed through `OCTOS_EVENT_SINK`. The runtime consumes those
//! events and folds them into durable task snapshots.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::AsyncReadExt;
use tokio::task::JoinHandle;
use tracing::warn;

use crate::abi_schema::{
    COST_ATTRIBUTION_SCHEMA_VERSION, HARNESS_ERROR_SCHEMA_VERSION,
    HARNESS_PROGRESS_EVENT_SCHEMA_VERSION, SUB_AGENT_DISPATCH_SCHEMA_VERSION,
    SWARM_DISPATCH_SCHEMA_VERSION, SWARM_REVIEW_DECISION_SCHEMA_VERSION,
};
use crate::harness_errors::HarnessErrorEvent;
use crate::task_supervisor::TaskSupervisor;
use crate::validators::VALIDATOR_RESULT_SCHEMA_VERSION;

pub const HARNESS_EVENT_SCHEMA_V1: &str = "octos.harness.event.v1";
pub const OCTOS_EVENT_SINK_ENV: &str = "OCTOS_EVENT_SINK";
pub const OCTOS_SESSION_ID_ENV: &str = "OCTOS_SESSION_ID";
pub const OCTOS_TASK_ID_ENV: &str = "OCTOS_TASK_ID";
pub const OCTOS_HARNESS_SESSION_ID_ENV: &str = "OCTOS_HARNESS_SESSION_ID";
pub const OCTOS_HARNESS_TASK_ID_ENV: &str = "OCTOS_HARNESS_TASK_ID";
pub const MAX_HARNESS_EVENT_LINE_BYTES: usize = 16 * 1024;
const MAX_SESSION_ID_BYTES: usize = 256;
const MAX_TASK_ID_BYTES: usize = 128;
/// Maximum byte length the validator accepts for the `workflow` (pipeline id)
/// field on every event variant. A `workflow` over this bound makes
/// [`HarnessEvent::validate`] (and therefore [`write_event_to_sink`]) reject
/// the event — so producers that copy an UNBOUNDED id into `workflow` (the
/// pipeline executor's DOT graph id) MUST truncate it to this cap at the emit
/// site, or the event silently drops. Exposed so the producer references the
/// canonical limit instead of a drifting magic number (Gap 4.2 / Blocker 3).
pub const MAX_WORKFLOW_BYTES: usize = 128;
const MAX_PHASE_BYTES: usize = 64;
const MAX_MESSAGE_BYTES: usize = 2 * 1024;
const MAX_CREDENTIAL_ID_BYTES: usize = 256;

fn default_validator_result_schema_version() -> u32 {
    VALIDATOR_RESULT_SCHEMA_VERSION
}

fn default_sub_agent_dispatch_schema_version() -> u32 {
    SUB_AGENT_DISPATCH_SCHEMA_VERSION
}

fn default_swarm_dispatch_schema_version() -> u32 {
    SWARM_DISPATCH_SCHEMA_VERSION
}

fn default_cost_attribution_schema_version() -> u32 {
    COST_ATTRIBUTION_SCHEMA_VERSION
}

fn default_harness_progress_event_schema_version() -> u32 {
    HARNESS_PROGRESS_EVENT_SCHEMA_VERSION
}

fn default_swarm_review_decision_schema_version() -> u32 {
    SWARM_REVIEW_DECISION_SCHEMA_VERSION
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HarnessEventError(String);

impl std::fmt::Display for HarnessEventError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for HarnessEventError {}

type HarnessResult<T> = std::result::Result<T, HarnessEventError>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HarnessEventSinkContext {
    pub session_id: String,
    pub task_id: String,
}

static SINK_CONTEXTS: OnceLock<Mutex<HashMap<String, HarnessEventSinkContext>>> = OnceLock::new();

fn sink_contexts() -> &'static Mutex<HashMap<String, HarnessEventSinkContext>> {
    SINK_CONTEXTS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Blocker 3 — per-sink-path write locks. The harness-event sink is an
/// append-only NDJSON file written by MULTIPLE concurrent emitters (the spawned
/// heartbeat task + the now-more-frequent parallel/dynamic_parallel node emits,
/// plus child tools). `writeln!` is NOT a single atomic write syscall — it can
/// issue separate `write`s for the body and the newline — so concurrent writers
/// can interleave a partial line and corrupt the NDJSON stream. We serialize
/// each line into ONE buffer (incl. the trailing newline) and write it under a
/// per-path `Mutex` so a whole line is written without interleaving. The lock is
/// keyed by the resolved sink path, so two different sinks never contend.
static SINK_WRITE_LOCKS: OnceLock<Mutex<HashMap<String, Arc<Mutex<()>>>>> = OnceLock::new();

fn sink_write_lock(path: &Path) -> Arc<Mutex<()>> {
    let registry = SINK_WRITE_LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut guard = registry.lock().unwrap_or_else(|e| e.into_inner());
    guard
        .entry(sink_lock_key(path))
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone()
}

/// Append a single, fully-formed NDJSON line (the caller's bytes plus a
/// trailing `\n`) to `path` in ONE `write_all`, serialized against concurrent
/// writers via the per-path [`sink_write_lock`]. Both [`write_event_to_sink`]
/// and [`write_event_line_to_sink`] funnel through here so EVERY sink write is
/// atomic at the whole-line granularity.
fn append_line_atomic(path: &Path, line: &str) -> std::io::Result<()> {
    // Build the entire line (incl. newline) up front so the locked region is a
    // single `write_all` — no formatting work or extra syscalls under the lock.
    let mut buf = String::with_capacity(line.len() + 1);
    buf.push_str(line);
    buf.push('\n');

    let write_lock = sink_write_lock(path);
    let _guard = write_lock.lock().unwrap_or_else(|e| e.into_inner());
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    file.write_all(buf.as_bytes())?;
    file.flush()
}

fn sink_path_from_raw(raw_sink: &str) -> PathBuf {
    if let Some(rest) = raw_sink.strip_prefix("file://") {
        return PathBuf::from(rest.strip_prefix("localhost").unwrap_or(rest));
    }
    PathBuf::from(raw_sink)
}

fn sink_key_from_raw(raw_sink: &str) -> String {
    sink_path_from_raw(raw_sink).display().to_string()
}

/// Key used for the sink-CONTEXT registry (session/task id lookup). This is
/// matched against [`sink_key_from_raw`] (the lookup path), so it MUST stay the
/// plain `display()` form — canonicalizing here would desync registration from
/// lookup. Lock keying uses the separate [`sink_lock_key`] (Blocker 4).
fn sink_key(path: &Path) -> String {
    path.display().to_string()
}

/// Blocker 4 — derive the per-path write-LOCK key from the CANONICAL path so
/// two lexically-different spellings of the same file (`./x` vs `/abs/x`, a
/// symlink vs its target, `a/../b` vs `b`) map to ONE lock and therefore
/// serialize against each other. Without canonicalization the lock is keyed by
/// `display()` and the two spellings get DIFFERENT locks — still racy.
///
/// This is intentionally SEPARATE from [`sink_key`] (the context-registry key,
/// which must stay `display()` to match the lookup path): the lock only needs a
/// stable per-file identity; the context registry needs registration/lookup to
/// agree on the SAME (verbatim) spelling.
///
/// `std::fs::canonicalize` requires the path to EXIST, but a sink file may not
/// exist on the first write. We degrade deterministically so the SAME target
/// always yields the SAME key regardless of which write happens first:
///   1. canonicalize the full path if it already exists;
///   2. else canonicalize the PARENT dir (usually exists) and re-join the file
///      name — this collapses symlinked/relative parents the same way for the
///      first and all subsequent writes;
///   3. else CWD-join to absolutize a relative spelling (no FS access);
///   4. else the raw `display()` string.
fn sink_lock_key(path: &Path) -> String {
    if let Ok(canon) = std::fs::canonicalize(path) {
        return canon.display().to_string();
    }
    if let (Some(parent), Some(file_name)) = (path.parent(), path.file_name()) {
        // An empty parent ("x" with no dir component) canonicalizes to CWD; that
        // is still consistent for every spelling that omits a parent, so use it.
        if let Ok(canon_parent) = std::fs::canonicalize(parent) {
            return canon_parent.join(file_name).display().to_string();
        }
    }
    // No FS access succeeded — CWD-join to absolutize without touching disk so a
    // relative spelling still maps to the same key as its absolute form when the
    // CWD is stable.
    if path.is_relative() {
        if let Ok(cwd) = std::env::current_dir() {
            return cwd.join(path).display().to_string();
        }
    }
    path.display().to_string()
}

fn register_sink_context(sink: String, context: HarnessEventSinkContext) {
    let mut contexts = sink_contexts().lock().unwrap_or_else(|e| e.into_inner());
    contexts.insert(sink, context);
}

/// Test-only wrapper around [`register_sink_context`] for crates that need
/// to assert against a sink without booting a full [`HarnessEventSink`].
#[doc(hidden)]
pub fn attach_event_sink_context(sink: String, context: HarnessEventSinkContext) {
    register_sink_context(sink, context);
}

/// Test-only wrapper around `unregister_sink_context`.
#[doc(hidden)]
pub fn detach_event_sink_context(sink: &str) {
    unregister_sink_context(sink);
}

fn unregister_sink_context(sink: &str) {
    let mut contexts = sink_contexts().lock().unwrap_or_else(|e| e.into_inner());
    contexts.remove(sink);
}

pub fn lookup_event_sink_context(raw_sink: impl AsRef<str>) -> Option<HarnessEventSinkContext> {
    let raw_sink = raw_sink.as_ref();
    let contexts = sink_contexts().lock().unwrap_or_else(|e| e.into_inner());
    contexts
        .get(raw_sink)
        .cloned()
        .or_else(|| contexts.get(&sink_key_from_raw(raw_sink)).cloned())
}

pub fn write_event_to_sink(raw_sink: impl AsRef<str>, event: &HarnessEvent) -> std::io::Result<()> {
    event
        .validate()
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?;
    let path = sink_path_from_raw(raw_sink.as_ref());
    let json = serde_json::to_string(event)
        .map_err(|error| std::io::Error::other(format!("serialize harness event: {error}")))?;
    // Blocker 3 — one whole line, one `write_all`, under the per-path lock so
    // concurrent emitters (heartbeat + parallel node emits) cannot interleave.
    append_line_atomic(&path, &json)
}

/// Append a pre-serialized event line to a sink without round-tripping
/// through the [`HarnessEvent`] validator.
///
/// Used by the plugin protocol-v2 shim, which builds events from the wire
/// format on a hot reader path. Callers MUST pass a single well-formed
/// JSON object; the writer adds a trailing newline.
pub fn write_event_line_to_sink(raw_sink: impl AsRef<str>, line: &str) -> std::io::Result<()> {
    let path = sink_path_from_raw(raw_sink.as_ref());
    // Blocker 3 — same atomic whole-line append as `write_event_to_sink`.
    append_line_atomic(&path, line)
}

pub fn emit_registered_progress_event(
    raw_sink: impl AsRef<str>,
    workflow: Option<&str>,
    phase: &str,
    message: &str,
    progress: Option<f64>,
) -> bool {
    let raw_sink = raw_sink.as_ref();
    let Some(context) = lookup_event_sink_context(raw_sink) else {
        return false;
    };
    let event = HarnessEvent::progress(
        context.session_id,
        context.task_id,
        workflow.map(ToOwned::to_owned),
        phase.to_string(),
        Some(message.to_string()),
        progress,
    );
    write_event_to_sink(raw_sink, &event).is_ok()
}

/// Emit a `Progress` event carrying additive structured `extra` fields to a
/// registered sink (Gap 4.2). Same lookup/write path as
/// [`emit_registered_progress_event`] but threads the structured
/// node/eta/preview map through [`HarnessEvent::progress_with_extra`].
/// Returns `true` when the sink accepted the write.
pub fn emit_registered_progress_event_with_extra(
    raw_sink: impl AsRef<str>,
    workflow: Option<&str>,
    phase: &str,
    message: &str,
    progress: Option<f64>,
    extra: HashMap<String, Value>,
) -> bool {
    let raw_sink = raw_sink.as_ref();
    let Some(context) = lookup_event_sink_context(raw_sink) else {
        return false;
    };
    let event = HarnessEvent::progress_with_extra(
        context.session_id,
        context.task_id,
        workflow.map(ToOwned::to_owned),
        phase.to_string(),
        Some(message.to_string()),
        progress,
        extra,
    );
    write_event_to_sink(raw_sink, &event).is_ok()
}

/// Emit a credential rotation event to a registered sink (M6.5). Returns
/// `true` when the sink accepted the write. Used by the harness-layer sink
/// adapter that forwards `octos_llm::CredentialRotationEvent` into the
/// structured event stream.
pub fn emit_registered_credential_rotation_event(
    raw_sink: impl AsRef<str>,
    credential_id: &str,
    reason: &str,
    strategy: &str,
) -> bool {
    let raw_sink = raw_sink.as_ref();
    let Some(context) = lookup_event_sink_context(raw_sink) else {
        return false;
    };
    let event = HarnessEvent::credential_rotation(
        context.session_id,
        context.task_id,
        credential_id,
        reason,
        strategy,
    );
    write_event_to_sink(raw_sink, &event).is_ok()
}

/// Sink adapter that forwards octos-llm credential rotation events to a
/// registered harness event sink identified by `raw_sink`. Implementations
/// typically create one of these per task when a pool is attached.
pub struct HarnessCredentialRotationSink {
    raw_sink: String,
}

impl HarnessCredentialRotationSink {
    pub fn new(raw_sink: impl Into<String>) -> Self {
        Self {
            raw_sink: raw_sink.into(),
        }
    }
}

impl octos_llm::RotationEventSink for HarnessCredentialRotationSink {
    fn emit(&self, event: &octos_llm::CredentialRotationEvent) {
        let _ = emit_registered_credential_rotation_event(
            &self.raw_sink,
            &event.credential_id,
            &event.reason,
            &event.strategy,
        );
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HarnessEvent {
    pub schema: String,
    #[serde(flatten)]
    pub payload: HarnessEventPayload,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HarnessEventPayload {
    Progress {
        #[serde(flatten)]
        data: HarnessProgressEvent,
    },
    Phase {
        #[serde(flatten)]
        data: HarnessPhaseEvent,
    },
    Artifact {
        #[serde(flatten)]
        data: HarnessArtifactEvent,
    },
    ValidatorResult {
        #[serde(flatten)]
        data: HarnessValidatorResultEvent,
    },
    Retry {
        #[serde(flatten)]
        data: HarnessRetryEvent,
    },
    Failure {
        #[serde(flatten)]
        data: HarnessFailureEvent,
    },
    /// Outer orchestrator invoked a session-level MCP tool exposed by `octos mcp-serve`.
    ///
    /// Emitted once per `tools/call` dispatch (stdio or http). The `outcome`
    /// field is one of `ready`, `failed`, `queued`, `running`, or `verifying`,
    /// matching [`TaskLifecycleState`](crate::task_supervisor::TaskLifecycleState).
    McpServerCall {
        #[serde(flatten)]
        data: HarnessMcpServerCallEvent,
    },
    SubAgentDispatch {
        #[serde(flatten)]
        data: HarnessSubAgentDispatchEvent,
    },
    SwarmDispatch {
        #[serde(flatten)]
        data: HarnessSwarmDispatchEvent,
    },
    /// Supervisor review outcome for a completed swarm dispatch (M7.6).
    ///
    /// Emitted when the contract-authoring dashboard's review gate
    /// accepts or rejects a finalized `SwarmResult`. Typed so the
    /// provenance ledger, archive, and Matrix audit stream all see the
    /// same decision without re-parsing free-form JSON.
    SwarmReviewDecision {
        #[serde(flatten)]
        data: HarnessSwarmReviewDecisionEvent,
    },
    CostAttribution {
        #[serde(flatten)]
        data: HarnessCostAttributionEvent,
    },
    /// Content-classified smart routing decision (M6.6).
    ///
    /// Emitted once per chat turn, before the adaptive router picks a lane.
    /// Contract: `octos.harness.event.v1 { kind: "routing.decision", tier, reasons }`.
    #[serde(rename = "routing.decision")]
    RoutingDecision {
        #[serde(flatten)]
        data: HarnessRoutingDecisionEvent,
    },
    CredentialRotation {
        #[serde(flatten)]
        data: HarnessCredentialRotationEvent,
    },
    /// Emitted once per session load after [`octos_bus::ResumePolicy`] runs
    /// (M8.6). Carries a typed report so operators can see what the
    /// sanitizer dropped and whether the worktree (if any) was still
    /// present on disk.
    SessionSanitized {
        #[serde(flatten)]
        data: HarnessSessionSanitizedEvent,
    },
    /// Periodic progress summary emitted by the `AgentSummaryGenerator`
    /// (M8.7). Produced every `tick` seconds while a spawn_only sub-agent
    /// is running, backed by a cheap-lane LLM call over the last N
    /// activities. The supervisor folds these into
    /// `BackgroundTask.runtime_detail`.
    SubagentProgress {
        #[serde(flatten)]
        data: HarnessSubagentProgressEvent,
    },
    Error {
        #[serde(flatten)]
        data: HarnessErrorEvent,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HarnessProgressEvent {
    #[serde(default = "default_harness_progress_event_schema_version")]
    pub schema_version: u32,
    pub session_id: String,
    pub task_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow: Option<String>,
    pub phase: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(
        default,
        alias = "progress_fraction",
        skip_serializing_if = "Option::is_none"
    )]
    pub progress: Option<f64>,
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HarnessPhaseEvent {
    pub session_id: String,
    pub task_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow: Option<String>,
    pub phase: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HarnessArtifactEvent {
    pub session_id: String,
    pub task_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HarnessValidatorResultEvent {
    #[serde(default = "default_validator_result_schema_version")]
    pub schema_version: u32,
    pub session_id: String,
    pub task_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    pub validator: String,
    pub passed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HarnessRetryEvent {
    pub session_id: String,
    pub task_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HarnessFailureEvent {
    pub session_id: String,
    pub task_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retryable: Option<bool>,
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}

/// Typed payload emitted when the `octos-swarm` primitive dispatches a
/// batch of contracts to MCP-backed sub-agents. Supervisors consume
/// these events to render live swarm state and drive re-dispatch on
/// partial failure.
///
/// The schema is versioned so downstream tooling can reject unknown
/// variants instead of silently dropping fields.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HarnessSwarmDispatchEvent {
    #[serde(default = "default_swarm_dispatch_schema_version")]
    pub schema_version: u32,
    pub session_id: String,
    pub task_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    /// Stable dispatch identifier — persists across process restart so
    /// the primitive can reload state and resume.
    pub dispatch_id: String,
    /// Topology label: `"parallel"` / `"sequential"` / `"pipeline"` /
    /// `"fanout"`. Stable metric cardinality.
    pub topology: String,
    /// Aggregate outcome label: `"success"` / `"partial"` / `"failed"` /
    /// `"aborted"`.
    pub outcome: String,
    /// Number of sub-contracts issued at dispatch time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_subtasks: Option<u32>,
    /// How many of them reached a successful terminal state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_subtasks: Option<u32>,
    /// Retry round index (0 = first round). Bounded by the primitive's
    /// MAX_RETRY_ROUNDS constant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_round: Option<u32>,
    /// Optional human-readable error message for non-success outcomes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}

/// Supervisor decision event emitted by the M7.6 review gate. Captures
/// whether the reviewer accepted or rejected a finalized swarm dispatch,
/// who reviewed it, and optional freeform notes. Downstream tooling
/// (Matrix audit, ledger archive, operator summary) reads the typed
/// variant so the decision never round-trips through stringly-typed JSON.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HarnessSwarmReviewDecisionEvent {
    #[serde(default = "default_swarm_review_decision_schema_version")]
    pub schema_version: u32,
    pub session_id: String,
    pub task_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    /// Dispatch id the review applies to. Matches the
    /// [`HarnessSwarmDispatchEvent::dispatch_id`] the primitive emits
    /// when the swarm finalises.
    pub dispatch_id: String,
    /// `true` when the reviewer accepted the aggregate artifact. `false`
    /// routes the dispatch back to the supervisor for re-contract or
    /// abandonment.
    pub accepted: bool,
    /// Stable identifier of the reviewer (user id, email, Matrix handle).
    /// Kept short enough to fit the session_id bound.
    pub reviewer: String,
    /// Optional free-form notes. Bounded to
    /// [`MAX_MESSAGE_BYTES`](crate::harness_events::MAX_HARNESS_EVENT_LINE_BYTES)
    /// so a single review cannot blow the event line limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}

/// One MCP-server-mode `tools/call` dispatch — emitted by `octos mcp-serve` so
/// outer orchestrators appear in the same harness audit log as local tool calls.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HarnessMcpServerCallEvent {
    pub session_id: String,
    pub task_id: String,
    /// The MCP tool name (currently always `run_octos_session`).
    pub tool: String,
    /// Opaque identifier for the caller. For stdio this is the parent process
    /// label; for HTTP it is the bearer-token fingerprint (never the raw token).
    pub caller_id: String,
    /// Transport that received this call: `stdio` or `http`.
    pub transport: String,
    /// Coarse lifecycle outcome: `ready`, `failed`, `queued`, `running`, or
    /// `verifying`. Matches [`TaskLifecycleState`](crate::task_supervisor::TaskLifecycleState).
    pub outcome: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contract: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}

/// Typed payload emitted when the harness dispatches a task to an
/// MCP-backed sub-agent. The schema is versioned so downstream tooling
/// can reject unknown variants instead of silently dropping fields.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HarnessSubAgentDispatchEvent {
    #[serde(default = "default_sub_agent_dispatch_schema_version")]
    pub schema_version: u32,
    pub session_id: String,
    pub task_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    /// Stable backend label: `"local"` (stdio subprocess) or `"remote"`
    /// (HTTPS).
    pub backend: String,
    /// Human-readable endpoint identifier (command or URL).
    pub endpoint: String,
    /// Outcome label from [`crate::tools::mcp_agent::DispatchOutcome`].
    pub outcome: String,
    /// Optional error text for non-success outcomes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}

/// Typed payload emitted when a sub-agent dispatch commits a cost
/// attribution to the ledger (M7.4). Fired after the dispatch succeeds
/// so operators can tie spend back to the originating contract, task,
/// and model without joining against raw dispatch logs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HarnessCostAttributionEvent {
    #[serde(default = "default_cost_attribution_schema_version")]
    pub schema_version: u32,
    pub session_id: String,
    pub task_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    /// Stable ledger row id — matches
    /// [`crate::cost_ledger::CostAttributionEvent::attribution_id`].
    pub attribution_id: String,
    /// Contract identifier the spend is booked against (workspace
    /// contract path, workflow slug, or an opaque operator-chosen id).
    pub contract_id: String,
    /// Model key declared by the sub-agent.
    pub model: String,
    pub tokens_in: u32,
    pub tokens_out: u32,
    pub cost_usd: f64,
    /// Dispatch outcome echoed from the originating
    /// [`HarnessSubAgentDispatchEvent::outcome`].
    pub outcome: String,
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}

/// Content-classified smart routing decision payload (M6.6).
///
/// Emitted once per chat turn with the classifier's tier choice and the
/// reasons that drove it. Useful for dashboards, A/B evaluation, and
/// debugging mis-classification.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HarnessRoutingDecisionEvent {
    pub session_id: String,
    pub task_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    /// Lowercase tier label: `"cheap"` or `"strong"`.
    pub tier: String,
    /// Optional lane hint (set by M6.5 credential-pool-aware selection).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lane: Option<String>,
    /// Ordered reasons (`"code_fence"`, `"keyword:debug"`, ...).
    #[serde(default)]
    pub reasons: Vec<String>,
    /// Classified input length in chars.
    #[serde(default)]
    pub input_chars: usize,
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}

/// Typed payload emitted when [`octos_bus::ResumePolicy`] sanitizes a
/// session transcript on load (M8.6).
///
/// The report fields mirror [`octos_bus::SessionSanitizeReport`] one-for-
/// one so operators can build dashboards without joining against a raw
/// log. `worktree_missing` is a hard signal that the sub-agent's git
/// worktree was cleaned up externally (Claude Code issue #22355) — the
/// caller should refuse to resume and start a fresh session instead.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HarnessSessionSanitizedEvent {
    pub session_id: String,
    pub task_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow: Option<String>,
    /// Messages loaded from JSONL before any filter ran.
    pub input_len: usize,
    /// Messages remaining after all 4 filter passes.
    pub output_len: usize,
    /// Tool-call assistant messages whose ids lacked matching results and
    /// were not pinned by retry state.
    #[serde(default)]
    pub unresolved_tool_uses_dropped: usize,
    /// Assistant messages with reasoning but no content or tool calls
    /// (non-tail only).
    #[serde(default)]
    pub orphan_thinking_dropped: usize,
    /// Assistant messages with whitespace-only content.
    #[serde(default)]
    pub whitespace_only_dropped: usize,
    /// Count of [`octos_bus::ReplacementStateRef`] entries recovered.
    #[serde(default)]
    pub content_replacements_restored: usize,
    /// `true` when `workspace_root` was provided and missing on disk.
    #[serde(default)]
    pub worktree_missing: bool,
    /// Non-fatal diagnostics from the policy. Order-preserving.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}

/// Periodic sub-agent progress summary event (M8.7).
///
/// Emitted every `tick` seconds by `AgentSummaryGenerator` while a
/// spawn_only sub-agent is in `Running` status. Supervisors fold the
/// `summary` string into `BackgroundTask.runtime_detail` so dashboards
/// can display live "what is the sub-agent doing" text without tailing
/// the per-task disk output log.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HarnessSubagentProgressEvent {
    pub session_id: String,
    pub task_id: String,
    /// Short LLM-generated description of the current activity (3-5 words,
    /// present continuous tense).
    pub summary: String,
    /// Monotonic tick sequence starting at 1 for the first summary of the
    /// task. Useful for clients that want to deduplicate or show "tick N".
    pub tick_seq: u32,
    /// When the summary was produced (wall-clock, UTC).
    pub at: chrono::DateTime<chrono::Utc>,
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}

/// Structured credential rotation event (M6.5).
///
/// Emitted by the credential pool on every successful selection. Consumers
/// can tie the event to a Prometheus counter
/// (`octos_llm_credential_rotation_total{reason, strategy}`) for parity.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HarnessCredentialRotationEvent {
    pub session_id: String,
    pub task_id: String,
    /// Stable identifier of the credential that was selected.
    pub credential_id: String,
    /// Stable reason label (e.g. `initial_acquire`, `rate_limit_cooldown`,
    /// `auth_failure`, `manual_release`).
    pub reason: String,
    /// Strategy label (`fill_first`, `round_robin`, `random`, `least_used`).
    pub strategy: String,
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}

impl HarnessEvent {
    pub fn progress(
        session_id: impl Into<String>,
        task_id: impl Into<String>,
        workflow: Option<impl Into<String>>,
        phase: impl Into<String>,
        message: Option<impl Into<String>>,
        progress: Option<f64>,
    ) -> Self {
        Self {
            schema: HARNESS_EVENT_SCHEMA_V1.to_string(),
            payload: HarnessEventPayload::Progress {
                data: HarnessProgressEvent {
                    schema_version: HARNESS_PROGRESS_EVENT_SCHEMA_VERSION,
                    session_id: session_id.into(),
                    task_id: task_id.into(),
                    workflow: workflow.map(Into::into),
                    phase: phase.into(),
                    message: message.map(Into::into),
                    progress,
                    extra: HashMap::new(),
                },
            },
        }
    }

    /// Build a `Progress` event carrying additive structured fields in the
    /// flattened `extra` map (Gap 4.2). The canonical `phase`/`message`/
    /// `progress` keep working for consumers that ignore `extra`; producers
    /// (e.g. the pipeline executor) attach structured per-node fields —
    /// `node`, `node_index`, `node_total`, `eta_secs`, `preview` — so the
    /// SPA/TUI can render real per-node progress instead of an opaque chip.
    ///
    /// `extra` is purely additive on the v1 wire (a HashMap flatten that
    /// round-trips); no schema-version bump is needed and consumers that
    /// don't read the keys are unaffected.
    pub fn progress_with_extra(
        session_id: impl Into<String>,
        task_id: impl Into<String>,
        workflow: Option<impl Into<String>>,
        phase: impl Into<String>,
        message: Option<impl Into<String>>,
        progress: Option<f64>,
        extra: HashMap<String, Value>,
    ) -> Self {
        Self {
            schema: HARNESS_EVENT_SCHEMA_V1.to_string(),
            payload: HarnessEventPayload::Progress {
                data: HarnessProgressEvent {
                    schema_version: HARNESS_PROGRESS_EVENT_SCHEMA_VERSION,
                    session_id: session_id.into(),
                    task_id: task_id.into(),
                    workflow: workflow.map(Into::into),
                    phase: phase.into(),
                    message: message.map(Into::into),
                    progress,
                    extra,
                },
            },
        }
    }

    /// Convenience builder for a `SubAgentDispatch` event. Takes a
    /// pre-populated [`HarnessSubAgentDispatchEvent`] so callers pay
    /// the construction cost once and this helper stays below clippy's
    /// argument limit.
    pub fn sub_agent_dispatch(data: HarnessSubAgentDispatchEvent) -> Self {
        Self {
            schema: HARNESS_EVENT_SCHEMA_V1.to_string(),
            payload: HarnessEventPayload::SubAgentDispatch { data },
        }
    }

    /// Convenience builder for a `SwarmDispatch` event. Takes a
    /// pre-populated [`HarnessSwarmDispatchEvent`] so callers pay the
    /// construction cost once and this helper stays below clippy's
    /// argument limit.
    pub fn swarm_dispatch(data: HarnessSwarmDispatchEvent) -> Self {
        Self {
            schema: HARNESS_EVENT_SCHEMA_V1.to_string(),
            payload: HarnessEventPayload::SwarmDispatch { data },
        }
    }

    /// Convenience builder for a `SwarmReviewDecision` event (M7.6).
    pub fn swarm_review_decision(data: HarnessSwarmReviewDecisionEvent) -> Self {
        Self {
            schema: HARNESS_EVENT_SCHEMA_V1.to_string(),
            payload: HarnessEventPayload::SwarmReviewDecision { data },
        }
    }

    /// Convenience builder for a `CostAttribution` event.
    pub fn cost_attribution(data: HarnessCostAttributionEvent) -> Self {
        Self {
            schema: HARNESS_EVENT_SCHEMA_V1.to_string(),
            payload: HarnessEventPayload::CostAttribution { data },
        }
    }

    pub fn phase_event(
        session_id: impl Into<String>,
        task_id: impl Into<String>,
        workflow: Option<impl Into<String>>,
        phase: impl Into<String>,
        message: Option<impl Into<String>>,
    ) -> Self {
        Self {
            schema: HARNESS_EVENT_SCHEMA_V1.to_string(),
            payload: HarnessEventPayload::Phase {
                data: HarnessPhaseEvent {
                    session_id: session_id.into(),
                    task_id: task_id.into(),
                    workflow: workflow.map(Into::into),
                    phase: phase.into(),
                    message: message.map(Into::into),
                    extra: HashMap::new(),
                },
            },
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn mcp_server_call(
        session_id: impl Into<String>,
        task_id: impl Into<String>,
        tool: impl Into<String>,
        caller_id: impl Into<String>,
        transport: impl Into<String>,
        outcome: impl Into<String>,
        contract: Option<impl Into<String>>,
        error: Option<impl Into<String>>,
    ) -> Self {
        Self {
            schema: HARNESS_EVENT_SCHEMA_V1.to_string(),
            payload: HarnessEventPayload::McpServerCall {
                data: HarnessMcpServerCallEvent {
                    session_id: session_id.into(),
                    task_id: task_id.into(),
                    tool: tool.into(),
                    caller_id: caller_id.into(),
                    transport: transport.into(),
                    outcome: outcome.into(),
                    contract: contract.map(Into::into),
                    error: error.map(Into::into),
                    extra: HashMap::new(),
                },
            },
        }
    }

    /// Build a `routing.decision` event for the content-classified smart router (M6.6).
    pub fn routing_decision(
        session_id: impl Into<String>,
        task_id: impl Into<String>,
        workflow: Option<impl Into<String>>,
        tier: impl Into<String>,
        reasons: Vec<String>,
        input_chars: usize,
    ) -> Self {
        Self {
            schema: HARNESS_EVENT_SCHEMA_V1.to_string(),
            payload: HarnessEventPayload::RoutingDecision {
                data: HarnessRoutingDecisionEvent {
                    session_id: session_id.into(),
                    task_id: task_id.into(),
                    workflow: workflow.map(Into::into),
                    phase: None,
                    tier: tier.into(),
                    lane: None,
                    reasons,
                    input_chars,
                    extra: HashMap::new(),
                },
            },
        }
    }

    /// Construct a `SessionSanitized` event from a
    /// [`octos_bus::SessionSanitizeReport`] (M8.6). The caller supplies
    /// session_id/task_id/workflow from its runtime context; the rest of
    /// the fields come straight from the report.
    pub fn session_sanitized(
        session_id: impl Into<String>,
        task_id: impl Into<String>,
        workflow: Option<impl Into<String>>,
        report: &octos_bus::SessionSanitizeReport,
    ) -> Self {
        Self {
            schema: HARNESS_EVENT_SCHEMA_V1.to_string(),
            payload: HarnessEventPayload::SessionSanitized {
                data: HarnessSessionSanitizedEvent {
                    session_id: session_id.into(),
                    task_id: task_id.into(),
                    workflow: workflow.map(Into::into),
                    input_len: report.input_len,
                    output_len: report.output_len,
                    unresolved_tool_uses_dropped: report.unresolved_tool_uses_dropped,
                    orphan_thinking_dropped: report.orphan_thinking_dropped,
                    whitespace_only_dropped: report.whitespace_only_dropped,
                    content_replacements_restored: report.content_replacements_restored,
                    worktree_missing: report.worktree_missing,
                    warnings: report.warnings.clone(),
                    extra: HashMap::new(),
                },
            },
        }
    }

    /// Build a `SubagentProgress` event (M8.7). Emitted every tick while a
    /// spawn_only sub-agent is running so operators can see a live
    /// natural-language summary of current activity.
    pub fn subagent_progress(
        session_id: impl Into<String>,
        task_id: impl Into<String>,
        summary: impl Into<String>,
        tick_seq: u32,
        at: chrono::DateTime<chrono::Utc>,
    ) -> Self {
        Self {
            schema: HARNESS_EVENT_SCHEMA_V1.to_string(),
            payload: HarnessEventPayload::SubagentProgress {
                data: HarnessSubagentProgressEvent {
                    session_id: session_id.into(),
                    task_id: task_id.into(),
                    summary: summary.into(),
                    tick_seq,
                    at,
                    extra: HashMap::new(),
                },
            },
        }
    }

    /// Construct a credential rotation event (M6.5).
    pub fn credential_rotation(
        session_id: impl Into<String>,
        task_id: impl Into<String>,
        credential_id: impl Into<String>,
        reason: impl Into<String>,
        strategy: impl Into<String>,
    ) -> Self {
        Self {
            schema: HARNESS_EVENT_SCHEMA_V1.to_string(),
            payload: HarnessEventPayload::CredentialRotation {
                data: HarnessCredentialRotationEvent {
                    session_id: session_id.into(),
                    task_id: task_id.into(),
                    credential_id: credential_id.into(),
                    reason: reason.into(),
                    strategy: strategy.into(),
                    extra: HashMap::new(),
                },
            },
        }
    }

    pub fn from_json_line(line: &str) -> HarnessResult<Self> {
        if line.len() > MAX_HARNESS_EVENT_LINE_BYTES {
            return Err(HarnessEventError(format!(
                "harness event line exceeded {MAX_HARNESS_EVENT_LINE_BYTES} bytes"
            )));
        }

        let event: Self = serde_json::from_str(line)
            .map_err(|error| HarnessEventError(format!("invalid harness event JSON: {error}")))?;
        event.validate()?;
        Ok(event)
    }

    pub fn validate(&self) -> HarnessResult<()> {
        if self.schema != HARNESS_EVENT_SCHEMA_V1 {
            return Err(HarnessEventError(format!(
                "unsupported harness event schema: {}",
                self.schema
            )));
        }

        match &self.payload {
            HarnessEventPayload::Progress { data } => {
                if data.schema_version > HARNESS_PROGRESS_EVENT_SCHEMA_VERSION {
                    return Err(HarnessEventError(format!(
                        "unsupported harness progress schema_version {} (max supported: {})",
                        data.schema_version, HARNESS_PROGRESS_EVENT_SCHEMA_VERSION
                    )));
                }
                validate_common_ids(&data.session_id, &data.task_id)?;
                validate_optional_name("workflow", data.workflow.as_deref(), MAX_WORKFLOW_BYTES)?;
                validate_phase(&data.phase)?;
                validate_optional_message(data.message.as_deref())?;
                validate_progress(data.progress)?;
            }
            HarnessEventPayload::Phase { data } => {
                validate_common_ids(&data.session_id, &data.task_id)?;
                validate_optional_name("workflow", data.workflow.as_deref(), MAX_WORKFLOW_BYTES)?;
                validate_phase(&data.phase)?;
                validate_optional_message(data.message.as_deref())?;
            }
            HarnessEventPayload::Artifact { data } => {
                validate_common_ids(&data.session_id, &data.task_id)?;
                validate_optional_name("workflow", data.workflow.as_deref(), MAX_WORKFLOW_BYTES)?;
                validate_optional_name("phase", data.phase.as_deref(), MAX_PHASE_BYTES)?;
                validate_bounded("artifact name", &data.name, MAX_MESSAGE_BYTES)?;
                validate_optional_message(data.message.as_deref())?;
                if let Some(path) = data.path.as_deref() {
                    validate_bounded("artifact path", path, MAX_MESSAGE_BYTES)?;
                }
            }
            HarnessEventPayload::ValidatorResult { data } => {
                if data.schema_version > VALIDATOR_RESULT_SCHEMA_VERSION {
                    return Err(HarnessEventError(format!(
                        "unsupported validator result schema_version {} (max supported: {})",
                        data.schema_version, VALIDATOR_RESULT_SCHEMA_VERSION
                    )));
                }
                validate_common_ids(&data.session_id, &data.task_id)?;
                validate_optional_name("workflow", data.workflow.as_deref(), MAX_WORKFLOW_BYTES)?;
                validate_optional_name("phase", data.phase.as_deref(), MAX_PHASE_BYTES)?;
                validate_bounded("validator", &data.validator, MAX_MESSAGE_BYTES)?;
                validate_optional_message(data.message.as_deref())?;
            }
            HarnessEventPayload::Retry { data } => {
                validate_common_ids(&data.session_id, &data.task_id)?;
                validate_optional_name("workflow", data.workflow.as_deref(), MAX_WORKFLOW_BYTES)?;
                validate_optional_name("phase", data.phase.as_deref(), MAX_PHASE_BYTES)?;
                validate_optional_message(data.message.as_deref())?;
            }
            HarnessEventPayload::Failure { data } => {
                validate_common_ids(&data.session_id, &data.task_id)?;
                validate_optional_name("workflow", data.workflow.as_deref(), MAX_WORKFLOW_BYTES)?;
                validate_optional_name("phase", data.phase.as_deref(), MAX_PHASE_BYTES)?;
                validate_bounded("failure message", &data.message, MAX_MESSAGE_BYTES)?;
            }
            HarnessEventPayload::McpServerCall { data } => {
                validate_common_ids(&data.session_id, &data.task_id)?;
                validate_bounded("tool", &data.tool, MAX_WORKFLOW_BYTES)?;
                validate_bounded("caller_id", &data.caller_id, MAX_WORKFLOW_BYTES)?;
                validate_bounded("transport", &data.transport, MAX_PHASE_BYTES)?;
                validate_bounded("outcome", &data.outcome, MAX_PHASE_BYTES)?;
                validate_optional_name("contract", data.contract.as_deref(), MAX_WORKFLOW_BYTES)?;
                if let Some(error) = data.error.as_deref() {
                    validate_bounded("error", error, MAX_MESSAGE_BYTES)?;
                }
            }
            HarnessEventPayload::SubAgentDispatch { data } => {
                if data.schema_version > SUB_AGENT_DISPATCH_SCHEMA_VERSION {
                    return Err(HarnessEventError(format!(
                        "unsupported sub-agent dispatch schema_version {} (max supported: {})",
                        data.schema_version, SUB_AGENT_DISPATCH_SCHEMA_VERSION
                    )));
                }
                validate_common_ids(&data.session_id, &data.task_id)?;
                validate_optional_name("workflow", data.workflow.as_deref(), MAX_WORKFLOW_BYTES)?;
                validate_optional_name("phase", data.phase.as_deref(), MAX_PHASE_BYTES)?;
                validate_bounded("sub-agent backend", &data.backend, MAX_MESSAGE_BYTES)?;
                validate_bounded("sub-agent endpoint", &data.endpoint, MAX_MESSAGE_BYTES)?;
                validate_bounded("sub-agent outcome", &data.outcome, MAX_MESSAGE_BYTES)?;
                validate_optional_message(data.message.as_deref())?;
            }
            HarnessEventPayload::SwarmDispatch { data } => {
                if data.schema_version > SWARM_DISPATCH_SCHEMA_VERSION {
                    return Err(HarnessEventError(format!(
                        "unsupported swarm dispatch schema_version {} (max supported: {})",
                        data.schema_version, SWARM_DISPATCH_SCHEMA_VERSION
                    )));
                }
                validate_common_ids(&data.session_id, &data.task_id)?;
                validate_optional_name("workflow", data.workflow.as_deref(), MAX_WORKFLOW_BYTES)?;
                validate_optional_name("phase", data.phase.as_deref(), MAX_PHASE_BYTES)?;
                validate_bounded("swarm dispatch_id", &data.dispatch_id, MAX_MESSAGE_BYTES)?;
                validate_bounded("swarm topology", &data.topology, MAX_MESSAGE_BYTES)?;
                validate_bounded("swarm outcome", &data.outcome, MAX_MESSAGE_BYTES)?;
                validate_optional_message(data.message.as_deref())?;
            }
            HarnessEventPayload::SwarmReviewDecision { data } => {
                if data.schema_version > SWARM_REVIEW_DECISION_SCHEMA_VERSION {
                    return Err(HarnessEventError(format!(
                        "unsupported swarm review decision schema_version {} (max supported: {})",
                        data.schema_version, SWARM_REVIEW_DECISION_SCHEMA_VERSION
                    )));
                }
                validate_common_ids(&data.session_id, &data.task_id)?;
                validate_optional_name("workflow", data.workflow.as_deref(), MAX_WORKFLOW_BYTES)?;
                validate_optional_name("phase", data.phase.as_deref(), MAX_PHASE_BYTES)?;
                validate_bounded("review dispatch_id", &data.dispatch_id, MAX_MESSAGE_BYTES)?;
                validate_bounded("reviewer", &data.reviewer, MAX_SESSION_ID_BYTES)?;
                if let Some(notes) = data.notes.as_deref() {
                    validate_bounded("review notes", notes, MAX_MESSAGE_BYTES)?;
                }
            }
            HarnessEventPayload::CostAttribution { data } => {
                if data.schema_version > COST_ATTRIBUTION_SCHEMA_VERSION {
                    return Err(HarnessEventError(format!(
                        "unsupported cost attribution schema_version {} (max supported: {})",
                        data.schema_version, COST_ATTRIBUTION_SCHEMA_VERSION
                    )));
                }
                validate_common_ids(&data.session_id, &data.task_id)?;
                validate_optional_name("workflow", data.workflow.as_deref(), MAX_WORKFLOW_BYTES)?;
                validate_optional_name("phase", data.phase.as_deref(), MAX_PHASE_BYTES)?;
                validate_bounded("attribution_id", &data.attribution_id, MAX_MESSAGE_BYTES)?;
                validate_bounded("contract_id", &data.contract_id, MAX_MESSAGE_BYTES)?;
                validate_bounded("model", &data.model, MAX_MESSAGE_BYTES)?;
                validate_bounded("outcome", &data.outcome, MAX_MESSAGE_BYTES)?;
                if !data.cost_usd.is_finite() {
                    return Err(HarnessEventError(format!(
                        "cost_usd must be finite, got {}",
                        data.cost_usd
                    )));
                }
                if data.cost_usd < 0.0 {
                    return Err(HarnessEventError(format!(
                        "cost_usd must be non-negative, got {}",
                        data.cost_usd
                    )));
                }
            }
            HarnessEventPayload::RoutingDecision { data } => {
                validate_common_ids(&data.session_id, &data.task_id)?;
                validate_optional_name("workflow", data.workflow.as_deref(), MAX_WORKFLOW_BYTES)?;
                validate_optional_name("phase", data.phase.as_deref(), MAX_PHASE_BYTES)?;
                validate_bounded("tier", &data.tier, MAX_PHASE_BYTES)?;
                validate_optional_name("lane", data.lane.as_deref(), MAX_PHASE_BYTES)?;
                for reason in &data.reasons {
                    validate_bounded("reason", reason, MAX_MESSAGE_BYTES)?;
                }
            }
            HarnessEventPayload::CredentialRotation { data } => {
                validate_common_ids(&data.session_id, &data.task_id)?;
                validate_bounded(
                    "credential_id",
                    &data.credential_id,
                    MAX_CREDENTIAL_ID_BYTES,
                )?;
                validate_bounded("reason", &data.reason, MAX_PHASE_BYTES)?;
                validate_bounded("strategy", &data.strategy, MAX_PHASE_BYTES)?;
            }
            HarnessEventPayload::SessionSanitized { data } => {
                validate_common_ids(&data.session_id, &data.task_id)?;
                validate_optional_name("workflow", data.workflow.as_deref(), MAX_WORKFLOW_BYTES)?;
                for warning in &data.warnings {
                    validate_bounded("warning", warning, MAX_MESSAGE_BYTES)?;
                }
            }
            HarnessEventPayload::SubagentProgress { data } => {
                validate_common_ids(&data.session_id, &data.task_id)?;
                validate_bounded("summary", &data.summary, MAX_MESSAGE_BYTES)?;
            }
            HarnessEventPayload::Error { data } => {
                if data.schema_version > HARNESS_ERROR_SCHEMA_VERSION {
                    return Err(HarnessEventError(format!(
                        "unsupported harness error schema_version {} (max supported: {})",
                        data.schema_version, HARNESS_ERROR_SCHEMA_VERSION
                    )));
                }
                validate_common_ids(&data.session_id, &data.task_id)?;
                validate_optional_name("workflow", data.workflow.as_deref(), MAX_WORKFLOW_BYTES)?;
                validate_optional_name("phase", data.phase.as_deref(), MAX_PHASE_BYTES)?;
                validate_bounded("variant", &data.variant, MAX_PHASE_BYTES)?;
                validate_bounded("recovery", &data.recovery, MAX_PHASE_BYTES)?;
                validate_bounded("error message", &data.message, MAX_MESSAGE_BYTES)?;
            }
        }

        Ok(())
    }

    pub fn runtime_detail_value(
        &self,
        fallback_workflow_kind: Option<&str>,
        fallback_current_phase: Option<&str>,
    ) -> Value {
        match &self.payload {
            HarnessEventPayload::Progress { data } => {
                let workflow = data.workflow.as_deref().or(fallback_workflow_kind);
                let current_phase = Some(data.phase.as_str()).or(fallback_current_phase);
                let message = data.message.as_deref();
                let mut detail = serde_json::json!({
                    "schema": self.schema,
                    "schema_version": data.schema_version,
                    "kind": "progress",
                    "session_id": data.session_id,
                    "task_id": data.task_id,
                    "workflow": workflow,
                    "workflow_kind": workflow,
                    "phase": data.phase,
                    "current_phase": current_phase,
                    "message": message,
                    "progress_message": message,
                    "progress": data.progress,
                });
                // Gap 4.2 — additively surface the structured `extra` fields
                // (node/node_index/node_total/eta_secs/preview) so consumers
                // can render real per-node progress. Canonical typed keys win:
                // a producer can never clobber `progress`/`kind`/etc. by
                // stuffing them into `extra`.
                if !data.extra.is_empty() {
                    if let Some(obj) = detail.as_object_mut() {
                        for (k, v) in &data.extra {
                            obj.entry(k.clone()).or_insert_with(|| v.clone());
                        }
                    }
                }
                detail
            }
            HarnessEventPayload::Phase { data } => {
                let workflow = data.workflow.as_deref().or(fallback_workflow_kind);
                let current_phase = Some(data.phase.as_str()).or(fallback_current_phase);
                let message = data.message.as_deref();
                serde_json::json!({
                    "schema": self.schema,
                    "kind": "phase",
                    "session_id": data.session_id,
                    "task_id": data.task_id,
                    "workflow": workflow,
                    "workflow_kind": workflow,
                    "phase": data.phase,
                    "current_phase": current_phase,
                    "message": message,
                    "progress_message": message,
                })
            }
            HarnessEventPayload::Artifact { data } => {
                let workflow = data.workflow.as_deref().or(fallback_workflow_kind);
                let current_phase = data.phase.as_deref().or(fallback_current_phase);
                serde_json::json!({
                    "schema": self.schema,
                    "kind": "artifact",
                    "session_id": data.session_id,
                    "task_id": data.task_id,
                    "workflow": workflow,
                    "workflow_kind": workflow,
                    "phase": data.phase,
                    "current_phase": current_phase,
                    "artifact_name": data.name,
                    "artifact_path": data.path,
                    "message": data.message,
                })
            }
            HarnessEventPayload::ValidatorResult { data } => {
                let workflow = data.workflow.as_deref().or(fallback_workflow_kind);
                let current_phase = data.phase.as_deref().or(fallback_current_phase);
                serde_json::json!({
                    "schema": self.schema,
                    "schema_version": data.schema_version,
                    "kind": "validator_result",
                    "session_id": data.session_id,
                    "task_id": data.task_id,
                    "workflow": workflow,
                    "workflow_kind": workflow,
                    "phase": data.phase,
                    "current_phase": current_phase,
                    "validator": data.validator,
                    "passed": data.passed,
                    "message": data.message,
                })
            }
            HarnessEventPayload::Retry { data } => {
                let workflow = data.workflow.as_deref().or(fallback_workflow_kind);
                let current_phase = data.phase.as_deref().or(fallback_current_phase);
                serde_json::json!({
                    "schema": self.schema,
                    "kind": "retry",
                    "session_id": data.session_id,
                    "task_id": data.task_id,
                    "workflow": workflow,
                    "workflow_kind": workflow,
                    "phase": data.phase,
                    "current_phase": current_phase,
                    "attempt": data.attempt,
                    "message": data.message,
                })
            }
            HarnessEventPayload::Failure { data } => {
                let workflow = data.workflow.as_deref().or(fallback_workflow_kind);
                let current_phase = data.phase.as_deref().or(fallback_current_phase);
                serde_json::json!({
                    "schema": self.schema,
                    "kind": "failure",
                    "session_id": data.session_id,
                    "task_id": data.task_id,
                    "workflow": workflow,
                    "workflow_kind": workflow,
                    "phase": data.phase,
                    "current_phase": current_phase,
                    "message": data.message,
                    "retryable": data.retryable,
                })
            }
            HarnessEventPayload::McpServerCall { data } => serde_json::json!({
                "schema": self.schema,
                "kind": "mcp_server_call",
                "session_id": data.session_id,
                "task_id": data.task_id,
                "tool": data.tool,
                "caller_id": data.caller_id,
                "transport": data.transport,
                "outcome": data.outcome,
                "contract": data.contract,
                "workflow": fallback_workflow_kind,
                "workflow_kind": fallback_workflow_kind,
                "current_phase": fallback_current_phase,
                "error": data.error,
            }),
            HarnessEventPayload::SubAgentDispatch { data } => {
                let workflow = data.workflow.as_deref().or(fallback_workflow_kind);
                let current_phase = data.phase.as_deref().or(fallback_current_phase);
                serde_json::json!({
                    "schema": self.schema,
                    "schema_version": data.schema_version,
                    "kind": "sub_agent_dispatch",
                    "session_id": data.session_id,
                    "task_id": data.task_id,
                    "workflow": workflow,
                    "workflow_kind": workflow,
                    "phase": data.phase,
                    "current_phase": current_phase,
                    "backend": data.backend,
                    "endpoint": data.endpoint,
                    "outcome": data.outcome,
                    "message": data.message,
                })
            }
            HarnessEventPayload::SwarmDispatch { data } => {
                let workflow = data.workflow.as_deref().or(fallback_workflow_kind);
                let current_phase = data.phase.as_deref().or(fallback_current_phase);
                serde_json::json!({
                    "schema": self.schema,
                    "schema_version": data.schema_version,
                    "kind": "swarm_dispatch",
                    "session_id": data.session_id,
                    "task_id": data.task_id,
                    "workflow": workflow,
                    "workflow_kind": workflow,
                    "phase": data.phase,
                    "current_phase": current_phase,
                    "dispatch_id": data.dispatch_id,
                    "topology": data.topology,
                    "outcome": data.outcome,
                    "total_subtasks": data.total_subtasks,
                    "completed_subtasks": data.completed_subtasks,
                    "retry_round": data.retry_round,
                    "message": data.message,
                })
            }
            HarnessEventPayload::SwarmReviewDecision { data } => {
                let workflow = data.workflow.as_deref().or(fallback_workflow_kind);
                let current_phase = data.phase.as_deref().or(fallback_current_phase);
                serde_json::json!({
                    "schema": self.schema,
                    "schema_version": data.schema_version,
                    "kind": "swarm_review_decision",
                    "session_id": data.session_id,
                    "task_id": data.task_id,
                    "workflow": workflow,
                    "workflow_kind": workflow,
                    "phase": data.phase,
                    "current_phase": current_phase,
                    "dispatch_id": data.dispatch_id,
                    "accepted": data.accepted,
                    "reviewer": data.reviewer,
                    "notes": data.notes,
                })
            }
            HarnessEventPayload::CostAttribution { data } => {
                let workflow = data.workflow.as_deref().or(fallback_workflow_kind);
                let current_phase = data.phase.as_deref().or(fallback_current_phase);
                serde_json::json!({
                    "schema": self.schema,
                    "schema_version": data.schema_version,
                    "kind": "cost_attribution",
                    "session_id": data.session_id,
                    "task_id": data.task_id,
                    "workflow": workflow,
                    "workflow_kind": workflow,
                    "phase": data.phase,
                    "current_phase": current_phase,
                    "attribution_id": data.attribution_id,
                    "contract_id": data.contract_id,
                    "model": data.model,
                    "tokens_in": data.tokens_in,
                    "tokens_out": data.tokens_out,
                    "cost_usd": data.cost_usd,
                    "outcome": data.outcome,
                })
            }
            HarnessEventPayload::RoutingDecision { data } => {
                let workflow = data.workflow.as_deref().or(fallback_workflow_kind);
                let current_phase = data.phase.as_deref().or(fallback_current_phase);
                serde_json::json!({
                    "schema": self.schema,
                    "kind": "routing.decision",
                    "session_id": data.session_id,
                    "task_id": data.task_id,
                    "workflow": workflow,
                    "workflow_kind": workflow,
                    "phase": data.phase,
                    "current_phase": current_phase,
                    "tier": data.tier,
                    "lane": data.lane,
                    "reasons": data.reasons,
                    "input_chars": data.input_chars,
                })
            }
            HarnessEventPayload::CredentialRotation { data } => {
                serde_json::json!({
                    "schema": self.schema,
                    "kind": "credential_rotation",
                    "session_id": data.session_id,
                    "task_id": data.task_id,
                    "credential_id": data.credential_id,
                    "reason": data.reason,
                    "strategy": data.strategy,
                })
            }
            HarnessEventPayload::SessionSanitized { data } => {
                let workflow = data.workflow.as_deref().or(fallback_workflow_kind);
                serde_json::json!({
                    "schema": self.schema,
                    "kind": "session_sanitized",
                    "session_id": data.session_id,
                    "task_id": data.task_id,
                    "workflow": workflow,
                    "workflow_kind": workflow,
                    "current_phase": fallback_current_phase,
                    "input_len": data.input_len,
                    "output_len": data.output_len,
                    "unresolved_tool_uses_dropped": data.unresolved_tool_uses_dropped,
                    "orphan_thinking_dropped": data.orphan_thinking_dropped,
                    "whitespace_only_dropped": data.whitespace_only_dropped,
                    "content_replacements_restored": data.content_replacements_restored,
                    "worktree_missing": data.worktree_missing,
                    "warnings": data.warnings,
                })
            }
            HarnessEventPayload::SubagentProgress { data } => {
                serde_json::json!({
                    "schema": self.schema,
                    "kind": "subagent_progress",
                    "session_id": data.session_id,
                    "task_id": data.task_id,
                    "summary": data.summary,
                    "tick": data.tick_seq,
                    "at": data.at.to_rfc3339(),
                    "workflow_kind": fallback_workflow_kind,
                    "current_phase": fallback_current_phase,
                })
            }
            HarnessEventPayload::Error { data } => {
                let workflow = data.workflow.as_deref().or(fallback_workflow_kind);
                let current_phase = data.phase.as_deref().or(fallback_current_phase);
                serde_json::json!({
                    "schema": self.schema,
                    "schema_version": data.schema_version,
                    "kind": "error",
                    "session_id": data.session_id,
                    "task_id": data.task_id,
                    "workflow": workflow,
                    "workflow_kind": workflow,
                    "phase": data.phase,
                    "current_phase": current_phase,
                    "variant": data.variant,
                    "recovery": data.recovery,
                    "message": data.message,
                    "details": data.details,
                })
            }
        }
    }

    pub fn session_id(&self) -> &str {
        match &self.payload {
            HarnessEventPayload::Progress { data } => &data.session_id,
            HarnessEventPayload::Phase { data } => &data.session_id,
            HarnessEventPayload::Artifact { data } => &data.session_id,
            HarnessEventPayload::ValidatorResult { data } => &data.session_id,
            HarnessEventPayload::Retry { data } => &data.session_id,
            HarnessEventPayload::Failure { data } => &data.session_id,
            HarnessEventPayload::McpServerCall { data } => &data.session_id,
            HarnessEventPayload::SubAgentDispatch { data } => &data.session_id,
            HarnessEventPayload::SwarmDispatch { data } => &data.session_id,
            HarnessEventPayload::SwarmReviewDecision { data } => &data.session_id,
            HarnessEventPayload::CostAttribution { data } => &data.session_id,
            HarnessEventPayload::RoutingDecision { data } => &data.session_id,
            HarnessEventPayload::CredentialRotation { data } => &data.session_id,
            HarnessEventPayload::SessionSanitized { data } => &data.session_id,
            HarnessEventPayload::SubagentProgress { data } => &data.session_id,
            HarnessEventPayload::Error { data } => &data.session_id,
        }
    }

    pub fn task_id(&self) -> &str {
        match &self.payload {
            HarnessEventPayload::Progress { data } => &data.task_id,
            HarnessEventPayload::Phase { data } => &data.task_id,
            HarnessEventPayload::Artifact { data } => &data.task_id,
            HarnessEventPayload::ValidatorResult { data } => &data.task_id,
            HarnessEventPayload::Retry { data } => &data.task_id,
            HarnessEventPayload::Failure { data } => &data.task_id,
            HarnessEventPayload::McpServerCall { data } => &data.task_id,
            HarnessEventPayload::SubAgentDispatch { data } => &data.task_id,
            HarnessEventPayload::SwarmDispatch { data } => &data.task_id,
            HarnessEventPayload::SwarmReviewDecision { data } => &data.task_id,
            HarnessEventPayload::CostAttribution { data } => &data.task_id,
            HarnessEventPayload::RoutingDecision { data } => &data.task_id,
            HarnessEventPayload::CredentialRotation { data } => &data.task_id,
            HarnessEventPayload::SessionSanitized { data } => &data.task_id,
            HarnessEventPayload::SubagentProgress { data } => &data.task_id,
            HarnessEventPayload::Error { data } => &data.task_id,
        }
    }

    pub fn workflow(&self) -> Option<&str> {
        match &self.payload {
            HarnessEventPayload::Progress { data } => data.workflow.as_deref(),
            HarnessEventPayload::Phase { data } => data.workflow.as_deref(),
            HarnessEventPayload::Artifact { data } => data.workflow.as_deref(),
            HarnessEventPayload::ValidatorResult { data } => data.workflow.as_deref(),
            HarnessEventPayload::Retry { data } => data.workflow.as_deref(),
            HarnessEventPayload::Failure { data } => data.workflow.as_deref(),
            HarnessEventPayload::McpServerCall { .. } => None,
            HarnessEventPayload::SubAgentDispatch { data } => data.workflow.as_deref(),
            HarnessEventPayload::SwarmDispatch { data } => data.workflow.as_deref(),
            HarnessEventPayload::SwarmReviewDecision { data } => data.workflow.as_deref(),
            HarnessEventPayload::CostAttribution { data } => data.workflow.as_deref(),
            HarnessEventPayload::RoutingDecision { data } => data.workflow.as_deref(),
            HarnessEventPayload::CredentialRotation { .. } => None,
            HarnessEventPayload::SessionSanitized { data } => data.workflow.as_deref(),
            HarnessEventPayload::SubagentProgress { .. } => None,
            HarnessEventPayload::Error { data } => data.workflow.as_deref(),
        }
    }

    pub fn phase(&self) -> Option<&str> {
        match &self.payload {
            HarnessEventPayload::Progress { data } => Some(data.phase.as_str()),
            HarnessEventPayload::Phase { data } => Some(data.phase.as_str()),
            HarnessEventPayload::Artifact { data } => data.phase.as_deref(),
            HarnessEventPayload::ValidatorResult { data } => data.phase.as_deref(),
            HarnessEventPayload::Retry { data } => data.phase.as_deref(),
            HarnessEventPayload::Failure { data } => data.phase.as_deref(),
            HarnessEventPayload::McpServerCall { .. } => None,
            HarnessEventPayload::SubAgentDispatch { data } => data.phase.as_deref(),
            HarnessEventPayload::SwarmDispatch { data } => data.phase.as_deref(),
            HarnessEventPayload::SwarmReviewDecision { data } => data.phase.as_deref(),
            HarnessEventPayload::CostAttribution { data } => data.phase.as_deref(),
            HarnessEventPayload::RoutingDecision { data } => data.phase.as_deref(),
            HarnessEventPayload::CredentialRotation { .. } => None,
            HarnessEventPayload::SessionSanitized { .. } => None,
            HarnessEventPayload::SubagentProgress { .. } => None,
            HarnessEventPayload::Error { data } => data.phase.as_deref(),
        }
    }
}

fn validate_common_ids(session_id: &str, task_id: &str) -> HarnessResult<()> {
    validate_bounded("session_id", session_id, MAX_SESSION_ID_BYTES)?;
    validate_bounded("task_id", task_id, MAX_TASK_ID_BYTES)?;
    Ok(())
}

fn validate_optional_name(
    field: &'static str,
    value: Option<&str>,
    max: usize,
) -> HarnessResult<()> {
    if let Some(value) = value {
        validate_bounded(field, value, max)?;
    }
    Ok(())
}

fn validate_phase(phase: &str) -> HarnessResult<()> {
    validate_bounded("phase", phase, MAX_PHASE_BYTES)?;
    if !is_valid_phase_name(phase) {
        return Err(HarnessEventError(format!(
            "invalid phase name '{phase}': expected snake_case"
        )));
    }
    Ok(())
}

fn validate_optional_message(message: Option<&str>) -> HarnessResult<()> {
    if let Some(message) = message {
        validate_bounded("message", message, MAX_MESSAGE_BYTES)?;
    }
    Ok(())
}

fn validate_progress(progress: Option<f64>) -> HarnessResult<()> {
    if let Some(progress) = progress {
        if !(0.0..=1.0).contains(&progress) {
            return Err(HarnessEventError(format!(
                "progress must be between 0.0 and 1.0, got {progress}"
            )));
        }
    }
    Ok(())
}

fn validate_bounded(field: &'static str, value: &str, max: usize) -> HarnessResult<()> {
    if value.is_empty() {
        return Err(HarnessEventError(format!("{field} cannot be empty")));
    }
    if value.len() > max {
        return Err(HarnessEventError(format!("{field} exceeded {max} bytes")));
    }
    Ok(())
}

fn is_valid_phase_name(phase: &str) -> bool {
    let mut chars = phase.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_lowercase() {
        return false;
    }
    chars.all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_')
}

/// Local sink that feeds structured child events into a task supervisor.
pub struct HarnessEventSink {
    sink_file: tempfile::NamedTempFile,
    sink_key: String,
    stop: Arc<AtomicBool>,
    reader: JoinHandle<()>,
}

impl HarnessEventSink {
    pub fn new(
        task_supervisor: Arc<TaskSupervisor>,
        task_id: impl Into<String>,
        session_id: impl Into<String>,
    ) -> std::io::Result<Self> {
        let sink_file = tempfile::NamedTempFile::new()?;
        let path = sink_file.path().to_path_buf();
        let sink_key = sink_key(&path);
        let task_id = task_id.into();
        let session_id = session_id.into();
        register_sink_context(
            sink_key.clone(),
            HarnessEventSinkContext {
                session_id: session_id.clone(),
                task_id: task_id.clone(),
            },
        );
        let stop = Arc::new(AtomicBool::new(false));
        let reader_stop = stop.clone();

        let reader = tokio::spawn(run_reader(
            path,
            task_supervisor,
            task_id,
            session_id,
            reader_stop,
        ));

        Ok(Self {
            sink_file,
            sink_key,
            stop,
            reader,
        })
    }

    pub fn path(&self) -> &Path {
        self.sink_file.path()
    }

    /// Return the transport URI child processes should receive in OCTOS_EVENT_SINK.
    pub fn uri(&self) -> String {
        format!("file://{}", self.path().display())
    }
}

impl Drop for HarnessEventSink {
    fn drop(&mut self) {
        unregister_sink_context(&self.sink_key);
        self.stop.store(true, Ordering::Release);
        self.reader.abort();
    }
}

async fn run_reader(
    path: PathBuf,
    task_supervisor: Arc<TaskSupervisor>,
    task_id: String,
    session_id: String,
    stop: Arc<AtomicBool>,
) {
    let mut file = loop {
        match tokio::fs::OpenOptions::new().read(true).open(&path).await {
            Ok(file) => break file,
            Err(error) => {
                if stop.load(Ordering::Acquire) {
                    return;
                }
                warn!(path = %path.display(), error = %error, "failed to open harness event sink");
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        }
    };

    let mut carry = Vec::new();
    let mut chunk = vec![0_u8; 4096];

    loop {
        if stop.load(Ordering::Acquire) {
            break;
        }

        let read = match file.read(&mut chunk).await {
            Ok(read) => read,
            Err(error) => {
                warn!(path = %path.display(), error = %error, "failed to read harness event sink");
                tokio::time::sleep(Duration::from_millis(25)).await;
                continue;
            }
        };

        if read == 0 {
            tokio::time::sleep(Duration::from_millis(25)).await;
            continue;
        }

        carry.extend_from_slice(&chunk[..read]);
        while let Some(pos) = carry.iter().position(|byte| *byte == b'\n') {
            let mut line = carry.drain(..=pos).collect::<Vec<u8>>();
            if line.last() == Some(&b'\n') {
                line.pop();
            }
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            if line.len() > MAX_HARNESS_EVENT_LINE_BYTES {
                warn!(
                    path = %path.display(),
                    task_id = %task_id,
                    "dropping oversized harness event line"
                );
                continue;
            }

            let Ok(line) = String::from_utf8(line) else {
                warn!(
                    path = %path.display(),
                    task_id = %task_id,
                    "dropping non-utf8 harness event line"
                );
                continue;
            };

            let Ok(event) = HarnessEvent::from_json_line(&line) else {
                warn!(
                    path = %path.display(),
                    task_id = %task_id,
                    "dropping invalid harness event line"
                );
                continue;
            };

            if event.session_id() != session_id || event.task_id() != task_id {
                warn!(
                    path = %path.display(),
                    task_id = %task_id,
                    session_id = %session_id,
                    "ignoring harness event for unexpected task/session"
                );
                continue;
            }

            if let Err(error) = task_supervisor.apply_harness_event(&task_id, &event) {
                warn!(
                    path = %path.display(),
                    task_id = %task_id,
                    error = %error,
                    "failed to apply harness event"
                );
            }
        }

        if carry.len() > MAX_HARNESS_EVENT_LINE_BYTES {
            warn!(
                path = %path.display(),
                task_id = %task_id,
                "discarding partial oversized harness event"
            );
            carry.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_event_round_trips_and_keeps_schema() {
        let event = HarnessEvent::progress(
            "session-1",
            "task-1",
            Some("deep_research"),
            "fetching_sources",
            Some("Fetching source 3/12"),
            Some(0.42),
        );

        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains(r#""schema":"octos.harness.event.v1""#));
        assert!(json.contains(r#""kind":"progress""#));

        let parsed = HarnessEvent::from_json_line(&json).unwrap();
        assert_eq!(parsed.schema, HARNESS_EVENT_SCHEMA_V1);
        assert_eq!(parsed.session_id(), "session-1");
        assert_eq!(parsed.task_id(), "task-1");
        assert_eq!(parsed.workflow(), Some("deep_research"));
        assert_eq!(parsed.phase(), Some("fetching_sources"));

        let detail = parsed.runtime_detail_value(None, None);
        assert_eq!(detail["workflow_kind"], "deep_research");
        assert_eq!(detail["current_phase"], "fetching_sources");
        assert_eq!(detail["progress_message"], "Fetching source 3/12");
    }

    /// Blocker 3 — concurrent sink writes must produce only well-formed NDJSON
    /// lines (each parses) with no interleaving. Many tasks write LARGE distinct
    /// event lines to the SAME append-only sink at once; the spawned heartbeat +
    /// parallel node emits race the same file in production. `writeln!` is not a
    /// single atomic syscall, so without the per-path write lock two large lines
    /// could interleave and corrupt the stream. After the fix every persisted
    /// line parses and every writer's unique node id appears exactly once.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_sink_writes_stay_well_formed_ndjson() {
        let sink_file = tempfile::NamedTempFile::new().expect("sink file");
        let sink_uri = sink_file.path().display().to_string();
        register_sink_context(
            sink_key_from_raw(&sink_uri),
            HarnessEventSinkContext {
                session_id: "api:session".to_string(),
                task_id: "tc-concurrent-sink".to_string(),
            },
        );

        // Large lines (~8 KiB body each, under the 16 KiB cap) maximise the
        // multi-syscall window the old `writeln!` left open.
        const WRITERS: usize = 32;
        let mut handles = Vec::new();
        for i in 0..WRITERS {
            let sink = sink_uri.clone();
            handles.push(tokio::spawn(async move {
                let mut extra: HashMap<String, Value> = HashMap::new();
                extra.insert("node".to_string(), Value::String(format!("worker-{i}")));
                // A big preview body so the serialized line spans multiple
                // syscalls' worth of bytes.
                extra.insert("preview".to_string(), Value::String("p".repeat(8 * 1024)));
                let event = HarnessEvent::progress_with_extra(
                    "api:session",
                    "tc-concurrent-sink",
                    Some("research"),
                    "node_completed",
                    Some(format!("worker {i} done")),
                    Some(1.0),
                    extra,
                );
                write_event_to_sink(&sink, &event).expect("write event");
            }));
        }
        for h in handles {
            h.await.expect("writer task");
        }

        unregister_sink_context(&sink_key_from_raw(&sink_uri));

        let contents = std::fs::read_to_string(sink_file.path()).expect("read sink");
        let lines: Vec<&str> = contents.lines().filter(|l| !l.trim().is_empty()).collect();
        assert_eq!(
            lines.len(),
            WRITERS,
            "expected exactly one well-formed line per writer (no merged/split \
             lines); got {} lines",
            lines.len()
        );
        let mut seen = std::collections::HashSet::new();
        for line in &lines {
            // Every line must parse as a complete, valid harness event — a
            // partially-interleaved write would fail here.
            let event = HarnessEvent::from_json_line(line)
                .unwrap_or_else(|e| panic!("line must be well-formed NDJSON: {e}; line={line:?}"));
            let detail = event.runtime_detail_value(None, None);
            let node = detail["node"].as_str().expect("node field").to_string();
            assert!(seen.insert(node.clone()), "duplicate node id {node}");
        }
        assert_eq!(
            seen.len(),
            WRITERS,
            "every writer's unique node id must appear exactly once"
        );
    }

    /// Blocker 4 — the per-path write lock must be keyed by the CANONICAL path
    /// so two lexically-different spellings of the SAME file share ONE lock (and
    /// therefore serialize). Before the fix the key was `display()`, so `a/../x`
    /// and `x`, or `./x` and an absolute `x`, got DIFFERENT locks — still racy.
    #[test]
    fn canonical_path_lock_shared_across_path_spellings() {
        let dir = tempfile::tempdir().expect("temp dir");
        let target = dir.path().join("sink.ndjson");
        // Touch the file so `canonicalize` resolves the full path.
        std::fs::write(&target, b"").expect("create sink");

        // Spelling A: the plain absolute path.
        let spelling_a = target.clone();
        // Spelling B: a `dir/subdir/../sink.ndjson` detour that resolves to the
        // same file. (`canonicalize` collapses the `..`.)
        let detour = dir.path().join("subdir");
        std::fs::create_dir_all(&detour).expect("subdir");
        let spelling_b = detour.join("..").join("sink.ndjson");

        assert_ne!(
            spelling_a.display().to_string(),
            spelling_b.display().to_string(),
            "the two spellings must be lexically different (else the test proves nothing)"
        );

        // The canonical lock KEY must be identical for both spellings…
        assert_eq!(
            sink_lock_key(&spelling_a),
            sink_lock_key(&spelling_b),
            "canonical lock key must be identical for two spellings of the same file"
        );
        // …and they must therefore resolve to the SAME lock Arc (pointer-equal).
        let lock_a = sink_write_lock(&spelling_a);
        let lock_b = sink_write_lock(&spelling_b);
        assert!(
            Arc::ptr_eq(&lock_a, &lock_b),
            "two spellings of the same file must share one write-lock Arc"
        );
    }

    /// Blocker 4 — the lock key for a NOT-YET-EXISTENT sink (first write before
    /// the file exists) must still be canonical and consistent: `canonicalize`
    /// fails on a missing file, so the parent dir is canonicalized and the file
    /// name re-joined. Two spellings whose parents differ only by a `..` detour
    /// must still map to one lock even before the file is created.
    #[test]
    fn canonical_path_lock_consistent_before_file_exists() {
        let dir = tempfile::tempdir().expect("temp dir");
        let detour = dir.path().join("subdir");
        std::fs::create_dir_all(&detour).expect("subdir");

        // Target does NOT exist yet (no `write`); both spellings reference it.
        let spelling_a = dir.path().join("pending.ndjson");
        let spelling_b = detour.join("..").join("pending.ndjson");
        assert!(!spelling_a.exists(), "target must not exist for this test");

        assert_eq!(
            sink_lock_key(&spelling_a),
            sink_lock_key(&spelling_b),
            "canonical lock key must be consistent across spellings even before \
             the sink file is created (parent-canonicalize fallback)"
        );
        assert!(
            Arc::ptr_eq(&sink_write_lock(&spelling_a), &sink_write_lock(&spelling_b)),
            "pre-creation spellings of the same file must share one lock"
        );
    }

    /// Blocker 4 — delegation events (a custom `{schema, kind:"delegation", …}`
    /// shape) and canonical harness events written CONCURRENTLY to the SAME sink
    /// via the atomic helpers must produce only well-formed NDJSON: no line is
    /// split or merged. This proves `delegate.rs`'s migration to
    /// `write_event_line_to_sink` shares the per-path lock with `write_event_to
    /// _sink` (the prior raw `writeln!` was UNLOCKED and could interleave).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn mixed_delegation_and_harness_writes_stay_well_formed_ndjson() {
        let sink_file = tempfile::NamedTempFile::new().expect("sink file");
        let sink_uri = sink_file.path().display().to_string();
        register_sink_context(
            sink_key_from_raw(&sink_uri),
            HarnessEventSinkContext {
                session_id: "api:session".to_string(),
                task_id: "tc-mixed-sink".to_string(),
            },
        );

        const N: usize = 16;
        let mut handles = Vec::new();
        // Half write canonical harness events (write_event_to_sink); half write
        // pre-serialized delegation-shaped lines (write_event_line_to_sink) — the
        // exact two paths delegate.rs and the executor now share.
        for i in 0..N {
            let sink = sink_uri.clone();
            handles.push(tokio::spawn(async move {
                if i % 2 == 0 {
                    let mut extra: HashMap<String, Value> = HashMap::new();
                    extra.insert("node".to_string(), Value::String(format!("hev-{i}")));
                    extra.insert("preview".to_string(), Value::String("h".repeat(6 * 1024)));
                    let event = HarnessEvent::progress_with_extra(
                        "api:session",
                        "tc-mixed-sink",
                        Some("research"),
                        "node_completed",
                        Some(format!("harness {i}")),
                        Some(1.0),
                        extra,
                    );
                    write_event_to_sink(&sink, &event).expect("write harness event");
                } else {
                    // A delegation-shaped line (NOT a HarnessEvent payload variant)
                    // with a large body to widen the interleave window.
                    let line = serde_json::json!({
                        "schema": HARNESS_EVENT_SCHEMA_V1,
                        "kind": "delegation",
                        "depth": 1,
                        "parent_task_id": format!("parent-{i}"),
                        "child_task_id": format!("child-{i}"),
                        "outcome": "ok",
                        "pad": "d".repeat(6 * 1024),
                    })
                    .to_string();
                    write_event_line_to_sink(&sink, &line).expect("write delegation line");
                }
            }));
        }
        for h in handles {
            h.await.expect("writer task");
        }

        unregister_sink_context(&sink_key_from_raw(&sink_uri));

        let contents = std::fs::read_to_string(sink_file.path()).expect("read sink");
        let lines: Vec<&str> = contents.lines().filter(|l| !l.trim().is_empty()).collect();
        assert_eq!(
            lines.len(),
            N,
            "expected exactly one whole line per writer (delegation + harness \
             share the per-path lock); got {} lines",
            lines.len()
        );
        // Every line must be complete, parseable JSON (a torn line would fail).
        for line in &lines {
            let v: Value = serde_json::from_str(line)
                .unwrap_or_else(|e| panic!("line must be whole JSON: {e}; line={line:?}"));
            assert_eq!(v["schema"], HARNESS_EVENT_SCHEMA_V1, "line={line:?}");
        }
        let delegations = lines
            .iter()
            .filter(|l| l.contains("\"delegation\""))
            .count();
        let harness = lines
            .iter()
            .filter(|l| l.contains("node_completed"))
            .count();
        assert_eq!(delegations, N / 2, "all delegation lines present and whole");
        assert_eq!(harness, N / 2, "all harness lines present and whole");
    }

    #[test]
    fn progress_with_extra_surfaces_structured_fields_in_runtime_detail() {
        // Gap 4.2 — producer-side structured per-node progress. The pipeline
        // executor needs to attach node-index/eta/preview as structured fields
        // (not buried in the message string) so existing consumers can render
        // them. They ride the additive `extra` map; `runtime_detail_value`
        // must surface them so the SPA/TUI see them via `BackgroundTask
        // .runtime_detail`.
        let mut extra = HashMap::new();
        extra.insert("node".to_string(), Value::String("analyze".into()));
        extra.insert("node_index".to_string(), Value::from(2));
        extra.insert("node_total".to_string(), Value::from(3));
        extra.insert("eta_secs".to_string(), Value::from(45));
        extra.insert(
            "preview".to_string(),
            Value::String("partial output…".into()),
        );

        let event = HarnessEvent::progress_with_extra(
            "session-1",
            "task-1",
            Some("research"),
            "node_completed",
            Some("analyze (2 of 3)"),
            Some(0.66),
            extra,
        );

        // Round-trips on the wire (extra is flattened, so it survives).
        let json = serde_json::to_string(&event).unwrap();
        let parsed = HarnessEvent::from_json_line(&json).unwrap();
        match &parsed.payload {
            HarnessEventPayload::Progress { data } => {
                assert_eq!(data.extra["node"], Value::String("analyze".into()));
                assert_eq!(data.extra["node_index"], Value::from(2));
            }
            other => panic!("expected Progress, got {other:?}"),
        }

        // Consumers read runtime_detail — the structured fields must be there.
        let detail = parsed.runtime_detail_value(Some("research"), None);
        assert_eq!(detail["progress_message"], "analyze (2 of 3)");
        assert_eq!(detail["node"], "analyze");
        assert_eq!(detail["node_index"], 2);
        assert_eq!(detail["node_total"], 3);
        assert_eq!(detail["eta_secs"], 45);
        assert_eq!(detail["preview"], "partial output…");
        // Backward-compat: the canonical progress keys must still be present so
        // consumers that ignore `extra` keep working.
        assert_eq!(detail["kind"], "progress");
        assert_eq!(detail["workflow_kind"], "research");
        let progress = detail["progress"].as_f64().unwrap();
        assert!((progress - 0.66).abs() < 0.0001);
    }

    #[test]
    fn progress_with_extra_does_not_let_extra_clobber_canonical_keys() {
        // Defense-in-depth: a producer that accidentally stuffs a reserved key
        // (e.g. "progress") into `extra` must not overwrite the typed
        // canonical value in runtime_detail — the typed fields win.
        let mut extra = HashMap::new();
        extra.insert("progress".to_string(), Value::from(0.99));
        extra.insert("kind".to_string(), Value::String("hijack".into()));
        extra.insert("node".to_string(), Value::String("plan".into()));

        let event = HarnessEvent::progress_with_extra(
            "s",
            "t",
            Some("research"),
            "node_started",
            Some("plan (1 of 3)"),
            Some(0.0),
            extra,
        );
        let detail = event.runtime_detail_value(None, None);
        // Canonical typed values survive; only the genuinely-new key lands.
        assert_eq!(detail["kind"], "progress");
        assert_eq!(detail["progress"], 0.0);
        assert_eq!(detail["node"], "plan");
    }

    #[test]
    fn ignores_unknown_future_fields() {
        let mut json = serde_json::to_value(HarnessEvent::phase_event(
            "s",
            "t",
            Some("demo"),
            "running",
            Some("phase changed"),
        ))
        .unwrap();
        json.as_object_mut()
            .unwrap()
            .insert("future_field".into(), Value::String("ok".into()));
        let parsed = HarnessEvent::from_json_line(&json.to_string()).unwrap();

        assert_eq!(parsed.workflow(), Some("demo"));
        assert_eq!(parsed.phase(), Some("running"));
    }

    #[test]
    fn accepts_legacy_progress_fraction_alias() {
        let raw = serde_json::json!({
            "schema": "octos.harness.event.v1",
            "kind": "progress",
            "session_id": "session-1",
            "task_id": "task-1",
            "workflow": "deep_research",
            "phase": "search",
            "message": "Searching",
            "progress_fraction": 0.25
        });

        let parsed = HarnessEvent::from_json_line(&raw.to_string()).unwrap();
        let detail = parsed.runtime_detail_value(None, None);
        assert_eq!(detail["progress"], 0.25);
    }

    #[test]
    fn progress_event_defaults_and_rejects_future_schema_version() {
        let legacy = serde_json::json!({
            "schema": "octos.harness.event.v1",
            "kind": "progress",
            "session_id": "session-1",
            "task_id": "task-1",
            "workflow": "deep_research",
            "phase": "fetch",
            "message": "Fetching",
            "progress": 0.4
        });

        let parsed = HarnessEvent::from_json_line(&legacy.to_string()).unwrap();
        match &parsed.payload {
            HarnessEventPayload::Progress { data } => {
                assert_eq!(data.schema_version, HARNESS_PROGRESS_EVENT_SCHEMA_VERSION);
            }
            other => panic!("expected Progress, got {other:?}"),
        }
        assert_eq!(
            parsed.runtime_detail_value(None, None)["schema_version"],
            serde_json::json!(HARNESS_PROGRESS_EVENT_SCHEMA_VERSION)
        );

        let future = serde_json::json!({
            "schema": "octos.harness.event.v1",
            "schema_version": HARNESS_PROGRESS_EVENT_SCHEMA_VERSION + 1,
            "kind": "progress",
            "session_id": "session-1",
            "task_id": "task-1",
            "phase": "fetch"
        });
        assert!(HarnessEvent::from_json_line(&future.to_string()).is_err());
    }

    #[test]
    fn validator_result_event_defaults_and_reports_schema_version() {
        let raw = serde_json::json!({
            "schema": "octos.harness.event.v1",
            "kind": "validator_result",
            "session_id": "session-1",
            "task_id": "task-1",
            "workflow": "coding",
            "phase": "verify",
            "validator": "cargo-test",
            "passed": true,
            "message": "ok"
        });

        let parsed = HarnessEvent::from_json_line(&raw.to_string()).unwrap();
        let detail = parsed.runtime_detail_value(None, None);
        assert_eq!(detail["schema_version"], VALIDATOR_RESULT_SCHEMA_VERSION);
        assert_eq!(detail["validator"], "cargo-test");
        assert_eq!(detail["passed"], true);
    }

    #[test]
    fn mcp_server_call_event_round_trips() {
        let event = HarnessEvent::mcp_server_call(
            "mcp:http",
            "task-42",
            "run_octos_session",
            "http-bearer",
            "http",
            "ready",
            Some("slides_delivery"),
            Option::<String>::None,
        );
        assert!(event.validate().is_ok());
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains(r#""kind":"mcp_server_call""#));
        let parsed = HarnessEvent::from_json_line(&json).unwrap();
        match &parsed.payload {
            HarnessEventPayload::McpServerCall { data } => {
                assert_eq!(data.tool, "run_octos_session");
                assert_eq!(data.transport, "http");
                assert_eq!(data.outcome, "ready");
                assert_eq!(data.contract.as_deref(), Some("slides_delivery"));
            }
            _ => panic!("expected McpServerCall variant"),
        }
        let detail = parsed.runtime_detail_value(None, None);
        assert_eq!(detail["kind"], "mcp_server_call");
        assert_eq!(detail["transport"], "http");
        assert_eq!(detail["outcome"], "ready");
    }

    #[test]
    fn mcp_server_call_event_rejects_empty_tool() {
        let event = HarnessEvent::mcp_server_call(
            "mcp:stdio",
            "task-1",
            "",
            "parent-process",
            "stdio",
            "ready",
            Option::<String>::None,
            Option::<String>::None,
        );
        assert!(event.validate().is_err());
    }

    #[test]
    fn should_round_trip_credential_rotation_event() {
        let event = HarnessEvent::credential_rotation(
            "session-1",
            "task-1",
            "key-42",
            "rate_limit_cooldown",
            "round_robin",
        );
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains(r#""kind":"credential_rotation""#));
        let parsed = HarnessEvent::from_json_line(&json).unwrap();
        assert_eq!(parsed.session_id(), "session-1");
        assert_eq!(parsed.task_id(), "task-1");
        let detail = parsed.runtime_detail_value(None, None);
        assert_eq!(detail["credential_id"], "key-42");
        assert_eq!(detail["reason"], "rate_limit_cooldown");
        assert_eq!(detail["strategy"], "round_robin");
    }

    #[test]
    fn should_reject_credential_rotation_event_without_required_fields() {
        let invalid = HarnessEvent::credential_rotation("s", "t", "", "initial_acquire", "random");
        assert!(invalid.validate().is_err());
        let invalid = HarnessEvent::credential_rotation("s", "t", "key", "", "random");
        assert!(invalid.validate().is_err());
        let invalid = HarnessEvent::credential_rotation("s", "t", "key", "init", "");
        assert!(invalid.validate().is_err());
    }

    #[test]
    fn routing_decision_event_round_trips_and_keeps_kind() {
        let event = HarnessEvent::routing_decision(
            "session-1",
            "task-1",
            Some("chat"),
            "strong",
            vec!["code_fence".into(), "keyword:debug".into()],
            512,
        );
        event.validate().expect("routing decision should be valid");

        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains(r#""schema":"octos.harness.event.v1""#));
        assert!(json.contains(r#""kind":"routing.decision""#));
        assert!(json.contains(r#""tier":"strong""#));

        let parsed = HarnessEvent::from_json_line(&json).unwrap();
        assert_eq!(parsed.session_id(), "session-1");
        assert_eq!(parsed.task_id(), "task-1");

        let detail = parsed.runtime_detail_value(None, None);
        assert_eq!(detail["kind"], "routing.decision");
        assert_eq!(detail["tier"], "strong");
        assert_eq!(detail["input_chars"], 512);
        assert_eq!(detail["reasons"][0], "code_fence");
    }

    #[test]
    fn subagent_progress_event_roundtrips_json_line() {
        let at = chrono::Utc::now();
        let event = HarnessEvent::subagent_progress(
            "api:session",
            "task-42",
            "fetching weather data",
            7,
            at,
        );
        assert!(event.validate().is_ok());

        let json = serde_json::to_string(&event).unwrap();
        let parsed = HarnessEvent::from_json_line(&json).unwrap();
        assert_eq!(parsed.session_id(), "api:session");
        assert_eq!(parsed.task_id(), "task-42");
        match &parsed.payload {
            HarnessEventPayload::SubagentProgress { data } => {
                assert_eq!(data.summary, "fetching weather data");
                assert_eq!(data.tick_seq, 7);
                assert_eq!(data.at, at);
            }
            other => panic!("expected SubagentProgress, got {other:?}"),
        }
        let detail = parsed.runtime_detail_value(None, None);
        assert_eq!(detail["kind"], "subagent_progress");
        assert_eq!(detail["summary"], "fetching weather data");
        assert_eq!(detail["tick"], 7);
    }

    #[test]
    fn subagent_progress_event_integrates_with_supervisor() {
        let supervisor = TaskSupervisor::new();
        let task_id = supervisor.register("search", "call-1", Some("api:session"));
        supervisor.mark_running(&task_id);

        let event = HarnessEvent::subagent_progress(
            "api:session",
            task_id.clone(),
            "parsing response",
            3,
            chrono::Utc::now(),
        );
        supervisor.apply_harness_event(&task_id, &event).unwrap();

        let task = supervisor.get_task(&task_id).expect("task missing");
        let detail: serde_json::Value =
            serde_json::from_str(task.runtime_detail.as_deref().unwrap()).unwrap();
        assert_eq!(detail["kind"], "subagent_progress");
        assert_eq!(detail["summary"], "parsing response");
        assert_eq!(detail["tick"], 3);
        // The coarse status must remain Running — progress ticks are purely
        // observational.
        assert_eq!(task.status, crate::task_supervisor::TaskStatus::Running);
    }

    #[test]
    fn rejects_oversized_fields_and_invalid_phases() {
        let oversized = HarnessEvent::progress(
            "session-1",
            "task-1",
            Some("deep_research"),
            "fetching_sources",
            Some("x".repeat(MAX_MESSAGE_BYTES + 1)),
            Some(0.42),
        );
        assert!(oversized.validate().is_err());

        let invalid_phase = HarnessEvent::progress(
            "session-1",
            "task-1",
            Some("deep_research"),
            "FetchSources",
            Some("ok"),
            Some(0.42),
        );
        assert!(invalid_phase.validate().is_err());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sink_reader_ignores_mismatched_task_or_session() {
        let supervisor = Arc::new(TaskSupervisor::new());
        let task_id = supervisor.register("search", "call-1", Some("api:session"));
        let other_task_id = supervisor.register("search", "call-2", Some("api:session"));
        supervisor.mark_running(&task_id);
        supervisor.mark_running(&other_task_id);

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        supervisor.set_on_change(move |task| {
            let _ = tx.send(task.clone());
        });

        let sink = HarnessEventSink::new(supervisor.clone(), task_id.clone(), "api:session")
            .expect("create sink");
        let wrong_task = HarnessEvent::progress(
            "api:session",
            other_task_id.clone(),
            Some("deep_research"),
            "search",
            Some("wrong task"),
            Some(0.2),
        );
        let wrong_session = HarnessEvent::progress(
            "api:other",
            task_id.clone(),
            Some("deep_research"),
            "search",
            Some("wrong session"),
            Some(0.3),
        );
        let correct = HarnessEvent::progress(
            "api:session",
            task_id.clone(),
            Some("deep_research"),
            "fetch",
            Some("Fetching 4 pages"),
            Some(0.4),
        );

        write_event_to_sink(sink.path().display().to_string(), &wrong_task).unwrap();
        write_event_to_sink(sink.path().display().to_string(), &wrong_session).unwrap();
        write_event_to_sink(sink.path().display().to_string(), &correct).unwrap();

        let updated = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let task = rx.recv().await.expect("task update");
                if task.id == task_id && task.runtime_detail.is_some() {
                    break task;
                }
            }
        })
        .await
        .expect("correct event should update task");

        let detail: Value =
            serde_json::from_str(updated.runtime_detail.as_deref().unwrap()).unwrap();
        assert_eq!(detail["task_id"], task_id);
        assert_eq!(detail["session_id"], "api:session");
        assert_eq!(detail["current_phase"], "fetch");
        assert_eq!(detail["progress_message"], "Fetching 4 pages");

        let other = supervisor
            .get_task(&other_task_id)
            .expect("other task missing");
        assert!(other.runtime_detail.is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sink_uri_is_file_transport_and_preserves_context_lookup() {
        let supervisor = Arc::new(TaskSupervisor::new());
        let task_id = supervisor.register("custom_report", "call-1", Some("api:session"));
        supervisor.mark_running(&task_id);

        let sink = HarnessEventSink::new(supervisor.clone(), task_id.clone(), "api:session")
            .expect("create sink");
        let uri = sink.uri();
        assert!(
            uri.starts_with("file://"),
            "sink URI must be file transport: {uri}"
        );

        let context = lookup_event_sink_context(&uri).expect("sink context registered for URI");
        assert_eq!(context.session_id, "api:session");
        assert_eq!(context.task_id, task_id);

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        supervisor.set_on_change(move |task| {
            let _ = tx.send(task.clone());
        });

        let event = HarnessEvent::progress(
            "api:session",
            context.task_id.clone(),
            Some("custom_report"),
            "rendering",
            Some("Rendering section 2/5"),
            Some(0.4),
        );
        write_event_to_sink(&uri, &event).unwrap();

        let updated = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let task = rx.recv().await.expect("task update");
                if task.runtime_detail.is_some() {
                    break task;
                }
            }
        })
        .await
        .expect("URI sink should update task");

        let detail: Value =
            serde_json::from_str(updated.runtime_detail.as_deref().unwrap()).unwrap();
        assert_eq!(detail["workflow_kind"], "custom_report");
        assert_eq!(detail["current_phase"], "rendering");
        assert_eq!(detail["progress_message"], "Rendering section 2/5");
    }

    /// M8.6: `SessionSanitized` round-trips through JSON and reports the
    /// report fields in `runtime_detail_value`.
    #[test]
    fn session_sanitized_event_round_trips() {
        let report = octos_bus::SessionSanitizeReport {
            input_len: 12,
            output_len: 9,
            unresolved_tool_uses_dropped: 2,
            orphan_thinking_dropped: 1,
            whitespace_only_dropped: 0,
            content_replacements_restored: 3,
            worktree_missing: false,
            warnings: vec!["mtime bump degraded".into()],
        };
        let event =
            HarnessEvent::session_sanitized("api:session", "task-resume", Some("coding"), &report);

        assert!(event.validate().is_ok());
        let json = serde_json::to_string(&event).unwrap();
        assert!(
            json.contains(r#""kind":"session_sanitized""#),
            "event should serialize the session_sanitized kind; got: {json}"
        );

        let parsed = HarnessEvent::from_json_line(&json).unwrap();
        match &parsed.payload {
            HarnessEventPayload::SessionSanitized { data } => {
                assert_eq!(data.input_len, 12);
                assert_eq!(data.output_len, 9);
                assert_eq!(data.unresolved_tool_uses_dropped, 2);
                assert_eq!(data.orphan_thinking_dropped, 1);
                assert_eq!(data.whitespace_only_dropped, 0);
                assert_eq!(data.content_replacements_restored, 3);
                assert!(!data.worktree_missing);
                assert_eq!(data.warnings, vec!["mtime bump degraded".to_string()]);
            }
            other => panic!("expected SessionSanitized variant, got {other:?}"),
        }

        let detail = parsed.runtime_detail_value(None, None);
        assert_eq!(detail["kind"], "session_sanitized");
        assert_eq!(detail["input_len"], 12);
        assert_eq!(detail["output_len"], 9);
        assert_eq!(detail["content_replacements_restored"], 3);
    }

    /// M8.6: a worktree-missing event must flag the condition so operators
    /// can see it on the task dashboard.
    #[test]
    fn session_sanitized_event_flags_worktree_missing() {
        let report = octos_bus::SessionSanitizeReport {
            input_len: 4,
            output_len: 4,
            worktree_missing: true,
            ..Default::default()
        };
        let event = HarnessEvent::session_sanitized(
            "api:session",
            "task-resume",
            Option::<String>::None,
            &report,
        );

        assert!(event.validate().is_ok());
        let detail = event.runtime_detail_value(None, None);
        assert_eq!(detail["worktree_missing"], true);
        assert_eq!(detail["kind"], "session_sanitized");
    }

    /// M8.6: verify the sink pipeline delivers a session-sanitized event
    /// to the task supervisor exactly as it does for progress/phase
    /// events. This is the "emit via sink" happy path the caller-side
    /// wiring relies on.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn should_emit_session_sanitized_event_when_sink_configured() {
        let supervisor = Arc::new(TaskSupervisor::new());
        let task_id = supervisor.register("resume", "call-1", Some("api:session"));
        supervisor.mark_running(&task_id);

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        supervisor.set_on_change(move |task| {
            let _ = tx.send(task.clone());
        });

        let sink = HarnessEventSink::new(supervisor.clone(), task_id.clone(), "api:session")
            .expect("create sink");

        let report = octos_bus::SessionSanitizeReport {
            input_len: 3,
            output_len: 2,
            unresolved_tool_uses_dropped: 1,
            ..Default::default()
        };
        let event = HarnessEvent::session_sanitized(
            "api:session",
            task_id.clone(),
            Some("coding"),
            &report,
        );

        write_event_to_sink(sink.path().display().to_string(), &event).unwrap();

        let updated = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let task = rx.recv().await.expect("task update");
                if task.id == task_id && task.runtime_detail.is_some() {
                    break task;
                }
            }
        })
        .await
        .expect("sink should deliver the session_sanitized event");

        let detail: Value =
            serde_json::from_str(updated.runtime_detail.as_deref().unwrap()).unwrap();
        assert_eq!(detail["kind"], "session_sanitized");
        assert_eq!(detail["input_len"], 3);
        assert_eq!(detail["output_len"], 2);
        assert_eq!(detail["unresolved_tool_uses_dropped"], 1);
    }
}
