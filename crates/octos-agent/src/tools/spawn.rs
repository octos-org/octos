//! Spawn tool for background subagent execution.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use async_trait::async_trait;
use eyre::{Result, WrapErr};
use metrics::counter;
use octos_core::{AgentId, InboundMessage, Task, TaskContext, TaskKind, TaskResult};
use octos_llm::{ContextWindowOverride, LlmProvider, ProviderRouter};
use octos_memory::EpisodeStore;
use regex::Regex;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use super::mcp_agent::{
    DispatchRequest, DispatchResponse, McpAgentBackendConfig, SharedBackend,
    build_backend_from_config, build_dispatch_event_payload, dispatch_with_metrics,
};
use super::{Tool, ToolPolicy, ToolRegistry, ToolResult};
use crate::harness_events::{HarnessEvent, HarnessEventSink, write_event_to_sink};
use crate::task_supervisor::TaskSupervisor;
use crate::workspace_git::{
    WorkspaceContractStatus, WorkspaceProjectKind,
    resolve_preferred_workspace_contract_artifact_path, resolve_workspace_contract_artifact_paths,
};
use crate::{Agent, AgentConfig, HookContext, HookExecutor, HookPayload, HookResult};

/// Default MCP tool name dispatched on the remote agent. Chosen to match
/// the `run_task` convention used by `claude mcp serve` and
/// `codex mcp serve` — configurable via
/// [`SpawnTool::with_mcp_agent_backend`] for runtimes that expose a
/// different entry point.
pub const DEFAULT_MCP_AGENT_TOOL_NAME: &str = "run_task";

/// Callback for delivering background task results directly to the session actor.
/// Returns `true` if the result was delivered, `false` if the actor is dead
/// (caller should fall back to the InboundMessage relay path).
pub type BackgroundResultSender =
    Arc<dyn Fn(BackgroundResultPayload) -> futures::future::BoxFuture<'static, bool> + Send + Sync>;

pub type ChildSessionLifecycleSender = Arc<
    dyn Fn(ChildSessionLifecyclePayload) -> futures::future::BoxFuture<'static, bool> + Send + Sync,
>;

pub type ChildToolFactory = Arc<dyn Fn() -> Arc<dyn Tool> + Send + Sync>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackgroundResultKind {
    Notification,
    Report,
}

#[derive(Debug, Clone)]
pub struct BackgroundResultPayload {
    pub task_label: String,
    pub content: String,
    pub kind: BackgroundResultKind,
    pub media: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChildSessionLifecycleKind {
    Spawned,
    Completed,
    RetryableFailed,
    TerminalFailed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChildSessionFailureAction {
    Retry,
    Escalate,
}

#[derive(Debug, Clone)]
pub struct ChildSessionLifecyclePayload {
    pub kind: ChildSessionLifecycleKind,
    pub task_id: String,
    pub task_label: String,
    pub instruction: String,
    pub parent_session_key: String,
    pub child_session_key: String,
    pub workflow_kind: Option<String>,
    pub current_phase: Option<String>,
    pub output_files: Vec<String>,
    pub failure_action: Option<ChildSessionFailureAction>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WorkflowTerminalOutputPolicy {
    deliver_final_artifact_only: bool,
    forbid_intermediate_files: bool,
    required_artifact_kind: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WorkflowMetadata {
    workflow_kind: String,
    current_phase: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    allowed_tools: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    terminal_output: Option<WorkflowTerminalOutputPolicy>,
}

fn is_retryable_child_failure(text: &str) -> bool {
    let text = text.to_ascii_lowercase();
    [
        "token budget exceeded",
        "timed out",
        "timeout",
        "temporarily",
        "retry",
        "rate limit",
        "connection reset",
        "overloaded",
        "unavailable",
    ]
    .iter()
    .any(|needle| text.contains(needle))
}

fn classify_child_session_lifecycle_kind(
    result: &Result<octos_core::TaskResult>,
) -> ChildSessionLifecycleKind {
    match result {
        Ok(task_result) if task_result.success => ChildSessionLifecycleKind::Completed,
        Ok(task_result) if is_retryable_child_failure(&task_result.output) => {
            ChildSessionLifecycleKind::RetryableFailed
        }
        Ok(_) => ChildSessionLifecycleKind::TerminalFailed,
        Err(error) if is_retryable_child_failure(&error.to_string()) => {
            ChildSessionLifecycleKind::RetryableFailed
        }
        Err(_) => ChildSessionLifecycleKind::TerminalFailed,
    }
}

fn child_session_lifecycle_kind_label(kind: ChildSessionLifecycleKind) -> &'static str {
    match kind {
        ChildSessionLifecycleKind::Spawned => "spawned",
        ChildSessionLifecycleKind::Completed => "completed",
        ChildSessionLifecycleKind::RetryableFailed => "retryable_failed",
        ChildSessionLifecycleKind::TerminalFailed => "terminal_failed",
    }
}

fn child_session_failure_action(
    kind: ChildSessionLifecycleKind,
) -> Option<ChildSessionFailureAction> {
    match kind {
        ChildSessionLifecycleKind::Spawned | ChildSessionLifecycleKind::Completed => None,
        ChildSessionLifecycleKind::RetryableFailed => Some(ChildSessionFailureAction::Retry),
        ChildSessionLifecycleKind::TerminalFailed => Some(ChildSessionFailureAction::Escalate),
    }
}

fn child_session_failure_action_label(action: ChildSessionFailureAction) -> &'static str {
    match action {
        ChildSessionFailureAction::Retry => "retry",
        ChildSessionFailureAction::Escalate => "escalate",
    }
}

fn record_child_session_lifecycle(kind: ChildSessionLifecycleKind, outcome: &'static str) {
    counter!(
        "octos_child_session_lifecycle_total",
        "kind" => child_session_lifecycle_kind_label(kind).to_string(),
        "outcome" => outcome.to_string()
    )
    .increment(1);
}

async fn dispatch_child_session_lifecycle(
    sender: Option<&ChildSessionLifecycleSender>,
    payload: ChildSessionLifecyclePayload,
) -> bool {
    match sender {
        Some(sender) => sender(payload).await,
        None => false,
    }
}

fn background_result_kind_label(kind: BackgroundResultKind) -> &'static str {
    match kind {
        BackgroundResultKind::Notification => "notification",
        BackgroundResultKind::Report => "report",
    }
}

fn record_result_delivery(path: &'static str, outcome: &'static str, kind: BackgroundResultKind) {
    counter!(
        "octos_result_delivery_total",
        "path" => path.to_string(),
        "outcome" => outcome.to_string(),
        "kind" => background_result_kind_label(kind).to_string()
    )
    .increment(1);
}

fn record_terminal_result_reason(kind: BackgroundResultKind, reason: &'static str) {
    counter!(
        "octos_terminal_result_reason_total",
        "kind" => background_result_kind_label(kind).to_string(),
        "reason" => reason.to_string()
    )
    .increment(1);
}

fn record_retry(reason: &'static str) {
    counter!("octos_retry_total", "reason" => reason.to_string()).increment(1);
}

async fn emit_lifecycle_hook(hooks: Option<&Arc<HookExecutor>>, payload: HookPayload) {
    let Some(hooks) = hooks else {
        return;
    };
    let event = payload.event;
    match hooks.run(event, &payload).await {
        HookResult::Allow => {}
        HookResult::Modified(_) => {
            warn!(event = ?event, "lifecycle hook attempted to modify payload; ignoring");
        }
        HookResult::Deny(reason) => {
            warn!(
                event = ?event,
                reason,
                "lifecycle hook attempted to deny a non-blocking event"
            );
        }
        HookResult::Error(error) => {
            warn!(event = ?event, error, "lifecycle hook failed");
        }
    }
}

fn parse_modified_spawn_verify_output_files(
    modified: serde_json::Value,
) -> std::result::Result<Vec<PathBuf>, String> {
    let files = match modified {
        serde_json::Value::Array(items) => items,
        serde_json::Value::Object(mut object) => object
            .remove("output_files")
            .and_then(|value| value.as_array().cloned())
            .ok_or_else(|| {
                "before_spawn_verify hook must return {\"output_files\": [...]} or a JSON string array"
                    .to_string()
            })?,
        _ => {
            return Err(
                "before_spawn_verify hook must return {\"output_files\": [...]} or a JSON string array"
                    .to_string(),
            )
        }
    };

    files
        .into_iter()
        .map(|value| match value {
            serde_json::Value::String(path) => Ok(PathBuf::from(path)),
            _ => Err("before_spawn_verify output_files entries must be strings".to_string()),
        })
        .collect()
}

async fn run_before_spawn_verify_hook(
    hooks: Option<&Arc<HookExecutor>>,
    payload: HookPayload,
) -> std::result::Result<Vec<PathBuf>, String> {
    let default_files = payload
        .output_files
        .iter()
        .map(PathBuf::from)
        .collect::<Vec<_>>();
    let Some(hooks) = hooks else {
        return Ok(default_files);
    };
    let event = payload.event;

    match hooks.run(event, &payload).await {
        HookResult::Allow => Ok(default_files),
        HookResult::Modified(modified) => parse_modified_spawn_verify_output_files(modified),
        HookResult::Deny(reason) => Err(reason),
        HookResult::Error(error) => {
            warn!(
                event = ?event,
                error,
                "pre-verify lifecycle hook failed; continuing with runtime output files"
            );
            Ok(default_files)
        }
    }
}

/// Tool that spawns background worker agents for long-running tasks.
pub struct SpawnTool {
    llm: Arc<dyn LlmProvider>,
    memory: Arc<EpisodeStore>,
    working_dir: PathBuf,
    inbound_tx: tokio::sync::mpsc::Sender<InboundMessage>,
    origin: std::sync::Mutex<(String, String)>,
    worker_count: AtomicU32,
    /// Inherited provider policy applied to subagent registries.
    provider_policy: Option<ToolPolicy>,
    /// Optional router for resolving prefixed model IDs to sub-providers.
    provider_router: Option<Arc<ProviderRouter>>,
    /// Default worker prompt for sub-agents (overrides compiled-in worker.txt).
    worker_prompt: Option<String>,
    /// Direct delivery channel to session actor (bypasses InboundMessage relay).
    background_result_sender: Option<BackgroundResultSender>,
    /// Optional lifecycle bridge for durable child-session state.
    child_session_sender: Option<ChildSessionLifecycleSender>,
    /// Inherited lifecycle hooks for spawned workers and background transitions.
    hooks: Option<Arc<HookExecutor>>,
    /// Template used to stamp parent/child session hook context.
    hook_context_template: Option<HookContext>,
    /// Plugin directories to load into subagent registries.
    /// Subagents can use plugin tools (fm_tts, etc.) when listed in allowed_tools.
    plugin_dirs: Vec<PathBuf>,
    /// Extra environment variables for plugin processes.
    plugin_extra_env: Vec<(String, String)>,
    /// Additional per-child tools that cannot live in octos-agent builtins.
    child_tool_factories: Vec<ChildToolFactory>,
    /// Shared task supervisor so background subagents show up in task tracking.
    task_supervisor: Option<Arc<TaskSupervisor>>,
    /// Owning session key for tracked background subagents.
    session_key: Option<String>,
    /// Append-only task ledger path for the owning parent session.
    task_ledger_path: Option<PathBuf>,
    /// Optional agent config inherited from the parent session.
    worker_config: Option<AgentConfig>,
    /// Optional MCP-backed sub-agent used when callers pick
    /// `backend == "agent_mcp"`. Parent context stays small because the
    /// sub-agent's internal messages never leak back — only the final
    /// contract-gated artifact flows through [`DispatchResponse`].
    mcp_agent_backend: Option<SharedBackend>,
    /// MCP `tools/call` name dispatched on the backend. Defaults to
    /// [`DEFAULT_MCP_AGENT_TOOL_NAME`].
    mcp_agent_tool_name: Option<String>,
    /// Cost / provenance accountant (M7.4). When present, every
    /// successful MCP sub-agent dispatch writes a
    /// [`crate::cost_ledger::CostAttributionEvent`] to the ledger.
    /// When combined with a budget policy, the dispatcher rejects
    /// spawns whose projected spend breaches the ceiling.
    cost_accountant: Option<Arc<crate::cost_ledger::CostAccountant>>,
}

impl SpawnTool {
    pub fn new(
        llm: Arc<dyn LlmProvider>,
        memory: Arc<EpisodeStore>,
        working_dir: PathBuf,
        inbound_tx: tokio::sync::mpsc::Sender<InboundMessage>,
    ) -> Self {
        Self {
            llm,
            memory,
            working_dir,
            inbound_tx,
            origin: std::sync::Mutex::new(("cli".into(), "default".into())),
            worker_count: AtomicU32::new(0),
            provider_policy: None,
            provider_router: None,
            worker_prompt: None,
            background_result_sender: None,
            child_session_sender: None,
            hooks: None,
            hook_context_template: None,
            plugin_dirs: Vec::new(),
            plugin_extra_env: Vec::new(),
            child_tool_factories: Vec::new(),
            task_supervisor: None,
            session_key: None,
            task_ledger_path: None,
            worker_config: None,
            mcp_agent_backend: None,
            mcp_agent_tool_name: None,
            cost_accountant: None,
        }
    }

    /// Create a new SpawnTool with context pre-set (for per-session instances).
    pub fn with_context(
        llm: Arc<dyn LlmProvider>,
        memory: Arc<EpisodeStore>,
        working_dir: PathBuf,
        inbound_tx: tokio::sync::mpsc::Sender<InboundMessage>,
        channel: impl Into<String>,
        chat_id: impl Into<String>,
    ) -> Self {
        Self {
            llm,
            memory,
            working_dir,
            inbound_tx,
            origin: std::sync::Mutex::new((channel.into(), chat_id.into())),
            worker_count: AtomicU32::new(0),
            provider_policy: None,
            provider_router: None,
            worker_prompt: None,
            background_result_sender: None,
            child_session_sender: None,
            hooks: None,
            hook_context_template: None,
            plugin_dirs: Vec::new(),
            plugin_extra_env: Vec::new(),
            child_tool_factories: Vec::new(),
            task_supervisor: None,
            session_key: None,
            task_ledger_path: None,
            worker_config: None,
            mcp_agent_backend: None,
            mcp_agent_tool_name: None,
            cost_accountant: None,
        }
    }

    /// Set a direct result sender that bypasses the InboundMessage relay.
    /// When set, background task results are injected as system messages
    /// into the session without triggering an extra LLM call.
    pub fn with_background_result_sender(mut self, sender: BackgroundResultSender) -> Self {
        self.background_result_sender = Some(sender);
        self
    }

    /// Set a child-session lifecycle sender for background workers.
    pub fn with_child_session_sender(mut self, sender: ChildSessionLifecycleSender) -> Self {
        self.child_session_sender = Some(sender);
        self
    }

    /// Inherit lifecycle hooks from the parent session.
    pub fn with_hooks(mut self, hooks: Arc<HookExecutor>) -> Self {
        self.hooks = Some(hooks);
        self
    }

    /// Set a hook context template for parent/child lifecycle events.
    pub fn with_hook_context(mut self, ctx: HookContext) -> Self {
        self.hook_context_template = Some(ctx);
        self
    }

    /// Inherit a provider-specific tool policy from the parent agent.
    pub fn with_provider_policy(mut self, policy: Option<ToolPolicy>) -> Self {
        self.provider_policy = policy;
        self
    }

    /// Set a provider router for multi-model sub-agent support.
    pub fn with_provider_router(mut self, router: Arc<ProviderRouter>) -> Self {
        self.provider_router = Some(router);
        self
    }

    /// Set a default worker prompt for sub-agents (overrides compiled-in worker.txt).
    pub fn with_worker_prompt(mut self, prompt: String) -> Self {
        self.worker_prompt = Some(prompt);
        self
    }

    /// Set plugin directories and env vars so subagents can use plugin tools.
    pub fn with_plugin_dirs(
        mut self,
        dirs: Vec<PathBuf>,
        extra_env: Vec<(String, String)>,
    ) -> Self {
        self.plugin_dirs = dirs;
        self.plugin_extra_env = extra_env;
        self
    }

    /// Add a factory for tools that must be instantiated per child worker.
    pub fn with_child_tool_factory(mut self, factory: ChildToolFactory) -> Self {
        self.child_tool_factories.push(factory);
        self
    }

    /// Register spawned background workers in the shared task supervisor.
    pub fn with_task_supervisor(
        mut self,
        supervisor: Arc<TaskSupervisor>,
        session_key: impl Into<String>,
        task_ledger_path: impl Into<PathBuf>,
    ) -> Self {
        self.task_supervisor = Some(supervisor);
        self.session_key = Some(session_key.into());
        self.task_ledger_path = Some(task_ledger_path.into());
        self
    }

    /// Inherit the parent agent configuration for spawned workers.
    pub fn with_agent_config(mut self, config: AgentConfig) -> Self {
        self.worker_config = Some(config);
        self
    }

    /// Configure an MCP-backed sub-agent for this tool instance. Callers
    /// that invoke spawn with `backend: "agent_mcp"` dispatch their task
    /// to `backend` and receive only the final contract-gated artifact in
    /// response — the sub-agent's intermediate messages stay inside the
    /// MCP call.
    pub fn with_mcp_agent_backend(
        mut self,
        backend: SharedBackend,
        tool_name: Option<String>,
    ) -> Self {
        self.mcp_agent_backend = Some(backend);
        self.mcp_agent_tool_name = tool_name;
        self
    }

    /// Convenience: build an MCP-backed sub-agent from typed config and
    /// wire it up as the default backend. The tool's working directory
    /// is forwarded to stdio backends as the child's cwd.
    pub fn with_mcp_agent_backend_config(
        self,
        config: &McpAgentBackendConfig,
        tool_name: Option<String>,
    ) -> Result<Self> {
        let backend = build_backend_from_config(config, Some(self.working_dir.as_path()))?;
        Ok(self.with_mcp_agent_backend(backend, tool_name))
    }

    /// Attach a cost / provenance accountant (M7.4). Every successful
    /// MCP sub-agent dispatch routed through this tool records an
    /// attribution on the accountant's ledger. If the accountant carries
    /// a [`crate::cost_ledger::CostBudgetPolicy`], pre-spawn projections
    /// reject dispatches that breach the configured ceiling.
    pub fn with_cost_accountant(
        mut self,
        accountant: Arc<crate::cost_ledger::CostAccountant>,
    ) -> Self {
        self.cost_accountant = Some(accountant);
        self
    }

    /// Dispatch a task to the configured MCP-backed sub-agent. Public so
    /// callers that want direct access (e.g. harness tests) can bypass
    /// the full spawn lifecycle. Returns the raw [`DispatchResponse`]
    /// alongside the typed harness payload the caller should emit.
    pub async fn dispatch_to_mcp_agent(
        &self,
        task: serde_json::Value,
        session_id: &str,
        task_id: &str,
        workflow: Option<&str>,
        phase: Option<&str>,
    ) -> Result<(DispatchResponse, HarnessEvent)> {
        let backend = self
            .mcp_agent_backend
            .as_ref()
            .ok_or_else(|| eyre::eyre!("no MCP agent backend configured on SpawnTool"))?;
        let tool_name = self
            .mcp_agent_tool_name
            .clone()
            .unwrap_or_else(|| DEFAULT_MCP_AGENT_TOOL_NAME.to_string());

        let request = DispatchRequest { tool_name, task };
        let (response, _summary) = dispatch_with_metrics(backend.as_ref(), request).await;
        let payload = build_dispatch_event_payload(
            session_id,
            task_id,
            workflow,
            phase,
            backend.as_ref(),
            &response,
        );
        let event = HarnessEvent {
            schema: crate::harness_events::HARNESS_EVENT_SCHEMA_V1.to_string(),
            payload,
        };
        event
            .validate()
            .map_err(|error| eyre::eyre!("dispatch event failed validation: {error}"))?;
        Ok((response, event))
    }

    /// Emit a pre-built dispatch event to the given sink. Noop when
    /// `sink_path` is `None` so callers without a supervisor still see
    /// the metrics side-effect without emitting stray events.
    pub fn emit_dispatch_event(sink_path: Option<&str>, event: &HarnessEvent) -> Result<()> {
        let Some(sink) = sink_path else {
            return Ok(());
        };
        write_event_to_sink(sink, event)
            .map_err(|error| eyre::eyre!("failed to write dispatch event to sink: {error}"))
    }

    /// Resolve the LLM provider for a sub-agent based on optional model and context_window.
    ///
    /// Context window priority: LLM-specified > config default > model native.
    fn resolve_sub_provider(
        &self,
        model: Option<&str>,
        context_window: Option<u32>,
    ) -> Result<Arc<dyn LlmProvider>> {
        let (base, default_cw): (Arc<dyn LlmProvider>, Option<u32>) =
            match (model, &self.provider_router) {
                (Some(model_key), Some(router)) => {
                    let provider = router.resolve(model_key)?;
                    // Look up default_context_window from metadata
                    let key = model_key.split_once('/').map_or(model_key, |(k, _)| k);
                    let default_cw = router
                        .list_models_with_meta()
                        .iter()
                        .find(|m| m.key == key)
                        .and_then(|m| m.default_context_window);
                    (provider, default_cw)
                }
                (Some(model_key), None) => {
                    warn!(
                        model = model_key,
                        "model specified but no provider router configured; using parent provider"
                    );
                    (self.llm.clone(), None)
                }
                _ => (self.llm.clone(), None),
            };

        // LLM-specified context_window takes priority, then config default
        let effective_cw = context_window.or(default_cw);
        match effective_cw {
            Some(cw) => Ok(Arc::new(ContextWindowOverride::new(base, cw))),
            None => Ok(base),
        }
    }

    /// Update the origin context for result delivery (called per inbound message).
    pub fn set_context(&self, channel: &str, chat_id: &str) {
        *self.origin.lock().unwrap_or_else(|e| e.into_inner()) =
            (channel.to_string(), chat_id.to_string());
    }
}

#[derive(Deserialize)]
struct Input {
    task: String,
    #[serde(default)]
    label: Option<String>,
    /// "background" (default) or "sync".
    #[serde(default = "default_mode")]
    mode: String,
    /// Tool names the subagent is allowed to use. Empty = all builtins.
    #[serde(default)]
    allowed_tools: Vec<String>,
    /// Extra context injected as a system-level prefix.
    #[serde(default)]
    context: Option<String>,
    /// Prefixed model ID (e.g. "anthropic/claude-haiku") to use a different provider.
    #[serde(default)]
    model: Option<String>,
    /// Override context window size (tokens) for the sub-agent.
    #[serde(default)]
    context_window: Option<u32>,
    /// Additional instructions appended to the subagent's system prompt.
    /// These are added after the parent's worker prompt, never replacing it.
    #[serde(default, alias = "system_prompt")]
    additional_instructions: Option<String>,
    /// Optional structured workflow metadata from the session runtime.
    #[serde(default)]
    workflow: Option<WorkflowMetadata>,
    /// Which sub-agent backend services this request. Defaults to
    /// `"builtin"` (in-process [`Agent`]). Set to `"agent_mcp"` to
    /// dispatch via the configured [`super::mcp_agent::McpAgentBackend`].
    #[serde(default = "default_backend")]
    backend: String,
    /// Optional override for the MCP tool name dispatched when
    /// `backend == "agent_mcp"`. Falls back to the SpawnTool's configured
    /// default and finally to [`DEFAULT_MCP_AGENT_TOOL_NAME`].
    #[serde(default)]
    agent_mcp_tool_name: Option<String>,
}

fn default_backend() -> String {
    "builtin".into()
}

fn default_mode() -> String {
    "background".into()
}

fn should_deliver_output_files(files: &[PathBuf]) -> bool {
    files.iter().any(|path| {
        !matches!(
            path.extension().and_then(|ext| ext.to_str()),
            Some("md" | "txt" | "json" | "csv")
        )
    })
}

fn encode_workflow_detail(workflow: &WorkflowMetadata) -> Option<String> {
    serde_json::to_string(workflow).ok()
}

fn workflow_artifact_matches_kind(path: &Path, kind: &str) -> bool {
    match kind {
        "audio" => matches!(
            path.extension()
                .and_then(|ext| ext.to_str())
                .map(|ext| ext.to_ascii_lowercase())
                .as_deref(),
            Some("mp3" | "wav" | "m4a" | "aac" | "flac" | "ogg")
        ),
        "presentation" => matches!(
            path.extension()
                .and_then(|ext| ext.to_str())
                .map(|ext| ext.to_ascii_lowercase())
                .as_deref(),
            Some("pptx" | "ppt" | "pdf")
        ),
        "site" => matches!(
            path.extension()
                .and_then(|ext| ext.to_str())
                .map(|ext| ext.to_ascii_lowercase())
                .as_deref(),
            Some("html" | "htm" | "xhtml")
        ),
        "report" => matches!(
            path.extension().and_then(|ext| ext.to_str()),
            Some("md" | "txt" | "pdf" | "html")
        ),
        _ => true,
    }
}

fn workflow_terminal_artifact_kind(workflow: Option<&WorkflowMetadata>) -> Option<&str> {
    workflow?
        .terminal_output
        .as_ref()
        .map(|policy| policy.required_artifact_kind.as_str())
        .filter(|kind| !kind.is_empty())
}

fn task_result_has_terminal_artifact_candidate(
    task_result: &TaskResult,
    workflow: Option<&WorkflowMetadata>,
) -> bool {
    let Some(required_kind) = workflow_terminal_artifact_kind(workflow) else {
        return true;
    };

    task_result
        .files_to_send
        .iter()
        .chain(task_result.files_modified.iter())
        .any(|path| workflow_artifact_matches_kind(path, required_kind))
}

fn select_preferred_terminal_output(
    files: &[PathBuf],
    required_artifact_kind: &str,
) -> Option<PathBuf> {
    files
        .iter()
        .enumerate()
        .max_by_key(|(index, path)| {
            let name = path
                .file_name()
                .and_then(|file| file.to_str())
                .unwrap_or_default()
                .to_ascii_lowercase();
            let mut score = 0_i32;
            if name.contains("final") || name.contains("full") {
                score += 20;
            }
            if required_artifact_kind == "audio" {
                if name.contains("podcast") {
                    score += 10;
                }
                if name.ends_with(".mp3") {
                    score += 5;
                }
            } else if required_artifact_kind == "presentation" {
                if name.contains("deck") {
                    score += 10;
                }
                if name.ends_with(".pptx") {
                    score += 5;
                }
            } else if required_artifact_kind == "site" {
                if name.ends_with("index.html") {
                    score += 10;
                }
                if name.contains("site") {
                    score += 5;
                }
            }
            (score, *index as i32)
        })
        .map(|(_, path)| path.clone())
}

fn select_workflow_terminal_files(
    files_to_send: &[PathBuf],
    files_modified: &[PathBuf],
    workflow: Option<&WorkflowMetadata>,
) -> Option<Vec<PathBuf>> {
    let policy = workflow?.terminal_output.as_ref()?;
    let mut candidates = if policy.forbid_intermediate_files {
        let explicit = files_to_send.to_vec();
        if explicit.is_empty() {
            files_modified.to_vec()
        } else {
            explicit
        }
    } else {
        files_to_send
            .iter()
            .chain(files_modified.iter())
            .cloned()
            .collect()
    };

    candidates.retain(|path| workflow_artifact_matches_kind(path, &policy.required_artifact_kind));

    if policy.deliver_final_artifact_only {
        return Some(
            select_preferred_terminal_output(&candidates, &policy.required_artifact_kind)
                .into_iter()
                .collect(),
        );
    }

    Some(candidates)
}

fn workflow_uses_contract_terminal_delivery(workflow: &WorkflowMetadata) -> bool {
    matches!(
        workflow
            .terminal_output
            .as_ref()
            .map(|policy| policy.required_artifact_kind.as_str()),
        Some("presentation" | "site")
    )
}

fn workflow_is_research_podcast(workflow: Option<&WorkflowMetadata>) -> bool {
    workflow.is_some_and(|workflow| workflow.workflow_kind == "research_podcast")
}

fn extract_inline_podcast_script(task_desc: &str) -> Option<String> {
    let header_re = Regex::new(r"\[[^\]\r\n]+?\s+-\s*[^\],\r\n]+,\s*[^\]\r\n]+\]").ok()?;
    let matches = header_re.find_iter(task_desc).collect::<Vec<_>>();
    if matches.len() < 2 {
        return None;
    }

    let mut script_lines = Vec::new();
    for (index, header_match) in matches.iter().enumerate() {
        let text_start = header_match.end();
        let text_end = matches
            .get(index + 1)
            .map(|next| next.start())
            .unwrap_or(task_desc.len());
        let dialogue = task_desc[text_start..text_end].trim();
        if dialogue.is_empty() {
            continue;
        }
        script_lines.push(format!(
            "{} {}",
            header_match.as_str().trim(),
            dialogue.replace('\n', " ").trim()
        ));
    }

    (script_lines.len() >= 2).then(|| script_lines.join("\n"))
}

async fn maybe_generate_inline_research_podcast(
    tools: &ToolRegistry,
    workflow: Option<&WorkflowMetadata>,
    task_desc: &str,
    task_result: &mut TaskResult,
) {
    if !workflow_is_research_podcast(workflow)
        || !task_result.success
        || task_result_has_terminal_artifact_candidate(task_result, workflow)
    {
        return;
    }

    let Some(script) = extract_inline_podcast_script(task_desc) else {
        return;
    };

    warn!(
        workflow = "research_podcast",
        "worker completed without audio; invoking podcast_generate directly from inline script"
    );
    match tools
        .execute("podcast_generate", &serde_json::json!({ "script": script }))
        .await
    {
        Ok(tool_result) if tool_result.success => {
            if let Some(path) = tool_result.file_modified.clone() {
                task_result.files_modified.push(path);
            }
            task_result
                .files_to_send
                .extend(tool_result.files_to_send.clone());
            let existing = task_result.output.trim();
            task_result.output = if existing.is_empty() {
                tool_result.output
            } else {
                format!("{existing}\n\n{}", tool_result.output)
            };
        }
        Ok(tool_result) => {
            task_result.success = false;
            task_result.output = format!(
                "research_podcast completed without audio, and direct podcast_generate failed: {}",
                tool_result.output
            );
        }
        Err(error) => {
            task_result.success = false;
            task_result.output = format!(
                "research_podcast completed without audio, and direct podcast_generate errored: {error}"
            );
        }
    }
}

fn build_subagent_tool_policy(
    allowed_tools: Vec<String>,
    workflow: Option<&WorkflowMetadata>,
) -> ToolPolicy {
    let mut deny = vec!["spawn".to_string()];
    if workflow.is_some_and(workflow_uses_contract_terminal_delivery) {
        // Contract-owned workflow families must have exactly one runtime-owned
        // terminal delivery path. Deny explicit send_file so child workers
        // cannot double-deliver slides/site artifacts.
        deny.push("send_file".to_string());
    }
    ToolPolicy {
        allow: allowed_tools,
        deny,
        ..Default::default()
    }
}

fn ensure_subagent_tools_available(
    tools: &ToolRegistry,
    allowed_tools: &[String],
) -> std::result::Result<(), String> {
    for tool_name in allowed_tools {
        tools.activate(tool_name);
    }

    let missing = allowed_tools
        .iter()
        .filter(|tool_name| tools.get(tool_name).is_none())
        .cloned()
        .collect::<Vec<_>>();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "required tool(s) not available on this host: {}",
            missing.join(", ")
        ))
    }
}

const PRIMARY_CONTRACT_ARTIFACT: &str = "primary";

fn workflow_contract_kind_label(kind: WorkspaceProjectKind) -> &'static str {
    match kind {
        WorkspaceProjectKind::Slides => "slides",
        WorkspaceProjectKind::Sites => "site",
    }
}

fn workflow_contract_project_kind(workflow: &WorkflowMetadata) -> Option<WorkspaceProjectKind> {
    match workflow
        .terminal_output
        .as_ref()
        .map(|policy| policy.required_artifact_kind.as_str())
    {
        Some("presentation") => Some(WorkspaceProjectKind::Slides),
        Some("site") => Some(WorkspaceProjectKind::Sites),
        _ => None,
    }
}

fn normalize_observed_path(base_dir: &std::path::Path, path: &std::path::Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base_dir.join(path)
    }
}

fn is_matching_workspace_root(path: &std::path::Path, expected_kind: WorkspaceProjectKind) -> bool {
    if !crate::workspace_policy_path(path).is_file() {
        return false;
    }

    matches!(
        (
            expected_kind,
            path.parent()
                .and_then(|parent| parent.file_name())
                .and_then(|name| name.to_str())
        ),
        (WorkspaceProjectKind::Slides, Some("slides"))
            | (WorkspaceProjectKind::Sites, Some("sites"))
    )
}

fn push_unique_path(paths: &mut Vec<PathBuf>, path: PathBuf) {
    if !paths.iter().any(|existing| existing == &path) {
        paths.push(path);
    }
}

fn resolve_contract_workspace_root(
    working_dir: &std::path::Path,
    files_to_send: &[PathBuf],
    files_modified: &[PathBuf],
    workflow: &WorkflowMetadata,
) -> std::result::Result<PathBuf, String> {
    let expected_kind = workflow_contract_project_kind(workflow).ok_or_else(|| {
        "workflow contract root resolution requires a contract-owned artifact kind".to_string()
    })?;
    let kind_label = workflow_contract_kind_label(expected_kind);

    let mut ancestry_candidates = Vec::new();
    for path in files_to_send.iter().chain(files_modified.iter()) {
        let observed = normalize_observed_path(working_dir, path);
        for ancestor in observed.ancestors() {
            if is_matching_workspace_root(ancestor, expected_kind) {
                push_unique_path(&mut ancestry_candidates, ancestor.to_path_buf());
                break;
            }
        }
    }

    match ancestry_candidates.as_slice() {
        [single] => return Ok(single.clone()),
        [] => {}
        _ => {
            return Err(format!(
                "multiple {kind_label} workspace contracts matched observed output paths"
            ));
        }
    }

    if is_matching_workspace_root(working_dir, expected_kind) {
        return Ok(working_dir.to_path_buf());
    }

    let matching_roots = crate::list_workspace_repos(working_dir)
        .map_err(|error| format!("workspace contract discovery failed: {error}"))?
        .into_iter()
        .filter(|repo| repo.kind == expected_kind)
        .map(|repo| repo.root)
        .collect::<Vec<_>>();

    match matching_roots.as_slice() {
        [single] => Ok(single.clone()),
        [] => Err(format!(
            "no {kind_label} workspace contract found beneath {}",
            working_dir.display()
        )),
        _ => Err(format!(
            "multiple {kind_label} workspace contracts found beneath {}; unable to choose a terminal artifact root deterministically",
            working_dir.display()
        )),
    }
}

fn resolve_background_terminal_files(
    working_dir: &std::path::Path,
    files_to_send: &[PathBuf],
    files_modified: &[PathBuf],
    workflow: Option<&WorkflowMetadata>,
) -> std::result::Result<Vec<PathBuf>, String> {
    if let Some(workflow) =
        workflow.filter(|workflow| workflow_uses_contract_terminal_delivery(workflow))
    {
        let workspace_root =
            resolve_contract_workspace_root(working_dir, files_to_send, files_modified, workflow)?;
        return resolve_contract_terminal_files(&workspace_root, Some(workflow))?
            .ok_or_else(|| "workspace contract returned no terminal files".to_string());
    }

    let terminal_files = select_workflow_terminal_files(files_to_send, files_modified, workflow)
        .unwrap_or_else(|| {
            files_to_send
                .iter()
                .chain(files_modified.iter())
                .cloned()
                .collect()
        });

    if terminal_files.is_empty() {
        if let Some(required_kind) = workflow_terminal_artifact_kind(workflow) {
            let workflow_kind = workflow
                .map(|workflow| workflow.workflow_kind.as_str())
                .unwrap_or("workflow");
            return Err(format!(
                "{workflow_kind} completed without required {required_kind} terminal artifact"
            ));
        }
    }

    Ok(terminal_files)
}

fn format_workspace_contract_failure(status: &WorkspaceContractStatus) -> String {
    let mut failures = Vec::new();
    if let Some(error) = status.error.as_deref() {
        failures.push(error.to_string());
    }
    failures.extend(
        status
            .turn_end_checks
            .iter()
            .chain(status.completion_checks.iter())
            .filter(|check| !check.passed)
            .map(|check| match check.reason.as_deref() {
                Some(reason) if !reason.is_empty() => format!("{}: {}", check.spec, reason),
                _ => format!("{}: failed", check.spec),
            }),
    );
    failures.extend(
        status
            .artifacts
            .iter()
            .filter(|artifact| !artifact.present)
            .map(|artifact| {
                format!(
                    "missing artifact '{}' matching '{}'",
                    artifact.name, artifact.pattern
                )
            }),
    );

    if failures.is_empty() {
        format!("workspace contract for {} is not ready", status.repo_label)
    } else {
        format!(
            "workspace contract for {} is not ready: {}",
            status.repo_label,
            failures.join("; ")
        )
    }
}

fn resolve_contract_terminal_files(
    workspace_root: &std::path::Path,
    workflow: Option<&WorkflowMetadata>,
) -> std::result::Result<Option<Vec<PathBuf>>, String> {
    let Some(workflow) = workflow else {
        return Ok(None);
    };
    if !workflow_uses_contract_terminal_delivery(workflow) {
        return Ok(None);
    }

    let status = crate::inspect_workspace_contract_at_root(workspace_root)
        .map_err(|error| format!("workspace contract inspection failed: {error}"))?;
    if !status.policy_managed {
        return Err(format!(
            "workspace contract missing for {}",
            status.repo_label
        ));
    }
    if !status.ready {
        return Err(format_workspace_contract_failure(&status));
    }

    let terminal_output = workflow
        .terminal_output
        .as_ref()
        .ok_or_else(|| "workflow terminal output policy missing".to_string())?;
    let mut selected = Vec::new();
    let primary_declared = status
        .artifacts
        .iter()
        .any(|artifact| artifact.name == PRIMARY_CONTRACT_ARTIFACT);
    let primary_ready = status
        .artifacts
        .iter()
        .any(|artifact| artifact.name == PRIMARY_CONTRACT_ARTIFACT && artifact.present);

    if terminal_output.deliver_final_artifact_only {
        if !primary_declared {
            return Err(format!(
                "workspace contract for {} is ready but does not declare a '{}' artifact",
                status.repo_label, PRIMARY_CONTRACT_ARTIFACT
            ));
        }

        if !primary_ready {
            return Err(format!(
                "workspace contract for {} is ready but its '{}' artifact is missing",
                status.repo_label, PRIMARY_CONTRACT_ARTIFACT
            ));
        }

        let path = resolve_preferred_workspace_contract_artifact_path(
            workspace_root,
            PRIMARY_CONTRACT_ARTIFACT,
        )
        .map_err(|error| format!("workspace contract resolution failed: {error}"))?;
        return path.map(|path| Some(vec![path])).ok_or_else(|| {
            format!(
                "workspace contract for {} is ready but the '{}' artifact could not be resolved",
                status.repo_label, PRIMARY_CONTRACT_ARTIFACT
            )
        });
    }

    for artifact in status.artifacts.iter().filter(|artifact| artifact.present) {
        selected.extend(
            resolve_workspace_contract_artifact_paths(workspace_root, &artifact.name)
                .map_err(|error| format!("workspace contract resolution failed: {error}"))?,
        );
    }

    selected.sort();
    selected.dedup();

    if !selected.is_empty() {
        return Ok(Some(selected));
    }

    Err(format!(
        "workspace contract for {} is ready but has no resolved artifact paths",
        status.repo_label
    ))
}
async fn deliver_background_result(
    sender: Option<BackgroundResultSender>,
    payload: BackgroundResultPayload,
) -> bool {
    let kind = payload.kind;
    match sender {
        Some(sender) => {
            let delivered = sender(payload).await;
            record_result_delivery(
                "direct_session_actor",
                if delivered { "accepted" } else { "unavailable" },
                kind,
            );
            delivered
        }
        None => {
            record_result_delivery("direct_session_actor", "missing_sender", kind);
            false
        }
    }
}

#[async_trait]
impl Tool for SpawnTool {
    fn name(&self) -> &str {
        "spawn"
    }

    fn description(&self) -> &str {
        "Spawn a subagent to work on a task. Use mode='sync' to wait for the result, or 'background' (default) for fire-and-forget."
    }

    fn tags(&self) -> &[&str] {
        &["gateway"]
    }

    fn input_schema(&self) -> serde_json::Value {
        // Build dynamic model field based on available sub-providers
        let model_prop = match &self.provider_router {
            Some(router) => {
                let models = router.list_models_with_meta();
                if models.is_empty() {
                    serde_json::json!({
                        "type": "string",
                        "description": "Prefixed model ID for the subagent. No sub-providers currently configured."
                    })
                } else {
                    let mut desc_parts =
                        vec!["Model key for the subagent. Available models:".to_string()];
                    let mut enum_vals = Vec::new();
                    for m in &models {
                        let mut line =
                            format!("- '{}': {} ({})", m.key, m.model_id, m.provider_name);
                        if let Some(ref cost) = m.cost_info {
                            line.push_str(&format!(", {cost}"));
                        }
                        line.push_str(&format!(", {}k max ctx", m.context_window / 1000));
                        line.push_str(&format!(", {}k max output", m.max_output_tokens / 1000));
                        if let Some(default_cw) = m.default_context_window {
                            line.push_str(&format!(", {}k default budget", default_cw / 1000));
                        }
                        if let Some(ref desc) = m.description {
                            line.push_str(&format!(". {desc}"));
                        }
                        desc_parts.push(line);
                        enum_vals.push(serde_json::Value::String(m.key.clone()));
                        enum_vals.push(serde_json::Value::String(format!(
                            "{}/{}",
                            m.key, m.model_id
                        )));
                    }
                    serde_json::json!({
                        "type": "string",
                        "description": desc_parts.join("\n"),
                        "enum": enum_vals
                    })
                }
            }
            None => serde_json::json!({
                "type": "string",
                "description": "Prefixed model ID for the subagent (e.g. 'anthropic/claude-haiku'). Requires a provider router."
            }),
        };

        serde_json::json!({
            "type": "object",
            "properties": {
                "task": {
                    "type": "string",
                    "description": "The task for the subagent to complete"
                },
                "label": {
                    "type": "string",
                    "description": "Optional short label for display"
                },
                "mode": {
                    "type": "string",
                    "enum": ["background", "sync"],
                    "description": "background: returns immediately, result announced later. sync: waits and returns the result.",
                    "default": "background"
                },
                "allowed_tools": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Tool names the subagent may use. Empty = all builtins."
                },
                "context": {
                    "type": "string",
                    "description": "Extra context prepended to the task prompt."
                },
                "model": model_prop,
                "context_window": {
                    "type": "integer",
                    "description": "Override the context window size (tokens) for the subagent."
                },
                "additional_instructions": {
                    "type": "string",
                    "description": "Extra instructions appended to the subagent's system prompt. Use to specialize behavior (e.g. 'Focus on OWASP Top 10 security issues.'). Cannot override or replace the base system prompt."
                },
                "workflow": {
                    "type": "object",
                    "description": "Optional structured workflow metadata for runtime-owned background workflows.",
                    "properties": {
                        "workflow_kind": {
                            "type": "string",
                            "description": "Stable workflow family identifier."
                        },
                        "current_phase": {
                            "type": "string",
                            "description": "Current workflow phase."
                        },
                        "allowed_tools": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "Workflow-owned tool allowlist snapshot."
                        },
                        "terminal_output": {
                            "type": "object",
                            "description": "Runtime-owned final output policy for workflow families.",
                            "properties": {
                                "deliver_final_artifact_only": { "type": "boolean" },
                                "forbid_intermediate_files": { "type": "boolean" },
                                "required_artifact_kind": { "type": "string" }
                            }
                        }
                    },
                    "required": ["workflow_kind", "current_phase"]
                },
                "backend": {
                    "type": "string",
                    "enum": ["builtin", "agent_mcp"],
                    "description": "Sub-agent backend. 'builtin' runs an in-process Agent (default). 'agent_mcp' dispatches to the configured MCP agent backend (Claude Code / Codex / hermes / jiuwenclaw) so the sub-agent's internal tool calls never leak back to the parent context.",
                    "default": "builtin"
                },
                "agent_mcp_tool_name": {
                    "type": "string",
                    "description": "Override the MCP tool name dispatched on the remote agent when backend='agent_mcp'. Defaults to 'run_task'."
                }
            },
            "required": ["task"]
        })
    }

    async fn execute(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let input: Input =
            serde_json::from_value(args.clone()).wrap_err("invalid spawn tool input")?;

        let worker_num = self.worker_count.fetch_add(1, Ordering::SeqCst);
        let worker_id = AgentId::new(format!("subagent-{worker_num}"));
        let label = input
            .label
            .unwrap_or_else(|| input.task.chars().take(60).collect());

        // Build the task prompt (optionally prepend context)
        let task_desc = match &input.context {
            Some(ctx) => format!("{ctx}\n\n{}", input.task),
            None => input.task.clone(),
        };

        let allowed_tools = input.allowed_tools.clone();
        let workflow = input.workflow.clone();
        let is_sync = input.mode == "sync";
        let is_agent_mcp = input.backend == "agent_mcp";

        info!(
            worker_id = %worker_id,
            mode = %input.mode,
            backend = %input.backend,
            task = %input.task,
            "spawning subagent"
        );

        // MCP-backed sub-agent dispatch. Runs synchronously (request /
        // response) — the sub-agent's internal tool calls stay inside the
        // MCP call; only the contract-gated artifact flows back. That's
        // the ~10x parent-context saving the M7 plan doc promises.
        if is_agent_mcp {
            let backend = self.mcp_agent_backend.as_ref().ok_or_else(|| {
                eyre::eyre!(
                    "spawn backend='agent_mcp' requires a configured MCP agent backend; \
                     use SpawnTool::with_mcp_agent_backend() to attach one"
                )
            })?;
            let tool_name = input
                .agent_mcp_tool_name
                .clone()
                .or_else(|| self.mcp_agent_tool_name.clone())
                .unwrap_or_else(|| DEFAULT_MCP_AGENT_TOOL_NAME.to_string());
            let session_key_for_event = self
                .session_key
                .clone()
                .unwrap_or_else(|| "sub-agent:unknown-session".to_string());
            let task_id_for_event = worker_id.to_string();
            let workflow_kind = workflow.as_ref().map(|w| w.workflow_kind.clone());
            let workflow_phase = workflow.as_ref().map(|w| w.current_phase.clone());

            let dispatch_payload = serde_json::json!({
                "task": task_desc,
                "label": label,
                "allowed_tools": allowed_tools,
                "workflow": workflow.clone(),
                "additional_instructions": input.additional_instructions,
            });

            // Pre-dispatch budget projection (M7.4). Absent a
            // configured accountant the check short-circuits and the
            // dispatch proceeds unchanged — this keeps existing M7.1
            // dispatch tests passing when no policy is configured.
            let model_for_ledger = input
                .model
                .clone()
                .unwrap_or_else(|| "unknown-model".to_string());
            let contract_id_for_ledger = workflow_kind
                .clone()
                .unwrap_or_else(|| session_key_for_event.clone());
            if let Some(accountant) = self.cost_accountant.as_ref() {
                if let Some(policy) = accountant.policy() {
                    if policy.is_enforced() {
                        // Pre-spawn estimate: tokens_in ≈ UTF-8 length of
                        // the outbound task description divided by 4
                        // (the classic 1 token ≈ 4 chars rule of thumb).
                        // Good enough for budget rejection — the ledger
                        // replaces this with the real count on success.
                        let tokens_in_estimate = task_desc.len().div_ceil(4) as u32;
                        let projected_usd = crate::cost_ledger::project_cost_usd(
                            &model_for_ledger,
                            tokens_in_estimate,
                            0,
                        )
                        .unwrap_or(0.0);
                        match accountant
                            .project_dispatch(&contract_id_for_ledger, projected_usd)
                            .await
                        {
                            Ok(crate::cost_ledger::BudgetProjection::Allowed { .. }) => {}
                            Ok(crate::cost_ledger::BudgetProjection::Rejected {
                                reason, ..
                            }) => {
                                let message = format!(
                                    "Status: FAILED\nDispatch rejected by cost budget policy: {reason}"
                                );
                                warn!(
                                    contract_id = %contract_id_for_ledger,
                                    reason = %reason,
                                    "rejecting MCP sub-agent dispatch before spawn"
                                );
                                return Ok(ToolResult {
                                    output: message,
                                    success: false,
                                    ..Default::default()
                                });
                            }
                            Err(error) => {
                                warn!(
                                    error = %error,
                                    "cost budget projection failed; allowing dispatch"
                                );
                            }
                        }
                    }
                }
            }

            let (response, event) = {
                let request = DispatchRequest {
                    tool_name,
                    task: dispatch_payload,
                };
                let (response, _summary) = dispatch_with_metrics(backend.as_ref(), request).await;
                let payload = build_dispatch_event_payload(
                    session_key_for_event.clone(),
                    task_id_for_event.clone(),
                    workflow_kind.as_deref(),
                    workflow_phase.as_deref(),
                    backend.as_ref(),
                    &response,
                );
                let event = HarnessEvent {
                    schema: crate::harness_events::HARNESS_EVENT_SCHEMA_V1.to_string(),
                    payload,
                };
                event.validate().map_err(|error| {
                    eyre::eyre!("sub-agent dispatch event failed validation: {error}")
                })?;
                (response, event)
            };

            if let Some(supervisor) = self.task_supervisor.as_ref() {
                if let Err(error) = supervisor.apply_harness_event(&task_id_for_event, &event) {
                    // The dispatch event is observational; absence of a
                    // tracked task is not a dispatch failure. Log and
                    // continue.
                    warn!(
                        task_id = %task_id_for_event,
                        error = %error,
                        "dispatch event could not be applied to task supervisor"
                    );
                }
            }

            let success = response.outcome == super::mcp_agent::DispatchOutcome::Success;

            // Post-dispatch cost attribution (M7.4). Only record when
            // the remote agent returned a ready artifact; failures and
            // timeouts are already visible via the dispatch event and
            // should not inflate the ledger.
            if success {
                if let Some(accountant) = self.cost_accountant.as_ref() {
                    let tokens_in_est = task_desc.len().div_ceil(4) as u32;
                    let tokens_out_est = response.output.len().div_ceil(4) as u32;
                    let cost_usd = crate::cost_ledger::project_cost_usd(
                        &model_for_ledger,
                        tokens_in_est,
                        tokens_out_est,
                    )
                    .unwrap_or(0.0);
                    let attribution = crate::cost_ledger::CostAttributionEvent::new(
                        session_key_for_event.clone(),
                        contract_id_for_ledger.clone(),
                        task_id_for_event.clone(),
                        model_for_ledger.clone(),
                        tokens_in_est,
                        tokens_out_est,
                        cost_usd,
                    )
                    .with_workflow(workflow_kind.clone(), workflow_phase.clone())
                    .with_backend_outcome(
                        Some(backend.as_ref().backend_label().to_string()),
                        Some("success".to_string()),
                    );
                    let attribution_id_for_event = attribution.attribution_id.clone();

                    // Commit to the ledger. Failing to persist is
                    // non-fatal for the dispatch itself — we log and
                    // continue so a bad disk does not mask a successful
                    // agent run.
                    if let Err(error) = accountant.ledger().record(attribution).await {
                        warn!(
                            task_id = %task_id_for_event,
                            error = %error,
                            "failed to persist cost attribution; dispatch succeeded"
                        );
                    } else {
                        // Emit the typed event so downstream sinks,
                        // including the operator summary aggregator,
                        // see the spend even without re-reading the
                        // ledger.
                        let cost_event = HarnessEvent::cost_attribution(
                            crate::harness_events::HarnessCostAttributionEvent {
                                schema_version: crate::abi_schema::COST_ATTRIBUTION_SCHEMA_VERSION,
                                session_id: session_key_for_event.clone(),
                                task_id: task_id_for_event.clone(),
                                workflow: workflow_kind.clone(),
                                phase: workflow_phase.clone(),
                                attribution_id: attribution_id_for_event,
                                contract_id: contract_id_for_ledger.clone(),
                                model: model_for_ledger.clone(),
                                tokens_in: tokens_in_est,
                                tokens_out: tokens_out_est,
                                cost_usd,
                                outcome: "success".to_string(),
                                extra: std::collections::HashMap::new(),
                            },
                        );
                        if let Err(error) = cost_event.validate() {
                            warn!(
                                task_id = %task_id_for_event,
                                error = %error,
                                "cost attribution event failed validation; skipping emission"
                            );
                        } else if let Some(supervisor) = self.task_supervisor.as_ref() {
                            if let Err(error) =
                                supervisor.apply_harness_event(&task_id_for_event, &cost_event)
                            {
                                warn!(
                                    task_id = %task_id_for_event,
                                    error = %error,
                                    "cost attribution event could not be applied"
                                );
                            }
                        }
                    }
                }
            }

            let mut files_to_send = response.files_to_send.clone();
            // Workflow contract families always gate outputs through the
            // workspace contract. The dispatch response is advisory; the
            // final delivery path remains owned by the runtime.
            if let Some(workflow_meta) = workflow.as_ref() {
                if workflow_uses_contract_terminal_delivery(workflow_meta) {
                    match resolve_contract_terminal_files(
                        self.working_dir.as_path(),
                        Some(workflow_meta),
                    ) {
                        Ok(Some(contract_files)) => files_to_send = contract_files,
                        Ok(None) => {}
                        Err(error) => {
                            return Ok(ToolResult {
                                output: format!("Status: FAILED\n{error}"),
                                success: false,
                                ..Default::default()
                            });
                        }
                    }
                }
            }

            return Ok(ToolResult {
                output: if success {
                    format!("Status: SUCCESS\n\n{}", response.output)
                } else {
                    format!(
                        "Status: FAILED\n{}",
                        response
                            .error
                            .clone()
                            .unwrap_or_else(|| response.output.clone())
                    )
                },
                success,
                files_to_send,
                ..Default::default()
            });
        }

        let sub_llm = self.resolve_sub_provider(input.model.as_deref(), input.context_window)?;

        if is_sync {
            // Sync mode: run subagent inline and return the result directly
            let mut tools = ToolRegistry::with_builtins(&self.working_dir);
            // Load plugin tools so subagents can use fm_tts, etc.
            if !self.plugin_dirs.is_empty() {
                let _ = crate::plugins::PluginLoader::load_into_with_work_dir(
                    &mut tools,
                    &self.plugin_dirs,
                    &self.plugin_extra_env,
                    Some(&self.working_dir),
                );
            }
            for factory in &self.child_tool_factories {
                tools.register_arc(factory());
            }
            // In subagent context, spawn_only tools should be regular tools —
            // the subagent IS the background, so no need to auto-background again.
            tools.clear_spawn_only();
            ensure_subagent_tools_available(&tools, &allowed_tools)
                .map_err(|error| eyre::eyre!(error))?;
            let policy = build_subagent_tool_policy(allowed_tools, workflow.as_ref());
            tools.apply_policy(&policy);
            if let Some(ref pp) = self.provider_policy {
                tools.set_provider_policy(pp.clone());
            }
            let mut worker = Agent::new(worker_id, sub_llm, tools, self.memory.clone());
            if let Some(ref config) = self.worker_config {
                worker = worker.with_config(config.clone());
            }
            // Base prompt: configured worker prompt, or compiled-in default.
            // Additional instructions are appended, never replacing the base.
            let base_prompt = self
                .worker_prompt
                .clone()
                .unwrap_or_else(|| crate::DEFAULT_WORKER_PROMPT.to_string());
            let full_prompt = match &input.additional_instructions {
                Some(extra) if !extra.is_empty() => format!("{base_prompt}\n\n{extra}"),
                _ => base_prompt,
            };
            worker = worker.with_system_prompt(full_prompt);

            let subtask = Task::new(
                TaskKind::Code {
                    instruction: task_desc.clone(),
                    files: vec![],
                },
                TaskContext {
                    working_dir: self.working_dir.clone(),
                    ..Default::default()
                },
            );

            let result = worker.run_task(&subtask).await;
            match result {
                Ok(r) => Ok(ToolResult {
                    output: r.output,
                    success: r.success,
                    tokens_used: Some(r.token_usage),
                    ..Default::default()
                }),
                Err(e) => Ok(ToolResult {
                    output: format!("Subagent failed: {e}"),
                    success: false,
                    ..Default::default()
                }),
            }
        } else {
            // Background mode: fire-and-forget
            let (origin_channel, origin_chat_id) = self
                .origin
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            let task_ledger_path = self
                .task_ledger_path
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned());
            let tracked_task_id = self.task_supervisor.as_ref().map(|supervisor| {
                supervisor.register_with_lineage(
                    &label,
                    &format!("spawn-{worker_id}"),
                    self.session_key.as_deref(),
                    task_ledger_path.as_deref(),
                )
            });
            let tracked_child_session_key = tracked_task_id.as_ref().and_then(|task_id| {
                self.task_supervisor
                    .as_ref()
                    .and_then(|supervisor| supervisor.get_task(task_id))
                    .and_then(|task| task.child_session_key)
            });
            let llm = sub_llm;
            let memory = self.memory.clone();
            let working_dir = self.working_dir.clone();
            let inbound_tx = self.inbound_tx.clone();
            let wid = worker_id.clone();
            let provider_policy = self.provider_policy.clone();
            let additional_instructions = input.additional_instructions;
            let default_worker_prompt = self.worker_prompt.clone();
            let bg_sender = self.background_result_sender.clone();
            let child_session_sender = self.child_session_sender.clone();
            let task_label = label.clone();
            let plugin_dirs = self.plugin_dirs.clone();
            let plugin_extra_env = self.plugin_extra_env.clone();
            let child_tool_factories = self.child_tool_factories.clone();
            let task_supervisor = self.task_supervisor.clone();
            let worker_config = self.worker_config.clone();
            let workflow_metadata = workflow.clone();
            let parent_session_key = self.session_key.clone();
            let worker_hooks = self.hooks.clone();
            let hook_context_template = self.hook_context_template.clone();

            tokio::spawn(async move {
                if let (Some(supervisor), Some(task_id)) =
                    (task_supervisor.as_ref(), tracked_task_id.as_ref())
                {
                    supervisor.mark_running(task_id);
                    if let Some(workflow) = workflow_metadata.as_ref() {
                        supervisor.mark_runtime_state(
                            task_id,
                            crate::task_supervisor::TaskRuntimeState::ExecutingTool,
                            encode_workflow_detail(workflow),
                        );
                    }
                }

                if let (Some(task_id), Some(parent_session_key), Some(child_session_key)) = (
                    tracked_task_id.as_ref(),
                    parent_session_key.as_ref(),
                    tracked_child_session_key.as_ref(),
                ) {
                    let joined = dispatch_child_session_lifecycle(
                        child_session_sender.as_ref(),
                        ChildSessionLifecyclePayload {
                            kind: ChildSessionLifecycleKind::Spawned,
                            task_id: task_id.clone(),
                            task_label: task_label.clone(),
                            instruction: task_desc.clone(),
                            parent_session_key: parent_session_key.clone(),
                            child_session_key: child_session_key.clone(),
                            workflow_kind: workflow_metadata
                                .as_ref()
                                .map(|workflow| workflow.workflow_kind.clone()),
                            current_phase: workflow_metadata
                                .as_ref()
                                .map(|workflow| workflow.current_phase.clone()),
                            output_files: Vec::new(),
                            failure_action: None,
                            error: None,
                        },
                    )
                    .await;
                    record_child_session_lifecycle(
                        ChildSessionLifecycleKind::Spawned,
                        if joined { "dispatched" } else { "not_joined" },
                    );
                }

                let harness_event_sink = match (
                    task_supervisor.as_ref(),
                    tracked_task_id.as_ref(),
                    parent_session_key.as_ref(),
                ) {
                    (Some(supervisor), Some(task_id), Some(session_key)) => {
                        match HarnessEventSink::new(
                            supervisor.clone(),
                            task_id.clone(),
                            session_key.clone(),
                        ) {
                            Ok(sink) => Some(sink),
                            Err(error) => {
                                warn!(
                                    task_id = %task_id,
                                    session_key = %session_key,
                                    error = %error,
                                    "failed to create harness event sink; continuing without structured child progress"
                                );
                                None
                            }
                        }
                    }
                    _ => None,
                };
                let harness_event_sink_path = harness_event_sink
                    .as_ref()
                    .map(|sink| sink.path().display().to_string());

                let mut tools = ToolRegistry::with_builtins(&working_dir);
                // Load plugin tools so subagents can use fm_tts, etc.
                if !plugin_dirs.is_empty() {
                    let _ = crate::plugins::PluginLoader::load_into_with_work_dir(
                        &mut tools,
                        &plugin_dirs,
                        &plugin_extra_env,
                        Some(&working_dir),
                    );
                }
                for factory in &child_tool_factories {
                    tools.register_arc(factory());
                }
                // In subagent context, spawn_only tools should be regular tools —
                // the subagent IS the background, so no need to auto-background again.
                tools.clear_spawn_only();
                let availability_check = ensure_subagent_tools_available(&tools, &allowed_tools)
                    .map_err(|error| eyre::eyre!(error));
                let policy = build_subagent_tool_policy(allowed_tools, workflow_metadata.as_ref());
                tools.apply_policy(&policy);
                if let Some(pp) = provider_policy {
                    tools.set_provider_policy(pp);
                }
                let mut worker = Agent::new(wid.clone(), llm, tools, memory);
                let mut effective_config = worker_config.clone().unwrap_or_default();
                effective_config.suppress_auto_send_files = true;
                worker = worker.with_config(effective_config);
                if let Some(ref sink_path) = harness_event_sink_path {
                    worker = worker.with_harness_event_sink(sink_path.clone());
                }
                if let Some(ref hooks) = worker_hooks {
                    worker = worker.with_hooks(hooks.clone());
                }
                if let Some(ctx) = hook_context_template.as_ref().map(|ctx| HookContext {
                    session_id: tracked_child_session_key
                        .clone()
                        .or_else(|| ctx.session_id.clone()),
                    profile_id: ctx.profile_id.clone(),
                }) {
                    worker = worker.with_hook_context(ctx);
                }
                let base_prompt = default_worker_prompt
                    .unwrap_or_else(|| crate::DEFAULT_WORKER_PROMPT.to_string());
                let full_prompt = match additional_instructions {
                    Some(extra) if !extra.is_empty() => format!("{base_prompt}\n\n{extra}"),
                    _ => base_prompt,
                };
                worker = worker.with_system_prompt(full_prompt);

                let subtask = Task::new(
                    TaskKind::Code {
                        instruction: task_desc.clone(),
                        files: vec![],
                    },
                    TaskContext {
                        working_dir: working_dir.clone(),
                        ..Default::default()
                    },
                );

                let mut result = match availability_check {
                    Ok(()) => worker.run_task(&subtask).await,
                    Err(error) => Err(error),
                };
                if let Ok(task_result) = result.as_mut() {
                    maybe_generate_inline_research_podcast(
                        worker.tool_registry(),
                        workflow_metadata.as_ref(),
                        &task_desc,
                        task_result,
                    )
                    .await;
                }
                let mut contract_failure = match &result {
                    Ok(task_result) if task_result.success => resolve_background_terminal_files(
                        &working_dir,
                        &task_result.files_to_send,
                        &task_result.files_modified,
                        workflow_metadata.as_ref(),
                    )
                    .err(),
                    _ => None,
                };
                let mut terminal_files = match (&result, contract_failure.as_ref()) {
                    (Ok(task_result), None) if task_result.success => {
                        resolve_background_terminal_files(
                            &working_dir,
                            &task_result.files_to_send,
                            &task_result.files_modified,
                            workflow_metadata.as_ref(),
                        )
                        .unwrap_or_default()
                    }
                    _ => Vec::new(),
                };
                let workflow_kind = workflow_metadata
                    .as_ref()
                    .map(|workflow| workflow.workflow_kind.clone());
                let workflow_phase = workflow_metadata
                    .as_ref()
                    .map(|workflow| workflow.current_phase.clone());
                let verify_phase = workflow_phase
                    .clone()
                    .or_else(|| Some("verify_outputs".to_string()));

                if matches!((&result, contract_failure.as_ref()), (Ok(task_result), None) if task_result.success)
                {
                    if let (Some(task_id), Some(parent_session_key), Some(child_session_key)) = (
                        tracked_task_id.as_ref(),
                        parent_session_key.as_ref(),
                        tracked_child_session_key.as_ref(),
                    ) {
                        let before_verify_payload = HookPayload::before_spawn_verify(
                            task_id.clone(),
                            task_label.clone(),
                            parent_session_key.clone(),
                            child_session_key.clone(),
                            workflow_kind.clone(),
                            verify_phase.clone(),
                            Some("candidate terminal outputs resolved"),
                            terminal_files
                                .iter()
                                .map(|path| path.to_string_lossy().to_string())
                                .collect(),
                            hook_context_template.as_ref(),
                        );
                        match run_before_spawn_verify_hook(
                            worker_hooks.as_ref(),
                            before_verify_payload,
                        )
                        .await
                        {
                            Ok(modified_files) => {
                                terminal_files = modified_files;
                            }
                            Err(reason) => {
                                contract_failure =
                                    Some(format!("spawn verify denied by hook: {reason}"));
                                terminal_files.clear();
                            }
                        }
                    }
                }

                let tracked_output_files = terminal_files
                    .iter()
                    .map(|path| path.to_string_lossy().to_string())
                    .collect::<Vec<_>>();

                if matches!((&result, contract_failure.as_ref()), (Ok(task_result), None) if task_result.success)
                {
                    if let (Some(task_id), Some(parent_session_key), Some(child_session_key)) = (
                        tracked_task_id.as_ref(),
                        parent_session_key.as_ref(),
                        tracked_child_session_key.as_ref(),
                    ) {
                        emit_lifecycle_hook(
                            worker_hooks.as_ref(),
                            HookPayload::on_spawn_verify(
                                task_id.clone(),
                                task_label.clone(),
                                parent_session_key.clone(),
                                child_session_key.clone(),
                                workflow_kind.clone(),
                                verify_phase.clone(),
                                Some("terminal outputs resolved"),
                                tracked_output_files.clone(),
                                hook_context_template.as_ref(),
                            ),
                        )
                        .await;
                    }
                }

                if matches!(&result, Ok(task_result) if task_result.success) {
                    if let (Some(supervisor), Some(task_id), Some(workflow)) = (
                        task_supervisor.as_ref(),
                        tracked_task_id.as_ref(),
                        workflow_metadata.as_ref(),
                    ) {
                        let mut deliver = workflow.clone();
                        deliver.current_phase = "deliver_result".to_string();
                        supervisor.mark_runtime_state(
                            task_id,
                            crate::task_supervisor::TaskRuntimeState::DeliveringOutputs,
                            encode_workflow_detail(&deliver),
                        );
                    }
                }

                let terminal_kind = if contract_failure.is_some() {
                    ChildSessionLifecycleKind::TerminalFailed
                } else {
                    classify_child_session_lifecycle_kind(&result)
                };

                if let (Some(supervisor), Some(task_id)) =
                    (task_supervisor.as_ref(), tracked_task_id.as_ref())
                {
                    match (&result, contract_failure.as_ref()) {
                        (Ok(task_result), None) if task_result.success => {
                            supervisor.mark_completed(task_id, tracked_output_files.clone());
                        }
                        (Ok(_), Some(error)) => {
                            supervisor.mark_failed(task_id, error.clone());
                        }
                        (Ok(task_result), None) => {
                            supervisor.mark_failed(task_id, task_result.output.clone());
                        }
                        (Err(error), _) => {
                            supervisor.mark_failed(task_id, error.to_string());
                        }
                    }
                }

                let terminal_result_text = match (&result, contract_failure.as_ref()) {
                    (Ok(_), Some(error)) => error.clone(),
                    (Ok(task_result), None) => task_result.output.clone(),
                    (Err(error), _) => error.to_string(),
                };

                if let (Some(task_id), Some(parent_session_key), Some(child_session_key)) = (
                    tracked_task_id.as_ref(),
                    parent_session_key.as_ref(),
                    tracked_child_session_key.as_ref(),
                ) {
                    let payload = match (&result, contract_failure.as_ref()) {
                        (Ok(task_result), None) if task_result.success => {
                            ChildSessionLifecyclePayload {
                                kind: terminal_kind,
                                task_id: task_id.clone(),
                                task_label: task_label.clone(),
                                instruction: task_desc.clone(),
                                parent_session_key: parent_session_key.clone(),
                                child_session_key: child_session_key.clone(),
                                workflow_kind: workflow_metadata
                                    .as_ref()
                                    .map(|workflow| workflow.workflow_kind.clone()),
                                current_phase: Some("deliver_result".to_string()),
                                output_files: tracked_output_files.clone(),
                                failure_action: child_session_failure_action(terminal_kind),
                                error: None,
                            }
                        }
                        (Ok(_), Some(error)) => ChildSessionLifecyclePayload {
                            kind: terminal_kind,
                            task_id: task_id.clone(),
                            task_label: task_label.clone(),
                            instruction: task_desc.clone(),
                            parent_session_key: parent_session_key.clone(),
                            child_session_key: child_session_key.clone(),
                            workflow_kind: workflow_metadata
                                .as_ref()
                                .map(|workflow| workflow.workflow_kind.clone()),
                            current_phase: Some("deliver_result".to_string()),
                            output_files: tracked_output_files.clone(),
                            failure_action: child_session_failure_action(terminal_kind),
                            error: Some(error.clone()),
                        },
                        (Ok(task_result), None) => ChildSessionLifecyclePayload {
                            kind: terminal_kind,
                            task_id: task_id.clone(),
                            task_label: task_label.clone(),
                            instruction: task_desc.clone(),
                            parent_session_key: parent_session_key.clone(),
                            child_session_key: child_session_key.clone(),
                            workflow_kind: workflow_metadata
                                .as_ref()
                                .map(|workflow| workflow.workflow_kind.clone()),
                            current_phase: workflow_metadata
                                .as_ref()
                                .map(|workflow| workflow.current_phase.clone()),
                            output_files: tracked_output_files.clone(),
                            failure_action: child_session_failure_action(terminal_kind),
                            error: Some(task_result.output.clone()),
                        },
                        (Err(error), _) => ChildSessionLifecyclePayload {
                            kind: terminal_kind,
                            task_id: task_id.clone(),
                            task_label: task_label.clone(),
                            instruction: task_desc.clone(),
                            parent_session_key: parent_session_key.clone(),
                            child_session_key: child_session_key.clone(),
                            workflow_kind: workflow_metadata
                                .as_ref()
                                .map(|workflow| workflow.workflow_kind.clone()),
                            current_phase: workflow_metadata
                                .as_ref()
                                .map(|workflow| workflow.current_phase.clone()),
                            output_files: tracked_output_files.clone(),
                            failure_action: child_session_failure_action(terminal_kind),
                            error: Some(error.to_string()),
                        },
                    };
                    let joined =
                        dispatch_child_session_lifecycle(child_session_sender.as_ref(), payload)
                            .await;
                    record_child_session_lifecycle(
                        terminal_kind,
                        if joined { "dispatched" } else { "not_joined" },
                    );
                    if let Some(supervisor) = task_supervisor.as_ref() {
                        if let Some(task_id) = tracked_task_id.as_ref() {
                            let terminal_state = match terminal_kind {
                                ChildSessionLifecycleKind::Completed => {
                                    crate::task_supervisor::ChildSessionTerminalState::Completed
                                }
                                ChildSessionLifecycleKind::RetryableFailed => {
                                    crate::task_supervisor::ChildSessionTerminalState::RetryableFailure
                                }
                                ChildSessionLifecycleKind::TerminalFailed => {
                                    crate::task_supervisor::ChildSessionTerminalState::TerminalFailure
                                }
                                ChildSessionLifecycleKind::Spawned => unreachable!(
                                    "child session terminal handling should never see Spawned"
                                ),
                            };
                            supervisor.mark_child_session_outcome(
                                task_id,
                                terminal_state,
                                if joined {
                                    crate::task_supervisor::ChildSessionJoinState::Joined
                                } else {
                                    crate::task_supervisor::ChildSessionJoinState::Orphaned
                                },
                            );
                        }
                    }
                }

                if let (Some(task_id), Some(parent_session_key), Some(child_session_key)) = (
                    tracked_task_id.as_ref(),
                    parent_session_key.as_ref(),
                    tracked_child_session_key.as_ref(),
                ) {
                    match terminal_kind {
                        ChildSessionLifecycleKind::Completed => {
                            emit_lifecycle_hook(
                                worker_hooks.as_ref(),
                                HookPayload::on_spawn_complete(
                                    task_id.clone(),
                                    task_label.clone(),
                                    parent_session_key.clone(),
                                    child_session_key.clone(),
                                    workflow_kind.clone(),
                                    Some("deliver_result".to_string()),
                                    Some(terminal_result_text.clone()),
                                    tracked_output_files.clone(),
                                    hook_context_template.as_ref(),
                                ),
                            )
                            .await;
                        }
                        ChildSessionLifecycleKind::RetryableFailed
                        | ChildSessionLifecycleKind::TerminalFailed => {
                            let failure_action = child_session_failure_action(terminal_kind)
                                .map(child_session_failure_action_label)
                                .unwrap_or("escalate");
                            emit_lifecycle_hook(
                                worker_hooks.as_ref(),
                                HookPayload::on_spawn_failure(
                                    task_id.clone(),
                                    task_label.clone(),
                                    parent_session_key.clone(),
                                    child_session_key.clone(),
                                    workflow_kind.clone(),
                                    workflow_phase.clone(),
                                    terminal_result_text.clone(),
                                    tracked_output_files.clone(),
                                    failure_action,
                                    hook_context_template.as_ref(),
                                ),
                            )
                            .await;
                        }
                        ChildSessionLifecycleKind::Spawned => {}
                    }
                }

                let content = match (&result, contract_failure.as_ref()) {
                    (Ok(_), Some(error)) => format!("Status: FAILED\nError: {error}"),
                    (Ok(r), None) => format!(
                        "Status: {}\n\n{}",
                        if r.success { "SUCCESS" } else { "FAILED" },
                        r.output
                    ),
                    (Err(e), _) => format!("Status: FAILED\nError: {e}"),
                };
                let (result_kind, result_media) = match (&result, contract_failure.as_ref()) {
                    (Ok(_), Some(_)) => {
                        record_terminal_result_reason(
                            BackgroundResultKind::Report,
                            "workspace_contract_failure",
                        );
                        (BackgroundResultKind::Report, Vec::new())
                    }
                    (Ok(r), None) if r.success => {
                        if !terminal_files.is_empty() {
                            record_terminal_result_reason(
                                BackgroundResultKind::Notification,
                                "workflow_terminal_artifact",
                            );
                            (
                                BackgroundResultKind::Notification,
                                terminal_files
                                    .into_iter()
                                    .map(|path| path.to_string_lossy().to_string())
                                    .collect::<Vec<_>>(),
                            )
                        } else if should_deliver_output_files(&r.files_to_send) {
                            record_terminal_result_reason(
                                BackgroundResultKind::Notification,
                                "explicit_output_files",
                            );
                            (
                                BackgroundResultKind::Notification,
                                r.files_to_send
                                    .iter()
                                    .map(|path| path.to_string_lossy().to_string())
                                    .collect::<Vec<_>>(),
                            )
                        } else {
                            record_terminal_result_reason(
                                BackgroundResultKind::Report,
                                "report_summary",
                            );
                            (BackgroundResultKind::Report, Vec::new())
                        }
                    }
                    _ => {
                        record_terminal_result_reason(
                            BackgroundResultKind::Report,
                            "task_failure_report",
                        );
                        (BackgroundResultKind::Report, Vec::new())
                    }
                };

                // Direct injection path: inject as system message, no extra LLM call.
                // If the actor has exited (idle timeout), the send fails and we
                // fall through to the legacy InboundMessage relay path.
                if deliver_background_result(
                    bg_sender,
                    BackgroundResultPayload {
                        task_label,
                        content: content.clone(),
                        kind: result_kind,
                        media: result_media.clone(),
                    },
                )
                .await
                {
                    return;
                }
                record_retry("background_result_relay_fallback");
                warn!("background result sender failed (actor dead?), falling back to relay");

                // Legacy path: relay via InboundMessage (triggers extra LLM call)
                let content = match &result {
                    Ok(r) => format!(
                        "[Subagent {} completed]\nTask: {}\nStatus: {}\n\nResult:\n{}\n\nPlease summarize this result naturally for the user.",
                        wid,
                        task_desc,
                        if r.success { "SUCCESS" } else { "FAILED" },
                        r.output
                    ),
                    Err(e) => format!(
                        "[Subagent {} failed]\nTask: {}\nError: {e}\n\nPlease inform the user about this failure.",
                        wid, task_desc
                    ),
                };

                let announce = InboundMessage {
                    channel: "system".into(),
                    sender_id: "subagent".into(),
                    chat_id: format!("{origin_channel}:{origin_chat_id}"),
                    content,
                    timestamp: chrono::Utc::now(),
                    media: vec![],
                    metadata: serde_json::json!({
                        "deliver_to_channel": origin_channel,
                        "deliver_to_chat_id": origin_chat_id,
                    }),
                    message_id: None,
                };

                if let Err(e) = inbound_tx.send(announce).await {
                    record_result_delivery("relay_inbound_message", "enqueue_failed", result_kind);
                    warn!(error = %e, "failed to announce subagent result");
                } else {
                    record_result_delivery("relay_inbound_message", "enqueued", result_kind);
                }
            });

            Ok(ToolResult {
                output: format!("Spawned background task: {label}"),
                success: true,
                ..Default::default()
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{HookConfig, HookEvent};

    #[cfg(unix)]
    fn capture_hook(event: HookEvent, log_path: &std::path::Path) -> HookConfig {
        HookConfig {
            event,
            command: vec![
                "/bin/sh".into(),
                "-c".into(),
                r#"payload=$(cat); printf "%s\n" "$payload" >> "$1""#.into(),
                "sh".into(),
                log_path.to_string_lossy().into_owned(),
            ],
            timeout_ms: 5000,
            tool_filter: vec![],
        }
    }

    #[cfg(unix)]
    fn rewrite_output_files_hook(replacement_path: &std::path::Path) -> HookConfig {
        HookConfig {
            event: HookEvent::BeforeSpawnVerify,
            command: vec![
                "/bin/sh".into(),
                "-c".into(),
                r#"cat >/dev/null; printf '{"output_files":["%s"]}\n' "$1"; exit 2"#.into(),
                "sh".into(),
                replacement_path.to_string_lossy().into_owned(),
            ],
            timeout_ms: 5000,
            tool_filter: vec![],
        }
    }

    #[tokio::test]
    async fn test_spawn_returns_immediately() {
        let (in_tx, _in_rx) = tokio::sync::mpsc::channel(16);

        // We can't easily create a real LLM + EpisodeStore for unit tests,
        // so just test the worker count and basic input parsing.
        let tool = SpawnTool {
            llm: Arc::new(MockProvider),
            memory: Arc::new(create_test_store().await),
            working_dir: PathBuf::from("/tmp"),
            inbound_tx: in_tx,
            origin: std::sync::Mutex::new(("cli".into(), "test".into())),
            worker_count: AtomicU32::new(0),
            provider_policy: None,
            provider_router: None,
            worker_prompt: None,
            background_result_sender: None,
            child_session_sender: None,
            hooks: None,
            hook_context_template: None,
            plugin_dirs: Vec::new(),
            plugin_extra_env: Vec::new(),
            child_tool_factories: Vec::new(),
            task_supervisor: None,
            session_key: None,
            task_ledger_path: None,
            worker_config: None,
            mcp_agent_backend: None,
            mcp_agent_tool_name: None,
            cost_accountant: None,
        };

        assert_eq!(tool.worker_count.load(Ordering::SeqCst), 0);

        // Invalid input test
        let result = tool.execute(&serde_json::json!({})).await;
        assert!(result.is_err());

        // Worker count should not increment on invalid input
        assert_eq!(tool.worker_count.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn test_background_spawn_tracks_supervisor_lifecycle() {
        let (in_tx, _in_rx) = tokio::sync::mpsc::channel(16);
        let supervisor = Arc::new(TaskSupervisor::new());
        let tool = SpawnTool::new(
            Arc::new(MockProvider),
            Arc::new(create_test_store().await),
            PathBuf::from("/tmp"),
            in_tx,
        )
        .with_task_supervisor(
            supervisor.clone(),
            "api:test-session",
            PathBuf::from("/tmp/tasks.jsonl"),
        );

        let result = tool
            .execute(&serde_json::json!({
                "task": "Write a short answer",
                "label": "Deep research",
                "mode": "background",
                "allowed_tools": []
            }))
            .await
            .unwrap();

        assert!(result.success);

        let started = std::time::Instant::now();
        loop {
            let tasks = supervisor.get_tasks_for_session("api:test-session");
            if let Some(task) = tasks.first() {
                if task.status == crate::task_supervisor::TaskStatus::Completed {
                    assert_eq!(task.tool_name, "Deep research");
                    break;
                }
            }
            assert!(
                started.elapsed() < std::time::Duration::from_secs(5),
                "background spawn task did not complete in time"
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }

    #[tokio::test]
    async fn test_background_spawn_uses_contract_selected_slides_artifact_for_persistence() {
        let (in_tx, _in_rx) = tokio::sync::mpsc::channel(16);
        let temp = tempfile::tempdir().unwrap();
        let repo_root = temp.path().join("slides/demo");
        let ledger = temp.path().join("tasks.jsonl");
        std::fs::create_dir_all(repo_root.join("output")).unwrap();
        crate::write_workspace_policy(
            &repo_root,
            &crate::WorkspacePolicy::for_kind(crate::WorkspaceProjectKind::Slides),
        )
        .unwrap();
        std::fs::write(repo_root.join("script.js"), "// slides").unwrap();
        std::fs::write(repo_root.join("memory.md"), "# memory").unwrap();
        std::fs::write(repo_root.join("changelog.md"), "# changelog").unwrap();
        std::fs::write(repo_root.join("output/deck.pptx"), "final").unwrap();
        std::fs::write(repo_root.join("output/slide-01.png"), "png").unwrap();

        let supervisor = Arc::new(TaskSupervisor::new());
        supervisor.enable_persistence(&ledger).unwrap();
        let tool = SpawnTool::new(
            Arc::new(ShellThenEndProvider {
                calls: std::sync::atomic::AtomicUsize::new(0),
            }),
            Arc::new(create_test_store().await),
            temp.path().to_path_buf(),
            in_tx,
        )
        .with_task_supervisor(supervisor.clone(), "api:test-session", ledger.clone());

        let result = tool
            .execute(&serde_json::json!({
                "task": "Acknowledge the request and stop.",
                "label": "Slides deliverable",
                "mode": "background",
                "allowed_tools": ["shell"],
                "workflow": {
                    "workflow_kind": "slides",
                    "current_phase": "design",
                    "allowed_tools": ["shell"],
                    "terminal_output": {
                        "deliver_final_artifact_only": true,
                        "forbid_intermediate_files": true,
                        "required_artifact_kind": "presentation"
                    }
                }
            }))
            .await
            .unwrap();

        assert!(result.success);

        let started = std::time::Instant::now();
        loop {
            let tasks = supervisor.get_tasks_for_session("api:test-session");
            if let Some(task) = tasks.first() {
                if task.status == crate::task_supervisor::TaskStatus::Completed {
                    assert_eq!(
                        task.output_files,
                        vec![
                            repo_root
                                .join("output/deck.pptx")
                                .to_string_lossy()
                                .to_string()
                        ]
                    );
                    break;
                }
            }
            assert!(
                started.elapsed() < std::time::Duration::from_secs(5),
                "background spawn task did not complete in time"
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }

        let restored = TaskSupervisor::new();
        restored.enable_persistence(&ledger).unwrap();
        let tasks = restored.get_tasks_for_session("api:test-session");
        assert_eq!(tasks.len(), 1);
        assert_eq!(
            tasks[0].status,
            crate::task_supervisor::TaskStatus::Completed
        );
        assert_eq!(
            tasks[0].output_files,
            vec![
                repo_root
                    .join("output/deck.pptx")
                    .to_string_lossy()
                    .to_string()
            ]
        );
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn test_before_spawn_verify_hook_can_replace_output_files() {
        let temp = tempfile::tempdir().unwrap();
        let replacement = temp.path().join("final-reviewed.pptx");
        std::fs::write(&replacement, "reviewed").unwrap();

        let hooks = Arc::new(HookExecutor::new(vec![rewrite_output_files_hook(
            &replacement,
        )]));
        let payload = HookPayload::before_spawn_verify(
            "task-1",
            "Slides deliverable",
            "api:test-session",
            "api:test-session:child",
            Some("slides"),
            Some("verify_outputs"),
            Some("candidate terminal outputs resolved"),
            vec!["/tmp/original-deck.pptx".to_string()],
            Some(&HookContext {
                session_id: Some("api:test-session".to_string()),
                profile_id: Some("test-profile".to_string()),
            }),
        );

        let modified_files = run_before_spawn_verify_hook(Some(&hooks), payload)
            .await
            .unwrap();

        assert_eq!(modified_files, vec![replacement]);
    }

    #[tokio::test]
    async fn test_background_spawn_fails_when_contract_owned_workflow_is_not_ready() {
        let (in_tx, _in_rx) = tokio::sync::mpsc::channel(16);
        let temp = tempfile::tempdir().unwrap();
        let repo_root = temp.path().join("slides/demo");
        let ledger = temp.path().join("tasks.jsonl");
        std::fs::create_dir_all(&repo_root).unwrap();
        crate::write_workspace_policy(
            &repo_root,
            &crate::WorkspacePolicy::for_kind(crate::WorkspaceProjectKind::Slides),
        )
        .unwrap();
        std::fs::write(repo_root.join("script.js"), "// slides").unwrap();
        std::fs::write(repo_root.join("memory.md"), "# memory").unwrap();
        std::fs::write(repo_root.join("changelog.md"), "# changelog").unwrap();

        let supervisor = Arc::new(TaskSupervisor::new());
        supervisor.enable_persistence(&ledger).unwrap();
        let tool = SpawnTool::new(
            Arc::new(ShellThenEndProvider {
                calls: std::sync::atomic::AtomicUsize::new(0),
            }),
            Arc::new(create_test_store().await),
            temp.path().to_path_buf(),
            in_tx,
        )
        .with_task_supervisor(supervisor.clone(), "api:test-session", ledger);

        let result = tool
            .execute(&serde_json::json!({
                "task": "Acknowledge the request and stop.",
                "label": "Slides deliverable",
                "mode": "background",
                "allowed_tools": ["shell"],
                "workflow": {
                    "workflow_kind": "slides",
                    "current_phase": "design",
                    "allowed_tools": ["shell"],
                    "terminal_output": {
                        "deliver_final_artifact_only": true,
                        "forbid_intermediate_files": true,
                        "required_artifact_kind": "presentation"
                    }
                }
            }))
            .await
            .unwrap();

        assert!(result.success);

        let started = std::time::Instant::now();
        loop {
            let tasks = supervisor.get_tasks_for_session("api:test-session");
            if let Some(task) = tasks.first() {
                if task.status == crate::task_supervisor::TaskStatus::Failed {
                    let error = task.error.as_deref().unwrap_or_default();
                    assert!(error.contains("workspace contract"), "{error}");
                    return;
                }
            }
            assert!(
                started.elapsed() < std::time::Duration::from_secs(5),
                "background spawn task did not fail in time"
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn test_background_spawn_emits_failure_hook_for_contract_failure() {
        let (in_tx, _in_rx) = tokio::sync::mpsc::channel(16);
        let temp = tempfile::tempdir().unwrap();
        let repo_root = temp.path().join("slides/demo");
        let ledger = temp.path().join("tasks.jsonl");
        let hook_log = temp.path().join("spawn-failure-hooks.jsonl");
        std::fs::create_dir_all(&repo_root).unwrap();
        crate::write_workspace_policy(
            &repo_root,
            &crate::WorkspacePolicy::for_kind(crate::WorkspaceProjectKind::Slides),
        )
        .unwrap();
        std::fs::write(repo_root.join("script.js"), "// slides").unwrap();
        std::fs::write(repo_root.join("memory.md"), "# memory").unwrap();
        std::fs::write(repo_root.join("changelog.md"), "# changelog").unwrap();

        let supervisor = Arc::new(TaskSupervisor::new());
        supervisor.enable_persistence(&ledger).unwrap();
        let hooks = Arc::new(HookExecutor::new(vec![capture_hook(
            HookEvent::OnSpawnFailure,
            &hook_log,
        )]));
        let tool = SpawnTool::new(
            Arc::new(MockProvider),
            Arc::new(create_test_store().await),
            temp.path().to_path_buf(),
            in_tx,
        )
        .with_task_supervisor(supervisor.clone(), "api:test-session", ledger)
        .with_hooks(hooks)
        .with_hook_context(HookContext {
            session_id: Some("api:test-session".to_string()),
            profile_id: Some("test-profile".to_string()),
        });

        let result = tool
            .execute(&serde_json::json!({
                "task": "Build the deck",
                "label": "Slides deliverable",
                "mode": "background",
                "allowed_tools": ["mofa_slides"],
                "workflow": {
                    "workflow_kind": "slides",
                    "current_phase": "design",
                    "allowed_tools": ["mofa_slides"],
                    "terminal_output": {
                        "deliver_final_artifact_only": true,
                        "forbid_intermediate_files": true,
                        "required_artifact_kind": "presentation"
                    }
                }
            }))
            .await
            .unwrap();

        assert!(result.success);

        let started = std::time::Instant::now();
        loop {
            let tasks = supervisor.get_tasks_for_session("api:test-session");
            let hook_lines = std::fs::read_to_string(&hook_log).unwrap_or_default();
            if let Some(task) = tasks.first() {
                if task.status == crate::task_supervisor::TaskStatus::Failed
                    && hook_lines.contains("\"event\":\"on_spawn_failure\"")
                {
                    assert!(hook_lines.contains("\"failure_action\":\"escalate\""));
                    assert!(hook_lines.contains("\"workflow_kind\":\"slides\""));
                    assert!(hook_lines.contains("\"result\":\""));
                    return;
                }
            }
            assert!(
                started.elapsed() < std::time::Duration::from_secs(5),
                "background spawn failure hook did not arrive in time"
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }

    #[test]
    fn workflow_terminal_output_prefers_final_audio_and_skips_intermediates() {
        let workflow = WorkflowMetadata {
            workflow_kind: "research_podcast".to_string(),
            current_phase: "generate_audio".to_string(),
            allowed_tools: vec!["podcast_generate".to_string()],
            terminal_output: Some(WorkflowTerminalOutputPolicy {
                deliver_final_artifact_only: true,
                forbid_intermediate_files: true,
                required_artifact_kind: "audio".to_string(),
            }),
        };

        let files_to_send = vec![
            PathBuf::from("/tmp/podcast_part_1.mp3"),
            PathBuf::from("/tmp/research_report.md"),
            PathBuf::from("/tmp/podcast_full_final.mp3"),
        ];
        let files_modified = vec![PathBuf::from("/tmp/script.md")];

        let selected =
            select_workflow_terminal_files(&files_to_send, &files_modified, Some(&workflow))
                .unwrap();

        assert_eq!(selected, vec![PathBuf::from("/tmp/podcast_full_final.mp3")]);
    }

    #[test]
    fn workflow_terminal_output_accepts_audio_from_modified_files_when_explicit_send_missing() {
        let workflow = WorkflowMetadata {
            workflow_kind: "research_podcast".to_string(),
            current_phase: "generate_audio".to_string(),
            allowed_tools: vec!["podcast_generate".to_string()],
            terminal_output: Some(WorkflowTerminalOutputPolicy {
                deliver_final_artifact_only: true,
                forbid_intermediate_files: true,
                required_artifact_kind: "audio".to_string(),
            }),
        };

        let files_modified = vec![
            PathBuf::from("/tmp/podcast_script.md"),
            PathBuf::from("/tmp/podcast_full_final.mp3"),
        ];

        let selected =
            select_workflow_terminal_files(&[], &files_modified, Some(&workflow)).unwrap();

        assert_eq!(selected, vec![PathBuf::from("/tmp/podcast_full_final.mp3")]);
    }

    #[test]
    fn workflow_terminal_output_requires_required_audio_artifact() {
        let temp = tempfile::tempdir().unwrap();
        let workflow = WorkflowMetadata {
            workflow_kind: "research_podcast".to_string(),
            current_phase: "deliver_result".to_string(),
            allowed_tools: vec!["podcast_generate".to_string()],
            terminal_output: Some(WorkflowTerminalOutputPolicy {
                deliver_final_artifact_only: true,
                forbid_intermediate_files: true,
                required_artifact_kind: "audio".to_string(),
            }),
        };

        let error = resolve_background_terminal_files(temp.path(), &[], &[], Some(&workflow))
            .expect_err("research_podcast must not complete without audio");

        assert!(error.contains("required audio terminal artifact"));
    }

    #[test]
    fn workflow_terminal_output_prefers_final_presentation_and_skips_scratch_files() {
        let workflow = WorkflowMetadata {
            workflow_kind: "slides".to_string(),
            current_phase: "deliver_result".to_string(),
            allowed_tools: vec!["mofa_slides".to_string()],
            terminal_output: Some(WorkflowTerminalOutputPolicy {
                deliver_final_artifact_only: true,
                forbid_intermediate_files: true,
                required_artifact_kind: "presentation".to_string(),
            }),
        };

        let files_to_send = vec![
            PathBuf::from("/tmp/output/slide-01.png"),
            PathBuf::from("/tmp/output/deck.pptx"),
            PathBuf::from("/tmp/output/notes.txt"),
        ];

        let selected =
            select_workflow_terminal_files(&files_to_send, &[], Some(&workflow)).unwrap();

        assert_eq!(selected, vec![PathBuf::from("/tmp/output/deck.pptx")]);
    }

    #[test]
    fn workflow_terminal_output_prefers_site_entrypoint_and_skips_assets() {
        let workflow = WorkflowMetadata {
            workflow_kind: "site".to_string(),
            current_phase: "deliver_result".to_string(),
            allowed_tools: vec!["shell".to_string()],
            terminal_output: Some(WorkflowTerminalOutputPolicy {
                deliver_final_artifact_only: true,
                forbid_intermediate_files: true,
                required_artifact_kind: "site".to_string(),
            }),
        };

        let files_to_send = vec![
            PathBuf::from("/tmp/site/dist/assets/logo.png"),
            PathBuf::from("/tmp/site/dist/index.html"),
            PathBuf::from("/tmp/site/dist/about.html"),
        ];

        let selected =
            select_workflow_terminal_files(&files_to_send, &[], Some(&workflow)).unwrap();

        assert_eq!(selected, vec![PathBuf::from("/tmp/site/dist/index.html")]);
    }

    #[test]
    fn contract_owned_workflow_denies_send_file_in_subagent_policy() {
        let workflow = WorkflowMetadata {
            workflow_kind: "slides".to_string(),
            current_phase: "deliver_result".to_string(),
            allowed_tools: vec!["mofa_slides".to_string(), "send_file".to_string()],
            terminal_output: Some(WorkflowTerminalOutputPolicy {
                deliver_final_artifact_only: true,
                forbid_intermediate_files: true,
                required_artifact_kind: "presentation".to_string(),
            }),
        };

        let policy = build_subagent_tool_policy(workflow.allowed_tools.clone(), Some(&workflow));

        assert!(policy.deny.contains(&"spawn".to_string()));
        assert!(policy.deny.contains(&"send_file".to_string()));
    }

    #[test]
    fn subagent_tool_preflight_activates_deferred_allowed_tool() {
        let mut tools = ToolRegistry::with_builtins("/tmp");
        tools.defer(["shell".to_string()]);
        assert!(tools.specs().iter().all(|spec| spec.name != "shell"));

        ensure_subagent_tools_available(&tools, &[String::from("shell")]).unwrap();

        assert!(tools.specs().iter().any(|spec| spec.name == "shell"));
    }

    #[test]
    fn subagent_tool_preflight_reports_missing_allowed_tool() {
        let tools = ToolRegistry::with_builtins("/tmp");

        let error = ensure_subagent_tools_available(&tools, &[String::from("podcast_generate")])
            .unwrap_err();

        assert!(error.contains("required tool(s) not available on this host"));
        assert!(error.contains("podcast_generate"));
    }

    struct StaticTestTool {
        name: &'static str,
    }

    #[async_trait]
    impl Tool for StaticTestTool {
        fn name(&self) -> &str {
            self.name
        }

        fn description(&self) -> &str {
            "test child tool"
        }

        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({ "type": "object", "properties": {} })
        }

        async fn execute(&self, _args: &serde_json::Value) -> Result<ToolResult> {
            Ok(ToolResult {
                output: "ok".to_string(),
                success: true,
                ..Default::default()
            })
        }
    }

    fn write_mock_podcast_plugin(root: &std::path::Path, script_seen: &std::path::Path) -> PathBuf {
        let plugin_root = root.join("plugins");
        let plugin_dir = plugin_root.join("mofa-podcast");
        std::fs::create_dir_all(&plugin_dir).unwrap();
        std::fs::write(
            plugin_dir.join("manifest.json"),
            r#"{
  "name": "mofa-podcast",
  "version": "0.0.0-test",
  "tools": [
    {
      "name": "podcast_generate",
      "spawn_only": true,
      "description": "mock podcast generator",
      "input_schema": {
        "type": "object",
        "properties": {
          "script": { "type": "string" }
        }
      }
    }
  ]
}"#,
        )
        .unwrap();
        let main = plugin_dir.join("main");
        std::fs::write(
            &main,
            format!(
                r#"#!/usr/bin/env bash
set -euo pipefail
INPUT="$(cat)"
SCRIPT_SEEN="{script_seen}"
OCTOS_PLUGIN_INPUT="$INPUT" SCRIPT_SEEN="$SCRIPT_SEEN" python3 - <<'PY'
import json
import os

payload = json.loads(os.environ.get("OCTOS_PLUGIN_INPUT") or "{{}}")
with open(os.environ["SCRIPT_SEEN"], "w", encoding="utf-8") as handle:
    handle.write(str(payload.get("script") or ""))

base = os.environ.get("OCTOS_WORK_DIR") or os.getcwd()
out_dir = os.path.join(base, "skill-output", "mofa-podcast")
os.makedirs(out_dir, exist_ok=True)
out = os.path.join(out_dir, "podcast_full_test.mp3")
with open(out, "wb") as handle:
    handle.write(b"0" * 8192)

print(json.dumps({{"output": f"Podcast generated successfully: {{out}}", "success": True, "files_to_send": [out]}}))
PY
"#,
                script_seen = script_seen.display()
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&main, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        plugin_root
    }

    #[tokio::test]
    async fn test_sync_spawn_registers_child_tool_factory_before_preflight() {
        let (in_tx, _in_rx) = tokio::sync::mpsc::channel(16);
        let tool = SpawnTool::new(
            Arc::new(MockProvider),
            Arc::new(create_test_store().await),
            PathBuf::from("/tmp"),
            in_tx,
        )
        .with_child_tool_factory(Arc::new(|| {
            Arc::new(StaticTestTool {
                name: "run_pipeline",
            })
        }));

        let result = tool
            .execute(&serde_json::json!({
                "task": "Use the injected pipeline tool if needed",
                "label": "Deep research",
                "mode": "sync",
                "allowed_tools": ["run_pipeline"]
            }))
            .await
            .unwrap();

        assert!(result.success);
        assert_eq!(result.output, "done");
    }

    #[test]
    fn contract_terminal_output_prefers_declared_slides_deck_name_over_newer_draft() {
        let temp = tempfile::tempdir().unwrap();
        let repo_root = temp.path().join("slides/demo");
        std::fs::create_dir_all(repo_root.join("output")).unwrap();
        crate::write_workspace_policy(
            &repo_root,
            &crate::WorkspacePolicy::for_kind(crate::WorkspaceProjectKind::Slides),
        )
        .unwrap();
        std::fs::write(repo_root.join("script.js"), "// slides").unwrap();
        std::fs::write(repo_root.join("memory.md"), "# memory").unwrap();
        std::fs::write(repo_root.join("changelog.md"), "# changelog").unwrap();
        std::fs::write(repo_root.join("output/deck.pptx"), "final").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(repo_root.join("output/deck-draft.pptx"), "draft").unwrap();
        std::fs::write(repo_root.join("output/slide-01.png"), "png").unwrap();

        let workflow = WorkflowMetadata {
            workflow_kind: "slides".to_string(),
            current_phase: "deliver_result".to_string(),
            allowed_tools: vec!["mofa_slides".to_string()],
            terminal_output: Some(WorkflowTerminalOutputPolicy {
                deliver_final_artifact_only: true,
                forbid_intermediate_files: true,
                required_artifact_kind: "presentation".to_string(),
            }),
        };

        let selected = resolve_contract_terminal_files(&repo_root, Some(&workflow))
            .unwrap()
            .unwrap();

        assert_eq!(selected, vec![repo_root.join("output/deck.pptx")]);
    }

    #[test]
    fn contract_terminal_output_fails_when_site_entrypoint_is_missing() {
        let temp = tempfile::tempdir().unwrap();
        let repo_root = temp.path().join("sites/news");
        std::fs::create_dir_all(&repo_root).unwrap();
        crate::write_workspace_policy(
            &repo_root,
            &crate::WorkspacePolicy::for_site_build_output("out"),
        )
        .unwrap();
        std::fs::write(repo_root.join("mofa-site-session.json"), "{}").unwrap();
        std::fs::write(repo_root.join("site-plan.json"), "{}").unwrap();
        std::fs::write(repo_root.join("optimized-prompt.md"), "# prompt").unwrap();

        let workflow = WorkflowMetadata {
            workflow_kind: "site".to_string(),
            current_phase: "deliver_result".to_string(),
            allowed_tools: vec!["shell".to_string()],
            terminal_output: Some(WorkflowTerminalOutputPolicy {
                deliver_final_artifact_only: true,
                forbid_intermediate_files: true,
                required_artifact_kind: "site".to_string(),
            }),
        };

        let error = resolve_contract_terminal_files(&repo_root, Some(&workflow)).unwrap_err();
        assert!(error.contains("workspace contract"));
        assert!(error.contains("out/index.html"));
    }
    #[tokio::test]
    async fn test_background_spawn_persists_workflow_phase_transitions() {
        let (in_tx, _in_rx) = tokio::sync::mpsc::channel(16);
        let temp = tempfile::tempdir().unwrap();
        let ledger = temp.path().join("tasks.jsonl");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let script_seen = temp.path().join("script_seen.md");
        let plugin_root = write_mock_podcast_plugin(temp.path(), &script_seen);
        let payloads = Arc::new(std::sync::Mutex::new(Vec::<BackgroundResultPayload>::new()));
        let payloads_for_sender = Arc::clone(&payloads);
        let sender: BackgroundResultSender = Arc::new(move |payload| {
            let payloads_for_sender = Arc::clone(&payloads_for_sender);
            Box::pin(async move {
                payloads_for_sender
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .push(payload);
                true
            })
        });
        let supervisor = Arc::new(TaskSupervisor::new());
        supervisor.enable_persistence(&ledger).unwrap();
        let tool = SpawnTool::new(
            Arc::new(MockProvider),
            Arc::new(create_test_store().await),
            workspace.clone(),
            in_tx,
        )
        .with_task_supervisor(supervisor.clone(), "api:test-session", ledger.clone())
        .with_background_result_sender(sender)
        .with_plugin_dirs(vec![plugin_root], vec![]);

        let result = tool
            .execute(&serde_json::json!({
                "task": "Produce a short podcast. Script: [杨幂 - clone:yangmi, professional] 大家好。 [窦文涛 - clone:douwentao, professional] 这里是测试播客。",
                "label": "Research podcast",
                "mode": "background",
                "allowed_tools": ["podcast_generate"],
                "workflow": {
                    "workflow_kind": "research_podcast",
                    "current_phase": "research",
                    "allowed_tools": ["podcast_generate"],
                    "terminal_output": {
                        "deliver_final_artifact_only": true,
                        "forbid_intermediate_files": true,
                        "required_artifact_kind": "audio"
                    }
                }
            }))
            .await
            .unwrap();

        assert!(result.success);

        let started = std::time::Instant::now();
        loop {
            let tasks = supervisor.get_tasks_for_session("api:test-session");
            if let Some(task) = tasks.first() {
                if task.status == crate::task_supervisor::TaskStatus::Completed {
                    assert_eq!(task.output_files.len(), 1);
                    assert!(task.output_files[0].ends_with(".mp3"));
                    assert!(PathBuf::from(&task.output_files[0]).starts_with(&workspace));
                    break;
                }
            }
            assert!(
                started.elapsed() < std::time::Duration::from_secs(5),
                "background spawn task did not complete in time"
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }

        let details: Vec<serde_json::Value> = std::fs::read_to_string(&ledger)
            .unwrap()
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .filter_map(|record| {
                record
                    .get("task")
                    .and_then(|task| task.get("runtime_detail"))
                    .and_then(|detail| detail.as_str())
                    .and_then(|detail| serde_json::from_str::<serde_json::Value>(detail).ok())
            })
            .collect();

        assert!(details.iter().any(|detail| {
            detail.get("workflow_kind").and_then(|v| v.as_str()) == Some("research_podcast")
                && detail.get("current_phase").and_then(|v| v.as_str()) == Some("research")
        }));
        assert!(details.iter().any(|detail| {
            detail.get("workflow_kind").and_then(|v| v.as_str()) == Some("research_podcast")
                && detail.get("current_phase").and_then(|v| v.as_str()) == Some("deliver_result")
        }));

        let script =
            std::fs::read_to_string(&script_seen).expect("podcast_generate should receive script");
        assert!(script.contains("大家好"));

        let payloads = payloads.lock().unwrap_or_else(|error| error.into_inner());
        let media = payloads
            .iter()
            .flat_map(|payload| payload.media.iter())
            .collect::<Vec<_>>();
        assert_eq!(media.len(), 1);
        assert!(media[0].ends_with(".mp3"));
    }

    #[tokio::test]
    async fn test_direct_background_result_short_circuits_legacy_fallback() {
        let called = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let called_clone = Arc::clone(&called);
        let sender: BackgroundResultSender = Arc::new(move |_payload| {
            let called_clone = Arc::clone(&called_clone);
            Box::pin(async move {
                called_clone.store(true, Ordering::SeqCst);
                true
            })
        });

        let payload = BackgroundResultPayload {
            task_label: "child-task".to_string(),
            content: "done".to_string(),
            kind: BackgroundResultKind::Notification,
            media: vec!["/tmp/output.mp3".to_string()],
        };

        assert!(deliver_background_result(Some(sender), payload.clone()).await);
        assert!(called.load(Ordering::SeqCst));
        assert!(
            !deliver_background_result(None, payload).await,
            "fallback should only be used when the direct sender is absent or rejected"
        );
    }

    #[tokio::test]
    async fn test_background_spawn_emits_child_session_lifecycle_events() {
        let memory = Arc::new(create_test_store().await);
        let llm = Arc::new(MockProvider);
        let (tx, _rx) = tokio::sync::mpsc::channel(4);
        let supervisor = Arc::new(TaskSupervisor::new());
        let temp = Box::leak(Box::new(tempfile::tempdir().unwrap()));
        let ledger = temp.path().join("tasks.jsonl");
        let events = Arc::new(std::sync::Mutex::new(
            Vec::<ChildSessionLifecyclePayload>::new(),
        ));
        let events_ref = Arc::clone(&events);
        let sender: ChildSessionLifecycleSender = Arc::new(move |payload| {
            let events_ref = Arc::clone(&events_ref);
            Box::pin(async move {
                events_ref
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(payload);
                true
            })
        });

        let tool = SpawnTool::with_context(
            llm,
            memory,
            temp.path().to_path_buf(),
            tx,
            "api",
            "test-chat",
        )
        .with_task_supervisor(supervisor.clone(), "api:test-session".to_string(), ledger)
        .with_child_session_sender(sender);

        let args = serde_json::json!({
            "task": "Draft the report",
            "mode": "background",
            "allowed_tools": []
        });
        let result = tool.execute(&args).await.unwrap();
        assert!(result.success);

        let started = std::time::Instant::now();
        loop {
            let events = events.lock().unwrap_or_else(|e| e.into_inner()).clone();
            if events.len() >= 2 {
                assert_eq!(events[0].kind, ChildSessionLifecycleKind::Spawned);
                assert_eq!(events[1].kind, ChildSessionLifecycleKind::Completed);
                assert_eq!(events[0].parent_session_key, "api:test-session");
                assert_eq!(events[1].parent_session_key, "api:test-session");
                assert_eq!(events[0].child_session_key, events[1].child_session_key);
                assert_eq!(events[0].task_id, events[1].task_id);
                return;
            }

            assert!(
                started.elapsed() < std::time::Duration::from_secs(5),
                "child-session lifecycle events did not arrive in time"
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn test_background_spawn_emits_verify_and_complete_hooks() {
        let memory = Arc::new(create_test_store().await);
        let llm = Arc::new(MockProvider);
        let (tx, _rx) = tokio::sync::mpsc::channel(4);
        let supervisor = Arc::new(TaskSupervisor::new());
        let temp = Box::leak(Box::new(tempfile::tempdir().unwrap()));
        let ledger = temp.path().join("tasks.jsonl");
        let hook_log = temp.path().join("spawn-hooks.jsonl");
        let hooks = Arc::new(HookExecutor::new(vec![
            capture_hook(HookEvent::OnSpawnVerify, &hook_log),
            capture_hook(HookEvent::OnSpawnComplete, &hook_log),
        ]));

        let tool = SpawnTool::with_context(
            llm,
            memory,
            temp.path().to_path_buf(),
            tx,
            "api",
            "test-chat",
        )
        .with_task_supervisor(supervisor, "api:test-session".to_string(), ledger)
        .with_hooks(hooks)
        .with_hook_context(HookContext {
            session_id: Some("api:test-session".to_string()),
            profile_id: Some("test-profile".to_string()),
        });

        let args = serde_json::json!({
            "task": "Draft the report",
            "mode": "background",
            "allowed_tools": []
        });
        let result = tool.execute(&args).await.unwrap();
        assert!(result.success);

        let started = std::time::Instant::now();
        loop {
            let lines = std::fs::read_to_string(&hook_log)
                .ok()
                .map(|contents| {
                    contents
                        .lines()
                        .map(str::to_string)
                        .filter(|line| !line.is_empty())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();

            if lines.len() >= 2 {
                assert!(
                    lines
                        .iter()
                        .any(|line| line.contains("\"event\":\"on_spawn_verify\""))
                );
                assert!(
                    lines
                        .iter()
                        .any(|line| line.contains("\"event\":\"on_spawn_complete\""))
                );
                assert!(
                    lines
                        .iter()
                        .all(|line| line.contains("\"session_id\":\"api:test-session\""))
                );
                return;
            }

            assert!(
                started.elapsed() < std::time::Duration::from_secs(5),
                "spawn lifecycle hooks did not arrive in time"
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }

    #[test]
    fn classify_child_session_failure_as_retryable_when_budget_exhausted() {
        let result = Ok::<octos_core::TaskResult, eyre::Report>(octos_core::TaskResult {
            schema_version: octos_core::TASK_RESULT_SCHEMA_VERSION,
            success: false,
            output: "Token budget exceeded (120 of 100).".to_string(),
            files_modified: vec![],
            files_to_send: vec![],
            subtasks: vec![],
            token_usage: Default::default(),
        });

        assert_eq!(
            classify_child_session_lifecycle_kind(&result),
            ChildSessionLifecycleKind::RetryableFailed
        );
    }

    #[test]
    fn child_session_failure_action_matches_terminal_kind() {
        assert_eq!(
            child_session_failure_action(ChildSessionLifecycleKind::Completed),
            None
        );
        assert_eq!(
            child_session_failure_action(ChildSessionLifecycleKind::RetryableFailed),
            Some(ChildSessionFailureAction::Retry)
        );
        assert_eq!(
            child_session_failure_action(ChildSessionLifecycleKind::TerminalFailed),
            Some(ChildSessionFailureAction::Escalate)
        );
    }

    #[tokio::test]
    async fn child_session_lifecycle_dispatch_defaults_to_not_joined_without_sender() {
        let joined = dispatch_child_session_lifecycle(
            None,
            ChildSessionLifecyclePayload {
                kind: ChildSessionLifecycleKind::Spawned,
                task_id: "task-123".to_string(),
                task_label: "Child task".to_string(),
                instruction: "Do work".to_string(),
                parent_session_key: "api:parent".to_string(),
                child_session_key: "api:parent#child-task-123".to_string(),
                workflow_kind: Some("deep_research".to_string()),
                current_phase: Some("execute".to_string()),
                output_files: Vec::new(),
                failure_action: None,
                error: None,
            },
        )
        .await;

        assert!(!joined);
    }

    // Minimal mock provider for testing
    struct MockProvider;

    struct ShellThenEndProvider {
        calls: std::sync::atomic::AtomicUsize,
    }

    #[async_trait]
    impl LlmProvider for MockProvider {
        async fn chat(
            &self,
            _messages: &[octos_core::Message],
            _tools: &[octos_llm::ToolSpec],
            _config: &octos_llm::ChatConfig,
        ) -> Result<octos_llm::ChatResponse> {
            Ok(octos_llm::ChatResponse {
                content: Some("done".into()),
                reasoning_content: None,
                tool_calls: vec![],
                stop_reason: octos_llm::StopReason::EndTurn,
                usage: octos_llm::TokenUsage {
                    input_tokens: 0,
                    output_tokens: 0,
                    ..Default::default()
                },
                provider_index: None,
            })
        }

        fn model_id(&self) -> &str {
            "mock"
        }

        fn provider_name(&self) -> &str {
            "mock"
        }
    }

    #[async_trait]
    impl LlmProvider for ShellThenEndProvider {
        async fn chat(
            &self,
            _messages: &[octos_core::Message],
            _tools: &[octos_llm::ToolSpec],
            _config: &octos_llm::ChatConfig,
        ) -> Result<octos_llm::ChatResponse> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            if call == 0 {
                return Ok(octos_llm::ChatResponse {
                    content: None,
                    reasoning_content: None,
                    tool_calls: vec![octos_core::ToolCall {
                        id: "call_shell".into(),
                        name: "shell".into(),
                        arguments: serde_json::json!({
                            "command": "printf ready",
                        }),
                        metadata: None,
                    }],
                    stop_reason: octos_llm::StopReason::ToolUse,
                    usage: octos_llm::TokenUsage::default(),
                    provider_index: None,
                });
            }

            Ok(octos_llm::ChatResponse {
                content: Some("done".into()),
                reasoning_content: None,
                tool_calls: vec![],
                stop_reason: octos_llm::StopReason::EndTurn,
                usage: octos_llm::TokenUsage::default(),
                provider_index: None,
            })
        }

        fn model_id(&self) -> &str {
            "mock"
        }

        fn provider_name(&self) -> &str {
            "mock"
        }
    }

    async fn create_test_store() -> EpisodeStore {
        let dir = tempfile::tempdir().unwrap();
        // Leak the dir so it stays alive for the test
        let dir = Box::leak(Box::new(dir));
        EpisodeStore::open(dir.path()).await.unwrap()
    }
}
