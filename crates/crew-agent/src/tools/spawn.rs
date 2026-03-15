//! Spawn tool for background subagent execution.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use async_trait::async_trait;
use crew_core::{AgentId, InboundMessage, Task, TaskContext, TaskKind};
use crew_llm::{ContextWindowOverride, LlmProvider, ProviderRouter};
use crew_memory::EpisodeStore;
use eyre::{Result, WrapErr};
use serde::Deserialize;
use tracing::{info, warn};

use super::{Tool, ToolPolicy, ToolRegistry, ToolResult};
use crate::Agent;

/// Callback for delivering background task results directly to the session actor.
/// Returns `true` if the result was delivered, `false` if the actor is dead
/// (caller should fall back to the InboundMessage relay path).
pub type BackgroundResultSender =
    Arc<dyn Fn(String, String) -> futures::future::BoxFuture<'static, bool> + Send + Sync>;

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
        }
    }

    /// Set a direct result sender that bypasses the InboundMessage relay.
    /// When set, background task results are injected as system messages
    /// into the session without triggering an extra LLM call.
    pub fn with_background_result_sender(mut self, sender: BackgroundResultSender) -> Self {
        self.background_result_sender = Some(sender);
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
    /// Custom system prompt for the sub-agent (replaces default worker prompt).
    #[serde(default)]
    system_prompt: Option<String>,
}

fn default_mode() -> String {
    "background".into()
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
                "system_prompt": {
                    "type": "string",
                    "description": "Custom system prompt that defines the subagent's role and behavior. Replaces the default worker prompt. Use this to specialize the subagent (e.g. 'You are a security-focused code reviewer. Flag OWASP Top 10 issues.')."
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
            if let Some(ref sp) = input.system_prompt {
                worker = worker.with_system_prompt(sp.clone());
            } else if let Some(ref wp) = self.worker_prompt {
                worker = worker.with_system_prompt(wp.clone());
            }

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
            let llm = sub_llm;
            let memory = self.memory.clone();
            let working_dir = self.working_dir.clone();
            let inbound_tx = self.inbound_tx.clone();
            let wid = worker_id.clone();
            let provider_policy = self.provider_policy.clone();
            let custom_system_prompt = input.system_prompt;
            let default_worker_prompt = self.worker_prompt.clone();
            let bg_sender = self.background_result_sender.clone();
            let task_label = label.clone();

            tokio::spawn(async move {
                let mut tools = ToolRegistry::with_builtins(&working_dir);
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
                if let Some(sp) = custom_system_prompt {
                    worker = worker.with_system_prompt(sp);
                } else if let Some(wp) = default_worker_prompt {
                    worker = worker.with_system_prompt(wp);
                }

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

                let content = match &result {
                    Ok(r) => format!(
                        "Status: {}\n\n{}",
                        if r.success { "SUCCESS" } else { "FAILED" },
                        r.output
                    ),
                    Err(e) => format!("Status: FAILED\nError: {e}"),
                };

                // Direct injection path: inject as system message, no extra LLM call.
                // If the actor has exited (idle timeout), the send fails and we
                // fall through to the legacy InboundMessage relay path.
                if let Some(sender) = bg_sender {
                    if sender(task_label, content.clone()).await {
                        return;
                    }
                    warn!("background result sender failed (actor dead?), falling back to relay");
                }

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
        };

        assert_eq!(tool.worker_count.load(Ordering::SeqCst), 0);

        // Invalid input test
        let result = tool.execute(&serde_json::json!({})).await;
        assert!(result.is_err());

        // Worker count should not increment on invalid input
        assert_eq!(tool.worker_count.load(Ordering::SeqCst), 0);
    }

    // Minimal mock provider for testing
    struct MockProvider;

    #[async_trait]
    impl LlmProvider for MockProvider {
        async fn chat(
            &self,
            _messages: &[crew_core::Message],
            _tools: &[crew_llm::ToolSpec],
            _config: &crew_llm::ChatConfig,
        ) -> Result<crew_llm::ChatResponse> {
            Ok(crew_llm::ChatResponse {
                content: Some("done".into()),
                reasoning_content: None,
                tool_calls: vec![],
                stop_reason: crew_llm::StopReason::EndTurn,
                usage: crew_llm::TokenUsage {
                    input_tokens: 0,
                    output_tokens: 0,
                    ..Default::default()
                },
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
