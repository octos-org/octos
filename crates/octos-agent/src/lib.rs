//! Agent runtime, tool execution, and coordination for octos.
//!
//! This crate provides:
//! - Agent struct that runs the agent loop
//! - Tool router for dispatching tool calls
//! - Command policy for approval before execution
//! - Progress reporting for real-time updates
//! - Integration with codex sandboxing (when enabled)

mod agent;
pub mod bootstrap;
pub mod builtin_skills;
pub mod bundled_app_skills;
mod compaction;
pub mod event_bus;
pub mod exec_env;
pub mod hooks;
pub mod loop_detect;
pub mod mcp;
pub mod plugins;
pub mod policy;
pub mod progress;
pub mod prompt_guard;
pub mod prompt_layer;
pub mod provider_tools;
pub mod sandbox;
mod sanitize;
pub mod session;
pub mod skills;
pub mod steering;
pub mod tools;
pub mod turn;

pub use agent::{
    Agent, AgentConfig, ConversationResponse, DEFAULT_SESSION_TIMEOUT_SECS,
    DEFAULT_TOOL_TIMEOUT_SECS, DEFAULT_WORKER_PROMPT, MAX_TOOL_TIMEOUT_SECS, TASK_REPORTER,
    TokenTracker,
};
pub use event_bus::{EventBus, EventSubscriber};
pub use exec_env::{DockerEnvironment, ExecEnvironment, ExecOutput, LocalEnvironment};
pub use hooks::{HookConfig, HookContext, HookEvent, HookExecutor};
pub use mcp::{McpClient, McpServerConfig};
pub use plugins::{PluginLoadResult, PluginLoader};
pub use progress::{ConsoleReporter, ProgressEvent, ProgressReporter, SilentReporter};
pub use prompt_layer::PromptLayerBuilder;
pub use provider_tools::{ProviderToolsets, ToolAdjustment};
pub use sandbox::{Sandbox, SandboxConfig, SandboxMode, create_sandbox};
pub use session::{SessionLimits, SessionState, SessionStateHandle, SessionUsage};
pub use skills::{SkillInfo, SkillsLoader};
pub use steering::{SteeringMessage, SteeringReceiver, SteeringSender};
pub use tools::{
    ActivateToolsTool, BrowserTool, ConfigureToolTool, DeepSearchTool, DiffEditTool, EditFileTool,
    GlobTool, GrepTool, ListDirTool, ManageSkillsTool, MessageTool, ReadFileTool, RecallMemoryTool,
    SaveMemoryTool, SendFileTool, ShellTool, SpawnTool, SynthesizeResearchTool, TakePhotoTool,
    Tool, ToolConfigStore, ToolPolicy, ToolRegistry, ToolResult, WebFetchTool, WebSearchTool,
    WriteFileTool,
    admin::{AdminApiContext, register_admin_api_tools},
};
pub use turn::{Turn, TurnKind, turns_to_messages};

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn setup_test_dir() -> TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("test.rs"),
            "fn main() {\n    println!(\"hello\");\n}\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("lib.rs"),
            "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n",
        )
        .unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("src/module.rs"),
            "// Module\npub struct Foo;\n",
        )
        .unwrap();
        dir
    }

    #[tokio::test]
    async fn test_glob_tool() {
        let dir = setup_test_dir();
        let tool = GlobTool::new(dir.path());

        let result = tool
            .execute(&serde_json::json!({"pattern": "*.rs"}))
            .await
            .unwrap();

        assert!(result.success);
        assert!(result.output.contains("test.rs"));
        assert!(result.output.contains("lib.rs"));
    }

    #[tokio::test]
    async fn test_glob_recursive() {
        let dir = setup_test_dir();
        let tool = GlobTool::new(dir.path());

        let result = tool
            .execute(&serde_json::json!({"pattern": "**/*.rs"}))
            .await
            .unwrap();

        assert!(result.success);
        assert!(
            result.output.contains("src/module.rs") || result.output.contains("src\\module.rs")
        );
    }

    #[tokio::test]
    async fn test_grep_tool() {
        let dir = setup_test_dir();
        let tool = GrepTool::new(dir.path());

        let result = tool
            .execute(&serde_json::json!({"pattern": "println"}))
            .await
            .unwrap();

        assert!(result.success);
        assert!(result.output.contains("test.rs"));
        assert!(result.output.contains("println"));
    }

    #[tokio::test]
    async fn test_grep_with_context() {
        let dir = setup_test_dir();
        let tool = GrepTool::new(dir.path());

        let result = tool
            .execute(&serde_json::json!({"pattern": "add", "context": 1}))
            .await
            .unwrap();

        assert!(result.success);
        // Should include surrounding lines
        assert!(result.output.contains("pub fn"));
    }

    #[tokio::test]
    async fn test_grep_case_insensitive() {
        let dir = setup_test_dir();
        let tool = GrepTool::new(dir.path());

        let result = tool
            .execute(&serde_json::json!({"pattern": "FOO", "ignore_case": true}))
            .await
            .unwrap();

        assert!(result.success);
        assert!(result.output.contains("Foo"));
    }

    #[tokio::test]
    async fn test_read_file_tool() {
        let dir = setup_test_dir();
        let tool = ReadFileTool::new(dir.path());

        let result = tool
            .execute(&serde_json::json!({"path": "test.rs"}))
            .await
            .unwrap();

        assert!(result.success);
        assert!(result.output.contains("fn main()"));
    }

    #[tokio::test]
    async fn test_write_file_tool() {
        let dir = setup_test_dir();
        let tool = WriteFileTool::new(dir.path());

        let result = tool
            .execute(&serde_json::json!({
                "path": "new_file.rs",
                "content": "// New file\n"
            }))
            .await
            .unwrap();

        assert!(result.success);
        assert!(result.file_modified.is_some());
        assert!(dir.path().join("new_file.rs").exists());
    }

    #[tokio::test]
    async fn test_tool_registry() {
        let dir = setup_test_dir();
        let registry = ToolRegistry::with_builtins(dir.path());

        // Should have all builtin tools
        let specs = registry.specs();
        let names: Vec<_> = specs.iter().map(|s| s.name.as_str()).collect();

        assert!(names.contains(&"shell"));
        assert!(names.contains(&"read_file"));
        assert!(names.contains(&"write_file"));
        assert!(names.contains(&"edit_file"));
        assert!(names.contains(&"glob"));
        assert!(names.contains(&"grep"));
        assert!(names.contains(&"list_dir"));
        assert!(names.contains(&"web_search"));
        assert!(names.contains(&"web_fetch"));
    }

    #[tokio::test]
    async fn test_registry_execute() {
        let dir = setup_test_dir();
        let registry = ToolRegistry::with_builtins(dir.path());

        let result = registry
            .execute("read_file", &serde_json::json!({"path": "test.rs"}))
            .await
            .unwrap();

        assert!(result.success);
        assert!(result.output.contains("fn main()"));
    }

    #[tokio::test]
    async fn test_glob_rejects_absolute_pattern() {
        let dir = setup_test_dir();
        let tool = GlobTool::new(dir.path());
        let result = tool
            .execute(&serde_json::json!({"pattern": "/etc/passwd"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.output.contains("not allowed"));
    }

    #[tokio::test]
    async fn test_glob_rejects_parent_traversal() {
        let dir = setup_test_dir();
        let tool = GlobTool::new(dir.path());
        let result = tool
            .execute(&serde_json::json!({"pattern": "../../etc/*"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.output.contains("not allowed"));
    }

    #[tokio::test]
    async fn test_grep_rejects_absolute_file_pattern() {
        let dir = setup_test_dir();
        let tool = GrepTool::new(dir.path());
        let result = tool
            .execute(&serde_json::json!({"pattern": "fn", "file_pattern": "/etc/*.conf"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.output.contains("not allowed"));
    }

    #[tokio::test]
    async fn test_list_dir_rejects_traversal() {
        let dir = setup_test_dir();
        let tool = ListDirTool::new(dir.path());
        let result = tool
            .execute(&serde_json::json!({"path": "../../.."}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.output.contains("Path outside"));
    }

    #[tokio::test]
    async fn test_web_fetch_rejects_localhost() {
        let tool = WebFetchTool::new();
        let result = tool
            .execute(&serde_json::json!({"url": "http://localhost:8080/admin"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.output.contains("private"));
    }

    #[tokio::test]
    async fn test_web_fetch_rejects_private_ip() {
        let tool = WebFetchTool::new();
        let result = tool
            .execute(&serde_json::json!({"url": "http://169.254.169.254/latest/meta-data"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.output.contains("private"));
    }

    #[tokio::test]
    async fn test_registry_unknown_tool() {
        let dir = setup_test_dir();
        let registry = ToolRegistry::with_builtins(dir.path());

        let result = registry
            .execute("nonexistent", &serde_json::json!({}))
            .await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_context_filter_restricts_specs() {
        let dir = setup_test_dir();
        let mut registry = ToolRegistry::with_builtins(dir.path());
        let all_count = registry.specs().len();

        // Only allow tools tagged "search"
        registry.set_context_filter(vec!["search".to_string()]);
        let specs = registry.specs();
        let names: Vec<_> = specs.iter().map(|s| s.name.as_str()).collect();

        // grep and glob have "search" tag — should be included
        assert!(names.contains(&"grep"));
        assert!(names.contains(&"glob"));
        // web_search has "web" tag only — should be filtered out
        assert!(!names.contains(&"web_search"));
        // shell has "runtime","code" tags — should be filtered out
        assert!(!names.contains(&"shell"));
        // Filtered count should be less than total
        assert!(specs.len() < all_count);
    }

    #[tokio::test]
    async fn test_oversized_args_rejected() {
        let dir = setup_test_dir();
        let registry = ToolRegistry::with_builtins(dir.path());

        // Create args larger than 1MB
        let big_string = "x".repeat(1_100_000);
        let result = registry
            .execute("read_file", &serde_json::json!({"path": big_string}))
            .await;

        match result {
            Err(e) => assert!(e.to_string().contains("too large")),
            Ok(_) => panic!("should reject oversized args"),
        }
    }

    #[test]
    fn test_registry_retain() {
        let dir = setup_test_dir();
        let mut registry = ToolRegistry::with_builtins(dir.path());
        let initial_count = registry.len();

        registry.retain(|name| name == "shell" || name == "read_file");
        assert_eq!(registry.len(), 2);
        assert!(registry.len() < initial_count);

        let specs = registry.specs();
        let names: Vec<_> = specs.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"shell"));
        assert!(names.contains(&"read_file"));
    }

    #[test]
    fn test_registry_is_empty() {
        let registry = ToolRegistry::new();
        assert!(registry.is_empty());
        assert_eq!(registry.len(), 0);
    }

    #[test]
    fn test_specs_cache_invalidated_on_register() {
        let mut registry = ToolRegistry::new();
        let specs1 = registry.specs();
        assert!(specs1.is_empty());

        registry.register(ReadFileTool::new("/tmp"));
        let specs2 = registry.specs();
        assert_eq!(specs2.len(), 1);
    }

    #[tokio::test]
    async fn test_provider_policy_filters_specs() {
        let dir = setup_test_dir();
        let mut registry = ToolRegistry::with_builtins(dir.path());
        let all_count = registry.specs().len();

        // Set provider policy that denies diff_edit and web_search
        let policy: ToolPolicy = serde_json::from_value(serde_json::json!({
            "deny": ["diff_edit", "web_search"]
        }))
        .unwrap();
        registry.set_provider_policy(policy);

        let filtered = registry.specs();
        let names: Vec<_> = filtered.iter().map(|s| s.name.as_str()).collect();
        assert!(!names.contains(&"diff_edit"));
        assert!(!names.contains(&"web_search"));
        assert!(names.contains(&"shell"));
        assert!(names.contains(&"read_file"));
        assert_eq!(filtered.len(), all_count - 2);

        // Allowed tools can still be executed
        let result = registry
            .execute("read_file", &serde_json::json!({"path": "test.rs"}))
            .await
            .unwrap();
        assert!(result.success);

        // Denied tools are blocked at execution time too
        match registry.execute("diff_edit", &serde_json::json!({})).await {
            Err(e) => assert!(e.to_string().contains("denied by provider policy")),
            Ok(_) => panic!("should be denied by provider policy"),
        }

        // Tools still registered internally (len unchanged)
        assert_eq!(registry.len(), all_count);
    }
}
