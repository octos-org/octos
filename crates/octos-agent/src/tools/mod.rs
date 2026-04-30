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

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use eyre::Result;
use octos_core::TokenUsage;

use crate::progress::ProgressReporter;

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
#[derive(Clone, Debug)]
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
    /// cache is wrapped in `Arc` so it can be cloned cheaply into subagents;
    /// use [`FileStateCache::clone_for_subagent`] when a delegate should
    /// receive an independent copy instead of a shared handle.
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
}

impl ToolContext {
    /// Zero-value context suitable for unit tests and tools that do not need
    /// live executor wiring. Uses a [`crate::progress::SilentReporter`] and
    /// leaves every M8.x placeholder at its default.
    pub fn zero() -> Self {
        Self {
            tool_id: String::new(),
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
            task_supervisor: None,
            cost_accountant: None,
            parent_session_key: None,
            spawn_depth: 0,
        }
    }
}

tokio::task_local! {
    /// Task-local tool context, scoped per tool invocation in agent.rs.
    pub static TOOL_CTX: ToolContext;
}

/// Request emitted by a tool when runtime policy requires user approval.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolApprovalRequest {
    pub tool_id: String,
    pub tool_name: String,
    pub title: String,
    pub body: String,
    pub command: Option<String>,
    pub cwd: Option<String>,
}

/// Decision returned to a blocked tool after client approval handling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolApprovalDecision {
    Approve,
    Deny,
}

/// Async approval bridge provided by clients that support interactive approval.
#[async_trait]
pub trait ToolApprovalRequester: Send + Sync {
    async fn request_approval(&self, request: ToolApprovalRequest) -> ToolApprovalDecision;
}

tokio::task_local! {
    /// Optional task-local approval bridge scoped around a turn by interactive clients.
    pub static TOOL_APPROVAL_CTX: Arc<dyn ToolApprovalRequester>;
}

#[derive(Clone, Debug, Default)]
pub struct TurnAttachmentContext {
    pub attachment_paths: Vec<String>,
    pub audio_attachment_paths: Vec<String>,
    pub file_attachment_paths: Vec<String>,
    pub prompt_summary: Option<String>,
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
///   dispatches in parallel (today's behaviour).
/// - If *any* call is [`ConcurrencyClass::Exclusive`], the entire batch runs
///   serially in call order. A single error from an exclusive call cancels
///   the remaining peers so the LLM sees the cascade instead of continuing
///   to mutate state on a doomed path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
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

    /// Downcast support for concrete tool access (e.g. wiring ActivateToolsTool).
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
}

/// LRU-based tool lifecycle manager.
///
/// Tracks per-tool usage and auto-evicts idle tools when the active count
/// exceeds a threshold. Base tools are pinned and never evicted.
pub struct ToolLifecycle {
    /// Per-tool last-used iteration counter.
    pub(crate) last_used: HashMap<String, u32>,
    /// Current iteration counter.
    pub(crate) iteration: u32,
    /// Tools that are never auto-evicted.
    pub(crate) base_tools: HashSet<String>,
    /// Maximum active tools before eviction kicks in.
    pub(crate) max_active: usize,
    /// Tools idle for this many iterations become eviction candidates.
    pub(crate) idle_threshold: u32,
}

impl Default for ToolLifecycle {
    fn default() -> Self {
        Self {
            last_used: HashMap::new(),
            iteration: 0,
            base_tools: HashSet::new(),
            max_active: 15,
            idle_threshold: 5,
        }
    }
}

impl ToolLifecycle {
    /// Set base tools that are never auto-evicted.
    pub fn set_base_tools(&mut self, names: impl IntoIterator<Item = impl Into<String>>) {
        self.base_tools = names.into_iter().map(|n| n.into()).collect();
    }

    /// Add more tools to the base set (extends, does not replace).
    pub fn add_base_tools(&mut self, names: impl IntoIterator<Item = impl Into<String>>) {
        self.base_tools.extend(names.into_iter().map(|n| n.into()));
    }

    /// Record that a tool was used at the current iteration.
    pub fn record_usage(&mut self, name: &str) {
        self.last_used.insert(name.to_string(), self.iteration);
    }

    /// Advance the iteration counter.
    pub fn tick(&mut self) {
        self.iteration += 1;
    }

    /// Find idle non-base tools to evict from `active_tools`, sorted by
    /// staleness (oldest first). Callers should have already excluded
    /// deferred tools from `active_tools`.
    pub fn find_evictable(&self, active_tools: &[&str]) -> Vec<String> {
        let active_count = active_tools.len();
        if active_count <= self.max_active {
            return Vec::new();
        }

        let mut candidates: Vec<(&str, u32)> = active_tools
            .iter()
            .filter(|name| !self.base_tools.contains(**name))
            .map(|name| {
                let last = self.last_used.get(*name).copied().unwrap_or(0);
                (*name, last)
            })
            .filter(|(_, last)| self.iteration.saturating_sub(*last) >= self.idle_threshold)
            .collect();

        candidates.sort_by_key(|(_, last)| *last);
        let to_evict = active_count.saturating_sub(self.max_active);
        candidates
            .into_iter()
            .take(to_evict)
            .map(|(name, _)| name.to_string())
            .collect()
    }
}

// Tool registry (extracted to its own module)
mod registry;
pub use registry::ToolRegistry;

// Tool policy
pub mod policy;
pub use policy::{PolicyDecision, ToolPolicy};

// Robot safety-tier groups consulted by ToolPolicy evaluation.
pub mod robot_groups;
pub use robot_groups::{RobotToolRegistry, install_registry as install_robot_registry};

// Shared SSRF protection
pub mod ssrf;

// Built-in tools
pub mod deep_search;
pub mod delegate;
pub mod diff_edit;
pub mod edit_file;
pub mod glob_tool;
pub mod grep_tool;
pub mod list_dir;
pub mod manage_skills;
pub mod mcp_agent;
pub mod message;
pub mod read_file;
pub mod recall_memory;
pub mod research_utils;
pub mod save_memory;
pub mod send_file;
pub mod shell;
#[allow(dead_code)]
pub(crate) mod site_crawl;
pub mod spawn;
pub mod synthesize_research;
pub mod web_fetch;
pub mod web_search;
pub mod write_file;

pub mod activate_tools;
pub mod admin;
pub mod browser;
pub mod check_background_tasks;
pub mod check_workspace_contract;
pub mod tool_config;
pub mod workspace_history;

#[cfg(feature = "git")]
pub mod git;

#[cfg(feature = "ast")]
pub mod code_structure;

pub use deep_search::DeepSearchTool;
pub use delegate::{
    DELEGATED_DENY_GROUP, DELEGATION_METRIC, DelegateTool, DelegationEvent, DelegationOutcome,
    DepthBudget, MAX_DEPTH, build_delegated_child_policy,
};
pub use diff_edit::DiffEditTool;
pub use edit_file::EditFileTool;
pub use glob_tool::GlobTool;
pub use grep_tool::GrepTool;
pub use list_dir::ListDirTool;
pub use manage_skills::ManageSkillsTool;
pub use mcp_agent::{
    DEFAULT_DISPATCH_TIMEOUT_SECS, DEFAULT_HTTP_CONNECT_TIMEOUT_SECS,
    DEFAULT_HTTP_READ_TIMEOUT_SECS, DispatchOutcome, DispatchRequest, DispatchResponse,
    HttpMcpAgent, McpAgentBackend, McpAgentBackendConfig, SharedBackend, StdioMcpAgent,
    build_backend_from_config, build_dispatch_event_payload, dispatch_with_metrics,
    record_dispatch,
};
pub use message::MessageTool;
pub use read_file::ReadFileTool;
pub use recall_memory::RecallMemoryTool;
pub use save_memory::SaveMemoryTool;
pub use send_file::SendFileTool;
pub use shell::ShellTool;
pub use spawn::{BackgroundResultKind, BackgroundResultPayload, SpawnTool};
pub use synthesize_research::SynthesizeResearchTool;
pub use web_fetch::WebFetchTool;
pub use web_search::WebSearchTool;
pub use write_file::WriteFileTool;

pub use activate_tools::ActivateToolsTool;
pub use browser::BrowserTool;
pub use check_background_tasks::CheckBackgroundTasksTool;
pub use check_workspace_contract::CheckWorkspaceContractTool;
pub use tool_config::{ConfigureToolTool, ToolConfigStore};
pub use workspace_history::{WorkspaceDiffTool, WorkspaceLogTool, WorkspaceShowTool};

#[cfg(feature = "git")]
pub use git::GitTool;

#[cfg(feature = "ast")]
pub use code_structure::CodeStructureTool;

use std::path::{Component, Path};

/// Resolve a user-provided path, ensuring it stays within base_dir.
///
/// Rejects absolute paths and prevents traversal via `../`.
/// Does NOT follow symlinks (normalize only, no filesystem access).
pub fn resolve_path(base_dir: &Path, user_path: &str) -> Result<PathBuf> {
    if PathBuf::from(user_path).is_absolute() {
        eyre::bail!("absolute paths are not allowed: {}", user_path);
    }

    let path = base_dir.join(user_path);
    let normalized = normalize_path(&path);
    let base_normalized = normalize_path(base_dir);

    if !normalized.starts_with(&base_normalized) {
        eyre::bail!("path outside working directory: {}", user_path);
    }

    Ok(normalized)
}

/// Normalize path by resolving `.` and `..` components without filesystem access.
fn normalize_path(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            // RootDir and Prefix reset the path (absolute path semantics)
            Component::RootDir | Component::Prefix(_) => {
                out.push(component.as_os_str());
            }
            Component::Normal(seg) => {
                out.push(seg);
            }
        }
    }
    out
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
        use std::io::Read;
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
        let mut content = String::new();
        file.read_to_string(&mut content)?;
        Ok(content)
    })
    .await
    .unwrap_or_else(|e| Err(std::io::Error::other(e)))
}

/// Write content to a file, atomically rejecting symlinks via O_NOFOLLOW on Unix.
///
/// Eliminates the TOCTOU race between `reject_symlink` and `tokio::fs::write`.
pub async fn write_no_follow(path: &Path, content: &[u8]) -> std::io::Result<()> {
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

#[cfg(test)]
mod nofollow_tests {
    use super::*;

    #[tokio::test]
    async fn test_read_no_follow_regular_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let file = dir.path().join("test.txt");
        std::fs::write(&file, "hello").unwrap();

        let content = read_no_follow(&file).await.unwrap();
        assert_eq!(content, "hello");
    }

    #[tokio::test]
    async fn test_read_no_follow_not_found() {
        let dir = tempfile::TempDir::new().unwrap();
        let file = dir.path().join("nonexistent.txt");

        let err = read_no_follow(&file).await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_read_no_follow_rejects_symlink() {
        let dir = tempfile::TempDir::new().unwrap();
        let target = dir.path().join("target.txt");
        std::fs::write(&target, "secret").unwrap();
        let link = dir.path().join("link.txt");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let err = read_no_follow(&link).await.unwrap_err();
        assert!(is_symlink_error(&err), "expected ELOOP, got: {err}");
    }

    #[tokio::test]
    async fn test_write_no_follow_regular_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let file = dir.path().join("out.txt");

        write_no_follow(&file, b"written").await.unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "written");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_write_no_follow_rejects_symlink() {
        let dir = tempfile::TempDir::new().unwrap();
        let target = dir.path().join("target.txt");
        std::fs::write(&target, "original").unwrap();
        let link = dir.path().join("link.txt");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let err = write_no_follow(&link, b"evil").await.unwrap_err();
        assert!(is_symlink_error(&err), "expected ELOOP, got: {err}");
        // Target must not be modified
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "original");
    }

    #[test]
    #[cfg(unix)]
    fn test_file_io_error_symlink() {
        let err = std::io::Error::from_raw_os_error(libc::ELOOP);
        let result = file_io_error(err, "test.txt");
        assert!(!result.success);
        assert!(result.output.contains("Symlinks"));
    }

    #[test]
    fn test_file_io_error_not_found() {
        let err = std::io::Error::new(std::io::ErrorKind::NotFound, "no such file");
        let result = file_io_error(err, "missing.txt");
        assert!(!result.success);
        assert!(result.output.contains("File not found"));
    }
}

#[cfg(test)]
mod path_tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn test_resolve_rejects_absolute_path() {
        let base = Path::new("/home/user/project");
        assert!(resolve_path(base, "/etc/passwd").is_err());
        assert!(resolve_path(base, "/home/user/project/../../../etc/shadow").is_err());
    }

    #[test]
    fn test_resolve_blocks_parent_traversal() {
        let base = Path::new("/home/user/project");
        assert!(resolve_path(base, "../../../etc/passwd").is_err());
        assert!(resolve_path(base, "subdir/../../..").is_err());
        assert!(resolve_path(base, "foo/../../../secret").is_err());
    }

    #[test]
    fn test_resolve_allows_valid_relative() {
        let base = Path::new("/home/user/project");
        let p = resolve_path(base, "src/main.rs").unwrap();
        assert_eq!(p, PathBuf::from("/home/user/project/src/main.rs"));
    }

    #[test]
    fn test_resolve_allows_dot_segments_within_base() {
        let base = Path::new("/home/user/project");
        let p = resolve_path(base, "src/../src/lib.rs").unwrap();
        assert_eq!(p, PathBuf::from("/home/user/project/src/lib.rs"));
    }

    #[test]
    fn test_resolve_allows_current_dir() {
        let base = Path::new("/home/user/project");
        let p = resolve_path(base, "./README.md").unwrap();
        assert_eq!(p, PathBuf::from("/home/user/project/README.md"));
    }

    #[test]
    fn test_resolve_allows_deeply_nested() {
        let base = Path::new("/home/user/project");
        let p = resolve_path(base, "a/b/c/d/e/f.rs").unwrap();
        assert_eq!(p, PathBuf::from("/home/user/project/a/b/c/d/e/f.rs"));
    }

    #[test]
    fn test_normalize_handles_complex_paths() {
        assert_eq!(
            normalize_path(Path::new("/a/b/../c/./d")),
            PathBuf::from("/a/c/d")
        );
        assert_eq!(
            normalize_path(Path::new("/a/b/../../c")),
            PathBuf::from("/c")
        );
    }

    /// Per-profile CWD isolation: when cwd is narrowed to a profile's data_dir,
    /// resolve_path must block access to other profiles' directories.
    #[test]
    fn test_resolve_blocks_cross_profile_access() {
        let base = Path::new("/home/user/.octos/profiles/alice/data");

        assert!(resolve_path(base, "../../bob/data/sessions/secret").is_err());
        assert!(resolve_path(base, "../../../profiles/bob/data/episodes.db").is_err());
        assert!(resolve_path(base, "../../../skills/evil-skill/main").is_err());

        assert!(resolve_path(base, "skills/my-skill/main").is_ok());
        assert!(resolve_path(base, "sessions/chat-123.json").is_ok());
        assert!(resolve_path(base, "skill-output/report.pdf").is_ok());
    }

    #[test]
    fn test_resolve_rejects_empty_path() {
        let base = Path::new("/home/user/project");
        let result = resolve_path(base, "");
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), PathBuf::from("/home/user/project"));
    }

    #[test]
    fn test_resolve_rejects_null_byte() {
        let base = Path::new("/home/user/project");
        let result = resolve_path(base, "file\0.txt");
        if let Ok(p) = &result {
            assert!(p.starts_with(base));
        }
    }

    #[test]
    fn test_resolve_rejects_windows_separators() {
        let base = Path::new("/home/user/project");
        let result = resolve_path(base, "..\\..\\etc\\passwd");
        if let Ok(p) = &result {
            assert!(p.starts_with(base));
        }
    }
}

#[cfg(test)]
mod tool_context_tests {
    //! M8.1 tests — typed `ToolContext` + `execute_with_context` scaffolding.

    use super::*;
    use async_trait::async_trait;
    use serde_json::Value;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Tool whose legacy `execute` records how many times it was called.
    /// Overrides *only* `execute`; the default `execute_with_context` impl
    /// must delegate here.
    struct LegacyTool {
        execute_calls: AtomicUsize,
    }

    impl LegacyTool {
        fn new() -> Self {
            Self {
                execute_calls: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait]
    impl Tool for LegacyTool {
        fn name(&self) -> &str {
            "legacy"
        }
        fn description(&self) -> &str {
            "legacy"
        }
        fn input_schema(&self) -> Value {
            serde_json::json!({})
        }
        async fn execute(&self, _args: &Value) -> Result<ToolResult> {
            self.execute_calls.fetch_add(1, Ordering::SeqCst);
            Ok(ToolResult {
                output: "legacy output".to_string(),
                success: true,
                ..Default::default()
            })
        }
    }

    /// Tool that consumes the typed `ToolContext` — overrides
    /// `execute_with_context` and re-enters via zero-value context from
    /// `execute`.
    struct ContextAwareTool {
        with_ctx_calls: AtomicUsize,
    }

    impl ContextAwareTool {
        fn new() -> Self {
            Self {
                with_ctx_calls: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait]
    impl Tool for ContextAwareTool {
        fn name(&self) -> &str {
            "ctx_aware"
        }
        fn description(&self) -> &str {
            "ctx"
        }
        fn input_schema(&self) -> Value {
            serde_json::json!({})
        }
        async fn execute(&self, args: &Value) -> Result<ToolResult> {
            // Re-enter the typed path with the zero context so callers that
            // still use the legacy entry point see identical behaviour.
            self.execute_with_context(&ToolContext::zero(), args).await
        }
        async fn execute_with_context(
            &self,
            ctx: &ToolContext,
            _args: &Value,
        ) -> Result<ToolResult> {
            self.with_ctx_calls.fetch_add(1, Ordering::SeqCst);
            Ok(ToolResult {
                output: format!(
                    "tool_id={};allow_all={};defs_empty={}",
                    ctx.tool_id,
                    ctx.permissions.is_tool_allowed("anything"),
                    ctx.agent_definitions.is_empty(),
                ),
                success: true,
                ..Default::default()
            })
        }
    }

    #[test]
    fn should_construct_zero_value_tool_context() {
        let ctx = ToolContext::zero();
        assert!(ctx.tool_id.is_empty());
        assert!(ctx.harness_event_sink.is_none());
        assert!(ctx.attachment_paths.is_empty());
        assert!(ctx.audio_attachment_paths.is_empty());
        assert!(ctx.file_attachment_paths.is_empty());
        // M8.x placeholders — zero-value but constructible without panic.
        assert!(ctx.agent_definitions.is_empty());
        assert!(ctx.permissions.is_tool_allowed("any_tool"));
        assert!(ctx.file_state_cache.is_none());
        assert!(ctx.notifications.is_empty());
        // AppStateHandle has no introspection beyond Default; just ensure
        // it cloned cheaply.
        let _cloned = ctx.app_state.clone();
    }

    #[tokio::test]
    async fn should_delegate_execute_to_execute_with_context() {
        // Legacy tool: override only `execute`. The default impl of
        // `execute_with_context` must route to it.
        let tool = LegacyTool::new();
        let ctx = ToolContext::zero();
        let result = tool
            .execute_with_context(&ctx, &serde_json::json!({}))
            .await
            .expect("legacy tool must succeed via default delegation");
        assert!(result.success);
        assert_eq!(result.output, "legacy output");
        assert_eq!(tool.execute_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn should_invoke_execute_with_context_for_migrated_tool() {
        let tool = ContextAwareTool::new();
        let mut ctx = ToolContext::zero();
        ctx.tool_id = "call-42".to_string();
        let result = tool
            .execute_with_context(&ctx, &serde_json::json!({}))
            .await
            .expect("ctx-aware tool must succeed");
        assert!(result.success);
        assert!(result.output.contains("tool_id=call-42"));
        assert!(result.output.contains("allow_all=true"));
        assert!(result.output.contains("defs_empty=true"));
        assert_eq!(tool.with_ctx_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn should_route_migrated_tool_execute_back_through_context_path() {
        // When a migrated tool is called via the legacy `execute` entry
        // point, it must still take its ctx-aware branch (invoked with
        // the zero-value context so out-of-band callers keep working).
        let tool = ContextAwareTool::new();
        let result = tool
            .execute(&serde_json::json!({}))
            .await
            .expect("migrated tool's legacy execute must succeed");
        assert!(result.success);
        // tool_id is empty because ToolContext::zero() carries no id.
        assert!(result.output.starts_with("tool_id=;"));
        assert_eq!(tool.with_ctx_calls.load(Ordering::SeqCst), 1);
    }

    // ---------- M8.8 concurrency-class tests ----------

    struct ExclusiveStubTool;

    #[async_trait]
    impl Tool for ExclusiveStubTool {
        fn name(&self) -> &str {
            "exclusive_stub"
        }
        fn description(&self) -> &str {
            "stub"
        }
        fn input_schema(&self) -> Value {
            serde_json::json!({})
        }
        async fn execute(&self, _args: &Value) -> Result<ToolResult> {
            Ok(ToolResult::default())
        }
        fn concurrency_class(&self) -> ConcurrencyClass {
            ConcurrencyClass::Exclusive
        }
    }

    #[test]
    fn default_concurrency_class_is_safe() {
        // A tool that does not override the default must report Safe so that
        // unmigrated tools keep pre-M8.8 parallel-friendly behaviour.
        let tool = LegacyTool::new();
        assert_eq!(tool.concurrency_class(), ConcurrencyClass::Safe);
        let ctx_tool = ContextAwareTool::new();
        assert_eq!(ctx_tool.concurrency_class(), ConcurrencyClass::Safe);
    }

    #[test]
    fn override_returns_exclusive() {
        // A tool that opts into Exclusive must be reported as Exclusive.
        let tool = ExclusiveStubTool;
        assert_eq!(tool.concurrency_class(), ConcurrencyClass::Exclusive);
    }

    #[test]
    fn concurrency_class_is_copy_eq_default() {
        // The enum exposes Copy + Eq + Default as contracted by the M8.8 spec.
        let a: ConcurrencyClass = ConcurrencyClass::default();
        let b = a; // Copy
        assert_eq!(a, b);
        assert_eq!(ConcurrencyClass::default(), ConcurrencyClass::Safe);
    }

    // ---------- M8 fix-first item 8 (gap 4b) — ToolPermissions::from_profile ----------

    use crate::profile::{PROFILE_SCHEMA_VERSION, PermissionMode, ProfileDefinition, ProfileTools};

    fn make_profile(name: &str, tools: ProfileTools) -> ProfileDefinition {
        ProfileDefinition {
            name: name.to_string(),
            version: PROFILE_SCHEMA_VERSION,
            tools,
            ..Default::default()
        }
    }

    #[test]
    fn should_allow_all_tools_when_profile_uses_default_filter() {
        // Default profile filter must remain pass-through so today's
        // `coding` profile path keeps allowing every registered tool.
        let profile = make_profile("default", ProfileTools::Default);
        let permissions = ToolPermissions::from_profile(&profile);
        assert!(permissions.is_tool_allowed("read_file"));
        assert!(permissions.is_tool_allowed("shell"));
        assert!(permissions.is_tool_allowed("anything_else"));
    }

    #[test]
    fn should_deny_listed_tools_when_profile_uses_deny_list() {
        // DenyList must block the named tools while leaving everything else
        // permitted. Plain tool names match exactly.
        let profile = make_profile(
            "no-shell",
            ProfileTools::DenyList {
                tools: vec!["shell".to_string()],
            },
        );
        let permissions = ToolPermissions::from_profile(&profile);
        assert!(
            !permissions.is_tool_allowed("shell"),
            "shell must be denied"
        );
        assert!(permissions.is_tool_allowed("read_file"));
    }

    #[test]
    fn should_only_allow_listed_tools_when_profile_uses_allow_list() {
        // AllowList must restrict to only the named tools (everything else
        // becomes implicitly denied). Tools outside the list lose.
        let profile = make_profile(
            "ro",
            ProfileTools::AllowList {
                tools: vec!["read_file".to_string()],
            },
        );
        let permissions = ToolPermissions::from_profile(&profile);
        assert!(permissions.is_tool_allowed("read_file"));
        assert!(
            !permissions.is_tool_allowed("shell"),
            "non-allow-listed tools must be denied"
        );
        assert!(!permissions.is_tool_allowed("write_file"));
    }

    #[test]
    fn should_expand_group_references_in_deny_list() {
        // `group:fs` references must expand to read_file / write_file /
        // edit_file / diff_edit per crate::tools::policy::TOOL_GROUPS so
        // the runtime gate matches the registry filter.
        let profile = make_profile(
            "no-fs",
            ProfileTools::DenyList {
                tools: vec!["group:fs".to_string()],
            },
        );
        let permissions = ToolPermissions::from_profile(&profile);
        assert!(!permissions.is_tool_allowed("read_file"));
        assert!(!permissions.is_tool_allowed("write_file"));
        assert!(!permissions.is_tool_allowed("edit_file"));
        assert!(!permissions.is_tool_allowed("diff_edit"));
        // Non-fs tools still permitted.
        assert!(permissions.is_tool_allowed("shell"));
    }

    #[test]
    fn should_expand_group_references_in_allow_list() {
        // `group:search` allows glob/grep/list_dir; everything else is
        // implicitly denied.
        let profile = make_profile(
            "search-only",
            ProfileTools::AllowList {
                tools: vec!["group:search".to_string()],
            },
        );
        let permissions = ToolPermissions::from_profile(&profile);
        assert!(permissions.is_tool_allowed("glob"));
        assert!(permissions.is_tool_allowed("grep"));
        assert!(permissions.is_tool_allowed("list_dir"));
        assert!(!permissions.is_tool_allowed("shell"));
        assert!(!permissions.is_tool_allowed("read_file"));
    }

    #[test]
    fn should_pass_through_when_allow_list_is_empty() {
        // Empty allow list mirrors the registry filter behaviour: an empty
        // allow list is a degenerate case that we treat as "no filter" (the
        // explicit deny list is the right tool to disable everything).
        let profile = make_profile("empty-allow", ProfileTools::AllowList { tools: Vec::new() });
        let permissions = ToolPermissions::from_profile(&profile);
        assert!(permissions.is_tool_allowed("anything"));
    }

    #[test]
    fn should_record_permission_mode_from_profile() {
        // The mode field is informational today; verify it survives the
        // from_profile boundary so future tier rules can read it.
        let profile = ProfileDefinition {
            name: "restricted".to_string(),
            version: PROFILE_SCHEMA_VERSION,
            permissions: PermissionMode::Restricted,
            ..Default::default()
        };
        let permissions = ToolPermissions::from_profile(&profile);
        assert_eq!(permissions.mode(), PermissionMode::Restricted);
    }

    #[test]
    fn default_tool_permissions_remain_allow_all() {
        // Ensure the existing zero-value default keeps its allow-all
        // semantics so unrelated tests/contexts do not regress.
        let permissions = ToolPermissions::default();
        assert!(permissions.is_tool_allowed("anything"));
        assert!(permissions.is_tool_allowed("shell"));
    }
}
