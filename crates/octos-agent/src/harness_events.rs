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

use crate::abi_schema::SUB_AGENT_DISPATCH_SCHEMA_VERSION;
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

fn default_validator_result_schema_version() -> u32 {
    VALIDATOR_RESULT_SCHEMA_VERSION
}

fn default_sub_agent_dispatch_schema_version() -> u32 {
    SUB_AGENT_DISPATCH_SCHEMA_VERSION
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
    SubAgentDispatch {
        #[serde(flatten)]
        data: HarnessSubAgentDispatchEvent,
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
            HarnessEventPayload::SubAgentDispatch { data } => &data.session_id,
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
            HarnessEventPayload::SubAgentDispatch { data } => &data.task_id,
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
            HarnessEventPayload::SubAgentDispatch { data } => data.workflow.as_deref(),
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
            HarnessEventPayload::SubAgentDispatch { data } => data.phase.as_deref(),
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
}
