//! Structured harness event ABI and local sink transport.
//!
//! Child tools/workflows write newline-delimited JSON events to a local path
//! exposed through `OCTOS_EVENT_SINK`. The runtime consumes those events and
//! folds them into durable task snapshots.

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
    SUB_AGENT_DISPATCH_SCHEMA_VERSION, SWARM_DISPATCH_SCHEMA_VERSION,
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
const MAX_WORKFLOW_BYTES: usize = 128;
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

fn sink_path_from_raw(raw_sink: &str) -> PathBuf {
    if let Some(rest) = raw_sink.strip_prefix("file://") {
        return PathBuf::from(rest.strip_prefix("localhost").unwrap_or(rest));
    }
    PathBuf::from(raw_sink)
}

fn sink_key_from_raw(raw_sink: &str) -> String {
    sink_path_from_raw(raw_sink).display().to_string()
}

fn sink_key(path: &Path) -> String {
    path.display().to_string()
}

fn register_sink_context(sink: String, context: HarnessEventSinkContext) {
    let mut contexts = sink_contexts().lock().unwrap_or_else(|e| e.into_inner());
    contexts.insert(sink, context);
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
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    let json = serde_json::to_string(event)
        .map_err(|error| std::io::Error::other(format!("serialize harness event: {error}")))?;
    writeln!(file, "{json}")?;
    file.flush()
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
    Error {
        #[serde(flatten)]
        data: HarnessErrorEvent,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HarnessProgressEvent {
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
                "harness event line exceeded {} bytes",
                MAX_HARNESS_EVENT_LINE_BYTES
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
                serde_json::json!({
                    "schema": self.schema,
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
                })
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
            HarnessEventPayload::CostAttribution { data } => &data.session_id,
            HarnessEventPayload::RoutingDecision { data } => &data.session_id,
            HarnessEventPayload::CredentialRotation { data } => &data.session_id,
            HarnessEventPayload::SessionSanitized { data } => &data.session_id,
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
            HarnessEventPayload::CostAttribution { data } => &data.task_id,
            HarnessEventPayload::RoutingDecision { data } => &data.task_id,
            HarnessEventPayload::CredentialRotation { data } => &data.task_id,
            HarnessEventPayload::SessionSanitized { data } => &data.task_id,
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
            HarnessEventPayload::CostAttribution { data } => data.workflow.as_deref(),
            HarnessEventPayload::RoutingDecision { data } => data.workflow.as_deref(),
            HarnessEventPayload::CredentialRotation { .. } => None,
            HarnessEventPayload::SessionSanitized { data } => data.workflow.as_deref(),
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
            HarnessEventPayload::CostAttribution { data } => data.phase.as_deref(),
            HarnessEventPayload::RoutingDecision { data } => data.phase.as_deref(),
            HarnessEventPayload::CredentialRotation { .. } => None,
            HarnessEventPayload::SessionSanitized { .. } => None,
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
        let task_id = supervisor.register("deep_search", "call-1", Some("api:session"));
        let other_task_id = supervisor.register("deep_search", "call-2", Some("api:session"));
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
