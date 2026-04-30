//! Tool registry: stores, filters, and executes registered tools.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use eyre::Result;
use octos_llm::ToolSpec;

use crate::task_supervisor::TaskSupervisor;

#[cfg(feature = "ast")]
use super::CodeStructureTool;
use super::policy::{self, ToolPolicy};
use super::{
    BrowserTool, CheckWorkspaceContractTool, ConfigureToolTool, DiffEditTool, EditFileTool,
    GlobTool, GrepTool, ListDirTool, ReadFileTool, ShellTool, Tool, ToolConfigStore, ToolLifecycle,
    ToolResult, WebFetchTool, WebSearchTool, WorkspaceDiffTool, WorkspaceLogTool,
    WorkspaceShowTool, WriteFileTool,
};
use crate::sandbox::{NoSandbox, Sandbox};

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
    /// Cached specs output, invalidated on registry mutations.
    cached_specs: std::sync::Mutex<Option<Vec<ToolSpec>>>,
    /// Deferred tools: registered but hidden from specs() until activated.
    /// Uses interior mutability so activate() can work through Arc<ToolRegistry>.
    deferred: std::sync::Mutex<HashSet<String>>,
    /// LRU lifecycle manager for auto-eviction of idle tools.
    lifecycle: std::sync::Mutex<ToolLifecycle>,
    /// Tool names that came from plugin binaries (for auto-send hook filtering).
    plugin_tools: HashSet<String>,
    /// Tools that are permanently deferred (spawn_only) -- cannot be activated by activate_tools.
    spawn_only: HashSet<String>,
    /// Custom messages for spawn_only tools returned to the LLM after auto-backgrounding.
    spawn_only_messages: HashMap<String, String>,
    /// Callback to notify session actor when background (spawn_only) tasks complete or fail.
    background_result_sender: Option<super::spawn::BackgroundResultSender>,
    /// Supervisor for tracking background task lifecycle.
    supervisor: Arc<TaskSupervisor>,
    /// Set to true when any spawn_only tool is actually invoked in this agent run.
    spawn_only_invoked: Arc<std::sync::atomic::AtomicBool>,
    /// Session key for tagging background tasks (set per-session).
    session_key: Option<String>,
    /// Precomputed output directory hint for spawn_only tool messaging.
    output_dir_hint: Option<String>,
}

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
            cached_specs: std::sync::Mutex::new(None),
            deferred: std::sync::Mutex::new(HashSet::new()),
            lifecycle: std::sync::Mutex::new(ToolLifecycle::default()),
            plugin_tools: HashSet::new(),
            spawn_only: HashSet::new(),
            spawn_only_messages: HashMap::new(),
            background_result_sender: None,
            supervisor: Arc::new(TaskSupervisor::new()),
            spawn_only_invoked: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            session_key: None,
            output_dir_hint: None,
        }
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
        self.tools.insert(tool.name().to_string(), Arc::new(tool));
        self.invalidate_cache();
    }

    /// Register a tool from an existing Arc (for keeping a separate reference).
    pub fn register_arc(&mut self, tool: Arc<dyn Tool>) {
        self.tools.insert(tool.name().to_string(), tool);
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
    /// rather than letting the admission phase fail silently. Plugin/MCP
    /// tools report `Safe` today because their wrapper inherits the trait
    /// default; they will be wired through a declared class in a follow-up.
    pub fn concurrency_class(&self, name: &str) -> super::ConcurrencyClass {
        self.tools
            .get(name)
            .map(|t| t.concurrency_class())
            .unwrap_or_default()
    }

    /// Get tool specifications for the LLM, filtered by provider policy if set.
    /// Results are cached and invalidated when the registry is mutated.
    pub fn specs(&self) -> Vec<ToolSpec> {
        let mut cache = self.cached_specs.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(ref specs) = *cache {
            return specs.clone();
        }

        let deferred = self.deferred.lock().unwrap_or_else(|e| e.into_inner());
        let specs: Vec<ToolSpec> = self
            .tools
            .values()
            .filter(|t| !deferred.contains(t.name()))
            .filter(|t| {
                self.provider_policy
                    .as_ref()
                    .is_none_or(|p| p.is_allowed_with_tags(t.name(), t.tags()))
            })
            .filter(|t| {
                self.context_filter.as_ref().is_none_or(|tags| {
                    // Tools with no tags pass through; tools with tags must match
                    let tool_tags = t.tags();
                    tool_tags.is_empty()
                        || tool_tags.iter().any(|tag| tags.contains(&tag.to_string()))
                })
            })
            .map(|t| ToolSpec {
                name: t.name().to_string(),
                description: t.description().to_string(),
                input_schema: t.input_schema(),
            })
            .collect();

        *cache = Some(specs.clone());
        specs
    }

    /// Get a tool by name.
    pub fn get(&self, name: &str) -> Option<&Arc<dyn Tool>> {
        self.tools.get(name)
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
    /// `spawn_only_messages`, `deferred`) for any names that were
    /// dropped. Without this, stale entries survive an `apply_policy`
    /// deny and produce confusing downstream behaviour:
    ///
    /// - A stale `spawn_only` marker fools the agent's spawn_only
    ///   intercept in `execution.rs` into treating an evicted tool as
    ///   background-eligible. The intercept falls through to
    ///   `bg_tools.execute_with_context` which fails async because the
    ///   tool itself is gone from the registry — so the foreground turn
    ///   observes a fake "started successfully". See PR #688 follow-up
    ///   MEDIUM #3.
    /// - A stale `deferred` entry would let `activate_tools` /
    ///   `has_deferred()` advertise a name that was already evicted by
    ///   policy. See PR #688 follow-up codex review (round 2).
    pub fn retain(&mut self, f: impl Fn(&str) -> bool) {
        self.tools.retain(|name, _| f(name));
        self.spawn_only.retain(|name| self.tools.contains_key(name));
        self.spawn_only_messages
            .retain(|name, _| self.tools.contains_key(name));
        // Stale `deferred` entries are interior-mutable; lock and prune
        // here so a subsequent `activate(...)` cannot resurrect a tool
        // that policy has already removed.
        {
            let mut deferred = self.deferred.lock().unwrap_or_else(|e| e.into_inner());
            deferred.retain(|name| self.tools.contains_key(name));
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

    /// Set a context-based tag filter. Only tools whose tags overlap with these
    /// values will appear in `specs()`. Tools with no tags always pass through.
    pub fn set_context_filter(&mut self, tags: Vec<String>) {
        if tags.is_empty() {
            return;
        }
        self.context_filter = Some(tags);
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
        let deferred = self
            .deferred
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let parent = self.lifecycle.lock().unwrap_or_else(|e| e.into_inner());
        let lifecycle = ToolLifecycle {
            last_used: HashMap::new(),
            iteration: 0,
            base_tools: parent.base_tools.clone(),
            max_active: parent.max_active,
            idle_threshold: parent.idle_threshold,
        };
        drop(parent);

        Self {
            tools,
            workspace_root: self.workspace_root.clone(),
            provider_policy: self.provider_policy.clone(),
            context_filter: self.context_filter.clone(),
            cached_specs: std::sync::Mutex::new(None),
            deferred: std::sync::Mutex::new(deferred),
            lifecycle: std::sync::Mutex::new(lifecycle),
            plugin_tools: self.plugin_tools.clone(),
            spawn_only: self.spawn_only.clone(),
            spawn_only_messages: self.spawn_only_messages.clone(),
            background_result_sender: None,
            supervisor: Arc::new(TaskSupervisor::new()),
            spawn_only_invoked: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            session_key: None,
            output_dir_hint: self.output_dir_hint.clone(),
        }
    }

    // -- Deferred tool activation -------------------------------------------

    /// Mark tools as deferred (hidden from specs until activated).
    /// Call during setup before wrapping in Arc.
    pub fn defer(&mut self, names: impl IntoIterator<Item = String>) {
        let deferred = self.deferred.get_mut().unwrap_or_else(|e| e.into_inner());
        for name in names {
            if self.tools.contains_key(&name) {
                deferred.insert(name);
            }
        }
        self.invalidate_cache();
    }

    /// Defer all tools in a named group (e.g. "group:web").
    pub fn defer_group(&mut self, group: &str) {
        if let Some(info) = policy::tool_group_info(group) {
            let deferred = self.deferred.get_mut().unwrap_or_else(|e| e.into_inner());
            for &tool in info.tools {
                if self.tools.contains_key(tool) {
                    deferred.insert(tool.to_string());
                }
            }
            self.invalidate_cache();
        }
    }

    /// Activate a deferred tool group or individual tool. Works through `&self`
    /// (interior mutability) so it can be called during the agent loop via Arc.
    /// Returns the names of tools that were activated.
    pub fn activate(&self, group_or_name: &str) -> Vec<String> {
        let mut deferred = self.deferred.lock().unwrap_or_else(|e| e.into_inner());
        let mut activated = Vec::new();

        if let Some(info) = policy::tool_group_info(group_or_name) {
            for &tool in info.tools {
                if deferred.remove(tool) {
                    activated.push(tool.to_string());
                }
            }
        } else if deferred.remove(group_or_name) {
            activated.push(group_or_name.to_string());
        }

        if !activated.is_empty() {
            self.invalidate_cache_shared();
        }
        activated
    }

    /// Returns info about currently deferred tool groups for the activate_tools tool.
    pub fn deferred_groups(&self) -> Vec<(String, String, usize)> {
        let deferred = self.deferred.lock().unwrap_or_else(|e| e.into_inner());
        if deferred.is_empty() {
            return Vec::new();
        }

        let mut groups = Vec::new();
        for info in policy::TOOL_GROUPS {
            let count = info.tools.iter().filter(|&&t| deferred.contains(t)).count();
            if count > 0 {
                groups.push((info.name.to_string(), info.description.to_string(), count));
            }
        }

        // Also list individually deferred tools not in any group
        let grouped: HashSet<&str> = policy::TOOL_GROUPS
            .iter()
            .flat_map(|g| g.tools.iter().copied())
            .collect();
        for name in deferred.iter() {
            if !grouped.contains(name.as_str()) {
                groups.push((name.clone(), "Plugin tool".to_string(), 1));
            }
        }
        groups
    }

    /// Whether any tools are currently deferred.
    pub fn has_deferred(&self) -> bool {
        !self
            .deferred
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty()
    }

    // -- LRU auto-eviction --------------------------------------------------

    /// Mark a set of tool names as "base" -- never auto-evicted.
    pub fn set_base_tools(&mut self, names: impl IntoIterator<Item = impl Into<String>>) {
        self.lifecycle
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .set_base_tools(names);
    }

    /// Add more tool names to the base set (extends, does not replace).
    pub fn add_base_tools(&mut self, names: impl IntoIterator<Item = impl Into<String>>) {
        self.lifecycle
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .add_base_tools(names);
    }

    /// Record that a tool was used (called from execute()).
    fn record_usage(&self, name: &str) {
        self.lifecycle
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .record_usage(name);
    }

    /// Advance the iteration counter. Called before each LLM call.
    pub fn tick(&self) {
        self.lifecycle
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .tick();
    }

    /// Auto-evict idle non-base tools if active count exceeds threshold.
    /// Returns the names of evicted tools (for logging).
    ///
    /// Lock ordering: lifecycle -> deferred (consistent with record_usage
    /// which only takes lifecycle, never both).
    pub fn auto_evict(&self) -> Vec<String> {
        // 1. Compute eviction candidates (lifecycle lock only)
        let to_evict = {
            let lifecycle = self.lifecycle.lock().unwrap_or_else(|e| e.into_inner());
            let deferred = self.deferred.lock().unwrap_or_else(|e| e.into_inner());
            let active: Vec<&str> = self
                .tools
                .keys()
                .filter(|n| !deferred.contains(n.as_str()))
                .map(|n| n.as_str())
                .collect();
            lifecycle.find_evictable(&active)
            // Both locks dropped here
        };

        if to_evict.is_empty() {
            return Vec::new();
        }

        // 2. Apply evictions (deferred lock only)
        {
            let mut deferred = self.deferred.lock().unwrap_or_else(|e| e.into_inner());
            for name in &to_evict {
                deferred.insert(name.clone());
            }
        }
        self.invalidate_cache_shared();

        to_evict
    }

    // -- Cache management ---------------------------------------------------

    /// Clear the cached specs (called by mutation methods with &mut self).
    fn invalidate_cache(&mut self) {
        // &mut self guarantees exclusive access, so get_mut() bypasses the mutex.
        *self
            .cached_specs
            .get_mut()
            .unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// Clear the cached specs through &self (for interior-mutability callers).
    fn invalidate_cache_shared(&self) {
        *self.cached_specs.lock().unwrap_or_else(|e| e.into_inner()) = None;
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
            if let policy::PolicyDecision::Deny { reason } = policy.evaluate(name) {
                eyre::bail!("tool '{}' denied by provider policy ({})", name, reason);
            }
        }

        // Auto-activate deferred tools on first use -- no need for the LLM
        // to call activate_tools first. This prevents the retry loop where
        // the LLM keeps calling a deferred tool and getting errors.
        {
            let deferred = self.deferred.lock().unwrap_or_else(|e| e.into_inner());
            if deferred.contains(name) {
                drop(deferred);
                // Find which group this tool belongs to and activate the whole group
                let group = policy::TOOL_GROUPS
                    .iter()
                    .find(|g| g.tools.contains(&name))
                    .map(|g| g.name);
                if let Some(group_name) = group {
                    let activated = self.activate(group_name);
                    tracing::info!(
                        tool = name,
                        group = group_name,
                        activated = %activated.join(", "),
                        "auto-activated deferred tool on first use"
                    );
                } else {
                    // Not in any group -- activate individually
                    let mut deferred = self.deferred.lock().unwrap_or_else(|e| e.into_inner());
                    deferred.remove(name);
                    drop(deferred);
                    self.invalidate_cache_shared();
                    tracing::info!(tool = name, "auto-activated deferred tool (no group)");
                }
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

        // Track usage for LRU auto-eviction
        self.record_usage(name);

        tool.execute_with_context(ctx, args).await
    }
}

impl ToolRegistry {
    /// Create a registry with built-in tools for the given working directory.
    pub fn with_builtins(cwd: impl AsRef<Path>) -> Self {
        Self::with_builtins_and_sandbox(cwd, Box::new(NoSandbox))
    }

    /// Create a registry with built-in tools and a custom sandbox for shell commands.
    pub fn with_builtins_and_sandbox(cwd: impl AsRef<Path>, sandbox: Box<dyn Sandbox>) -> Self {
        let cwd = cwd.as_ref();
        let mut registry = Self::new();
        registry.workspace_root = Some(cwd.to_path_buf());
        registry.register(ShellTool::new(cwd).with_sandbox(sandbox));
        registry.register(ReadFileTool::new(cwd));
        registry.register(DiffEditTool::new(cwd));
        registry.register(EditFileTool::new(cwd));
        registry.register(WriteFileTool::new(cwd));
        registry.register(GlobTool::new(cwd));
        registry.register(GrepTool::new(cwd));
        registry.register(ListDirTool::new(cwd));
        registry.register(WebSearchTool::new());
        registry.register(WebFetchTool::new());
        registry.register(BrowserTool::new());
        registry.register(CheckWorkspaceContractTool::new(cwd));
        registry.register(WorkspaceLogTool::new(cwd));
        registry.register(WorkspaceShowTool::new(cwd));
        registry.register(WorkspaceDiffTool::new(cwd));
        #[cfg(feature = "git")]
        registry.register(super::GitTool::new(cwd));
        #[cfg(feature = "ast")]
        registry.register(CodeStructureTool::new(cwd));
        registry
    }

    /// Tool names that are bound to a working directory (cwd / base_dir).
    /// Used by `rebind_cwd()` to re-register these tools with a new workspace path.
    pub const CWD_BOUND_TOOLS: &'static [&'static str] = &[
        "shell",
        "read_file",
        "write_file",
        "edit_file",
        "diff_edit",
        "glob",
        "grep",
        "list_dir",
        "check_workspace_contract",
        "workspace_log",
        "workspace_show",
        "workspace_diff",
        #[cfg(feature = "git")]
        "git",
        #[cfg(feature = "ast")]
        "code_structure",
    ];

    /// Create a copy of this registry with all cwd-bound tools re-registered
    /// to use a new working directory and sandbox. Non-cwd tools (web_search,
    /// web_fetch, browser, MCP, plugins, etc.) are preserved via Arc cloning.
    pub fn rebind_cwd(&self, cwd: impl AsRef<Path>, sandbox: Box<dyn Sandbox>) -> Self {
        let cwd = cwd.as_ref();
        // Clone everything except cwd-bound tools
        let mut registry = self.snapshot_excluding(Self::CWD_BOUND_TOOLS);
        registry.workspace_root = Some(cwd.to_path_buf());
        // Re-register cwd-bound tools with the new workspace
        registry.register(ShellTool::new(cwd).with_sandbox(sandbox));
        registry.register(ReadFileTool::new(cwd));
        registry.register(DiffEditTool::new(cwd));
        registry.register(EditFileTool::new(cwd));
        registry.register(WriteFileTool::new(cwd));
        registry.register(GlobTool::new(cwd));
        registry.register(GrepTool::new(cwd));
        registry.register(ListDirTool::new(cwd));
        registry.register(CheckWorkspaceContractTool::new(cwd));
        registry.register(WorkspaceLogTool::new(cwd));
        registry.register(WorkspaceShowTool::new(cwd));
        registry.register(WorkspaceDiffTool::new(cwd));
        #[cfg(feature = "git")]
        registry.register(super::GitTool::new(cwd));
        #[cfg(feature = "ast")]
        registry.register(CodeStructureTool::new(cwd));
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
        let base_task = registry.register_task("deep_search", "call-base");

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

        let rebound_task = rebound.register_task("deep_search", "call-rebound");
        let rebound_task = rebound
            .supervisor()
            .get_task(&rebound_task)
            .expect("rebound task should be tracked");
        assert!(
            rebound_task.session_key.is_none(),
            "session key must be supplied by the new session actor, not inherited"
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
mod lifecycle_tests {
    use super::*;
    use std::path::PathBuf;

    fn make_registry(max_active: usize, idle_threshold: u32) -> ToolRegistry {
        let mut reg = ToolRegistry::with_builtins(PathBuf::from("/tmp"));
        {
            let lc = reg.lifecycle.get_mut().unwrap();
            lc.max_active = max_active;
            lc.idle_threshold = idle_threshold;
        }
        reg
    }

    fn active_tool_names(reg: &ToolRegistry) -> Vec<String> {
        let mut names: Vec<String> = reg.specs().iter().map(|s| s.name.clone()).collect();
        names.sort();
        names
    }

    fn deferred_tool_names(reg: &ToolRegistry) -> Vec<String> {
        let deferred = reg.deferred.lock().unwrap();
        let mut names: Vec<String> = deferred.iter().cloned().collect();
        names.sort();
        names
    }

    #[test]
    fn idle_tools_evicted_when_over_threshold() {
        let mut reg = make_registry(3, 2);
        reg.set_base_tools(["read_file", "write_file"]);

        let initial_count = reg.specs().len();
        println!("Initial active tools: {initial_count}");
        assert!(initial_count > 3, "builtins should exceed threshold");

        for _ in 0..3 {
            reg.tick();
            reg.record_usage("read_file");
            reg.record_usage("write_file");
        }

        let evicted = reg.auto_evict();
        println!("Evicted: {evicted:?}");
        assert!(!evicted.is_empty(), "should evict idle tools");

        let active = active_tool_names(&reg);
        assert!(
            active.contains(&"read_file".to_string()),
            "base tool read_file must survive"
        );
        assert!(
            active.contains(&"write_file".to_string()),
            "base tool write_file must survive"
        );

        let deferred = deferred_tool_names(&reg);
        for name in &evicted {
            assert!(
                deferred.contains(name),
                "{name} should be deferred after eviction"
            );
        }

        println!(
            "After eviction -- active: {}, deferred: {}",
            active.len(),
            deferred.len()
        );
        assert!(active.len() <= 3, "should be at or under threshold");
    }

    #[test]
    fn recently_used_tools_not_evicted() {
        let mut reg = make_registry(3, 2);
        reg.set_base_tools(["read_file"]);

        for _ in 0..3 {
            reg.tick();
            reg.record_usage("read_file");
            reg.record_usage("shell");
        }

        let evicted = reg.auto_evict();
        println!("Evicted: {evicted:?}");

        assert!(
            !evicted.contains(&"shell".to_string()),
            "recently used 'shell' should not be evicted"
        );

        let active = active_tool_names(&reg);
        assert!(
            active.contains(&"shell".to_string()),
            "shell must remain active"
        );
    }

    #[tokio::test]
    async fn activated_tool_gets_usage_tracking() {
        let mut reg = make_registry(3, 2);
        reg.set_base_tools(["read_file", "write_file"]);

        for _ in 0..3 {
            reg.tick();
            reg.record_usage("read_file");
            reg.record_usage("write_file");
        }
        let evicted = reg.auto_evict();
        println!("First eviction: {evicted:?}");
        assert!(!evicted.is_empty());

        let deferred = deferred_tool_names(&reg);
        let shell_was_evicted = deferred.contains(&"shell".to_string());
        println!("shell deferred: {shell_was_evicted}");

        if shell_was_evicted {
            let activated = reg.activate("group:runtime");
            println!("Activated: {activated:?}");
            assert!(activated.contains(&"shell".to_string()));

            reg.tick();
            reg.record_usage("shell");

            let evicted2 = reg.auto_evict();
            assert!(
                !evicted2.contains(&"shell".to_string()),
                "freshly used shell should survive eviction"
            );
            println!("Second eviction (shell survived): {evicted2:?}");
        }
    }

    #[test]
    fn base_tools_never_evicted() {
        let mut reg = make_registry(2, 1);
        reg.set_base_tools(["read_file", "write_file", "shell"]);

        for _ in 0..5 {
            reg.tick();
        }

        let evicted = reg.auto_evict();
        println!("Evicted: {evicted:?}");

        for name in &["read_file", "write_file", "shell"] {
            assert!(
                !evicted.contains(&name.to_string()),
                "base tool {name} must never be evicted"
            );
        }
    }

    #[test]
    fn stalest_evicted_first() {
        let mut reg = make_registry(5, 2);
        reg.set_base_tools(["read_file"]);

        reg.tick();
        reg.record_usage("read_file");
        reg.record_usage("shell");
        reg.record_usage("write_file");
        reg.record_usage("edit_file");
        reg.record_usage("glob");
        reg.record_usage("grep");
        reg.record_usage("list_dir");

        for _ in 0..3 {
            reg.tick();
            reg.record_usage("shell");
            reg.record_usage("write_file");
        }

        let evicted = reg.auto_evict();
        println!("Evicted: {evicted:?}");

        if !evicted.is_empty() {
            assert!(
                !evicted.contains(&"shell".to_string()),
                "shell (iter 4) should survive over stale tools"
            );
            assert!(
                !evicted.contains(&"write_file".to_string()),
                "write_file (iter 4) should survive over stale tools"
            );
        }
    }

    #[test]
    fn no_eviction_when_under_threshold() {
        let reg = make_registry(100, 1);

        for _ in 0..5 {
            reg.tick();
        }

        let evicted = reg.auto_evict();
        assert!(evicted.is_empty(), "should not evict when under threshold");
    }

    #[tokio::test]
    async fn full_session_lifecycle() {
        let mut reg = make_registry(5, 3);
        reg.set_base_tools(["read_file", "write_file"]);

        println!("=== Turn 1: Research query ===");
        reg.tick();
        reg.record_usage("read_file");
        reg.record_usage("shell");
        let active = active_tool_names(&reg);
        println!("Active ({}): {:?}", active.len(), active);

        println!("\n=== Turns 2-4: Only using read/write ===");
        for i in 2..=4 {
            reg.tick();
            reg.record_usage("read_file");
            reg.record_usage("write_file");
            let evicted = reg.auto_evict();
            if !evicted.is_empty() {
                println!("Turn {i} evicted: {evicted:?}");
            }
        }
        let active = active_tool_names(&reg);
        let deferred = deferred_tool_names(&reg);
        println!(
            "After turn 4 -- active: {}, deferred: {}",
            active.len(),
            deferred.len()
        );

        println!("\n=== Turn 5: Need shell again -- re-activate ===");
        if deferred.contains(&"shell".to_string()) {
            let activated = reg.activate("group:runtime");
            println!("Activated: {activated:?}");
        }
        reg.tick();
        reg.record_usage("shell");
        let active = active_tool_names(&reg);
        println!(
            "Active after re-activation ({}): {:?}",
            active.len(),
            active
        );
        assert!(
            active.contains(&"shell".to_string()),
            "shell should be active again"
        );

        println!("\n=== Turn 6-8: Use shell, others go idle ===");
        for i in 6..=8 {
            reg.tick();
            reg.record_usage("read_file");
            reg.record_usage("shell");
            let evicted = reg.auto_evict();
            if !evicted.is_empty() {
                println!("Turn {i} evicted: {evicted:?}");
            }
        }
        let active = active_tool_names(&reg);
        let deferred = deferred_tool_names(&reg);
        println!(
            "\nFinal state -- active: {}, deferred: {}",
            active.len(),
            deferred.len()
        );
        println!("Active: {:?}", active);
        println!("Deferred: {:?}", deferred);

        assert!(active.contains(&"read_file".to_string()));
        assert!(active.contains(&"write_file".to_string()));
        assert!(active.contains(&"shell".to_string()));
    }

    #[test]
    fn spawn_only_message_uses_runtime_output_dir_hint() {
        let mut reg = make_registry(5, 3);
        reg.mark_spawn_only("mofa_slides", None);
        reg.set_output_dir_hint("/tmp/octos-profile/skill-output");

        let msg = reg.spawn_only_message("mofa_slides");

        assert!(msg.contains("Output directory: /tmp/octos-profile/skill-output/"));
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
    fn coding_profile_produces_same_registry_as_default_builtins() {
        // Behaviour parity gate: applying the built-in `coding` profile
        // to a builtin registry must leave the registry IDENTICAL to
        // what today's no-flag default path produces. This is the
        // critical regression guard called out in the M8.3 issue.
        use crate::profile::ProfileDefinition;

        let dir = tempfile::tempdir().expect("tempdir");
        let reference = ToolRegistry::with_builtins(dir.path());
        let reference_names = builtin_names(&reference);

        let coding = ProfileDefinition::builtin("coding").expect("coding builtin");
        let mut profiled = ToolRegistry::with_builtins(dir.path());
        coding.apply_to_registry(&mut profiled);

        let profiled_names = builtin_names(&profiled);
        assert_eq!(
            reference_names, profiled_names,
            "coding profile must preserve behaviour parity with the default path",
        );
    }
}
