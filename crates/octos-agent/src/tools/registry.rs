//! Tool registry: stores, filters, and executes registered tools.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use eyre::Result;
use octos_llm::ToolSpec;

use crate::policy::EffectivePermissions;
use crate::task_supervisor::TaskSupervisor;

#[cfg(feature = "ast")]
use super::CodeStructureTool;
use super::policy::{self, ToolPolicy};
use super::{
    ApplyPatchTool, AskUserQuestionTool, BrowserTool, CheckWorkspaceContractTool, CloseAgentTool,
    ConfigureToolTool, DiffEditTool, EditFileTool, ExecCommandTool, GlobTool, GrepTool,
    ImageGenerationTool, ListDirTool, ReadFileTool, RequestUserInputTool, ResumeAgentTool,
    SendInputTool, ShellTool, SpawnAgentTool, Tool, ToolCatalogEntry, ToolConfigStore, ToolResult,
    ToolSearchTool, ToolSuggestTool, UpdatePlanTool, ViewImageTool, WaitAgentTool, WebFetchTool,
    WebSearchTool, WorkspaceDiffTool, WorkspaceLogTool, WorkspaceShowTool, WriteFileTool,
    WriteStdinTool,
};
use crate::sandbox::{NoSandbox, Sandbox};

fn policy_equivalent_tool_names(name: &str) -> Vec<&str> {
    match name {
        "spawn_agent" => vec!["spawn_agent", "spawn"],
        "wait_agent" => vec!["wait_agent", "read_task_output"],
        // #1607 (codex round 2): `shell`/`bash`/`exec_command` are the same
        // command capability (the `bash`/`exec_command` names are codex-compat
        // aliases that share the shell policy — see `register`). A provider
        // policy naming any one must apply to all three: otherwise `deny=["shell"]`
        // is trivially bypassed via `bash`, and `allow=["shell"]` wrongly drops a
        // `ToolCall` validator that uses `bash`/`exec_command`.
        "shell" | "bash" | "exec_command" => vec!["shell", "bash", "exec_command"],
        _ => vec![name],
    }
}

fn evaluate_provider_policy_equivalent(policy: &ToolPolicy, name: &str) -> policy::PolicyDecision {
    let names = policy_equivalent_tool_names(name);
    for entry in &policy.deny {
        if names
            .iter()
            .any(|candidate| policy::entry_matches(entry, candidate))
        {
            return policy::PolicyDecision::Deny {
                reason: policy::GENERIC_DENY_REASON,
            };
        }
    }
    if policy.allow.is_empty()
        || policy.allow.iter().any(|entry| {
            names
                .iter()
                .any(|candidate| policy::entry_matches(entry, candidate))
        })
    {
        return policy::PolicyDecision::Allow;
    }
    policy::PolicyDecision::Deny {
        reason: policy::GENERIC_DENY_REASON,
    }
}

fn provider_policy_allows_equivalent_with_tags(
    policy: &ToolPolicy,
    name: &str,
    tool_tags: &[&str],
) -> bool {
    if !matches!(
        evaluate_provider_policy_equivalent(policy, name),
        policy::PolicyDecision::Allow
    ) {
        return false;
    }
    if policy.require_tags.is_empty() {
        return true;
    }
    // SECURITY (peer-review fix): mirror `ToolPolicy::is_allowed_with_tags`
    // — untagged tools FAIL a non-empty `require_tags` gate (fail closed).
    // Plugin and MCP tools never declare tags, so the old empty-tags
    // exemption made `require_tags` a no-op for exactly the unaudited tool
    // surface it exists to confine.
    tool_tags
        .iter()
        .any(|tag| policy.require_tags.iter().any(|required| required == tag))
}

/// Estimate the serialized JSON size without allocating.
/// Walks the serde_json::Value tree recursively, counting bytes.
fn estimate_json_size(value: &serde_json::Value) -> usize {
    match value {
        serde_json::Value::Null => 4,
        serde_json::Value::Bool(b) => {
            if *b {
                4
            } else {
                5
            }
        }
        serde_json::Value::Number(n) => n.to_string().len(),
        serde_json::Value::String(s) => {
            let escapes = s
                .bytes()
                .filter(|&b| matches!(b, b'"' | b'\\' | b'\n' | b'\r' | b'\t'))
                .count();
            s.len() + escapes + 2 // content + escape overheads + quotes
        }
        serde_json::Value::Array(arr) => {
            2 + arr.iter().map(estimate_json_size).sum::<usize>() + arr.len().saturating_sub(1) // commas
        }
        serde_json::Value::Object(obj) => {
            2 + obj
                .iter()
                .map(|(k, v)| k.len() + 3 + estimate_json_size(v))
                .sum::<usize>()
                + obj.len().saturating_sub(1) // commas
        }
    }
}

/// Registry of available tools.
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
    workspace_root: Option<PathBuf>,
    /// Provider-specific policy that filters specs() output without removing tools.
    provider_policy: Option<ToolPolicy>,
    /// Context-based tag filter: only tools with matching tags appear in specs().
    /// Tools with empty tags always pass.
    context_filter: Option<Vec<String>>,
    /// Per-turn model context used to expose context-scoped plugin tools.
    /// `None` is the ordinary/default context.
    active_context: Option<String>,
    /// Cached specs output, invalidated on registry mutations.
    cached_specs: std::sync::Mutex<Option<Vec<ToolSpec>>>,
    /// Tool names that came from plugin binaries (for auto-send hook filtering).
    plugin_tools: HashSet<String>,
    /// Live MCP transport handles, owned for the registry's whole lifetime.
    ///
    /// `McpService` is an `Arc<RunningService<..>>` and every `McpTool` holds a
    /// clone, so before this existed the registered TOOLS were the only owners.
    /// Any `retain()` that dropped the last MCP tool therefore dropped the last
    /// `Arc`, cancelling the transport and killing the stdio child — profile
    /// narrowing silently tore down a server the operator had explicitly
    /// configured (#1886). Tool VISIBILITY must not control transport LIFETIME,
    /// so the registry holds its own reference that `retain()` never touches.
    ///
    /// Type-erased: the registry does not care what kind of handle this is, only
    /// that dropping the tools must not drop the connection.
    mcp_services: Vec<Arc<dyn std::any::Any + Send + Sync>>,
    /// Tools whose execution is auto-redirected to a background tokio task
    /// in the execution loop (see `is_spawn_only` + the spawn_only branch
    /// in `agent/execution.rs`). These tools ARE visible in `specs()` and
    /// callable by the LLM — the LLM's tool call is intercepted at execute
    /// time and converted into a background spawn that returns immediately.
    ///
    /// RFC-0 (#1289): LRU tool deferral was removed, so spawn_only tools are
    /// always visible in `specs()` for the life of the session — there is no
    /// longer any recency-based eviction path that could hide them.
    spawn_only: HashSet<String>,
    /// Custom messages for spawn_only tools returned to the LLM after auto-backgrounding.
    spawn_only_messages: HashMap<String, String>,
    /// Callback to notify session actor when background (spawn_only) tasks complete or fail.
    background_result_sender: Option<super::spawn::BackgroundResultSender>,
    /// Supervisor for tracking background task lifecycle.
    supervisor: Arc<TaskSupervisor>,
    /// Set to true when any spawn_only tool is actually invoked in this agent run.
    spawn_only_invoked: Arc<std::sync::atomic::AtomicBool>,
    /// #1148 codex P2: shared live catalog cell used by `tool_search` /
    /// `tool_suggest`. Updated on every mutation (`register`,
    /// `register_arc`, `apply_policy`, `mark_spawn_only`, etc.) so
    /// the discovery surface always reflects the live registry's
    /// visible tools. The Mutex is fine here — refreshes are cheap
    /// (clones a small Vec) and only happen on registry mutations.
    live_catalog: Arc<std::sync::Mutex<Vec<ToolCatalogEntry>>>,
    /// Session key for tagging background tasks (set per-session).
    session_key: Option<String>,
    /// Precomputed output directory hint for spawn_only tool messaging.
    output_dir_hint: Option<String>,
    /// Global per-tool execution-timeout backstop (seconds) enforced at the
    /// dispatch boundary in `execute_with_context` (Gap 3.3). A hung
    /// foreground tool degrades to a failed `ToolResult` after this many
    /// seconds instead of wedging the caller (e.g. the session-actor turn).
    ///
    /// Defaults to [`DEFAULT_REGISTRY_TOOL_TIMEOUT_SECS`] (1800s) so it never
    /// fires before the agent loop's own per-batch timeout on that path, and
    /// so genuinely long-running foreground tools (`web_fetch`, `browser`,
    /// deep research/crawl) have generous headroom. A tool can tighten this
    /// for itself via [`Tool::execution_timeout_secs`]; the per-tool override
    /// wins when present. `spawn_only` tools never reach this path (they are
    /// backgrounded earlier in the execution loop).
    tool_timeout_secs: u64,
    /// RFC-1 fixup (codex P1): tool names that are registered for
    /// **internal** dispatch only — callable through `get()` /
    /// `get_tool()` so internal forwarders (e.g. the `mofa_make`
    /// dispatcher routing to its target) can reach them, but excluded
    /// from `specs()` so the LLM never sees them in its tool list.
    ///
    /// This is the right semantic for `mofa_make`'s hidden targets
    /// (`mofa_slides`, `mofa_cards`, ...): the dispatcher is the ONLY
    /// supported LLM entry-point.
    internal_hidden: HashSet<String>,
    /// #1607: the session sandbox handed to the shell/exec/bash tools at
    /// construction. Stored (not just handed off and dropped) so the
    /// Agent-internal project-root validator path
    /// (`workspace_contract::build_validator_runner`) can thread the same
    /// sandbox into its `ValidatorRunner` and confine
    /// `ValidatorSpec::Command` validators declared by an untrusted
    /// workspace policy. Defaults to `Arc::new(NoSandbox)` (a no-op sandbox
    /// whose `is_noop()==true`), so on the plain `with_builtins`/`Default`
    /// path — and on any host without a real backend — command validators
    /// run the argv directly and behavior is unchanged.
    sandbox: Arc<dyn Sandbox>,
}

/// Default per-tool execution-timeout backstop (seconds) for the registry
/// dispatch boundary. Matches the agent loop's `MAX_TOOL_TIMEOUT_SECS`
/// (1800s / 30 min) so this guard is a pure safety net for hung tools and
/// for direct registry callers that do not run under the agent loop's
/// per-batch timeout — it never pre-empts a tool the LLM legitimately
/// requested up to 1800s for.
pub const DEFAULT_REGISTRY_TOOL_TIMEOUT_SECS: u64 = 1800;

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolRegistry {
    /// Create an empty tool registry.
    pub fn new() -> Self {
        Self {
            tools: HashMap::new(),
            workspace_root: None,
            provider_policy: None,
            context_filter: None,
            active_context: None,
            cached_specs: std::sync::Mutex::new(None),
            plugin_tools: HashSet::new(),
            mcp_services: Vec::new(),
            spawn_only: HashSet::new(),
            spawn_only_messages: HashMap::new(),
            background_result_sender: None,
            supervisor: Arc::new(TaskSupervisor::new()),
            spawn_only_invoked: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            live_catalog: Arc::new(std::sync::Mutex::new(Vec::new())),
            session_key: None,
            output_dir_hint: None,
            tool_timeout_secs: DEFAULT_REGISTRY_TOOL_TIMEOUT_SECS,
            internal_hidden: HashSet::new(),
            // #1607: default to a no-op sandbox. Constructors that receive a
            // real sandbox (`with_builtins_and_permissions`,
            // `rebind_cwd_with_permissions`) overwrite this below.
            sandbox: Arc::new(NoSandbox),
        }
    }

    /// #1607: the session sandbox stored on this registry. Threaded into the
    /// Agent-internal project-root validator runner
    /// (`workspace_contract::build_validator_runner`) so
    /// `ValidatorSpec::Command` validators declared by an untrusted workspace
    /// policy are confined to the same sandbox as the shell/exec tools instead
    /// of running unsandboxed on the host. A no-op sandbox
    /// (`NoSandbox`, or a backend whose helper is unavailable) has nothing to
    /// escape, so `ValidatorRunner` runs the argv directly there.
    pub fn sandbox(&self) -> Arc<dyn Sandbox> {
        self.sandbox.clone()
    }

    /// Set the global per-tool execution-timeout backstop (seconds) enforced
    /// at the dispatch boundary (Gap 3.3). A value of `0` is clamped up to
    /// `1` so the guard is always live. Per-tool overrides via
    /// [`Tool::execution_timeout_secs`] still win when present.
    pub fn set_tool_timeout_secs(&mut self, secs: u64) {
        self.tool_timeout_secs = secs.max(1);
    }

    /// The global per-tool execution-timeout backstop (seconds).
    pub fn tool_timeout_secs(&self) -> u64 {
        self.tool_timeout_secs
    }

    /// Mark a tool name as coming from a plugin binary.
    pub fn mark_as_plugin(&mut self, name: &str) {
        self.plugin_tools.insert(name.to_string());
    }

    /// Set the session key used to tag background tasks.
    pub fn set_session_key(&mut self, key: String) {
        self.session_key = Some(key);
    }

    /// Mark a tool as spawn_only with an optional custom message.
    pub fn mark_spawn_only(&mut self, name: &str, message: Option<String>) {
        self.spawn_only.insert(name.to_string());
        if let Some(msg) = message {
            self.spawn_only_messages.insert(name.to_string(), msg);
        }
    }

    /// RFC-1 fixup (codex P1): mark a tool as **internal hidden**.
    ///
    /// Internal hidden tools are still callable through `get()` /
    /// `get_tool()` (so internal forwarders like the `mofa_make`
    /// dispatcher can reach them), but they are removed from `specs()`
    /// AND from `activate_tools`'s enumerated description AND cannot
    /// be re-promoted via `activate(name)`.
    ///
    /// Use this for dispatcher target tools (mofa_slides, mofa_cards,
    /// etc.) that should ONLY be reachable through their dispatcher
    /// (`mofa_make`) — never directly callable by the LLM.
    ///
    /// Unlike `defer`, this is a one-way operation from the LLM's
    /// point of view: there is no "un-hide" path through any
    /// LLM-callable tool. (Internal callers can clear the marker
    /// programmatically via [`Self::clear_internal_hidden`] if a
    /// future code path needs to surface the tool — e.g. spawn child
    /// registries that clear spawn_only also clear this set.)
    pub fn mark_internal_hidden(&mut self, name: &str) {
        if self.tools.contains_key(name) {
            self.internal_hidden.insert(name.to_string());
        }
        self.invalidate_cache();
    }

    /// Whether the given tool is currently internal-hidden (RFC-1).
    pub fn is_internal_hidden(&self, name: &str) -> bool {
        self.internal_hidden.contains(name)
    }

    /// Clear all internal-hidden markers. Used by spawn child registries
    /// where the same tools should be directly callable (the subagent
    /// IS the background context — no dispatcher indirection needed).
    pub fn clear_internal_hidden(&mut self) {
        if self.internal_hidden.is_empty() {
            return;
        }
        self.internal_hidden.clear();
        self.invalidate_cache();
    }

    /// Check if a tool is marked spawn_only.
    pub fn is_spawn_only(&self, name: &str) -> bool {
        self.spawn_only.contains(name)
    }

    /// Clear all spawn_only markers so tools appear as regular tools.
    /// Used in subagent registries where spawn_only tools should be
    /// callable directly (the subagent IS the background context).
    pub fn clear_spawn_only(&mut self) {
        self.spawn_only.clear();
        self.spawn_only_messages.clear();
        self.invalidate_cache();
    }

    /// Get the custom message for a spawn_only tool, or a default.
    /// Includes the output directory so the LLM knows where files will be written.
    pub fn spawn_only_message(&self, name: &str) -> String {
        let base = self.spawn_only_messages
            .get(name)
            .cloned()
            .unwrap_or_else(|| "SUCCESS: Task is now running in background. The result will be delivered to the user automatically. No further action needed.".to_string());
        let output_dir = self
            .output_dir_hint
            .clone()
            .unwrap_or_else(|| "skill-output/".to_string());
        format!("{base}\nOutput directory: {output_dir}")
    }

    /// M10 Phase 4 — agent context isolation.
    ///
    /// Build the JSON-shaped tool result returned to the LLM when a
    /// `spawn_only` tool is auto-backgrounded. Instead of the previous
    /// free-text "SUCCESS…" line plus the full tool stdout, the LLM now
    /// receives a small `task_handle` envelope and is expected to call
    /// `read_task_output(task_handle, mode=…)` if it wants to inspect the
    /// background work.
    ///
    /// Wire-compat note: the full output is still persisted server-side
    /// via the M8.7 `SubAgentOutputRouter` and delivered to the SPA via
    /// `BackgroundResultSender::turn.spawn_complete`. This change only
    /// alters what the *LLM* sees; the UI envelope is unchanged.
    pub fn spawn_only_handle_message(
        &self,
        name: &str,
        task_id: &str,
        expected_files: &[String],
    ) -> String {
        let custom = self.spawn_only_messages.get(name).cloned();
        let summary = custom.unwrap_or_else(|| {
            format!(
                "Background work started for `{name}`. The final result will be delivered \
                 automatically when ready. Use read_task_output(task_handle, mode={{…}}) to \
                 inspect intermediate output without bloating context."
            )
        });
        let output_dir = self
            .output_dir_hint
            .clone()
            .unwrap_or_else(|| "skill-output/".to_string());
        let payload = serde_json::json!({
            "ok": true,
            "task_handle": task_id,
            "summary": summary,
            "expected_files": expected_files,
            "output_dir": output_dir,
            "read_with": "read_task_output",
            "read_modes": ["head", "tail", "grep", "line_range", "file"],
        });
        // serde_json::to_string never fails on a json!{} value built from
        // owned strings + arrays, but fall back to lossy stringification
        // just in case.
        serde_json::to_string(&payload).unwrap_or_else(|_| payload.to_string())
    }

    /// Set the output directory hint included in spawn_only tool messages.
    pub fn set_output_dir_hint(&mut self, output_dir: impl Into<String>) {
        let mut output_dir = output_dir.into();
        if !output_dir.ends_with('/') {
            output_dir.push('/');
        }
        self.output_dir_hint = Some(output_dir);
    }

    /// Set background result sender for spawn_only task lifecycle notifications.
    pub fn set_background_result_sender(&mut self, sender: super::spawn::BackgroundResultSender) {
        self.background_result_sender = Some(sender);
    }

    /// Get background result sender (cloned Arc).
    pub fn background_result_sender(&self) -> Option<super::spawn::BackgroundResultSender> {
        self.background_result_sender.clone()
    }

    /// Get a shared handle to the task supervisor.
    pub fn supervisor(&self) -> Arc<TaskSupervisor> {
        self.supervisor.clone()
    }

    /// Root workspace path associated with this registry, if any.
    pub fn workspace_root(&self) -> Option<&Path> {
        self.workspace_root.as_deref()
    }

    /// Record a workspace cwd on this registry without re-creating the
    /// cwd-bound tools. Used by the AppUi `session_tool_registry` Tier-2
    /// fallback so an operator-configured default folder shows up in
    /// `workspace_root()` and the per-session `rebind_cwd` path can pick
    /// it up. The existing `rebind_cwd` API mints a fresh registry, which
    /// is wasteful when we only want to update the recorded path on a
    /// freshly-built registry; this setter mutates in place.
    pub fn set_workspace_root(&mut self, cwd: PathBuf) {
        self.workspace_root = Some(cwd);
    }

    /// Register a background task and return its ID.
    pub fn register_task(&self, tool_name: &str, tool_call_id: &str) -> String {
        self.supervisor
            .register(tool_name, tool_call_id, self.session_key.as_deref())
    }

    /// Register a background task and capture the original tool input so
    /// failure-recovery flows (M8.9) can reference it without re-walking
    /// the message history.
    pub fn register_task_with_input(
        &self,
        tool_name: &str,
        tool_call_id: &str,
        tool_input: Option<serde_json::Value>,
    ) -> String {
        self.supervisor.register_with_input(
            tool_name,
            tool_call_id,
            self.session_key.as_deref(),
            tool_input,
        )
    }

    /// Issue #738 fix: register a background task while also threading
    /// the originating user turn's `client_message_id`. Used by the
    /// spawn_only execution path so the synthetic recovery turn (M8.9)
    /// can stamp the original cmid into its `InboundMessage` metadata
    /// instead of minting an orphan UUIDv7.
    pub fn register_task_with_input_and_cmid(
        &self,
        tool_name: &str,
        tool_call_id: &str,
        tool_input: Option<serde_json::Value>,
        originating_client_message_id: Option<String>,
    ) -> String {
        self.supervisor.register_with_input_and_cmid(
            tool_name,
            tool_call_id,
            self.session_key.as_deref(),
            tool_input,
            originating_client_message_id,
        )
    }

    /// Return the number of currently active background tasks.
    pub fn bg_task_count(&self) -> u32 {
        self.supervisor.task_count() as u32
    }

    /// Return the set of spawn_only tool names.
    pub fn spawn_only_tools(&self) -> &HashSet<String> {
        &self.spawn_only
    }

    /// Mark that a spawn_only tool was invoked in this agent run.
    pub fn mark_spawn_only_invoked(&self) {
        self.spawn_only_invoked.store(true, Ordering::SeqCst);
    }

    /// Check if any spawn_only tool was invoked in this agent run.
    pub fn spawn_only_was_invoked(&self) -> bool {
        self.spawn_only_invoked.load(Ordering::SeqCst)
    }

    /// Reset the spawn_only_invoked flag (call at start of each agent run).
    pub fn reset_spawn_only_invoked(&self) {
        self.spawn_only_invoked.store(false, Ordering::SeqCst);
    }

    /// Check if a tool came from a plugin binary.
    pub fn is_plugin(&self, name: &str) -> bool {
        self.plugin_tools.contains(name)
    }

    /// Register a tool.
    pub fn register(&mut self, tool: impl Tool + 'static) {
        let name = tool.name().to_string();
        let tool: Arc<dyn Tool> = Arc::new(tool);
        self.tools.insert(name.clone(), tool.clone());
        if name == "spawn" {
            let spawn_agent: Arc<dyn Tool> = Arc::new(SpawnAgentTool::with_delegate(tool));
            self.tools
                .insert("spawn_agent".to_string(), spawn_agent.clone());
            // #1172: the `delegate` alias wraps spawn_agent + wait_agent.
            // Re-bind it whenever spawn_agent moves so the Codex alias
            // sees the live delegate, not the no-op default.
            self.tools.insert(
                "delegate".to_string(),
                Arc::new(super::coding_tools::DelegateAliasTool::with_spawn_agent(
                    spawn_agent,
                )),
            );
        }
        self.invalidate_cache();
    }

    /// Register a tool from an existing Arc (for keeping a separate reference).
    pub fn register_arc(&mut self, tool: Arc<dyn Tool>) {
        let name = tool.name().to_string();
        self.tools.insert(name.clone(), tool.clone());
        if name == "spawn" {
            let spawn_agent: Arc<dyn Tool> = Arc::new(SpawnAgentTool::with_delegate(tool));
            self.tools
                .insert("spawn_agent".to_string(), spawn_agent.clone());
            self.tools.insert(
                "delegate".to_string(),
                Arc::new(super::coding_tools::DelegateAliasTool::with_spawn_agent(
                    spawn_agent,
                )),
            );
        }
        self.invalidate_cache();
    }

    /// Return the names of every registered tool.
    ///
    /// Used by the validator runner's lightweight dispatcher to capture a
    /// snapshot of available tools without cloning the full registry.
    pub fn tool_names(&self) -> Vec<String> {
        self.tools.keys().cloned().collect()
    }

    /// Return a handle to a tool by name, if it exists.
    pub fn get_tool(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
    }

    /// Look up the concurrency class of a registered tool (M8.8).
    ///
    /// Unknown tools report [`super::ConcurrencyClass::Safe`] — the executor
    /// defers error handling to `execute()` which bails with `unknown tool`
    /// rather than letting the admission phase fail silently.
    ///
    /// Plugin and MCP wrappers override `Tool::concurrency_class()` and
    /// surface their declared class:
    /// - Plugin wrapper: reads `concurrency_class` from the manifest tool
    ///   def. Defaults to `Safe` so the bundled read-only skills (weather,
    ///   news, time, deep-search, …) keep their parallel-friendly path. A
    ///   plugin tool that writes files or mutates remote state must declare
    ///   `"exclusive"` in its manifest.
    /// - MCP wrapper: reads `concurrency_class` from
    ///   `McpServerConfig`. Defaults to `Safe` because most MCP servers
    ///   in practice are read-only (search, wiki, time, weather);
    ///   operators must declare `"exclusive"` per server when the MCP
    ///   server mutates files / remote state and could race with the
    ///   native `edit_file` / `write_file` tools. Unknown values fail
    ///   safe to `Exclusive`.
    pub fn concurrency_class(&self, name: &str) -> super::ConcurrencyClass {
        self.tools
            .get(name)
            .map(|t| t.concurrency_class())
            .unwrap_or_default()
    }

    /// Whether the named tool blocks on human input (e.g. `ask_user_question`
    /// awaiting the requester until the client answers). Mirrors
    /// [`Tool::blocks_on_human_input`]; unknown tools report `false`.
    ///
    /// Used by the agent batch dispatcher (`agent::execution`) to detect a
    /// human-wait batch and skip the finite batch-level `tokio::time::timeout`
    /// wrap that would otherwise detach the still-running tool task after the
    /// ceiling fired — leaking the pending-question store entry and replaying a
    /// stale prompt after the turn moved on (UPCR-2026-023). The registry
    /// dispatch boundary already exempts these tools via
    /// [`Tool::blocks_on_human_input`]; this surfaces the same fact one layer
    /// up so the outer batch wrap can be skipped too.
    pub fn blocks_on_human_input(&self, name: &str) -> bool {
        self.tools
            .get(name)
            .map(|t| t.blocks_on_human_input())
            .unwrap_or(false)
    }

    /// Return a tool's declared foreground execution budget, when it has
    /// one. The agent dispatcher uses this before spawning a batch so plugin
    /// manifest timeouts are not accidentally shortened by the generic
    /// interactive-tool default.
    pub fn execution_timeout_secs(&self, name: &str) -> Option<u64> {
        self.tools
            .get(name)
            .and_then(|tool| tool.execution_timeout_secs())
    }

    /// Get tool specifications for the LLM, filtered by provider policy if set.
    /// Results are cached and invalidated when the registry is mutated.
    /// Codex round 2 P2: visibility-aware tool lookup.
    ///
    /// Returns `true` only if `name` is registered AND would be exposed to
    /// the LLM by `specs()` — i.e. it is not internal-hidden, not denied by
    /// the provider policy, and (when a context filter is set) carries a
    /// matching tag. Used by the spawn_only intercept to decide whether
    /// the LLM can actually call `read_task_output` before it advertises
    /// the new `task_handle` envelope.
    pub fn is_tool_visible(&self, name: &str) -> bool {
        // RFC-1 fixup (codex P1): internal-hidden tools are invisible to
        // the LLM. They are callable only via internal forwarders (e.g.
        // `mofa_make`), never through the LLM's tool list.
        if self.internal_hidden.contains(name) {
            return false;
        }
        self.is_tool_visible_post_activation(name)
    }

    /// Visibility predicate shared with [`is_tool_visible`] that applies the
    /// `provider_policy` + `context_filter` checks.
    pub fn is_tool_visible_post_activation(&self, name: &str) -> bool {
        let Some(tool) = self.tools.get(name) else {
            return false;
        };
        if self.internal_hidden.contains(name) {
            return false;
        }
        if let Some(ref policy) = self.provider_policy {
            if !provider_policy_allows_equivalent_with_tags(policy, name, tool.tags()) {
                return false;
            }
        }
        if let Some(ref tags) = self.context_filter {
            let tool_tags = tool.tags();
            if !tool_tags.is_empty() && !tool_tags.iter().any(|tag| tags.contains(&tag.to_string()))
            {
                return false;
            }
        }
        if !self.active_context_allows(tool.as_ref()) {
            return false;
        }
        true
    }

    fn active_context_allows(&self, tool: &dyn Tool) -> bool {
        let contexts = tool.contexts();
        contexts.is_empty()
            || self
                .active_context
                .as_ref()
                .is_some_and(|active| contexts.iter().any(|allowed| allowed == active))
    }

    pub fn specs(&self) -> Vec<ToolSpec> {
        let mut cache = self.cached_specs.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(ref specs) = *cache {
            return specs.clone();
        }

        // RFC-0 (#1289): every enabled tool is emitted every turn. The only
        // exclusions remaining are internal-hidden tools (mofa_make
        // dispatcher targets), provider-policy denials, and context-filter
        // misses. There is no longer any recency-based (LRU) deferral nor an
        // `activate_tools` meta-tool description to inject.
        let mut specs: Vec<ToolSpec> = self
            .tools
            .values()
            // RFC-1 fixup (codex P1): exclude internal-hidden tools from
            // the LLM-visible spec set. They remain callable via `get()`
            // for internal forwarders (e.g. `mofa_make`).
            .filter(|t| !self.internal_hidden.contains(t.name()))
            .filter(|t| {
                self.provider_policy.as_ref().is_none_or(|p| {
                    provider_policy_allows_equivalent_with_tags(p, t.name(), t.tags())
                })
            })
            .filter(|t| {
                self.context_filter.as_ref().is_none_or(|tags| {
                    // Tools with no tags pass through; tools with tags must match
                    let tool_tags = t.tags();
                    tool_tags.is_empty()
                        || tool_tags.iter().any(|tag| tags.contains(&tag.to_string()))
                })
            })
            .filter(|t| self.active_context_allows(t.as_ref()))
            .map(|t| ToolSpec {
                name: t.name().to_string(),
                description: t.description().to_string(),
                input_schema: t.input_schema(),
            })
            .collect();

        // Deterministic order: `self.tools` is a HashMap, so `.values()`
        // iteration order varies per process and per registry rebuild.
        // Providers replay this array verbatim into the LLM prompt prefix, so
        // a shuffled order made requests nondeterministic AND busted
        // provider-side prompt caches (the tool array is the first segment of
        // the cached prefix — e.g. Anthropic `cache_control`) on every
        // rebuild. Sort by name so identical tool sets serialize identically.
        specs.sort_by(|a, b| a.name.cmp(&b.name));

        *cache = Some(specs.clone());
        specs
    }

    /// Get a tool by name.
    pub fn get(&self, name: &str) -> Option<&Arc<dyn Tool>> {
        self.tools.get(name)
    }

    /// Return the names of tools that advertise a given tag.
    pub fn names_with_tag(&self, tag: &str) -> Vec<String> {
        self.tools
            .iter()
            .filter(|(_, tool)| tool.tags().contains(&tag))
            .map(|(name, _)| name.clone())
            .collect()
    }

    /// Number of registered tools.
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    /// Whether the registry is empty.
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// Retain only tools whose names satisfy the predicate.
    ///
    /// Also prunes parallel side state (`spawn_only`,
    /// `spawn_only_messages`, `internal_hidden`) for any names that were
    /// dropped. Without this, a stale `spawn_only` marker fools the agent's
    /// spawn_only intercept in `execution.rs` into treating an evicted tool
    /// as background-eligible. The intercept falls through to
    /// `bg_tools.execute_with_context` which fails async because the tool
    /// itself is gone from the registry — so the foreground turn observes a
    /// fake "started successfully". See PR #688 follow-up MEDIUM #3.
    /// Take ownership of a live MCP transport so it outlives any filtering of
    /// the tools it provides — see [`ToolRegistry::mcp_services`]. Without this
    /// the tools are the only owners and narrowing kills the child process.
    pub fn keep_mcp_service_alive(&mut self, service: Arc<dyn std::any::Any + Send + Sync>) {
        self.mcp_services.push(service);
    }

    /// How many MCP transports this registry keeps alive, so a caller can assert
    /// they survived a filter that removed their tools.
    pub fn live_mcp_transport_count(&self) -> usize {
        self.mcp_services.len()
    }

    pub fn retain(&mut self, f: impl Fn(&str) -> bool) {
        self.tools.retain(|name, _| f(name));
        self.spawn_only.retain(|name| self.tools.contains_key(name));
        self.spawn_only_messages
            .retain(|name, _| self.tools.contains_key(name));
        // RFC-1 fixup: prune stale internal-hidden markers symmetrically.
        self.internal_hidden
            .retain(|name| self.tools.contains_key(name));
        // RFC-1 fixup (codex round 4 P2 + round 5 P1/P2): prune
        // dispatcher catalogs when their forwarding targets are
        // evicted. The slides-session
        // `retain(keep_tool_in_slides_session)` removes `mofa_cards`,
        // `mofa_comic`, etc. from `tools`, but the `MofaMakeTool`
        // dispatcher's catalog (built at load time) still advertises
        // those `content_type` enum values to the LLM. Without this
        // prune the LLM can call `mofa_make({content_type: "cards"})`
        // and observe a `[DISPATCHER_ERROR]` because the target was
        // evicted — weakening the slides-only guardrail.
        //
        // Round 5 P1: this registry may have been built via
        // `snapshot_excluding` / `rebind_cwd`, which clones the
        // dispatcher tool as a SHARED `Arc<dyn Tool>` with the
        // base/profile registry. Calling `replace_entries` on a shared
        // dispatcher would poison the base's catalog — every subsequent
        // session cloned from the same base would also observe the
        // pruned catalog. Round 5 P2: an earlier attempt gated on
        // `Arc::strong_count > 2`, but the threshold is racy under
        // concurrent retains across sibling snapshots (the count can
        // dip back to 2 between snapshot ops, causing the in-place
        // branch to fire on a still-shared Arc). Always mint a FRESH
        // dispatcher seeded with surviving entries and register it
        // locally — the allocation cost is paid once per retain pass
        // and the shared-Arc hazard is unconditionally eliminated. The
        // `Weak<ToolRegistry>` back-ref on the fresh dispatcher is
        // intentionally left unwired here —
        // `Agent::new::refresh_mofa_make_dispatcher_in_place` /
        // `wire_mofa_make_registry_back_ref` rewires it before the
        // agent loop executes, matching the freshen path used by
        // pipeline / per-turn snapshots.
        if let Some(arc) = self.tools.get("mofa_make").cloned() {
            if let Some(dispatcher) = arc.as_any().downcast_ref::<super::MofaMakeTool>() {
                let surviving: Vec<super::MakeTypeEntry> = dispatcher
                    .entries()
                    .into_iter()
                    .filter(|entry| self.tools.contains_key(&entry.target_tool))
                    .collect();
                let fresh = super::MofaMakeTool::new();
                for entry in &surviving {
                    fresh.register_or_replace(entry.clone());
                }
                self.register(fresh);
                if let Some(describe_arc) = self.tools.get("mofa_describe_content_type").cloned() {
                    if describe_arc
                        .as_any()
                        .downcast_ref::<super::MofaDescribeContentTypeTool>()
                        .is_some()
                    {
                        let fresh_describe = super::MofaDescribeContentTypeTool::new();
                        for entry in &surviving {
                            fresh_describe.register_or_replace(entry.clone());
                        }
                        self.register(fresh_describe);
                    }
                }
            }
        }
        self.invalidate_cache();
    }

    /// Remove tools not permitted by the given policy.
    pub fn apply_policy(&mut self, policy: &ToolPolicy) {
        if policy.is_empty() {
            return;
        }
        self.retain(|name| policy.is_allowed(name));
    }

    /// Narrow the registry to the tools permitted by a profile's tool
    /// declaration ([`crate::profile::ProfileTools`]).
    ///
    /// Unlike [`ToolRegistry::apply_policy`] this method consumes the
    /// profile-shaped enum directly so the CLI does not need to translate
    /// profile modes into a [`ToolPolicy`] round-trip. Behaviour by mode:
    ///
    /// - [`crate::profile::ProfileTools::Default`] — no-op. The registry
    ///   passes through untouched so the built-in `coding` profile
    ///   preserves today's behaviour byte-for-byte.
    /// - [`crate::profile::ProfileTools::AllowList`] — keeps tools whose
    ///   names match the allow list (plain name, `group:<id>`, or
    ///   `<prefix>*` wildcard). Any tool marked `spawn_only` is retained
    ///   regardless — they carry background-execution wiring the runtime
    ///   depends on.
    /// - [`crate::profile::ProfileTools::DenyList`] — drops tools matching
    ///   any deny list entry (same matching rules). Spawn-only tools are
    ///   likewise preserved.
    ///
    /// The filter runs in-place. Cache invalidation is handled by
    /// [`ToolRegistry::retain`]. Intended to be called as a post-build
    /// step during startup; never from inside the agent loop.
    pub fn filter_by_profile(&mut self, tools: &crate::profile::ProfileTools) {
        use crate::profile::ProfileTools;

        match tools {
            ProfileTools::Default => {
                // No-op — the default mode is the behaviour-parity path.
            }
            ProfileTools::AllowList { tools: allow } => {
                if allow.is_empty() {
                    // Empty allow list would evict the entire registry
                    // minus spawn_only tools; that is a surprising outcome
                    // for profile authors, so treat it as a pass-through
                    // with a warning. Authors who really want to kill
                    // every tool should use an explicit `deny_list`.
                    tracing::warn!(
                        "profile declares empty allow_list — skipping filter; use deny_list to \
                         blacklist tools"
                    );
                    return;
                }
                let spawn_only = self.spawn_only.clone();
                let allow_entries: Vec<String> = allow.clone();
                self.retain(|name| {
                    spawn_only.contains(name)
                        || allow_entries
                            .iter()
                            .any(|entry| policy::entry_matches(entry, name))
                });
            }
            ProfileTools::DenyList { tools: deny } => {
                if deny.is_empty() {
                    return;
                }
                let spawn_only = self.spawn_only.clone();
                let deny_entries: Vec<String> = deny.clone();
                self.retain(|name| {
                    spawn_only.contains(name)
                        || !deny_entries
                            .iter()
                            .any(|entry| policy::entry_matches(entry, name))
                });
            }
        }
    }

    /// Set a provider-specific policy that filters `specs()` and `execute()`.
    ///
    /// Unlike `apply_policy` which permanently removes tools from the registry,
    /// this keeps tools registered but blocks both spec visibility and execution.
    pub fn set_provider_policy(&mut self, policy: ToolPolicy) {
        if policy.is_empty() {
            return;
        }
        self.provider_policy = Some(policy);
        self.invalidate_cache();
    }

    /// Return the current provider policy (if any), so callers like SpawnTool
    /// can propagate it to subagent registries.
    pub fn provider_policy(&self) -> Option<&ToolPolicy> {
        self.provider_policy.as_ref()
    }

    /// #1607 (P2): whether the active **provider policy** permits dispatching
    /// the named tool. Applies the exact deny-wins-then-allow semantics that
    /// [`Self::execute_with_context`] enforces at the dispatch boundary
    /// (including alias equivalence, e.g. `bash`/`shell`/`exec_command`), but
    /// unlike [`Self::is_tool_visible`] does NOT apply the `context_filter` or
    /// internal-hidden markers — it answers only "would the provider policy
    /// let the model call this tool".
    ///
    /// Used by [`crate::validators::MapToolDispatcher::from_registry`] to keep
    /// project-root `ToolCall` validators from reaching a tool the provider
    /// policy denies. With no provider policy set every tool is permitted (the
    /// default), so this is a no-op on the common path.
    pub fn provider_policy_permits(&self, name: &str) -> bool {
        self.provider_policy.as_ref().is_none_or(|policy| {
            matches!(
                evaluate_provider_policy_equivalent(policy, name),
                policy::PolicyDecision::Allow
            )
        })
    }

    /// Set a context-based tag filter. Only tools whose tags overlap with these
    /// values will appear in `specs()`. Tools with no tags always pass through.
    pub fn set_context_filter(&mut self, tags: Vec<String>) {
        if tags.is_empty() {
            return;
        }
        self.context_filter = Some(tags);
        self.invalidate_cache();
    }

    /// Set the model context for this registry snapshot.
    pub fn set_active_context(&mut self, context: Option<String>) {
        let normalized = context
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        if self.active_context == normalized {
            return;
        }
        self.active_context = normalized;
        self.invalidate_cache();
    }

    /// Create a new ToolRegistry by cloning all tools except the named exclusions.
    ///
    /// The new registry shares the same `Arc<dyn Tool>` instances (cheap).
    /// Provider policy and context filter are also copied. Runtime state that
    /// is session-scoped stays fresh so cloned registries cannot leak task
    /// status, result routing, or spawn-only flags across sessions.
    pub fn snapshot_excluding(&self, exclude: &[&str]) -> Self {
        let tools: HashMap<String, Arc<dyn Tool>> = self
            .tools
            .iter()
            .filter(|(name, _)| !exclude.contains(&name.as_str()))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();

        let mut snapshot = Self {
            tools,
            // Carried, not reset: a snapshot that EXCLUDES the MCP tools would
            // otherwise replace the last owner and let the transport die when
            // the original registry drops. Cheap — one Arc per server.
            mcp_services: self.mcp_services.clone(),
            workspace_root: self.workspace_root.clone(),
            provider_policy: self.provider_policy.clone(),
            context_filter: self.context_filter.clone(),
            active_context: self.active_context.clone(),
            cached_specs: std::sync::Mutex::new(None),
            plugin_tools: self.plugin_tools.clone(),
            spawn_only: self.spawn_only.clone(),
            spawn_only_messages: self.spawn_only_messages.clone(),
            background_result_sender: None,
            // Fresh per-snapshot supervisor (deliberate per-subtree
            // isolation of task maps); #2055 review round 2 — the
            // REGISTRATION observers are inherited right below so a nested
            // registration still reaches the goal-ledger recorder/settle
            // wiring installed on the parent.
            supervisor: Arc::new(TaskSupervisor::new()),
            spawn_only_invoked: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            live_catalog: Arc::new(std::sync::Mutex::new(Vec::new())),
            session_key: None,
            output_dir_hint: self.output_dir_hint.clone(),
            tool_timeout_secs: self.tool_timeout_secs,
            // RFC-1 fixup (codex P1): propagate internal-hidden markers
            // onto per-turn snapshots so the per-turn registry observes
            // the same invariants as the parent (mofa_make targets stay
            // hidden from `specs()`).
            internal_hidden: self.internal_hidden.clone(),
            // #1607: carry the parent's sandbox onto the snapshot so a
            // snapshot-derived registry (used by `rebind_cwd_with_permissions`)
            // keeps a real sandbox by default. `rebind_cwd_with_permissions`
            // overwrites it below with the sandbox for the rebound cwd, but a
            // plain `snapshot_excluding` caller still observes the same
            // confinement as the parent.
            sandbox: self.sandbox.clone(),
        };
        // #1148 codex P2: the cloned `tool_search` / `tool_suggest`
        // Arcs still point to the PARENT's catalog cell. Re-register
        // fresh instances bound to the snapshot's own cell so search
        // reflects the snapshot's (possibly filtered) tool surface,
        // not the parent's. The `register` call below also fires
        // `refresh_live_catalog` via `invalidate_cache`.
        if snapshot.tools.contains_key("tool_search") {
            let cell = snapshot.live_catalog_handle();
            snapshot.register(ToolSearchTool::new(cell));
        }
        if snapshot.tools.contains_key("tool_suggest") {
            let cell = snapshot.live_catalog_handle();
            snapshot.register(ToolSuggestTool::new(cell));
        }
        // #2055 review round 2 — the fresh supervisor keeps its task map
        // isolated per-subtree, but the REGISTRATION observers (`on_register`
        // + the named `on_change_listeners`) ride along so goal-ledger task
        // rows cover registrations on the snapshot. Wake callbacks
        // (`on_change`/`on_failure`/`on_terminal`) stay per-instance.
        snapshot
            .supervisor
            .inherit_registration_observers(&self.supervisor);
        snapshot.refresh_live_catalog();
        snapshot
    }

    // -- Cache management ---------------------------------------------------

    /// Clear the cached specs (called by mutation methods with &mut self).
    fn invalidate_cache(&mut self) {
        // &mut self guarantees exclusive access, so get_mut() bypasses the mutex.
        *self
            .cached_specs
            .get_mut()
            .unwrap_or_else(|e| e.into_inner()) = None;
        // #1148 codex P2: refresh the live catalog cell so the
        // `tool_search` / `tool_suggest` discovery surface sees this
        // mutation. Every existing mutation site already calls
        // `invalidate_cache`, so threading the refresh through here
        // covers all of them in one shot.
        self.refresh_live_catalog();
    }

    /// Execute a tool by name.
    ///
    /// Respects provider policy: tools hidden from `specs()` are also blocked
    /// from execution. This prevents an LLM from calling tools it shouldn't
    /// have access to.
    ///
    /// Delegates to [`ToolRegistry::execute_with_context`] with the zero-value
    /// [`ToolContext`] so legacy callers continue to work unchanged.
    pub async fn execute(&self, name: &str, args: &serde_json::Value) -> Result<ToolResult> {
        let ctx = super::ToolContext::zero();
        self.execute_with_context(&ctx, name, args).await
    }

    /// Execute a tool by name with a typed [`ToolContext`].
    ///
    /// Migrated tools override [`super::Tool::execute_with_context`] and will
    /// see the caller's context; unmigrated tools fall back to the default
    /// trait impl which delegates to [`super::Tool::execute`].
    pub async fn execute_with_context(
        &self,
        ctx: &super::ToolContext,
        name: &str,
        args: &serde_json::Value,
    ) -> Result<ToolResult> {
        if let Some(ref policy) = self.provider_policy {
            if let policy::PolicyDecision::Deny { reason } =
                evaluate_provider_policy_equivalent(policy, name)
            {
                eyre::bail!("tool '{}' denied by provider policy ({})", name, reason);
            }
        }

        // Reject oversized arguments (1 MB limit).
        const MAX_ARGS_SIZE: usize = 1_048_576;
        let args_size = estimate_json_size(args);
        if args_size > MAX_ARGS_SIZE {
            eyre::bail!(
                "tool '{}' arguments too large: ~{} bytes (max {})",
                name,
                args_size,
                MAX_ARGS_SIZE
            );
        }

        let tool = self
            .tools
            .get(name)
            .ok_or_else(|| eyre::eyre!("unknown tool: {}", name))?;

        // Layer 2 (mini5 soak): isolate a tool panic at this single dispatch
        // boundary. A panic inside a tool used to unwind through the session
        // actor's task, killing the actor AND every in-process sub-agent it had
        // spawned (which then got stamped "orphaned across restart"). Catching
        // it here degrades the panic to a failed ToolResult: the caller sees a
        // clean tool error and the actor — plus its sub-agents — keeps running.
        // `catch_unwind` relies on unwind (the default profile). `AssertUnwindSafe`
        // is sound because on panic we DISCARD the tool's future/state entirely
        // and return a fresh error; nothing from the poisoned call is reused.
        //
        // Timeout and panic-isolation compose: a tool can panic OR time out,
        // and both degrade to a failed ToolResult. The timeout wraps the
        // catch_unwind so an elapsed timeout drops the (possibly panicking)
        // tool future entirely and returns a fresh failure.
        let invocation = tool.execute_with_context(ctx, args);
        let guarded = futures::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(invocation));

        // #1 — human-wait exemption. A tool that blocks on human input (e.g.
        // `ask_user_question` awaiting the requester, mirroring the approval
        // gate) must NOT be killed by the dispatch timeout: a human may take
        // longer than any finite ceiling, and firing the timeout would drop
        // the requester's receiver and leak the pending store entry forever.
        // Skip the timeout wrap entirely for these tools — the turn-interrupt
        // drain (which resolves the waiter as `Cancelled`) is the correct
        // cancellation path, not a fixed timeout. Panic isolation still
        // applies. This matches how the approval-blocking `shell` gate is not
        // double-killed by the tool timeout.
        if tool.blocks_on_human_input() {
            return match guarded.await {
                Ok(result) => result,
                Err(panic) => Ok(panic_to_failed_result(name, panic)),
            };
        }

        // Gap 3.3: per-tool execution timeout. A hung foreground tool used to
        // block the caller's turn (the session actor's "10-min opaque
        // pipeline" / hung-tool class) indefinitely — the agent loop's
        // per-batch timeout only guards the agent::execution dispatch path,
        // leaving direct registry callers (serve/API tool path, workspace
        // contract auto-send) unbounded. This dispatch-boundary timeout
        // bounds EVERY caller. The per-tool override
        // (`Tool::execution_timeout_secs`) wins when present; otherwise the
        // registry's generous global backstop applies. `spawn_only` tools are
        // intercepted/backgrounded earlier and never reach this path.
        let timeout_secs = tool
            .execution_timeout_secs()
            .unwrap_or(self.tool_timeout_secs)
            .max(1);
        let timeout = std::time::Duration::from_secs(timeout_secs);

        match tokio::time::timeout(timeout, guarded).await {
            Ok(Ok(result)) => result,
            Ok(Err(panic)) => Ok(panic_to_failed_result(name, panic)),
            Err(_elapsed) => {
                tracing::error!(
                    tool = name,
                    timeout_secs,
                    "tool execution timed out — degraded to a failed tool error; \
                     caller turn preserved"
                );
                Ok(ToolResult {
                    output: format!("Tool '{name}' timed out after {timeout_secs}s"),
                    success: false,
                    ..Default::default()
                })
            }
        }
    }
}

/// Degrade a caught tool panic to a failed [`ToolResult`] (Layer-2 panic
/// isolation, mini5 soak). Shared by the timeout-guarded dispatch arm and the
/// human-wait (timeout-exempt) dispatch arm so both isolate a panic
/// identically.
fn panic_to_failed_result(name: &str, panic: Box<dyn std::any::Any + Send>) -> ToolResult {
    let detail = panic
        .downcast_ref::<&str>()
        .map(|s| (*s).to_string())
        .or_else(|| panic.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown panic".to_string());
    tracing::error!(
        tool = name,
        panic = %detail,
        "tool execution panicked — isolated to a tool error; session actor preserved"
    );
    ToolResult {
        output: format!("tool '{name}' failed (internal error): {detail}"),
        success: false,
        ..Default::default()
    }
}

impl ToolRegistry {
    /// Create a registry with built-in tools for the given working directory.
    pub fn with_builtins(cwd: impl AsRef<Path>) -> Self {
        Self::with_builtins_and_sandbox(cwd, Box::new(NoSandbox))
    }

    /// Create a registry with built-in tools and a custom sandbox for shell commands.
    pub fn with_builtins_and_sandbox(cwd: impl AsRef<Path>, sandbox: Box<dyn Sandbox>) -> Self {
        let permissions = EffectivePermissions::workspace_write();
        Self::with_builtins_and_permissions(cwd, sandbox, permissions)
    }

    /// Create a registry with built-in tools under explicit runtime permissions.
    pub fn with_builtins_and_permissions(
        cwd: impl AsRef<Path>,
        sandbox: Box<dyn Sandbox>,
        permissions: EffectivePermissions,
    ) -> Self {
        let cwd = cwd.as_ref();
        let mut registry = Self::new();
        registry.workspace_root = Some(cwd.to_path_buf());
        let sandbox: Arc<dyn Sandbox> = Arc::from(sandbox);
        // #1607: store the session sandbox so the Agent-internal project-root
        // validator path can confine command validators to it (see
        // `Self::sandbox`). Kept in lockstep with the shell/exec/bash tools
        // registered just below.
        registry.sandbox = sandbox.clone();
        registry.register(
            ShellTool::new(cwd)
                .with_shared_sandbox(sandbox.clone())
                .with_policy(permissions.shell_command_policy())
                .with_approval_policy(permissions.approval_policy),
        );
        registry.register(
            ExecCommandTool::new(cwd, sandbox.clone())
                .with_filesystem_scope(permissions.filesystem_scope)
                .with_policy(permissions.shell_command_policy())
                .with_approval_policy(permissions.approval_policy)
                .with_bash_file_writes(permissions.bash_file_writes),
        );
        // #1172: Codex-compatible `bash` alias. Shares command policy /
        // approval policy / sandbox with `shell` and `exec_command`, so a
        // deny in one path denies in all three.
        registry.register(
            super::coding_tools::BashTool::new(cwd, sandbox)
                .with_filesystem_scope(permissions.filesystem_scope)
                .with_policy(permissions.shell_command_policy())
                .with_approval_policy(permissions.approval_policy)
                .with_bash_file_writes(permissions.bash_file_writes),
        );
        registry.register(WriteStdinTool);
        registry.register(UpdatePlanTool);
        registry.register(RequestUserInputTool);
        // UPCR-2026-023: structured AskUserQuestion. The synchronous,
        // answer-routed superset of `request_user_input`.
        registry.register(AskUserQuestionTool::new());
        registry.register(SpawnAgentTool::new());
        // #1172: Codex-compatible `delegate` one-call wrapper. The default
        // instance has no spawn_agent bound — `register("spawn")` swaps
        // both `spawn_agent` and `delegate` in lockstep, so when the
        // session runtime wires a native spawn delegate this alias picks
        // up the live one.
        registry.register(super::coding_tools::DelegateAliasTool::new());
        registry.register(SendInputTool);
        registry.register(ResumeAgentTool);
        registry.register(WaitAgentTool);
        registry.register(CloseAgentTool);
        registry
            .register(ReadFileTool::new(cwd).with_filesystem_scope(permissions.filesystem_scope));
        registry.register(
            ApplyPatchTool::new(cwd)
                .with_filesystem_scope(permissions.filesystem_scope)
                .with_file_access(permissions.file_access),
        );
        registry.register(
            DiffEditTool::new(cwd)
                .with_filesystem_scope(permissions.filesystem_scope)
                .with_file_access(permissions.file_access),
        );
        registry.register(
            EditFileTool::new(cwd)
                .with_filesystem_scope(permissions.filesystem_scope)
                .with_file_access(permissions.file_access),
        );
        registry.register(
            WriteFileTool::new(cwd)
                .with_filesystem_scope(permissions.filesystem_scope)
                .with_file_access(permissions.file_access),
        );
        registry.register(GlobTool::new(cwd).with_filesystem_scope(permissions.filesystem_scope));
        registry.register(GrepTool::new(cwd));
        registry
            .register(ListDirTool::new(cwd).with_filesystem_scope(permissions.filesystem_scope));
        registry.register(WebSearchTool::new());
        registry.register(WebFetchTool::new());
        registry.register(BrowserTool::new());
        registry.register(CheckWorkspaceContractTool::new(cwd));
        registry.register(WorkspaceLogTool::new(cwd));
        registry.register(WorkspaceShowTool::new(cwd));
        registry.register(WorkspaceDiffTool::new(cwd));
        // #1772 (lite): project static-check with compact diagnostics.
        // Shares the session sandbox with shell/exec/bash — `cargo check`
        // executes build.rs/proc-macros (project-controlled code), so it
        // must be confined like any shell command (#1607).
        registry.register(super::CheckTool::new(cwd).with_shared_sandbox(registry.sandbox.clone()));
        #[cfg(feature = "git")]
        registry.register(super::GitTool::new(cwd));
        #[cfg(feature = "ast")]
        registry.register(CodeStructureTool::new(cwd));
        // #972 / M14-B P1: `view_image`, `tool_search`, `tool_suggest`.
        //
        // `view_image` inherits the workspace filesystem scope so it can only
        // read images inside the active project.
        registry.register(
            ViewImageTool::new(cwd)
                .with_filesystem_scope(permissions.filesystem_scope)
                .with_file_access(permissions.file_access),
        );
        // #1148 codex P2: pass the LIVE shared catalog cell instead
        // of a frozen Vec snapshot. The registry refreshes the cell
        // on every mutation via `refresh_live_catalog` (called from
        // `invalidate_cache` / `invalidate_cache_shared`), so the
        // discovery surface reflects post-builtin registrations
        // (chat/gateway/profile setup, MCP/plugin/pipeline/memory).
        let catalog_cell = registry.live_catalog_handle();
        registry.register(ToolSearchTool::new(catalog_cell.clone()));
        registry.register(ToolSuggestTool::new(catalog_cell));
        // #1149 / M14-B P2: register the canonical Codex
        // `image_generation` entry. It currently returns a typed
        // `coding_tool_unsupported` envelope because no native or
        // skill backend is bound; the wire-level contract is complete
        // so the model gets a clean error instead of a "tool not
        // found" miss. Follow-up to wire a real backend lives on
        // issue #1149.
        registry.register(ImageGenerationTool::new());
        // Final refresh so the catalog reflects the just-registered
        // search/suggest tools too (cosmetic — they show up in their
        // own search results).
        registry.refresh_live_catalog();
        registry
    }

    /// Snapshot of every currently model-visible tool as a [`ToolCatalogEntry`]
    /// list. Used by `with_builtins` to wire `tool_search` / `tool_suggest`
    /// against the effective coding tool contract.
    pub fn catalog_snapshot(&self) -> Vec<ToolCatalogEntry> {
        self.tools
            .values()
            // RFC-1 fixup (codex P1): exclude internal-hidden tools from
            // tool_search / tool_suggest discovery too — the LLM cannot
            // call them directly, advertising them would be misleading.
            .filter(|tool| !self.internal_hidden.contains(tool.name()))
            .filter(|tool| {
                self.provider_policy.as_ref().is_none_or(|policy| {
                    provider_policy_allows_equivalent_with_tags(policy, tool.name(), tool.tags())
                })
            })
            .filter(|tool| {
                self.context_filter.as_ref().is_none_or(|tags| {
                    let tool_tags = tool.tags();
                    tool_tags.is_empty()
                        || tool_tags.iter().any(|tag| tags.contains(&tag.to_string()))
                })
            })
            .filter(|tool| self.active_context_allows(tool.as_ref()))
            .map(|tool| {
                ToolCatalogEntry::new(
                    tool.name(),
                    tool.description(),
                    tool.tags().iter().map(|t| (*t).to_string()).collect(),
                )
            })
            .collect()
    }

    /// #1148 codex P2 — return the shared live-catalog cell so
    /// `ToolSearchTool` / `ToolSuggestTool` see post-mutation tool
    /// state. Cloning the `Arc` is cheap; readers acquire the inner
    /// Mutex briefly at execute time.
    pub fn live_catalog_handle(&self) -> Arc<std::sync::Mutex<Vec<ToolCatalogEntry>>> {
        self.live_catalog.clone()
    }

    /// #1148 codex P2 — rebuild the live catalog from the current
    /// visible tool set. Called from every mutation site
    /// (`register`, `register_arc`, `unregister`, `apply_policy`,
    /// `mark_spawn_only`, ...). Idempotent + cheap; the inner Vec
    /// is replaced wholesale to avoid stale entries.
    pub(crate) fn refresh_live_catalog(&self) {
        let snapshot = self.catalog_snapshot();
        if let Ok(mut guard) = self.live_catalog.lock() {
            *guard = snapshot;
        }
    }

    /// Tool names that are bound to a working directory (cwd / base_dir).
    /// Used by `rebind_cwd()` to re-register these tools with a new workspace path.
    pub const CWD_BOUND_TOOLS: &'static [&'static str] = &[
        "shell",
        "exec_command",
        // #1172: `bash` alias holds a workspace base_dir for workdir
        // resolution and must follow `rebind_cwd` so a re-scoped session
        // doesn't keep running commands under the old project root.
        "bash",
        "read_file",
        "write_file",
        "apply_patch",
        "edit_file",
        "diff_edit",
        "glob",
        "grep",
        "list_dir",
        "check_workspace_contract",
        "workspace_log",
        "workspace_show",
        "workspace_diff",
        // #1772 (lite): `check` detects the project (Cargo.toml / tsconfig /
        // go.mod) at its bound workspace root and runs the checker there, so
        // a re-scoped session must re-register it against the new root.
        "check",
        // #972 / M14-B P1: `view_image` reads files from the workspace and
        // must follow `rebind_cwd` so a session targeting a new project root
        // does not leak previously bound paths.
        "view_image",
        #[cfg(feature = "git")]
        "git",
        #[cfg(feature = "ast")]
        "code_structure",
    ];

    /// Create a copy of this registry with all cwd-bound tools re-registered
    /// to use a new working directory and sandbox. Non-cwd tools (web_search,
    /// web_fetch, browser, MCP, plugins, etc.) are preserved via Arc cloning.
    pub fn rebind_cwd(&self, cwd: impl AsRef<Path>, sandbox: Box<dyn Sandbox>) -> Self {
        self.rebind_cwd_with_permissions(cwd, sandbox, EffectivePermissions::workspace_write())
    }

    /// Like [`Self::rebind_cwd`], but applies explicit runtime permissions.
    pub fn rebind_cwd_with_permissions(
        &self,
        cwd: impl AsRef<Path>,
        sandbox: Box<dyn Sandbox>,
        permissions: EffectivePermissions,
    ) -> Self {
        let cwd = cwd.as_ref();
        // Clone everything except cwd-bound tools and the dynamic-discovery
        // tools, which hold a snapshot of the *previous* catalog and would
        // otherwise advertise stale tool descriptions after a rebind.
        let mut exclude: Vec<&str> = Self::CWD_BOUND_TOOLS.to_vec();
        exclude.extend_from_slice(&["tool_search", "tool_suggest"]);
        let mut registry = self.snapshot_excluding(&exclude);
        registry.workspace_root = Some(cwd.to_path_buf());
        let sandbox: Arc<dyn Sandbox> = Arc::from(sandbox);
        // #1607: store the sandbox for the rebound cwd (overwriting the
        // parent's, carried by `snapshot_excluding`) so the project-root
        // validator path confines command validators to the same sandbox as
        // the shell/exec/bash tools re-registered just below.
        registry.sandbox = sandbox.clone();
        // Re-register cwd-bound tools with the new workspace
        registry.register(
            ShellTool::new(cwd)
                .with_shared_sandbox(sandbox.clone())
                .with_policy(permissions.shell_command_policy())
                .with_approval_policy(permissions.approval_policy),
        );
        registry.register(
            ExecCommandTool::new(cwd, sandbox.clone())
                .with_filesystem_scope(permissions.filesystem_scope)
                .with_policy(permissions.shell_command_policy())
                .with_approval_policy(permissions.approval_policy)
                .with_bash_file_writes(permissions.bash_file_writes),
        );
        // #1172: re-register the `bash` alias against the new cwd.
        registry.register(
            super::coding_tools::BashTool::new(cwd, sandbox)
                .with_filesystem_scope(permissions.filesystem_scope)
                .with_policy(permissions.shell_command_policy())
                .with_approval_policy(permissions.approval_policy)
                .with_bash_file_writes(permissions.bash_file_writes),
        );
        registry
            .register(ReadFileTool::new(cwd).with_filesystem_scope(permissions.filesystem_scope));
        registry.register(
            ApplyPatchTool::new(cwd)
                .with_filesystem_scope(permissions.filesystem_scope)
                .with_file_access(permissions.file_access),
        );
        registry.register(
            DiffEditTool::new(cwd)
                .with_filesystem_scope(permissions.filesystem_scope)
                .with_file_access(permissions.file_access),
        );
        registry.register(
            EditFileTool::new(cwd)
                .with_filesystem_scope(permissions.filesystem_scope)
                .with_file_access(permissions.file_access),
        );
        registry.register(
            WriteFileTool::new(cwd)
                .with_filesystem_scope(permissions.filesystem_scope)
                .with_file_access(permissions.file_access),
        );
        registry.register(GlobTool::new(cwd).with_filesystem_scope(permissions.filesystem_scope));
        registry.register(GrepTool::new(cwd));
        registry
            .register(ListDirTool::new(cwd).with_filesystem_scope(permissions.filesystem_scope));
        registry.register(CheckWorkspaceContractTool::new(cwd));
        registry.register(WorkspaceLogTool::new(cwd));
        registry.register(WorkspaceShowTool::new(cwd));
        registry.register(WorkspaceDiffTool::new(cwd));
        // #1772 (lite): `check` detects the project type from the workspace
        // root, so it must follow `rebind_cwd` like the other cwd-bound
        // tools — and it re-binds to the NEW session sandbox stored just
        // above, in lockstep with the shell/exec/bash re-registrations.
        registry.register(super::CheckTool::new(cwd).with_shared_sandbox(registry.sandbox.clone()));
        #[cfg(feature = "git")]
        registry.register(super::GitTool::new(cwd));
        #[cfg(feature = "ast")]
        registry.register(CodeStructureTool::new(cwd));
        // #972 / M14-B P1: re-register cwd-bound `view_image` and refresh the
        // dynamic-discovery catalog so `tool_search` / `tool_suggest` reflect
        // the rebound workspace's tool surface.
        registry.register(
            ViewImageTool::new(cwd)
                .with_filesystem_scope(permissions.filesystem_scope)
                .with_file_access(permissions.file_access),
        );
        // #1148 codex P2: live shared catalog cell — see `with_builtins`.
        let catalog_cell = registry.live_catalog_handle();
        registry.register(ToolSearchTool::new(catalog_cell.clone()));
        registry.register(ToolSuggestTool::new(catalog_cell));
        registry.refresh_live_catalog();
        // yolo GAP #2: plugin tools are carried across the snapshot unchanged,
        // so thread the session approval context into them here — the same
        // permissions the built-in shell/coding tools were just re-registered
        // with above. Under a `never`/DangerFullAccess session this makes the
        // manifest risk gate honor `ApprovalPolicy` instead of always
        // prompting.
        registry.apply_permissions_to_plugin_tools(permissions);
        registry
    }

    /// Re-bind all plugin tools to a new work directory.
    ///
    /// Creates copies of each `PluginTool` with the given work_dir so that
    /// per-session output (e.g. voice profiles) lands inside the user's
    /// workspace where the agent's sandboxed tools can access it.
    pub fn rebind_plugin_work_dirs(&mut self, work_dir: &Path) {
        use crate::plugins::PluginTool;
        let replacements: Vec<_> = self
            .tools
            .iter()
            .filter_map(|(name, tool)| {
                tool.as_any()
                    .downcast_ref::<PluginTool>()
                    .map(|pt| (name.clone(), pt.clone_with_work_dir(work_dir.to_path_buf())))
            })
            .collect();
        for (name, new_tool) in replacements {
            self.tools.insert(name, Arc::new(new_tool));
        }
    }

    /// yolo GAP #2: thread the session's approval context into every
    /// `PluginTool` so the manifest risk gate honors `ApprovalPolicy` the
    /// same way the shell/coding tools do.
    ///
    /// `rebind_cwd_with_permissions` re-registers built-in tools with the
    /// session `EffectivePermissions`, but plugin tools are carried across
    /// the snapshot unchanged — so a `high`/`critical`-risk plugin would
    /// otherwise still prompt in a `never`/DangerFullAccess session. This
    /// pass replaces each plugin tool with a copy that carries:
    ///   * `approval_policy` — under `Never`, the risk gate denies without
    ///     prompting (parity with shell.rs);
    ///   * `auto_approve_high_risk` — set from `permissions.is_dangerous()`,
    ///     so a DangerFullAccess ("yolo") context auto-allows the gate
    ///     (parity with the shell tools swapping in `AllowAllPolicy`).
    ///
    /// Called automatically at the end of `rebind_cwd_with_permissions`.
    pub fn apply_permissions_to_plugin_tools(&mut self, permissions: EffectivePermissions) {
        use crate::plugins::PluginTool;
        let approval_policy = permissions.approval_policy;
        let auto_approve = permissions.is_dangerous();
        let replacements: Vec<_> = self
            .tools
            .iter()
            .filter_map(|(name, tool)| {
                tool.as_any().downcast_ref::<PluginTool>().map(|pt| {
                    (
                        name.clone(),
                        pt.clone_with_permissions(approval_policy, auto_approve),
                    )
                })
            })
            .collect();
        for (name, new_tool) in replacements {
            self.tools.insert(name, Arc::new(new_tool));
        }
    }

    /// Re-register builtin configurable tools with a ToolConfigStore.
    ///
    /// Tools already registered by `with_builtins_and_sandbox()` are replaced
    /// with config-aware instances. Also registers the `configure_tool` tool.
    pub fn inject_tool_config(&mut self, config: Arc<ToolConfigStore>) {
        if self.tools.contains_key("web_search") {
            self.register(WebSearchTool::new().with_config(config.clone()));
        }
        if self.tools.contains_key("web_fetch") {
            self.register(WebFetchTool::new().with_config(config.clone()));
        }
        if self.tools.contains_key("browser") {
            self.register(BrowserTool::new().with_config(config.clone()));
        }
        self.register(ConfigureToolTool::new(config));
    }
}

#[cfg(test)]
mod estimate_tests {
    use super::*;

    #[test]
    fn test_null() {
        assert_eq!(estimate_json_size(&serde_json::Value::Null), 4);
    }

    #[test]
    fn test_bool() {
        assert_eq!(estimate_json_size(&serde_json::json!(true)), 4);
        assert_eq!(estimate_json_size(&serde_json::json!(false)), 5);
    }

    #[test]
    fn test_number() {
        assert_eq!(estimate_json_size(&serde_json::json!(42)), 2);
        assert_eq!(estimate_json_size(&serde_json::json!(2.72)), 4);
    }

    #[test]
    fn test_string_simple() {
        // "hello" -> 5 chars + 2 quotes = 7
        assert_eq!(estimate_json_size(&serde_json::json!("hello")), 7);
    }

    #[test]
    fn test_string_with_escapes() {
        // "a\"b" has 3 chars + 1 escape overhead + 2 quotes = 6
        assert_eq!(estimate_json_size(&serde_json::json!("a\"b")), 6);
        // "a\nb" has 3 chars + 1 escape + 2 quotes = 6
        assert_eq!(estimate_json_size(&serde_json::json!("a\nb")), 6);
    }

    #[test]
    fn test_empty_array() {
        assert_eq!(estimate_json_size(&serde_json::json!([])), 2);
    }

    #[test]
    fn test_array_with_elements() {
        // [1,2,3] = 2 brackets + 3 numbers (1+1+1) + 2 commas = 7
        assert_eq!(estimate_json_size(&serde_json::json!([1, 2, 3])), 7);
    }

    #[test]
    fn test_empty_object() {
        assert_eq!(estimate_json_size(&serde_json::json!({})), 2);
    }

    #[test]
    fn test_object_with_fields() {
        // {"a":1} = 2 braces + key(1) + 3 (quotes+colon) + value(1) = 7
        let v = serde_json::json!({"a": 1});
        assert_eq!(estimate_json_size(&v), 7);
    }

    #[test]
    fn test_nested_structure() {
        let v = serde_json::json!({"x": [1, 2]});
        // Outer: 2 + key(1+3) + inner array
        // Inner array: 2 + 1 + 1 + 1 comma = 5
        // Total: 2 + 4 + 5 = 11
        assert_eq!(estimate_json_size(&v), 11);
    }
}

#[cfg(test)]
mod tag_lookup_tests {
    use super::*;

    #[test]
    fn should_return_app_reply_tool_names_when_tag_matches() {
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let mut registry = ToolRegistry::new();
        registry.register(crate::tools::SendAppCardTool::new(tx.clone()));
        registry.register(crate::tools::MessageTool::new(tx));

        let names = registry.names_with_tag("app_reply");
        assert_eq!(names, vec!["send_app_card".to_string()]);
        assert!(registry.names_with_tag("no_such_tag").is_empty());
    }
}

#[cfg(test)]
mod cwd_isolation_tests {
    use super::*;
    use crate::sandbox::NoSandbox;

    #[tokio::test]
    async fn test_rebind_cwd_file_tools_reject_outside_paths() {
        let broad_cwd = std::path::Path::new("/tmp");
        let registry = ToolRegistry::with_builtins_and_sandbox(broad_cwd, Box::new(NoSandbox));

        let narrow_cwd = tempfile::tempdir().expect("create temp dir");
        let narrow = narrow_cwd.path();
        let rebound = registry.rebind_cwd(narrow, Box::new(NoSandbox));

        let inside_file = narrow.join("allowed.txt");
        std::fs::write(&inside_file, "hello").expect("write test file");

        let result = rebound
            .execute("read_file", &serde_json::json!({"path": "allowed.txt"}))
            .await;
        assert!(result.is_ok(), "read inside narrow cwd should work");
        let tr = result.unwrap();
        assert!(tr.success, "read_file should succeed: {}", tr.output);

        let result = rebound
            .execute(
                "read_file",
                &serde_json::json!({"path": "../../etc/passwd"}),
            )
            .await;
        assert!(result.is_ok(), "should not return transport error");
        let tr = result.unwrap();
        assert!(
            !tr.success,
            "read_file with traversal should be rejected: {}",
            tr.output
        );

        let result = rebound
            .execute(
                "write_file",
                &serde_json::json!({
                    "path": "../escape.txt",
                    "content": "pwned"
                }),
            )
            .await;
        assert!(result.is_ok());
        let tr = result.unwrap();
        assert!(
            !tr.success,
            "write_file outside narrow cwd should be rejected: {}",
            tr.output
        );

        let result = rebound
            .execute("glob", &serde_json::json!({"pattern": "*.txt"}))
            .await;
        assert!(result.is_ok());
        let tr = result.unwrap();
        assert!(tr.success, "glob inside cwd should work: {}", tr.output);

        let result = rebound
            .execute("list_dir", &serde_json::json!({"path": "."}))
            .await;
        assert!(result.is_ok());
        let tr = result.unwrap();
        assert!(tr.success, "list_dir inside cwd should work: {}", tr.output);

        let result = rebound
            .execute("list_dir", &serde_json::json!({"path": "../../"}))
            .await;
        assert!(result.is_ok());
        let tr = result.unwrap();
        assert!(
            !tr.success,
            "list_dir with traversal should be rejected: {}",
            tr.output
        );
    }

    #[tokio::test]
    async fn test_rebind_cwd_preserves_non_cwd_tools() {
        let initial_cwd = tempfile::tempdir().expect("create temp dir");
        let registry =
            ToolRegistry::with_builtins_and_sandbox(initial_cwd.path(), Box::new(NoSandbox));

        let new_cwd = tempfile::tempdir().expect("create temp dir");
        let rebound = registry.rebind_cwd(new_cwd.path(), Box::new(NoSandbox));

        assert!(
            rebound.get("web_fetch").is_some(),
            "web_fetch should survive rebind"
        );
        assert!(
            rebound.get("web_search").is_some(),
            "web_search should survive rebind"
        );
        assert!(
            rebound.get("read_file").is_some(),
            "read_file should be re-registered"
        );
        assert!(
            rebound.get("shell").is_some(),
            "shell should be re-registered"
        );
        assert!(
            rebound.get("write_file").is_some(),
            "write_file should be re-registered"
        );
    }

    #[test]
    fn test_rebind_cwd_isolates_session_runtime_state() {
        let initial_cwd = tempfile::tempdir().expect("create temp dir");
        let mut registry =
            ToolRegistry::with_builtins_and_sandbox(initial_cwd.path(), Box::new(NoSandbox));
        registry.set_session_key("api:base-session".to_string());
        registry.mark_spawn_only_invoked();
        let base_task = registry.register_task("search", "call-base");

        let new_cwd = tempfile::tempdir().expect("create temp dir");
        let rebound = registry.rebind_cwd(new_cwd.path(), Box::new(NoSandbox));

        assert!(
            rebound.supervisor().get_task(&base_task).is_none(),
            "rebound registry must not inherit another session's task ledger"
        );
        assert!(
            !rebound.spawn_only_was_invoked(),
            "spawn-only invocation state is per agent run/session"
        );

        let rebound_task = rebound.register_task("search", "call-rebound");
        let rebound_task = rebound
            .supervisor()
            .get_task(&rebound_task)
            .expect("rebound task should be tracked");
        assert!(
            rebound_task.session_key.is_none(),
            "session key must be supplied by the new session actor, not inherited"
        );
    }

    #[tokio::test]
    async fn should_register_check_tool_and_rebind_its_cwd() {
        let initial_cwd = tempfile::tempdir().expect("create temp dir");
        let registry =
            ToolRegistry::with_builtins_and_sandbox(initial_cwd.path(), Box::new(NoSandbox));
        assert!(
            registry.get("check").is_some(),
            "check must be a builtin tool"
        );

        // `check` is cwd-bound: after a rebind it must detect the project at
        // the NEW workspace root (the empty new cwd → "no supported project"),
        // not the old one.
        let new_cwd = tempfile::tempdir().expect("create temp dir");
        std::fs::write(initial_cwd.path().join("go.mod"), "module old").unwrap();
        let rebound = registry.rebind_cwd(new_cwd.path(), Box::new(NoSandbox));
        let tr = rebound
            .execute("check", &serde_json::json!({}))
            .await
            .expect("check dispatch");
        assert!(tr.success, "no-project answer is a success: {}", tr.output);
        assert!(
            tr.output.contains("no supported project detected"),
            "rebound check must look at the NEW cwd: {}",
            tr.output
        );
    }

    /// Review #1772 (high): `check` spawns cargo/tsc/go, which execute
    /// project-controlled code (build.rs / proc-macros), so BOTH registry
    /// constructors must hand it the session sandbox — same lockstep as
    /// shell/exec/bash. The marker sandbox replaces the wrapped command
    /// with an echo, so the marker in the output proves the checker went
    /// through `Sandbox::wrap_command` instead of a direct host spawn.
    #[cfg(unix)]
    #[tokio::test]
    async fn should_confine_check_tool_to_session_sandbox_on_build_and_rebind() {
        struct MarkerSandbox;
        impl Sandbox for MarkerSandbox {
            fn wrap_command(
                &self,
                command: &str,
                cwd: &std::path::Path,
            ) -> tokio::process::Command {
                let mut cmd = tokio::process::Command::new("sh");
                cmd.arg("-c")
                    .arg(format!("echo \"SANDBOX-WRAPPED: {command}\"; exit 7"))
                    .current_dir(cwd);
                cmd
            }
        }

        // `cargo` resolves from PATH — always present under `cargo test` —
        // but the marker sandbox substitutes the command, so no real
        // checker ever runs.
        let cwd = tempfile::tempdir().expect("create temp dir");
        std::fs::write(cwd.path().join("Cargo.toml"), b"[package]").unwrap();

        let registry = ToolRegistry::with_builtins_and_sandbox(cwd.path(), Box::new(MarkerSandbox));
        let tr = registry
            .execute("check", &serde_json::json!({}))
            .await
            .expect("check dispatch");
        assert!(
            tr.output.contains("SANDBOX-WRAPPED"),
            "with_builtins must confine check to the session sandbox: {}",
            tr.output
        );

        let rebound = registry.rebind_cwd(cwd.path(), Box::new(MarkerSandbox));
        let tr = rebound
            .execute("check", &serde_json::json!({}))
            .await
            .expect("check dispatch");
        assert!(
            tr.output.contains("SANDBOX-WRAPPED"),
            "rebind_cwd must re-hand the session sandbox to check: {}",
            tr.output
        );
    }

    #[test]
    fn set_workspace_root_records_path_for_session_tool_registry_fallback() {
        let mut reg = ToolRegistry::new();
        assert!(
            reg.workspace_root().is_none(),
            "fresh registry must not advertise a workspace_root"
        );
        let cwd = std::path::PathBuf::from("/tmp/test-default-cwd");
        reg.set_workspace_root(cwd.clone());
        assert_eq!(reg.workspace_root(), Some(cwd.as_path()));
    }
}

#[cfg(test)]
mod registry_dispatch_tests {
    use super::*;
    use std::path::PathBuf;

    // RFC-0 (#1289): LRU tool-lifecycle deferral was removed, so this helper no
    // longer carries the old `(max_active, idle_threshold)` tuning knobs.
    fn make_registry() -> ToolRegistry {
        ToolRegistry::with_builtins(PathBuf::from("/tmp"))
    }

    #[test]
    fn spawn_only_message_uses_runtime_output_dir_hint() {
        let mut reg = make_registry();
        reg.mark_spawn_only("mofa_slides", None);
        reg.set_output_dir_hint("/tmp/octos-profile/skill-output");

        let msg = reg.spawn_only_message("mofa_slides");

        assert!(msg.contains("Output directory: /tmp/octos-profile/skill-output/"));
    }

    #[test]
    fn spawn_only_handle_message_returns_task_handle_envelope() {
        let mut reg = make_registry();
        reg.mark_spawn_only("search", None);
        reg.set_output_dir_hint("/tmp/octos/skill-output");

        let payload = reg.spawn_only_handle_message(
            "search",
            "task_abc123",
            &["research/_report.md".to_string()],
        );

        let value: serde_json::Value = serde_json::from_str(&payload)
            .expect("spawn_only_handle_message must produce valid JSON");
        assert_eq!(value["ok"], true);
        assert_eq!(value["task_handle"], "task_abc123");
        assert_eq!(value["read_with"], "read_task_output");
        assert!(
            value["expected_files"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v == "research/_report.md")
        );
        // The summary must point the LLM at read_task_output rather than
        // dumping content into context.
        assert!(
            value["summary"]
                .as_str()
                .unwrap()
                .contains("read_task_output")
        );
    }

    #[test]
    fn is_tool_visible_respects_provider_policy_deny() {
        // Codex round 2 P2: visibility helper must mirror the same filters
        // `specs()` applies, so the spawn_only intercept does not advertise
        // a tool the provider policy hid from the LLM's tool list.
        let mut reg = make_registry();
        // After make_registry, "shell" exists.
        assert!(reg.is_tool_visible("shell"));

        let policy = ToolPolicy {
            deny: vec!["shell".to_string()],
            ..Default::default()
        };
        reg.set_provider_policy(policy);

        assert!(
            !reg.is_tool_visible("shell"),
            "provider-policy-denied tools must not be reported as visible"
        );
    }

    #[tokio::test]
    async fn spawn_agent_execution_policy_is_equivalent_to_spawn_alias() {
        let mut reg = make_registry();
        reg.set_provider_policy(ToolPolicy {
            deny: vec!["spawn".to_owned()],
            ..Default::default()
        });
        assert!(
            !reg.is_tool_visible("spawn_agent"),
            "spawn_agent should be hidden when policy denies its backend spawn alias"
        );
        let denied = match reg
            .execute("spawn_agent", &serde_json::json!({ "message": "review" }))
            .await
        {
            Ok(result) => panic!(
                "spawn deny should deny spawn_agent alias, got: {}",
                result.output
            ),
            Err(error) => error,
        };
        assert!(denied.to_string().contains("denied by provider policy"));

        let mut reg = make_registry();
        reg.set_provider_policy(ToolPolicy {
            allow: vec!["spawn".to_owned()],
            ..Default::default()
        });
        assert!(
            reg.is_tool_visible("spawn_agent"),
            "spawn_agent should be visible when policy allows its backend spawn alias"
        );
        let allowed = reg
            .execute("spawn_agent", &serde_json::json!({ "message": "review" }))
            .await
            .expect("spawn allow should allow spawn_agent alias to execute");
        assert!(
            !allowed.success,
            "the alias should pass policy, then fail only because the bare builtin registry has no native spawn delegate"
        );
        assert!(allowed.output.contains("native spawn delegate"));
    }

    #[test]
    fn is_tool_visible_returns_false_for_unregistered_tools() {
        let reg = make_registry();
        assert!(!reg.is_tool_visible("nope_does_not_exist"));
    }

    #[test]
    fn should_expose_a_noop_sandbox_when_registry_built_without_a_real_backend() {
        // #1607 (P1): `with_builtins` (and `Default`) install `NoSandbox`, so
        // the getter the project-root validator path calls must return a no-op
        // sandbox — `ValidatorRunner` then runs command validators' argv
        // directly, keeping host behavior unchanged where no backend exists.
        let reg = make_registry();
        assert!(
            reg.sandbox().is_noop(),
            "registry built without a real sandbox must expose a no-op sandbox"
        );
    }

    #[test]
    fn should_store_the_real_sandbox_handed_to_builtins_and_expose_it() {
        // #1607 (P1): a real (non-no-op) sandbox handed to the shell/exec/bash
        // tools must be STORED on the registry and surfaced via `sandbox()`,
        // not handed off and dropped — that is the handle the project-root
        // validator runner threads into `.with_sandbox(...)`.
        struct FakeRealSandbox;
        impl Sandbox for FakeRealSandbox {
            fn wrap_command(
                &self,
                command: &str,
                _cwd: &std::path::Path,
            ) -> tokio::process::Command {
                let mut c = tokio::process::Command::new("sh");
                c.arg("-c").arg(command);
                c
            }
            fn is_noop(&self) -> bool {
                false
            }
        }
        let reg = ToolRegistry::with_builtins_and_sandbox(
            PathBuf::from("/tmp"),
            Box::new(FakeRealSandbox),
        );
        assert!(
            !reg.sandbox().is_noop(),
            "a real sandbox handed to with_builtins_and_sandbox must be stored, not dropped"
        );
    }

    #[test]
    fn should_permit_all_tools_when_no_provider_policy_is_set() {
        // #1607 (P2): with no provider policy the permit predicate is a no-op,
        // so `MapToolDispatcher::from_registry` snapshots every tool.
        let reg = make_registry();
        assert!(reg.provider_policy_permits("shell"));
        assert!(reg.provider_policy_permits("read_file"));
    }

    #[test]
    fn should_deny_provider_policy_denied_tools_including_aliases() {
        // #1607 (P2): the permit predicate must mirror the deny-wins semantics
        // (with alias equivalence) that `execute` enforces, so a project-root
        // ToolCall validator can't reach a denied tool via the snapshot.
        let mut reg = make_registry();
        reg.set_provider_policy(ToolPolicy {
            deny: vec!["spawn".to_string()],
            ..Default::default()
        });
        // `spawn_agent` maps to the `spawn` alias, so denying `spawn` denies it.
        assert!(
            !reg.provider_policy_permits("spawn_agent"),
            "alias-equivalent denied tools must not pass the permit predicate"
        );
        // A tool outside the deny list still passes (allow list empty => allow).
        assert!(reg.provider_policy_permits("read_file"));
    }

    #[test]
    fn should_permit_only_allowlisted_tools_when_allow_list_is_set() {
        // #1607 (P2): a non-empty allow list means only listed (or
        // alias-equivalent) tools are permitted.
        let mut reg = make_registry();
        reg.set_provider_policy(ToolPolicy {
            allow: vec!["read_file".to_string()],
            ..Default::default()
        });
        assert!(reg.provider_policy_permits("read_file"));
        assert!(
            !reg.provider_policy_permits("shell"),
            "tools absent from a non-empty allow list must not pass the permit predicate"
        );
    }

    /// SECURITY (peer-review finding: untagged-tool tag bypass): the
    /// provider-policy `require_tags` gate must fail closed for untagged
    /// tools. Plugin and MCP tools never declare tags (`tags()` defaults to
    /// `&[]`), so the old empty-tags exemption made a `require_tags`
    /// confinement filter a no-op for exactly the unaudited tool surface it
    /// exists to gate — any tool that simply omitted `tags()` walked
    /// straight through `specs()` and `is_tool_visible` to the LLM.
    #[test]
    fn should_fail_closed_for_untagged_tool_under_require_tags_provider_policy() {
        // Does not override `tags()` — it models every plugin/MCP/newly
        // added tool that ships untagged.
        struct UntaggedStubTool;

        #[async_trait::async_trait]
        impl Tool for UntaggedStubTool {
            fn name(&self) -> &str {
                "untagged_stub"
            }
            fn description(&self) -> &str {
                "test-only untagged tool"
            }
            fn input_schema(&self) -> serde_json::Value {
                serde_json::json!({"type": "object"})
            }
            async fn execute(&self, _args: &serde_json::Value) -> Result<ToolResult> {
                Ok(ToolResult {
                    output: String::new(),
                    success: true,
                    ..Default::default()
                })
            }
        }

        let mut reg = make_registry();
        reg.register_arc(Arc::new(UntaggedStubTool));
        reg.set_provider_policy(ToolPolicy {
            require_tags: vec!["code".to_string()],
            ..Default::default()
        });
        // A tool tagged with a matching tag stays visible (read_file is
        // tagged ["fs", "code"]).
        assert!(
            reg.is_tool_visible("read_file"),
            "a tool tagged 'code' must pass the require_tags gate"
        );
        // The untagged tool must be hidden from visibility and from the
        // LLM-facing specs() output.
        assert!(
            !reg.is_tool_visible("untagged_stub"),
            "untagged tools must FAIL a non-empty require_tags gate (fail closed)"
        );
        assert!(
            reg.specs().iter().all(|spec| spec.name != "untagged_stub"),
            "untagged tools must not be advertised to the LLM under require_tags"
        );
    }

    #[test]
    fn spawn_only_handle_message_payload_stays_under_one_kb() {
        // Phase 4 acceptance criterion: spawn_only tool result in agent
        // context is < 1KB (was 50KB+).
        let mut reg = make_registry();
        reg.mark_spawn_only("search", None);

        let payload = reg.spawn_only_handle_message("search", "task_xyz", &[]);

        assert!(
            payload.len() < 1024,
            "spawn_only handle envelope must be < 1KB, got {} bytes",
            payload.len()
        );
    }
}

#[cfg(test)]
mod context_threading_tests {
    //! M8.1 — tool context threaded through the registry dispatch path.

    use super::super::{Tool, ToolContext, ToolResult};
    use super::*;
    use async_trait::async_trait;
    use eyre::Result;
    use serde_json::Value;
    use std::sync::Mutex;

    /// Tool that echoes the `tool_id` it saw on the context, letting tests
    /// confirm the registry forwarded the caller's `ToolContext` into
    /// `execute_with_context`.
    struct CapturingTool {
        seen: Mutex<Option<String>>,
    }

    impl CapturingTool {
        fn new() -> Self {
            Self {
                seen: Mutex::new(None),
            }
        }
    }

    #[async_trait]
    impl Tool for CapturingTool {
        fn name(&self) -> &str {
            "capturing"
        }
        fn description(&self) -> &str {
            "test-only"
        }
        fn input_schema(&self) -> Value {
            serde_json::json!({"type": "object"})
        }
        async fn execute(&self, args: &Value) -> Result<ToolResult> {
            self.execute_with_context(&ToolContext::zero(), args).await
        }
        async fn execute_with_context(
            &self,
            ctx: &ToolContext,
            _args: &Value,
        ) -> Result<ToolResult> {
            *self.seen.lock().unwrap() = Some(ctx.tool_id.clone());
            Ok(ToolResult {
                output: ctx.tool_id.clone(),
                success: true,
                ..Default::default()
            })
        }
    }

    #[tokio::test]
    async fn should_pass_context_through_executor() {
        let mut reg = ToolRegistry::new();
        let tool = Arc::new(CapturingTool::new());
        reg.register_arc(tool.clone());

        let mut ctx = ToolContext::zero();
        ctx.tool_id = "call-m8.1".to_string();

        let result = reg
            .execute_with_context(&ctx, "capturing", &serde_json::json!({}))
            .await
            .expect("capturing tool must succeed");
        assert!(result.success);
        assert_eq!(result.output, "call-m8.1");

        let seen = tool.seen.lock().unwrap().clone();
        assert_eq!(
            seen.as_deref(),
            Some("call-m8.1"),
            "registry must forward the caller's ToolContext into execute_with_context",
        );
    }

    struct PanickingTool;

    #[async_trait]
    impl Tool for PanickingTool {
        fn name(&self) -> &str {
            "panicker"
        }
        fn description(&self) -> &str {
            "test-only: panics on execute"
        }
        fn input_schema(&self) -> Value {
            serde_json::json!({"type": "object"})
        }
        async fn execute(&self, _args: &Value) -> Result<ToolResult> {
            panic!("simulated tool panic");
        }
    }

    #[tokio::test]
    async fn tool_panic_is_isolated_to_a_failed_result_not_an_actor_crash() {
        // mini5 soak (Layer 2): a panicking tool must NOT unwind through the
        // registry — that would crash the session actor and orphan its
        // in-process sub-agents. The dispatch boundary catches the panic and
        // returns a failed ToolResult. This test COMPLETING (rather than
        // panicking) is itself the core assertion.
        let mut reg = ToolRegistry::new();
        reg.register_arc(Arc::new(PanickingTool));

        let result = reg
            .execute("panicker", &serde_json::json!({}))
            .await
            .expect("registry must return Ok(failed result), not propagate the panic");
        assert!(
            !result.success,
            "a panicking tool must yield a failed result"
        );
        assert!(
            result.output.contains("internal error"),
            "result should flag the internal failure: {}",
            result.output
        );
    }

    /// Tool whose `execute` never completes. Models the mini5 soak's
    /// "10-min opaque pipeline" / hung-foreground-tool class: without a
    /// per-tool timeout this would block the session actor's turn forever.
    struct HangingTool;

    #[async_trait]
    impl Tool for HangingTool {
        fn name(&self) -> &str {
            "hanger"
        }
        fn description(&self) -> &str {
            "test-only: never completes"
        }
        fn input_schema(&self) -> Value {
            serde_json::json!({"type": "object"})
        }
        async fn execute(&self, _args: &Value) -> Result<ToolResult> {
            // Awaits forever; the registry's per-tool timeout must degrade
            // this to a failed ToolResult rather than wedge the caller.
            futures::future::pending::<()>().await;
            unreachable!("pending() never resolves");
        }
    }

    #[tokio::test]
    async fn hung_tool_degrades_to_failed_timeout_result_not_a_hang() {
        // Gap 3.3: a foreground tool that HANGS must not wedge the caller
        // (session actor turn). With a short injected timeout the registry
        // degrades the hang to a failed ToolResult bearing a clear message.
        // This test COMPLETING (rather than hanging) is itself the core
        // assertion — `tokio::time::timeout` here is a belt-and-suspenders
        // backstop in case the registry guard regresses.
        let mut reg = ToolRegistry::new();
        reg.register_arc(Arc::new(HangingTool));
        reg.set_tool_timeout_secs(1);

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            reg.execute("hanger", &serde_json::json!({})),
        )
        .await
        .expect("registry must return within its own timeout, not hang the caller")
        .expect("registry must return Ok(failed result), not an Err");

        assert!(
            !result.success,
            "a hung tool must yield a failed result, got output={}",
            result.output
        );
        assert!(
            result.output.contains("timed out") && result.output.contains("hanger"),
            "result should name the tool and flag the timeout: {}",
            result.output
        );
    }

    #[tokio::test]
    async fn fast_tool_completes_well_within_timeout_no_false_positive() {
        // A normal fast tool must NOT be killed by the per-tool timeout.
        let mut reg = ToolRegistry::new();
        reg.register_arc(Arc::new(CapturingTool::new()));
        reg.set_tool_timeout_secs(1);

        let result = reg
            .execute("capturing", &serde_json::json!({}))
            .await
            .expect("fast tool must succeed");
        assert!(
            result.success,
            "fast tool must not trip the timeout, got output={}",
            result.output
        );
    }

    /// Tool that sleeps longer than the global default but overrides
    /// `execution_timeout_secs` to a tight bound, proving the per-tool
    /// override path is honoured.
    struct SlowOverrideTool;

    #[async_trait]
    impl Tool for SlowOverrideTool {
        fn name(&self) -> &str {
            "slow_override"
        }
        fn description(&self) -> &str {
            "test-only: sleeps forever but caps its own timeout"
        }
        fn input_schema(&self) -> Value {
            serde_json::json!({"type": "object"})
        }
        fn execution_timeout_secs(&self) -> Option<u64> {
            Some(1)
        }
        async fn execute(&self, _args: &Value) -> Result<ToolResult> {
            futures::future::pending::<()>().await;
            unreachable!("pending() never resolves");
        }
    }

    /// A human-wait tool: blocks on a requester (like `ask_user_question`'s
    /// `request_user_question` await). It must be EXEMPT from the dispatch
    /// timeout — a human may legitimately take longer than any finite tool
    /// timeout, and killing the future would drop the receiver and leak the
    /// pending store entry forever.
    struct HumanWaitTool {
        unblock: Arc<tokio::sync::Notify>,
    }

    #[async_trait]
    impl Tool for HumanWaitTool {
        fn name(&self) -> &str {
            "human_wait"
        }
        fn description(&self) -> &str {
            "test-only: blocks on a human until notified"
        }
        fn input_schema(&self) -> Value {
            serde_json::json!({"type": "object"})
        }
        fn blocks_on_human_input(&self) -> bool {
            true
        }
        async fn execute(&self, _args: &Value) -> Result<ToolResult> {
            self.unblock.notified().await;
            Ok(ToolResult {
                output: "answered".into(),
                success: true,
                ..Default::default()
            })
        }
    }

    #[tokio::test]
    async fn human_wait_tool_is_exempt_from_dispatch_timeout() {
        // A human-wait tool must NOT be killed by the dispatch timeout even
        // when the registry backstop is set to 1s — it stays blocked until
        // the human answers, then returns success. Mirrors how `shell`'s
        // approval gate is not killed by the tool timeout (#1).
        let mut reg = ToolRegistry::new();
        let unblock = Arc::new(tokio::sync::Notify::new());
        reg.register_arc(Arc::new(HumanWaitTool {
            unblock: unblock.clone(),
        }));
        // A tight backstop that WOULD kill a normal tool.
        reg.set_tool_timeout_secs(1);

        let unblock_for_task = unblock.clone();
        // Answer the "human" after 2s — comfortably past the 1s backstop.
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            unblock_for_task.notify_one();
        });

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            reg.execute("human_wait", &serde_json::json!({})),
        )
        .await
        .expect("human-wait tool must not hang the test")
        .expect("registry returns Ok");

        assert!(
            result.success,
            "human-wait tool must survive the dispatch timeout and return its answer, got: {}",
            result.output
        );
        assert_eq!(result.output, "answered");
    }

    #[tokio::test]
    async fn per_tool_timeout_override_is_honored() {
        // The tool caps itself at 1s via `execution_timeout_secs`, so even
        // with the registry's long default backstop it times out fast.
        let mut reg = ToolRegistry::new();
        reg.register_arc(Arc::new(SlowOverrideTool));
        // Leave the registry default at its long backstop; the per-tool
        // override must win.

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            reg.execute("slow_override", &serde_json::json!({})),
        )
        .await
        .expect("per-tool override must bound execution, not hang")
        .expect("registry must return Ok(failed result)");

        assert!(!result.success, "override timeout must fail the tool");
        assert!(
            result.output.contains("timed out"),
            "override timeout result must flag the timeout: {}",
            result.output
        );
    }

    #[tokio::test]
    async fn should_route_legacy_execute_through_zero_value_context() {
        // The legacy `execute(name, args)` entry must reach the same tool
        // but with a zero-value context (empty tool_id).
        let mut reg = ToolRegistry::new();
        let tool = Arc::new(CapturingTool::new());
        reg.register_arc(tool.clone());

        let result = reg
            .execute("capturing", &serde_json::json!({}))
            .await
            .expect("capturing tool must succeed via legacy entry");
        assert!(result.success);

        let seen = tool.seen.lock().unwrap().clone();
        assert_eq!(seen.as_deref(), Some(""));
    }
}

#[cfg(test)]
mod profile_filter_tests {
    //! M8.3 — `filter_by_profile` narrows the registry through a
    //! [`crate::profile::ProfileTools`] declaration. Behaviour parity
    //! with today's default path is covered by the
    //! `default_mode_is_pass_through` test.

    use super::*;
    use crate::profile::ProfileTools;

    fn builtin_names(reg: &ToolRegistry) -> Vec<String> {
        let mut names: Vec<String> = reg.tools.keys().cloned().collect();
        names.sort();
        names
    }

    #[test]
    fn should_not_filter_when_profile_mode_is_default() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut reg = ToolRegistry::with_builtins(dir.path());
        let before = builtin_names(&reg);

        reg.filter_by_profile(&ProfileTools::Default);

        let after = builtin_names(&reg);
        assert_eq!(before, after, "default mode must not narrow the registry");
    }

    #[test]
    fn should_filter_tool_registry_by_allow_list() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut reg = ToolRegistry::with_builtins(dir.path());

        reg.filter_by_profile(&ProfileTools::AllowList {
            tools: vec!["read_file".into(), "group:search".into()],
        });

        let names: Vec<String> = reg.tools.keys().cloned().collect();
        assert!(names.contains(&"read_file".to_string()));
        // group:search expands to glob/grep/list_dir.
        assert!(names.contains(&"glob".to_string()));
        assert!(names.contains(&"grep".to_string()));
        assert!(names.contains(&"list_dir".to_string()));
        // Not on the allow list, not spawn_only -> evicted.
        assert!(!names.contains(&"shell".to_string()));
        assert!(!names.contains(&"web_fetch".to_string()));
    }

    #[test]
    fn should_filter_tool_registry_by_deny_list() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut reg = ToolRegistry::with_builtins(dir.path());
        let before = builtin_names(&reg);

        reg.filter_by_profile(&ProfileTools::DenyList {
            tools: vec!["web_fetch".into(), "browser".into()],
        });

        let after = builtin_names(&reg);
        assert!(!after.contains(&"web_fetch".to_string()));
        assert!(!after.contains(&"browser".to_string()));
        // Everything else must survive.
        let expected_survivors: Vec<String> = before
            .iter()
            .filter(|n| n.as_str() != "web_fetch" && n.as_str() != "browser")
            .cloned()
            .collect();
        for n in expected_survivors {
            assert!(
                after.contains(&n),
                "{n} should survive the deny-list filter",
            );
        }
    }

    #[test]
    fn should_not_filter_spawn_only_tools_from_allow_list() {
        // A spawn_only tool that does not appear in the allow list must
        // still be retained — it carries background execution wiring the
        // runtime depends on.
        let dir = tempfile::tempdir().expect("tempdir");
        let mut reg = ToolRegistry::with_builtins(dir.path());
        reg.mark_spawn_only("mofa_slides", None);
        // Fake-register the tool so the filter has something to keep.
        // We reuse an existing builtin name for the test; mark_spawn_only
        // is just an annotation, it doesn't need the name to exist in
        // `self.tools` — for the retention check we need a real entry,
        // so register a no-op tool under that name.
        use async_trait::async_trait;
        use eyre::Result;
        use serde_json::Value;
        struct Noop;
        #[async_trait]
        impl Tool for Noop {
            fn name(&self) -> &str {
                "mofa_slides"
            }
            fn description(&self) -> &str {
                "noop"
            }
            fn input_schema(&self) -> Value {
                serde_json::json!({"type": "object"})
            }
            async fn execute(&self, _: &Value) -> Result<ToolResult> {
                Ok(ToolResult::default())
            }
        }
        reg.register(Noop);

        reg.filter_by_profile(&ProfileTools::AllowList {
            tools: vec!["read_file".into()],
        });

        let names: Vec<String> = reg.tools.keys().cloned().collect();
        assert!(
            names.contains(&"mofa_slides".to_string()),
            "spawn_only tools must survive an allow-list filter",
        );
        assert!(names.contains(&"read_file".to_string()));
        assert!(!names.contains(&"shell".to_string()));
    }

    #[test]
    fn should_not_filter_spawn_only_tools_from_deny_list() {
        // Same invariant, but the user declared a deny list that *names*
        // the spawn-only tool. The registry must still retain it.
        let dir = tempfile::tempdir().expect("tempdir");
        let mut reg = ToolRegistry::with_builtins(dir.path());
        reg.mark_spawn_only("mofa_slides", None);

        use async_trait::async_trait;
        use eyre::Result;
        use serde_json::Value;
        struct Noop;
        #[async_trait]
        impl Tool for Noop {
            fn name(&self) -> &str {
                "mofa_slides"
            }
            fn description(&self) -> &str {
                "noop"
            }
            fn input_schema(&self) -> Value {
                serde_json::json!({"type": "object"})
            }
            async fn execute(&self, _: &Value) -> Result<ToolResult> {
                Ok(ToolResult::default())
            }
        }
        reg.register(Noop);

        reg.filter_by_profile(&ProfileTools::DenyList {
            tools: vec!["mofa_slides".into()],
        });

        let names: Vec<String> = reg.tools.keys().cloned().collect();
        assert!(
            names.contains(&"mofa_slides".to_string()),
            "spawn_only tools cannot be evicted by a profile deny list",
        );
    }

    #[test]
    fn empty_allow_list_is_a_pass_through_with_warning() {
        // Defensive: an empty allow list would wipe the registry (minus
        // spawn_only). That is almost always an author mistake, so the
        // filter treats it as a pass-through. Authors who really want an
        // empty registry should use `deny_list` explicitly.
        let dir = tempfile::tempdir().expect("tempdir");
        let mut reg = ToolRegistry::with_builtins(dir.path());
        let before = builtin_names(&reg);

        reg.filter_by_profile(&ProfileTools::AllowList { tools: Vec::new() });

        let after = builtin_names(&reg);
        assert_eq!(before, after);
    }

    #[test]
    fn empty_deny_list_is_a_pass_through() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut reg = ToolRegistry::with_builtins(dir.path());
        let before = builtin_names(&reg);

        reg.filter_by_profile(&ProfileTools::DenyList { tools: Vec::new() });

        let after = builtin_names(&reg);
        assert_eq!(before, after);
    }

    #[test]
    fn allow_list_wildcard_matches_prefix() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut reg = ToolRegistry::with_builtins(dir.path());

        reg.filter_by_profile(&ProfileTools::AllowList {
            tools: vec!["workspace_*".into()],
        });

        let names: Vec<String> = reg.tools.keys().cloned().collect();
        assert!(names.contains(&"workspace_log".to_string()));
        assert!(names.contains(&"workspace_show".to_string()));
        assert!(names.contains(&"workspace_diff".to_string()));
        assert!(!names.contains(&"shell".to_string()));
    }

    #[test]
    fn coding_full_profile_produces_same_registry_as_default_builtins() {
        // Behaviour parity gate: applying the built-in `coding-full`
        // profile to a builtin registry must leave the registry IDENTICAL
        // to what the no-flag default path produced before the lean
        // `coding` default landed. This is the critical regression guard
        // called out in the M8.3 issue, retargeted at the unfiltered
        // escape hatch now that `coding` itself carries an allow list
        // (see `crate::profile::tests` for the lean-narrowing pins).
        use crate::profile::ProfileDefinition;

        let dir = tempfile::tempdir().expect("tempdir");
        let reference = ToolRegistry::with_builtins(dir.path());
        let reference_names = builtin_names(&reference);

        let full = ProfileDefinition::builtin("coding-full").expect("coding-full builtin");
        let mut profiled = ToolRegistry::with_builtins(dir.path());
        full.apply_to_registry(&mut profiled);

        let profiled_names = builtin_names(&profiled);
        assert_eq!(
            reference_names, profiled_names,
            "coding-full profile must preserve behaviour parity with the default path",
        );
    }

    #[test]
    fn should_retain_only_mofa_slides_when_slides_session_filter_runs() {
        // Pins the wiring in session_actor.rs::spawn slides branch:
        // `tools.retain(octos_agent::keep_tool_in_slides_session)` must
        // evict every fake mofa skill except `mofa_slides`, and must NOT
        // evict the unrelated tools (read_file, shell, etc.).
        //
        // Without this guardrail the kimi-k2.6 fallback on mini1 dspfac
        // misrouted "Make a 3-slide intro deck" → mofa_site (2026-05-24
        // soak). The structural filter makes that misroute literally
        // impossible regardless of LLM judgement.
        use super::policy::keep_tool_in_slides_session;
        use async_trait::async_trait;
        use eyre::Result;
        use serde_json::Value;

        struct FakeMofa(&'static str);
        #[async_trait]
        impl Tool for FakeMofa {
            fn name(&self) -> &str {
                self.0
            }
            fn description(&self) -> &str {
                "fake mofa skill (test fixture)"
            }
            fn input_schema(&self) -> Value {
                serde_json::json!({"type": "object"})
            }
            async fn execute(&self, _: &Value) -> Result<ToolResult> {
                Ok(ToolResult::default())
            }
        }

        let dir = tempfile::tempdir().expect("tempdir");
        let mut reg = ToolRegistry::with_builtins(dir.path());
        // Simulate the fleet skill surface: every dspfac-installed mofa
        // skill is registered as a plugin tool. (Real plugins go via
        // PluginLoader; here we use a stub for an isolated unit test.)
        for name in [
            "mofa_slides",
            "mofa_site",
            "mofa_youtube",
            "mofa_publish",
            "mofa_research",
            "mofa_pdf",
            "mofa_xlsx",
            "mofa_cli",
            "mofa_fm",
            "mofa_frame",
            "mofa_podcast",
            "mofa_infographic",
            "mofa_cards",
            "mofa_comic",
        ] {
            reg.register(FakeMofa(name));
        }

        reg.retain(keep_tool_in_slides_session);

        let names = builtin_names(&reg);
        assert!(
            names.contains(&"mofa_slides".to_string()),
            "mofa_slides MUST survive the slides-session filter",
        );
        for unwanted in [
            "mofa_site",
            "mofa_youtube",
            "mofa_publish",
            "mofa_research",
            "mofa_pdf",
            "mofa_xlsx",
            "mofa_cli",
            "mofa_fm",
            "mofa_frame",
            "mofa_podcast",
            "mofa_infographic",
            "mofa_cards",
            "mofa_comic",
        ] {
            assert!(
                !names.contains(&unwanted.to_string()),
                "{unwanted} MUST be evicted from a slides session",
            );
        }
        // Built-in non-mofa tools must remain — these are the tools the
        // slides system prompt's "TOOL DISCIPLINE" block depends on.
        for kept in ["read_file", "write_file", "glob", "shell"] {
            assert!(
                names.contains(&kept.to_string()),
                "{kept} must NOT be evicted by the slides filter",
            );
        }
    }

    /// RFC-1 (issue #1290): a make_type dispatcher target marked
    /// `mark_internal_hidden` is excluded from the LLM-visible `specs()`
    /// set but remains callable via `get()` (so the dispatcher can forward
    /// to it). Non-hidden siblings stay visible.
    #[test]
    fn internal_hidden_excluded_from_specs_but_callable_via_get() {
        use async_trait::async_trait;
        use eyre::Result;
        use serde_json::Value;

        struct FakeTool(&'static str);
        #[async_trait]
        impl Tool for FakeTool {
            fn name(&self) -> &str {
                self.0
            }
            fn description(&self) -> &str {
                "fake (test fixture)"
            }
            fn input_schema(&self) -> Value {
                serde_json::json!({"type": "object"})
            }
            async fn execute(&self, _: &Value) -> Result<ToolResult> {
                Ok(ToolResult::default())
            }
        }

        let mut reg = ToolRegistry::new();
        for name in ["mofa_slides", "mofa_cards"] {
            reg.register(FakeTool(name));
        }
        // `mofa_slides` is the dispatcher's target — hide it from the LLM.
        reg.mark_internal_hidden("mofa_slides");

        let visible: Vec<String> = reg.specs().into_iter().map(|s| s.name).collect();
        assert!(
            !visible.contains(&"mofa_slides".to_string()),
            "internal-hidden mofa_slides must NOT appear in specs; got {visible:?}"
        );
        assert!(
            visible.contains(&"mofa_cards".to_string()),
            "non-hidden mofa_cards must remain visible in specs; got {visible:?}"
        );
        // Still reachable via get() for internal dispatcher forwarding.
        assert!(
            reg.get("mofa_slides").is_some(),
            "internal-hidden tool must remain callable via get()"
        );
        assert!(reg.is_internal_hidden("mofa_slides"));
    }

    /// RFC-1 fixup (codex round 4 P2): when `retain` evicts dispatcher
    /// target tools (e.g. `mofa_cards`, `mofa_comic` during the
    /// slides-session retain pass), the surviving `MofaMakeTool`
    /// dispatcher's catalog must be pruned in lockstep. Otherwise the
    /// dispatcher's `content_type` enum continues to advertise the
    /// evicted content types and the LLM can call them, only to
    /// observe `[DISPATCHER_ERROR]` because the target is gone.
    #[test]
    fn retain_prunes_mofa_make_catalog_to_surviving_targets() {
        use super::policy::keep_tool_in_slides_session;
        use crate::tools::{MakeTypeEntry, MofaDescribeContentTypeTool, MofaMakeTool};
        use async_trait::async_trait;
        use eyre::Result;
        use serde_json::Value;

        struct FakeTool(&'static str);
        #[async_trait]
        impl Tool for FakeTool {
            fn name(&self) -> &str {
                self.0
            }
            fn description(&self) -> &str {
                "fake"
            }
            fn input_schema(&self) -> Value {
                serde_json::json!({"type": "object"})
            }
            async fn execute(&self, _: &Value) -> Result<ToolResult> {
                Ok(ToolResult::default())
            }
        }

        let mut reg = ToolRegistry::new();
        // Register target tools for three content_types so the
        // dispatcher catalog has entries to prune.
        reg.register(FakeTool("mofa_slides"));
        reg.register(FakeTool("mofa_cards"));
        reg.register(FakeTool("mofa_comic"));

        // Construct a dispatcher pair seeded with all three entries
        // (mirrors what the loader does after discovering three
        // `make_type` plugins).
        let dispatcher = MofaMakeTool::new();
        let describe = MofaDescribeContentTypeTool::new();
        for entry in [
            MakeTypeEntry::new("slides", "mofa-slides", "mofa_slides", "PPTX decks"),
            MakeTypeEntry::new("cards", "mofa-cards", "mofa_cards", "Greeting cards"),
            MakeTypeEntry::new("comic", "mofa-comic", "mofa_comic", "Comic strips"),
        ] {
            dispatcher.register_or_replace(entry.clone());
            describe.register_or_replace(entry);
        }
        reg.register(dispatcher);
        reg.register(describe);
        // Hide each target (the loader does this in
        // `mark_internal_hidden` after dispatcher registration).
        for target in ["mofa_slides", "mofa_cards", "mofa_comic"] {
            reg.mark_internal_hidden(target);
        }

        // Sanity: the catalog has 3 entries before retain.
        let pre = reg
            .get("mofa_make")
            .and_then(|arc| arc.as_any().downcast_ref::<MofaMakeTool>())
            .unwrap()
            .entries();
        assert_eq!(pre.len(), 3, "catalog must have 3 entries before retain");

        // Apply the slides-session retain. This evicts `mofa_cards`
        // and `mofa_comic` but keeps `mofa_slides` + the dispatcher
        // pair.
        reg.retain(keep_tool_in_slides_session);

        // Post-condition: the dispatcher's catalog has been pruned to
        // only the surviving content_type (slides). The LLM's
        // mofa_make enum will no longer offer `cards` or `comic`.
        let post = reg
            .get("mofa_make")
            .and_then(|arc| arc.as_any().downcast_ref::<MofaMakeTool>())
            .expect("mofa_make survived retain")
            .entries();
        assert_eq!(
            post.len(),
            1,
            "catalog must be pruned to surviving targets; got {post:?}"
        );
        assert_eq!(post[0].content_type, "slides");

        // The describe tool's catalog is pruned symmetrically so the
        // LLM cannot fetch a schema for an evicted content_type.
        let describe_post = reg
            .get("mofa_describe_content_type")
            .and_then(|arc| arc.as_any().downcast_ref::<MofaDescribeContentTypeTool>())
            .expect("describe tool survived retain")
            .entries();
        assert_eq!(describe_post.len(), 1);
        assert_eq!(describe_post[0].content_type, "slides");
    }

    /// RFC-1 fixup (codex round 5 P1): when a registry is built via
    /// `snapshot_excluding` (or `rebind_cwd`, which calls that
    /// internally), the per-session registry shares the SAME
    /// `Arc<MofaMakeTool>` instance with the base/profile registry it
    /// was cloned from. A subsequent `retain` on the per-session
    /// registry must NOT poison the base/profile registry's
    /// dispatcher catalog via interior-mutable `replace_entries` on
    /// the shared `Arc<MofaMakeTool>`. Otherwise the next non-slides
    /// session cloned from the SAME base also sees the pruned
    /// catalog (e.g. only `slides`), and `mofa_make` silently loses
    /// `cards`, `comic`, `site`, etc. until restart.
    #[test]
    fn retain_does_not_corrupt_base_registry_dispatcher_catalog() {
        use super::policy::keep_tool_in_slides_session;
        use crate::tools::{MakeTypeEntry, MofaDescribeContentTypeTool, MofaMakeTool};
        use async_trait::async_trait;
        use eyre::Result;
        use serde_json::Value;

        struct FakeTool(&'static str);
        #[async_trait]
        impl Tool for FakeTool {
            fn name(&self) -> &str {
                self.0
            }
            fn description(&self) -> &str {
                "fake"
            }
            fn input_schema(&self) -> Value {
                serde_json::json!({"type": "object"})
            }
            async fn execute(&self, _: &Value) -> Result<ToolResult> {
                Ok(ToolResult::default())
            }
        }

        // Build the base registry mirroring what the gateway / profile
        // assembles before per-session snapshots are made.
        let mut base = ToolRegistry::new();
        for target in ["mofa_slides", "mofa_cards", "mofa_comic", "mofa_site"] {
            base.register(FakeTool(target));
        }
        let dispatcher = MofaMakeTool::new();
        let describe = MofaDescribeContentTypeTool::new();
        for entry in [
            MakeTypeEntry::new("slides", "mofa-slides", "mofa_slides", "PPTX decks"),
            MakeTypeEntry::new("cards", "mofa-cards", "mofa_cards", "Cards"),
            MakeTypeEntry::new("comic", "mofa-comic", "mofa_comic", "Comic strips"),
            MakeTypeEntry::new("site", "mofa-site", "mofa_site", "Static sites"),
        ] {
            dispatcher.register_or_replace(entry.clone());
            describe.register_or_replace(entry);
        }
        base.register(dispatcher);
        base.register(describe);
        for target in ["mofa_slides", "mofa_cards", "mofa_comic", "mofa_site"] {
            base.mark_internal_hidden(target);
        }

        // Snapshot from the base — this is what a per-session registry
        // looks like before any retain pass. The cloned `Arc<dyn Tool>`
        // for `mofa_make` is SHARED with the base.
        let mut slides_session = base.snapshot_excluding(&[]);

        // Pre-condition: both base and snapshot have all 4 entries.
        let base_pre = base
            .get("mofa_make")
            .and_then(|arc| arc.as_any().downcast_ref::<MofaMakeTool>())
            .unwrap()
            .entries();
        assert_eq!(
            base_pre.len(),
            4,
            "sanity: base has 4 entries before retain"
        );

        // Slides-session retain: keeps only mofa_slides plus the
        // dispatcher pair; evicts mofa_cards/mofa_comic/mofa_site.
        slides_session.retain(keep_tool_in_slides_session);

        // The slides session's dispatcher must observe only `slides`.
        let session_post = slides_session
            .get("mofa_make")
            .and_then(|arc| arc.as_any().downcast_ref::<MofaMakeTool>())
            .expect("slides session retained mofa_make")
            .entries();
        assert_eq!(
            session_post.len(),
            1,
            "slides session catalog must be pruned; got {session_post:?}"
        );
        assert_eq!(session_post[0].content_type, "slides");

        // CRITICAL: the base registry's dispatcher catalog must be
        // unchanged. If the retain mutated the SHARED Arc, the base
        // would silently lose the other content types.
        let base_post = base
            .get("mofa_make")
            .and_then(|arc| arc.as_any().downcast_ref::<MofaMakeTool>())
            .expect("base still has mofa_make")
            .entries();
        assert_eq!(
            base_post.len(),
            4,
            "base registry's dispatcher catalog MUST NOT be poisoned by \
             a slides-session retain; got {:?}",
            base_post
                .iter()
                .map(|e| &e.content_type)
                .collect::<Vec<_>>()
        );
        let base_types: std::collections::HashSet<&str> =
            base_post.iter().map(|e| e.content_type.as_str()).collect();
        for required in ["slides", "cards", "comic", "site"] {
            assert!(
                base_types.contains(required),
                "base catalog lost {required:?} after slides session retain; got {base_types:?}"
            );
        }

        // Mirror the assertion for the describe tool.
        let base_describe_post = base
            .get("mofa_describe_content_type")
            .and_then(|arc| arc.as_any().downcast_ref::<MofaDescribeContentTypeTool>())
            .expect("base still has describe tool")
            .entries();
        assert_eq!(
            base_describe_post.len(),
            4,
            "base describe catalog must also survive intact",
        );
    }

    /// RFC-1 fixup (codex round 5 P1): build a base; spawn a slides
    /// session via `snapshot_excluding`; THEN spawn a second
    /// (non-slides) session from the same base. The second session's
    /// dispatcher must observe all original content types — the slides
    /// session's retain must not leak into other sessions cloned from
    /// the same base.
    #[test]
    fn slides_session_retain_leaves_other_sessions_unaffected() {
        use super::policy::keep_tool_in_slides_session;
        use crate::tools::{MakeTypeEntry, MofaMakeTool};
        use async_trait::async_trait;
        use eyre::Result;
        use serde_json::Value;

        struct FakeTool(&'static str);
        #[async_trait]
        impl Tool for FakeTool {
            fn name(&self) -> &str {
                self.0
            }
            fn description(&self) -> &str {
                "fake"
            }
            fn input_schema(&self) -> Value {
                serde_json::json!({"type": "object"})
            }
            async fn execute(&self, _: &Value) -> Result<ToolResult> {
                Ok(ToolResult::default())
            }
        }

        let mut base = ToolRegistry::new();
        for target in ["mofa_slides", "mofa_cards", "mofa_comic"] {
            base.register(FakeTool(target));
        }
        let dispatcher = MofaMakeTool::new();
        for entry in [
            MakeTypeEntry::new("slides", "mofa-slides", "mofa_slides", "PPTX decks"),
            MakeTypeEntry::new("cards", "mofa-cards", "mofa_cards", "Cards"),
            MakeTypeEntry::new("comic", "mofa-comic", "mofa_comic", "Comic strips"),
        ] {
            dispatcher.register_or_replace(entry);
        }
        base.register(dispatcher);

        // Session A: slides session → retain.
        let mut session_a = base.snapshot_excluding(&[]);
        session_a.retain(keep_tool_in_slides_session);
        let a_entries = session_a
            .get("mofa_make")
            .and_then(|arc| arc.as_any().downcast_ref::<MofaMakeTool>())
            .unwrap()
            .entries();
        assert_eq!(a_entries.len(), 1, "session A pruned to slides only");

        // Session B: a fresh snapshot from the SAME base — must see
        // ALL original content types. If session A's retain corrupted
        // the shared dispatcher Arc, session B would also see only
        // slides → permanent regression until process restart.
        let session_b = base.snapshot_excluding(&[]);
        let b_entries = session_b
            .get("mofa_make")
            .and_then(|arc| arc.as_any().downcast_ref::<MofaMakeTool>())
            .expect("session B has mofa_make")
            .entries();
        assert_eq!(
            b_entries.len(),
            3,
            "session B must see the full catalog; got {:?}",
            b_entries
                .iter()
                .map(|e| &e.content_type)
                .collect::<Vec<_>>()
        );
        let b_types: std::collections::HashSet<&str> =
            b_entries.iter().map(|e| e.content_type.as_str()).collect();
        for required in ["slides", "cards", "comic"] {
            assert!(
                b_types.contains(required),
                "session B lost {required:?}; got {b_types:?}"
            );
        }
    }

    /// RFC-1 fixup (codex round 5 P1): two slides sessions retain
    /// concurrently from the same base. Each session's local
    /// dispatcher must be pruned to `slides`-only, but the base must
    /// remain fully intact. Exercises the shared-Arc hazard under
    /// concurrent mutation rather than sequential.
    #[test]
    fn concurrent_retains_on_shared_base_dont_race() {
        use super::policy::keep_tool_in_slides_session;
        use crate::tools::{MakeTypeEntry, MofaMakeTool};
        use async_trait::async_trait;
        use eyre::Result;
        use serde_json::Value;

        struct FakeTool(&'static str);
        #[async_trait]
        impl Tool for FakeTool {
            fn name(&self) -> &str {
                self.0
            }
            fn description(&self) -> &str {
                "fake"
            }
            fn input_schema(&self) -> Value {
                serde_json::json!({"type": "object"})
            }
            async fn execute(&self, _: &Value) -> Result<ToolResult> {
                Ok(ToolResult::default())
            }
        }

        let mut base = ToolRegistry::new();
        for target in ["mofa_slides", "mofa_cards", "mofa_comic", "mofa_site"] {
            base.register(FakeTool(target));
        }
        let dispatcher = MofaMakeTool::new();
        for entry in [
            MakeTypeEntry::new("slides", "mofa-slides", "mofa_slides", "PPTX decks"),
            MakeTypeEntry::new("cards", "mofa-cards", "mofa_cards", "Cards"),
            MakeTypeEntry::new("comic", "mofa-comic", "mofa_comic", "Comic strips"),
            MakeTypeEntry::new("site", "mofa-site", "mofa_site", "Static sites"),
        ] {
            dispatcher.register_or_replace(entry);
        }
        base.register(dispatcher);

        // Spawn two slides session snapshots and run their retain
        // passes on separate threads to expose any race on the shared
        // dispatcher Arc.
        let mut handles = Vec::new();
        for _ in 0..2 {
            let mut session = base.snapshot_excluding(&[]);
            handles.push(std::thread::spawn(move || {
                session.retain(keep_tool_in_slides_session);
                session
                    .get("mofa_make")
                    .and_then(|arc| arc.as_any().downcast_ref::<MofaMakeTool>())
                    .unwrap()
                    .entries()
                    .into_iter()
                    .map(|e| e.content_type)
                    .collect::<Vec<_>>()
            }));
        }
        for h in handles {
            let session_entries = h.join().expect("retain thread panicked");
            assert_eq!(
                session_entries,
                vec!["slides".to_string()],
                "every session must end with only slides; got {session_entries:?}"
            );
        }

        // Base registry must still hold every original content type.
        let base_entries = base
            .get("mofa_make")
            .and_then(|arc| arc.as_any().downcast_ref::<MofaMakeTool>())
            .unwrap()
            .entries();
        assert_eq!(
            base_entries.len(),
            4,
            "base catalog must be untouched by concurrent session retains; got {:?}",
            base_entries
                .iter()
                .map(|e| &e.content_type)
                .collect::<Vec<_>>()
        );
    }

    /// Profile narrowing must NOT tear down an MCP transport (#1886).
    ///
    /// `McpService` is an `Arc<RunningService<..>>` and every `McpTool` held a
    /// clone, so the registered TOOLS were the only owners. `filter_by_profile`
    /// is a `retain()`, and MCP tool names are absent from a lean profile's
    /// allow-list, so narrowing dropped the last `Arc` — cancelling the
    /// transport and killing the stdio child roughly 1ms after startup. The
    /// operator got an agent silently missing tools they had explicitly
    /// configured, and the only trace was two INFO lines that read like an
    /// ordinary shutdown.
    ///
    /// The registry now owns the handle independently, so this asserts the real
    /// property via a DROP FLAG rather than a count that could go stale: after a
    /// retain that removes everything, the transport is still alive.
    #[test]
    fn retaining_no_tools_must_not_drop_a_live_mcp_transport() {
        struct Transport(Arc<std::sync::atomic::AtomicBool>);
        impl Drop for Transport {
            fn drop(&mut self) {
                self.0.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }

        let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut registry = ToolRegistry::new();
        registry.keep_mcp_service_alive(Arc::new(Transport(dropped.clone())));
        assert_eq!(registry.live_mcp_transport_count(), 1);

        // The narrowing case: evict every tool, exactly as a lean profile's
        // allow-list does to MCP tool names.
        registry.retain(|_| false);

        assert!(
            !dropped.load(std::sync::atomic::Ordering::SeqCst),
            "evicting every tool must NOT drop the MCP transport — that is the \
             bug: a visibility filter killing the server's child process"
        );
        assert_eq!(
            registry.live_mcp_transport_count(),
            1,
            "the registry must still own the transport after narrowing"
        );

        // ...and it IS released with the registry, so this is not a leak.
        drop(registry);
        assert!(
            dropped.load(std::sync::atomic::Ordering::SeqCst),
            "the transport must be released when the registry drops"
        );
    }
}

#[cfg(test)]
mod spec_order_tests {
    //! `specs()` order determinism: providers replay the tool array verbatim
    //! into the LLM prompt prefix, so a shuffled order both busts
    //! provider-side prompt caches (Anthropic `cache_control` breakpoints
    //! cache the tools+system prefix) and makes requests nondeterministic
    //! across registry rebuilds. `specs()` must emit tools sorted by name.

    use super::super::{Tool, ToolResult};
    use super::*;
    use async_trait::async_trait;
    use eyre::Result;
    use serde_json::Value;

    struct NamedTool {
        name: String,
        contexts: Vec<String>,
    }

    #[async_trait]
    impl Tool for NamedTool {
        fn name(&self) -> &str {
            &self.name
        }
        fn description(&self) -> &str {
            "test-only"
        }
        fn input_schema(&self) -> Value {
            serde_json::json!({"type": "object"})
        }
        fn contexts(&self) -> &[String] {
            &self.contexts
        }
        async fn execute(&self, _args: &Value) -> Result<ToolResult> {
            Ok(ToolResult {
                output: String::new(),
                success: true,
                ..Default::default()
            })
        }
    }

    fn registry_with(names: &[&str]) -> ToolRegistry {
        let mut registry = ToolRegistry::new();
        for name in names {
            registry.register(NamedTool {
                name: (*name).to_string(),
                contexts: Vec::new(),
            });
        }
        registry
    }

    #[test]
    fn specs_are_sorted_by_name_and_deterministic_across_rebuilds() {
        let names = [
            "zeta", "alpha", "mid", "beta", "omega", "kappa", "gamma", "delta",
        ];
        let mut reversed = names;
        reversed.reverse();

        let a = registry_with(&names);
        let b = registry_with(&reversed);

        let a_names: Vec<String> = a.specs().iter().map(|s| s.name.clone()).collect();
        let b_names: Vec<String> = b.specs().iter().map(|s| s.name.clone()).collect();

        let mut sorted = a_names.clone();
        sorted.sort();
        assert_eq!(
            a_names, sorted,
            "specs() must emit tools sorted by name (HashMap order is nondeterministic)"
        );
        assert_eq!(
            a_names, b_names,
            "two registries with the same tools must serialize identically"
        );
    }

    #[test]
    fn should_expose_context_scoped_tools_only_in_matching_turn_context() {
        let mut registry = ToolRegistry::new();
        registry.register(NamedTool {
            name: "always_available".to_string(),
            contexts: Vec::new(),
        });
        registry.register(NamedTool {
            name: "notebook_only".to_string(),
            contexts: vec!["notebook".to_string()],
        });

        let default_names: Vec<String> =
            registry.specs().into_iter().map(|spec| spec.name).collect();
        assert_eq!(default_names, vec!["always_available"]);
        assert!(!registry.is_tool_visible("notebook_only"));
        assert!(
            registry
                .catalog_snapshot()
                .iter()
                .all(|entry| entry.name != "notebook_only")
        );

        registry.set_active_context(Some("notebook".to_string()));

        let notebook_names: Vec<String> =
            registry.specs().into_iter().map(|spec| spec.name).collect();
        assert_eq!(notebook_names, vec!["always_available", "notebook_only"]);
        assert!(registry.is_tool_visible("notebook_only"));
        assert!(
            registry
                .catalog_snapshot()
                .iter()
                .any(|entry| entry.name == "notebook_only")
        );

        registry.set_active_context(Some("voice".to_string()));
        assert!(!registry.is_tool_visible("notebook_only"));
        assert_eq!(
            registry
                .specs()
                .into_iter()
                .map(|spec| spec.name)
                .collect::<Vec<_>>(),
            vec!["always_available"]
        );
    }

    #[test]
    fn should_isolate_active_context_between_registry_snapshots() {
        let mut base = ToolRegistry::new();
        base.register(NamedTool {
            name: "notebook_only".to_string(),
            contexts: vec!["notebook".to_string()],
        });

        let mut notebook_turn = base.snapshot_excluding(&[]);
        notebook_turn.set_active_context(Some("notebook".to_string()));
        let ordinary_turn = base.snapshot_excluding(&[]);

        assert!(notebook_turn.is_tool_visible("notebook_only"));
        assert!(!ordinary_turn.is_tool_visible("notebook_only"));
        assert!(!base.is_tool_visible("notebook_only"));
    }
}
