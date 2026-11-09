//! Hook/lifecycle system for running shell commands at agent lifecycle points.
//!
//! Supports tool, LLM, session, and background-task lifecycle events.
//! Before-hooks can deny operations (exit code 1). Circuit breaker auto-disables
//! hooks after consecutive failures. #2329: on guard-class (before-)events a
//! breaker-disabled hook surfaces `HookResult::GuardUnavailable` instead of
//! silently allowing — tool-call and spawn-verify gates fail closed, and
//! recovery is explicit via [`HookExecutor::re_arm`].

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use metrics::counter;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
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
                // Batch reads contain paths, never file contents. Retain every
                // path for path-filtered hooks; do not silently skip a denied
                // member because it was inside read_file.files.
                if let Some(files) = obj.get("files").and_then(serde_json::Value::as_array) {
                    kept.insert("files".into(), serde_json::json!(files.iter().map(|file| {
                        serde_json::json!({"path": file.get("path").or_else(|| file.get("filePath")).and_then(serde_json::Value::as_str)})
                    }).collect::<Vec<_>>()));
                }
                if let Some(path) = obj.get("filePath").and_then(serde_json::Value::as_str) {
                    kept.insert("path".into(), serde_json::json!(path));
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

/// Typed error for a before-hook policy deny that escapes as an
/// `eyre::Report` (#2249). A deny is expected policy behaviour, not a
/// harness fault — carrying a type (instead of a bare `eyre::bail!` string)
/// lets `HarnessError::classify_report` downcast it into the `policy` /
/// `expected` classification instead of `internal` / `bug`.
///
/// The `Display` wording is load-bearing: it surfaces verbatim to users as
/// the permission-denial message, and the FailFast hook-deny exclusion in
/// `loop_runner.rs` prefix-matches it.
#[derive(Debug)]
pub struct HookDeniedError {
    /// Deny reason reported by the hook (empty when the hook gave none).
    pub reason: String,
}

impl std::fmt::Display for HookDeniedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "LLM call denied by hook: {}", self.reason)
    }
}

impl std::error::Error for HookDeniedError {}

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
    /// A guard-class (before-event) hook was disabled by the circuit breaker
    /// after repeated infrastructure failures (#2329). The availability
    /// breaker must never silently strip a deny gate, so guard events surface
    /// this outcome instead of [`Self::Allow`]. Callers decide the policy:
    /// tool-call gates must fail closed (block the call with this reason);
    /// turn-level events may keep their existing surfaced-continue behavior.
    /// Recovery is explicit via [`HookExecutor::re_arm`] (or a fresh session).
    GuardUnavailable(String),
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

    /// Explicitly reset circuit-breaker failure counters — the recovery path
    /// for a tripped guard (#2329): since #2329 a guard-class event surfaces
    /// `HookResult::GuardUnavailable` (fail closed) instead of silently
    /// running without its deny gate, so recovery must be deliberate. Pass
    /// `Some(key)` to re-arm one session (the key is the same session scope
    /// the breaker uses: session id, else cwd, else `""`) or `None` to
    /// re-arm every session sharing this executor. Debounce windows are left
    /// untouched — re-arming a security breaker must not also lift the
    /// advisory run-rate limiter.
    pub fn re_arm(&self, session_key: Option<&str>) {
        // Poison-tolerant lock (no unwrap — the quality ratchet counts
        // prod-side unwraps): a panic elsewhere while holding the map must
        // not turn the recovery path itself into an unwrap.
        let mut map = self
            .session_state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match session_key {
            Some(key) => {
                map.remove(key);
            }
            None => map.clear(),
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

        // #2329: guard-class events can DENY, so a breaker-disabled hook on
        // one of them is a silently stripped security control. Track the
        // first tripped guard hook and surface `GuardUnavailable` at the end
        // (feedback/last_error still win — a fresh failure is more actionable
        // than a repeat of the trip).
        let is_guard_event = matches!(
            event,
            HookEvent::UserPromptSubmit
                | HookEvent::BeforeToolCall
                | HookEvent::BeforeLlmCall
                | HookEvent::BeforeSpawnVerify
        );
        let mut guard_disabled: Option<String> = None;

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
                let paths = payload_ref
                    .arguments
                    .as_ref()
                    .map(extract_tool_paths)
                    .unwrap_or_default();
                let matched = paths.iter().any(|path| {
                    self.path_filters[i]
                        .iter()
                        .any(|pat| pat.matches_path(std::path::Path::new(path)))
                });
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
                // #2329: for guard-class (before-)events the disabled hook is
                // a stripped deny gate, not a saved retry — surface it as
                // GuardUnavailable so callers can fail closed. After-events
                // keep the silent skip (availability for advisory checkers).
                if is_guard_event && guard_disabled.is_none() {
                    guard_disabled = Some(format!(
                        "guard hook {:?} was disabled after {} consecutive \
                         infrastructure failures; fail closed until the hook is \
                         fixed and the breaker is re-armed (`HookExecutor::re_arm`) \
                         or the session restarts",
                        hook.command, self.failure_threshold
                    ));
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
        } else if let Some(reason) = guard_disabled {
            // #2329: no fresh failure this call — the outcome IS the tripped
            // guard. Never silently degrade to Allow on a guard event.
            HookResult::GuardUnavailable(reason)
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
        //
        // R-23: each stream is capped (the pipe is still drained to EOF so
        // the child never wedges on a full pipe); overflow fails the hook
        // loudly instead of buffering the whole flood before the display
        // truncation.
        let stdout_handle = child.stdout.take();
        let stderr_handle = child.stderr.take();
        // Box the drain future: this hook path sits inside the session
        // actor's deeply nested future chain, and adding the capped-drain
        // stage inline tipped that chain over the test-thread stack limit
        // (observed as a stack overflow in session_actor tests).
        let drain = Box::pin(crate::bounded_capture::drain_child_capped(
            &mut child,
            stdout_handle,
            stderr_handle,
            crate::bounded_capture::HOOK_CAPTURE_CAP_BYTES,
        ));

        let timeout = Duration::from_millis(hook.timeout_ms);
        match tokio::time::timeout(timeout, drain).await {
            Ok(Ok((status, out_buf, out_overflow, err_buf, err_overflow))) => {
                // Exit 1 is a verdict: a noisy guard must still deny.
                if (out_overflow || err_overflow) && status.code() != Some(1) {
                    return Err(eyre::eyre!(
                        "{} hook exceeded the {}-byte {} capture cap — \
                         fix the hook to emit less output",
                        hook.event.as_str(),
                        crate::bounded_capture::HOOK_CAPTURE_CAP_BYTES,
                        if out_overflow { "stdout" } else { "stderr" }
                    ));
                }
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

fn extract_tool_paths(args: &serde_json::Value) -> Vec<String> {
    let mut paths: Vec<_> = extract_tool_path(args).into_iter().collect();
    if let Some(files) = args.get("files").and_then(serde_json::Value::as_array) {
        paths.extend(files.iter().filter_map(extract_tool_path));
    }
    paths
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
