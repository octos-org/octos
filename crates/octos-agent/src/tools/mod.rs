//! Tool framework for agent tool execution.
//!
//! # Typed `ToolContext` migration (M8.1)
//!
//! Tools receive execution context through [`ToolContext`]. Historically the
//! context was delivered indirectly via the [`TOOL_CTX`] task-local, which the
//! executor populated before calling each tool's [`Tool::execute`]. That works
//! but makes the carrier invisible at the trait surface, so tools that want a
//! field must either read the task-local or reach into globals.
//!
//! M8.1 introduces [`Tool::execute_with_context`], a typed entry point that
//! threads `&ToolContext` explicitly. To keep the migration additive:
//!
//! - The trait's default implementation of `execute_with_context` falls back
//!   to the legacy [`Tool::execute`]. Existing tools keep working unchanged.
//! - Migrated tools override `execute_with_context` and use the typed record.
//!   Their `execute` impl simply re-enters `execute_with_context` with a
//!   zero-value context so out-of-band callers (tests, integrations that have
//!   not been updated) still get predictable behaviour.
//! - [`ToolContext`] carries the legacy fields *plus* placeholder stubs for
//!   future milestones: [`AgentDefinitions`], [`ToolPermissions`],
//!   [`FileStateCache`] (populated in M8.4), [`Notifications`], and
//!   [`AppStateHandle`]. Each stub is annotated with the future issue that
//!   will populate it. They all have cheap zero-value constructors so today's
//!   executor can build a context without wiring.
//!
//! The executor still sets [`TOOL_CTX`] for legacy plugin tools that rely on
//! the task-local read path (see `plugins/tool.rs`). Once every tool is
//! migrated the task-local becomes redundant and can be retired, but that
//! clean-up is out of scope for M8.1.

mod batch_read;
mod build_cache_usage;
mod file_mutation_metadata;
mod output_claims;
pub use build_cache_usage::{BuildCacheUsage, BuildCacheUseGuard};
pub(crate) use file_mutation_metadata::{
    file_mutation_previous_text, read_file_mutation_previous_text,
};

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use eyre::Result;
use octos_core::TokenUsage;

use crate::progress::ProgressReporter;
use octos_core::{PathClassification, SessionScope};

/// Error a tool returns when the MODEL supplied malformed arguments — a schema
/// / deserialize failure, as opposed to a genuine execution error. Such
/// failures have no side effects, so the serial-batch (M8.8) scheduler treats
/// them as NON-cascading: one malformed call must not cancel its well-formed
/// siblings (#1690). The `Display` text is delivered verbatim to the model, so
/// callers should include the underlying detail plus a schema hint the model
/// can use to self-repair on the next turn.
#[derive(Debug, Clone)]
pub struct ToolInputError(String);

/// Upper bound on a [`ToolInputError`]'s model-facing message. The message can
/// embed caller-controlled content (unknown parameter names), so it is bounded
/// at construction — well under every tool's output limit (`tool_output_limit`
/// min is 20_000) — so an armed tool's `Err` can never exceed the cap and be
/// mangled by the execution loop's blind head/tail cut (#2193 R4). Real
/// validation messages are a few hundred bytes.
pub(crate) const TOOL_INPUT_ERROR_MAX_BYTES: usize = 4096;

impl ToolInputError {
    /// Build an input-validation error from a model-facing message, bounded to
    /// [`TOOL_INPUT_ERROR_MAX_BYTES`] so caller-supplied content (e.g. a
    /// pathological unknown-parameter name) cannot make the error exceed the
    /// tool-output cap.
    pub fn new(message: impl Into<String>) -> Self {
        let mut message = message.into();
        octos_core::truncate_utf8(&mut message, TOOL_INPUT_ERROR_MAX_BYTES, "…[truncated]");
        Self(message)
    }
}

impl std::fmt::Display for ToolInputError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ToolInputError {}

/// Registry of [`AgentDefinition`]-style manifests available to tools.
///
/// Re-exported from [`crate::agents`] where the schema and loader live. M8.2
/// filled in the stub shipped by M8.1: the registry now carries real
/// [`crate::agents::AgentDefinition`] records by id. `ToolContext` keeps its
/// M8.1 signature (`Arc<AgentDefinitions>`), so consumers of the field do
/// not need to change.
pub use crate::agents::AgentDefinitions;

/// Per-tool permission facts consulted before each execution.
///
/// M8 fix-first item 8 (gap 4b): the M8.1 stub was always allow-all; the
/// agent's recorded [`crate::profile::ProfileDefinition`] envelope was never
/// consulted at the tool boundary even when the profile declared an explicit
/// allow- or deny-list. The struct now carries the resolved policy:
///
/// - [`ToolPermissions::allow_all`] / [`ToolPermissions::default`] preserve
///   the pre-M8.3 status quo (no restrictions).
/// - [`ToolPermissions::from_profile`] derives the deny / allow lists from the
///   profile's `tools` filter, expanding `group:*` references through
///   [`crate::tools::policy::TOOL_GROUPS`] and the user-provided
///   [`crate::profile::PermissionMode`]. Tools not on the allow list (when
///   one is configured) are blocked, and tools on the deny list always lose.
///
/// Permission is evaluated by [`ToolPermissions::is_tool_allowed`] — the same
/// hook the existing tools (e.g. `read_file`) consult before executing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolPermissions {
    /// Coarse permission tier from the profile envelope. Reserved for future
    /// per-tier rules; today the variant is informational so callers can log
    /// it without changing semantics.
    mode: crate::profile::PermissionMode,
    /// Tools the active profile explicitly forbids. Always wins over the
    /// allow list (deny-wins semantics, mirroring [`ToolPolicy`]).
    denied_tools: HashSet<String>,
    /// Optional explicit allow list. When `Some`, only tools whose names are
    /// in the set are permitted. When `None`, no allow-list filter applies
    /// (default behaviour).
    allowed_tools: Option<HashSet<String>>,
}

impl Default for ToolPermissions {
    fn default() -> Self {
        Self::allow_all()
    }
}

impl ToolPermissions {
    /// Allow-all permissions — the zero-value default carried by the context.
    pub fn allow_all() -> Self {
        Self {
            mode: crate::profile::PermissionMode::Default,
            denied_tools: HashSet::new(),
            allowed_tools: None,
        }
    }

    /// Derive a [`ToolPermissions`] envelope from a resolved
    /// [`crate::profile::ProfileDefinition`].
    ///
    /// `group:*` references in the profile's tool filter are expanded
    /// against [`crate::tools::policy::TOOL_GROUPS`] so the runtime gate
    /// matches the registry filter from M8.3. The resulting record is
    /// consulted at every tool boundary (see
    /// [`ToolPermissions::is_tool_allowed`]).
    pub fn from_profile(profile: &crate::profile::ProfileDefinition) -> Self {
        use crate::profile::ProfileTools;
        let mut denied: HashSet<String> = HashSet::new();
        let mut allowed: Option<HashSet<String>> = None;
        match &profile.tools {
            ProfileTools::Default => {}
            ProfileTools::AllowList { tools } => {
                if !tools.is_empty() {
                    allowed = Some(expand_profile_tool_entries(tools));
                }
            }
            ProfileTools::DenyList { tools } => {
                denied = expand_profile_tool_entries(tools);
            }
        }
        Self {
            mode: profile.permissions,
            denied_tools: denied,
            allowed_tools: allowed,
        }
    }

    /// Permission tier carried by the envelope. Reserved for future
    /// per-tier rules; today purely informational.
    pub fn mode(&self) -> crate::profile::PermissionMode {
        self.mode
    }

    /// Check whether the named tool is currently permitted.
    ///
    /// Returns `false` when:
    /// - the tool is on the profile's deny list, or
    /// - the profile carries an allow list and the tool is not in it.
    pub fn is_tool_allowed(&self, tool: &str) -> bool {
        if self.denied_tools.contains(tool) {
            return false;
        }
        match &self.allowed_tools {
            Some(allow) => allow.contains(tool),
            None => true,
        }
    }
}

/// Expand a profile-tool list (which may contain `group:*` references) into
/// a flat set of tool names.
fn expand_profile_tool_entries(entries: &[String]) -> HashSet<String> {
    let mut out: HashSet<String> = HashSet::new();
    for entry in entries {
        if let Some(group) = crate::tools::policy::tool_group_info(entry) {
            for t in group.tools {
                out.insert((*t).to_string());
            }
        } else {
            out.insert(entry.clone());
        }
    }
    out
}

/// File-state cache re-export (M8.4).
///
/// The concrete LRU + mtime/hash implementation lives in
/// [`crate::file_state_cache`]; this re-export keeps the historical public
/// path (`crate::tools::FileStateCache`) stable for downstream users while
/// the ToolContext carries a shared handle.
pub use crate::file_state_cache::FileStateCache;

/// Inbox of in-flight notifications surfaced to tools and the agent loop.
///
/// M8.2/M8.3 will route real notifications (e.g. permission prompts, gate
/// state) through this handle. Today it is a zero-length inbox.
#[derive(Clone, Debug, Default)]
pub struct Notifications {
    // M8.2/M8.3 will add the notification queue and backpressure state here.
}

impl Notifications {
    /// Create an empty notifications inbox.
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether the inbox is empty (no pending notifications). Always `true`
    /// until M8.2/M8.3 start enqueueing notifications.
    pub fn is_empty(&self) -> bool {
        true
    }
}

/// Handle to the ambient app state shared across tools.
///
/// M8.3 will use this to expose profile/app state that tools may read (e.g.
/// the active profile name, locale, workspace contract root). Today it is an
/// empty handle that tools can carry without wiring.
#[derive(Clone, Debug, Default)]
pub struct AppStateHandle {
    // M8.3 will add the shared state handle (Arc<ProfileState>) here.
}

impl AppStateHandle {
    /// Create an empty app-state handle.
    pub fn new() -> Self {
        Self::default()
    }
}

/// Execution context available to tools.
///
/// The legacy fields (`tool_id`, `reporter`, `harness_event_sink`, three
/// attachment lists) carry today's behaviour. The trailing fields are M8.x
/// placeholders — see each field's doc comment for the issue that will wire
/// it up. Building a zero-value context is cheap: all placeholders implement
/// `Default` and the required handles are backed by `Arc` so cloning is O(1).
#[derive(Clone)]
pub struct ToolContext {
    pub tool_id: String,
    /// Runtime identity of the active artifact-evidence observer.
    pub continuation_owner: Option<String>,
    pub reporter: Arc<dyn ProgressReporter>,
    /// Local newline-delimited JSON sink for structured harness progress.
    pub harness_event_sink: Option<String>,
    pub attachment_paths: Vec<String>,
    pub audio_attachment_paths: Vec<String>,
    pub file_attachment_paths: Vec<String>,
    /// Agent manifests available to tools. M8.2 will populate this.
    pub agent_definitions: Arc<AgentDefinitions>,
    /// Per-tool permission facts. M8.3 will populate this.
    pub permissions: ToolPermissions,
    /// File-state cache shared across tools in a turn (M8.4).
    ///
    /// File tools consult this cache on read and invalidate it on write. When
    /// `None`, tools behave as they did pre-M8.4 (no cache, no stub). The
    /// cache is wrapped in `Arc` for tools in the SAME model context.
    /// Independent children use [`FileStateCache::empty_for_new_context`];
    /// copying or sharing parent entries would claim unseen file bodies.
    pub file_state_cache: Option<Arc<FileStateCache>>,
    /// Notification inbox surfaced to tools. M8.2/M8.3 will populate this.
    pub notifications: Arc<Notifications>,
    /// Handle to the ambient app state. M8.3 will populate this.
    pub app_state: AppStateHandle,
    /// M8 parity (W1.A1): shared sub-agent output router from the
    /// session actor. Background sub-agents (pipeline workers, spawn
    /// children) clone this `Arc` so their output lands in the same
    /// disk-backed router the parent session uses for dashboards.
    pub subagent_output_router: Option<Arc<crate::subagent_output::SubAgentOutputRouter>>,
    /// M8 parity (W1.A1): shared sub-agent summary generator. Pipeline
    /// workers clone this so periodic LLM summaries fire for their
    /// background tasks just like top-level spawn children.
    pub subagent_summary_generator: Option<Arc<crate::subagent_summary::AgentSummaryGenerator>>,
    /// LLM provider for tools that need to make independent model calls
    /// (e.g., goal completion verifier). Populated by the session runtime.
    pub llm_provider: Arc<dyn octos_llm::LlmProvider>,
    /// M8 parity (W1.A3): per-session task supervisor. Pipeline node
    /// workers register a child task in this supervisor so the admin
    /// dashboard sees the substructure under the parent run_pipeline
    /// invocation.
    pub task_supervisor: Option<Arc<crate::task_supervisor::TaskSupervisor>>,
    /// M8 parity (W1.A4): shared cost accountant. Pipeline workers
    /// open a per-node `CostReservationHandle` against the same
    /// accountant the session uses so spend is unified under the
    /// parent contract.
    pub cost_accountant: Option<Arc<crate::cost_ledger::CostAccountant>>,
    /// M8 parity: parent session key when the tool is invoked from a
    /// session actor. Pipeline workers and spawn children carry this so
    /// background-task registration links to the owning session.
    pub parent_session_key: Option<String>,
    /// Guard C (issue #607): nesting depth for `spawn`-within-`spawn`
    /// invocations. Top-level tool calls ride at depth 0; the spawn
    /// tool increments this when dispatching a child agent so the
    /// child's own `spawn` calls see the higher value via `TOOL_CTX`.
    /// Beyond [`crate::tools::spawn::MAX_SPAWN_DEPTH`] the spawn tool
    /// refuses further nesting to bound mutual-recursion blowups.
    pub spawn_depth: u8,
    /// Phase 1 of the [`SessionScope`] migration (PR #1198 follow-up):
    /// the single filesystem contract for this session. Constructed at
    /// the host entry point (`chat.rs` for solo, `serve.rs` /
    /// `runtime/session.rs` for multi-tenant) and threaded through
    /// `TOOL_CTX` so any tool can derive its CWD and validate paths
    /// against the same scope.
    ///
    /// `Optional` because Phase 1 is additive — no consumer reads this
    /// yet. Phase 2 PRs will migrate `RunPipelineTool.working_dir`,
    /// plugin tool `work_dir`, file tools, shell, etc. to read from
    /// this field; Phase 3 will retire bespoke validators like
    /// `api_session_workspace_dirs` in favour of
    /// [`SessionScope::workspace`]. See `octos_core::session_scope`
    /// for the contract and migration notes.
    pub session_scope: Option<Arc<SessionScope>>,
    /// Goal ID this tool call is working under (peer-agent-based goal).
    /// Populated from `Agent::goal_id` at tool dispatch when the agent runs
    /// inside a peer staged with a `goal` file. Read by the `goal_*` tool
    /// family to scope reads/writes to the goal without requiring the model
    /// to repeat the id on every call.
    pub goal_id: Option<String>,
    /// Task ID within the goal (peer-agent-based goal). Populated from
    /// `Agent::task_id`. May be `None` even when `goal_id` is set (the peer
    /// is goal-scoped but not task-scoped).
    pub task_id: Option<String>,
    /// The session that staged this peer (peer-agent-based goal). Captured
    /// at peer boot from `peers/<slug>/originator` and threaded through so
    /// goal-aware tools (`goal_get` by-id, `model_goal_record_peer_finding`)
    /// can enforce the goal-binding check WITHOUT re-reading the originator
    /// file on every call. `None` for non-peer sessions.
    pub originator_session: Option<String>,
    /// Build-cache pool slot this peer's CURRENT turn holds (outer-loop #4,
    /// design docs/build-cache-pool.md §7.4). Populated from
    /// `Agent::build_cache_slot` at tool dispatch, exactly like
    /// `goal_id`/`task_id` above. Read by the shell tool to inject
    /// `CARGO_TARGET_DIR=<slot>/target` + `CARGO_INCREMENTAL=0` PER TOOL
    /// CALL — never via `std::env::set_var`, because on the serve path a
    /// peer shares the process with the master and every other peer.
    /// `None` for non-peer sessions and a peer turn that failed to acquire
    /// a slot (it still runs, just with cargo's default target dir).
    pub build_cache_slot: Option<std::path::PathBuf>,
    /// Shared child lifetime accounting for this cache claim.
    pub build_cache_usage: Option<BuildCacheUsage>,
    /// Post-edit formatting (issue #1774): when true, a successful
    /// `edit_file` / `write_file` / `diff_edit` runs the language formatter
    /// for the file (rustfmt / prettier / black / gofmt — see
    /// [`crate::format`]) and echoes the formatted content back in the tool
    /// result. OFF by default; threaded from
    /// [`crate::AgentConfig::format_after_edit`].
    pub format_after_edit: bool,
    /// T-0241: per-turn cancellation token. Fires when the owning turn is
    /// interrupted (`turn/interrupt`, Stop, work-secret revocation/expiry)
    /// or its connection is aborted. Tools that hold live resources
    /// (subprocesses, streams) MUST watch it and terminate those resources
    /// before returning; the execution layer also stops waiting for the
    /// tool after a bounded grace and drops its future, so a tool that
    /// cannot cancel must return an honest "still running / outcome
    /// unknown" result rather than claiming success. `None` means the call
    /// is not attached to a cancellable turn (tests, detached workers).
    pub turn_cancel: Option<tokio_util::sync::CancellationToken>,
}

impl ToolContext {
    /// Zero-value context suitable for unit tests and tools that do not need
    /// live executor wiring. Uses a [`crate::progress::SilentReporter`] and
    /// leaves every M8.x placeholder at its default.
    pub fn zero() -> Self {
        // Noop provider for zero context (always fails, tools should not use it)
        struct NoopProvider;
        #[async_trait::async_trait]
        impl octos_llm::LlmProvider for NoopProvider {
            async fn chat(
                &self,
                _messages: &[octos_core::Message],
                _tools: &[octos_llm::ToolSpec],
                _config: &octos_llm::ChatConfig,
            ) -> eyre::Result<octos_llm::ChatResponse> {
                eyre::bail!("ToolContext::zero() has no real provider")
            }
            fn model_id(&self) -> &str {
                "noop"
            }
            fn provider_name(&self) -> &str {
                "noop"
            }
        }

        Self {
            tool_id: String::new(),
            continuation_owner: None,
            reporter: Arc::new(crate::progress::SilentReporter),
            harness_event_sink: None,
            attachment_paths: Vec::new(),
            audio_attachment_paths: Vec::new(),
            file_attachment_paths: Vec::new(),
            agent_definitions: Arc::new(AgentDefinitions::new()),
            permissions: ToolPermissions::default(),
            file_state_cache: None,
            notifications: Arc::new(Notifications::new()),
            app_state: AppStateHandle::new(),
            subagent_output_router: None,
            subagent_summary_generator: None,
            llm_provider: Arc::new(NoopProvider),
            task_supervisor: None,
            cost_accountant: None,
            parent_session_key: None,
            spawn_depth: 0,
            session_scope: None,
            goal_id: None,
            task_id: None,
            originator_session: None,
            build_cache_slot: None,
            build_cache_usage: None,
            format_after_edit: false,
            turn_cancel: None,
        }
    }
}

tokio::task_local! {
    pub(crate) static CONTINUATION_OWNER: String;
}

tokio::task_local! {
    /// Task-local tool context, scoped per tool invocation in agent.rs.
    pub static TOOL_CTX: ToolContext;
}

mod bridges;

pub use bridges::{
    TOOL_APPROVAL_CTX, ToolApprovalDecision, ToolApprovalRequest, ToolApprovalRequester,
    USER_QUESTION_CTX, UserQuestionOutcome, UserQuestionRequest, UserQuestionRequester,
};

#[derive(Clone, Debug, Default)]
pub struct TurnAttachmentContext {
    pub attachment_paths: Vec<String>,
    pub audio_attachment_paths: Vec<String>,
    pub file_attachment_paths: Vec<String>,
    pub prompt_summary: Option<String>,
    /// Explicit live-video signal for this turn, set by the ingress from the
    /// client (`InboundMessage.metadata.live_video`) — NOT inferred from
    /// attachment types. True only when the turn is a real-time video call
    /// whose attached image is the user's current camera frame; drives the
    /// agent loop's video-call note. Defaults false (no auto-detection): a
    /// voice note plus an uploaded image is not a camera frame.
    pub live_video: bool,
}

tokio::task_local! {
    /// Task-local per-turn attachment context, scoped to the current agent run.
    pub static TURN_ATTACHMENT_CTX: TurnAttachmentContext;
}

/// Progress update from a long-running tool execution.
#[derive(Debug, Clone)]
pub enum ToolProgress {
    /// Status text update (e.g., "Searching 3 of 10 sources...").
    Status(String),
    /// Percentage completion (0..100).
    Percent(u8),
    /// Intermediate result available (e.g., partial research findings).
    Intermediate { summary: String },
}

/// Concurrency class of a tool — controls how the executor admits tool calls
/// into a parallel batch (M8.8).
///
/// The executor unconditionally ran every tool call in parallel before M8.8.
/// This was unsafe in the presence of mutating tools: a `shell && rm foo`
/// dispatched concurrently with `read_file foo/x` could race and return
/// inconsistent observations to the LLM. Claude Code's
/// `StreamingToolExecutor.ts` classifies tools via `isConcurrencySafe()` —
/// this mirrors that pattern at the trait surface.
///
/// Admission policy (implemented in `agent::execution`):
/// - If every call in the batch is [`ConcurrencyClass::Safe`], the batch
///   dispatches in parallel.
/// - If every call is [`ConcurrencyClass::Exclusive`], the batch runs
///   serially in call order. A single error from an exclusive call cancels
///   the remaining peers so the LLM sees the cascade instead of continuing
///   to mutate state on a doomed path.
/// - Mixed batches (#1766) run the Safe calls in parallel first, then the
///   Exclusive calls serially in call order; results are reassembled in the
///   original call order. See the `agent::execution` module doc for the
///   pinned visibility and cascade semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConcurrencyClass {
    /// Read-only / side-effect-free. Can run in parallel with any other
    /// `Safe` tool call without observable interference.
    #[default]
    Safe,
    /// Mutating or stateful (writes files, spawns shells, updates memory).
    /// Must run serialized: no other tool call runs concurrently while an
    /// `Exclusive` call is in-flight.
    Exclusive,
}

/// Result of executing a tool.
#[derive(Default)]
pub struct ToolResult {
    /// Output to return to the LLM.
    pub output: String,
    /// Whether the tool execution succeeded.
    pub success: bool,
    /// File modified by this tool (if any).
    pub file_modified: Option<PathBuf>,
    /// Files to automatically send to the user via the chat channel.
    /// Plugins set this via `"files_to_send": ["/path/to/file.mp3"]` in JSON output.
    /// The agent loop sends these files after the tool completes, without requiring
    /// an extra LLM call to invoke send_file.
    pub files_to_send: Vec<PathBuf>,
    /// Tokens used by this tool (for subagent tools).
    pub tokens_used: Option<TokenUsage>,
    /// Optional structured side-channel for tool-specific metadata the host
    /// wants to surface beyond plain output text. Used today for per-node
    /// cost rows from `run_pipeline` (`{"node_costs": [...]}`); the session
    /// actor pulls this back into the SSE `done` event so the W1.G4 cost
    /// panel can render real per-node attribution. Absent (`None`) for
    /// every tool that does not opt in — keeps legacy callers byte-identical.
    pub structured_metadata: Option<serde_json::Value>,
    /// Optional named outputs the tool wants the contract layer to read.
    /// spawn_only plugin tools emit this via `"named_outputs": {"key": "value"}`
    /// in their stdout JSON envelope. The contract layer forwards each entry
    /// to validators so `${output.<key>}` interpolation can resolve against
    /// tool-emitted values (e.g. `mofa_publish` emitting `deploy_url`).
    /// Values are restricted to strings in v1; key shape must match
    /// `[a-z][a-z0-9_]*`. Absent (`None`) when the tool emits nothing.
    pub named_outputs: Option<std::collections::HashMap<String, String>>,
    /// Images the tool wants the MODEL to look at, as local paths.
    ///
    /// Distinct from `files_to_send`, which goes to the human over the chat
    /// channel and is never shown to the model. The agent loop turns these
    /// into a user-role message carrying the image right after the batch's
    /// tool results, so every provider renders them the way it renders a
    /// user upload; the tool result itself stays text. `view_image` is the
    /// first caller: without this, a model that can see could only learn a
    /// screenshot's byte length.
    pub model_media: Vec<PathBuf>,
}

/// Trait for implementing tools.
///
/// # Context threading
///
/// Tools get their execution context through one of two entry points:
///
/// - [`Tool::execute`] — the legacy argument-only entry point. Kept as the
///   primary signature so unmigrated tools, tests, and external callers do
///   not need to thread a [`ToolContext`]. The default implementation of
///   `execute_with_context` delegates here, so implementors who override
///   only `execute` keep working.
/// - [`Tool::execute_with_context`] — the typed entry point introduced by
///   M8.1. Migrated tools override this and may read any field on the
///   [`ToolContext`]. The default body re-enters the legacy [`Tool::execute`]
///   so unmigrated tools keep working.
///
/// A tool should override at most one of the two. Overriding both produces
/// two independent entry paths that the executor cannot reconcile.
#[async_trait]
pub trait Tool: Send + Sync {
    /// Whether this implementation has a usable backend. Compatibility stubs
    /// remain callable but need not consume space in discovery results.
    fn is_available(&self) -> bool {
        true
    }

    /// Tool name (must be unique).
    fn name(&self) -> &str;

    /// Description for the LLM.
    fn description(&self) -> &str;

    /// JSON Schema for input parameters.
    fn input_schema(&self) -> serde_json::Value;

    /// Semantic tags for capability-based filtering (e.g. "code", "web", "gateway").
    /// Default: empty (tool passes all tag filters).
    fn tags(&self) -> &[&str] {
        &[]
    }

    /// Model contexts in which this tool may be advertised.
    ///
    /// An empty list keeps the existing behavior: the tool is visible in
    /// every context. A non-empty list requires an exact match with the
    /// active per-turn context.
    fn contexts(&self) -> &[String] {
        &[]
    }

    /// Execute the tool with the given arguments.
    ///
    /// Kept as the primary entry point so existing tools, tests, and
    /// integrations do not need to construct a [`ToolContext`]. Migrated
    /// tools re-enter this via [`Tool::execute_with_context`]; to avoid
    /// infinite recursion implementors that override `execute_with_context`
    /// must also override `execute` to call
    /// `self.execute_with_context(&ToolContext::zero(), args).await`.
    async fn execute(&self, args: &serde_json::Value) -> Result<ToolResult>;

    /// Execute the tool with typed execution context.
    ///
    /// How the model can retrieve output the harness had to truncate.
    ///
    /// Returns the advice to append when this tool's result exceeded
    /// [`octos_core::tool_output_limit`] and was cut. `None` (the default)
    /// means the tool has no resume path, so no advice is offered rather than
    /// inventing one.
    ///
    /// ## Why this lives on the tool
    ///
    /// Truncation happens in the execution loop, at the one point every tool's
    /// output funnels through — which is also the point that knows the least.
    /// By then the result is a plain `String`: the helper doing the cutting
    /// (`truncate_head_tail_report(s, max_len, head_ratio)`) receives a string and a
    /// number and cannot know which tool produced it, whether that tool paginates,
    /// or what the parameter is called.
    ///
    /// So the loop knows it truncated but not how to resume; the tool knows how
    /// to resume but not that it was truncated. This hook is the missing half.
    ///
    /// Without it the model is told only `... [47000 bytes omitted] ...` — a
    /// dead end whose only recovery is re-running the call, which returns the
    /// same output cut the same way and spends the tokens the cap was meant to
    /// save.
    ///
    /// # Arguments
    /// * `args` - the arguments this call was made with, so advice can name a
    ///   concrete next call rather than a generic one.
    /// * `omitted_bytes` - how much was dropped.
    fn truncation_recovery(
        &self,
        args: &serde_json::Value,
        omitted_bytes: usize,
    ) -> Option<String> {
        let _ = (args, omitted_bytes);
        None
    }

    /// The default implementation delegates to [`Tool::execute`], discarding
    /// the context. Tools that want to read [`ToolContext`] fields override
    /// this and ignore `execute`'s default path. See the module-level doc
    /// comment for the migration pattern.
    async fn execute_with_context(
        &self,
        _ctx: &ToolContext,
        args: &serde_json::Value,
    ) -> Result<ToolResult> {
        self.execute(args).await
    }

    /// Pre-flight argument validation that runs synchronously in the
    /// foreground before the `spawn_only` intercept dispatches the tool to
    /// the background.
    ///
    /// Returning `Err(msg)` causes the spawn_only intercept to surface the
    /// error as a normal tool_result `Message` (mirroring the policy-deny
    /// path) so the LLM sees the failure in its next iteration and can
    /// retry with corrected arguments. Without this, an LLM-generated bad
    /// argument (e.g. a structurally invalid DOT graph for `run_pipeline`)
    /// fails inside the background task with no chance for the agent to
    /// re-engage — the user sees an error bubble but the LLM thinks it
    /// succeeded.
    ///
    /// Default: no pre-flight check (returns `Ok`). Override in tools whose
    /// arguments are LLM-generated and cheap to validate, where catching
    /// malformed input synchronously avoids a wasted background round-trip.
    /// Keep the check fast (parse + structural validation, no network /
    /// long-running work) since it blocks the agent's foreground turn.
    async fn pre_flight_validate(&self, _args: &serde_json::Value) -> Result<(), String> {
        Ok(())
    }

    /// Downcast support for concrete tool access (e.g. mofa-make dispatcher wiring).
    fn as_any(&self) -> &dyn std::any::Any {
        // Default: no downcasting. Override in tools that need it.
        &()
    }

    /// Concurrency class for parallel-batch admission (M8.8).
    ///
    /// The default is [`ConcurrencyClass::Safe`] so pre-M8.8 tools keep their
    /// parallel-friendly behaviour. Mutating or stateful tools override this
    /// and return [`ConcurrencyClass::Exclusive`] — see each tool's doc for
    /// rationale. The executor (in `agent::execution`) uses the class to
    /// decide whether a batch may fan out in parallel or must serialize.
    fn concurrency_class(&self) -> ConcurrencyClass {
        ConcurrencyClass::Safe
    }

    /// Per-tool execution timeout (seconds) enforced at the registry dispatch
    /// boundary (Gap 3.3). `None` (the default) means "use the registry's
    /// global backstop" (`ToolRegistry::set_tool_timeout_secs`, default
    /// 1800s). A tool that returns `Some(n)` caps its own foreground
    /// execution at `n` seconds regardless of the registry default.
    ///
    /// This is the LAST line of defence against a hung foreground tool
    /// wedging the session-actor turn forever: even direct registry callers
    /// (e.g. the serve/API tool path, workspace-contract auto-send) that do
    /// not run inside the agent loop's per-batch timeout get bounded here.
    /// It composes with — and is independent of — the agent loop's
    /// fast/long batch timeout in `agent::execution`, which fires first on
    /// that path; this guard catches the unprotected direct-caller paths.
    ///
    /// `spawn_only` tools are intercepted and backgrounded BEFORE the
    /// foreground dispatch path, so they never hit this timeout. Genuinely
    /// long-running foreground tools (`web_fetch`, `web_search`, `browser`,
    /// deep research/crawl) inherit the generous 1800s backstop by leaving
    /// this `None`; the `shell` tool already clamps its own internal timeout
    /// to [1, 600]s, well under the backstop, so it is not double-killed.
    ///
    /// Default: `None` (inherit the registry backstop). Keep any override
    /// generous — this is a safety net, not a tuning knob for normal work.
    fn execution_timeout_secs(&self) -> Option<u64> {
        None
    }

    /// Preserve legacy, host-installed background wiring during startup
    /// profile filtering. Built-in optional observers opt out so their
    /// registration obeys the same allow/deny lists as foreground tools.
    /// This retention is not permission to execute a denied tool.
    fn preserve_background_registration(&self) -> bool {
        true
    }

    /// Whether the spawn-only executor may retry a failed result whose text
    /// resembles a transient transport error. Observers of already submitted
    /// jobs opt out: a terminal receipt is evidence, even when the external
    /// job's error text says "connection refused". Re-executing the observer
    /// would delay that notification and reset its observation deadline.
    fn retry_background_on_transient_error(&self) -> bool {
        true
    }

    /// Whether this tool BLOCKS on human input (e.g. `ask_user_question`
    /// awaits the [`USER_QUESTION_CTX`] requester until the client answers,
    /// exactly as the approval gate blocks on [`TOOL_APPROVAL_CTX`]). Such a
    /// tool must be EXEMPT from the dispatch-boundary timeout in
    /// [`ToolRegistry::execute_with_context`]: a human may legitimately take
    /// longer than any finite tool timeout, and firing the timeout would drop
    /// the requester's receiver and leak the pending question/approval store
    /// entry forever (Gap-3.3 interaction). The waiting future is instead
    /// cancelled the right way — when the turn is interrupted the pending
    /// store drains the entry and resolves the waiter as `Cancelled`.
    ///
    /// Returning `true` here makes the registry skip wrapping the call in the
    /// dispatch timeout entirely (it still composes with the agent loop's
    /// per-turn lifecycle and the turn-interrupt drain). Default: `false`.
    fn blocks_on_human_input(&self) -> bool {
        false
    }

    /// Where the tool's code runs ([`ToolOrigin`]). The registry records it
    /// at registration, so a filter can select by origin instead of by
    /// name. Default: [`ToolOrigin::Builtin`].
    fn origin(&self) -> ToolOrigin {
        ToolOrigin::Builtin
    }
}

// Tool registry (extracted to its own module)
pub(crate) mod dispatcher;
/// Observe-only probe for the read-paging decision (changes no behaviour).
pub(crate) mod read_paging_probe;
pub(crate) mod read_window;
mod registry;
pub use registry::{RESERVED_BUILTIN_TOOL_NAMES, ToolOrigin, ToolRegistry};

// Tool policy
pub mod policy;
pub use policy::{PolicyDecision, ToolPolicy, keep_tool_in_slides_session};

// Shared dispatch-policy gate (#714 / #713) re-exported from the
// crate root so [`SpawnTool::with_dispatch_policy`] callers can pull
// the type alongside the other `tools::*` re-exports.
pub use crate::dispatch_policy::{
    DispatchBackendMetadata, DispatchPolicy, DispatchTarget, GateDenial, enforce_dispatch_gates,
    enforce_dispatch_gates_for_backend,
};

// Robot safety-tier groups consulted by ToolPolicy evaluation.
pub mod robot_groups;
pub use robot_groups::{RobotToolRegistry, install_registry as install_robot_registry};

// Shared SSRF protection
pub mod ssrf;

// #1770: structured, model-facing tool-argument validation.
pub mod args;

// Built-in tools
pub mod apply_patch;
pub mod ask_user_question;
pub mod coding_tools;
pub(crate) mod command_output;
pub mod deep_search;
pub mod delegate;
pub mod diff_edit;
pub mod dora_bridge;
pub mod edit_file;
pub mod glob_tool;
pub mod grep_tool;
#[cfg(unix)]
pub mod host_confined_search;
pub mod http;
pub mod list_dir;
pub mod manage_skills;
pub mod mcp_agent;
pub mod memory_load;
pub mod memory_note;
pub mod memory_search;
pub mod message;
pub mod peer_close;
pub mod peer_gather;
pub mod peer_handoff;
pub mod peer_host_tool;
pub mod peer_list;
pub mod peer_respond;
pub mod peer_send_input;
pub mod read_file;
pub mod read_task_output;
pub mod recall;
pub mod recall_memory;
pub mod record_memory_use;
pub(crate) mod replacer;
pub mod research_utils;
pub mod save_memory;
pub mod send_app_card;
pub mod send_file;
pub mod shell;
#[allow(dead_code)]
pub(crate) mod site_crawl;
pub mod spawn;
pub mod synthesize_research;
pub(crate) mod terminal;
pub mod watch_external_job;
pub mod web_fetch;
pub mod web_search;
pub mod write_file;
pub mod write_grant;

pub mod admin;
pub mod browser;
pub mod check;
pub mod check_background_tasks;
pub mod check_workspace_contract;
pub mod mofa_make;
pub mod tool_config;
pub mod workspace_history;

#[cfg(feature = "git")]
pub mod git;

#[cfg(feature = "ast")]
pub mod code_structure;

pub use apply_patch::ApplyPatchTool;
pub use ask_user_question::AskUserQuestionTool;
pub use coding_tools::{
    BashTool, CloseAgentTool, DelegateAliasTool, ExecCommandTool, ImageGenerationTool,
    RequestUserInputTool, ResumeAgentTool, SendInputTool, SpawnAgentTool, ToolCatalogEntry,
    ToolSearchTool, ToolSuggestTool, UpdatePlanTool, ViewImageTool, WaitAgentTool, WriteStdinTool,
};
pub use deep_search::DeepSearchTool;
pub use delegate::{
    DELEGATED_DENY_GROUP, DELEGATION_METRIC, DelegateTool, DelegationEvent, DelegationOutcome,
    DepthBudget, MAX_DEPTH, build_delegated_child_policy,
};
pub use diff_edit::DiffEditTool;
pub use edit_file::EditFileTool;
pub use glob_tool::GlobTool;
pub use grep_tool::GrepTool;
pub use http::HttpTool;
pub use list_dir::ListDirTool;
pub use manage_skills::ManageSkillsTool;
pub use mcp_agent::{
    DEFAULT_DISPATCH_TIMEOUT_SECS, DEFAULT_HTTP_CONNECT_TIMEOUT_SECS,
    DEFAULT_HTTP_READ_TIMEOUT_SECS, DispatchContextContract, DispatchOutcome, DispatchRequest,
    DispatchResponse, HttpMcpAgent, McpAgentBackend, McpAgentBackendConfig, SharedBackend,
    StdioMcpAgent, build_backend_from_config, build_dispatch_event_payload, dispatch_with_metrics,
    record_dispatch,
};
pub use memory_load::MemoryLoadTool;
pub use memory_note::MemoryNoteTool;
pub use memory_search::MemorySearchTool;
pub use message::MessageTool;
pub use peer_close::{PeerCloseCallback, PeerCloseTool};
pub use peer_gather::{PeerGatherCallback, PeerGatherTool};
mod peer_publish;
pub use peer_handoff::{
    PeerHandoffCallback, PeerHandoffRequest, PeerHandoffStaged, PeerHandoffTool,
};
pub use peer_host_tool::{
    HOST_TOOL_MAX_ARGS_BYTES, HostRoutedTool, HostToolAudit, HostToolCall, HostToolCallOutcome,
    HostToolCaller, HostToolConfirm, HostToolDecl, HostToolRisk, HostToolRouter, OccurrenceClaim,
};
pub use peer_list::{PeerListCallback, PeerListTool};
pub use peer_publish::{PeerPublishCallback, PeerPublishTool};
pub use peer_respond::{
    PeerRespondAnswer, PeerRespondCallback, PeerRespondRequest, PeerRespondTool,
};
pub use peer_send_input::{
    PeerSendInputAnswerCallback, PeerSendInputCallback, PeerSendInputDelivery,
    PeerSendInputRefusal, PeerSendInputRequest, PeerSendInputTool,
};
pub use read_file::ReadFileTool;
pub use read_task_output::ReadTaskOutputTool;
pub use recall::{RecallTool, ToolOutputLedger};
pub use recall_memory::RecallMemoryTool;
pub use record_memory_use::RecordMemoryUseTool;
pub use save_memory::SaveMemoryTool;
pub use send_app_card::SendAppCardTool;
pub use send_file::SendFileTool;
pub use shell::ShellTool;
pub use spawn::{BackgroundResultKind, BackgroundResultPayload, SpawnTool};
pub use synthesize_research::SynthesizeResearchTool;
pub use watch_external_job::WatchExternalJobTool;
pub use web_fetch::WebFetchTool;
pub use web_search::WebSearchTool;
pub use write_file::WriteFileTool;
pub use write_grant::{
    DENIED_MARKER, WriteGrantViolation, WriteGrantViolationSink, WritePathGrant,
};

pub use browser::BrowserTool;
pub use check::CheckTool;
pub use check_background_tasks::CheckBackgroundTasksTool;
pub use check_workspace_contract::CheckWorkspaceContractTool;
pub use mofa_make::{
    MakeTypeEntry, MofaDescribeContentTypeTool, MofaMakeTool, make_dispatcher_with_entries,
};
pub use tool_config::{ConfigureToolTool, ToolConfigStore};
pub use workspace_history::{WorkspaceDiffTool, WorkspaceLogTool, WorkspaceShowTool};

#[cfg(feature = "git")]
pub use git::GitTool;

#[cfg(feature = "ast")]
pub use code_structure::CodeStructureTool;

use std::path::{Component, Path};

use crate::policy::FilesystemScope;

/// Resolve a user-provided tool-argument path, ensuring it stays within
/// `base_dir` **or** inside the authenticated upload tmpdir.
///
/// This is a thin compatibility wrapper around
/// [`octos_core::file_handle::resolve_tool_path`] — the unified resolver
/// introduced by `refactor: unified file-path resolver`. The wrapper
/// preserves the historical signature (`(base_dir, user_path) ->
/// Result<PathBuf>`) so existing tool implementations keep compiling,
/// but the actual policy now lives in `octos-bus` so every entry point
/// (read_file/write_file/edit_file/glob/grep/list_dir, plugin tools,
/// `send_file`, `read_task_output`) follows the same resolution table:
///
/// - `up/<base64>` / `up/<base64>/<display>` upload-handle short-circuit
/// - `pf/<base64>` / `pf/<base64>/<display>` profile-handle short-circuit
///   (only honoured when the call site supplies a profile root)
/// - absolute paths inside upload tmpdir, workspace, or profile root
/// - bare basenames that exist under the upload tmpdir
/// - workspace-relative paths (with `..` traversal rejected)
///
/// Symlink rejection is the caller's responsibility — use
/// `read_no_follow` / `write_no_follow` on the returned path. The
/// resolver only checks containment; the open-time `O_NOFOLLOW` is
/// what closes the symlink-redirect class of escape.
///
/// Callers that need to know whether the resolved file lives inside
/// the upload tmpdir vs the workspace (e.g. for read-only enforcement
/// on profile files) should call
/// [`octos_core::file_handle::resolve_tool_path`] directly and inspect
/// the [`octos_core::file_handle::ToolPathScope`].
pub fn resolve_path(base_dir: &Path, user_path: &str) -> Result<PathBuf> {
    match octos_core::file_handle::resolve_tool_path(base_dir, None, user_path) {
        Ok(resolved) => Ok(resolved.absolute),
        Err(octos_core::file_handle::ToolPathError::Traversal) => {
            eyre::bail!("path outside working directory: {}", user_path)
        }
        Err(octos_core::file_handle::ToolPathError::OutsideAllowedRoots) => {
            // Preserve the legacy error text — call sites and tests
            // string-match on "absolute paths are not allowed" to
            // identify the upload-tmpdir-only escape rejection.
            eyre::bail!(
                "absolute paths are not allowed outside the upload tmpdir: {}",
                user_path
            )
        }
        Err(octos_core::file_handle::ToolPathError::DecodeFailed) => {
            // Should not happen for callers that pass `profile_root =
            // None` (only `pf/...` handles produce `DecodeFailed`). If
            // we ever do see one, surface it as the closest matching
            // legacy message rather than silently swallowing it.
            eyre::bail!("path outside working directory: {}", user_path)
        }
    }
}

// Note: lexical-normalisation and lossy-canonicalisation now live in
// `octos_core::file_handle::resolve_tool_path` so every entry point (file
// tools, plugin tools, send_file, read_task_output) shares the same
// machinery. The previously-inline helpers were retired with that
// unification.

/// Resolve a user-provided path under an explicit filesystem scope.
///
/// [`FilesystemScope::Workspace`] (default) preserves the historical
/// workspace fence and delegates to [`resolve_path`] (unified resolver).
///
/// [`FilesystemScope::Host`] is used only by the explicit
/// `DangerFullAccess` permission profile (solo-runtime only). It accepts
/// absolute paths anywhere on disk after syntactic normalization and
/// resolves relative paths against `base_dir`. The workspace fence is
/// explicitly bypassed — the safety guarantee comes from the gating on
/// `PermissionProfile::DangerFullAccess` + `RuntimeMode::Solo`.
pub fn resolve_path_with_scope(
    base_dir: &Path,
    user_path: &str,
    filesystem_scope: FilesystemScope,
) -> Result<PathBuf> {
    let resolved = if filesystem_scope.is_host() {
        let candidate = PathBuf::from(user_path);
        if candidate.is_absolute() {
            normalize_lexical(&candidate)
        } else {
            normalize_lexical(&base_dir.join(user_path))
        }
    } else {
        resolve_path(base_dir, user_path)?
    };
    if is_process_secret_path(&resolved) || is_process_secret_path(Path::new(user_path)) {
        eyre::bail!("process environments and command lines are off limits: {user_path}");
    }
    Ok(resolved)
}

/// Refuse any path that reaches into a git directory (`.git` as ANY path
/// component). The kernel runs `git` over agent-writable trees (workspace
/// snapshots, history tools, receipts), and a repository's `config`,
/// `hooks/` and `info/attributes` decide which programs git executes — a
/// model that could write `sites/x/.git/config` could make the kernel run a
/// clean filter, fsmonitor or hook of its choosing. File tools therefore
/// never create, modify, move or delete anything inside a `.git`.
///
/// Judged on the lexical path and, when an ancestor exists, on its canonical
/// form (so a symlink pointing into a `.git` does not slip past). Matching is
/// case-insensitive and ignores the trailing dots/spaces and `GIT~1` short
/// name NTFS/APFS normalise away, the same spellings git itself refuses.
pub fn refuse_git_internal_path(path: &Path) -> std::result::Result<(), String> {
    fn is_git_dir_name(name: &std::ffi::OsStr) -> bool {
        let lossy = name.to_string_lossy();
        let head = lossy.split(':').next().unwrap_or("");
        let trimmed = head.trim_end_matches(['.', ' ']).to_ascii_lowercase();
        trimmed == ".git" || trimmed == "git~1"
    }
    fn has_git_component(path: &Path) -> bool {
        path.components()
            .any(|c| matches!(c, Component::Normal(name) if is_git_dir_name(name)))
    }
    let refused = || {
        Err(format!(
            "writes into a .git directory are not permitted: {}",
            octos_core::truncated_utf8(&path.display().to_string(), 200, "…")
        ))
    };
    if has_git_component(path) || has_git_component(&normalize_lexical(path)) {
        return refused();
    }
    // Canonical check: the deepest existing ancestor, resolved, plus the
    // not-yet-existing tail.
    let mut existing = path.to_path_buf();
    let mut tail = Vec::new();
    while !existing.as_os_str().is_empty() && std::fs::symlink_metadata(&existing).is_err() {
        match (
            existing.file_name().map(|n| n.to_owned()),
            existing.parent(),
        ) {
            (Some(name), Some(parent)) => {
                tail.push(name);
                existing = parent.to_path_buf();
            }
            _ => break,
        }
    }
    if let Ok(mut real) = std::fs::canonicalize(&existing) {
        for name in tail.iter().rev() {
            real.push(name);
        }
        if has_git_component(&real) {
            return refused();
        }
    }
    Ok(())
}

/// A process's private view: anything under `/proc/self`, `/proc/thread-self`
/// or `/proc/<pid>` (environment, command line, `fd/`, `root/`, `cwd`,
/// `mem`, …) and `/dev/fd`, `/dev/std*`. No file tool opens these in any
/// filesystem scope, however the path is spelled: the raw spelling, its
/// lexical normalization and (when it exists) its canonical target are all
/// judged, so `..`, a leading `/../..` or a workspace symlink cannot slip
/// past. System-wide `/proc` files such as `/proc/cpuinfo` stay readable.
pub fn is_process_secret_path(path: &Path) -> bool {
    fn private_view(path: &Path) -> bool {
        let text = path.to_string_lossy();
        let text = text.trim_end_matches('/');
        let mut parts = text
            .split('/')
            .filter(|part| !part.is_empty() && *part != ".");
        let found = |parts: &mut dyn Iterator<Item = &str>| -> bool {
            let mut previous = "";
            for part in parts {
                if previous == "proc"
                    && (part == "self"
                        || part == "thread-self"
                        || part.chars().all(|c| c.is_ascii_digit()))
                {
                    return true;
                }
                if previous == "dev" && (part == "fd" || part.starts_with("std")) {
                    return true;
                }
                if part == "environ" || part == "cmdline" {
                    return text.contains("/proc/");
                }
                previous = part;
            }
            false
        };
        found(&mut parts)
    }
    private_view(path)
        || private_view(&normalize_lexical(path))
        || std::fs::canonicalize(path).is_ok_and(|real| private_view(&real))
}

/// Resolve and classify a user-supplied path against a [`SessionScope`]
/// for Phase 2-C file tools. Relative paths anchor at
/// `scope.workspace()`; absolute paths are accepted but must classify
/// inside one of the scope's zones.
///
/// **Symlink containment.** [`SessionScope::classify_lexical_path`] is
/// intentionally lexical-only — a candidate like
/// `<workspace>/symlink/out`, where `symlink` is a symbolic link
/// pointing outside the workspace, would classify as `InWorkspace`
/// even though the actual on-disk location is elsewhere. `O_NOFOLLOW`
/// (applied in [`read_no_follow`] / [`write_no_follow`]) only protects
/// the FINAL component, not symlinked ancestors. To close that gap
/// before classification we canonicalize both the candidate and each
/// zone root via [`canonicalize_lossy`], matching the containment
/// guarantee `octos_core::file_handle::resolve_tool_path` gave the
/// pre-Phase-2C path. See PR #1201 codex review for the precise
/// scenario (`<workspace>/link/out`).
///
/// Returns the (lexically-normalised, NOT canonicalized) absolute path
/// the file tool should open. We deliberately return the lexical form
/// so callers can pass it back to `read_no_follow`/`write_no_follow`
/// without re-resolving — the canonicalization here is for
/// classification only.
pub fn resolve_path_for_session_scope_read(
    scope: &SessionScope,
    user_path: &str,
) -> Result<PathBuf, &'static str> {
    resolve_for_scope(scope, user_path, /*for_write=*/ false)
}

/// Same as [`resolve_path_for_session_scope_read`] but with the
/// write-side policy: `InSharedZone` is refused.
pub fn resolve_path_for_session_scope_write(
    scope: &SessionScope,
    user_path: &str,
) -> Result<PathBuf, &'static str> {
    resolve_for_scope(scope, user_path, /*for_write=*/ true)
}

fn resolve_for_scope(
    scope: &SessionScope,
    user_path: &str,
    for_write: bool,
) -> Result<PathBuf, &'static str> {
    if is_process_secret_path(Path::new(user_path))
        || is_process_secret_path(&normalize_lexical(Path::new(user_path)))
    {
        return Err("process environments and command lines are off limits");
    }
    // Upload handles (`up/<base64>/<name>`) are opaque references to a file in
    // the authenticated upload tmpdir — NOT workspace-relative paths. Without
    // this short-circuit the join+classify logic below treats them as
    // `<workspace>/up/<base64>/<name>` (lexically InWorkspace) and the file is
    // never found, so every scoped (SPA web-/slides-/site-) session is unable
    // to read ANY uploaded file. Decode via the unified resolver (which
    // canonicalizes under the upload root, firmlink-safe), matching the legacy
    // `resolve_path`.
    //
    // SECURITY: only trust this short-circuit when the resolver actually
    // landed in the upload tmpdir (`ToolPathScope::UploadTmpdir`). A path that
    // merely *starts with* `up/` but is not a valid handle (e.g.
    // `up/link/secret.txt`) decode-fails and `resolve_tool_path` falls back to
    // `ToolPathScope::Workspace`, returning the LEXICAL `<workspace>/up/...`
    // form. Returning that here would skip the canonical ancestor-symlink guard
    // in `classify_canonical_path` below — and `read_no_follow` only protects
    // the leaf component, so a symlinked `workspace/up` would reopen a scoped
    // read escape. Such paths therefore fall through to the normal resolver.
    // Uploads are read sources, so writes to a resolved upload handle are
    // refused. The `pf/` profile handle needs a profile root that
    // `SessionScope` does not currently expose; tracked as a follow-up
    // (issue #1367).
    if user_path.starts_with("up/") {
        // #1377 tenant isolation: in a multi-tenant (scoped) session, uploads
        // are materialized into `<workspace>/uploads/` at turn start and read
        // by that workspace path. Refuse global `up/` handle resolution here so
        // a scoped session cannot reach the process-global upload tmpdir (and
        // thus another tenant's uploads) by handle. Solo sessions (CLI
        // `octos chat`) keep resolving handles for back-compat.
        if scope.tenant_id().is_some()
            && matches!(
                octos_core::file_handle::decode_file_handle(user_path),
                Some(octos_core::file_handle::FileHandleScope::TempUpload(_))
            )
        {
            return Err(
                "Uploaded file not found; uploaded files are under uploads/ — \
                 read with read_file(\"uploads/<name>\")",
            );
        }
        match octos_core::file_handle::resolve_tool_path(scope.workspace(), None, user_path) {
            Ok(resolved)
                if resolved.scope == octos_core::file_handle::ToolPathScope::UploadTmpdir =>
            {
                return if for_write {
                    Err("Writes to uploaded files are not permitted")
                } else {
                    Ok(resolved.absolute)
                };
            }
            _ => {
                // If the value DECODES as an upload handle it is unambiguously an
                // upload reference: never fall through to the workspace resolver
                // (which would let `write_file` create `<workspace>/up/<payload>/
                // <name>`, or a read silently hit an unrelated workspace file with
                // the same literal path). The temp file may have been deleted or
                // no longer canonicalises under the upload root — report it as a
                // missing upload rather than masking it as a workspace path.
                if matches!(
                    octos_core::file_handle::decode_file_handle(user_path),
                    Some(octos_core::file_handle::FileHandleScope::TempUpload(_))
                ) {
                    return Err("Uploaded file not found");
                }
                // A value that merely starts with `up/` but does NOT decode as a
                // handle (e.g. a real workspace path) falls through to the scoped
                // resolver, where the canonical containment guard still applies.
            }
        }
    }
    let candidate = PathBuf::from(user_path);
    let absolute = if candidate.is_absolute() {
        candidate
    } else {
        scope.workspace().join(candidate)
    };
    // Refuse `..` lexically first so the subsequent canonicalize walk
    // cannot accidentally surface inside a zone after climbing out.
    let lex_normalised = match lexical_normalise_strict(&absolute) {
        Some(p) => p,
        None => return Err("Path outside session scope"),
    };
    // PR-A round-2 (codex BLOCKER 1 follow-up): delegate the canonical
    // containment guard to `SessionScope::classify_canonical_path` so
    // every consumer (file tools here, plugin tools in
    // `plugins/tool.rs`) shares one implementation. The helper lives in
    // `octos-core` next to `classify_lexical_path` because it has no
    // dependency on tool-side state.
    match scope.classify_canonical_path(&lex_normalised) {
        PathClassification::InWorkspace => Ok(lex_normalised),
        PathClassification::InGrantedDir { .. } => Ok(lex_normalised),
        PathClassification::InSharedZone { .. } => {
            if for_write {
                Err("Writes to shared zones are not permitted")
            } else {
                Ok(lex_normalised)
            }
        }
        // PR-A: read-only plugin skill dirs. Mirror the
        // `InSharedZone` policy — reads pass through, writes refuse.
        // The SKILL.md auto-inject teaches the agent to read files
        // under the plugin's install directory but never write back.
        PathClassification::InSkillDir { .. } => {
            if for_write {
                Err("Writes to plugin skill directories are not permitted")
            } else {
                Ok(lex_normalised)
            }
        }
        // UPCR-2026-034 `read_parent`: a request context reads its peer's
        // folder; it never writes there.
        PathClassification::InReadOnlyView { .. } => {
            if for_write {
                Err("Writes outside this context's own folder are not permitted")
            } else {
                Ok(lex_normalised)
            }
        }
        PathClassification::OutOfScope => Err("Path outside session scope"),
    }
}

/// Interim mitigation (#1378, superseded by #1377): `up/<base64>/<name>` is an
/// opaque UPLOAD HANDLE, not a browsable directory — and uploaded files live in
/// a tmpdir *outside* the session workspace, so `list_dir`/`glob` can never
/// enumerate the `up/` namespace. They returned a bare "Directory not found",
/// which was observed to make models (deepseek-chat in prod, and gemini-2.5-pro
/// on replay) wrongly conclude the upload is missing and ask the user to
/// re-upload — even after a successful `read_file` of the same handle.
///
/// Returns guidance to redirect the model when a directory-listing tool is
/// pointed at the upload-handle namespace (`up` or `up/...`). Matches the EXACT
/// spelling `read_file`/`resolve_for_scope` accept as a handle (`up/` prefix);
/// it deliberately does NOT normalise `./up/...` or trim whitespace, because the
/// read path doesn't either — guiding the model to `read_file("./up/...")` would
/// just fail there too (codex #1378 round 6). This is a band-aid; the real fix
/// (#1377) materialises uploads into `<workspace>/uploads/`.
pub(crate) fn upload_handle_namespace_guidance(path: &str) -> Option<&'static str> {
    if path == "up" || path.starts_with("up/") {
        Some(
            "`up/…` is an opaque upload handle, not a directory — uploaded files \
             are NOT browsable via list_dir or glob. Read the file directly with \
             read_file using the exact `up/…` handle from the attachment (you may \
             already have its contents from a prior read).",
        )
    } else {
        None
    }
}

/// Whether a `list_dir`/`glob` failure for `path` should be replaced with
/// upload guidance, given the session's `workspace_root`. Centralises the gate
/// so the two tools cannot diverge (codex #1378): redirect iff `path` targets
/// the `up/` namespace AND it either decodes as a real upload handle OR there is
/// no real browsable `up/` directory in the workspace. Uses the path verbatim
/// (no `./`/whitespace normalisation) so the redirect decision matches exactly
/// what `read_file` would accept as a handle.
pub(crate) fn upload_namespace_redirect(path: &str, workspace_root: &Path) -> Option<&'static str> {
    let guidance = upload_handle_namespace_guidance(path)?;
    let decodes_as_upload = matches!(
        octos_core::file_handle::decode_file_handle(path),
        Some(octos_core::file_handle::FileHandleScope::TempUpload(_))
    );
    if decodes_as_upload || !workspace_root.join("up").is_dir() {
        Some(guidance)
    } else {
        None
    }
}

/// Lexical normalise (collapse `.`, refuse `..`). Mirrors the helper
/// inside `octos_core::session_scope` so the canonicalize walk above
/// can't absorb a traversal escape.
pub(crate) fn lexical_normalise_strict(path: &Path) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => out.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => return None,
            Component::Normal(part) => out.push(part),
        }
    }
    Some(out)
}

/// Syntactic path normalization (no filesystem access). Collapses `.`
/// components and resolves `..` against in-memory parents without
/// canonicalising symlinks. Used by the Host-scope branch above where
/// the workspace fence is intentionally absent.
fn normalize_lexical(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(p) => normalized.push(p.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    normalized.push("..");
                }
            }
            Component::Normal(name) => normalized.push(name),
        }
    }
    normalized
}

/// Check that a path is not a symlink. Returns error message if it is.
///
/// Call AFTER `resolve_path` and before any filesystem read/write.
/// Prevents symlink-based escapes where a link inside base_dir points outside.
///
/// NOTE: For file read/write operations, prefer `read_no_follow` / `write_no_follow`
/// which atomically reject symlinks via O_NOFOLLOW (no TOCTOU race).
/// This function is still useful for directory operations (e.g. list_dir).
pub async fn reject_symlink(path: &Path) -> Option<ToolResult> {
    match tokio::fs::symlink_metadata(path).await {
        Ok(meta) if meta.is_symlink() => Some(ToolResult {
            output: "Symlinks are not allowed".to_string(),
            success: false,
            ..Default::default()
        }),
        _ => None,
    }
}

/// Check if an I/O error indicates a symlink was rejected (ELOOP from O_NOFOLLOW).
pub fn is_symlink_error(e: &std::io::Error) -> bool {
    #[cfg(unix)]
    {
        e.raw_os_error() == Some(libc::ELOOP)
    }
    #[cfg(not(unix))]
    {
        // Non-Unix fallback: detect our synthetic error from read/write_no_follow
        e.kind() == std::io::ErrorKind::PermissionDenied
    }
}

/// Read file contents, atomically rejecting symlinks via O_NOFOLLOW on Unix.
///
/// Eliminates the TOCTOU race between `reject_symlink` and `tokio::fs::read_to_string`.
pub async fn read_no_follow(path: &Path) -> std::io::Result<String> {
    let path = path.to_owned();
    tokio::task::spawn_blocking(move || {
        use std::io::{Read, Seek, SeekFrom};
        let mut opts = std::fs::OpenOptions::new();
        opts.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.custom_flags(libc::O_NOFOLLOW);
        }
        #[cfg(not(unix))]
        {
            if path.symlink_metadata().is_ok_and(|m| m.is_symlink()) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "symlink rejected",
                ));
            }
        }
        let mut file = opts.open(&path)?;

        // Peek the first 5 bytes to detect a PDF (`%PDF-`). PDF content is
        // binary so `read_to_string` would fail with a UTF-8 error — for
        // those we route through `pdf-extract` to recover plain text. Pinned
        // by the mini5 invoice upload regression (2026-05-12).
        //
        // Both branches read the REST from the SAME O_NOFOLLOW file
        // descriptor (seek back to 0), never by re-opening the path. The
        // previous PDF branch did `std::fs::read(&path)`, which follows
        // symlinks and does no symlink re-check — a leaf swapped between the
        // O_NOFOLLOW open (time-of-check) and that read (time-of-use) was
        // followed, so an attacker could redirect the read to `/etc/passwd`
        // or any file and feed its contents to the LLM. Reading from the
        // held fd binds every byte to the inode we already validated.
        let mut magic = [0u8; 5];
        let n = file.read(&mut magic)?;
        file.seek(SeekFrom::Start(0))?;
        if n >= 5 && &magic == b"%PDF-" {
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes)?;
            match pdf_extract::extract_text_from_mem(&bytes) {
                Ok(text) => Ok(text),
                Err(err) => Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("pdf extraction failed: {err}"),
                )),
            }
        } else {
            let mut content = String::with_capacity(n);
            file.read_to_string(&mut content)?;
            Ok(content)
        }
    })
    .await
    .unwrap_or_else(|e| Err(std::io::Error::other(e)))
}

/// Open a file read-only, atomically rejecting symlinks (O_NOFOLLOW on Unix).
///
/// SECURITY: the flags here MUST match [`read_no_follow`]'s open exactly.
#[cfg(unix)]
fn open_no_follow_ro(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
}

#[cfg(not(unix))]
fn open_no_follow_ro(path: &Path) -> std::io::Result<std::fs::File> {
    if path.symlink_metadata().is_ok_and(|m| m.is_symlink()) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "symlink rejected",
        ));
    }
    std::fs::OpenOptions::new().read(true).open(path)
}

/// The descriptor-derived facts an armed windowed read needs beyond the bytes.
pub(crate) struct ReadMeta {
    /// [`crate::tools::read_window::ViewEpoch`] taken from the SAME descriptor
    /// the bytes came from — not a separate path stat — so it describes the
    /// exact inode whose bytes were shown (#2193 R4, read-side TOCTOU).
    /// `None` only if the descriptor's metadata was unavailable, which never
    /// authorizes a write.
    pub epoch: Option<crate::tools::read_window::ViewEpoch>,
    /// The returned content is a DECODE of the on-disk bytes (PDF text
    /// extraction), not the bytes themselves — so a whole-file rewrite
    /// reconstructed from it can never be faithful, and the view must never be
    /// allowed to reach COMPLETE (#2193 R4, PDF false-COMPLETE).
    pub transformed: bool,
}

/// Like [`read_no_follow`] but also returns the [`ReadMeta`] the armed
/// windowed-read ledger needs. Kept as a distinct entry point so the many
/// non-armed `read_no_follow` callers pay nothing for the extra `fstat`.
pub(crate) async fn read_no_follow_with_meta(path: &Path) -> std::io::Result<(String, ReadMeta)> {
    read_with_meta(path, None).await
}

/// Open every component relative to the already bound workspace. A sibling
/// shell may replace directories concurrently, so checking a canonical path
/// before a leaf-only O_NOFOLLOW open is insufficient.
pub(crate) async fn read_confined_with_meta(
    root: &Path,
    path: &Path,
) -> std::io::Result<(String, ReadMeta)> {
    read_with_meta(path, Some(root)).await
}

async fn read_with_meta(path: &Path, root: Option<&Path>) -> std::io::Result<(String, ReadMeta)> {
    let path = path.to_owned();
    let root = root.map(Path::to_owned);
    tokio::task::spawn_blocking(move || {
        use std::io::{Read, Seek, SeekFrom};
        let mut file = if let Some(root) = &root {
            let relative = path.strip_prefix(root).map_err(|_| {
                std::io::Error::new(std::io::ErrorKind::PermissionDenied, "outside workspace")
            })?;
            write_grant::open_confined(root, relative, write_grant::ConfinedLeaf::OpenExistingRead)?
        } else {
            open_no_follow_ro(&path)?
        };
        if root.is_some() && file.metadata()?.len() > 10_000_000 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "file exceeds read limit",
            ));
        }
        // fstat the DESCRIPTOR (not a re-resolution of the path): binds the
        // epoch to the exact inode these bytes are read from.
        let epoch = file
            .metadata()
            .ok()
            .and_then(|m| crate::tools::read_window::ViewEpoch::from_metadata(&m));
        // Same PDF handling as read_no_follow, reading the rest from the SAME
        // O_NOFOLLOW descriptor (seek back to 0), never by re-opening the path.
        let mut magic = [0u8; 5];
        let n = file.read(&mut magic)?;
        file.seek(SeekFrom::Start(0))?;
        if n >= 5 && &magic == b"%PDF-" {
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes)?;
            match pdf_extract::extract_text_from_mem(&bytes) {
                Ok(text) => Ok((
                    text,
                    ReadMeta {
                        epoch,
                        transformed: true,
                    },
                )),
                Err(err) => Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("pdf extraction failed: {err}"),
                )),
            }
        } else {
            let mut content = String::with_capacity(n);
            file.read_to_string(&mut content)?;
            Ok((
                content,
                ReadMeta {
                    epoch,
                    transformed: false,
                },
            ))
        }
    })
    .await
    .unwrap_or_else(|e| Err(std::io::Error::other(e)))
}

/// Write content to a file, atomically rejecting symlinks via O_NOFOLLOW on Unix.
///
/// Eliminates the TOCTOU race between `reject_symlink` and `tokio::fs::write`.
pub async fn write_no_follow(path: &Path, content: &[u8]) -> std::io::Result<()> {
    if let Err(reason) = refuse_git_internal_path(path) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            reason,
        ));
    }
    let path = path.to_owned();
    let content = content.to_owned();
    tokio::task::spawn_blocking(move || {
        use std::io::Write;
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.custom_flags(libc::O_NOFOLLOW);
        }
        #[cfg(not(unix))]
        {
            if path.symlink_metadata().is_ok_and(|m| m.is_symlink()) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "symlink rejected",
                ));
            }
        }
        let mut file = opts.open(&path)?;
        file.write_all(&content)?;
        Ok(())
    })
    .await
    .unwrap_or_else(|e| Err(std::io::Error::other(e)))
}

/// Outcome of an epoch-bound overwrite ([`write_no_follow_checked`]).
#[derive(Debug)]
pub(crate) enum CheckedWrite {
    /// The opened descriptor matched the authorizing epoch and was truncated
    /// and rewritten.
    Written,
    /// The opened descriptor's epoch (mtime/size/ctime/inode) no longer
    /// matched what authorized the write — nothing was written.
    EpochChanged {
        /// The descriptor's actual epoch at open time.
        found: crate::tools::read_window::ViewEpoch,
    },
}

/// Overwrite an EXISTING file, but only if the descriptor we open still
/// matches `expected` (#1638 R1 — TOCTOU).
///
/// The armed write guard authorizes an overwrite against a *stat of the path*.
/// A plain `write_no_follow` then opens the path AGAIN and truncates whatever
/// it resolves to — so an external replacement in that narrow authorize→open
/// window is silently clobbered. This binds the authorization to the exact
/// opened inode: it opens WITHOUT `O_TRUNC`, `fstat`s the descriptor, and only
/// truncates + writes when that descriptor's `(mtime, size)` still equals the
/// epoch that authorized the write. `O_NOFOLLOW` still rejects a symlink swap;
/// operating on the validated descriptor (not a re-resolved path) closes the
/// race even against a same-name inode swap after validation.
pub(crate) async fn write_no_follow_checked(
    path: &Path,
    content: &[u8],
    expected: crate::tools::read_window::ViewEpoch,
) -> std::io::Result<CheckedWrite> {
    if let Err(reason) = refuse_git_internal_path(path) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            reason,
        ));
    }
    let path = path.to_owned();
    let content = content.to_owned();
    tokio::task::spawn_blocking(move || {
        use std::io::Write;
        let mut opts = std::fs::OpenOptions::new();
        // No create, no truncate — we must inspect the descriptor before
        // destroying its contents.
        opts.write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.custom_flags(libc::O_NOFOLLOW);
        }
        #[cfg(not(unix))]
        {
            if path.symlink_metadata().is_ok_and(|m| m.is_symlink()) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "symlink rejected",
                ));
            }
        }
        let mut file = opts.open(&path)?;
        // fstat the DESCRIPTOR itself (not a re-resolution of the path). The
        // epoch binds mtime/size/ctime/inode, so a same-size replacement that
        // forged mtime is caught by the ctime (or inode) mismatch.
        let meta = file.metadata()?;
        let found = crate::tools::read_window::ViewEpoch::from_metadata(&meta)
            .ok_or_else(|| std::io::Error::other("descriptor metadata unavailable"))?;
        if found != expected {
            return Ok(CheckedWrite::EpochChanged { found });
        }
        // The validated descriptor is the one we truncate and rewrite.
        file.set_len(0)?;
        file.write_all(&content)?;
        Ok(CheckedWrite::Written)
    })
    .await
    .unwrap_or_else(|e| Err(std::io::Error::other(e)))
}

// #1976 note: `create_only` (`O_CREAT|O_EXCL`) enforcement moved into the
// component-wise confined `openat` walk in `tools::write_grant::open_confined`
// (which also closes the ancestor-symlink TOCTOU a whole-path lexical open
// cannot). A standalone leaf-only exclusive writer would re-introduce that
// divergence, so it is intentionally absent here.

/// Convert a file I/O error to a ToolResult, handling symlink and not-found cases.
pub fn file_io_error(e: std::io::Error, display_path: &str) -> ToolResult {
    if is_symlink_error(&e) {
        ToolResult {
            output: "Symlinks are not allowed".to_string(),
            success: false,
            ..Default::default()
        }
    } else if e.kind() == std::io::ErrorKind::NotFound {
        ToolResult {
            output: format!("File not found: {display_path}"),
            success: false,
            ..Default::default()
        }
    } else {
        ToolResult {
            output: format!("Failed to access {display_path}: {e}"),
            success: false,
            ..Default::default()
        }
    }
}
