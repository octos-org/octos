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
pub mod memory;
mod message_repair;
pub mod prompt_segments;
pub mod realtime;
pub mod rich_output;
mod streaming;
pub mod turn_failure;
mod turn_state;
pub mod verifier;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, RwLock};

use octos_core::{AgentId, Message, SessionScope, TokenUsage};
use octos_llm::{EmbeddingProvider, LlmProvider, ProviderMetadata};
use octos_memory::EpisodeStore;

pub use prompt_segments::PromptSegmentProvider;

use crate::file_state_cache::FileStateCache;
use crate::hooks::{HookContext, HookExecutor};
use crate::progress::{ProgressReporter, SilentReporter};
use crate::prompt_context::PromptContextManager;
use crate::session::{SessionLimits, SessionUsage};
use crate::tools::ToolRegistry;
use verifier::AgentVerifierConfig;

pub use message_repair::normalize_tool_call_id;
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
    /// Default timeout (seconds) for a batch of ordinary interactive/fast
    /// tools (`glob`, `list_dir`, `read_file`, `grep`, ...) when the LLM does
    /// NOT request a per-call `timeout_secs`. Genuinely long-running tools
    /// (`shell`, `spawn`, `run_pipeline`, `browser`, deep research/crawl)
    /// keep `tool_timeout_secs` / `MAX_TOOL_TIMEOUT_SECS` instead.
    ///
    /// Default 120s; env override `OCTOS_INTERACTIVE_TOOL_TIMEOUT_SECS`
    /// (clamped [1, 1800]). mini5 soak motivation: a read-only `glob`/
    /// `list_dir` over an unscoped home dir must not inherit the 1800s
    /// ceiling and hang the whole turn.
    pub default_interactive_tool_timeout_secs: u64,
    /// Per-call max output tokens override. When set, overrides `ChatConfig::default()`.
    /// Useful for pipeline nodes that produce long outputs (e.g. synthesize).
    pub chat_max_tokens: Option<u32>,
    /// Reasoning effort for thinking models. Flows into `ChatConfig::reasoning_effort`;
    /// providers translate it per model (no-op for models without a reasoning style).
    pub reasoning_effort: Option<octos_llm::ReasoningEffort>,
    /// Suppress the generic auto-send loop for tool `files_to_send`.
    /// Background spawned workers rely on their outer workflow/session runtime
    /// to persist terminal results exactly once.
    pub suppress_auto_send_files: bool,
    /// Grace period awaiting the FIRST streamed chunk (time-to-first-token).
    /// Reasoning models (e.g. `deepseek-v4-pro`) can legitimately take minutes
    /// before the first token, so this is generous. Default 180s; env override
    /// `OCTOS_LLM_FIRST_TOKEN_GRACE_SECS`.
    pub llm_first_token_grace: std::time::Duration,
    /// Inter-chunk idle timeout once streaming has begun. A stalled provider
    /// that stops yielding tokens trips this and aborts the call (retryable).
    /// Default 90s; env override `OCTOS_LLM_STREAM_IDLE_SECS`.
    pub llm_stream_idle: std::time::Duration,
    /// Overall wall-clock cap on a single streaming LLM call, measured from
    /// call start. Final backstop so a stream that keeps trickling a token
    /// every <idle> seconds forever still terminates. Default 1200s (20 min);
    /// env override `OCTOS_LLM_CALL_MAX_SECS`.
    pub llm_call_max: std::time::Duration,
    /// Config-driven human-approval rules for the suspend-and-resume flow
    /// (see `docs/ROBRIX-PHASE4-APPROVAL-FLOW-ADR.md`). When a tool call
    /// matches a rule, the conversation loop returns early with
    /// [`ConversationResponse::pending_approval`] instead of executing the
    /// tool; the host projects the request to the channel and resumes via
    /// [`Agent::execute_approved_tool`]. `None` disables the flow.
    pub human_approval_rules: Option<crate::approval::HumanApprovalRules>,
    /// Voice fail-fast overall deadline for a single foreground LLM call,
    /// covering BOTH the stream-build (`chat_stream().await`) and consume
    /// phases. `StreamTimeouts` only starts ticking inside `consume_stream`,
    /// so a provider that hangs while returning response headers would
    /// otherwise inherit the long production request timeout. Only applied
    /// under [`octos_llm::LlmCallPolicy::FailFast`] (voice turns). Default 30s;
    /// env override `OCTOS_VOICE_LLM_DEADLINE_SECS`.
    pub voice_overall_deadline: std::time::Duration,
    /// Post-edit formatting (issue #1774): when true, a successful
    /// `edit_file` / `write_file` / `diff_edit` runs the file's language
    /// formatter (rustfmt / prettier / black / gofmt — see [`crate::format`])
    /// and echoes the formatted content back in the tool result. Best-effort:
    /// missing binaries, failures, and timeouts never fail the edit. OFF by
    /// default — opt in via `format_after_edit: true` in config.json.
    pub format_after_edit: bool,
}

/// Default time-to-first-token grace for streaming LLM calls (180s).
pub const DEFAULT_LLM_FIRST_TOKEN_GRACE_SECS: u64 = 180;
/// Default inter-chunk idle timeout for streaming LLM calls (90s).
pub const DEFAULT_LLM_STREAM_IDLE_SECS: u64 = 90;
/// Default overall wall-clock cap for a single streaming LLM call (1200s / 20m).
pub const DEFAULT_LLM_CALL_MAX_SECS: u64 = 1200;
/// Default voice fail-fast overall deadline (30s) covering build + consume.
pub const DEFAULT_VOICE_LLM_DEADLINE_SECS: u64 = 30;
/// Tightened time-to-first-token grace for voice fail-fast turns (10s). A
/// spoken reply cannot wait minutes for the first token the way a reasoning
/// chat turn can, so the voice path overrides the generous production grace.
pub const VOICE_STREAM_TTFT_SECS: u64 = 10;
/// Tightened inter-chunk idle timeout for voice fail-fast turns (10s).
pub const VOICE_STREAM_IDLE_SECS: u64 = 10;

/// Read an env-overridable seconds value, mirroring the convention in
/// `octos-cli/src/session_actor.rs` (`std::env::var(...).parse()` with a clamp
/// so a misconfigured value cannot disable the guard entirely). A parsed `0`
/// is clamped up to `1` so the timeout is always live.
fn env_secs_or(var: &str, default_secs: u64) -> std::time::Duration {
    let secs = std::env::var(var)
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .map(|v| v.clamp(1, 86_400))
        .unwrap_or(default_secs);
    std::time::Duration::from_secs(secs)
}

/// Like [`env_secs_or`] but returns a raw `u64` seconds value clamped to
/// `[1, MAX_TOOL_TIMEOUT_SECS]`. Used for the interactive-tool-timeout knob,
/// which is stored as a `u64` on [`AgentConfig`] (not a `Duration`).
fn env_secs_u64_or(var: &str, default_secs: u64) -> u64 {
    std::env::var(var)
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .map(|v| v.clamp(1, MAX_TOOL_TIMEOUT_SECS))
        .unwrap_or(default_secs)
}

/// Default tool execution timeout in seconds.
/// Matches `MAX_TOOL_TIMEOUT_SECS` so long-running tools like `run_pipeline`
/// (default 1800s) are not silently capped when the LLM omits `timeout_secs`
/// in the tool call.
pub const DEFAULT_TOOL_TIMEOUT_SECS: u64 = 1800;
/// Maximum tool timeout the LLM can request (30 minutes).
pub const MAX_TOOL_TIMEOUT_SECS: u64 = 1800;
/// Default timeout (seconds) for a batch of ordinary interactive/fast tools
/// (`glob`, `list_dir`, `read_file`, `grep`, ...) when the LLM omits a
/// per-call `timeout_secs`. Genuinely long-running tools keep the 1800s
/// default. See [`AgentConfig::default_interactive_tool_timeout_secs`].
pub const DEFAULT_INTERACTIVE_TOOL_TIMEOUT_SECS: u64 = 120;
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
            default_interactive_tool_timeout_secs: env_secs_u64_or(
                "OCTOS_INTERACTIVE_TOOL_TIMEOUT_SECS",
                DEFAULT_INTERACTIVE_TOOL_TIMEOUT_SECS,
            ),
            chat_max_tokens: None,
            reasoning_effort: None,
            suppress_auto_send_files: false,
            llm_first_token_grace: env_secs_or(
                "OCTOS_LLM_FIRST_TOKEN_GRACE_SECS",
                DEFAULT_LLM_FIRST_TOKEN_GRACE_SECS,
            ),
            llm_stream_idle: env_secs_or(
                "OCTOS_LLM_STREAM_IDLE_SECS",
                DEFAULT_LLM_STREAM_IDLE_SECS,
            ),
            llm_call_max: env_secs_or("OCTOS_LLM_CALL_MAX_SECS", DEFAULT_LLM_CALL_MAX_SECS),
            human_approval_rules: None,
            voice_overall_deadline: env_secs_or(
                "OCTOS_VOICE_LLM_DEADLINE_SECS",
                DEFAULT_VOICE_LLM_DEADLINE_SECS,
            ),
            format_after_edit: false,
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
    /// Estimated spend for `token_usage`, summed per response with each
    /// response priced at the model that actually produced it (failover /
    /// routed slots at their own rate). `None` when no response in the
    /// turn had catalog pricing. Embedders must persist THIS instead of
    /// re-pricing `token_usage` at the final `provider_metadata` model —
    /// a turn that crossed models would re-price earlier responses at
    /// the final model's rate (codex #1632 P1).
    pub estimated_spend_usd: Option<f64>,
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
    /// Marks `content` as a synthesized acknowledgement fabricated by the
    /// spawn_only branch in `loop_runner::process_message_inner` (the
    /// "Background work started for `<tool>`. The final result will be
    /// delivered automatically when it is ready." string). When `true`,
    /// the API persist site skips the synthesized acknowledgement entirely.
    /// The real background result persists independently and emits its
    /// canonical v2 background-child envelope, avoiding a duplicate
    /// foreground bubble.
    /// Defaults to `false`; only set in the spawn_only synthesis path.
    pub synthesized_from_spawn_only: bool,
    /// Set when a tool call matched a [`AgentConfig::human_approval_rules`]
    /// rule: the turn was suspended before executing that tool and the host
    /// must project this request to the channel, await a human decision, and
    /// resume via [`Agent::execute_approved_tool`]
    /// (`docs/ROBRIX-PHASE4-APPROVAL-FLOW-ADR.md`). `content` is empty in
    /// that case. `None` for every ordinary turn.
    pub pending_approval: Option<crate::approval::PendingApprovalDraft>,
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
    /// Whether THIS conversation has already saved its episode (#1587
    /// write side). Set on the first compaction; subsequent compactions
    /// skip. One conversation episode per session — bounded regardless of
    /// how many times the session compacts, and no index churn (the hybrid
    /// index only tombstones on delete, so supersede would bloat it).
    /// Per-agent = per-session (codex-confirmed).
    pub(super) conversation_episode_saved: std::sync::atomic::AtomicBool,
    /// System prompt for this agent, as ordered segments (RwLock for
    /// hot-reload support). See [`prompt_segments::PromptSegments`].
    pub(super) system_prompt: RwLock<prompt_segments::PromptSegments>,
    /// Providers that refresh named prompt segments between turns
    /// (e.g. the memory block). Run by [`Agent::refresh_prompt_segments`].
    pub(super) segment_providers: RwLock<Vec<Arc<dyn PromptSegmentProvider>>>,
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
    /// Session-cumulative usage base shared with the owning session
    /// actor. The actor seeds it from the persistent usage ledger and
    /// folds each completed run back in (priced at the model that ran
    /// it); `emit_cost_update` READS it so the `session_*` figures on
    /// `ProgressEvent::CostUpdate` cover the whole session — surviving
    /// per-turn resets, provider failover, and the runtime-cache
    /// eviction a `profile/llm/select` model switch triggers. `None`
    /// (chat mode, sub-agents) keeps emissions turn-scoped.
    pub(super) session_usage_base: Option<crate::session_usage::SharedSessionUsage>,
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
    /// Optional caller-owned prompt context manager. When present, the agent
    /// loop asks it to prepare the final model-visible prompt before each LLM
    /// call. This lets AppUI/session runtimes route mid-turn prompt compaction
    /// through their durable context ledger without making `octos-agent`
    /// depend on the CLI crate.
    pub(super) prompt_context_manager: Option<Arc<dyn PromptContextManager>>,
    /// Phase 1 of the [`SessionScope`] migration (PR #1198 follow-up):
    /// the single filesystem contract for this session, constructed at
    /// the host entry point (`chat.rs` solo, `runtime/session.rs`
    /// multi-tenant). Threaded onto every per-tool
    /// [`crate::tools::ToolContext`] so the same scope is visible to
    /// tools and to pipeline workers via
    /// [`octos_pipeline::PipelineHostContext::from_tool_context`].
    ///
    /// `None` keeps pre-Phase-1 behaviour byte-for-byte — no consumer
    /// reads the field yet. Phase 2 PRs will migrate file tools,
    /// pipeline workers, plugins, and shell to derive their CWD and
    /// path validation from this scope.
    pub(super) session_scope: Option<Arc<SessionScope>>,
    /// Goal ID this agent runs under (peer-agent-based goal). Populated by
    /// the peer session boot when the staged peer dir carries a `goal` file
    /// (`peers/<slug>/goal`, written by `stage_peer` when the master passed
    /// `goal_id`/`task_id` to `peer_handoff`). `None` for goal-less peers
    /// and non-peer sessions. Read by `goal_*` tools via `ToolContext.goal_id`.
    pub(super) goal_id: Option<String>,
    /// Task ID within the goal (peer-agent-based goal). Sourced from line 2
    /// of the peer's `goal` file; may be `None` even when `goal_id` is set
    /// (the master scoped the peer to a goal but no specific sub-task).
    pub(super) task_id: Option<String>,
    /// The session that staged this peer (peer-agent-based goal). Captured
    /// once at peer boot from `peers/<slug>/originator` and threaded into
    /// `ToolContext::originator_session` so goal-aware tools can enforce
    /// the goal-binding check WITHOUT re-reading the (mutable, symlink-
    /// vulnerable) originator file on every call. `None` for non-peer
    /// sessions.
    pub(super) originator_session: Option<String>,
    /// Optional inference-time verifier plus structured TurnLedger. Absent
    /// by default so legacy agent loops do not spend verifier calls or write
    /// verifier sidecars unless a caller opts in explicitly.
    pub(super) verifier_config: Option<AgentVerifierConfig>,
    /// Voice-turn failure projection sink (Task 8). When the agent loop runs
    /// under [`octos_llm::LlmCallPolicy::FailFast`] and a FOREGROUND LLM call
    /// fails terminally, the loop emits a single [`crate::TurnFailure`] here so
    /// the voice closeout (octos-cli) can render a spoken error/empty message.
    /// `None` keeps pre-Task-8 behaviour byte-for-byte — the original
    /// `eyre::Report` still flows out of the loop unchanged.
    pub(super) voice_failure_sink: Option<tokio::sync::mpsc::UnboundedSender<crate::TurnFailure>>,
    /// Git-backed workspace snapshot store (#1768, opt-in). When present,
    /// `execute_tools` records a snapshot of the workspace before any
    /// batch containing a mutating tool so the user can restore
    /// pre-mutation state later. `None` (the default) disables the
    /// feature entirely — no git subprocess is ever spawned.
    pub(super) snapshot_manager: Option<Arc<crate::snapshot::SnapshotManager>>,
    /// Per-turn pending-input buffer for mid-turn prompt injection
    /// ("steer") — codex `TurnState.pending_input` parity. The host pushes
    /// while the turn runs; the conversation loop drains FIFO at the top of
    /// each iteration (before the next LLM call) and appends each entry as
    /// a plain `role: user` message with no wrapper text. A steer that
    /// lands after the model's final answer forces one more round
    /// (`needs_follow_up = model_wants_more || buffer_nonempty`). `None`
    /// (the default) keeps the loop byte-identical to pre-steer behaviour.
    pub(super) steer_buffer: Option<crate::steering::SharedSteerBuffer>,
    /// Host callback observing each drained steer batch (codex
    /// `record_user_prompt_and_emit_turn_item` parity): the host persists
    /// the injected user message + emits its standard persisted
    /// user-message event. Called inline at the drain point, before the
    /// next LLM call. When set, drained steer rows stay OUT of the turn
    /// output log so end-of-turn persistence cannot double-write them.
    pub(super) steer_drained_callback: Option<crate::steering::SteerDrainedCallback>,
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
        // RFC-1 fixup (codex P1 + round-3 P2): refresh the `mofa_make`
        // dispatcher pair before wrapping the registry in `Arc`, then
        // wire the (fresh) dispatcher's `Weak<ToolRegistry>` back-reference.
        //
        // Why the refresh: callers that build per-node/per-turn
        // registries from CACHED `Arc<dyn Tool>` instances (notably
        // `octos-pipeline`, which caches plugin tool Arcs once and
        // registers the SAME `Arc<MofaMakeTool>` into every node
        // registry) would otherwise have the central wire below mutate
        // the SHARED dispatcher object. Two overlapping pipeline nodes
        // would then race on the dispatcher's `Mutex<Weak>` and one
        // node's `mofa_make` call could resolve through the OTHER
        // node's registry — or, once one node's registry drops, the
        // shared dispatcher's Weak would point at a dropped registry
        // and surface `[DISPATCHER_ERROR]`.
        //
        // Minting fresh instances seeded from the existing catalog
        // gives each registry its own dispatcher object; the cached
        // Arc kept by the caller is untouched. Mirrors the same
        // share-mutate-hazard fix the per-turn WS path applies (see
        // `ui_protocol.rs::process_chat_message_streaming`).
        let mut tools = tools;
        Self::refresh_mofa_make_dispatcher_in_place(&mut tools);
        let tools = Arc::new(tools);
        crate::plugins::PluginLoader::wire_mofa_make_registry_back_ref(&tools);

        Self {
            id,
            llm,
            tools,
            memory,
            embedder: None,
            conversation_episode_saved: std::sync::atomic::AtomicBool::new(false),
            system_prompt: RwLock::new(prompt_segments::PromptSegments::from_base(system_prompt)),
            segment_providers: RwLock::new(Vec::new()),
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
            session_usage_base: None,
            parent_session_key: None,
            spawn_depth: 0,
            sandbox_config: None,
            prompt_context_manager: None,
            session_scope: None,
            goal_id: None,
            task_id: None,
            originator_session: None,
            verifier_config: None,
            voice_failure_sink: None,
            snapshot_manager: None,
            steer_buffer: None,
            steer_drained_callback: None,
        }
    }

    /// Create a new agent sharing pre-existing Arc-wrapped resources.
    /// Useful for per-request agents that share tools/memory with a base agent.
    ///
    /// **Share-mutate hazard for `mofa_make`**: this method calls
    /// [`Self::wire_mofa_make_dispatcher`] which mutates the dispatcher's
    /// internal `Mutex<Weak<ToolRegistry>>`. If `tools` carries a
    /// dispatcher Arc that is ALSO held by another `Arc<ToolRegistry>`
    /// (typical for per-turn snapshots built from `snapshot_excluding`,
    /// or per-node pipeline registries built from a shared plugin tool
    /// cache), that other registry will silently lose its back-reference.
    ///
    /// Callers MUST mint fresh `MofaMakeTool` / `MofaDescribeContentTypeTool`
    /// instances seeded from the existing dispatcher's catalog and
    /// `register_arc` them on `tools` BEFORE calling `Agent::new_shared`
    /// (the ui_protocol.rs per-turn path is the canonical example). The
    /// constructor cannot do this itself because `Arc<ToolRegistry>` is
    /// shared-immutable.
    ///
    /// `Agent::new` does the freshen internally (it owns the
    /// `ToolRegistry` and can mutate it before the Arc wrap); use that
    /// entry-point when the caller has an owned registry.
    pub fn new_shared(
        id: AgentId,
        llm: Arc<dyn LlmProvider>,
        tools: Arc<ToolRegistry>,
        memory: Arc<EpisodeStore>,
    ) -> Self {
        let system_prompt = include_str!("../prompts/worker.txt").to_string();
        // RFC-1 fixup (codex P1 + round-3 P2): wire the dispatcher's
        // `Weak<ToolRegistry>` back-reference. The freshen step that
        // `Agent::new` does in-place cannot happen here (the registry
        // is shared-immutable behind `Arc`), so callers must freshen
        // before construction. See the doc comment above for details.
        crate::plugins::PluginLoader::wire_mofa_make_registry_back_ref(&tools);

        Self {
            id,
            llm,
            tools,
            memory,
            embedder: None,
            conversation_episode_saved: std::sync::atomic::AtomicBool::new(false),
            system_prompt: RwLock::new(prompt_segments::PromptSegments::from_base(system_prompt)),
            segment_providers: RwLock::new(Vec::new()),
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
            session_usage_base: None,
            parent_session_key: None,
            spawn_depth: 0,
            sandbox_config: None,
            prompt_context_manager: None,
            session_scope: None,
            goal_id: None,
            task_id: None,
            originator_session: None,
            verifier_config: None,
            voice_failure_sink: None,
            snapshot_manager: None,
            steer_buffer: None,
            steer_drained_callback: None,
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

    /// RFC-1 (issue #1290): wire the `mofa_make` dispatcher + companion
    /// `mofa_describe_content_type` to the shared tool registry. The
    /// dispatcher needs a `Weak<ToolRegistry>` so its `execute` path
    /// can look up the forwarding target by name.
    ///
    /// Idempotent and silent on agents whose registry has no mofa-*
    /// skills (no dispatcher registered → no-op). Hosts should call
    /// this after agent construction.
    pub fn wire_mofa_make_dispatcher(&self) {
        crate::plugins::PluginLoader::wire_mofa_make_registry_back_ref(&self.tools);
    }

    /// RFC-1 fixup (codex P2 round 3): mint a fresh `MofaMakeTool` +
    /// `MofaDescribeContentTypeTool` pair seeded from the existing
    /// dispatcher's catalog, then re-register them on `tools` so the
    /// per-agent dispatcher is a SEPARATE Arc object from whatever
    /// the caller cached / cloned in.
    ///
    /// Why: when `octos-pipeline` (and similar callers) build per-node
    /// registries from a shared `Arc<MofaMakeTool>` cache, the central
    /// wire in `Agent::new` would otherwise mutate the SHARED
    /// dispatcher's `Mutex<Weak<ToolRegistry>>` and let one node's
    /// `mofa_make` call resolve through another node's registry. After
    /// the refresh, each node owns its own dispatcher instance whose
    /// Weak can be wired safely.
    ///
    /// No-op when the registry has no mofa-* skills (no dispatcher to
    /// freshen). Internal-hidden markers, spawn_only markers, and
    /// every other registry side-state survive the refresh because
    /// only the dispatcher tool instances are replaced.
    fn refresh_mofa_make_dispatcher_in_place(tools: &mut ToolRegistry) {
        use crate::tools::{MofaDescribeContentTypeTool, MofaMakeTool};

        let entries = match tools.get("mofa_make") {
            Some(arc) => match arc.as_any().downcast_ref::<MofaMakeTool>() {
                Some(dispatcher) => dispatcher.entries(),
                None => return,
            },
            None => return,
        };
        if entries.is_empty() {
            return;
        }

        let fresh_dispatcher = MofaMakeTool::new();
        for entry in &entries {
            fresh_dispatcher.register_or_replace(entry.clone());
        }
        tools.register(fresh_dispatcher);

        if tools.get("mofa_describe_content_type").is_some() {
            let fresh_describe = MofaDescribeContentTypeTool::new();
            for entry in &entries {
                fresh_describe.register_or_replace(entry.clone());
            }
            tools.register(fresh_describe);
        }
    }

    /// Set the agent configuration.
    pub fn with_config(mut self, config: AgentConfig) -> Self {
        // Apply worker_prompt override if provided.
        // Lock poisoning recovery: safe — we just need the inner value.
        // A poisoned lock means a prior holder panicked, but the String
        // data itself is still valid and overwritten here.
        if let Some(ref wp) = config.worker_prompt {
            self.system_prompt
                .write()
                .unwrap_or_else(|e| e.into_inner())
                .replace_all(wp.clone());
        }
        self.config = config;
        self
    }

    /// Attach a caller-owned prompt context manager. Optional; absent keeps
    /// the legacy agent-local compaction path.
    pub fn with_prompt_context_manager(mut self, manager: Arc<dyn PromptContextManager>) -> Self {
        self.prompt_context_manager = Some(manager);
        self
    }

    /// Access the attached prompt context manager, if any.
    pub fn prompt_context_manager(&self) -> Option<Arc<dyn PromptContextManager>> {
        self.prompt_context_manager.clone()
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

    /// Attach the voice-turn failure projection sink (Task 8). When set and the
    /// loop runs under [`octos_llm::LlmCallPolicy::FailFast`], a single
    /// [`crate::TurnFailure`] is emitted on terminal foreground-LLM failure
    /// (empty response or classified LLM error). Hook-deny LLM failures are
    /// intentionally excluded so the existing permission behaviour is
    /// preserved.
    pub fn set_voice_failure_sink(
        &mut self,
        tx: tokio::sync::mpsc::UnboundedSender<crate::TurnFailure>,
    ) {
        self.voice_failure_sink = Some(tx);
    }

    /// Attach the per-turn pending-input buffer for mid-turn prompt
    /// injection ("steer"). The host keeps a clone and pushes inputs while
    /// the turn runs; the conversation loop drains FIFO at the top of each
    /// iteration, before the next LLM call, appending each entry as a plain
    /// `role: user` message (codex `TurnState.pending_input` parity —
    /// codex-rs `core/src/session/turn.rs:225-233`). Absent = pre-steer
    /// behaviour, byte-identical.
    pub fn with_steer_buffer(mut self, buffer: crate::steering::SharedSteerBuffer) -> Self {
        self.steer_buffer = Some(buffer);
        self
    }

    /// Register the host callback observing each drained steer batch.
    /// Called inline from the drain point (after the drained texts joined
    /// the prompt, before the next LLM call) so the host can persist the
    /// injected user message and emit its standard persisted user-message
    /// event. When set, the loop keeps drained steer rows OUT of
    /// `ConversationResponse.messages` — the host owns their persistence,
    /// and the end-of-turn persist pass must not write them again.
    pub fn with_steer_drained_callback(
        mut self,
        callback: crate::steering::SteerDrainedCallback,
    ) -> Self {
        self.steer_drained_callback = Some(callback);
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

    /// Share a session-cumulative usage base with this agent. The owner
    /// (session actor) seeds it from the usage ledger and folds completed
    /// runs; the agent only reads it when emitting cost updates, so the
    /// wire's `session_*` figures cover the whole session instead of
    /// resetting every turn. See [`crate::session_usage`].
    pub fn with_session_usage_base(
        mut self,
        usage: crate::session_usage::SharedSessionUsage,
    ) -> Self {
        self.session_usage_base = Some(usage);
        self
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

    /// Attach the session's [`SessionScope`] (Phase 1 of the migration
    /// landed by PR #1198). Threaded into every per-tool
    /// [`crate::tools::ToolContext`] so the same scope is visible to
    /// the foreground branch, the spawn_only background branch, and —
    /// via [`octos_pipeline::PipelineHostContext::from_tool_context`] —
    /// to pipeline workers.
    ///
    /// Phase 1 is additive: no consumer reads the field yet. Setting
    /// `None` (the default) preserves pre-Phase-1 behaviour byte-for-
    /// byte; setting `Some(scope)` makes the value visible to the
    /// downstream consumers that will come online in Phase 2.
    pub fn with_session_scope(mut self, scope: Arc<SessionScope>) -> Self {
        self.session_scope = Some(scope);
        self
    }

    /// Access the configured session scope, if any.
    pub fn session_scope(&self) -> Option<&Arc<SessionScope>> {
        self.session_scope.as_ref()
    }

    /// Builder: set the goal id this agent runs under. Called by the peer
    /// session boot when the staged peer dir carries a `goal` file. Read by
    /// `goal_*` tools via `ToolContext.goal_id`.
    pub fn with_goal_id(mut self, goal_id: String) -> Self {
        self.goal_id = Some(goal_id);
        self
    }

    /// Builder: set the task id this agent runs under (sub-task within the
    /// goal). Called alongside [`Self::with_goal_id`] when the peer's `goal`
    /// file carries a task id.
    pub fn with_task_id(mut self, task_id: String) -> Self {
        self.task_id = Some(task_id);
        self
    }

    /// The goal id this agent runs under (peer-agent-based goal). `None`
    /// for goal-less peers and non-peer sessions.
    pub fn goal_id(&self) -> Option<&str> {
        self.goal_id.as_deref()
    }

    /// The task id this agent runs under within its goal, if any.
    pub fn task_id(&self) -> Option<&str> {
        self.task_id.as_deref()
    }

    /// Builder: set the session that staged this peer. Called at peer boot
    /// alongside [`Self::with_goal_id`] when the staged peer dir carries an
    /// `originator` file.
    pub fn with_originator_session(mut self, originator: String) -> Self {
        self.originator_session = Some(originator);
        self
    }

    /// The session that staged this peer, if any (peer sessions only).
    pub fn originator_session(&self) -> Option<&str> {
        self.originator_session.as_deref()
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

    /// Returns the attached lifecycle hooks executor, if any. Used by the
    /// runtime layer to assert that profile-scope hook configs survive the
    /// per-session `Agent` rebuild and per-request rebuild paths
    /// (`ws_standalone_agent`, ui_protocol per-turn).
    pub fn hooks(&self) -> Option<Arc<HookExecutor>> {
        self.hooks.clone()
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

    /// Attach a workspace [`crate::snapshot::SnapshotManager`] (#1768).
    /// When present, `execute_tools` records a snapshot before any batch
    /// containing a mutating tool (see
    /// [`crate::snapshot::is_mutating_tool`]) so the user can later
    /// restore pre-mutation state. `None` (the default) disables
    /// snapshotting entirely — the feature is opt-in.
    pub fn with_snapshot_manager(mut self, manager: Arc<crate::snapshot::SnapshotManager>) -> Self {
        self.snapshot_manager = Some(manager);
        self
    }

    /// Returns the attached snapshot manager, if any. Lets hosts (and the
    /// follow-up UI/RPC surface) list/restore snapshots for this agent's
    /// workspace.
    pub fn snapshot_manager(&self) -> Option<Arc<crate::snapshot::SnapshotManager>> {
        self.snapshot_manager.clone()
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
    ///
    /// Full replace: drops any named segments (re-set them afterwards if
    /// still wanted).
    pub fn with_system_prompt(self, prompt: String) -> Self {
        self.system_prompt
            .write()
            .unwrap_or_else(|e| {
                tracing::warn!("system prompt lock was poisoned, recovering");
                e.into_inner()
            })
            .replace_all(prompt);
        self
    }

    /// Append additional content to the current system prompt (e.g. bootstrap files).
    pub fn append_system_prompt(&self, extra: &str) {
        self.system_prompt
            .write()
            .unwrap_or_else(|e| {
                tracing::warn!("system prompt lock was poisoned, recovering");
                e.into_inner()
            })
            .append(extra);
    }

    /// Insert (first call) or replace in place (later calls) a named prompt
    /// segment such as the memory block. The segment keeps its insertion
    /// position across replacements, so bootstrap-before / skills-after
    /// ordering is preserved when the content refreshes.
    pub fn set_prompt_segment(&self, name: &str, content: String) {
        self.system_prompt
            .write()
            .unwrap_or_else(|e| {
                tracing::warn!("system prompt lock was poisoned, recovering");
                e.into_inner()
            })
            .set_named(name, content);
    }

    /// Register a provider that refreshes a named segment between turns.
    pub fn add_prompt_segment_provider(&self, provider: Arc<dyn PromptSegmentProvider>) {
        self.segment_providers
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .push(provider);
    }

    /// Run all registered segment providers, applying any changed content.
    ///
    /// Called by the conversation loop at turn start; a no-op when no
    /// providers are registered, and providers keep the unchanged path
    /// cheap (typically one stat).
    pub async fn refresh_prompt_segments(&self) {
        let providers: Vec<Arc<dyn PromptSegmentProvider>> = self
            .segment_providers
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if providers.is_empty() {
            return;
        }
        let mut updates = Vec::new();
        for provider in providers {
            if let Some(content) = provider.refresh().await {
                updates.push((provider.segment_name().to_string(), content));
            }
        }
        if updates.is_empty() {
            return;
        }
        let mut guard = self.system_prompt.write().unwrap_or_else(|e| {
            tracing::warn!("system prompt lock was poisoned, recovering");
            e.into_inner()
        });
        for (name, content) in updates {
            guard.set_named(&name, content);
        }
    }

    /// Update the system prompt at runtime (hot-reload).
    ///
    /// Full replace: drops any named segments (re-set them afterwards if
    /// still wanted).
    pub fn set_system_prompt(&self, prompt: String) {
        self.system_prompt
            .write()
            .unwrap_or_else(|e| {
                tracing::warn!("system prompt lock was poisoned, recovering");
                e.into_inner()
            })
            .replace_all(prompt);
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

    /// Anchor the agent's tool registry to a workspace cwd.
    ///
    /// This is the Tier-2 hook used by the AppUi `session_tool_registry`
    /// fallback chain in `octos serve`: when a client did not advertise
    /// the `session.workspace_cwd.v1` capability and so cannot send its
    /// own per-session cwd, the registry's `workspace_root()` becomes the
    /// rebind target. Without this builder, the API agent's registry
    /// always reports `None` and Tier-2 is dead.
    ///
    /// Mutates the registry in place when this builder owns the only
    /// strong `Arc` (the typical post-`Agent::new` chain). If the `Arc`
    /// is already shared, falls back to copying via `snapshot_excluding`
    /// so we still anchor a fresh registry rather than silently dropping
    /// the request.
    ///
    pub fn with_workspace_root(mut self, cwd: PathBuf) -> Self {
        if let Some(tools) = Arc::get_mut(&mut self.tools) {
            tools.set_workspace_root(cwd);
        } else {
            // The Arc is already shared. Fall back to a deep copy so the
            // new workspace_root still wins. ToolRegistry is intentionally
            // not Clone, so use the existing snapshot helper which handles
            // interior mutex state correctly.
            let mut copy = self.tools.snapshot_excluding(&[]);
            copy.set_workspace_root(cwd);
            self.tools = Arc::new(copy);
        }
        self
    }

    /// Get a snapshot of the current system prompt.
    pub fn system_prompt_snapshot(&self) -> String {
        self.system_prompt
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .render()
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

    async fn agent_with_builtin_profile(cwd: &std::path::Path, name: &str) -> Agent {
        use crate::profile::ProfileDefinition;

        let memory = Arc::new(
            EpisodeStore::open(cwd.join(format!("memory-profile-{name}")))
                .await
                .expect("episode store"),
        );
        let provider: Arc<dyn LlmProvider> = Arc::new(NoopProvider);

        let profile = ProfileDefinition::builtin(name).expect("builtin profile");
        let mut tools = ToolRegistry::with_builtins(cwd);
        profile.apply_to_registry(&mut tools);

        Agent::new(AgentId::new(name), provider, tools, memory).with_profile(Arc::new(profile))
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
    async fn coding_profile_narrows_default_tool_set() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let base = agent_default(tmp.path()).await;
        let profiled = agent_with_builtin_profile(tmp.path(), "coding").await;

        let base_names = tool_names(&base);
        let lean_names = tool_names(&profiled);
        // Lean default: `coding` narrows the surface to the core coding
        // loop instead of passing the registry through untouched.
        assert!(
            lean_names.len() < base_names.len(),
            "lean coding profile must narrow the default set \
             ({} -> {})",
            base_names.len(),
            lean_names.len(),
        );
        assert!(
            lean_names.iter().all(|n| base_names.contains(n)),
            "lean set must be a subset of the default set",
        );
        for kept in ["read_file", "shell", "edit_file", "grep"] {
            assert!(
                lean_names.contains(&kept.to_string()),
                "core-loop tool {kept} missing from lean set: {lean_names:?}",
            );
        }
        for dropped in ["web_search", "browser", "image_generation"] {
            assert!(
                !lean_names.contains(&dropped.to_string()),
                "non-core tool {dropped} must be filtered by the lean coding profile",
            );
        }

        // The profiled agent also exposes the recorded profile handle.
        let prof = profiled.profile().expect("profile handle present");
        assert_eq!(prof.name, "coding");
        assert_eq!(prof.version, 1);
    }

    #[tokio::test]
    async fn coding_full_profile_matches_default_tool_set() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let base = agent_default(tmp.path()).await;
        let profiled = agent_with_builtin_profile(tmp.path(), "coding-full").await;

        // The byte-for-byte parity contract moved from `coding` to the
        // `coding-full` escape hatch when the lean default landed.
        assert_eq!(
            tool_names(&base),
            tool_names(&profiled),
            "coding-full profile must preserve the default tool set byte-for-byte",
        );

        let prof = profiled.profile().expect("profile handle present");
        assert_eq!(prof.name, "coding-full");
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

    #[tokio::test]
    async fn agent_without_session_scope_returns_none_by_default() {
        // Phase 1 of the SessionScope migration: agents built without
        // an explicit scope keep the legacy pre-Phase-1 behaviour. The
        // accessor reports `None`; downstream consumers (Phase 2) must
        // treat that as "no contract, fall back to today's path".
        let tmp = tempfile::tempdir().expect("tempdir");
        let agent = agent_default(tmp.path()).await;
        assert!(
            agent.session_scope().is_none(),
            "agents built without a SessionScope return None for the accessor",
        );
    }

    #[tokio::test]
    async fn with_session_scope_populates_field_for_solo_cwd() {
        // Phase 1 wiring contract: once the host entry point calls
        // `with_session_scope(...)`, the field is present and the
        // accessor returns the scope's `workspace()` == the user's cwd
        // for solo mode. No downstream consumer reads it yet — the test
        // exists so a regression in the plumbing fails loudly.
        let tmp = tempfile::tempdir().expect("tempdir");
        let cwd = tmp.path().to_path_buf();
        let scope = Arc::new(SessionScope::solo(cwd.clone(), vec![]).expect("solo scope"));
        let agent = agent_default(tmp.path()).await.with_session_scope(scope);
        let recorded = agent
            .session_scope()
            .expect("scope is wired into the agent");
        assert_eq!(recorded.workspace(), cwd.as_path());
    }
}
