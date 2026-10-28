use octos_agent::ToolRegistry;
use serde_json::json;

#[test]
fn standard_dispatcher_keeps_distinct_capabilities_and_historical_entry_points() {
    let dir = tempfile::tempdir().unwrap();
    let registry = ToolRegistry::with_builtins(dir.path());
    assert_eq!(registry.specs().len(), 6);
    for name in [
        "glob",
        "grep",
        "list_dir",
        "apply_patch",
        "write_stdin",
        "shell",
        "web_search",
        "web_fetch",
        "browser",
        "wait_agent",
        "view_video",
        "bash",
        "diff_edit",
        "view_image",
        "tool_suggest",
        "request_user_input",
    ] {
        assert!(registry.get(name).is_some(), "lost native entry {name}");
        assert!(
            registry.is_tool_model_reachable(name),
            "lost capability {name}"
        );
    }
    assert_eq!(
        serde_json::to_value(registry.specs()).unwrap(),
        serde_json::to_value(registry.snapshot_excluding(&[]).specs()).unwrap()
    );
}

#[tokio::test]
async fn batch_read_returns_text_images_and_individual_errors() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "first\nsecond\nthird").unwrap();
    std::fs::write(dir.path().join("b.png"), b"\x89PNG\r\n\x1a\n").unwrap();
    let registry = ToolRegistry::with_builtins(dir.path());
    let result = registry
        .execute(
            "read_file",
            &json!({"files": [
                {"path": "a.txt", "start_line": 2, "limit": 1},
                {"path": "b.png"},
                {"path": "missing.txt"}
            ]}),
        )
        .await
        .unwrap();
    assert!(!result.success);
    assert!(result.output.contains("second"));
    assert!(!result.output.contains("third"));
    assert_eq!(result.model_media.len(), 1);
    assert!(result.output.contains("missing.txt"));
    assert_eq!(
        result.structured_metadata.unwrap()["reads"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
}

#[tokio::test]
async fn batch_read_enforces_scope_and_rejects_ambiguous_input() {
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(outside.path(), "OUTSIDE_SECRET").unwrap();
    let registry = ToolRegistry::with_builtins(dir.path());
    let result = registry
        .execute("read_file", &json!({"files": [{"path": outside.path()}]}))
        .await
        .unwrap();
    assert!(!result.success);
    assert!(!result.output.contains("OUTSIDE_SECRET"));
    assert!(
        registry
            .execute("read_file", &json!({"path":"a", "files":[]}))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn dispatcher_never_expands_a_host_allowlist() {
    let dir = tempfile::tempdir().unwrap();
    let registry = ToolRegistry::with_builtins(dir.path());
    registry.set_host_tool_allowlist(Some(&["view_image".into()]));
    assert_eq!(
        registry
            .specs()
            .iter()
            .map(|s| s.name.as_str())
            .collect::<Vec<_>>(),
        ["view_image"]
    );
    assert!(
        registry
            .execute("read_file", &json!({"files":[{"path":"a"}]}))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn web_fetch_batch_retains_ssrf_checks_and_rejects_nesting() {
    let dir = tempfile::tempdir().unwrap();
    let registry = ToolRegistry::with_builtins(dir.path());
    let result = registry
        .execute(
            "web_fetch",
            &json!({"requests":[{"url":"http://127.0.0.1/private"},{"url":"file:///etc/passwd"}]}),
        )
        .await
        .unwrap();
    assert!(!result.success);
    assert_eq!(
        result.structured_metadata.unwrap()["reads"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert!(
        registry
            .execute("web_fetch", &json!({"requests":[{"requests":[]}]}))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn truncated_batch_does_not_poison_later_file_cache_reads() {
    use octos_agent::file_state_cache::FileStateCache;
    use octos_agent::tools::ToolContext;
    use std::sync::Arc;
    let dir = tempfile::tempdir().unwrap();
    let content = "content line\n".repeat(6000);
    std::fs::write(dir.path().join("large.txt"), content).unwrap();
    let registry = ToolRegistry::with_builtins(dir.path());
    let ctx = ToolContext {
        file_state_cache: Some(Arc::new(FileStateCache::new())),
        ..ToolContext::zero()
    };
    let result = registry
        .execute_with_context(&ctx, "read_file", &json!({"files":[{"path":"large.txt"}]}))
        .await
        .unwrap();
    assert!(result.output.len() <= octos_core::tool_output_limit("read_file"));
    assert!(result.output.contains("truncated"));
    let next = registry
        .execute_with_context(
            &ctx,
            "read_file",
            &json!({"path":"large.txt", "start_line":5000, "limit":1}),
        )
        .await
        .unwrap();
    assert!(next.success);
    assert!(next.output.contains("content line"));
    assert!(!next.output.contains("unchanged"));
}
