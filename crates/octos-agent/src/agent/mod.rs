//! Agent implementation.

mod activity;
mod budget;
mod compaction;
mod detection;
mod execution;
mod llm_call;
mod loop_compaction;
mod loop_runner;
pub mod loop_state;
mod memory;
mod message_repair;
pub mod realtime;
mod streaming;
mod turn_state;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, RwLock};

use octos_core::{AgentId, Message, TokenUsage};
use octos_llm::{EmbeddingProvider, LlmProvider, ProviderMetadata};
use octos_memory::EpisodeStore;

use crate::file_state_cache::FileStateCache;
use crate::hooks::{HookContext, HookExecutor};
use crate::progress::{ProgressReporter, SilentReporter};
use crate::session::{SessionLimits, SessionUsage};
use crate::tools::ToolRegistry;

pub use realtime::RealtimeController;

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
    /// Activity timeout for the entire agent run. None = unlimited.
    /// This is only enforced when the loop has not reported recent progress,
    /// so active long-running turns are not killed just because wall time grew.
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
    /// Suppress the generic auto-send loop for tool `files_to_send`.
    /// Background spawned workers rely on their outer workflow/session runtime
    /// to persist terminal results exactly once.
    pub suppress_auto_send_files: bool,
}

/// Default tool execution timeout in seconds.
/// Matches `MAX_TOOL_TIMEOUT_SECS` so long-running tools like `run_pipeline`
/// (default 1800s) are not silently capped when the LLM omits `timeout_secs`
/// in the tool call.
pub const DEFAULT_TOOL_TIMEOUT_SECS: u64 = 1800;
/// Maximum tool timeout the LLM can request (30 minutes).
pub const MAX_TOOL_TIMEOUT_SECS: u64 = 1800;
/// Default session processing timeout in seconds.
pub const DEFAULT_SESSION_TIMEOUT_SECS: u64 = 1800;

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_iterations: 50,
            max_tokens: None,
            max_timeout: Some(std::time::Duration::from_secs(1800)),
            save_episodes: true,
            worker_prompt: None,
            tool_timeout_secs: DEFAULT_TOOL_TIMEOUT_SECS,
            chat_max_tokens: None,
            suppress_auto_send_files: false,
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
    pub files_to_send: Vec<PathBuf>,
    pub streamed: bool,
    /// All messages generated during processing (assistant replies, tool calls,
    /// tool results). Includes the user message at the front. Callers should
    /// persist these to session history so subsequent calls see the full context.
    pub messages: Vec<Message>,
    /// Structured side-channel metadata surfaced by tools that ran during
    /// this conversation, keyed by `tool_call_id`. Used today for per-node
    /// cost rows from `run_pipeline` (`{"node_costs": [...]}`); the session
    /// actor pulls these into the SSE `done` event so the W1.G4 cost panel
    /// can render real per-node attribution. Empty when no tool opted in.
    pub tool_results: Vec<(String, serde_json::Value)>,
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
    /// Local harness event sink path shared with child tools in this agent.
    pub(super) harness_event_sink: Option<String>,
    /// Shutdown signal.
    pub(super) shutdown: Arc<AtomicBool>,
    /// Tracks whether the LOOP DETECTED warning has already fired in the
    /// current session-burst. Reset at the start of each `process_message`
    /// invocation; if a second loop fire happens within the same turn (e.g.
    /// re-engagement before the turn ends), the duplicate warning is replaced
    /// by a terminal error so the loop cannot keep emitting identical noise.
    pub(super) loop_detected_recently: Arc<AtomicBool>,
    /// Optional per-session runtime limits for tool rounds and per-tool calls.
    pub(super) session_limits: Option<SessionLimits>,
    /// Mutable usage tracked against `session_limits`.
    pub(super) session_usage: std::sync::Mutex<SessionUsage>,
    /// Optional realtime controller (heartbeat + sensor context injector) for
    /// robotics operators. Absent by default -- the agent loop behaves exactly
    /// as before when this is `None`.
    pub(super) realtime: Option<Arc<RealtimeController>>,
    /// Harness M6.3 compaction contract. When present, the loop performs
    /// preflight compaction before the first LLM call, swaps the summarizer
    /// flavour declared in policy, prunes old tool results to typed
    /// placeholders, and gates post-compaction artifact preservation. Absent
    /// = legacy extractive path behaves exactly as before M6.3.
    pub(super) compaction_runner: Option<Arc<crate::compaction::CompactionRunner>>,
    /// Workspace policy associated with the compaction runner (used by the
    /// post-compaction validator rail to resolve preserved artifacts).
    pub(super) compaction_workspace: Option<crate::workspace_policy::WorkspacePolicy>,
    /// Cross-turn persistent retry bucket state (Review A F-015). When
    /// present, the loop uses this shared state instead of constructing a
    /// fresh `LoopRetryState` per `process_message` / `run_task`. Callers
    /// (e.g. `SessionActor`) own the save/load lifecycle via the
    /// `LoopRetryState::Serialize + Deserialize` impls. Absent = legacy
    /// per-turn-reset behaviour, identical to every pre-F-015 caller.
    pub(super) persistent_retry_state:
        Option<Arc<std::sync::Mutex<crate::agent::loop_state::LoopRetryState>>>,
    /// M8.2 agent manifest registry shared with tools via `ToolContext`.
    /// Shared behind an `Arc` so every per-tool `ToolContext::agent_definitions`
    /// clone is O(1). When left at the default (empty registry) the agent
    /// behaves exactly as pre-M8.2.
    pub(super) agent_definitions: Arc<crate::agents::AgentDefinitions>,
    /// Optional shared [`FileStateCache`] threaded into every
    /// [`crate::tools::ToolContext`] so file tools can short-circuit
    /// re-reads (M8.4). `None` keeps pre-M8.4 behaviour.
    pub(super) file_state_cache: Option<Arc<FileStateCache>>,
    /// M8.3 profile envelope applied at bootstrap. Recorded so callers can
    /// introspect the active profile name, compaction overrides, and model
    /// preferences. `None` means no profile was explicitly applied — the
    /// agent runs in legacy pre-M8.3 mode.
    pub(super) profile: Option<Arc<crate::profile::ProfileDefinition>>,
    /// Three-tier compaction runner (harness M8.5). Optional — when wired,
    /// the loop runs tier 1 (micro-compaction) at the top of each iteration
    /// and decorates Anthropic requests with the tier-2
    /// `context_management` payload. Tier 3 delegates to the existing
    /// [`crate::compaction::CompactionRunner`] wrapped as a
    /// [`crate::compaction_tiered::FullCompactor`].
    pub(super) tiered_compaction: Option<Arc<crate::compaction_tiered::TieredCompactionRunner>>,
    /// M8.7 sub-agent output router. When configured, the spawn_only
    /// background branch in `execution.rs` calls
    /// [`crate::SubAgentOutputRouter::mark_terminal`] when a task ends so
    /// dashboards can stop tailing the on-disk output log. `None` keeps
    /// pre-M8.7 behaviour.
    pub(super) subagent_output_router: Option<Arc<crate::subagent_output::SubAgentOutputRouter>>,
    /// M8.7 sub-agent progress summary generator. When configured, the
    /// spawn_only background branch starts a watcher per task and stops
    /// it on terminal completion. `None` keeps pre-M8.7 behaviour.
    pub(super) subagent_summary_generator:
        Option<Arc<crate::subagent_summary::AgentSummaryGenerator>>,
    /// M8 parity (W1.A4): optional shared cost accountant. When set,
    /// the agent threads it onto every `ToolContext` so background
    /// sub-agents (pipeline workers, spawn children) reserve and commit
    /// against the same ledger as the parent session.
    pub(super) cost_accountant: Option<Arc<crate::cost_ledger::CostAccountant>>,
    /// M8 parity: optional parent session key. When the agent is owned
    /// by a session actor, this carries the session key down through
    /// `ToolContext.parent_session_key` so spawn children / pipeline
    /// workers can register tasks against the owning session.
    pub(super) parent_session_key: Option<String>,
    /// Guard C (issue #607): nesting depth this agent's tool calls
    /// inherit via `ToolContext.spawn_depth`. The session-actor's
    /// top-level agent leaves this at 0; sub-agents created by the
    /// `spawn` tool set it via [`Self::with_spawn_depth`] so the
    /// child's own spawn calls see the higher value and the
    /// `MAX_SPAWN_DEPTH` gate fires after a bounded number of nests.
    pub(super) spawn_depth: u8,
    /// M9 review fix (HIGH #1): the effective [`crate::sandbox::SandboxConfig`]
    /// that built this agent's `ShellTool` sandbox. Recorded so per-session
    /// callers (notably the AppUi `session_tool_registry` rebind path) can
    /// re-create a sandbox that inherits the running server's policy
    /// (mode, network, read paths, profile) instead of silently dropping
    /// back to `SandboxConfig::default()` and disabling features like
    /// `npm install` that need network or specific read paths.
    /// `None` keeps legacy behaviour — callers that don't track the sandbox
    /// config (chat, gateway, tests) get the previous default.
    pub(super) sandbox_config: Option<crate::sandbox::SandboxConfig>,
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
            harness_event_sink: None,
            shutdown: Arc::new(AtomicBool::new(false)),
            loop_detected_recently: Arc::new(AtomicBool::new(false)),
            session_limits: None,
            session_usage: std::sync::Mutex::new(SessionUsage::default()),
            realtime: None,
            compaction_runner: None,
            compaction_workspace: None,
            persistent_retry_state: None,
            agent_definitions: Arc::new(crate::agents::AgentDefinitions::new()),
            file_state_cache: None,
            profile: None,
            tiered_compaction: None,
            subagent_output_router: None,
            subagent_summary_generator: None,
            cost_accountant: None,
            parent_session_key: None,
            spawn_depth: 0,
            sandbox_config: None,
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
            harness_event_sink: None,
            shutdown: Arc::new(AtomicBool::new(false)),
            loop_detected_recently: Arc::new(AtomicBool::new(false)),
            session_limits: None,
            session_usage: std::sync::Mutex::new(SessionUsage::default()),
            realtime: None,
            compaction_runner: None,
            compaction_workspace: None,
            persistent_retry_state: None,
            agent_definitions: Arc::new(crate::agents::AgentDefinitions::new()),
            file_state_cache: None,
            profile: None,
            tiered_compaction: None,
            subagent_output_router: None,
            subagent_summary_generator: None,
            cost_accountant: None,
            parent_session_key: None,
            spawn_depth: 0,
            sandbox_config: None,
        }
    }

    /// Attach an [`crate::agents::AgentDefinitions`] registry. Threaded into
    /// every per-tool [`crate::tools::ToolContext`] so tools that read
    /// `ctx.agent_definitions` see the live registry instead of the M8.1
    /// zero-value default. Idempotent — callers may swap the registry at
    /// any time.
    pub fn with_agent_definitions(mut self, defs: Arc<crate::agents::AgentDefinitions>) -> Self {
        self.agent_definitions = defs;
        self
    }

    /// Record the active [`crate::profile::ProfileDefinition`] envelope.
    ///
    /// Call this after the caller has already applied the profile's tool
    /// filter to the [`crate::tools::ToolRegistry`] (via
    /// [`crate::tools::ToolRegistry::filter_by_profile`]) and passed the
    /// filtered registry into [`Agent::new`]. This setter only *records*
    /// the profile so downstream code can introspect the active name,
    /// compaction policy overrides, and model preferences.
    ///
    /// Fields that today land as *recorded only* (compaction policy, model
    /// preferences, MCP server ids) keep their semantics — the agent loop
    /// does not enforce them yet. See the
    /// [`crate::profile`] module doc for the follow-up milestones that
    /// wire each field in.
    pub fn with_profile(mut self, profile: Arc<crate::profile::ProfileDefinition>) -> Self {
        self.profile = Some(profile);
        self
    }

    /// Access the recorded [`crate::profile::ProfileDefinition`], if any.
    /// Returns `None` when the agent was built without a profile envelope
    /// (legacy pre-M8.3 mode).
    pub fn profile(&self) -> Option<Arc<crate::profile::ProfileDefinition>> {
        self.profile.clone()
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

    /// Enable M8.4's [`FileStateCache`] for file tools.
    ///
    /// When set, file tools like `read_file`, `write_file`, `edit_file`, and
    /// `diff_edit` consult this cache to short-circuit re-reads of unchanged
    /// files and invalidate entries on write. Absent = pre-M8.4 behaviour.
    pub fn with_file_state_cache(mut self, cache: Arc<FileStateCache>) -> Self {
        self.file_state_cache = Some(cache);
        self
    }

    /// Access the agent's [`FileStateCache`] handle (if configured). Used by
    /// the compaction runner to invoke [`FileStateCache::clear`] at tier-3
    /// compaction boundaries — see M8.5 for the full integration.
    pub fn file_state_cache(&self) -> Option<&Arc<FileStateCache>> {
        self.file_state_cache.as_ref()
    }

    /// Wire an M8.7 [`crate::subagent_output::SubAgentOutputRouter`] so the
    /// spawn_only background branch can route textual output to disk and
    /// flag terminal state for dashboards. Absent = pre-M8.7 behaviour.
    pub fn with_subagent_output_router(
        mut self,
        router: Arc<crate::subagent_output::SubAgentOutputRouter>,
    ) -> Self {
        self.subagent_output_router = Some(router);
        self
    }

    /// Access the M8.7 sub-agent output router, if configured.
    pub fn subagent_output_router(
        &self,
    ) -> Option<&Arc<crate::subagent_output::SubAgentOutputRouter>> {
        self.subagent_output_router.as_ref()
    }

    /// Wire an M8.7 [`crate::subagent_summary::AgentSummaryGenerator`] so the
    /// spawn_only background branch can spawn a periodic summary watcher
    /// per qualifying task and stop it on terminal completion. Absent =
    /// pre-M8.7 behaviour.
    pub fn with_subagent_summary_generator(
        mut self,
        generator: Arc<crate::subagent_summary::AgentSummaryGenerator>,
    ) -> Self {
        self.subagent_summary_generator = Some(generator);
        self
    }

    /// Access the M8.7 sub-agent summary generator, if configured.
    pub fn subagent_summary_generator(
        &self,
    ) -> Option<&Arc<crate::subagent_summary::AgentSummaryGenerator>> {
        self.subagent_summary_generator.as_ref()
    }

    /// Wire a shared [`crate::cost_ledger::CostAccountant`] onto the
    /// agent so background sub-agents (pipeline workers, spawn
    /// children) inherit the same accountant via `TOOL_CTX` and commit
    /// per-node spend to the same ledger. M8 parity (W1.A4).
    pub fn with_cost_accountant(
        mut self,
        accountant: Arc<crate::cost_ledger::CostAccountant>,
    ) -> Self {
        self.cost_accountant = Some(accountant);
        self
    }

    /// Access the configured cost accountant, if any.
    pub fn cost_accountant(&self) -> Option<&Arc<crate::cost_ledger::CostAccountant>> {
        self.cost_accountant.as_ref()
    }

    /// Record the owning session key so pipeline workers / spawn
    /// children can register child tasks against the parent session
    /// in the supervisor's task store. M8 parity.
    pub fn with_parent_session_key(mut self, key: impl Into<String>) -> Self {
        self.parent_session_key = Some(key.into());
        self
    }

    /// Access the recorded parent session key, if any.
    pub fn parent_session_key(&self) -> Option<&str> {
        self.parent_session_key.as_deref()
    }

    /// Guard C (issue #607): record this agent's spawn nesting depth so
    /// every tool call it dispatches inherits the value via
    /// `ToolContext.spawn_depth`. The spawn tool consults this when
    /// deciding whether the next nested spawn should be allowed; values
    /// at or above [`crate::tools::spawn::MAX_SPAWN_DEPTH`] are refused.
    pub fn with_spawn_depth(mut self, depth: u8) -> Self {
        self.spawn_depth = depth;
        self
    }

    /// Access the agent's recorded spawn nesting depth.
    pub fn spawn_depth(&self) -> u8 {
        self.spawn_depth
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

    /// Set the local harness event sink path for child tools.
    pub fn with_harness_event_sink(mut self, sink_path: impl Into<String>) -> Self {
        self.harness_event_sink = Some(sink_path.into());
        self
    }

    /// Set per-session runtime limits for tool execution.
    pub fn with_session_limits(mut self, limits: SessionLimits) -> Self {
        self.session_limits = Some(limits);
        self.session_usage = std::sync::Mutex::new(SessionUsage::default());
        self
    }

    /// Attach a realtime controller so each loop iteration beats the
    /// heartbeat, checks for stalls, and (if configured) injects a bounded
    /// sensor summary into the system prompt.
    pub fn with_realtime(mut self, controller: Arc<RealtimeController>) -> Self {
        self.realtime = Some(controller);
        self
    }

    /// Returns the attached realtime controller, if any. Tools and tests
    /// reach through this to inspect heartbeat state.
    pub fn realtime_controller(&self) -> Option<Arc<RealtimeController>> {
        self.realtime.clone()
    }

    /// Wire the declarative compaction runner (harness M6.3). Optional — when
    /// absent, the loop falls back to the legacy extractive trim path.
    pub fn with_compaction_runner(
        mut self,
        runner: Arc<crate::compaction::CompactionRunner>,
    ) -> Self {
        self.compaction_runner = Some(runner);
        self
    }

    /// Attach the workspace policy that backs the compaction runner. Used by
    /// the post-compaction validator rail to resolve declared artifact names.
    pub fn with_compaction_workspace(
        mut self,
        workspace: crate::workspace_policy::WorkspacePolicy,
    ) -> Self {
        self.compaction_workspace = Some(workspace);
        self
    }

    /// Access the attached compaction runner, if any.
    pub fn compaction_runner(&self) -> Option<Arc<crate::compaction::CompactionRunner>> {
        self.compaction_runner.clone()
    }

    /// Access the attached workspace policy used for compaction gating.
    pub fn compaction_workspace(&self) -> Option<&crate::workspace_policy::WorkspacePolicy> {
        self.compaction_workspace.as_ref()
    }

    /// Attach a cross-turn persistent [`LoopRetryState`]. When set, the
    /// agent loop observes failures against this shared state instead of
    /// constructing a fresh `LoopRetryState` per turn, so bucket counters
    /// accumulate across `process_message` calls for the same session.
    ///
    /// The caller owns the save/load cycle — this is intentionally a shim
    /// over `Arc<Mutex<...>>` so session actors can round-trip the state
    /// to a JSON sidecar without re-implementing the bucket machine. See
    /// Review A F-015 for the motivating bug: without this wiring, a
    /// sequence of transient rate-limits spread across two turns never
    /// triggers the per-bucket exhaustion path because the counters reset
    /// on every turn boundary.
    pub fn with_persistent_retry_state(
        mut self,
        state: Arc<std::sync::Mutex<crate::agent::loop_state::LoopRetryState>>,
    ) -> Self {
        self.persistent_retry_state = Some(state);
        self
    }

    /// Access the attached persistent retry state, if any. Exposed so
    /// session actors can snapshot/serialize the bucket counters at turn
    /// boundaries without having to plumb the handle back through a
    /// separate field.
    pub fn persistent_retry_state(
        &self,
    ) -> Option<Arc<std::sync::Mutex<crate::agent::loop_state::LoopRetryState>>> {
        self.persistent_retry_state.clone()
    }

    /// Wire the M8.5 three-tier compaction runner. Tier 1 runs at the top
    /// of every loop iteration; tier 2 decorates outgoing Anthropic
    /// requests; tier 3 is the existing declarative runner wrapped behind a
    /// [`crate::compaction_tiered::FullCompactor`].
    pub fn with_tiered_compaction(
        mut self,
        runner: Arc<crate::compaction_tiered::TieredCompactionRunner>,
    ) -> Self {
        self.tiered_compaction = Some(runner);
        self
    }

    /// Access the attached three-tier compaction runner, if any.
    pub fn tiered_compaction(
        &self,
    ) -> Option<Arc<crate::compaction_tiered::TieredCompactionRunner>> {
        self.tiered_compaction.clone()
    }

    /// Beat the heartbeat once (if a realtime controller is attached) and
    /// return `Err(AgentError::HeartbeatStalled)` when the controller reports
    /// a stall. Callers invoke this at the top of each loop iteration so that
    /// a hung LLM or I/O call can surface a typed error instead of silently
    /// freezing the robot.
    pub(super) fn beat_heartbeat(&self, iteration: u32) -> eyre::Result<()> {
        use realtime::{AgentError, HeartbeatState};

        let Some(controller) = self.realtime.as_ref() else {
            return Ok(());
        };
        if !controller.config().enabled {
            return Ok(());
        }
        match controller.beat_and_check() {
            HeartbeatState::Alive => Ok(()),
            HeartbeatState::Stalled => {
                let timeout_ms = controller.config().heartbeat_timeout_ms;
                tracing::warn!(
                    iteration,
                    timeout_ms,
                    "realtime heartbeat stalled, aborting iteration"
                );
                Err(eyre::Report::new(AgentError::HeartbeatStalled {
                    iteration,
                    timeout_ms,
                }))
            }
        }
    }

    /// Render the sensor context summary (bounded by the configured token
    /// budget) for the current system prompt, if the realtime controller is
    /// enabled and has an injector. Returns `None` when realtime is off, the
    /// injector has no data, or the source is empty.
    pub(super) fn realtime_sensor_summary(&self) -> Option<String> {
        let controller = self.realtime.as_ref()?;
        if !controller.config().enabled {
            return None;
        }
        controller.sensor_summary()
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

    /// Record the effective [`crate::sandbox::SandboxConfig`] that built this
    /// agent's `ShellTool` sandbox.
    ///
    /// Callers that need to recreate a sandbox for a per-session
    /// [`crate::tools::ToolRegistry::rebind_cwd`] (e.g. AppUi session cwd
    /// binding) can read it back via [`Self::sandbox_config`] and pass it to
    /// [`crate::sandbox::create_sandbox`] so the new shell tool inherits
    /// network access, read-allow paths, profile name, and mode from the
    /// running server's configuration instead of falling back to
    /// `SandboxConfig::default()`.
    pub fn with_sandbox_config(mut self, sandbox: crate::sandbox::SandboxConfig) -> Self {
        self.sandbox_config = Some(sandbox);
        self
    }

    /// Return the recorded effective [`crate::sandbox::SandboxConfig`], if
    /// any was supplied via [`Self::with_sandbox_config`]. `None` keeps
    /// pre-M9 behaviour — callers should fall back to
    /// `SandboxConfig::default()` only when this is `None`.
    pub fn sandbox_config(&self) -> Option<crate::sandbox::SandboxConfig> {
        self.sandbox_config.clone()
    }

    /// Get a snapshot of the current system prompt.
    pub fn system_prompt_snapshot(&self) -> String {
        self.system_prompt
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Whether the loop-detector warning has fired since the last reset.
    /// Exposed for tests so they can verify single-fire-per-burst semantics.
    pub fn is_loop_detected_recently(&self) -> bool {
        self.loop_detected_recently.load(Ordering::Acquire)
    }

    /// Clear the "loop detected recently" flag.
    /// Called at the start of each `process_message` turn so a new user
    /// message starts with a clean slate.
    pub(super) fn reset_loop_detected_recently(&self) {
        self.loop_detected_recently.store(false, Ordering::Release);
    }

    /// Mark the loop-detector warning as having just fired.
    pub(super) fn mark_loop_detected_recently(&self) {
        self.loop_detected_recently.store(true, Ordering::Release);
    }
}

#[cfg(test)]
mod profile_integration_tests {
    //! M8.3 — bootstrapping an [`Agent`] with the built-in `coding`
    //! profile must yield the same tool set as today's default path. This
    //! is the behaviour-parity gate called out in the milestone issue.

    use super::*;
    use octos_core::AgentId;
    use octos_llm::{ChatResponse, LlmProvider, ToolSpec};
    use octos_memory::EpisodeStore;

    struct NoopProvider;

    #[async_trait::async_trait]
    impl LlmProvider for NoopProvider {
        async fn chat(
            &self,
            _messages: &[octos_core::Message],
            _tools: &[ToolSpec],
            _config: &octos_llm::ChatConfig,
        ) -> eyre::Result<ChatResponse> {
            eyre::bail!("unused in profile integration tests")
        }
        fn model_id(&self) -> &str {
            "mock"
        }
        fn provider_name(&self) -> &str {
            "mock"
        }
    }

    async fn agent_default(cwd: &std::path::Path) -> Agent {
        let memory = Arc::new(
            EpisodeStore::open(cwd.join("memory-default"))
                .await
                .expect("episode store"),
        );
        let provider: Arc<dyn LlmProvider> = Arc::new(NoopProvider);
        let tools = ToolRegistry::with_builtins(cwd);
        Agent::new(AgentId::new("default"), provider, tools, memory)
    }

    async fn agent_with_coding_profile(cwd: &std::path::Path) -> Agent {
        use crate::profile::ProfileDefinition;

        let memory = Arc::new(
            EpisodeStore::open(cwd.join("memory-profile"))
                .await
                .expect("episode store"),
        );
        let provider: Arc<dyn LlmProvider> = Arc::new(NoopProvider);

        let coding = ProfileDefinition::builtin("coding").expect("coding builtin");
        let mut tools = ToolRegistry::with_builtins(cwd);
        coding.apply_to_registry(&mut tools);

        Agent::new(AgentId::new("coding"), provider, tools, memory).with_profile(Arc::new(coding))
    }

    fn tool_names(agent: &Agent) -> Vec<String> {
        let mut names: Vec<String> = agent
            .tool_registry()
            .specs()
            .into_iter()
            .map(|s| s.name)
            .collect();
        names.sort();
        names
    }

    #[tokio::test]
    async fn coding_profile_matches_default_tool_set() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let base = agent_default(tmp.path()).await;
        let profiled = agent_with_coding_profile(tmp.path()).await;

        assert_eq!(
            tool_names(&base),
            tool_names(&profiled),
            "coding profile must preserve the default tool set byte-for-byte",
        );

        // The profiled agent also exposes the recorded profile handle.
        let prof = profiled.profile().expect("profile handle present");
        assert_eq!(prof.name, "coding");
        assert_eq!(prof.version, 1);
    }

    #[tokio::test]
    async fn agent_without_profile_returns_none() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let agent = agent_default(tmp.path()).await;
        assert!(
            agent.profile().is_none(),
            "agents built without a profile envelope return None",
        );
    }
}
