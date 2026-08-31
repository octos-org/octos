//! Hook/lifecycle system for running shell commands at agent lifecycle points.
//!
//! Supports tool, LLM, session, and background-task lifecycle events.
//! Before-hooks can deny operations (exit code 1). Circuit breaker auto-disables
//! hooks after consecutive failures.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use metrics::counter;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::warn;

use crate::abi_schema::{HOOK_PAYLOAD_SCHEMA_VERSION, default_hook_payload_schema_version};
use crate::sandbox::BLOCKED_ENV_VARS;
use crate::subprocess_env::{EnvAllowlist, sanitize_command_env};

/// Session-level context injected into hook payloads.
/// Set by the caller (gateway/chat) before the agent loop starts.
#[derive(Debug, Clone, Default)]
pub struct HookContext {
    pub session_id: Option<String>,
    pub profile_id: Option<String>,
}

/// Lifecycle events that can trigger hooks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookEvent {
    /// Fires once when a real user-submitted prompt enters a turn, BEFORE the
    /// agent/LLM processes it (mirrors Claude Code's `UserPromptSubmit`). A
    /// before-hook can DENY the prompt (exit 1) to block the turn, or inject
    /// additional per-turn context (exit 0 with stdout) that is prepended to
    /// the model's input for that turn. Distinct from [`Self::BeforeLlmCall`],
    /// which fires on every LLM iteration within a turn.
    UserPromptSubmit,
    BeforeToolCall,
    AfterToolCall,
    BeforeLlmCall,
    AfterLlmCall,
    OnResume,
    OnTurnEnd,
    BeforeSpawnVerify,
    OnSpawnVerify,
    OnSpawnComplete,
    OnSpawnFailure,
}

impl HookEvent {
    /// The config-string form of this event, matching the `snake_case` serde
    /// rename used in `config.json`'s `hooks[].event` field. Kept as an
    /// explicit match (rather than re-deriving from serde) so the enum ⇄
    /// string mapping is auditable in one place and usable in log/error
    /// messages without a `serde_json` round-trip.
    pub fn as_str(&self) -> &'static str {
        match self {
            HookEvent::UserPromptSubmit => "user_prompt_submit",
            HookEvent::BeforeToolCall => "before_tool_call",
            HookEvent::AfterToolCall => "after_tool_call",
            HookEvent::BeforeLlmCall => "before_llm_call",
            HookEvent::AfterLlmCall => "after_llm_call",
            HookEvent::OnResume => "on_resume",
            HookEvent::OnTurnEnd => "on_turn_end",
            HookEvent::BeforeSpawnVerify => "before_spawn_verify",
            HookEvent::OnSpawnVerify => "on_spawn_verify",
            HookEvent::OnSpawnComplete => "on_spawn_complete",
            HookEvent::OnSpawnFailure => "on_spawn_failure",
        }
    }
}

/// Configuration for a single hook.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HookConfig {
    /// Which lifecycle event triggers this hook.
    pub event: HookEvent,
    /// Command as argv array (no shell interpretation).
    pub command: Vec<String>,
    /// Timeout in milliseconds (default 5000).
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
    /// Only trigger for these tool names (tool events only). Empty = all tools.
    #[serde(default)]
    pub tool_filter: Vec<String>,
    /// Only fire when the tool call's argument `path` matches one of these
    /// glob patterns (tool events only). Empty = no path filtering applied
    /// (today's behaviour: fire-for-all matching tools). Used by the M9
    /// AfterTool coding-agent gate to scope `cargo check` to `**/*.rs`
    /// edits, etc.
    ///
    /// Path extraction is tool-specific: `edit_file`, `write_file`, and
    /// `diff_edit` all expose the target path at `args.path`. For tools
    /// that do not surface a path (e.g. `shell`, `read_file`), a non-empty
    /// `path_filter` causes the hook to be skipped — operators opt into
    /// path-scoped filtering at their own risk.
    ///
    /// Invalid glob patterns are logged once at executor init and the
    /// pattern is dropped; the remaining valid patterns still apply.
    #[serde(default)]
    pub path_filter: Vec<String>,
    /// Optional binary that must be discoverable on `PATH` for this hook
    /// to fire. Used by [`WorkspacePolicy::for_coding`] to gate optional
    /// language hooks (`eslint`, `ruff`) without forcing operators to
    /// install every linter. Absent or empty = no gating.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requires_bin: Option<String>,
}

fn default_timeout_ms() -> u64 {
    5000
}

/// Payload sent to hook process as JSON on stdin.
///
/// `schema_version` is the durable ABI version. Hook consumers can branch on
/// it before reading schema-specific fields; see
/// `docs/OCTOS_HARNESS_ABI_VERSIONING.md` for the stable and experimental
/// field list.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookPayload {
    /// Durable ABI schema version for this payload. Defaults to
    /// [`HOOK_PAYLOAD_SCHEMA_VERSION`] when absent so consumers that replay a
    /// pre-versioned stream continue to parse.
    #[serde(default = "default_hook_payload_schema_version")]
    pub schema_version: u32,
    pub event: HookEvent,
    /// The user's submitted prompt text (`user_prompt_submit` event only).
    /// Truncated to [`MAX_PAYLOAD_FIELD_BYTES`] like other free-text fields.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    /// Working directory the turn runs in (`user_prompt_submit` event).
    /// Lets prompt hooks emit cwd-scoped context (git state, policy, etc).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arguments: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub success: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_count: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub iteration: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub has_tool_calls: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u32>,

    // Session context (all events)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<String>,

    // Cumulative tracking (after_llm)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cumulative_input_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cumulative_output_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_cost: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_cost: Option<f64>,

    // Provider info (after_llm)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<u64>,

    // Session/background lifecycle events
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_session_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub child_session_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workflow_kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_phase: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub output_files: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_action: Option<String>,

    /// Opaque integrator-supplied context (robotics, domain-specific sensors, etc).
    /// Populated by a `HookPayloadEnricher` registered on `HookExecutor`.
    /// Serialized form is truncated to `MAX_PAYLOAD_FIELD_BYTES`; if the rendered
    /// JSON exceeds that limit the field is replaced with a `{"truncated": true}`
    /// marker object so hook scripts always see valid JSON.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub domain_data: Option<serde_json::Value>,
}

/// Maximum byte length for arguments/result fields in hook payloads.
const MAX_PAYLOAD_FIELD_BYTES: usize = 1024;

/// Tool names whose arguments and results may contain secrets (file contents,
/// shell output, passwords). Their payloads are replaced with a redaction
/// notice instead of being truncated.
const SENSITIVE_TOOLS: &[&str] = &["shell", "write_file", "read_file"];

/// Truncate a string to at most `max_bytes`, cutting at a UTF-8 boundary.
/// Appends "... (truncated)" when truncation occurs.
fn truncate_string(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}... (truncated)", &s[..end])
}

/// Truncate a JSON value to at most `max_bytes` when serialized.
/// Objects/arrays are serialized then truncated as a string; scalars are
/// returned as-is if they fit.
fn truncate_json_value(v: &serde_json::Value, max_bytes: usize) -> serde_json::Value {
    match v {
        serde_json::Value::String(s) => serde_json::Value::String(truncate_string(s, max_bytes)),
        other => {
            let serialized = serde_json::to_string(other).unwrap_or_default();
            if serialized.len() <= max_bytes {
                other.clone()
            } else {
                serde_json::Value::String(truncate_string(&serialized, max_bytes))
            }
        }
    }
}

/// Sanitize arguments and result fields for hook payloads.
/// For sensitive tools, replaces content with a redaction notice.
/// For other tools, truncates to `MAX_PAYLOAD_FIELD_BYTES`.
fn sanitize_payload(
    tool_name: Option<&str>,
    arguments: Option<serde_json::Value>,
    result: Option<String>,
) -> (Option<serde_json::Value>, Option<String>) {
    let is_sensitive = tool_name
        .map(|n| SENSITIVE_TOOLS.contains(&n))
        .unwrap_or(false);

    let sanitized_args = arguments.map(|args| {
        if is_sensitive {
            // #2129 review round 2, finding 2: a file PATH is not a secret
            // (the file CONTENT is), and the path_filter matcher reads
            // `arguments.path` to decide whether a checker fires. Redacting
            // the whole object silently disabled every path-filtered hook
            // on write_file (the new-file case). Preserve the path-
            // identifying keys; redact everything else.
            let mut kept = serde_json::Map::new();
            if let Some(obj) = args.as_object() {
                for key in ["path", "file_path", "filename", "file"] {
                    if let Some(v @ serde_json::Value::String(_)) = obj.get(key) {
                        kept.insert(key.to_string(), v.clone());
                    }
                }
            }
            kept.insert("redacted".into(), serde_json::Value::Bool(true));
            kept.insert("reason".into(), serde_json::json!("sensitive tool"));
            serde_json::Value::Object(kept)
        } else {
            truncate_json_value(&args, MAX_PAYLOAD_FIELD_BYTES)
        }
    });

    let sanitized_result = result.map(|r| {
        if is_sensitive {
            "[redacted: sensitive tool output]".to_string()
        } else {
            truncate_string(&r, MAX_PAYLOAD_FIELD_BYTES)
        }
    });

    (sanitized_args, sanitized_result)
}

impl HookPayload {
    /// Payload for a session resume hook.
    pub fn on_resume(ctx: Option<&HookContext>) -> Self {
        let mut p = Self::empty(HookEvent::OnResume);
        p.apply_context(ctx);
        p
    }

    /// Payload for a turn-end hook.
    pub fn on_turn_end(turn_summary: impl Into<String>, ctx: Option<&HookContext>) -> Self {
        let turn_summary = truncate_string(&turn_summary.into(), MAX_PAYLOAD_FIELD_BYTES);
        let mut p = Self {
            turn_summary: Some(turn_summary),
            ..Self::empty(HookEvent::OnTurnEnd)
        };
        p.apply_context(ctx);
        p
    }

    /// Payload for a `user_prompt_submit` hook.
    ///
    /// Fires once when a real user prompt enters a turn, before the first LLM
    /// call. Carries the prompt text plus the turn's `model` and `cwd`, and
    /// the session/profile context (via `ctx`) — mirroring the request codex's
    /// UserPromptSubmit handler receives. The prompt is truncated to
    /// [`MAX_PAYLOAD_FIELD_BYTES`] to keep the hook stdin bounded, matching the
    /// other free-text payload fields.
    pub fn user_prompt_submit(
        prompt: &str,
        model: &str,
        cwd: Option<&str>,
        ctx: Option<&HookContext>,
    ) -> Self {
        let mut p = Self {
            event: HookEvent::UserPromptSubmit,
            prompt: Some(truncate_string(prompt, MAX_PAYLOAD_FIELD_BYTES)),
            model: Some(model.to_string()),
            cwd: cwd.map(str::to_string),
            ..Self::empty(HookEvent::UserPromptSubmit)
        };
        p.apply_context(ctx);
        p
    }

    /// Payload for a before-LLM-call hook.
    pub fn before_llm(
        model: &str,
        message_count: usize,
        iteration: u32,
        ctx: Option<&HookContext>,
    ) -> Self {
        let mut p = Self {
            event: HookEvent::BeforeLlmCall,
            message_count: Some(message_count),
            model: Some(model.to_string()),
            iteration: Some(iteration),
            ..Self::empty(HookEvent::BeforeLlmCall)
        };
        p.apply_context(ctx);
        p
    }

    /// Payload for an after-LLM-call hook.
    #[allow(clippy::too_many_arguments)]
    pub fn after_llm(
        model: &str,
        iteration: u32,
        stop_reason: &str,
        has_tool_calls: bool,
        input_tokens: u32,
        output_tokens: u32,
        provider_name: &str,
        latency_ms: u64,
        cumulative_input_tokens: u32,
        cumulative_output_tokens: u32,
        session_cost: Option<f64>,
        response_cost: Option<f64>,
        ctx: Option<&HookContext>,
    ) -> Self {
        let mut p = Self {
            event: HookEvent::AfterLlmCall,
            model: Some(model.to_string()),
            iteration: Some(iteration),
            stop_reason: Some(stop_reason.to_string()),
            has_tool_calls: Some(has_tool_calls),
            input_tokens: Some(input_tokens),
            output_tokens: Some(output_tokens),
            provider_name: Some(provider_name.to_string()),
            latency_ms: Some(latency_ms),
            cumulative_input_tokens: Some(cumulative_input_tokens),
            cumulative_output_tokens: Some(cumulative_output_tokens),
            session_cost,
            response_cost,
            ..Self::empty(HookEvent::AfterLlmCall)
        };
        p.apply_context(ctx);
        p
    }

    /// Payload for a before-tool-call hook.
    ///
    /// Arguments are sanitized: sensitive tools are redacted, others truncated
    /// to 1 KB to prevent secrets from leaking to hook processes.
    pub fn before_tool(
        name: &str,
        arguments: serde_json::Value,
        tool_id: &str,
        ctx: Option<&HookContext>,
    ) -> Self {
        let (sanitized_args, _) = sanitize_payload(Some(name), Some(arguments), None);
        let mut p = Self {
            event: HookEvent::BeforeToolCall,
            tool_name: Some(name.to_string()),
            arguments: sanitized_args,
            tool_id: Some(tool_id.to_string()),
            ..Self::empty(HookEvent::BeforeToolCall)
        };
        p.apply_context(ctx);
        p
    }

    /// Payload for an after-tool-call hook.
    ///
    /// Result is sanitized: sensitive tools are redacted, others truncated
    /// to 1 KB to prevent secrets from leaking to hook processes.
    #[allow(clippy::too_many_arguments)]
    pub fn after_tool(
        name: &str,
        tool_id: &str,
        result: String,
        success: bool,
        duration_ms: u64,
        arguments: Option<&serde_json::Value>,
        cwd: Option<&std::path::Path>,
        ctx: Option<&HookContext>,
    ) -> Self {
        // `arguments` is not decoration: the path-filter matcher reads
        // `arguments.path` and SKIPS any path_filter-bearing hook when it is
        // absent — an after_tool payload without arguments silently disables
        // every path-filtered after-hook (#2129 review, finding 1). `cwd` is
        // where the hook child runs; project-scoped checkers are meaningless
        // in the daemon's start directory.
        let (sanitized_args, sanitized_result) =
            sanitize_payload(Some(name), arguments.cloned(), Some(result));
        let mut p = Self {
            event: HookEvent::AfterToolCall,
            tool_name: Some(name.to_string()),
            tool_id: Some(tool_id.to_string()),
            arguments: sanitized_args,
            result: sanitized_result,
            success: Some(success),
            duration_ms: Some(duration_ms),
            cwd: cwd.map(|c| c.to_string_lossy().into_owned()),
            ..Self::empty(HookEvent::AfterToolCall)
        };
        p.apply_context(ctx);
        p
    }

    #[allow(clippy::too_many_arguments)]
    fn spawn_lifecycle(
        event: HookEvent,
        task_id: impl Into<String>,
        task_label: impl Into<String>,
        parent_session_key: impl Into<String>,
        child_session_key: impl Into<String>,
        workflow_kind: Option<impl Into<String>>,
        current_phase: Option<impl Into<String>>,
        result: Option<impl Into<String>>,
        success: Option<bool>,
        output_files: Vec<String>,
        failure_action: Option<impl Into<String>>,
        ctx: Option<&HookContext>,
    ) -> Self {
        let mut p = Self {
            event,
            task_id: Some(task_id.into()),
            task_label: Some(task_label.into()),
            parent_session_key: Some(parent_session_key.into()),
            child_session_key: Some(child_session_key.into()),
            workflow_kind: workflow_kind.map(Into::into),
            current_phase: current_phase.map(Into::into),
            result: result.map(|value| truncate_string(&value.into(), MAX_PAYLOAD_FIELD_BYTES)),
            success,
            output_files,
            failure_action: failure_action.map(Into::into),
            ..Self::empty(event)
        };
        p.apply_context(ctx);
        p
    }

    #[allow(clippy::too_many_arguments)]
    pub fn before_spawn_verify(
        task_id: impl Into<String>,
        task_label: impl Into<String>,
        parent_session_key: impl Into<String>,
        child_session_key: impl Into<String>,
        workflow_kind: Option<impl Into<String>>,
        current_phase: Option<impl Into<String>>,
        result: Option<impl Into<String>>,
        output_files: Vec<String>,
        ctx: Option<&HookContext>,
    ) -> Self {
        Self::spawn_lifecycle(
            HookEvent::BeforeSpawnVerify,
            task_id,
            task_label,
            parent_session_key,
            child_session_key,
            workflow_kind,
            current_phase,
            result,
            None,
            output_files,
            None::<String>,
            ctx,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn on_spawn_verify(
        task_id: impl Into<String>,
        task_label: impl Into<String>,
        parent_session_key: impl Into<String>,
        child_session_key: impl Into<String>,
        workflow_kind: Option<impl Into<String>>,
        current_phase: Option<impl Into<String>>,
        result: Option<impl Into<String>>,
        output_files: Vec<String>,
        ctx: Option<&HookContext>,
    ) -> Self {
        Self::spawn_lifecycle(
            HookEvent::OnSpawnVerify,
            task_id,
            task_label,
            parent_session_key,
            child_session_key,
            workflow_kind,
            current_phase,
            result,
            None,
            output_files,
            None::<String>,
            ctx,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn on_spawn_complete(
        task_id: impl Into<String>,
        task_label: impl Into<String>,
        parent_session_key: impl Into<String>,
        child_session_key: impl Into<String>,
        workflow_kind: Option<impl Into<String>>,
        current_phase: Option<impl Into<String>>,
        result: Option<impl Into<String>>,
        output_files: Vec<String>,
        ctx: Option<&HookContext>,
    ) -> Self {
        Self::spawn_lifecycle(
            HookEvent::OnSpawnComplete,
            task_id,
            task_label,
            parent_session_key,
            child_session_key,
            workflow_kind,
            current_phase,
            result,
            Some(true),
            output_files,
            None::<String>,
            ctx,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn on_spawn_failure(
        task_id: impl Into<String>,
        task_label: impl Into<String>,
        parent_session_key: impl Into<String>,
        child_session_key: impl Into<String>,
        workflow_kind: Option<impl Into<String>>,
        current_phase: Option<impl Into<String>>,
        result: impl Into<String>,
        output_files: Vec<String>,
        failure_action: impl Into<String>,
        ctx: Option<&HookContext>,
    ) -> Self {
        Self::spawn_lifecycle(
            HookEvent::OnSpawnFailure,
            task_id,
            task_label,
            parent_session_key,
            child_session_key,
            workflow_kind,
            current_phase,
            Some(result),
            Some(false),
            output_files,
            Some(failure_action),
            ctx,
        )
    }

    fn apply_context(&mut self, ctx: Option<&HookContext>) {
        if let Some(ctx) = ctx {
            self.session_id.clone_from(&ctx.session_id);
            self.profile_id.clone_from(&ctx.profile_id);
        }
    }

    fn empty(event: HookEvent) -> Self {
        Self {
            schema_version: HOOK_PAYLOAD_SCHEMA_VERSION,
            event,
            prompt: None,
            cwd: None,
            tool_name: None,
            arguments: None,
            tool_id: None,
            result: None,
            success: None,
            duration_ms: None,
            message_count: None,
            model: None,
            iteration: None,
            stop_reason: None,
            has_tool_calls: None,
            input_tokens: None,
            output_tokens: None,
            session_id: None,
            profile_id: None,
            cumulative_input_tokens: None,
            cumulative_output_tokens: None,
            session_cost: None,
            response_cost: None,
            provider_name: None,
            latency_ms: None,
            turn_summary: None,
            task_id: None,
            task_label: None,
            parent_session_key: None,
            child_session_key: None,
            workflow_kind: None,
            current_phase: None,
            output_files: Vec::new(),
            failure_action: None,
            domain_data: None,
        }
    }
}

/// Synchronous extension point for integrators to attach opaque, domain-specific
/// context to hook payloads before they are serialized to the hook process stdin.
///
/// Robotics integrators use this to attach live sensor telemetry (force/torque,
/// workspace bounds, e-stop state) that their shell-based before-hooks then
/// filter on. The core agent stays domain-agnostic: it does not introduce
/// robot-specific `HookEvent` variants.
///
/// Invariants:
/// - `enrich` runs on the Tokio runtime before payload serialization; keep it
///   cheap and non-blocking. Expensive I/O must be done off-thread ahead of time
///   and surfaced through an `Arc`-shared snapshot.
/// - The populated `HookPayload.domain_data` is subject to truncation: anything
///   whose rendered JSON exceeds `MAX_PAYLOAD_FIELD_BYTES` is replaced with
///   a `{"truncated": true}` marker object.
/// - Implementors MUST be `Send + Sync` so the executor can share them through
///   `Arc`.
pub trait HookPayloadEnricher: Send + Sync {
    /// Mutate the payload in place. Typically sets `payload.domain_data` to a
    /// JSON object describing the integrator's domain state for `event`.
    fn enrich(&self, event: &HookEvent, payload: &mut HookPayload);
}

/// Result of running hooks for an event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookResult {
    /// All hooks passed (or no hooks matched).
    Allow,
    /// A before-hook denied the operation.
    Deny(String),
    /// A before-hook modified the pending input for the event (exit code 2,
    /// stdout = replacement JSON payload).
    Modified(serde_json::Value),
    /// One or more `user_prompt_submit` hooks allowed the prompt and emitted
    /// additional context on stdout (exit code 0) to inject into the turn
    /// before the first LLM call. Holds one entry per context-emitting hook,
    /// in configuration order. Only produced for `HookEvent::UserPromptSubmit`.
    Context(Vec<String>),
    /// A hook encountered an error (does not block).
    Error(String),
    /// One or more AFTER-hooks exited 1 WITH output — the feedback channel
    /// working as designed (a checker reporting diagnostics), distinct from
    /// [`Self::Error`] infrastructure failures (missing binary, timeout,
    /// exit >= 2) which must NOT be injected into the model conversation.
    /// One entry per failing hook, in configuration order, so a failing
    /// operator after-hook cannot overwrite the coding defaults' compile
    /// errors (#2129 review, findings 8 and 9).
    Feedback(Vec<String>),
}

/// Per-session hook state: consecutive-failure counts and last-run instants,
/// each indexed parallel to `HookExecutor::hooks`.
///
/// #2153: this used to be a single `Vec<AtomicU32>` owned by the executor, so
/// an `Arc<HookExecutor>` shared across every session/workspace under a profile
/// shared ONE breaker — a genuine infra failure in one workspace counted
/// toward disabling the hook for ALL sessions. Scoping the state by session
/// key isolates both the breaker and the debounce window per session.
#[derive(Debug)]
struct SessionHookState {
    /// Consecutive failure count per hook index (the circuit-breaker counter).
    failures: Vec<u32>,
    /// When each hook index last actually ran, for the after-event debounce.
    last_run: Vec<Option<Instant>>,
}

impl SessionHookState {
    fn new(hook_count: usize) -> Self {
        Self {
            failures: vec![0; hook_count],
            last_run: vec![None; hook_count],
        }
    }
}

/// Executes hooks with circuit breaker protection.
pub struct HookExecutor {
    hooks: Vec<HookConfig>,
    /// Precompiled path-filter glob patterns per hook, parallel to `hooks`.
    /// Each inner Vec is the parsed `glob::Pattern` list for the hook's
    /// `path_filter` field. Invalid patterns are dropped at construction
    /// time so the matcher loop stays infallible. Hooks with no
    /// `path_filter` keep an empty inner Vec.
    path_filters: Vec<Vec<glob::Pattern>>,
    /// Per-session breaker + debounce state, keyed by session scope
    /// (session_id, else workspace cwd, else a shared global bucket). The
    /// executor is `Arc`-shared across sessions, so this interior-mutable map
    /// is what keeps one session's failures/throttle from leaking onto
    /// another (#2153). Entries are created lazily on first use.
    session_state: Mutex<HashMap<String, SessionHookState>>,
    failure_threshold: u32,
    /// After-event (advisory) hooks that already ran within this window for
    /// the same session are SKIPPED, so a burst of edits (e.g. several
    /// `edit_file` calls in one assistant turn) does not pay one full
    /// project `cargo check` each. `Duration::ZERO` disables it (the default);
    /// before-event deny hooks are never debounced. (#2153 finding 2.)
    after_event_debounce: Duration,
    /// Optional domain-data enricher applied to payloads before serialization.
    enricher: Option<Arc<dyn HookPayloadEnricher>>,
}

impl HookExecutor {
    pub fn new(hooks: Vec<HookConfig>) -> Self {
        Self::with_threshold(hooks, 3)
    }

    pub fn with_threshold(hooks: Vec<HookConfig>, failure_threshold: u32) -> Self {
        let path_filters = hooks
            .iter()
            .map(|hook| compile_path_filters(&hook.command, &hook.path_filter))
            .collect();
        Self {
            hooks,
            path_filters,
            session_state: Mutex::new(HashMap::new()),
            failure_threshold,
            after_event_debounce: Duration::ZERO,
            enricher: None,
        }
    }

    /// Enable after-event hook debouncing (#2153 finding 2): an advisory
    /// after-event hook that ran within `window` for the same session is
    /// skipped, coalescing a burst of edits into far fewer full project
    /// checks. `Duration::ZERO` (the default) leaves every hook running every
    /// time. Builder-style so the coding-defaults assembly can opt in while
    /// plain executors stay unthrottled.
    pub fn with_after_event_debounce(mut self, window: Duration) -> Self {
        self.after_event_debounce = window;
        self
    }

    /// The session-scope key for breaker + debounce state. Prefer the
    /// session id, fall back to the workspace cwd, and finally a shared
    /// empty-string bucket for context-free runs (e.g. unit tests). Keying
    /// on either session or workspace fixes the cross-contamination in
    /// #2153 — a flaky hook in one scope never disables it in another.
    fn session_key(payload: &HookPayload) -> String {
        payload
            .session_id
            .clone()
            .or_else(|| payload.cwd.clone())
            .unwrap_or_default()
    }

    /// Read a hook's consecutive-failure count for a session scope.
    fn breaker_load(&self, key: &str, i: usize) -> u32 {
        let map = self.session_state.lock().unwrap();
        map.get(key).map(|s| s.failures[i]).unwrap_or(0)
    }

    /// Reset a hook's failure count to zero for a session scope.
    fn breaker_reset(&self, key: &str, i: usize) {
        let mut map = self.session_state.lock().unwrap();
        if let Some(state) = map.get(key) {
            // Avoid allocating an entry just to store a zero into a
            // never-failed hook.
            if state.failures[i] == 0 {
                return;
            }
        } else {
            return;
        }
        map.get_mut(key).unwrap().failures[i] = 0;
    }

    /// Increment a hook's failure count for a session scope, returning the
    /// new value. Creates the session entry on first failure.
    fn breaker_incr(&self, key: &str, i: usize) -> u32 {
        let mut map = self.session_state.lock().unwrap();
        let state = map
            .entry(key.to_string())
            .or_insert_with(|| SessionHookState::new(self.hooks.len()));
        state.failures[i] = state.failures[i].saturating_add(1);
        state.failures[i]
    }

    /// Claim the one-shot "hook disabled" warning for a session scope: returns
    /// true exactly once, when the count first reaches the threshold, by
    /// bumping it past the threshold so later calls stay silent.
    fn breaker_claim_warning(&self, key: &str, i: usize) -> bool {
        let mut map = self.session_state.lock().unwrap();
        let Some(state) = map.get_mut(key) else {
            return false;
        };
        if state.failures[i] == self.failure_threshold {
            state.failures[i] = self.failure_threshold + 1;
            true
        } else {
            false
        }
    }

    /// Debounce gate for an after-event hook: true if it last COMPLETED within
    /// the window for this session (→ skip). The window is measured from the
    /// previous run's completion, not its start, because the check itself
    /// (a whole-project `cargo check`) can take far longer than the window —
    /// stamping at start would never coalesce a burst of edits. With
    /// sequential inline tool execution this collapses several `edit_file`
    /// calls in one assistant turn to a single check, while a later edit (a
    /// new thinking step, arriving after the window) still gets a fresh check.
    /// `Duration::ZERO` never throttles.
    fn debounce_should_skip(&self, key: &str, i: usize) -> bool {
        if self.after_event_debounce.is_zero() {
            return false;
        }
        let map = self.session_state.lock().unwrap();
        map.get(key)
            .and_then(|s| s.last_run[i])
            .is_some_and(|last| last.elapsed() < self.after_event_debounce)
    }

    /// Stamp an after-event hook's completion time for the debounce window.
    /// Called after the hook runs (any outcome) so the NEXT edit within the
    /// window is coalesced away.
    fn debounce_mark_ran(&self, key: &str, i: usize) {
        if self.after_event_debounce.is_zero() {
            return;
        }
        let mut map = self.session_state.lock().unwrap();
        let state = map
            .entry(key.to_string())
            .or_insert_with(|| SessionHookState::new(self.hooks.len()));
        state.last_run[i] = Some(Instant::now());
    }

    /// Test-only: preset a hook's failure count for a session scope (the
    /// per-session replacement for poking the old `failures[i]` atomic).
    #[cfg(test)]
    fn set_failures_for_test(&self, key: &str, i: usize, n: u32) {
        let mut map = self.session_state.lock().unwrap();
        map.entry(key.to_string())
            .or_insert_with(|| SessionHookState::new(self.hooks.len()))
            .failures[i] = n;
    }

    /// Test-only: read a hook's failure count for a session scope.
    #[cfg(test)]
    fn failures_for_test(&self, key: &str, i: usize) -> u32 {
        self.breaker_load(key, i)
    }

    /// Attach a synchronous domain-data enricher. Additive: callers that do
    /// not register an enricher see no payload change.
    pub fn with_enricher(mut self, enricher: Arc<dyn HookPayloadEnricher>) -> Self {
        self.enricher = Some(enricher);
        self
    }

    /// The hook configurations this executor runs, in run order. Lets a
    /// host MERGE executors (e.g. coding defaults + operator hooks) without
    /// forking the runner: rebuild via `HookExecutor::new` from the
    /// concatenated lists.
    pub fn configs(&self) -> &[HookConfig] {
        &self.hooks
    }

    /// Run all matching hooks for the given event sequentially.
    /// Returns `Deny` on the first before-hook that exits with 1.
    pub async fn run(&self, event: HookEvent, payload: &HookPayload) -> HookResult {
        // Apply the optional enricher before serialization so integrators can
        // attach domain-specific telemetry (force/torque, workspace bounds,
        // e-stop) that the hook script filters on.
        let payload_owned;
        let payload_ref: &HookPayload = if let Some(ref enricher) = self.enricher {
            let mut enriched = payload.clone();
            enricher.enrich(&event, &mut enriched);
            if let Some(ref data) = enriched.domain_data {
                // Truncate to MAX_PAYLOAD_FIELD_BYTES. Replace with a
                // marker object so hook scripts always receive valid JSON.
                let serialized = serde_json::to_string(data).unwrap_or_default();
                if serialized.len() > MAX_PAYLOAD_FIELD_BYTES {
                    enriched.domain_data = Some(serde_json::json!({"truncated": true}));
                }
                counter!(
                    "octos_hook_domain_data_enriched_total",
                    "event" => format!("{:?}", event)
                )
                .increment(1);
            }
            payload_owned = enriched;
            &payload_owned
        } else {
            payload
        };
        let payload_json = match serde_json::to_string(payload_ref) {
            Ok(j) => j,
            Err(e) => return HookResult::Error(format!("failed to serialize payload: {e}")),
        };

        let mut last_error = None;
        // Additional context emitted by exit-0 `user_prompt_submit` hooks
        // (one entry per hook that printed to stdout). Collected across all
        // matching hooks and returned as `HookResult::Context` at the end so
        // the caller can inject it into the turn before the first LLM call.
        let mut injected_contexts: Vec<String> = Vec::new();
        let mut feedback: Vec<String> = Vec::new();

        // #2153: breaker + debounce state is scoped to this session key so a
        // flaky hook (or a rapid edit burst) in one session never disables or
        // throttles the hook for another session sharing this Arc executor.
        let session_key = Self::session_key(payload_ref);

        for (i, hook) in self.hooks.iter().enumerate() {
            if hook.event != event {
                continue;
            }

            // Apply tool_filter for tool events
            if matches!(event, HookEvent::BeforeToolCall | HookEvent::AfterToolCall)
                && !hook.tool_filter.is_empty()
            {
                let tool_name = payload_ref.tool_name.as_deref().unwrap_or("");
                if !hook.tool_filter.iter().any(|f| f == tool_name) {
                    continue;
                }
            }

            // Apply path_filter for tool events (Audit Gap-1 closure).
            // Globs are matched against the path string from
            // `arguments.path`. If the hook declares a non-empty
            // `path_filter` and either (a) the tool's arguments do not
            // surface a `path` field or (b) no glob matches, skip the
            // hook. Falling through silently keeps the catch-all
            // (empty path_filter) at today's behaviour.
            if matches!(event, HookEvent::BeforeToolCall | HookEvent::AfterToolCall)
                && !self.path_filters[i].is_empty()
            {
                let tool_path = payload_ref.arguments.as_ref().and_then(extract_tool_path);
                let Some(tool_path) = tool_path else {
                    continue;
                };
                let candidate = std::path::Path::new(&tool_path);
                let matched = self.path_filters[i]
                    .iter()
                    .any(|pat| pat.matches_path(candidate));
                if !matched {
                    continue;
                }
            }

            // Optional binary presence gate (`requires_bin`). When a hook
            // declares a required binary, skip it if the binary is not on
            // PATH. Used by `WorkspacePolicy::for_coding` to ship eslint /
            // ruff hooks as opt-in defaults without forcing operators to
            // install every linter. The lookup uses `which::which` once
            // per call — cheap and cache-friendly via the OS path cache.
            if let Some(bin) = hook.requires_bin.as_deref()
                && !bin.is_empty()
                && which::which(bin).is_err()
            {
                continue;
            }

            // Circuit breaker: skip if too many failures for THIS session.
            let fail_count = self.breaker_load(&session_key, i);
            if fail_count >= self.failure_threshold {
                // Claim the warning (threshold -> threshold+1) so it fires once
                if self.breaker_claim_warning(&session_key, i) {
                    warn!(
                        hook_command = ?hook.command,
                        "hook disabled after {} consecutive failures",
                        self.failure_threshold
                    );
                }
                continue;
            }

            // #2153: debounce after-event (advisory) hooks — a burst of edits
            // in one turn should not pay one full project check each. A
            // before-event hook can DENY, so it is never throttled.
            if matches!(event, HookEvent::AfterToolCall)
                && self.debounce_should_skip(&session_key, i)
            {
                continue;
            }

            let hook_cwd = payload_ref.cwd.as_deref().map(std::path::Path::new);
            // #2129 review round 2, finding 5: a project-scoped checker
            // (declares `requires_bin`: cargo/eslint/ruff) is meaningless
            // without a known workspace root — running it in the daemon's
            // start directory checks an unrelated project (or exits 101 and
            // trips the breaker). Skip rather than run in the wrong place.
            if hook_cwd.is_none() && hook.requires_bin.is_some() {
                continue;
            }
            let hook_result = self.execute_hook(hook, &payload_json, hook_cwd).await;
            // #2153: stamp the debounce window at COMPLETION (any outcome) so a
            // subsequent edit within the window is coalesced. After-events only.
            if matches!(event, HookEvent::AfterToolCall) {
                self.debounce_mark_ran(&session_key, i);
            }
            match hook_result {
                Ok((0, stdout, _stderr)) => {
                    self.breaker_reset(&session_key, i);
                    // Context injection (user_prompt_submit): a hook that exits
                    // 0 and prints to stdout contributes that text as extra
                    // per-turn context. Other events ignore exit-0 stdout.
                    if event == HookEvent::UserPromptSubmit && !stdout.is_empty() {
                        injected_contexts.push(stdout);
                    }
                }
                // Exit 2 on a before-modify event = replacement payload.
                Ok((2, stdout, _stderr))
                    if matches!(
                        event,
                        HookEvent::BeforeToolCall | HookEvent::BeforeSpawnVerify
                    ) =>
                {
                    self.breaker_reset(&session_key, i);
                    match serde_json::from_str::<serde_json::Value>(&stdout) {
                        Ok(modified_args) => {
                            tracing::info!(
                                hook_command = ?hook.command,
                                ?event,
                                "hook modified event payload"
                            );
                            return HookResult::Modified(modified_args);
                        }
                        Err(e) => {
                            warn!(
                                hook_command = ?hook.command,
                                error = %e,
                                "hook exit 2 but stdout is not valid JSON, treating as error"
                            );
                            last_error = Some(format!("hook modified output not valid JSON: {e}"));
                        }
                    }
                }
                // Any OTHER nonzero exit.
                Ok((code, stdout, stderr)) => {
                    let is_before = matches!(
                        event,
                        HookEvent::UserPromptSubmit
                            | HookEvent::BeforeToolCall
                            | HookEvent::BeforeLlmCall
                            | HookEvent::BeforeSpawnVerify
                    );
                    if is_before {
                        // Before-events: exit 1 DENIES; anything else is infra.
                        if code == 1 {
                            self.breaker_reset(&session_key, i);
                            return HookResult::Deny(stdout);
                        }
                        let new_count = self.breaker_incr(&session_key, i);
                        let msg = format!(
                            "hook {:?} exited with code {} on before-event ({}/{})",
                            hook.command, code, new_count, self.failure_threshold
                        );
                        warn!("{}", msg);
                        last_error = Some(msg);
                    } else {
                        // AFTER-events: a checker reporting problems. ANY
                        // nonzero exit WITH output is FEEDBACK and does NOT
                        // count toward the breaker — cargo check exits 101
                        // on compile errors (#2129 review round 2, finding
                        // 1), eslint/ruff exit 1, tsc 1/2; a model iterating
                        // on errors legitimately fails many times in a row.
                        // Empty output on a nonzero exit is indistinguishable
                        // from a broken hook: infra error, counted.
                        let mut output = if stderr.is_empty() {
                            stdout
                        } else if stdout.is_empty() {
                            stderr
                        } else {
                            format!("{stdout}\n{stderr}")
                        };
                        octos_core::truncate_utf8(
                            &mut output,
                            2000,
                            "\n... (hook output truncated)",
                        );
                        if output.trim().is_empty() {
                            let new_count = self.breaker_incr(&session_key, i);
                            let msg = format!(
                                "hook {:?} exited with code {} and no output ({}/{})",
                                hook.command, code, new_count, self.failure_threshold
                            );
                            warn!("{}", msg);
                            last_error = Some(msg);
                        } else {
                            self.breaker_reset(&session_key, i);
                            feedback.push(format!("{:?}:\n{}", hook.command, output));
                        }
                    }
                }
                Err(e) => {
                    let new_count = self.breaker_incr(&session_key, i);
                    let msg = format!(
                        "hook {:?} failed: {} ({}/{})",
                        hook.command, e, new_count, self.failure_threshold
                    );
                    warn!("{}", msg);
                    last_error = Some(msg);
                }
            }
        }

        if !feedback.is_empty() {
            // Feedback outranks infra errors: diagnostics are actionable,
            // and infra errors are logged above either way.
            HookResult::Feedback(feedback)
        } else if let Some(err) = last_error {
            HookResult::Error(err)
        } else if !injected_contexts.is_empty() {
            HookResult::Context(injected_contexts)
        } else {
            HookResult::Allow
        }
    }

    /// Execute a single hook process in `cwd` (when given — the payload's
    /// workspace root). Returns (exit_code, stdout, stderr).
    async fn execute_hook(
        &self,
        hook: &HookConfig,
        payload_json: &str,
        cwd: Option<&std::path::Path>,
    ) -> eyre::Result<(i32, String, String)> {
        let (program, args) = hook
            .command
            .split_first()
            .ok_or_else(|| eyre::eyre!("empty hook command"))?;

        // Expand ~ to home directory in program and all arguments
        let program = expand_tilde(program);
        let expanded_args: Vec<String> = args.iter().map(|a| expand_tilde(a)).collect();

        let mut cmd = tokio::process::Command::new(&program);
        cmd.args(&expanded_args);
        if let Some(cwd) = cwd {
            cmd.current_dir(cwd);
        }
        cmd.stdin(std::process::Stdio::piped());
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());

        // Sanitize environment
        sanitize_command_env(&mut cmd, &EnvAllowlist::empty());
        for var in BLOCKED_ENV_VARS {
            cmd.env_remove(var);
        }

        let mut child = cmd.spawn()?;

        // Write payload to stdin inline (payload is small JSON, no need to spawn)
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(payload_json.as_bytes()).await;
            let _ = stdin.shutdown().await;
        }

        // Take stdout/stderr handles and read them CONCURRENTLY with the
        // wait: reading only after wait() returns deadlocks the moment the
        // child emits more than the pipe buffer (~64KB) — the child blocks
        // on write, the parent blocks in wait, and the timeout kills the
        // exact heavy-diagnostics run the feedback loop exists for (#2129
        // review, finding 3).
        let mut stdout_handle = child.stdout.take();
        let mut stderr_handle = child.stderr.take();
        let drain = async {
            let mut out_buf = Vec::new();
            let mut err_buf = Vec::new();
            let stdout_read = async {
                if let Some(handle) = stdout_handle.as_mut() {
                    let _ = handle.read_to_end(&mut out_buf).await;
                }
            };
            let stderr_read = async {
                if let Some(handle) = stderr_handle.as_mut() {
                    let _ = handle.read_to_end(&mut err_buf).await;
                }
            };
            let (status, _, _) = tokio::join!(child.wait(), stdout_read, stderr_read);
            status.map(|status| (status, out_buf, err_buf))
        };

        let timeout = Duration::from_millis(hook.timeout_ms);
        match tokio::time::timeout(timeout, drain).await {
            Ok(Ok((status, out_buf, err_buf))) => {
                let stdout = String::from_utf8_lossy(&out_buf).trim().to_string();
                // Checkers put their diagnostics on stderr (`cargo check
                // --message-format=short` writes every diagnostic there);
                // the after-event feedback path surfaces them to the model.
                // Still logged for operators.
                let stderr = String::from_utf8_lossy(&err_buf).trim().to_string();
                for line in stderr.lines() {
                    let line = line.trim();
                    if !line.is_empty() {
                        tracing::info!(
                            hook = ?hook.command,
                            "{line}"
                        );
                    }
                }
                let code = status.code().unwrap_or(2);
                tracing::info!(
                    hook = ?hook.command,
                    exit_code = code,
                    stdout_len = stdout.len(),
                    "hook executed"
                );
                Ok((code, stdout, stderr))
            }
            Ok(Err(e)) => Err(e.into()),
            Err(_) => {
                // Timeout: kill the child process to prevent orphans. The
                // message names the event and points at the fix so operators
                // can act on it directly (mirrors Claude Code's guidance).
                let _ = child.kill().await;
                Err(eyre::eyre!(
                    "{} hook timed out after {}ms — raise the hook's timeout_ms to allow more time",
                    hook.event.as_str(),
                    hook.timeout_ms
                ))
            }
        }
    }
}

/// Compile the `path_filter` globs declared on a [`HookConfig`]. Invalid
/// patterns log a warning and are dropped so the matcher stays infallible.
/// Returns an empty Vec when the input is empty (caller uses Vec::is_empty
/// as the "no filtering" predicate).
fn compile_path_filters(command: &[String], patterns: &[String]) -> Vec<glob::Pattern> {
    patterns
        .iter()
        .filter_map(|p| match glob::Pattern::new(p) {
            Ok(pat) => Some(pat),
            Err(e) => {
                warn!(
                    hook_command = ?command,
                    pattern = %p,
                    error = %e,
                    "hook path_filter pattern is invalid; dropped"
                );
                None
            }
        })
        .collect()
}

/// Extract the `path` argument from a tool's `arguments` JSON object. This
/// matches the shape used by `edit_file`, `write_file`, and `diff_edit`
/// (path at `args.path`). For tools that do not surface a path, returns
/// `None` so the caller can skip the hook.
fn extract_tool_path(args: &serde_json::Value) -> Option<String> {
    args.as_object()
        .and_then(|obj| obj.get("path"))
        .and_then(|v| v.as_str())
        .map(str::to_owned)
}

/// Expand leading `~` or `~/` to the user's home directory.
/// Also handles `~username/` by looking up `/home/username` (Unix) or
/// `/Users/username` (macOS).
fn expand_tilde(path: &str) -> String {
    if path == "~" || path.starts_with("~/") {
        if let Some(home) = dirs::home_dir() {
            return format!("{}{}", home.display(), &path[1..]);
        }
    } else if let Some(rest) = path.strip_prefix('~') {
        // ~username or ~username/...
        let (username, suffix) = match rest.find('/') {
            Some(pos) => (&rest[..pos], &rest[pos..]),
            None => (rest, ""),
        };
        // Reject usernames with path traversal or unsafe characters.
        // Only allow alphanumeric, hyphen, underscore, and dot (no leading dot).
        // This allowlist implicitly blocks path separators (/ \), null bytes,
        // and other injection characters on all platforms.
        let is_safe_username = !username.is_empty()
            && !username.starts_with('.')
            && username
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.');
        if !is_safe_username {
            warn!(
                path,
                username, "tilde expansion blocked: invalid username, returning path as-is"
            );
            return path.to_string();
        }
        #[cfg(target_os = "macos")]
        let home_base = "/Users";
        #[cfg(windows)]
        let home_base = {
            let drive = std::env::var("SYSTEMDRIVE").unwrap_or_else(|_| "C:".to_string());
            format!("{drive}\\Users")
        };
        #[cfg(not(any(target_os = "macos", windows)))]
        let home_base = "/home";
        #[cfg(windows)]
        return format!("{}\\{}{}", home_base, username, suffix);
        #[cfg(not(windows))]
        return format!("{home_base}/{username}{suffix}");
    }
    path.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hook_config_deserialize() {
        let json = r#"{
            "event": "before_tool_call",
            "command": ["python3", "~/.octos/hooks/audit.py"],
            "timeout_ms": 3000,
            "tool_filter": ["shell", "write_file"]
        }"#;
        let hook: HookConfig = serde_json::from_str(json).unwrap();
        assert_eq!(hook.event, HookEvent::BeforeToolCall);
        assert_eq!(hook.command, vec!["python3", "~/.octos/hooks/audit.py"]);
        assert_eq!(hook.timeout_ms, 3000);
        assert_eq!(hook.tool_filter, vec!["shell", "write_file"]);
    }

    #[test]
    fn test_hook_config_defaults() {
        let json = r#"{
            "event": "after_llm_call",
            "command": ["echo", "done"]
        }"#;
        let hook: HookConfig = serde_json::from_str(json).unwrap();
        assert_eq!(hook.timeout_ms, 5000);
        assert!(hook.tool_filter.is_empty());
    }

    #[test]
    fn test_payload_serialization() {
        let payload = HookPayload::before_tool(
            "shell",
            serde_json::json!({"command": "ls"}),
            "call_1",
            None,
        );
        let json = serde_json::to_string(&payload).unwrap();
        assert!(json.contains("\"event\":\"before_tool_call\""));
        assert!(json.contains("\"tool_name\":\"shell\""));
        assert!(!json.contains("\"result\""));
        assert!(!json.contains("\"success\""));
        // No context — session_id/profile_id should be absent
        assert!(!json.contains("\"session_id\""));
        assert!(!json.contains("\"profile_id\""));
    }

    #[test]
    fn should_stamp_current_schema_version_on_every_constructor() {
        let payloads = vec![
            HookPayload::before_tool("shell", serde_json::json!({}), "tc1", None),
            HookPayload::after_tool("shell", "tc1", "ok".into(), true, 10, None, None, None),
            HookPayload::before_llm("gpt-4", 0, 1, None),
            HookPayload::on_resume(None),
            HookPayload::on_turn_end("done", None),
        ];
        for p in payloads {
            assert_eq!(p.schema_version, HOOK_PAYLOAD_SCHEMA_VERSION);
        }
    }

    #[test]
    fn should_default_missing_schema_version_to_v1_on_deserialize() {
        // A payload emitted before M4.6 would have no schema_version field.
        let legacy = r#"{
            "event": "after_tool_call",
            "tool_name": "shell",
            "tool_id": "tc1",
            "success": true,
            "duration_ms": 12
        }"#;
        let parsed: HookPayload = serde_json::from_str(legacy).expect("legacy payload parses");
        assert_eq!(parsed.schema_version, HOOK_PAYLOAD_SCHEMA_VERSION);
        assert_eq!(parsed.event, HookEvent::AfterToolCall);
        assert_eq!(parsed.tool_name.as_deref(), Some("shell"));
    }

    #[test]
    fn should_include_schema_version_field_in_serialized_payload() {
        let payload =
            HookPayload::before_tool("shell", serde_json::json!({"command": "ls"}), "tc1", None);
        let json = serde_json::to_string(&payload).unwrap();
        assert!(json.contains("\"schema_version\":1"));
    }

    #[test]
    fn test_payload_constructors() {
        let before_llm = HookPayload::before_llm("gpt-4", 10, 3, None);
        assert_eq!(before_llm.event, HookEvent::BeforeLlmCall);
        assert_eq!(before_llm.model.as_deref(), Some("gpt-4"));
        assert_eq!(before_llm.message_count, Some(10));
        assert_eq!(before_llm.iteration, Some(3));
        assert!(before_llm.tool_name.is_none());
        assert!(before_llm.session_id.is_none());

        let after_llm = HookPayload::after_llm(
            "gpt-4",
            3,
            "EndTurn",
            false,
            100,
            50,
            "openai",
            1234,
            500,
            200,
            Some(0.05),
            Some(0.01),
            None,
        );
        assert_eq!(after_llm.event, HookEvent::AfterLlmCall);
        assert_eq!(after_llm.input_tokens, Some(100));
        assert_eq!(after_llm.has_tool_calls, Some(false));
        assert_eq!(after_llm.provider_name.as_deref(), Some("openai"));
        assert_eq!(after_llm.latency_ms, Some(1234));
        assert_eq!(after_llm.cumulative_input_tokens, Some(500));
        assert_eq!(after_llm.cumulative_output_tokens, Some(200));
        assert_eq!(after_llm.session_cost, Some(0.05));
        assert_eq!(after_llm.response_cost, Some(0.01));

        let after_tool =
            HookPayload::after_tool("shell", "tc1", "ok".into(), true, 42, None, None, None);
        assert_eq!(after_tool.event, HookEvent::AfterToolCall);
        assert_eq!(after_tool.success, Some(true));
        assert_eq!(after_tool.duration_ms, Some(42));

        let on_resume = HookPayload::on_resume(None);
        assert_eq!(on_resume.event, HookEvent::OnResume);
        assert!(on_resume.task_id.is_none());

        let on_turn_end = HookPayload::on_turn_end("turn finished", None);
        assert_eq!(on_turn_end.event, HookEvent::OnTurnEnd);
        assert_eq!(on_turn_end.turn_summary.as_deref(), Some("turn finished"));

        let before_spawn_verify = HookPayload::before_spawn_verify(
            "task-1",
            "Render deck",
            "parent-session",
            "child-session",
            Some("slides"),
            Some("verify_outputs"),
            Some("candidate outputs resolved"),
            vec!["deck.pdf".into()],
            None,
        );
        assert_eq!(before_spawn_verify.event, HookEvent::BeforeSpawnVerify);
        assert_eq!(before_spawn_verify.output_files, vec!["deck.pdf"]);
        assert!(before_spawn_verify.success.is_none());

        let on_spawn_verify = HookPayload::on_spawn_verify(
            "task-1",
            "Render deck",
            "parent-session",
            "child-session",
            Some("slides"),
            Some("verify"),
            Some("artifacts ready"),
            vec!["deck.pdf".into()],
            None,
        );
        assert_eq!(on_spawn_verify.event, HookEvent::OnSpawnVerify);
        assert_eq!(on_spawn_verify.task_id.as_deref(), Some("task-1"));
        assert_eq!(on_spawn_verify.output_files, vec!["deck.pdf"]);
        assert!(on_spawn_verify.success.is_none());

        let on_spawn_complete = HookPayload::on_spawn_complete(
            "task-1",
            "Render deck",
            "parent-session",
            "child-session",
            Some("slides"),
            Some("complete"),
            Some("delivered"),
            vec!["deck.pdf".into()],
            None,
        );
        assert_eq!(on_spawn_complete.event, HookEvent::OnSpawnComplete);
        assert_eq!(on_spawn_complete.success, Some(true));

        let on_spawn_failure = HookPayload::on_spawn_failure(
            "task-1",
            "Render deck",
            "parent-session",
            "child-session",
            Some("slides"),
            Some("verify"),
            "artifact missing",
            vec![],
            "retry",
            None,
        );
        assert_eq!(on_spawn_failure.event, HookEvent::OnSpawnFailure);
        assert_eq!(on_spawn_failure.success, Some(false));
        assert_eq!(on_spawn_failure.failure_action.as_deref(), Some("retry"));
    }

    #[test]
    fn test_lifecycle_payloads_truncate_large_text_fields() {
        let large = "x".repeat(MAX_PAYLOAD_FIELD_BYTES * 2);

        let turn_end = HookPayload::on_turn_end(large.clone(), None);
        assert!(
            turn_end
                .turn_summary
                .as_deref()
                .is_some_and(|value| value.ends_with("... (truncated)"))
        );

        let failure = HookPayload::on_spawn_failure(
            "task-1",
            "Render deck",
            "parent-session",
            "child-session",
            Some("slides"),
            Some("verify"),
            large,
            vec![],
            "retry",
            None,
        );
        assert!(
            failure
                .result
                .as_deref()
                .is_some_and(|value| value.ends_with("... (truncated)"))
        );
    }

    #[test]
    fn test_payload_with_hook_context() {
        let ctx = HookContext {
            session_id: Some("sess-123".into()),
            profile_id: Some("prof-abc".into()),
        };
        let payload = HookPayload::before_tool("shell", serde_json::json!({}), "tc1", Some(&ctx));
        let json = serde_json::to_string(&payload).unwrap();
        assert!(json.contains("\"session_id\":\"sess-123\""));
        assert!(json.contains("\"profile_id\":\"prof-abc\""));
    }

    #[test]
    fn test_after_llm_enriched_payload() {
        let ctx = HookContext {
            session_id: Some("s1".into()),
            profile_id: Some("p1".into()),
        };
        let payload = HookPayload::after_llm(
            "kimi-2.5",
            5,
            "ToolUse",
            true,
            200,
            80,
            "moonshot",
            3456,
            1000,
            400,
            Some(0.12),
            Some(0.03),
            Some(&ctx),
        );
        let json = serde_json::to_string(&payload).unwrap();
        assert!(json.contains("\"provider_name\":\"moonshot\""));
        assert!(json.contains("\"latency_ms\":3456"));
        assert!(json.contains("\"cumulative_input_tokens\":1000"));
        assert!(json.contains("\"cumulative_output_tokens\":400"));
        assert!(json.contains("\"session_cost\":0.12"));
        assert!(json.contains("\"response_cost\":0.03"));
        assert!(json.contains("\"session_id\":\"s1\""));
    }

    #[tokio::test]
    async fn test_circuit_breaker_tracking() {
        // A hook at the failure threshold should be skipped (not executed).
        // Use a command that would fail if actually run.
        let executor = HookExecutor::new(vec![HookConfig {
            event: HookEvent::AfterToolCall,
            command: vec!["false".into()], // would fail if executed
            timeout_ms: 1000,
            tool_filter: vec![],
            path_filter: vec![],
            requires_bin: None,
        }]);
        // Set failures at threshold so circuit breaker trips
        executor.set_failures_for_test("", 0, 3);

        let payload = HookPayload {
            schema_version: HOOK_PAYLOAD_SCHEMA_VERSION,
            event: HookEvent::AfterToolCall,
            prompt: None,
            cwd: None,
            tool_name: Some("test".into()),
            arguments: None,
            tool_id: None,
            result: None,
            success: None,
            duration_ms: None,
            message_count: None,
            model: None,
            iteration: None,
            stop_reason: None,
            has_tool_calls: None,
            input_tokens: None,
            output_tokens: None,
            session_id: None,
            profile_id: None,
            cumulative_input_tokens: None,
            cumulative_output_tokens: None,
            session_cost: None,
            response_cost: None,
            provider_name: None,
            latency_ms: None,
            turn_summary: None,
            task_id: None,
            task_label: None,
            parent_session_key: None,
            child_session_key: None,
            workflow_kind: None,
            current_phase: None,
            output_files: Vec::new(),
            failure_action: None,
            domain_data: None,
        };
        let result = executor.run(HookEvent::AfterToolCall, &payload).await;
        // Hook should be skipped (circuit broken), not denied
        assert!(matches!(result, HookResult::Allow));
    }

    #[test]
    fn test_tool_filter_config() {
        let hook = HookConfig {
            event: HookEvent::BeforeToolCall,
            command: vec!["check".into()],
            timeout_ms: 1000,
            tool_filter: vec!["shell".into(), "write_file".into()],
            path_filter: vec![],
            requires_bin: None,
        };
        assert!(hook.tool_filter.contains(&"shell".to_string()));
        assert!(!hook.tool_filter.contains(&"read_file".to_string()));
    }

    #[test]
    fn test_expand_tilde() {
        let expanded = expand_tilde("~/foo/bar");
        assert!(!expanded.starts_with('~'));
        assert!(expanded.contains("foo/bar") || expanded.contains("foo\\bar"));

        // ~username expansion
        let expanded = expand_tilde("~alice/scripts/hook.sh");
        assert!(expanded.contains("alice"));
        assert!(expanded.ends_with("/scripts/hook.sh"));
        assert!(!expanded.starts_with('~'));

        // ~username without trailing path
        let expanded = expand_tilde("~bob");
        assert!(expanded.contains("bob"));

        // Non-tilde paths unchanged
        assert_eq!(expand_tilde("/usr/bin/foo"), "/usr/bin/foo");
        assert_eq!(expand_tilde("relative/path"), "relative/path");
    }

    #[test]
    fn test_expand_tilde_rejects_traversal() {
        // Path traversal via username must return the path unexpanded
        assert_eq!(expand_tilde("~../../bin/evil"), "~../../bin/evil");
        assert_eq!(expand_tilde("~../etc/passwd"), "~../etc/passwd");
        assert_eq!(expand_tilde("~.hidden/path"), "~.hidden/path");
    }

    #[test]
    fn test_expand_tilde_rejects_unsafe_chars() {
        // Null bytes and backslashes in username are blocked by the allowlist
        assert_eq!(expand_tilde("~user\0evil"), "~user\0evil");
        assert_eq!(expand_tilde("~user\\evil"), "~user\\evil");
        assert_eq!(expand_tilde("~user:evil"), "~user:evil");
        assert_eq!(expand_tilde("~ spaces"), "~ spaces");
    }

    #[test]
    fn test_expand_tilde_allows_valid_usernames() {
        let expanded = expand_tilde("~valid-user_1/path");
        assert!(!expanded.starts_with('~'));
        assert!(expanded.contains("valid-user_1"));

        let expanded = expand_tilde("~user.name/path");
        assert!(!expanded.starts_with('~'));
        assert!(expanded.contains("user.name"));
    }

    #[tokio::test]
    async fn test_executor_no_hooks() {
        let executor = HookExecutor::new(vec![]);
        let payload = HookPayload::before_tool("shell", serde_json::json!({}), "tc1", None);
        let result = executor.run(HookEvent::BeforeToolCall, &payload).await;
        assert_eq!(result, HookResult::Allow);
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn test_executor_allow_hook() {
        let executor = HookExecutor::new(vec![HookConfig {
            event: HookEvent::BeforeToolCall,
            command: vec!["true".into()],
            timeout_ms: 5000,
            tool_filter: vec![],
            path_filter: vec![],
            requires_bin: None,
        }]);
        let payload = HookPayload::before_tool("shell", serde_json::json!({}), "tc1", None);
        let result = executor.run(HookEvent::BeforeToolCall, &payload).await;
        assert_eq!(result, HookResult::Allow);
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn test_executor_deny_hook() {
        // `false` exits with code 1
        let executor = HookExecutor::new(vec![HookConfig {
            event: HookEvent::BeforeToolCall,
            command: vec!["false".into()],
            timeout_ms: 5000,
            tool_filter: vec![],
            path_filter: vec![],
            requires_bin: None,
        }]);
        let payload = HookPayload::before_tool("shell", serde_json::json!({}), "tc1", None);
        let result = executor.run(HookEvent::BeforeToolCall, &payload).await;
        assert!(matches!(result, HookResult::Deny(_)));
    }

    #[tokio::test]
    async fn test_executor_tool_filter_skips() {
        let executor = HookExecutor::new(vec![HookConfig {
            event: HookEvent::BeforeToolCall,
            command: vec!["false".into()],
            timeout_ms: 5000,
            tool_filter: vec!["write_file".into()],
            path_filter: vec![],
            requires_bin: None,
        }]);
        let payload = HookPayload::before_tool("read_file", serde_json::json!({}), "tc1", None);
        let result = executor.run(HookEvent::BeforeToolCall, &payload).await;
        assert_eq!(result, HookResult::Allow);
    }

    #[tokio::test]
    async fn test_executor_event_mismatch_skips() {
        let executor = HookExecutor::new(vec![HookConfig {
            event: HookEvent::AfterToolCall,
            command: vec!["false".into()],
            timeout_ms: 5000,
            tool_filter: vec![],
            path_filter: vec![],
            requires_bin: None,
        }]);
        let payload = HookPayload::before_tool("shell", serde_json::json!({}), "tc1", None);
        let result = executor.run(HookEvent::BeforeToolCall, &payload).await;
        assert_eq!(result, HookResult::Allow);
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn test_circuit_breaker_below_threshold_still_runs() {
        // After-tool hook that exits with code 2 (error, not deny)
        let executor = HookExecutor::with_threshold(
            vec![HookConfig {
                event: HookEvent::AfterToolCall,
                command: vec!["sh".into(), "-c".into(), "exit 2".into()],
                timeout_ms: 5000,
                tool_filter: vec![],
                path_filter: vec![],
                requires_bin: None,
            }],
            3,
        );
        let payload =
            HookPayload::after_tool("shell", "tc1", "ok".into(), true, 10, None, None, None);

        // First two failures: hook still runs (returns Error, not Allow)
        let r1 = executor.run(HookEvent::AfterToolCall, &payload).await;
        assert!(matches!(r1, HookResult::Error(_)));
        let r2 = executor.run(HookEvent::AfterToolCall, &payload).await;
        assert!(matches!(r2, HookResult::Error(_)));
        assert_eq!(executor.failures_for_test("", 0), 2);
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn test_circuit_breaker_at_threshold_disables() {
        let executor = HookExecutor::with_threshold(
            vec![HookConfig {
                event: HookEvent::AfterToolCall,
                command: vec!["sh".into(), "-c".into(), "exit 2".into()],
                timeout_ms: 5000,
                tool_filter: vec![],
                path_filter: vec![],
                requires_bin: None,
            }],
            3,
        );
        let payload =
            HookPayload::after_tool("shell", "tc1", "ok".into(), true, 10, None, None, None);

        // Trigger 3 failures to hit threshold
        for _ in 0..3 {
            executor.run(HookEvent::AfterToolCall, &payload).await;
        }

        // Fourth call: hook is disabled (skipped), returns Allow
        let r = executor.run(HookEvent::AfterToolCall, &payload).await;
        assert_eq!(r, HookResult::Allow);
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn circuit_breaker_is_scoped_per_session_not_shared() {
        // #2153 finding 1: one Arc-shared executor must NOT let a hook's
        // failures in one session/workspace disable it for another. Two
        // payloads with distinct cwds are two distinct session scopes.
        let executor = HookExecutor::with_threshold(
            vec![HookConfig {
                event: HookEvent::AfterToolCall,
                // exit 2 with no output = infra failure that counts.
                command: vec!["sh".into(), "-c".into(), "exit 2".into()],
                timeout_ms: 5000,
                tool_filter: vec![],
                path_filter: vec![],
                requires_bin: None,
            }],
            3,
        );
        let dir_a = tempfile::tempdir().unwrap();
        let dir_b = tempfile::tempdir().unwrap();
        let key_a = dir_a.path().to_string_lossy().to_string();
        let key_b = dir_b.path().to_string_lossy().to_string();
        let payload_a = HookPayload::after_tool(
            "shell",
            "t",
            "ok".into(),
            true,
            1,
            None,
            Some(dir_a.path()),
            None,
        );
        let payload_b = HookPayload::after_tool(
            "shell",
            "t",
            "ok".into(),
            true,
            1,
            None,
            Some(dir_b.path()),
            None,
        );

        // Trip the breaker for session A only.
        for _ in 0..3 {
            executor.run(HookEvent::AfterToolCall, &payload_a).await;
        }
        // A is disabled — the 4th run is SKIPPED (Allow, not the Error the
        // hook would otherwise produce).
        assert_eq!(
            executor.run(HookEvent::AfterToolCall, &payload_a).await,
            HookResult::Allow,
            "session A's breaker must trip after its own failures"
        );
        // B shares the SAME Arc executor but its breaker is untouched: the
        // hook still RUNS (returns Error from exit 2) and B's counter is
        // independent — the pre-#2153 shared Vec would have skipped it here.
        assert!(
            matches!(
                executor.run(HookEvent::AfterToolCall, &payload_b).await,
                HookResult::Error(_)
            ),
            "session B must be unaffected by session A's tripped breaker"
        );
        assert!(executor.failures_for_test(&key_a, 0) >= 3);
        assert_eq!(executor.failures_for_test(&key_b, 0), 1);
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn after_event_debounce_coalesces_a_burst_per_session() {
        // #2153 finding 2: with a debounce window, a second after-event hook
        // within the window for the SAME session is skipped (so N rapid edits
        // pay one project check, not N) — but a different session is not.
        let executor = HookExecutor::with_threshold(
            vec![HookConfig {
                event: HookEvent::AfterToolCall,
                // nonzero WITH output => Feedback (does not count toward breaker).
                command: vec!["sh".into(), "-c".into(), "echo problem; exit 1".into()],
                timeout_ms: 5000,
                tool_filter: vec![],
                path_filter: vec![],
                requires_bin: None,
            }],
            3,
        )
        .with_after_event_debounce(std::time::Duration::from_secs(60));
        let dir_a = tempfile::tempdir().unwrap();
        let dir_b = tempfile::tempdir().unwrap();
        let payload_a = HookPayload::after_tool(
            "shell",
            "t",
            "ok".into(),
            true,
            1,
            None,
            Some(dir_a.path()),
            None,
        );
        let payload_b = HookPayload::after_tool(
            "shell",
            "t",
            "ok".into(),
            true,
            1,
            None,
            Some(dir_b.path()),
            None,
        );

        // First edit in session A: the hook runs and returns Feedback.
        assert!(
            matches!(
                executor.run(HookEvent::AfterToolCall, &payload_a).await,
                HookResult::Feedback(_)
            ),
            "the first edit runs the check"
        );
        // Second edit in A within the 60s window: coalesced away (Allow).
        assert_eq!(
            executor.run(HookEvent::AfterToolCall, &payload_a).await,
            HookResult::Allow,
            "a second edit within the debounce window is coalesced"
        );
        // A different session is NOT throttled — its first edit still runs.
        assert!(
            matches!(
                executor.run(HookEvent::AfterToolCall, &payload_b).await,
                HookResult::Feedback(_)
            ),
            "the debounce window is per-session, not global"
        );
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn test_circuit_breaker_resets_on_success() {
        let executor = HookExecutor::with_threshold(
            vec![HookConfig {
                event: HookEvent::AfterToolCall,
                command: vec!["true".into()],
                timeout_ms: 5000,
                tool_filter: vec![],
                path_filter: vec![],
                requires_bin: None,
            }],
            3,
        );

        // Simulate 2 prior failures
        executor.set_failures_for_test("", 0, 2);

        // Success resets counter
        let payload =
            HookPayload::after_tool("shell", "tc1", "ok".into(), true, 10, None, None, None);
        let r = executor.run(HookEvent::AfterToolCall, &payload).await;
        assert_eq!(r, HookResult::Allow);
        assert_eq!(executor.failures_for_test("", 0), 0);
    }

    #[test]
    fn test_truncate_string_short() {
        assert_eq!(truncate_string("hello", 1024), "hello");
    }

    #[test]
    fn test_truncate_string_long() {
        let long = "x".repeat(2000);
        let result = truncate_string(&long, 1024);
        assert!(result.len() < 1100); // 1024 + "... (truncated)"
        assert!(result.ends_with("... (truncated)"));
    }

    #[test]
    fn test_truncate_string_utf8_boundary() {
        // Multi-byte char: each is 3 bytes
        let s = "\u{4e16}\u{754c}"; // 6 bytes total
        let result = truncate_string(s, 4);
        // Should cut at char boundary (3), not at 4
        assert!(result.contains("... (truncated)"));
    }

    #[test]
    fn test_sensitive_tool_before_redacted() {
        let payload = HookPayload::before_tool(
            "shell",
            serde_json::json!({"command": "cat /etc/passwd"}),
            "tc1",
            None,
        );
        let json = serde_json::to_string(&payload).unwrap();
        assert!(json.contains("\"redacted\":true"));
        assert!(!json.contains("/etc/passwd"));
    }

    #[test]
    fn test_sensitive_tool_after_redacted() {
        let payload = HookPayload::after_tool(
            "read_file",
            "tc1",
            "SECRET_KEY=hunter2\nDB_PASS=abc".into(),
            true,
            10,
            None,
            None,
            None,
        );
        let json = serde_json::to_string(&payload).unwrap();
        assert!(json.contains("redacted"));
        assert!(!json.contains("hunter2"));
    }

    #[test]
    fn test_nonsensitive_tool_truncated_not_redacted() {
        let big_args = serde_json::json!({"data": "x".repeat(2000)});
        let payload = HookPayload::before_tool("glob", big_args, "tc1", None);
        let json = serde_json::to_string(&payload).unwrap();
        // Should be truncated, not redacted
        assert!(json.contains("truncated"));
        assert!(!json.contains("\"redacted\""));
    }

    #[test]
    fn test_nonsensitive_tool_small_payload_unchanged() {
        let payload =
            HookPayload::before_tool("glob", serde_json::json!({"pattern": "*.rs"}), "tc1", None);
        let json = serde_json::to_string(&payload).unwrap();
        assert!(json.contains("*.rs"));
        assert!(!json.contains("truncated"));
        assert!(!json.contains("redacted"));
    }

    // ----- Path filter (Audit Gap-1) tests -----

    /// Convenience constructor for path-filter tests so the cases that follow
    /// stay focused on the filtering behaviour itself.
    #[cfg(unix)]
    fn hook_with_path_filter(event: HookEvent, patterns: Vec<&str>) -> HookConfig {
        HookConfig {
            event,
            // `false` would fail if the hook actually fires — perfect for
            // "should this hook fire?" semantics.
            command: vec!["false".into()],
            timeout_ms: 5000,
            tool_filter: vec![],
            path_filter: patterns.into_iter().map(String::from).collect(),
            requires_bin: None,
        }
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn should_skip_hook_when_path_filter_does_not_match() {
        let executor = HookExecutor::new(vec![hook_with_path_filter(
            HookEvent::AfterToolCall,
            vec!["**/*.rs"],
        )]);
        // edit_file on a Python path — `**/*.rs` glob should NOT match.
        let payload =
            HookPayload::after_tool("edit_file", "tc1", "ok".into(), true, 10, None, None, None);
        let mut payload = payload;
        payload.arguments = Some(serde_json::json!({"path": "scripts/build.py"}));
        let result = executor.run(HookEvent::AfterToolCall, &payload).await;
        // Hook skipped -> Allow (not denied, not errored).
        assert_eq!(result, HookResult::Allow);
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn should_fire_hook_when_path_filter_matches() {
        // `true` would succeed; deny-via-exit-1 only makes sense for before-
        // hooks. Use a before-hook with `false` so a match yields Deny.
        let mut cfg = hook_with_path_filter(HookEvent::BeforeToolCall, vec!["**/*.rs"]);
        cfg.command = vec!["false".into()];
        let executor = HookExecutor::new(vec![cfg]);
        let mut payload = HookPayload::before_tool("edit_file", serde_json::json!({}), "tc1", None);
        payload.arguments = Some(serde_json::json!({"path": "src/lib.rs"}));
        let result = executor.run(HookEvent::BeforeToolCall, &payload).await;
        assert!(
            matches!(result, HookResult::Deny(_)),
            "matching glob should fire the hook (got {result:?})"
        );
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn should_fire_hook_when_path_filter_is_empty() {
        // Empty path_filter — fire-for-all-matching-tools (today's
        // backward-compat behaviour).
        let cfg = HookConfig {
            event: HookEvent::BeforeToolCall,
            command: vec!["false".into()],
            timeout_ms: 5000,
            tool_filter: vec![],
            path_filter: vec![], // explicit empty
            requires_bin: None,
        };
        let executor = HookExecutor::new(vec![cfg]);
        let payload = HookPayload::before_tool(
            "edit_file",
            serde_json::json!({"path": "src/lib.rs"}),
            "tc1",
            None,
        );
        let result = executor.run(HookEvent::BeforeToolCall, &payload).await;
        assert!(
            matches!(result, HookResult::Deny(_)),
            "empty path_filter should fall through to today's behaviour (got {result:?})"
        );
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn should_skip_hook_when_arguments_have_no_path_field() {
        // Tool with `path_filter` set but a tool whose arguments lack a
        // `path` key. The hook must be skipped (operator opted into
        // path-scoped filtering and there is no path to test against).
        let executor = HookExecutor::new(vec![hook_with_path_filter(
            HookEvent::BeforeToolCall,
            vec!["**/*.rs"],
        )]);
        let payload =
            HookPayload::before_tool("shell", serde_json::json!({"command": "ls"}), "tc1", None);
        let result = executor.run(HookEvent::BeforeToolCall, &payload).await;
        assert_eq!(result, HookResult::Allow);
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn should_match_any_pattern_in_path_filter() {
        // Multiple globs: hook fires if ANY matches.
        let mut cfg = hook_with_path_filter(HookEvent::BeforeToolCall, vec!["**/*.js", "**/*.ts"]);
        cfg.command = vec!["false".into()];
        let executor = HookExecutor::new(vec![cfg]);
        let mut payload = HookPayload::before_tool("edit_file", serde_json::json!({}), "tc1", None);
        payload.arguments = Some(serde_json::json!({"path": "frontend/src/app.ts"}));
        let result = executor.run(HookEvent::BeforeToolCall, &payload).await;
        assert!(
            matches!(result, HookResult::Deny(_)),
            "second glob should match .ts file (got {result:?})"
        );
    }

    #[tokio::test]
    async fn should_drop_invalid_glob_patterns_at_init() {
        // An invalid pattern should be dropped silently (logged once at
        // init) without breaking the executor. Combine with a valid
        // pattern so we can verify the valid one still works.
        let cfg = HookConfig {
            event: HookEvent::BeforeToolCall,
            command: vec!["false".into()],
            timeout_ms: 5000,
            tool_filter: vec![],
            path_filter: vec!["[unterminated".into(), "**/*.rs".into()],
            requires_bin: None,
        };
        let executor = HookExecutor::new(vec![cfg]);
        // path_filters[0] should only contain the valid pattern.
        assert_eq!(executor.path_filters[0].len(), 1);
        assert_eq!(executor.path_filters[0][0].as_str(), "**/*.rs");
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn should_combine_tool_filter_and_path_filter() {
        // tool_filter scopes by tool name; path_filter scopes by path.
        // Both must be satisfied for the hook to fire.
        let cfg = HookConfig {
            event: HookEvent::BeforeToolCall,
            command: vec!["false".into()],
            timeout_ms: 5000,
            tool_filter: vec!["edit_file".into()],
            path_filter: vec!["**/*.rs".into()],
            requires_bin: None,
        };
        let executor = HookExecutor::new(vec![cfg]);

        // Right tool, right path → hook fires.
        let mut payload = HookPayload::before_tool(
            "edit_file",
            serde_json::json!({"path": "src/lib.rs"}),
            "tc1",
            None,
        );
        payload.arguments = Some(serde_json::json!({"path": "src/lib.rs"}));
        let r = executor.run(HookEvent::BeforeToolCall, &payload).await;
        assert!(matches!(r, HookResult::Deny(_)));

        // Right tool, wrong path → skipped.
        let mut payload = HookPayload::before_tool(
            "edit_file",
            serde_json::json!({"path": "README.md"}),
            "tc1",
            None,
        );
        payload.arguments = Some(serde_json::json!({"path": "README.md"}));
        let r = executor.run(HookEvent::BeforeToolCall, &payload).await;
        assert_eq!(r, HookResult::Allow);

        // Wrong tool → skipped (regardless of path).
        let mut payload = HookPayload::before_tool(
            "write_file",
            serde_json::json!({"path": "src/lib.rs"}),
            "tc1",
            None,
        );
        payload.arguments = Some(serde_json::json!({"path": "src/lib.rs"}));
        let r = executor.run(HookEvent::BeforeToolCall, &payload).await;
        assert_eq!(r, HookResult::Allow);
    }

    #[tokio::test]
    async fn should_skip_hook_when_requires_bin_missing() {
        // Sentinel binary that should not exist on any reasonable test
        // environment. The hook must be skipped without trying to spawn
        // anything.
        let cfg = HookConfig {
            event: HookEvent::BeforeToolCall,
            command: vec!["false".into()],
            timeout_ms: 5000,
            tool_filter: vec![],
            path_filter: vec![],
            requires_bin: Some(
                "definitely-not-a-real-binary-on-this-host-octos-wave3c-test".into(),
            ),
        };
        let executor = HookExecutor::new(vec![cfg]);
        let payload = HookPayload::before_tool("edit_file", serde_json::json!({}), "tc1", None);
        let r = executor.run(HookEvent::BeforeToolCall, &payload).await;
        assert_eq!(r, HookResult::Allow);
    }

    #[test]
    fn should_deserialize_hook_config_with_path_filter() {
        let json = r#"{
            "event": "after_tool_call",
            "command": ["cargo", "check"],
            "tool_filter": ["edit_file", "write_file"],
            "path_filter": ["**/*.rs"]
        }"#;
        let hook: HookConfig = serde_json::from_str(json).unwrap();
        assert_eq!(hook.path_filter, vec!["**/*.rs"]);
        assert_eq!(hook.tool_filter.len(), 2);
        assert!(hook.requires_bin.is_none());
    }

    #[test]
    fn should_default_path_filter_to_empty_when_absent() {
        let json = r#"{
            "event": "after_tool_call",
            "command": ["echo"]
        }"#;
        let hook: HookConfig = serde_json::from_str(json).unwrap();
        assert!(hook.path_filter.is_empty());
        assert!(hook.requires_bin.is_none());
    }

    #[test]
    fn should_extract_path_from_arguments() {
        let args = serde_json::json!({"path": "src/lib.rs", "content": "..."});
        assert_eq!(extract_tool_path(&args).as_deref(), Some("src/lib.rs"));

        // Missing path key
        let args = serde_json::json!({"command": "ls"});
        assert!(extract_tool_path(&args).is_none());

        // Non-string path
        let args = serde_json::json!({"path": 42});
        assert!(extract_tool_path(&args).is_none());

        // Non-object
        let args = serde_json::json!([]);
        assert!(extract_tool_path(&args).is_none());
    }

    // ----- UserPromptSubmit hook tests -----

    /// Convenience constructor for the UserPromptSubmit tests below.
    #[cfg(unix)]
    fn user_prompt_hook(command: Vec<&str>, timeout_ms: u64) -> HookConfig {
        HookConfig {
            event: HookEvent::UserPromptSubmit,
            command: command.into_iter().map(String::from).collect(),
            timeout_ms,
            tool_filter: vec![],
            path_filter: vec![],
            requires_bin: None,
        }
    }

    #[test]
    fn should_map_user_prompt_submit_event_to_snake_case_string() {
        assert_eq!(HookEvent::UserPromptSubmit.as_str(), "user_prompt_submit");
        // serde rename matches as_str()
        let json = serde_json::to_string(&HookEvent::UserPromptSubmit).unwrap();
        assert_eq!(json, "\"user_prompt_submit\"");
        // round-trips from the config-string form
        let parsed: HookEvent = serde_json::from_str("\"user_prompt_submit\"").unwrap();
        assert_eq!(parsed, HookEvent::UserPromptSubmit);
        // as_str() agrees with serde for every variant
        for ev in [
            HookEvent::UserPromptSubmit,
            HookEvent::BeforeToolCall,
            HookEvent::AfterToolCall,
            HookEvent::BeforeLlmCall,
            HookEvent::AfterLlmCall,
            HookEvent::OnResume,
            HookEvent::OnTurnEnd,
            HookEvent::BeforeSpawnVerify,
            HookEvent::OnSpawnVerify,
            HookEvent::OnSpawnComplete,
            HookEvent::OnSpawnFailure,
        ] {
            let via_serde = serde_json::to_string(&ev).unwrap();
            assert_eq!(via_serde, format!("\"{}\"", ev.as_str()));
        }
    }

    #[test]
    fn should_deserialize_user_prompt_submit_hook_config() {
        let cfg: HookConfig =
            serde_json::from_str(r#"{"event":"user_prompt_submit","command":["true"]}"#).unwrap();
        assert_eq!(cfg.event, HookEvent::UserPromptSubmit);
        assert_eq!(cfg.timeout_ms, 5000);
    }

    #[test]
    fn should_build_user_prompt_submit_payload_with_prompt_cwd_and_model() {
        let ctx = HookContext {
            session_id: Some("sess-9".into()),
            profile_id: Some("prof-9".into()),
        };
        let payload =
            HookPayload::user_prompt_submit("hello world", "gpt-4", Some("/tmp/wd"), Some(&ctx));
        assert_eq!(payload.event, HookEvent::UserPromptSubmit);
        assert_eq!(payload.prompt.as_deref(), Some("hello world"));
        assert_eq!(payload.model.as_deref(), Some("gpt-4"));
        assert_eq!(payload.cwd.as_deref(), Some("/tmp/wd"));
        assert_eq!(payload.session_id.as_deref(), Some("sess-9"));
        assert_eq!(payload.schema_version, HOOK_PAYLOAD_SCHEMA_VERSION);

        let json = serde_json::to_string(&payload).unwrap();
        assert!(json.contains("\"event\":\"user_prompt_submit\""));
        assert!(json.contains("\"prompt\":\"hello world\""));
        assert!(json.contains("\"cwd\":\"/tmp/wd\""));
        assert!(json.contains("\"model\":\"gpt-4\""));
    }

    #[test]
    fn should_truncate_long_user_prompt_in_payload() {
        let long = "z".repeat(MAX_PAYLOAD_FIELD_BYTES * 2);
        let payload = HookPayload::user_prompt_submit(&long, "m", None, None);
        assert!(
            payload
                .prompt
                .as_deref()
                .is_some_and(|p| p.ends_with("... (truncated)"))
        );
        // cwd omitted when None
        let json = serde_json::to_string(&payload).unwrap();
        assert!(!json.contains("\"cwd\""));
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn should_deny_prompt_when_user_prompt_submit_hook_exits_1() {
        // A UserPromptSubmit hook is a BEFORE hook: exit 1 denies the prompt.
        // The hook's stdout is surfaced as the deny reason (same convention as
        // a denied tool call).
        let executor = HookExecutor::new(vec![user_prompt_hook(
            vec!["sh", "-c", "echo 'blocked: policy violation'; exit 1"],
            5000,
        )]);
        let payload = HookPayload::user_prompt_submit("do the risky thing", "m", None, None);
        let result = executor.run(HookEvent::UserPromptSubmit, &payload).await;
        match result {
            HookResult::Deny(reason) => assert_eq!(reason, "blocked: policy violation"),
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn should_inject_context_when_user_prompt_submit_hook_exits_0_with_stdout() {
        let executor = HookExecutor::new(vec![user_prompt_hook(
            vec!["sh", "-c", "echo 'git branch: main, 3 files staged'"],
            5000,
        )]);
        let payload = HookPayload::user_prompt_submit("what changed?", "m", None, None);
        let result = executor.run(HookEvent::UserPromptSubmit, &payload).await;
        match result {
            HookResult::Context(contexts) => {
                assert_eq!(
                    contexts,
                    vec!["git branch: main, 3 files staged".to_string()]
                );
            }
            other => panic!("expected Context, got {other:?}"),
        }
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn should_allow_when_user_prompt_submit_hook_exits_0_without_stdout() {
        let executor = HookExecutor::new(vec![user_prompt_hook(vec!["true"], 5000)]);
        let payload = HookPayload::user_prompt_submit("hi", "m", None, None);
        let result = executor.run(HookEvent::UserPromptSubmit, &payload).await;
        assert_eq!(result, HookResult::Allow);
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn should_collect_context_from_multiple_hooks_in_config_order() {
        let executor = HookExecutor::new(vec![
            user_prompt_hook(vec!["sh", "-c", "echo first"], 5000),
            user_prompt_hook(vec!["sh", "-c", "echo second"], 5000),
        ]);
        let payload = HookPayload::user_prompt_submit("hi", "m", None, None);
        let result = executor.run(HookEvent::UserPromptSubmit, &payload).await;
        match result {
            HookResult::Context(contexts) => {
                assert_eq!(contexts, vec!["first".to_string(), "second".to_string()]);
            }
            other => panic!("expected Context, got {other:?}"),
        }
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn should_deny_when_any_user_prompt_submit_hook_exits_1_even_after_context() {
        // First hook injects context, second denies. Deny short-circuits.
        let executor = HookExecutor::new(vec![
            user_prompt_hook(vec!["sh", "-c", "echo context-note"], 5000),
            user_prompt_hook(vec!["sh", "-c", "echo 'denied by second'; exit 1"], 5000),
        ]);
        let payload = HookPayload::user_prompt_submit("hi", "m", None, None);
        let result = executor.run(HookEvent::UserPromptSubmit, &payload).await;
        match result {
            HookResult::Deny(reason) => assert_eq!(reason, "denied by second"),
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn should_report_actionable_message_when_user_prompt_submit_hook_times_out() {
        // Timeout is fail-open (returns Error, not Deny) but the surfaced
        // message must be actionable and name the event.
        let executor = HookExecutor::new(vec![user_prompt_hook(vec!["sh", "-c", "sleep 5"], 50)]);
        let payload = HookPayload::user_prompt_submit("hi", "m", None, None);
        let result = executor.run(HookEvent::UserPromptSubmit, &payload).await;
        match result {
            HookResult::Error(msg) => {
                assert!(
                    msg.contains("user_prompt_submit hook timed out after 50ms"),
                    "message should name the event and timeout: {msg}"
                );
                assert!(
                    msg.contains("raise the hook's timeout_ms"),
                    "message should be actionable: {msg}"
                );
            }
            other => panic!("expected Error(timeout), got {other:?}"),
        }
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn should_apply_circuit_breaker_to_user_prompt_submit_hook() {
        // Generic error exit (code 3) counts as a failure; after the threshold
        // the hook is disabled and the executor returns Allow (fail-open).
        let executor = HookExecutor::with_threshold(
            vec![user_prompt_hook(vec!["sh", "-c", "exit 3"], 1000)],
            3,
        );
        let payload = HookPayload::user_prompt_submit("hi", "m", None, None);
        for _ in 0..3 {
            executor.run(HookEvent::UserPromptSubmit, &payload).await;
        }
        let result = executor.run(HookEvent::UserPromptSubmit, &payload).await;
        assert_eq!(result, HookResult::Allow);
    }

    #[tokio::test]
    async fn should_be_noop_when_no_user_prompt_submit_hook_configured() {
        // Backwards-compat: an executor with only other events never fires for
        // UserPromptSubmit.
        let executor = HookExecutor::new(vec![HookConfig {
            event: HookEvent::AfterToolCall,
            command: vec!["false".into()],
            timeout_ms: 5000,
            tool_filter: vec![],
            path_filter: vec![],
            requires_bin: None,
        }]);
        let payload = HookPayload::user_prompt_submit("hi", "m", None, None);
        let result = executor.run(HookEvent::UserPromptSubmit, &payload).await;
        assert_eq!(result, HookResult::Allow);
    }

    /// A failing AFTER-hook must carry its child's output — stdout and
    /// stderr both, since checkers (`cargo check --message-format=short`)
    /// write diagnostics to stderr. The pre-#2129 message ("exited with
    /// code 1 on after-event") fed the model nothing actionable.
    #[tokio::test]
    async fn after_hook_failure_carries_child_output() {
        let hook = HookConfig {
            event: HookEvent::AfterToolCall,
            command: vec![
                "sh".to_string(),
                "-c".to_string(),
                "echo out-line; echo err-line >&2; exit 1".to_string(),
            ],
            timeout_ms: 5000,
            tool_filter: Vec::new(),
            path_filter: Vec::new(),
            requires_bin: None,
        };
        let executor = HookExecutor::new(vec![hook]);
        let payload = HookPayload::after_tool(
            "edit_file",
            "id",
            "out".to_string(),
            true,
            5,
            None,
            None,
            None,
        );
        match executor.run(HookEvent::AfterToolCall, &payload).await {
            HookResult::Feedback(entries) => {
                let msg = entries.join("\n");
                assert!(msg.contains("out-line"), "stdout must surface: {msg}");
                assert!(msg.contains("err-line"), "stderr must surface: {msg}");
            }
            other => panic!("expected Feedback carrying output, got {other:?}"),
        }
    }

    /// #2129 review finding 1: the after_tool payload must carry the tool
    /// ARGUMENTS — the path-filter matcher reads `arguments.path` and skips
    /// path_filtered hooks without it. Regression test for the
    /// dead-on-arrival wiring.
    #[tokio::test]
    async fn path_filtered_after_hook_fires_when_arguments_carry_the_path() {
        let hook = HookConfig {
            event: HookEvent::AfterToolCall,
            command: vec![
                "sh".to_string(),
                "-c".to_string(),
                "echo diagnostics; exit 1".to_string(),
            ],
            timeout_ms: 5000,
            tool_filter: vec!["edit_file".to_string()],
            path_filter: vec!["**/*.rs".to_string()],
            requires_bin: None,
        };
        let executor = HookExecutor::new(vec![hook]);
        let args = serde_json::json!({"path": "src/lib.rs"});
        let payload = HookPayload::after_tool(
            "edit_file",
            "id",
            "ok".to_string(),
            true,
            5,
            Some(&args),
            None,
            None,
        );
        assert!(matches!(
            executor.run(HookEvent::AfterToolCall, &payload).await,
            HookResult::Feedback(_)
        ));
        let args = serde_json::json!({"path": "README.md"});
        let payload = HookPayload::after_tool(
            "edit_file",
            "id",
            "ok".to_string(),
            true,
            5,
            Some(&args),
            None,
            None,
        );
        assert!(matches!(
            executor.run(HookEvent::AfterToolCall, &payload).await,
            HookResult::Allow
        ));
    }

    /// #2129 review finding 2: repeated exit-1-with-output must NOT trip
    /// the circuit breaker — a model iterating on compile errors fails the
    /// #2129 review round 2, finding 1: cargo check exits 101 on compile
    /// errors — its diagnostics must reach the model as Feedback (not the
    /// discarded infra arm) and must NOT count toward the breaker.
    #[tokio::test]
    async fn cargo_style_101_exit_reaches_the_model_as_feedback() {
        let hook = HookConfig {
            event: HookEvent::AfterToolCall,
            command: vec![
                "sh".to_string(),
                "-c".to_string(),
                "echo 'error[E0308]: mismatched types' >&2; exit 101".to_string(),
            ],
            timeout_ms: 5000,
            tool_filter: Vec::new(),
            path_filter: Vec::new(),
            requires_bin: None,
        };
        let executor = HookExecutor::new(vec![hook]);
        for _ in 0..10 {
            let payload =
                HookPayload::after_tool("edit_file", "id", "ok".into(), true, 5, None, None, None);
            match executor.run(HookEvent::AfterToolCall, &payload).await {
                HookResult::Feedback(entries) => {
                    assert!(
                        entries.join("\n").contains("E0308"),
                        "diagnostics must surface"
                    );
                }
                other => panic!("cargo 101 must be Feedback, got {other:?}"),
            }
        }
    }

    /// #2129 review round 2, finding 2: write_file is a SENSITIVE_TOOL, but
    /// its PATH is not a secret — the path must survive redaction so a
    /// path-filtered checker still fires on new-file creation.
    #[tokio::test]
    async fn write_file_path_survives_redaction_so_checkers_fire() {
        let hook = HookConfig {
            event: HookEvent::AfterToolCall,
            command: vec![
                "sh".to_string(),
                "-c".to_string(),
                "echo diag; exit 1".to_string(),
            ],
            timeout_ms: 5000,
            tool_filter: vec!["write_file".to_string()],
            path_filter: vec!["**/*.rs".to_string()],
            requires_bin: None,
        };
        let executor = HookExecutor::new(vec![hook]);
        // write_file args carry both a path AND file content (a secret-
        // bearing field). The path must be preserved; the content redacted.
        let args = serde_json::json!({
            "path": "src/lib.rs",
            "content": "const API_KEY: &str = \"sk-secret\";"
        });
        let payload = HookPayload::after_tool(
            "write_file",
            "id",
            "ok".into(),
            true,
            5,
            Some(&args),
            None,
            None,
        );
        // The sanitized payload keeps the path, drops the content.
        let json = serde_json::to_string(&payload).unwrap();
        assert!(json.contains("src/lib.rs"), "path must survive: {json}");
        assert!(
            !json.contains("sk-secret"),
            "content must be redacted: {json}"
        );
        // And the path-filtered checker fires (matches src/lib.rs).
        assert!(matches!(
            executor.run(HookEvent::AfterToolCall, &payload).await,
            HookResult::Feedback(_)
        ));
    }

    /// #2129 review round 2, finding 5: a project-scoped checker
    /// (requires_bin) must be SKIPPED when the workspace root is unknown
    /// (cwd None) rather than run in the daemon's directory.
    #[tokio::test]
    async fn project_hook_skipped_when_workspace_root_unknown() {
        let hook = HookConfig {
            event: HookEvent::AfterToolCall,
            command: vec![
                "sh".to_string(),
                "-c".to_string(),
                "echo should-not-run; exit 1".to_string(),
            ],
            timeout_ms: 5000,
            tool_filter: Vec::new(),
            path_filter: Vec::new(),
            requires_bin: Some("sh".to_string()), // present, but cwd is None
        };
        let executor = HookExecutor::new(vec![hook]);
        let payload =
            HookPayload::after_tool("edit_file", "id", "ok".into(), true, 5, None, None, None);
        assert!(
            matches!(
                executor.run(HookEvent::AfterToolCall, &payload).await,
                HookResult::Allow
            ),
            "project hook must be skipped without a workspace root"
        );
    }

    /// check many times in a row, and that is the channel working.
    #[tokio::test]
    async fn repeated_check_failures_do_not_trip_the_breaker() {
        let hook = HookConfig {
            event: HookEvent::AfterToolCall,
            command: vec![
                "sh".to_string(),
                "-c".to_string(),
                "echo still-broken; exit 1".to_string(),
            ],
            timeout_ms: 5000,
            tool_filter: Vec::new(),
            path_filter: Vec::new(),
            requires_bin: None,
        };
        let executor = HookExecutor::new(vec![hook]);
        for i in 0..5 {
            let payload = HookPayload::after_tool(
                "edit_file",
                "id",
                "ok".to_string(),
                true,
                5,
                None,
                None,
                None,
            );
            assert!(
                matches!(
                    executor.run(HookEvent::AfterToolCall, &payload).await,
                    HookResult::Feedback(_)
                ),
                "attempt {i} must still deliver feedback (breaker must not trip)"
            );
        }
    }

    /// The hook child runs in the payload's cwd (the workspace root) — a
    /// project-scoped checker is meaningless in the daemon's start dir, and
    /// carrying cwd on the payload lets ONE profile-level executor serve
    /// every session (#2129 review, findings 4 and 7).
    ///
    /// Unix-gated: it drives `sh -c "pwd"` and matches the output against
    /// Rust's canonicalized path — on Windows Git Bash's `pwd` emits a
    /// unix-style path (`/c/...`) that never equals the `C:\...` form, so
    /// the assertion is inherently POSIX. The cwd-passing behavior is
    /// platform-independent; only this path-format check is unix-specific.
    #[cfg(unix)]
    #[tokio::test]
    async fn hook_child_runs_in_payload_cwd() {
        let tmp = tempfile::tempdir().unwrap();
        let expected = tmp.path().canonicalize().unwrap();
        let hook = HookConfig {
            event: HookEvent::AfterToolCall,
            command: vec![
                "sh".to_string(),
                "-c".to_string(),
                "pwd; exit 1".to_string(),
            ],
            timeout_ms: 5000,
            tool_filter: Vec::new(),
            path_filter: Vec::new(),
            requires_bin: None,
        };
        let executor = HookExecutor::new(vec![hook]);
        let payload = HookPayload::after_tool(
            "edit_file",
            "id",
            "out".to_string(),
            true,
            5,
            None,
            Some(tmp.path()),
            None,
        );
        match executor.run(HookEvent::AfterToolCall, &payload).await {
            HookResult::Feedback(entries) => assert!(
                entries
                    .join("\n")
                    .contains(&expected.to_string_lossy().to_string()),
                "child must run in the payload cwd; got: {entries:?}"
            ),
            other => panic!("expected Feedback, got {other:?}"),
        }
    }
}
