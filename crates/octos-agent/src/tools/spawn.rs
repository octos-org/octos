//! Spawn tool for background subagent execution.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use async_trait::async_trait;
use eyre::{Result, WrapErr};
use octos_core::{AgentId, InboundMessage, Task, TaskContext, TaskKind};
use octos_llm::{ContextWindowOverride, LlmProvider, ProviderRouter};
use octos_memory::EpisodeStore;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use super::{Tool, ToolPolicy, ToolRegistry, ToolResult};
use crate::task_supervisor::TaskSupervisor;
use crate::{Agent, AgentConfig};

/// Callback for delivering background task results directly to the session actor.
/// Returns `true` if the result was delivered, `false` if the actor is dead
/// (caller should fall back to the InboundMessage relay path).
pub type BackgroundResultSender =
    Arc<dyn Fn(BackgroundResultPayload) -> futures::future::BoxFuture<'static, bool> + Send + Sync>;

pub type ChildSessionLifecycleSender = Arc<
    dyn Fn(ChildSessionLifecyclePayload) -> futures::future::BoxFuture<'static, ()> + Send + Sync,
>;

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
    Failed,
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
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WorkflowTerminalOutputPolicy {
    deliver_final_artifact_only: bool,
    deliver_media_only: bool,
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
    /// Plugin directories to load into subagent registries.
    /// Subagents can use plugin tools (fm_tts, etc.) when listed in allowed_tools.
    plugin_dirs: Vec<PathBuf>,
    /// Extra environment variables for plugin processes.
    plugin_extra_env: Vec<(String, String)>,
    /// Shared task supervisor so background subagents show up in task tracking.
    task_supervisor: Option<Arc<TaskSupervisor>>,
    /// Owning session key for tracked background subagents.
    session_key: Option<String>,
    /// Append-only task ledger path for the owning parent session.
    task_ledger_path: Option<PathBuf>,
    /// Optional agent config inherited from the parent session.
    worker_config: Option<AgentConfig>,
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
            plugin_dirs: Vec::new(),
            plugin_extra_env: Vec::new(),
            task_supervisor: None,
            session_key: None,
            task_ledger_path: None,
            worker_config: None,
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
            plugin_dirs: Vec::new(),
            plugin_extra_env: Vec::new(),
            task_supervisor: None,
            session_key: None,
            task_ledger_path: None,
            worker_config: None,
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

fn workflow_artifact_matches_kind(path: &PathBuf, kind: &str) -> bool {
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
        files_to_send.to_vec()
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

async fn deliver_background_result(
    sender: Option<BackgroundResultSender>,
    payload: BackgroundResultPayload,
) -> bool {
    match sender {
        Some(sender) => sender(payload).await,
        None => false,
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
                                "deliver_media_only": { "type": "boolean" },
                                "forbid_intermediate_files": { "type": "boolean" },
                                "required_artifact_kind": { "type": "string" }
                            }
                        }
                    },
                    "required": ["workflow_kind", "current_phase"]
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

        info!(
            worker_id = %worker_id,
            mode = %input.mode,
            task = %input.task,
            "spawning subagent"
        );

        let sub_llm = self.resolve_sub_provider(input.model.as_deref(), input.context_window)?;

        if is_sync {
            // Sync mode: run subagent inline and return the result directly
            let mut tools = ToolRegistry::with_builtins(&self.working_dir);
            // Load plugin tools so subagents can use fm_tts, etc.
            if !self.plugin_dirs.is_empty() {
                let _ = crate::plugins::PluginLoader::load_into(
                    &mut tools,
                    &self.plugin_dirs,
                    &self.plugin_extra_env,
                );
            }
            // In subagent context, spawn_only tools should be regular tools —
            // the subagent IS the background, so no need to auto-background again.
            tools.clear_spawn_only();
            let policy = ToolPolicy {
                allow: allowed_tools,
                deny: vec!["spawn".into()],
                ..Default::default()
            };
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
            let task_supervisor = self.task_supervisor.clone();
            let worker_config = self.worker_config.clone();
            let workflow_metadata = workflow.clone();
            let parent_session_key = self.session_key.clone();

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

                if let (
                    Some(sender),
                    Some(task_id),
                    Some(parent_session_key),
                    Some(child_session_key),
                ) = (
                    child_session_sender.as_ref(),
                    tracked_task_id.as_ref(),
                    parent_session_key.as_ref(),
                    tracked_child_session_key.as_ref(),
                ) {
                    sender(ChildSessionLifecyclePayload {
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
                        error: None,
                    })
                    .await;
                }

                let mut tools = ToolRegistry::with_builtins(&working_dir);
                // Load plugin tools so subagents can use fm_tts, etc.
                if !plugin_dirs.is_empty() {
                    let _ = crate::plugins::PluginLoader::load_into(
                        &mut tools,
                        &plugin_dirs,
                        &plugin_extra_env,
                    );
                }
                // In subagent context, spawn_only tools should be regular tools —
                // the subagent IS the background, so no need to auto-background again.
                tools.clear_spawn_only();
                let policy = ToolPolicy {
                    allow: allowed_tools,
                    deny: vec!["spawn".into()],
                    ..Default::default()
                };
                tools.apply_policy(&policy);
                if let Some(pp) = provider_policy {
                    tools.set_provider_policy(pp);
                }
                let mut worker = Agent::new(wid.clone(), llm, tools, memory);
                if let Some(ref config) = worker_config {
                    worker = worker.with_config(config.clone());
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
                        working_dir,
                        ..Default::default()
                    },
                );

                let result = worker.run_task(&subtask).await;
                let tracked_output_files = match &result {
                    Ok(task_result) => select_workflow_terminal_files(
                        &task_result.files_to_send,
                        &task_result.files_modified,
                        workflow_metadata.as_ref(),
                    )
                    .unwrap_or_else(|| {
                        task_result
                            .files_to_send
                            .iter()
                            .chain(task_result.files_modified.iter())
                            .cloned()
                            .collect()
                    })
                    .into_iter()
                    .map(|path| path.to_string_lossy().to_string())
                    .collect::<Vec<_>>(),
                    Err(_) => Vec::new(),
                };

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

                if let (Some(supervisor), Some(task_id)) =
                    (task_supervisor.as_ref(), tracked_task_id.as_ref())
                {
                    match &result {
                        Ok(task_result) if task_result.success => {
                            supervisor.mark_completed(task_id, tracked_output_files.clone());
                        }
                        Ok(task_result) => {
                            supervisor.mark_failed(task_id, task_result.output.clone());
                        }
                        Err(error) => {
                            supervisor.mark_failed(task_id, error.to_string());
                        }
                    }
                }

                if let (
                    Some(sender),
                    Some(task_id),
                    Some(parent_session_key),
                    Some(child_session_key),
                ) = (
                    child_session_sender.as_ref(),
                    tracked_task_id.as_ref(),
                    parent_session_key.as_ref(),
                    tracked_child_session_key.as_ref(),
                ) {
                    let payload = match &result {
                        Ok(task_result) if task_result.success => ChildSessionLifecyclePayload {
                            kind: ChildSessionLifecycleKind::Completed,
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
                            error: None,
                        },
                        Ok(task_result) => ChildSessionLifecyclePayload {
                            kind: ChildSessionLifecycleKind::Failed,
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
                            error: Some(task_result.output.clone()),
                        },
                        Err(error) => ChildSessionLifecyclePayload {
                            kind: ChildSessionLifecycleKind::Failed,
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
                            error: Some(error.to_string()),
                        },
                    };
                    sender(payload).await;
                }

                let content = match &result {
                    Ok(r) => format!(
                        "Status: {}\n\n{}",
                        if r.success { "SUCCESS" } else { "FAILED" },
                        r.output
                    ),
                    Err(e) => format!("Status: FAILED\nError: {e}"),
                };
                let (result_kind, result_media) = match &result {
                    Ok(r) if r.success => {
                        let workflow_media = select_workflow_terminal_files(
                            &r.files_to_send,
                            &r.files_modified,
                            workflow_metadata.as_ref(),
                        )
                        .unwrap_or_default();
                        if !workflow_media.is_empty() {
                            (
                                BackgroundResultKind::Notification,
                                workflow_media
                                    .into_iter()
                                    .map(|path| path.to_string_lossy().to_string())
                                    .collect::<Vec<_>>(),
                            )
                        } else if should_deliver_output_files(&r.files_to_send) {
                            (
                                BackgroundResultKind::Notification,
                                r.files_to_send
                                    .iter()
                                    .map(|path| path.to_string_lossy().to_string())
                                    .collect::<Vec<_>>(),
                            )
                        } else {
                            (BackgroundResultKind::Report, Vec::new())
                        }
                    }
                    _ => (BackgroundResultKind::Report, Vec::new()),
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
                    warn!(error = %e, "failed to announce subagent result");
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
            plugin_dirs: Vec::new(),
            plugin_extra_env: Vec::new(),
            task_supervisor: None,
            session_key: None,
            task_ledger_path: None,
            worker_config: None,
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

    #[test]
    fn workflow_terminal_output_prefers_final_audio_and_skips_intermediates() {
        let workflow = WorkflowMetadata {
            workflow_kind: "research_podcast".to_string(),
            current_phase: "generate_audio".to_string(),
            allowed_tools: vec!["podcast_generate".to_string()],
            terminal_output: Some(WorkflowTerminalOutputPolicy {
                deliver_final_artifact_only: true,
                deliver_media_only: true,
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
    fn workflow_terminal_output_prefers_final_presentation_and_skips_scratch_files() {
        let workflow = WorkflowMetadata {
            workflow_kind: "slides".to_string(),
            current_phase: "deliver_result".to_string(),
            allowed_tools: vec!["mofa_slides".to_string()],
            terminal_output: Some(WorkflowTerminalOutputPolicy {
                deliver_final_artifact_only: true,
                deliver_media_only: false,
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
                deliver_media_only: false,
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

    #[tokio::test]
    async fn test_background_spawn_persists_workflow_phase_transitions() {
        let (in_tx, _in_rx) = tokio::sync::mpsc::channel(16);
        let temp = tempfile::tempdir().unwrap();
        let ledger = temp.path().join("tasks.jsonl");
        let supervisor = Arc::new(TaskSupervisor::new());
        supervisor.enable_persistence(&ledger).unwrap();
        let tool = SpawnTool::new(
            Arc::new(MockProvider),
            Arc::new(create_test_store().await),
            PathBuf::from("/tmp"),
            in_tx,
        )
        .with_task_supervisor(supervisor.clone(), "api:test-session", ledger.clone());

        let result = tool
            .execute(&serde_json::json!({
                "task": "Produce a short podcast",
                "label": "Research podcast",
                "mode": "background",
                "allowed_tools": ["podcast_generate"],
                "workflow": {
                    "workflow_kind": "research_podcast",
                    "current_phase": "research",
                    "allowed_tools": ["podcast_generate"],
                    "terminal_output": {
                        "deliver_final_artifact_only": true,
                        "deliver_media_only": true,
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

    // Minimal mock provider for testing
    struct MockProvider;

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

    async fn create_test_store() -> EpisodeStore {
        let dir = tempfile::tempdir().unwrap();
        // Leak the dir so it stays alive for the test
        let dir = Box::leak(Box::new(dir));
        EpisodeStore::open(dir.path()).await.unwrap()
    }
}
