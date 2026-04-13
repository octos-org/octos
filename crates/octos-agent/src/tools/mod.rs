//! Tool framework for agent tool execution.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use eyre::Result;
use octos_core::TokenUsage;

use crate::progress::ProgressReporter;

/// Execution context available to tools via task-local.
/// Set by the agent before each tool invocation so plugin tools
/// can report progress without changing the Tool trait signature.
#[derive(Clone)]
pub struct ToolContext {
    pub tool_id: String,
    pub reporter: Arc<dyn ProgressReporter>,
    pub attachment_paths: Vec<String>,
    pub audio_attachment_paths: Vec<String>,
    pub file_attachment_paths: Vec<String>,
}

tokio::task_local! {
    /// Task-local tool context, scoped per tool invocation in agent.rs.
    pub static TOOL_CTX: ToolContext;
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
pub mod check_background_tasks;
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
pub use spawn::{BackgroundResultKind, BackgroundResultPayload, SpawnTool};
pub use synthesize_research::SynthesizeResearchTool;
pub use take_photo::TakePhotoTool;
pub use web_fetch::WebFetchTool;
pub use web_search::WebSearchTool;
pub use write_file::WriteFileTool;

pub use activate_tools::ActivateToolsTool;
pub use browser::BrowserTool;
pub use check_background_tasks::CheckBackgroundTasksTool;
pub use tool_config::{ConfigureToolTool, ToolConfigStore};

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
