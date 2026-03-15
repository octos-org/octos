//! Tool framework for agent tool execution.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use crew_core::TokenUsage;
use crew_llm::ToolSpec;
use eyre::Result;

use crate::progress::ProgressReporter;

/// Execution context available to tools via task-local.
/// Set by the agent before each tool invocation so plugin tools
/// can report progress without changing the Tool trait signature.
#[derive(Clone)]
pub struct ToolContext {
    pub tool_id: String,
    pub reporter: Arc<dyn ProgressReporter>,
}

tokio::task_local! {
    /// Task-local tool context, scoped per tool invocation in agent.rs.
    pub static TOOL_CTX: ToolContext;
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

/// Result of executing a tool.
#[derive(Default)]
pub struct ToolResult {
    /// Output to return to the LLM.
    pub output: String,
    /// Whether the tool execution succeeded.
    pub success: bool,
    /// File modified by this tool (if any).
    pub file_modified: Option<PathBuf>,
    /// Tokens used by this tool (for subagent tools).
    pub tokens_used: Option<TokenUsage>,
}

/// Trait for implementing tools.
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
    async fn execute(&self, args: &serde_json::Value) -> Result<ToolResult>;

    /// Downcast support for concrete tool access (e.g. wiring ActivateToolsTool).
    fn as_any(&self) -> &dyn std::any::Any {
        // Default: no downcasting. Override in tools that need it.
        &()
    }
}

/// Registry of available tools.
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
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
            provider_policy: None,
            context_filter: None,
            cached_specs: std::sync::Mutex::new(None),
            deferred: std::sync::Mutex::new(HashSet::new()),
        }
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
    pub fn retain(&mut self, f: impl Fn(&str) -> bool) {
        self.tools.retain(|name, _| f(name));
        self.invalidate_cache();
    }

    /// Remove tools not permitted by the given policy.
    pub fn apply_policy(&mut self, policy: &ToolPolicy) {
        if policy.is_empty() {
            return;
        }
        self.retain(|name| policy.is_allowed(name));
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
    /// Provider policy and context filter are also copied.
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
        Self {
            tools,
            provider_policy: self.provider_policy.clone(),
            context_filter: self.context_filter.clone(),
            cached_specs: std::sync::Mutex::new(None),
            deferred: std::sync::Mutex::new(deferred),
        }
    }

    // ── Deferred tool activation ──────────────────────────────────────

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

    // ── Cache management ────────────────────────────────────────────

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
    pub async fn execute(&self, name: &str, args: &serde_json::Value) -> Result<ToolResult> {
        if let Some(ref policy) = self.provider_policy {
            if !policy.is_allowed(name) {
                eyre::bail!("tool '{}' denied by provider policy", name);
            }
        }

        if self
            .deferred
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(name)
        {
            eyre::bail!(
                "tool '{}' is deferred; call activate_tools to activate it first",
                name
            );
        }

        // Reject oversized arguments (1 MB limit).
        // Use a counting writer to avoid allocating the full serialized string,
        // which could OOM on deeply nested JSON before the check triggers.
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
        tool.execute(args).await
    }
}

// Tool policy
pub mod policy;
pub use policy::ToolPolicy;

// Shared SSRF protection
pub mod ssrf;

// Built-in tools
pub mod deep_search;
pub mod diff_edit;
pub mod edit_file;
pub mod glob_tool;
pub mod grep_tool;
pub mod list_dir;
pub mod manage_skills;
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
pub mod take_photo;
pub mod web_fetch;
pub mod web_search;
pub mod write_file;

pub mod activate_tools;
pub mod admin;
pub mod browser;
pub mod tool_config;

#[cfg(feature = "git")]
pub mod git;

#[cfg(feature = "ast")]
pub mod code_structure;

pub use deep_search::DeepSearchTool;
pub use diff_edit::DiffEditTool;
pub use edit_file::EditFileTool;
pub use glob_tool::GlobTool;
pub use grep_tool::GrepTool;
pub use list_dir::ListDirTool;
pub use manage_skills::ManageSkillsTool;
pub use message::MessageTool;
pub use read_file::ReadFileTool;
pub use recall_memory::RecallMemoryTool;
pub use save_memory::SaveMemoryTool;
pub use send_file::SendFileTool;
pub use shell::ShellTool;
pub use spawn::SpawnTool;
pub use synthesize_research::SynthesizeResearchTool;
pub use take_photo::TakePhotoTool;
pub use web_fetch::WebFetchTool;
pub use web_search::WebSearchTool;
pub use write_file::WriteFileTool;

pub use activate_tools::ActivateToolsTool;
pub use browser::BrowserTool;
pub use tool_config::{ConfigureToolTool, ToolConfigStore};

#[cfg(feature = "git")]
pub use git::GitTool;

#[cfg(feature = "ast")]
pub use code_structure::CodeStructureTool;

use std::path::{Component, Path};

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

impl ToolRegistry {
    /// Create a registry with built-in tools for the given working directory.
    pub fn with_builtins(cwd: impl AsRef<Path>) -> Self {
        Self::with_builtins_and_sandbox(cwd, Box::new(NoSandbox))
    }

    /// Create a registry with built-in tools and a custom sandbox for shell commands.
    pub fn with_builtins_and_sandbox(cwd: impl AsRef<Path>, sandbox: Box<dyn Sandbox>) -> Self {
        let cwd = cwd.as_ref();
        let mut registry = Self::new();
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
        #[cfg(feature = "git")]
        registry.register(GitTool::new(cwd));
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
        // Re-register cwd-bound tools with the new workspace
        registry.register(ShellTool::new(cwd).with_sandbox(sandbox));
        registry.register(ReadFileTool::new(cwd));
        registry.register(DiffEditTool::new(cwd));
        registry.register(EditFileTool::new(cwd));
        registry.register(WriteFileTool::new(cwd));
        registry.register(GlobTool::new(cwd));
        registry.register(GrepTool::new(cwd));
        registry.register(ListDirTool::new(cwd));
        #[cfg(feature = "git")]
        registry.register(GitTool::new(cwd));
        #[cfg(feature = "ast")]
        registry.register(CodeStructureTool::new(cwd));
        registry
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
        // Simulate per-profile isolation: cwd = ~/.crew/profiles/alice/data
        let base = Path::new("/home/user/.crew/profiles/alice/data");

        // Trying to read another profile's data via traversal
        assert!(resolve_path(base, "../../bob/data/sessions/secret").is_err());
        assert!(resolve_path(base, "../../../profiles/bob/data/episodes.db").is_err());

        // Trying to reach shared crew home
        assert!(resolve_path(base, "../../../skills/evil-skill/main").is_err());

        // Valid access within own data dir
        assert!(resolve_path(base, "skills/my-skill/main").is_ok());
        assert!(resolve_path(base, "sessions/chat-123.json").is_ok());
        assert!(resolve_path(base, "skill-output/report.pdf").is_ok());
    }

    #[test]
    fn test_resolve_rejects_empty_path() {
        let base = Path::new("/home/user/project");
        // Empty string should resolve to the base dir itself, which is valid
        // (it's not a traversal). Verify it doesn't panic.
        let result = resolve_path(base, "");
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), PathBuf::from("/home/user/project"));
    }

    #[test]
    fn test_resolve_rejects_null_byte() {
        let base = Path::new("/home/user/project");
        // Null bytes in paths are invalid on Unix — verify resolve_path
        // handles them without panicking. The OS will reject the path.
        let result = resolve_path(base, "file\0.txt");
        // On Unix, Path::new("file\0.txt") is valid at the Rust level but
        // normalize_path will produce a path that starts_with base, so it
        // passes. The actual OS call (open/stat) would reject it.
        // The key security property: it doesn't escape the base directory.
        if let Ok(p) = &result {
            assert!(p.starts_with(base));
        }
    }

    #[test]
    fn test_resolve_rejects_windows_separators() {
        let base = Path::new("/home/user/project");
        // Backslash is a valid filename char on Unix, not a separator.
        // Verify it doesn't enable traversal.
        let result = resolve_path(base, "..\\..\\etc\\passwd");
        // On Unix this is a single filename component "..\..\etc\passwd",
        // which stays inside base.
        if let Ok(p) = &result {
            assert!(p.starts_with(base));
        }
    }
}
