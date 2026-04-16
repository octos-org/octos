//! Agent implementation.

mod activity;
mod budget;
mod compaction;
mod detection;
mod execution;
mod llm_call;
mod loop_compaction;
mod loop_runner;
mod memory;
mod message_repair;
mod streaming;
mod turn_state;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32};
use std::sync::{Arc, RwLock};

use octos_core::{AgentId, Message, TokenUsage};
use octos_llm::{EmbeddingProvider, LlmProvider, ProviderMetadata};
use octos_memory::EpisodeStore;

use crate::hooks::{HookContext, HookExecutor};
use crate::progress::{ProgressReporter, SilentReporter};
use crate::tools::ToolRegistry;

tokio::task_local! {
    /// Task-local reporter override.  When set (via `TASK_REPORTER.scope()`),
    /// `Agent::reporter()` returns this instead of the instance-level RwLock
    /// reporter.  This lets concurrent overflow tasks each have their own
    /// stream reporter without mutating the shared `Arc<Agent>`.
    pub static TASK_REPORTER: Arc<dyn ProgressReporter>;
}

/// Compiled-in default worker prompt (from `prompts/worker.txt`).
pub const DEFAULT_WORKER_PROMPT: &str = include_str!("../prompts/worker.txt");

/// Configuration for agent execution.
#[derive(Debug, Clone)]
pub struct AgentConfig {
    /// Maximum number of iterations before stopping.
    pub max_iterations: u32,
    /// Maximum total tokens (input + output) before stopping. None = unlimited.
    pub max_tokens: Option<u32>,
    /// Wall-clock timeout for the entire agent run. None = unlimited.
    pub max_timeout: Option<std::time::Duration>,
    /// Whether to save episodes to memory.
    pub save_episodes: bool,
    /// Optional worker system prompt override (used by Agent::new as the default prompt).
    /// When None, falls back to the compiled-in prompts/worker.txt.
    pub worker_prompt: Option<String>,
    /// Maximum seconds for all parallel tool calls to complete. Default: 300.
    pub tool_timeout_secs: u64,
    /// Per-call max output tokens override. When set, overrides `ChatConfig::default()`.
    /// Useful for pipeline nodes that produce long outputs (e.g. synthesize).
    pub chat_max_tokens: Option<u32>,
}

/// Default tool execution timeout in seconds.
pub const DEFAULT_TOOL_TIMEOUT_SECS: u64 = 600;
/// Maximum tool timeout the LLM can request (30 minutes).
pub const MAX_TOOL_TIMEOUT_SECS: u64 = 1800;
/// Default session processing timeout in seconds.
pub const DEFAULT_SESSION_TIMEOUT_SECS: u64 = 1800;

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_iterations: 50,
            max_tokens: None,
            max_timeout: Some(std::time::Duration::from_secs(600)),
            save_episodes: true,
            worker_prompt: None,
            tool_timeout_secs: DEFAULT_TOOL_TIMEOUT_SECS,
            chat_max_tokens: None,
        }
    }
}

/// Response from conversation mode (process_message).
#[derive(Debug, Clone)]
pub struct ConversationResponse {
    pub content: String,
    /// Reasoning/thinking content from thinking models (o1, DeepSeek, kimi, etc.).
    pub reasoning_content: Option<String>,
    /// Exact provider instance provenance for the final assistant reply.
    pub provider_metadata: Option<ProviderMetadata>,
    pub token_usage: TokenUsage,
    pub files_modified: Vec<PathBuf>,
    pub streamed: bool,
    /// All messages generated during processing (assistant replies, tool calls,
    /// tool results). Includes the user message at the front. Callers should
    /// persist these to session history so subsequent calls see the full context.
    pub messages: Vec<Message>,
}

/// Shared atomic counters for real-time token tracking (used by status indicators).
pub struct TokenTracker {
    pub input_tokens: AtomicU32,
    pub output_tokens: AtomicU32,
}

impl TokenTracker {
    pub fn new() -> Self {
        Self {
            input_tokens: AtomicU32::new(0),
            output_tokens: AtomicU32::new(0),
        }
    }
}

impl Default for TokenTracker {
    fn default() -> Self {
        Self::new()
    }
}

/// An agent that can execute tasks.
pub struct Agent {
    /// Unique identifier for this agent.
    pub id: AgentId,
    /// LLM provider for generating responses.
    pub(super) llm: Arc<dyn LlmProvider>,
    /// Tool registry for executing tool calls (Arc for sharing with spawned tool tasks).
    pub(super) tools: Arc<ToolRegistry>,
    /// Episode store for memory.
    pub(super) memory: Arc<EpisodeStore>,
    /// Embedding provider for hybrid memory search.
    pub(super) embedder: Option<Arc<dyn EmbeddingProvider>>,
    /// System prompt for this agent (RwLock for hot-reload support).
    pub(super) system_prompt: RwLock<String>,
    /// Agent configuration.
    pub(super) config: AgentConfig,
    /// Progress reporter (RwLock for interior-mutable swap without &mut self).
    pub(super) reporter: RwLock<Arc<dyn ProgressReporter>>,
    /// Lifecycle hooks executor.
    pub(super) hooks: Option<Arc<HookExecutor>>,
    /// Session-level context for hook payloads.
    pub(super) hook_context: std::sync::Mutex<Option<HookContext>>,
    /// Shutdown signal.
    pub(super) shutdown: Arc<AtomicBool>,
}

impl Agent {
    /// Create a new agent.
    pub fn new(
        id: AgentId,
        llm: Arc<dyn LlmProvider>,
        tools: ToolRegistry,
        memory: Arc<EpisodeStore>,
    ) -> Self {
        let system_prompt = include_str!("../prompts/worker.txt").to_string();

        Self {
            id,
            llm,
            tools: Arc::new(tools),
            memory,
            embedder: None,
            system_prompt: RwLock::new(system_prompt),
            config: AgentConfig::default(),
            reporter: RwLock::new(Arc::new(SilentReporter)),
            hooks: None,
            hook_context: std::sync::Mutex::new(None),
            shutdown: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Create a new agent sharing pre-existing Arc-wrapped resources.
    /// Useful for per-request agents that share tools/memory with a base agent.
    pub fn new_shared(
        id: AgentId,
        llm: Arc<dyn LlmProvider>,
        tools: Arc<ToolRegistry>,
        memory: Arc<EpisodeStore>,
    ) -> Self {
        let system_prompt = include_str!("../prompts/worker.txt").to_string();

        Self {
            id,
            llm,
            tools,
            memory,
            embedder: None,
            system_prompt: RwLock::new(system_prompt),
            config: AgentConfig::default(),
            reporter: RwLock::new(Arc::new(SilentReporter)),
            hooks: None,
            hook_context: std::sync::Mutex::new(None),
            shutdown: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Wire the `activate_tools` tool's back-reference to the shared tool registry.
    /// Must be called after construction if `ActivateToolsTool` was registered.
    pub fn wire_activate_tools(&self) {
        use crate::tools::activate_tools::ActivateToolsTool;
        if let Some(tool) = self.tools.get("activate_tools") {
            if let Some(at) = tool.as_any().downcast_ref::<ActivateToolsTool>() {
                at.set_registry(Arc::downgrade(&self.tools));
            }
        }
    }

    /// Set the agent configuration.
    pub fn with_config(mut self, config: AgentConfig) -> Self {
        // Apply worker_prompt override if provided.
        // Lock poisoning recovery: safe — we just need the inner value.
        // A poisoned lock means a prior holder panicked, but the String
        // data itself is still valid and overwritten here.
        if let Some(ref wp) = config.worker_prompt {
            *self
                .system_prompt
                .write()
                .unwrap_or_else(|e| e.into_inner()) = wp.clone();
        }
        self.config = config;
        self
    }

    /// Set the progress reporter.
    pub fn with_reporter(self, reporter: Arc<dyn ProgressReporter>) -> Self {
        *self.reporter.write().unwrap_or_else(|e| e.into_inner()) = reporter;
        self
    }

    /// Replace the progress reporter at runtime (e.g. per-message stream reporter).
    /// Takes `&self` (not `&mut self`) -- uses interior mutability via RwLock so
    /// the agent can be behind an Arc for concurrent speculative overflow.
    pub fn set_reporter(&self, reporter: Arc<dyn ProgressReporter>) {
        *self.reporter.write().unwrap_or_else(|e| e.into_inner()) = reporter;
    }

    /// Get a clone of the current reporter.
    ///
    /// Checks `TASK_REPORTER` task-local first (set per-overflow-task), then
    /// falls back to the instance-level RwLock reporter.
    pub(super) fn reporter(&self) -> Arc<dyn ProgressReporter> {
        TASK_REPORTER.try_with(|r| r.clone()).unwrap_or_else(|_| {
            self.reporter
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
        })
    }

    /// Set the shutdown signal.
    pub fn with_shutdown(mut self, shutdown: Arc<AtomicBool>) -> Self {
        self.shutdown = shutdown;
        self
    }

    /// Set the embedding provider for hybrid memory search.
    pub fn with_embedder(mut self, embedder: Arc<dyn EmbeddingProvider>) -> Self {
        self.embedder = Some(embedder);
        self
    }

    /// Set lifecycle hooks executor.
    pub fn with_hooks(mut self, hooks: Arc<HookExecutor>) -> Self {
        self.hooks = Some(hooks);
        self
    }

    /// Set session-level context for hook payloads.
    pub fn with_hook_context(self, ctx: HookContext) -> Self {
        *self.hook_context.lock().unwrap_or_else(|e| e.into_inner()) = Some(ctx);
        self
    }

    /// Update the session ID in the hook context (call before each message).
    pub fn set_session_id(&self, session_id: &str) {
        let mut guard = self.hook_context.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(ref mut ctx) = *guard {
            ctx.session_id = Some(session_id.to_string());
        }
    }

    /// Get a snapshot of the current hook context.
    pub(super) fn hook_ctx(&self) -> Option<HookContext> {
        self.hook_context
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Override the system prompt (e.g. for gateway mode).
    pub fn with_system_prompt(self, prompt: String) -> Self {
        *self.system_prompt.write().unwrap_or_else(|e| {
            tracing::warn!("system prompt lock was poisoned, recovering");
            e.into_inner()
        }) = prompt;
        self
    }

    /// Append additional content to the current system prompt (e.g. bootstrap files).
    pub fn append_system_prompt(&self, extra: &str) {
        let mut guard = self.system_prompt.write().unwrap_or_else(|e| {
            tracing::warn!("system prompt lock was poisoned, recovering");
            e.into_inner()
        });
        guard.push_str("\n\n");
        guard.push_str(extra);
    }

    /// Update the system prompt at runtime (hot-reload).
    pub fn set_system_prompt(&self, prompt: String) {
        *self.system_prompt.write().unwrap_or_else(|e| {
            tracing::warn!("system prompt lock was poisoned, recovering");
            e.into_inner()
        }) = prompt;
    }

    /// The LLM model ID in use.
    pub fn model_id(&self) -> &str {
        self.llm.model_id()
    }

    /// The LLM provider name in use.
    pub fn provider_name(&self) -> &str {
        self.llm.provider_name()
    }

    /// Get a reference to the LLM provider (for sharing with per-request agents).
    pub fn llm_provider(&self) -> Arc<dyn LlmProvider> {
        self.llm.clone()
    }

    /// Get a reference to the tool registry.
    pub fn tool_registry(&self) -> &Arc<ToolRegistry> {
        &self.tools
    }

    /// Get a reference to the episode store.
    pub fn memory_store(&self) -> Arc<EpisodeStore> {
        self.memory.clone()
    }

    /// Get a clone of the agent config.
    pub fn agent_config(&self) -> AgentConfig {
        self.config.clone()
    }

    /// Get a snapshot of the current system prompt.
    pub fn system_prompt_snapshot(&self) -> String {
        self.system_prompt
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}
