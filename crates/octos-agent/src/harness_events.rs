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
    /// Emitted once per session load after `ResumePolicy` runs
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

/// Typed payload emitted when `ResumePolicy` sanitizes a
/// session transcript on load (M8.6).
///
/// The report fields mirror [`octos_core::SessionSanitizeReport`] one-for-
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
    /// Count of [`octos_core::ReplacementStateRef`] entries recovered.
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
    /// [`octos_core::SessionSanitizeReport`] (M8.6). The caller supplies
    /// session_id/task_id/workflow from its runtime context; the rest of
    /// the fields come straight from the report.
    pub fn session_sanitized(
        session_id: impl Into<String>,
        task_id: impl Into<String>,
        workflow: Option<impl Into<String>>,
        report: &octos_core::SessionSanitizeReport,
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
