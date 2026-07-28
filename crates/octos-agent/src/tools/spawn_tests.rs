use super::*;
#[cfg(unix)]
use crate::{HookConfig, HookEvent};

#[test]
fn frame_subagent_task_leads_with_identity_and_directive() {
    let out = frame_subagent_task("review-octos-web", "Clone and review the repo.");
    assert!(out.starts_with("You are a delegated SUB-AGENT named \"review-octos-web\""));
    assert!(out.contains("do NOT respond to it"));
    // The real task is present and clearly delimited AFTER the framing.
    let task_pos = out.find("=== YOUR TASK ===").expect("task delimiter");
    assert!(out[task_pos..].contains("Clone and review the repo."));
}

#[test]
fn role_task_warning_fires_for_readonly_role_with_clone_and_write_task() {
    // The mini4 reviewer case: read-only allow-list + "clone and write".
    let reviewer = [
        "read_file".to_string(),
        "group:search".to_string(),
        "web_fetch".to_string(),
    ];
    let note = role_task_capability_warning(
        &reviewer,
        "Clone the repo and write a review to octos-web-review.md",
    )
    .expect("mismatch must warn");
    assert!(
        note.contains("git clone"),
        "flags the missing shell: {note}"
    );
    assert!(
        note.contains("write_file"),
        "flags the missing writer: {note}"
    );
    assert!(note.contains("FINAL TEXT ANSWER"));
}

#[test]
fn role_task_warning_silent_when_tools_are_sufficient() {
    let equipped = [
        "shell".to_string(),
        "write_file".to_string(),
        "read_file".to_string(),
    ];
    assert!(
        role_task_capability_warning(
            &equipped,
            "Clone the repo and write a review to octos-web-review.md"
        )
        .is_none()
    );
}

#[test]
fn role_task_warning_silent_for_unconstrained_and_for_pure_read_task() {
    // Empty allow-list = all builtins → no restriction.
    assert!(role_task_capability_warning(&[], "clone and write a report").is_none());
    // Read-only role + a pure read/summarize task → no mismatch.
    let reviewer = ["read_file".to_string(), "group:search".to_string()];
    assert!(
        role_task_capability_warning(&reviewer, "Summarize the architecture of this diff")
            .is_none()
    );
}

#[test]
fn derive_deliverable_filename_matches_the_declared_glob() {
    // The single-* review glob → slug from label's first word.
    assert_eq!(
        derive_deliverable_filename("*-review.md", "octos-web review"),
        "octos-web-review.md"
    );
    assert_eq!(
        derive_deliverable_filename("*.md", "octos-one review"),
        "octos-one.md"
    );
    // Literal filename → verbatim.
    assert_eq!(
        derive_deliverable_filename("report.md", "anything"),
        "report.md"
    );
    // Odd/multi-* glob → sensible fallback that matches *-review.md / *.md.
    let fb = derive_deliverable_filename("**/*.md", "octos-web review");
    assert_eq!(fb, "octos-web-review.md");
    // Non-alnum label sanitized; empty → output.
    assert_eq!(
        derive_deliverable_filename("*-review.md", "  "),
        "output-review.md"
    );
}

#[tokio::test]
async fn background_deliverable_auto_materializes_inline_final_output() {
    // Live-soak fix: a child that declared a deliverable but returned its
    // work as FINAL TEXT (no file) must have that text written to the
    // deliverable path so it surfaces in output_files instead of being
    // lost. Uses a mock provider that ends with a long text answer and
    // never writes a file.
    struct InlineReviewProvider;
    #[async_trait]
    impl LlmProvider for InlineReviewProvider {
        async fn chat(
            &self,
            _m: &[octos_core::Message],
            _t: &[octos_llm::ToolSpec],
            _c: &octos_llm::ChatConfig,
        ) -> Result<octos_llm::ChatResponse> {
            Ok(octos_llm::ChatResponse {
                content: Some(format!(
                    "# Code Review\n\n{}",
                    "detailed finding. ".repeat(60)
                )),
                reasoning_content: None,
                tool_calls: vec![],
                stop_reason: octos_llm::StopReason::EndTurn,
                usage: octos_llm::TokenUsage::default(),
                provider_index: None,
            })
        }
        fn model_id(&self) -> &str {
            "mock"
        }
        fn provider_name(&self) -> &str {
            "mock"
        }
    }

    let (in_tx, _in_rx) = tokio::sync::mpsc::channel(16);
    let temp = tempfile::tempdir().unwrap();
    let ledger = temp.path().join("tasks.jsonl");
    let workspace = temp.path().join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let supervisor = Arc::new(TaskSupervisor::new());
    supervisor.enable_persistence(&ledger).unwrap();
    let tool = SpawnTool::new(
        Arc::new(InlineReviewProvider),
        Arc::new(create_test_store().await),
        workspace.clone(),
        in_tx,
    )
    .with_task_supervisor(supervisor.clone(), "api:test-session", ledger.clone())
    .with_sandbox(SandboxConfig {
        mode: crate::sandbox::SandboxMode::None,
        ..Default::default()
    });

    let result = tool
        .execute(&serde_json::json!({
            "task": "review the repo and write octos-web-review.md",
            "label": "octos-web review",
            "mode": "background",
            "allowed_tools": ["read_file"],
            "deliverable": "*-review.md"
        }))
        .await
        .unwrap();
    assert!(result.success, "{}", result.output);

    let started = std::time::Instant::now();
    let task = loop {
        let tasks = supervisor.get_tasks_for_session("api:test-session");
        if let Some(t) = tasks.first() {
            if t.status == crate::task_supervisor::TaskStatus::Completed {
                break t.clone();
            }
            if t.status == crate::task_supervisor::TaskStatus::Failed {
                panic!("spawn failed: {:?}", t.error);
            }
        }
        if started.elapsed() >= std::time::Duration::from_secs(15) {
            let tasks = supervisor.get_tasks_for_session("api:test-session");
            panic!(
                "did not complete in 15s; tasks = {:?}",
                tasks
                    .iter()
                    .map(|t| (t.status.as_str(), t.error.clone()))
                    .collect::<Vec<_>>()
            );
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    };
    assert_eq!(
        task.output_files.len(),
        1,
        "inline review must be auto-materialized into a deliverable file: {:?}",
        task.output_files
    );
    assert!(task.output_files[0].ends_with("octos-web-review.md"));
    let written = std::fs::read_to_string(&task.output_files[0]).unwrap();
    assert!(written.contains("# Code Review"));
}
/// Stub embedder — never invoked; these tests only assert the Arc
/// is threaded through (mirrors
/// `octos-pipeline/tests/embedder_propagation.rs`).
struct StubEmbedder;

#[async_trait]
impl octos_llm::EmbeddingProvider for StubEmbedder {
    async fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        Ok(vec![vec![0.0_f32; 1]; texts.len()])
    }

    fn dimension(&self) -> usize {
        1
    }
}

async fn embedder_probe_tool() -> SpawnTool {
    let dir = tempfile::tempdir().expect("tempdir");
    let memory = Arc::new(
        octos_memory::EpisodeStore::open(dir.path().join("mem"))
            .await
            .expect("episode store"),
    );
    let (tx, _rx) = tokio::sync::mpsc::channel(1);
    SpawnTool::new(
        Arc::new(MockProvider) as Arc<dyn LlmProvider>,
        memory,
        std::env::temp_dir(),
        tx,
    )
}

/// Workers save episodes by default (`AgentConfig::default().save_episodes`),
/// so a spawn tool without embedder propagation stores them vectorless
/// and worker recall silently skips. `with_embedder` must persist the
/// handle so worker construction can forward it.
#[tokio::test]
async fn should_store_embedder_when_builder_provides_one() {
    let embedder = Arc::new(StubEmbedder) as Arc<dyn octos_llm::EmbeddingProvider>;
    let tool = embedder_probe_tool().await.with_embedder(embedder);
    assert!(
        tool.embedder_for_test().is_some(),
        "SpawnTool::with_embedder must persist the handle so every \
             worker Agent inherits embed-on-save + hybrid recall"
    );
}

#[tokio::test]
async fn should_default_to_no_embedder_when_not_provided() {
    let tool = embedder_probe_tool().await;
    assert!(
        tool.embedder_for_test().is_none(),
        "legacy callers without an embedder stay byte-for-byte identical"
    );
}

#[test]
fn role_template_selection_applies_prompt_and_tool_budget() {
    let mut input: Input = serde_json::from_value(serde_json::json!({
        "task": "review this diff",
        "role": crate::ROLE_REVIEWER,
        "additional_instructions": "Focus on API behavior."
    }))
    .expect("input parses");

    let template = apply_role_template(&mut input)
        .expect("role template resolves")
        .expect("template");

    assert_eq!(template.name, crate::ROLE_REVIEWER);
    assert_eq!(
        input.allowed_tools,
        template.allowed_tools_vec(),
        "empty allowed_tools should resolve from the backend-owned role template"
    );
    let instructions = input
        .additional_instructions
        .as_deref()
        .expect("instructions");
    assert!(
        instructions.starts_with(template.prompt_prefix),
        "role prompt prefix must be prepended by the runtime factory"
    );
    assert!(instructions.contains("Focus on API behavior."));
}

#[test]
fn role_template_selection_rejects_unknown_role() {
    let mut input: Input = serde_json::from_value(serde_json::json!({
        "task": "do something",
        "role": "planner"
    }))
    .expect("input parses");

    let error = apply_role_template(&mut input).expect_err("unknown role is rejected");
    assert!(error.to_string().contains("unknown role template"));
}

#[cfg(unix)]
fn capture_hook(event: HookEvent, log_path: &std::path::Path) -> HookConfig {
    HookConfig {
        event,
        command: vec![
            "/bin/sh".into(),
            "-c".into(),
            r#"payload=$(cat); printf "%s\n" "$payload" >> "$1""#.into(),
            "sh".into(),
            log_path.to_string_lossy().into_owned(),
        ],
        timeout_ms: 5000,
        tool_filter: vec![],
        path_filter: vec![],
        requires_bin: None,
    }
}

#[cfg(unix)]
fn rewrite_output_files_hook(replacement_path: &std::path::Path) -> HookConfig {
    HookConfig {
        event: HookEvent::BeforeSpawnVerify,
        command: vec![
            "/bin/sh".into(),
            "-c".into(),
            r#"cat >/dev/null; printf '{"output_files":["%s"]}\n' "$1"; exit 2"#.into(),
            "sh".into(),
            replacement_path.to_string_lossy().into_owned(),
        ],
        timeout_ms: 5000,
        tool_filter: vec![],
        path_filter: vec![],
        requires_bin: None,
    }
}

#[tokio::test]
async fn test_spawn_returns_immediately() {
    let (in_tx, _in_rx) = tokio::sync::mpsc::channel(16);

    // We can't easily create a real LLM + EpisodeStore for unit tests,
    // so just test the worker count and basic input parsing.
    let tool = SpawnTool {
        llm: Arc::new(MockProvider),
        memory: Arc::new(create_test_store().await),
        working_dir: PathBuf::from("/tmp"),
        inbound_tx: in_tx,
        origin: std::sync::Mutex::new(("cli".into(), "test".into())),
        worker_count: AtomicU32::new(0),
        provider_policy: None,
        provider_router: None,
        worker_prompt: None,
        background_result_sender: None,
        child_session_sender: None,
        hooks: None,
        hook_context_template: None,
        plugin_dirs: Vec::new(),
        plugin_extra_env: Vec::new(),
        plugin_require_signed: false,
        child_tool_factories: Vec::new(),
        task_supervisor: None,
        session_key: None,
        task_ledger_path: None,
        worker_config: None,
        embedder: None,
        mcp_agent_backend: None,
        mcp_agent_tool_name: None,
        cost_accountant: None,
        parent_file_state_cache: None,
        parent_subagent_output_router: None,
        child_stream_callback: None,
        parent_subagent_summary_generator: None,
        child_prompt_context_manager_factory: None,
        dispatch_policy: None,
        // Explicit no-op sandbox keeps this unit test host-independent
        // (no dependency on whether a real backend helper is installed).
        sandbox: SandboxConfig {
            mode: crate::sandbox::SandboxMode::None,
            ..SandboxConfig::default()
        },
        deliverable_root: None,
        workspace_write_access: true,
    };

    assert_eq!(tool.worker_count.load(Ordering::SeqCst), 0);

    // Invalid input test
    let result = tool.execute(&serde_json::json!({})).await;
    assert!(result.is_err());

    // Worker count should not increment on invalid input
    assert_eq!(tool.worker_count.load(Ordering::SeqCst), 0);
}

/// #1607 (codex-review follow-up): the spawn/agent_mcp child completion
/// path builds its validator registries with
/// `ToolRegistry::with_builtins_and_sandbox(&self.working_dir,
/// create_sandbox(&self.sandbox))`. This test locks in that the sandbox
/// threaded via `with_sandbox` actually reaches that registry (i.e. the
/// two construction sites are NOT the pre-fix hardcoded `with_builtins` /
/// `NoSandbox`). Docker mode is chosen because `create_sandbox` returns a
/// `DockerSandbox` unconditionally (no docker binary required), so the
/// assertion is host-independent: a hardcoded `NoSandbox` would report
/// `is_noop() == true` / `is_docker() == false`, which would fail here.
#[tokio::test]
async fn spawn_threads_configured_sandbox_into_validator_registry() {
    let (in_tx, _in_rx) = tokio::sync::mpsc::channel(16);
    let tool = SpawnTool::new(
        Arc::new(MockProvider),
        Arc::new(create_test_store().await),
        PathBuf::from("/tmp"),
        in_tx,
    )
    .with_sandbox(SandboxConfig {
        mode: crate::sandbox::SandboxMode::Docker,
        ..SandboxConfig::default()
    });

    // Reconstruct exactly what the two `execute_with_context` validator
    // blocks build (`with_builtins_and_sandbox(&self.working_dir,
    // create_sandbox(&self.sandbox))`) and assert the backend is the one
    // we configured, not a hardcoded no-op.
    let registry =
        ToolRegistry::with_builtins_and_sandbox(&tool.working_dir, create_sandbox(&tool.sandbox));
    let sandbox = registry.sandbox();
    assert!(
        sandbox.is_docker(),
        "spawn validator registry must inherit the SpawnTool sandbox \
             (Docker here), not the pre-#1607 hardcoded NoSandbox"
    );
    assert!(
        !sandbox.is_noop(),
        "a real backend threaded via with_sandbox must not be a no-op"
    );
}

/// #1607 (codex-review follow-up): the default (unconfigured) SpawnTool
/// keeps a `SandboxConfig::default()` and stays host-independent — an
/// explicit `SandboxMode::None` resolves to `NoSandbox` (is_noop), so the
/// validator registry runs command validators directly (pre-#1607
/// behaviour) on hosts without a real backend.
#[tokio::test]
async fn spawn_none_sandbox_registry_is_noop() {
    let (in_tx, _in_rx) = tokio::sync::mpsc::channel(16);
    let tool = SpawnTool::new(
        Arc::new(MockProvider),
        Arc::new(create_test_store().await),
        PathBuf::from("/tmp"),
        in_tx,
    )
    .with_sandbox(SandboxConfig {
        mode: crate::sandbox::SandboxMode::None,
        ..SandboxConfig::default()
    });

    let registry =
        ToolRegistry::with_builtins_and_sandbox(&tool.working_dir, create_sandbox(&tool.sandbox));
    assert!(
        registry.sandbox().is_noop(),
        "SandboxMode::None must resolve to a no-op backend so command \
             validators run directly (host-independent)"
    );
}

#[tokio::test]
async fn test_background_spawn_tracks_supervisor_lifecycle() {
    let (in_tx, _in_rx) = tokio::sync::mpsc::channel(16);
    let supervisor = Arc::new(TaskSupervisor::new());
    let tool = SpawnTool::new(
        Arc::new(MockProvider),
        Arc::new(create_test_store().await),
        PathBuf::from("/tmp"),
        in_tx,
    )
    .with_task_supervisor(
        supervisor.clone(),
        "api:test-session",
        PathBuf::from("/tmp/tasks.jsonl"),
    );

    let result = tool
        .execute(&serde_json::json!({
            "task": "Write a short answer",
            "label": "Deep research",
            "mode": "background",
            "allowed_tools": []
        }))
        .await
        .unwrap();

    assert!(result.success);

    let started = std::time::Instant::now();
    loop {
        let tasks = supervisor.get_tasks_for_session("api:test-session");
        if let Some(task) = tasks.first() {
            if task.status == crate::task_supervisor::TaskStatus::Completed {
                assert_eq!(task.tool_name, "Deep research");
                break;
            }
        }
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "background spawn task did not complete in time"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

/// Cap refusal must refuse the SPAWN, not just the tracking. Regression:
/// `register_with_lineage` returns an empty-string sentinel when the
/// per-session child-fanout cap rejects the registration, and the
/// background branch spawned the detached worker anyway — untracked,
/// uncancellable, with the terminal guard armed under `""`.
#[tokio::test]
async fn test_background_spawn_refused_at_fanout_cap_does_not_spawn() {
    let (in_tx, _in_rx) = tokio::sync::mpsc::channel(16);
    let supervisor = Arc::new(TaskSupervisor::new());
    // Saturate the parent with ACTIVE (Spawned) children so the next
    // register is refused.
    for i in 0..crate::task_supervisor::MAX_CHILDREN_PER_PARENT {
        let id = supervisor.register("busy", &format!("call-{i}"), Some("api:test-session"));
        assert!(!id.is_empty(), "saturation register #{i} must succeed");
    }

    let tool = SpawnTool::new(
        Arc::new(MockProvider),
        Arc::new(create_test_store().await),
        PathBuf::from("/tmp"),
        in_tx,
    )
    .with_task_supervisor(
        supervisor.clone(),
        "api:test-session",
        PathBuf::from("/tmp/tasks.jsonl"),
    );

    let result = tool
        .execute(&serde_json::json!({
            "task": "Write a short answer",
            "label": "over-cap spawn",
            "mode": "background",
            "allowed_tools": []
        }))
        .await
        .unwrap();

    assert!(
        !result.success,
        "a cap-refused background spawn must fail, not report started; got: {}",
        result.output
    );
    assert!(
        result.output.contains("[TASK LIMIT]"),
        "the refusal must tell the LLM about the cap; got: {}",
        result.output
    );
    // No untracked worker: the supervisor still holds exactly the cap.
    assert_eq!(
        supervisor.get_tasks_for_session("api:test-session").len(),
        crate::task_supervisor::MAX_CHILDREN_PER_PARENT,
        "the refused spawn must not register or run new work"
    );
}

#[ignore = "Pre-migration test: the SpawnOnlyFiles-source MagicBytes validator \
                (post-#997 round-3) rejects no-files-emitted tasks at the project-scope \
                gate, so this test's `ShellThenEndProvider`-driven shell tool (which \
                doesn't emit `files_to_send`) can no longer simulate a successful \
                slides spawn — the deck-on-disk fallback the old Glob validator \
                provided is gone by design. Re-enable by replacing `ShellThenEndProvider` \
                with a stub plugin tool that returns the staged deck path in \
                `tool_result.files_to_send`."]
#[tokio::test]
async fn test_background_spawn_uses_contract_selected_slides_artifact_for_persistence() {
    let (in_tx, _in_rx) = tokio::sync::mpsc::channel(16);
    let temp = tempfile::tempdir().unwrap();
    let repo_root = temp.path().join("slides/demo");
    let ledger = temp.path().join("tasks.jsonl");
    std::fs::create_dir_all(repo_root.join("output")).unwrap();
    crate::write_workspace_policy(
        &repo_root,
        &crate::WorkspacePolicy::for_kind(crate::WorkspaceProjectKind::Slides),
    )
    .unwrap();
    std::fs::write(repo_root.join("script.js"), "// slides").unwrap();
    std::fs::write(repo_root.join("memory.md"), "# memory").unwrap();
    std::fs::write(repo_root.join("changelog.md"), "# changelog").unwrap();
    // octos #997 (round-2): real PPTX magic bytes ONLY. The spawn loop
    // itself runs the slides-kind project-scope validator at the project
    // root after `run_task` succeeds — that production wiring writes the
    // Pass row into `slides/demo/.octos/validator_outcomes.jsonl`, which
    // the contract-gated terminal delivery step then reads. Pre-round-2
    // this fixture manually seeded the Pass via `ledger.append(...)`,
    // masking the gap codex flagged. No manual seeding here.
    let mut pptx = vec![0x50, 0x4B, 0x03, 0x04];
    pptx.extend_from_slice(b"final");
    std::fs::write(repo_root.join("output/deck.pptx"), pptx).unwrap();
    std::fs::write(repo_root.join("output/slide-01.png"), "png").unwrap();

    let supervisor = Arc::new(TaskSupervisor::new());
    supervisor.enable_persistence(&ledger).unwrap();
    let tool = SpawnTool::new(
        Arc::new(ShellThenEndProvider {
            calls: std::sync::atomic::AtomicUsize::new(0),
        }),
        Arc::new(create_test_store().await),
        temp.path().to_path_buf(),
        in_tx,
    )
    .with_task_supervisor(supervisor.clone(), "api:test-session", ledger.clone());

    let result = tool
        .execute(&serde_json::json!({
            "task": "Acknowledge the request and stop.",
            "label": "Slides deliverable",
            "mode": "background",
            "allowed_tools": ["shell"],
            "workflow": {
                "workflow_kind": "slides",
                "current_phase": "design",
                "allowed_tools": ["shell"],
                "terminal_output": {
                    "deliver_final_artifact_only": true,
                    "forbid_intermediate_files": true,
                    "required_artifact_kind": "presentation"
                }
            }
        }))
        .await
        .unwrap();

    assert!(result.success);

    let started = std::time::Instant::now();
    loop {
        let tasks = supervisor.get_tasks_for_session("api:test-session");
        if let Some(task) = tasks.first() {
            if task.status == crate::task_supervisor::TaskStatus::Completed {
                assert_eq!(
                    task.output_files,
                    vec![
                        repo_root
                            .join("output/deck.pptx")
                            .to_string_lossy()
                            .to_string()
                    ]
                );
                break;
            }
        }
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "background spawn task did not complete in time"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    let restored = TaskSupervisor::new();
    restored.enable_persistence(&ledger).unwrap();
    let tasks = restored.get_tasks_for_session("api:test-session");
    assert_eq!(tasks.len(), 1);
    assert_eq!(
        tasks[0].status,
        crate::task_supervisor::TaskStatus::Completed
    );
    assert_eq!(
        tasks[0].output_files,
        vec![
            repo_root
                .join("output/deck.pptx")
                .to_string_lossy()
                .to_string()
        ]
    );
}

#[tokio::test]
#[cfg(unix)]
async fn test_before_spawn_verify_hook_can_replace_output_files() {
    let temp = tempfile::tempdir().unwrap();
    let replacement = temp.path().join("final-reviewed.pptx");
    std::fs::write(&replacement, "reviewed").unwrap();

    let hooks = Arc::new(HookExecutor::new(vec![rewrite_output_files_hook(
        &replacement,
    )]));
    let payload = HookPayload::before_spawn_verify(
        "task-1",
        "Slides deliverable",
        "api:test-session",
        "api:test-session:child",
        Some("slides"),
        Some("verify_outputs"),
        Some("candidate terminal outputs resolved"),
        vec!["/tmp/original-deck.pptx".to_string()],
        Some(&HookContext {
            session_id: Some("api:test-session".to_string()),
            profile_id: Some("test-profile".to_string()),
        }),
    );

    let modified_files = run_before_spawn_verify_hook(Some(&hooks), payload)
        .await
        .unwrap();

    assert_eq!(modified_files, vec![replacement]);
}

#[tokio::test]
async fn test_background_spawn_fails_when_contract_owned_workflow_is_not_ready() {
    let (in_tx, _in_rx) = tokio::sync::mpsc::channel(16);
    let temp = tempfile::tempdir().unwrap();
    let repo_root = temp.path().join("slides/demo");
    let ledger = temp.path().join("tasks.jsonl");
    std::fs::create_dir_all(&repo_root).unwrap();
    crate::write_workspace_policy(
        &repo_root,
        &crate::WorkspacePolicy::for_kind(crate::WorkspaceProjectKind::Slides),
    )
    .unwrap();
    std::fs::write(repo_root.join("script.js"), "// slides").unwrap();
    std::fs::write(repo_root.join("memory.md"), "# memory").unwrap();
    std::fs::write(repo_root.join("changelog.md"), "# changelog").unwrap();

    let supervisor = Arc::new(TaskSupervisor::new());
    supervisor.enable_persistence(&ledger).unwrap();
    let tool = SpawnTool::new(
        Arc::new(ShellThenEndProvider {
            calls: std::sync::atomic::AtomicUsize::new(0),
        }),
        Arc::new(create_test_store().await),
        temp.path().to_path_buf(),
        in_tx,
    )
    .with_task_supervisor(supervisor.clone(), "api:test-session", ledger);

    let result = tool
        .execute(&serde_json::json!({
            "task": "Acknowledge the request and stop.",
            "label": "Slides deliverable",
            "mode": "background",
            "allowed_tools": ["shell"],
            "workflow": {
                "workflow_kind": "slides",
                "current_phase": "design",
                "allowed_tools": ["shell"],
                "terminal_output": {
                    "deliver_final_artifact_only": true,
                    "forbid_intermediate_files": true,
                    "required_artifact_kind": "presentation"
                }
            }
        }))
        .await
        .unwrap();

    assert!(result.success);

    let started = std::time::Instant::now();
    loop {
        let tasks = supervisor.get_tasks_for_session("api:test-session");
        if let Some(task) = tasks.first() {
            if task.status == crate::task_supervisor::TaskStatus::Failed {
                let error = task.error.as_deref().unwrap_or_default();
                assert!(error.contains("workspace contract"), "{error}");
                return;
            }
        }
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "background spawn task did not fail in time"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

#[tokio::test]
#[cfg(unix)]
async fn test_background_spawn_emits_failure_hook_for_contract_failure() {
    let (in_tx, _in_rx) = tokio::sync::mpsc::channel(16);
    let temp = tempfile::tempdir().unwrap();
    let repo_root = temp.path().join("slides/demo");
    let ledger = temp.path().join("tasks.jsonl");
    let hook_log = temp.path().join("spawn-failure-hooks.jsonl");
    std::fs::create_dir_all(&repo_root).unwrap();
    crate::write_workspace_policy(
        &repo_root,
        &crate::WorkspacePolicy::for_kind(crate::WorkspaceProjectKind::Slides),
    )
    .unwrap();
    std::fs::write(repo_root.join("script.js"), "// slides").unwrap();
    std::fs::write(repo_root.join("memory.md"), "# memory").unwrap();
    std::fs::write(repo_root.join("changelog.md"), "# changelog").unwrap();

    let supervisor = Arc::new(TaskSupervisor::new());
    supervisor.enable_persistence(&ledger).unwrap();
    let hooks = Arc::new(HookExecutor::new(vec![capture_hook(
        HookEvent::OnSpawnFailure,
        &hook_log,
    )]));
    let tool = SpawnTool::new(
        Arc::new(MockProvider),
        Arc::new(create_test_store().await),
        temp.path().to_path_buf(),
        in_tx,
    )
    .with_task_supervisor(supervisor.clone(), "api:test-session", ledger)
    .with_hooks(hooks)
    .with_hook_context(HookContext {
        session_id: Some("api:test-session".to_string()),
        profile_id: Some("test-profile".to_string()),
    });

    let result = tool
        .execute(&serde_json::json!({
            "task": "Build the deck",
            "label": "Slides deliverable",
            "mode": "background",
            "allowed_tools": ["mofa_slides"],
            "workflow": {
                "workflow_kind": "slides",
                "current_phase": "design",
                "allowed_tools": ["mofa_slides"],
                "terminal_output": {
                    "deliver_final_artifact_only": true,
                    "forbid_intermediate_files": true,
                    "required_artifact_kind": "presentation"
                }
            }
        }))
        .await
        .unwrap();

    assert!(result.success);

    let started = std::time::Instant::now();
    loop {
        let tasks = supervisor.get_tasks_for_session("api:test-session");
        let hook_lines = std::fs::read_to_string(&hook_log).unwrap_or_default();
        if let Some(task) = tasks.first() {
            if task.status == crate::task_supervisor::TaskStatus::Failed
                && hook_lines.contains("\"event\":\"on_spawn_failure\"")
            {
                assert!(hook_lines.contains("\"failure_action\":\"escalate\""));
                assert!(hook_lines.contains("\"workflow_kind\":\"slides\""));
                assert!(hook_lines.contains("\"result\":\""));
                return;
            }
        }
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "background spawn failure hook did not arrive in time"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

#[test]
fn workflow_terminal_output_prefers_final_audio_and_skips_intermediates() {
    let workflow = WorkflowMetadata {
        workflow_kind: "research_podcast".to_string(),
        current_phase: "generate_audio".to_string(),
        allowed_tools: vec!["podcast_generate".to_string()],
        terminal_output: Some(WorkflowTerminalOutputPolicy {
            deliver_final_artifact_only: true,
            forbid_intermediate_files: true,
            required_artifact_kind: "audio".to_string(),
        }),
        progress: None,
    };

    let files_to_send = vec![
        PathBuf::from("/tmp/podcast_part_1.mp3"),
        PathBuf::from("/tmp/research_report.md"),
        PathBuf::from("/tmp/podcast_full_final.mp3"),
    ];
    let files_modified = vec![PathBuf::from("/tmp/script.md")];

    let selected =
        select_workflow_terminal_files(&files_to_send, &files_modified, Some(&workflow)).unwrap();

    assert_eq!(selected, vec![PathBuf::from("/tmp/podcast_full_final.mp3")]);
}

#[test]
fn workflow_terminal_output_accepts_audio_from_modified_files_when_explicit_send_missing() {
    let workflow = WorkflowMetadata {
        workflow_kind: "research_podcast".to_string(),
        current_phase: "generate_audio".to_string(),
        allowed_tools: vec!["podcast_generate".to_string()],
        terminal_output: Some(WorkflowTerminalOutputPolicy {
            deliver_final_artifact_only: true,
            forbid_intermediate_files: true,
            required_artifact_kind: "audio".to_string(),
        }),
        progress: None,
    };

    let files_modified = vec![
        PathBuf::from("/tmp/podcast_script.md"),
        PathBuf::from("/tmp/podcast_full_final.mp3"),
    ];

    let selected = select_workflow_terminal_files(&[], &files_modified, Some(&workflow)).unwrap();

    assert_eq!(selected, vec![PathBuf::from("/tmp/podcast_full_final.mp3")]);
}

#[test]
fn workflow_terminal_output_requires_required_audio_artifact() {
    let temp = tempfile::tempdir().unwrap();
    let workflow = WorkflowMetadata {
        workflow_kind: "research_podcast".to_string(),
        current_phase: "deliver_result".to_string(),
        allowed_tools: vec!["podcast_generate".to_string()],
        terminal_output: Some(WorkflowTerminalOutputPolicy {
            deliver_final_artifact_only: true,
            forbid_intermediate_files: true,
            required_artifact_kind: "audio".to_string(),
        }),
        progress: None,
    };

    let error = resolve_background_terminal_files(temp.path(), &[], &[], Some(&workflow))
        .expect_err("research_podcast must not complete without audio");

    assert!(error.contains("required audio terminal artifact"));
}

#[test]
fn deliverable_contract_surfaces_a_shell_written_file_no_tool_reported_it() {
    // Reproduces the mini4 "zero deliverable" bug: a worker with no
    // write_file in its toolset wrote its review via a shell heredoc, so
    // the tool-record path saw nothing. The seeded workspace contract must
    // surface the file regardless of how it was written.
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    seed_deliverable_contract(root, "*.md").expect("seed deliverable contract");

    // Simulate `cat > octos-review.md <<EOF ...` — no write_file ran, so
    // files_modified / files_to_send are both empty.
    std::fs::write(root.join("octos-review.md"), "# Review\n").unwrap();

    // Control: the pre-existing tool-record path is blind to it (the bug).
    let via_tool_record = resolve_background_terminal_files(root, &[], &[], None).unwrap();
    assert!(
        via_tool_record.is_empty(),
        "tool-record path should see nothing: {via_tool_record:?}"
    );

    // Fix: the workspace contract surfaces the shell-written deliverable.
    let via_contract = resolve_deliverable_terminal_files(root);
    assert_eq!(via_contract.len(), 1, "contract surfaced: {via_contract:?}");
    assert!(via_contract[0].ends_with("octos-review.md"));
}

#[test]
fn deliverable_contract_only_surfaces_the_declared_glob() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    seed_deliverable_contract(root, "*.md").unwrap();
    std::fs::write(root.join("keep.md"), "deliverable").unwrap();
    std::fs::write(root.join("scratch.log"), "noise").unwrap();
    // A nested clone dir must not be swept in by the top-level `*.md` glob.
    std::fs::create_dir_all(root.join("cloned-repo")).unwrap();
    std::fs::write(root.join("cloned-repo").join("README.md"), "upstream").unwrap();

    let files = resolve_deliverable_terminal_files(root);
    assert_eq!(
        files.len(),
        1,
        "only the top-level .md deliverable: {files:?}"
    );
    assert!(files[0].ends_with("keep.md"));
}

#[test]
fn deliverable_artifact_glob_normalizes_empty_and_absent() {
    assert_eq!(deliverable_artifact_glob(None), None);
    assert_eq!(
        deliverable_artifact_glob(Some("")).as_deref(),
        Some(DEFAULT_DELIVERABLE_GLOB)
    );
    assert_eq!(
        deliverable_artifact_glob(Some("  ")).as_deref(),
        Some(DEFAULT_DELIVERABLE_GLOB)
    );
    assert_eq!(
        deliverable_artifact_glob(Some("*-review.md")).as_deref(),
        Some("*-review.md")
    );
}

#[test]
fn workflow_terminal_output_prefers_final_presentation_and_skips_scratch_files() {
    let workflow = WorkflowMetadata {
        workflow_kind: "slides".to_string(),
        current_phase: "deliver_result".to_string(),
        allowed_tools: vec!["mofa_slides".to_string()],
        terminal_output: Some(WorkflowTerminalOutputPolicy {
            deliver_final_artifact_only: true,
            forbid_intermediate_files: true,
            required_artifact_kind: "presentation".to_string(),
        }),
        progress: None,
    };

    let files_to_send = vec![
        PathBuf::from("/tmp/output/slide-01.png"),
        PathBuf::from("/tmp/output/deck.pptx"),
        PathBuf::from("/tmp/output/notes.txt"),
    ];

    let selected = select_workflow_terminal_files(&files_to_send, &[], Some(&workflow)).unwrap();

    assert_eq!(selected, vec![PathBuf::from("/tmp/output/deck.pptx")]);
}

#[test]
fn workflow_terminal_output_prefers_site_entrypoint_and_skips_assets() {
    let workflow = WorkflowMetadata {
        workflow_kind: "site".to_string(),
        current_phase: "deliver_result".to_string(),
        allowed_tools: vec!["shell".to_string()],
        terminal_output: Some(WorkflowTerminalOutputPolicy {
            deliver_final_artifact_only: true,
            forbid_intermediate_files: true,
            required_artifact_kind: "site".to_string(),
        }),
        progress: None,
    };

    let files_to_send = vec![
        PathBuf::from("/tmp/site/dist/assets/logo.png"),
        PathBuf::from("/tmp/site/dist/index.html"),
        PathBuf::from("/tmp/site/dist/about.html"),
    ];

    let selected = select_workflow_terminal_files(&files_to_send, &[], Some(&workflow)).unwrap();

    assert_eq!(selected, vec![PathBuf::from("/tmp/site/dist/index.html")]);
}

#[test]
fn contract_owned_workflow_denies_send_file_in_subagent_policy() {
    let workflow = WorkflowMetadata {
        workflow_kind: "slides".to_string(),
        current_phase: "deliver_result".to_string(),
        allowed_tools: vec!["mofa_slides".to_string(), "send_file".to_string()],
        terminal_output: Some(WorkflowTerminalOutputPolicy {
            deliver_final_artifact_only: true,
            forbid_intermediate_files: true,
            required_artifact_kind: "presentation".to_string(),
        }),
        progress: None,
    };

    // Workflow-node spawn: no agent-definition manifest, so no
    // manifest-level disallowed_tools deny-list.
    let policy =
        build_subagent_tool_policy(workflow.allowed_tools.clone(), Vec::new(), Some(&workflow));

    assert!(policy.deny.contains(&"spawn".to_string()));
    assert!(policy.deny.contains(&"send_file".to_string()));
}

#[test]
fn workflow_phase_progress_is_coarse_but_non_null_and_monotonic() {
    // Initial phases of every workflow family — research_runtime path
    // names them differently per family; the helper must seed a small
    // non-null fraction for each so `runtime_detail.progress` is never
    // null on the first phase transition.
    for initial_phase in &["research", "design", "scaffold", "outline"] {
        let value = workflow_phase_progress(initial_phase);
        assert!(
            value > 0.0 && value <= 0.5,
            "initial phase {initial_phase} should map to a small non-null fraction, got {value}"
        );
    }

    // The terminal-ish phases must produce values strictly greater
    // than initial-phase values so the dashboard sees forward motion.
    let initial = workflow_phase_progress("research");
    let verifying = workflow_phase_progress("verify_outputs");
    let deliver = workflow_phase_progress("deliver_result");
    assert!(
        verifying > initial,
        "verify_outputs ({verifying}) must exceed initial ({initial})"
    );
    assert!(
        deliver > verifying,
        "deliver_result ({deliver}) must exceed verify_outputs ({verifying})"
    );
    assert!(
        deliver < 1.0,
        "deliver_result ({deliver}) must stay strictly under 1.0 — terminal completion is signalled by lifecycle state, not by a synthesized progress sentinel"
    );
}

#[test]
fn subagent_tool_preflight_passes_when_allowed_tool_present() {
    let tools = ToolRegistry::with_builtins("/tmp");
    // RFC-0 (#1289): `shell` is always registered and visible — no
    // deferral to un-hide.
    assert!(tools.specs().iter().any(|spec| spec.name == "shell"));
    ensure_subagent_tools_available(&tools, &[String::from("shell")], true).unwrap();
}

#[test]
fn subagent_tool_preflight_reports_missing_allowed_tool() {
    let tools = ToolRegistry::with_builtins("/tmp");

    let error = ensure_subagent_tools_available(&tools, &[String::from("podcast_generate")], true)
        .unwrap_err();

    assert!(error.contains("required tool(s) not available on this host"));
    assert!(error.contains("podcast_generate"));
}

#[test]
fn preflight_skips_group_and_wildcard_tokens() {
    // #1689 (retained sub-fix): `group:*` / `*` are policy EXPRESSIONS,
    // not concrete tool names — `tools.get("group:fs")` is always None —
    // so they must be skipped, not reported "not available on this host".
    // Otherwise any role / caller using group tokens could never spawn.
    let tools = ToolRegistry::with_builtins("/tmp");
    ensure_subagent_tools_available(
        &tools,
        &["group:fs".into(), "group:web".into(), "exec*".into()],
        true,
    )
    .expect("group / wildcard tokens must not be treated as missing tools");

    // A concrete missing tool alongside a group token is still reported —
    // but only the concrete name.
    let err = ensure_subagent_tools_available(
        &tools,
        &["group:fs".into(), "definitely_not_a_tool".into()],
        true,
    )
    .unwrap_err();
    assert!(err.contains("definitely_not_a_tool"));
    assert!(!err.contains("group:fs"));
}

#[test]
fn preflight_soft_drops_suggested_missing_tools_but_hard_fails_caller_named() {
    // Bug A (reviewer-role preflight): when the allow-list came from a role
    // template / manifest (strict = false), a tool that isn't wired here —
    // like the reviewer role's runtime-gated recall_memory /
    // synthesize_research — must be DROPPED with a warning, not fail the
    // spawn. The SAME missing tool is a hard error when the caller named it.
    let tools = ToolRegistry::with_builtins("/tmp");
    let list = [
        String::from("read_file"),
        String::from("definitely_not_a_tool"),
    ];

    ensure_subagent_tools_available(&tools, &list, false)
        .expect("role/manifest-suggested missing tools must be dropped, not fail the spawn");

    let err = ensure_subagent_tools_available(&tools, &list, true).unwrap_err();
    assert!(err.contains("definitely_not_a_tool"));
}

#[tokio::test]
async fn reviewer_role_spawn_does_not_fail_on_unwired_role_tools() {
    // Bug A end-to-end (the exact mini4 reviewer failure): a
    // `role: "reviewer"` spawn with no inline allowed_tools has the role
    // template fill the allow-list with `recall_memory` / `synthesize_research`
    // — runtime-gated tools that are NOT registered without a memory /
    // research provider. Before the fix the availability preflight
    // hard-failed with "not available on this host: ... recall_memory,
    // synthesize_research". Now those role-SUGGESTED tools are dropped and
    // the spawn runs.
    let (in_tx, _in_rx) = tokio::sync::mpsc::channel(16);
    let tool = SpawnTool::new(
        Arc::new(MockProvider),
        Arc::new(create_test_store().await),
        PathBuf::from("/tmp"),
        in_tx,
    );

    let result = tool
        .execute(&serde_json::json!({
            "task": "review the diff",
            "label": "reviewer",
            "mode": "sync",
            "role": "reviewer"
            // no allowed_tools — the role template fills it with tools that
            // include unwired recall_memory / synthesize_research
        }))
        .await
        .unwrap();

    assert!(
        !result.output.contains("not available on this host"),
        "the preflight must not hard-fail on role-suggested unwired tools: {}",
        result.output
    );
    assert!(
        result.success,
        "reviewer-role spawn must run despite unwired role tools: {}",
        result.output
    );
}

struct StaticTestTool {
    name: &'static str,
}

#[async_trait]
impl Tool for StaticTestTool {
    fn name(&self) -> &str {
        self.name
    }

    fn description(&self) -> &str {
        "test child tool"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object", "properties": {} })
    }

    async fn execute(&self, _args: &serde_json::Value) -> Result<ToolResult> {
        Ok(ToolResult {
            output: "ok".to_string(),
            success: true,
            ..Default::default()
        })
    }
}

fn write_mock_podcast_plugin(root: &std::path::Path, script_seen: &std::path::Path) -> PathBuf {
    let plugin_root = root.join("plugins");
    let plugin_dir = plugin_root.join("mofa-podcast");
    std::fs::create_dir_all(&plugin_dir).unwrap();
    std::fs::write(
        plugin_dir.join("manifest.json"),
        r#"{
  "name": "mofa-podcast",
  "version": "0.0.0-test",
  "tools": [
    {
      "name": "podcast_generate",
      "spawn_only": true,
      "description": "mock podcast generator",
      "input_schema": {
        "type": "object",
        "properties": {
          "script": { "type": "string" }
        }
      }
    }
  ]
}"#,
    )
    .unwrap();
    let main = plugin_dir.join("main");
    std::fs::write(
            &main,
            format!(
                r#"#!/usr/bin/env bash
set -euo pipefail
INPUT="$(cat)"
SCRIPT_SEEN="{script_seen}"
OCTOS_PLUGIN_INPUT="$INPUT" SCRIPT_SEEN="$SCRIPT_SEEN" python3 - <<'PY'
import json
import os

payload = json.loads(os.environ.get("OCTOS_PLUGIN_INPUT") or "{{}}")
with open(os.environ["SCRIPT_SEEN"], "w", encoding="utf-8") as handle:
    handle.write(str(payload.get("script") or ""))

base = os.environ.get("OCTOS_WORK_DIR") or os.getcwd()
out_dir = os.path.join(base, "skill-output", "mofa-podcast")
os.makedirs(out_dir, exist_ok=True)
out = os.path.join(out_dir, "podcast_full_test.mp3")
with open(out, "wb") as handle:
    handle.write(b"0" * 8192)

print(json.dumps({{"output": f"Podcast generated successfully: {{out}}", "success": True, "files_to_send": [out]}}))
PY
"#,
                script_seen = script_seen.display()
            ),
        )
        .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&main, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    plugin_root
}

#[tokio::test]
async fn test_sync_spawn_registers_child_tool_factory_before_preflight() {
    let (in_tx, _in_rx) = tokio::sync::mpsc::channel(16);
    let tool = SpawnTool::new(
        Arc::new(MockProvider),
        Arc::new(create_test_store().await),
        PathBuf::from("/tmp"),
        in_tx,
    )
    .with_child_tool_factory(Arc::new(|| {
        Arc::new(StaticTestTool {
            name: "run_pipeline",
        })
    }));

    let result = tool
        .execute(&serde_json::json!({
            "task": "Use the injected pipeline tool if needed",
            "label": "Deep research",
            "mode": "sync",
            "allowed_tools": ["run_pipeline"]
        }))
        .await
        .unwrap();

    assert!(result.success);
    assert_eq!(result.output, "done");
}

#[tokio::test]
async fn should_bind_native_spawn_in_child_registry_for_nested_delegation() {
    // Regression: a spawned subagent's registry must carry the native
    // `spawn` tool so the `ToolRegistry` swap binds `spawn_agent` /
    // `delegate` behind it. Without it a child that nested a spawn hit
    // "No native Octos spawn tool is bound behind spawn_agent in this
    // ToolRegistry.", and the second-round agents were orphaned with
    // empty output_files (the exact failure a live review session hit).
    //
    // Probe through the real `execute` path: a child declaring
    // `allowed_tools: ["spawn"]` must clear the
    // `ensure_subagent_tools_available` preflight, which only passes
    // when `spawn` is actually present in the child registry. (The
    // subagent policy then denies DIRECT `spawn` use — deny is
    // exact-match, so `spawn_agent` / `delegate` stay allowed and keep
    // the freshly-bound delegate.)
    let (in_tx, _in_rx) = tokio::sync::mpsc::channel(16);
    let tool = SpawnTool::new(
        Arc::new(MockProvider),
        Arc::new(create_test_store().await),
        PathBuf::from("/tmp"),
        in_tx,
    );

    let result = tool
        .execute(&serde_json::json!({
            "task": "review the change",
            "label": "reviewer",
            "mode": "sync",
            "allowed_tools": ["spawn"]
        }))
        .await
        .expect(
            "child spawn preflight must pass once the native spawn delegate is bound in the \
                 child registry",
        );

    assert!(
        result.success,
        "sync child must complete, got: {}",
        result.output
    );
    assert_eq!(result.output, "done");
}

#[tokio::test]
async fn child_spawn_clone_is_named_spawn_and_binds_spawn_agent_via_registry_swap() {
    // The clone the child-registry sites register must be named "spawn"
    // (so `ToolRegistry::register` swaps the delegate-less builtin
    // `spawn_agent` for a delegate-bound one) and must be rebased onto
    // the child's working directory rather than the parent's.
    let (in_tx, _in_rx) = tokio::sync::mpsc::channel(16);
    let parent = SpawnTool::new(
        Arc::new(MockProvider),
        Arc::new(create_test_store().await),
        PathBuf::from("/tmp"),
        in_tx,
    )
    .with_deliverable_root(PathBuf::from("/runtime/deliverables"))
    .with_workspace_write_access(false);

    let child_id = AgentId::new("child-0");
    let child_spawn = parent.child_spawn_clone(PathBuf::from("/work/child"), &child_id);
    assert_eq!(child_spawn.name(), "spawn");
    assert_eq!(child_spawn.working_dir, PathBuf::from("/work/child"));
    assert_eq!(
        child_spawn.deliverable_root,
        Some(
            PathBuf::from("/runtime/deliverables")
                .join("child-0")
                .join("children")
        )
    );
    assert!(!child_spawn.workspace_write_access);

    // A bare builtins registry (what every child registry starts from)
    // carries only the delegate-LESS builtin spawn_agent — the reason a
    // nested spawn failed with "No native Octos spawn tool is bound".
    let mut registry = ToolRegistry::with_builtins("/tmp");
    assert!(
        registry.get("spawn").is_none(),
        "builtins must not carry a native spawn delegate on their own"
    );

    // Registering the clone triggers the swap: spawn + a delegate-bound
    // spawn_agent + delegate are all present for the child.
    registry.register(child_spawn);
    assert!(registry.get("spawn").is_some());
    assert!(registry.get("spawn_agent").is_some());
    assert!(registry.get("delegate").is_some());
}

#[test]
fn default_subagent_policy_allows_spawn_agent_and_delegate_but_denies_direct_spawn() {
    // The usability half of the fix: a DEFAULT child (empty allow-list =
    // allow-all-except-deny) keeps the sanctioned nesting aliases while the
    // low-level `spawn` stays denied. This is the policy the child registry
    // runs under after `apply_policy`, so a child with no explicit tool
    // restriction can actually call the freshly-bound spawn_agent/delegate.
    // (A child with an explicit allow-list that omits them is denied — that
    // is the intended per-contract gate, not a regression.)
    let policy = build_subagent_tool_policy(Vec::new(), Vec::new(), None);
    assert!(
        !policy.is_allowed("spawn"),
        "direct low-level spawn stays denied for subagents"
    );
    assert!(
        policy.is_allowed("spawn_agent"),
        "spawn_agent (the sanctioned nesting alias) is allowed for a default child"
    );
    assert!(
        policy.is_allowed("delegate"),
        "delegate is allowed for a default child"
    );
}

#[tokio::test]
async fn nested_spawn_agent_resolves_a_real_agent_id_through_the_bound_delegate() {
    // Build a child-style registry exactly as the spawn sites do: a fresh
    // builtins registry (which carries only the delegate-LESS builtin
    // spawn_agent) plus the aligned native-spawn clone. The clone is bound
    // to THIS registry's own supervisor — the same one the child worker's
    // `ctx.task_supervisor` exposes (align-down / per-subtree isolation).
    //
    // Then drive `spawn_agent` the way a nesting child would: with
    // `ctx.task_supervisor` == that registry supervisor. It must (a) reach
    // the bound delegate instead of failing "No native Octos spawn tool is
    // bound", and (b) resolve a concrete `agent_id` — proving the
    // before/after task lookup and the delegate register into the SAME
    // supervisor.
    let (in_tx, _in_rx) = tokio::sync::mpsc::channel(16);
    let parent = SpawnTool::new(
        Arc::new(MockProvider),
        Arc::new(create_test_store().await),
        std::env::temp_dir(),
        in_tx,
    );

    let mut tools = ToolRegistry::with_builtins(std::env::temp_dir());
    let child_id = AgentId::new("child-0");
    let mut clone = parent.child_spawn_clone(std::env::temp_dir(), &child_id);
    clone.task_supervisor = Some(tools.supervisor());
    tools.register(clone);

    let mut ctx = crate::tools::ToolContext::zero();
    ctx.task_supervisor = Some(tools.supervisor());
    let spawn_agent = tools.get("spawn_agent").expect("spawn_agent is bound");
    let result = spawn_agent
        .execute_with_context(
            &ctx,
            &serde_json::json!({
                "task": "grandchild review",
                "label": "grandchild",
                "mode": "background"
            }),
        )
        .await
        .expect("spawn_agent executes");

    assert!(
        !result
            .output
            .contains("No native Octos spawn tool is bound"),
        "the delegate must be bound behind spawn_agent: {}",
        result.output
    );
    assert!(
        result.success,
        "nested spawn_agent must succeed via the bound delegate: {}",
        result.output
    );
    assert!(
        result.output.contains("agent_id"),
        "spawn_agent must resolve a concrete agent_id from the aligned \
             supervisor; got: {}",
        result.output
    );
}

#[test]
fn contract_terminal_output_prefers_declared_slides_deck_name_over_newer_draft() {
    let temp = tempfile::tempdir().unwrap();
    let repo_root = temp.path().join("slides/demo");
    std::fs::create_dir_all(repo_root.join("output")).unwrap();
    crate::write_workspace_policy(
        &repo_root,
        &crate::WorkspacePolicy::for_kind(crate::WorkspaceProjectKind::Slides),
    )
    .unwrap();
    std::fs::write(repo_root.join("script.js"), "// slides").unwrap();
    std::fs::write(repo_root.join("memory.md"), "# memory").unwrap();
    std::fs::write(repo_root.join("changelog.md"), "# changelog").unwrap();
    // octos #997: real PPTX magic bytes so the slides-kind project-scope
    // `MagicBytes` validator does not block delivery on a fake-bytes deck.
    let mut pptx_final = vec![0x50, 0x4B, 0x03, 0x04];
    pptx_final.extend_from_slice(b"final");
    std::fs::write(repo_root.join("output/deck.pptx"), pptx_final).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));
    let mut pptx_draft = vec![0x50, 0x4B, 0x03, 0x04];
    pptx_draft.extend_from_slice(b"draft");
    std::fs::write(repo_root.join("output/deck-draft.pptx"), pptx_draft).unwrap();
    std::fs::write(repo_root.join("output/slide-01.png"), "png").unwrap();
    // octos #997 (round-2): exercise the production project-root
    // validator helper so `inspect_workspace_contract_at_root` sees a
    // real `Pass` row in the project ledger. Pre-round-2 this fixture
    // manually `ledger.append(...)`ed a Pass — codex flagged that as
    // masking the gap (the validator was declared but never RUN at the
    // project root in production).
    {
        let registry = std::sync::Arc::new(crate::ToolRegistry::new());
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build tokio runtime for fixture validator run");
        let files_to_send = vec![repo_root.join("output/deck.pptx")];
        runtime.block_on(async {
            let _ = crate::workspace_contract::run_project_root_validators(
                &registry,
                temp.path(),
                Some(crate::WorkspaceProjectKind::Slides),
                &files_to_send,
                std::sync::Arc::new(crate::sandbox::NoSandbox),
            )
            .await;
        });
    }

    let workflow = WorkflowMetadata {
        workflow_kind: "slides".to_string(),
        current_phase: "deliver_result".to_string(),
        allowed_tools: vec!["mofa_slides".to_string()],
        terminal_output: Some(WorkflowTerminalOutputPolicy {
            deliver_final_artifact_only: true,
            forbid_intermediate_files: true,
            required_artifact_kind: "presentation".to_string(),
        }),
        progress: None,
    };

    let selected = resolve_contract_terminal_files(&repo_root, Some(&workflow))
        .unwrap()
        .unwrap();

    assert_eq!(selected, vec![repo_root.join("output/deck.pptx")]);
}

#[test]
fn contract_terminal_output_fails_when_site_entrypoint_is_missing() {
    let temp = tempfile::tempdir().unwrap();
    let repo_root = temp.path().join("sites/news");
    std::fs::create_dir_all(&repo_root).unwrap();
    crate::write_workspace_policy(
        &repo_root,
        &crate::WorkspacePolicy::for_site_build_output("out"),
    )
    .unwrap();
    std::fs::write(repo_root.join("mofa-site-session.json"), "{}").unwrap();
    std::fs::write(repo_root.join("site-plan.json"), "{}").unwrap();
    std::fs::write(repo_root.join("optimized-prompt.md"), "# prompt").unwrap();

    let workflow = WorkflowMetadata {
        workflow_kind: "site".to_string(),
        current_phase: "deliver_result".to_string(),
        allowed_tools: vec!["shell".to_string()],
        terminal_output: Some(WorkflowTerminalOutputPolicy {
            deliver_final_artifact_only: true,
            forbid_intermediate_files: true,
            required_artifact_kind: "site".to_string(),
        }),
        progress: None,
    };

    let error = resolve_contract_terminal_files(&repo_root, Some(&workflow)).unwrap_err();
    assert!(error.contains("workspace contract"));
    assert!(error.contains("out/index.html"));
}
#[tokio::test]
async fn test_background_spawn_persists_workflow_phase_transitions() {
    let (in_tx, _in_rx) = tokio::sync::mpsc::channel(16);
    let temp = tempfile::tempdir().unwrap();
    let ledger = temp.path().join("tasks.jsonl");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let script_seen = temp.path().join("script_seen.md");
    let plugin_root = write_mock_podcast_plugin(temp.path(), &script_seen);
    let payloads = Arc::new(std::sync::Mutex::new(Vec::<BackgroundResultPayload>::new()));
    let payloads_for_sender = Arc::clone(&payloads);
    let sender: BackgroundResultSender = Arc::new(move |payload| {
        let payloads_for_sender = Arc::clone(&payloads_for_sender);
        Box::pin(async move {
            payloads_for_sender
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .push(payload);
            true
        })
    });
    let supervisor = Arc::new(TaskSupervisor::new());
    supervisor.enable_persistence(&ledger).unwrap();
    let tool = SpawnTool::new(
        Arc::new(MockProvider),
        Arc::new(create_test_store().await),
        workspace.clone(),
        in_tx,
    )
    .with_task_supervisor(supervisor.clone(), "api:test-session", ledger.clone())
    .with_background_result_sender(sender)
    .with_plugin_dirs(vec![plugin_root], vec![]);

    let result = tool
            .execute(&serde_json::json!({
                "task": "Produce a short podcast. Script: [杨幂 - clone:yangmi, professional] 大家好。 [窦文涛 - clone:douwentao, professional] 这里是测试播客。",
                "label": "Research podcast",
                "mode": "background",
                "allowed_tools": ["podcast_generate"],
                "workflow": {
                    "workflow_kind": "research_podcast",
                    "current_phase": "research",
                    "allowed_tools": ["podcast_generate"],
                    "terminal_output": {
                        "deliver_final_artifact_only": true,
                        "forbid_intermediate_files": true,
                        "required_artifact_kind": "audio"
                    }
                }
            }))
            .await
            .unwrap();

    assert!(result.success);

    let started = std::time::Instant::now();
    loop {
        let tasks = supervisor.get_tasks_for_session("api:test-session");
        if let Some(task) = tasks.first() {
            if task.status == crate::task_supervisor::TaskStatus::Completed {
                assert_eq!(task.output_files.len(), 1);
                assert!(task.output_files[0].ends_with(".mp3"));
                assert!(PathBuf::from(&task.output_files[0]).starts_with(&workspace));
                break;
            }
        }
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "background spawn task did not complete in time"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    let details: Vec<serde_json::Value> = std::fs::read_to_string(&ledger)
        .unwrap()
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter_map(|record| {
            record
                .get("task")
                .and_then(|task| task.get("runtime_detail"))
                .and_then(|detail| detail.as_str())
                .and_then(|detail| serde_json::from_str::<serde_json::Value>(detail).ok())
        })
        .collect();

    assert!(details.iter().any(|detail| {
        detail.get("workflow_kind").and_then(|v| v.as_str()) == Some("research_podcast")
            && detail.get("current_phase").and_then(|v| v.as_str()) == Some("research")
    }));
    assert!(details.iter().any(|detail| {
        detail.get("workflow_kind").and_then(|v| v.as_str()) == Some("research_podcast")
            && detail.get("current_phase").and_then(|v| v.as_str()) == Some("deliver_result")
    }));

    // The workflow_runtime path must seed `progress` on every phase
    // transition so dashboards (and the e2e live-progress gate) never
    // see `runtime_detail.progress == null` for workflows whose
    // internal tools do not emit per-event progress. The exact values
    // are coarse — a small starting fraction at the initial phase and
    // a near-terminal fraction once `deliver_result` is reached.
    let initial_progress = details
        .iter()
        .find(|detail| detail.get("current_phase").and_then(|v| v.as_str()) == Some("research"))
        .and_then(|detail| detail.get("progress"))
        .and_then(|v| v.as_f64())
        .expect("research phase must populate progress");
    assert!(
        (0.0..=0.5).contains(&initial_progress),
        "research-phase progress should be small but non-null, got {initial_progress}"
    );
    let deliver_progress = details
        .iter()
        .find(|detail| {
            detail.get("current_phase").and_then(|v| v.as_str()) == Some("deliver_result")
        })
        .and_then(|detail| detail.get("progress"))
        .and_then(|v| v.as_f64())
        .expect("deliver_result phase must populate progress");
    assert!(
        (0.85..=1.0).contains(&deliver_progress),
        "deliver_result progress should be near-terminal, got {deliver_progress}"
    );
    assert!(
        deliver_progress > initial_progress,
        "progress must monotonically advance from research ({initial_progress}) to deliver_result ({deliver_progress})"
    );

    let script =
        std::fs::read_to_string(&script_seen).expect("podcast_generate should receive script");
    assert!(script.contains("大家好"));

    let payloads = payloads.lock().unwrap_or_else(|error| error.into_inner());
    let media = payloads
        .iter()
        .flat_map(|payload| payload.media.iter())
        .collect::<Vec<_>>();
    assert_eq!(media.len(), 1);
    assert!(media[0].ends_with(".mp3"));
}

#[tokio::test]
async fn test_direct_background_result_short_circuits_legacy_fallback() {
    let called = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let called_clone = Arc::clone(&called);
    let sender: BackgroundResultSender = Arc::new(move |_payload| {
        let called_clone = Arc::clone(&called_clone);
        Box::pin(async move {
            called_clone.store(true, Ordering::SeqCst);
            true
        })
    });

    let payload = BackgroundResultPayload {
        task_label: "child-task".to_string(),
        content: "done".to_string(),
        kind: BackgroundResultKind::Notification,
        media: vec!["/tmp/output.mp3".to_string()],
        envelope_media: vec![],
        originating_thread_id: None,
        task_id: None,
        originating_client_message_id: None,
        tool_call_id: None,
        terminal_status: None,
    };

    assert!(deliver_background_result(Some(sender), payload.clone()).await);
    assert!(called.load(Ordering::SeqCst));
    assert!(
        !deliver_background_result(None, payload).await,
        "fallback should only be used when the direct sender is absent or rejected"
    );
}

#[tokio::test]
async fn test_background_spawn_emits_child_session_lifecycle_events() {
    let memory = Arc::new(create_test_store().await);
    let llm = Arc::new(MockProvider);
    let (tx, _rx) = tokio::sync::mpsc::channel(4);
    let supervisor = Arc::new(TaskSupervisor::new());
    let temp = Box::leak(Box::new(tempfile::tempdir().unwrap()));
    let ledger = temp.path().join("tasks.jsonl");
    let events = Arc::new(std::sync::Mutex::new(
        Vec::<ChildSessionLifecyclePayload>::new(),
    ));
    let events_ref = Arc::clone(&events);
    let sender: ChildSessionLifecycleSender = Arc::new(move |payload| {
        let events_ref = Arc::clone(&events_ref);
        Box::pin(async move {
            events_ref
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(payload);
            true
        })
    });

    let tool = SpawnTool::with_context(
        llm,
        memory,
        temp.path().to_path_buf(),
        tx,
        "api",
        "test-chat",
    )
    .with_task_supervisor(supervisor.clone(), "api:test-session".to_string(), ledger)
    .with_child_session_sender(sender);

    let args = serde_json::json!({
        "task": "Draft the report",
        "mode": "background",
        "allowed_tools": []
    });
    let result = tool.execute(&args).await.unwrap();
    assert!(result.success);

    let started = std::time::Instant::now();
    loop {
        let events = events.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if events.len() >= 2 {
            assert_eq!(events[0].kind, ChildSessionLifecycleKind::Spawned);
            assert_eq!(events[1].kind, ChildSessionLifecycleKind::Completed);
            assert_eq!(events[0].parent_session_key, "api:test-session");
            assert_eq!(events[1].parent_session_key, "api:test-session");
            assert_eq!(events[0].child_session_key, events[1].child_session_key);
            assert_eq!(events[0].task_id, events[1].task_id);
            return;
        }

        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "child-session lifecycle events did not arrive in time"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

#[tokio::test]
#[cfg(unix)]
async fn test_background_spawn_emits_verify_and_complete_hooks() {
    let memory = Arc::new(create_test_store().await);
    let llm = Arc::new(MockProvider);
    let (tx, _rx) = tokio::sync::mpsc::channel(4);
    let supervisor = Arc::new(TaskSupervisor::new());
    let temp = Box::leak(Box::new(tempfile::tempdir().unwrap()));
    let ledger = temp.path().join("tasks.jsonl");
    let hook_log = temp.path().join("spawn-hooks.jsonl");
    let hooks = Arc::new(HookExecutor::new(vec![
        capture_hook(HookEvent::OnSpawnVerify, &hook_log),
        capture_hook(HookEvent::OnSpawnComplete, &hook_log),
    ]));

    let tool = SpawnTool::with_context(
        llm,
        memory,
        temp.path().to_path_buf(),
        tx,
        "api",
        "test-chat",
    )
    .with_task_supervisor(supervisor, "api:test-session".to_string(), ledger)
    .with_hooks(hooks)
    .with_hook_context(HookContext {
        session_id: Some("api:test-session".to_string()),
        profile_id: Some("test-profile".to_string()),
    });

    let args = serde_json::json!({
        "task": "Draft the report",
        "mode": "background",
        "allowed_tools": []
    });
    let result = tool.execute(&args).await.unwrap();
    assert!(result.success);

    let started = std::time::Instant::now();
    loop {
        let lines = std::fs::read_to_string(&hook_log)
            .ok()
            .map(|contents| {
                contents
                    .lines()
                    .map(str::to_string)
                    .filter(|line| !line.is_empty())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        if lines.len() >= 2 {
            assert!(
                lines
                    .iter()
                    .any(|line| line.contains("\"event\":\"on_spawn_verify\""))
            );
            assert!(
                lines
                    .iter()
                    .any(|line| line.contains("\"event\":\"on_spawn_complete\""))
            );
            assert!(
                lines
                    .iter()
                    .all(|line| line.contains("\"session_id\":\"api:test-session\""))
            );
            return;
        }

        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "spawn lifecycle hooks did not arrive in time"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

#[test]
fn classify_child_session_failure_as_retryable_when_budget_exhausted() {
    let result = Ok::<octos_core::TaskResult, eyre::Report>(octos_core::TaskResult {
        schema_version: octos_core::TASK_RESULT_SCHEMA_VERSION,
        success: false,
        output: "Token budget exceeded (120 of 100).".to_string(),
        files_modified: vec![],
        files_to_send: vec![],
        subtasks: vec![],
        token_usage: Default::default(),
    });

    assert_eq!(
        classify_child_session_lifecycle_kind(&result),
        ChildSessionLifecycleKind::RetryableFailed
    );
}

#[test]
fn child_session_failure_action_matches_terminal_kind() {
    assert_eq!(
        child_session_failure_action(ChildSessionLifecycleKind::Completed),
        None
    );
    assert_eq!(
        child_session_failure_action(ChildSessionLifecycleKind::RetryableFailed),
        Some(ChildSessionFailureAction::Retry)
    );
    assert_eq!(
        child_session_failure_action(ChildSessionLifecycleKind::TerminalFailed),
        Some(ChildSessionFailureAction::Escalate)
    );
}

#[tokio::test]
async fn child_session_lifecycle_dispatch_defaults_to_not_joined_without_sender() {
    let joined = dispatch_child_session_lifecycle(
        None,
        ChildSessionLifecyclePayload {
            kind: ChildSessionLifecycleKind::Spawned,
            task_id: "task-123".to_string(),
            task_label: "Child task".to_string(),
            instruction: "Do work".to_string(),
            parent_session_key: "api:parent".to_string(),
            child_session_key: "api:parent#child-task-123".to_string(),
            workflow_kind: Some("deep_research".to_string()),
            current_phase: Some("execute".to_string()),
            output_files: Vec::new(),
            failure_action: None,
            error: None,
        },
    )
    .await;

    assert!(!joined);
}

// Minimal mock provider for testing
struct MockProvider;

struct ShellThenEndProvider {
    calls: std::sync::atomic::AtomicUsize,
}

#[async_trait]
impl LlmProvider for MockProvider {
    async fn chat(
        &self,
        _messages: &[octos_core::Message],
        _tools: &[octos_llm::ToolSpec],
        _config: &octos_llm::ChatConfig,
    ) -> Result<octos_llm::ChatResponse> {
        Ok(octos_llm::ChatResponse {
            content: Some("done".into()),
            reasoning_content: None,
            tool_calls: vec![],
            stop_reason: octos_llm::StopReason::EndTurn,
            usage: octos_llm::TokenUsage {
                input_tokens: 0,
                output_tokens: 0,
                ..Default::default()
            },
            provider_index: None,
        })
    }

    fn model_id(&self) -> &str {
        "mock"
    }

    fn provider_name(&self) -> &str {
        "mock"
    }
}

#[async_trait]
impl LlmProvider for ShellThenEndProvider {
    async fn chat(
        &self,
        _messages: &[octos_core::Message],
        _tools: &[octos_llm::ToolSpec],
        _config: &octos_llm::ChatConfig,
    ) -> Result<octos_llm::ChatResponse> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        if call == 0 {
            return Ok(octos_llm::ChatResponse {
                content: None,
                reasoning_content: None,
                tool_calls: vec![octos_core::ToolCall {
                    id: "call_shell".into(),
                    name: "shell".into(),
                    arguments: serde_json::json!({
                        "command": "printf ready",
                    }),
                    metadata: None,
                }],
                stop_reason: octos_llm::StopReason::ToolUse,
                usage: octos_llm::TokenUsage::default(),
                provider_index: None,
            });
        }

        Ok(octos_llm::ChatResponse {
            content: Some("done".into()),
            reasoning_content: None,
            tool_calls: vec![],
            stop_reason: octos_llm::StopReason::EndTurn,
            usage: octos_llm::TokenUsage::default(),
            provider_index: None,
        })
    }

    fn model_id(&self) -> &str {
        "mock"
    }

    fn provider_name(&self) -> &str {
        "mock"
    }
}

async fn create_test_store() -> EpisodeStore {
    let dir = tempfile::tempdir().unwrap();
    // Leak the dir so it stays alive for the test
    let dir = Box::leak(Box::new(dir));
    EpisodeStore::open(dir.path()).await.unwrap()
}

/// Emits assistant text + a `list_dir` call, then a final text answer —
/// the minimal shape of a child that plans, uses a tool, and reports.
struct ContentThenToolProvider {
    calls: std::sync::atomic::AtomicUsize,
}

#[async_trait]
impl LlmProvider for ContentThenToolProvider {
    async fn chat(
        &self,
        _messages: &[octos_core::Message],
        _tools: &[octos_llm::ToolSpec],
        _config: &octos_llm::ChatConfig,
    ) -> Result<octos_llm::ChatResponse> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        if call == 0 {
            return Ok(octos_llm::ChatResponse {
                content: Some("PLAN: scan the tree".into()),
                reasoning_content: None,
                tool_calls: vec![octos_core::ToolCall {
                    id: "call_ls".into(),
                    name: "list_dir".into(),
                    arguments: serde_json::json!({"path": "."}),
                    metadata: None,
                }],
                stop_reason: octos_llm::StopReason::ToolUse,
                usage: octos_llm::TokenUsage::default(),
                provider_index: None,
            });
        }
        Ok(octos_llm::ChatResponse {
            content: Some("FINAL: transcript test done".into()),
            reasoning_content: None,
            tool_calls: vec![],
            stop_reason: octos_llm::StopReason::EndTurn,
            usage: octos_llm::TokenUsage::default(),
            provider_index: None,
        })
    }

    fn model_id(&self) -> &str {
        "mock"
    }

    fn provider_name(&self) -> &str {
        "mock"
    }
}

#[tokio::test]
async fn background_child_transcript_streams_to_router_and_final_output_is_recorded() {
    // Mini4 re-review pipeline fix, end-to-end through a REAL detached
    // background child:
    //  (a) the child's transcript (assistant text + tool activity) must
    //      stream into the parent's SubAgentOutputRouter file — the
    //      live window `read_task_output` reads (old behaviour:
    //      SilentReporter dropped everything);
    //  (b) the child's full final result must be recorded on the task
    //      (`final_output`) through the real completion path.
    let (in_tx, _in_rx) = tokio::sync::mpsc::channel(16);
    let temp = tempfile::tempdir().unwrap();
    let ledger = temp.path().join("tasks.jsonl");
    let workspace = temp.path().join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let supervisor = Arc::new(TaskSupervisor::new());
    supervisor.enable_persistence(&ledger).unwrap();
    let router = Arc::new(crate::subagent_output::SubAgentOutputRouter::new(
        temp.path().join("router"),
    ));
    let tool = SpawnTool::new(
        Arc::new(ContentThenToolProvider {
            calls: std::sync::atomic::AtomicUsize::new(0),
        }),
        Arc::new(create_test_store().await),
        workspace.clone(),
        in_tx,
    )
    .with_task_supervisor(supervisor.clone(), "api:test-session", ledger.clone())
    .with_parent_subagent_output_router(router.clone());

    let result = tool
        .execute(&serde_json::json!({
            "task": "scan the workspace and report",
            "label": "transcripter",
            "mode": "background",
            "allowed_tools": ["list_dir"]
        }))
        .await
        .unwrap();
    assert!(result.success, "dispatch failed: {}", result.output);

    let started = std::time::Instant::now();
    let task = loop {
        let tasks = supervisor.get_tasks_for_session("api:test-session");
        if let Some(task) = tasks.first() {
            if task.status == crate::task_supervisor::TaskStatus::Completed {
                break task.clone();
            }
            if task.status == crate::task_supervisor::TaskStatus::Failed {
                panic!("background child failed: {:?}", task.error);
            }
        }
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "background child did not complete in time"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    };

    // (b) full final result recorded on the task record.
    let final_output = task
        .final_output
        .as_deref()
        .expect("final_output must be recorded at completion");
    assert!(
        final_output.contains("FINAL: transcript test done"),
        "final_output must carry the child's answer: {final_output:?}"
    );
    assert!(final_output.contains("Status: SUCCESS"));

    // (a) the transcript reached the parent's router file.
    let transcript = router
        .preview(&task.id)
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_default();
    assert!(
        transcript.contains("[tool] list_dir"),
        "tool start must stream to the router: {transcript:?}"
    );
    assert!(
        transcript.contains("[tool ok] list_dir"),
        "tool completion must stream to the router: {transcript:?}"
    );
    assert!(
        transcript.contains("FINAL: transcript test done"),
        "assistant text must stream to the router: {transcript:?}"
    );
}

/// PR #1799 fix pins, all through a REAL detached background child.
///
/// (a) the `child_stream_callback` receives live `(task_id, start_offset,
///     text)` chunks with START-offset cursor semantics (offset of the
///     window's first byte — the convention every sibling cursor producer
///     uses), monotonically: offset[i+1] == offset[i] + len(text[i]).
/// (b) duplication pin: each child message lands in the router file
///     EXACTLY once. Before the fix the StreamChunk arm appended live
///     text to the same `<task_id>.out` the Response arm appends full
///     iteration text to, doubling every message (the exact duplication
///     the original drop-comment warned about).
#[tokio::test]
async fn child_stream_callback_gets_start_offsets_and_router_file_has_no_duplicates() {
    let (in_tx, _in_rx) = tokio::sync::mpsc::channel(16);
    let temp = tempfile::tempdir().unwrap();
    let ledger = temp.path().join("tasks.jsonl");
    let workspace = temp.path().join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let supervisor = Arc::new(TaskSupervisor::new());
    supervisor.enable_persistence(&ledger).unwrap();
    let router = Arc::new(crate::subagent_output::SubAgentOutputRouter::new(
        temp.path().join("router"),
    ));
    let chunks: Arc<std::sync::Mutex<Vec<(String, u64, String)>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let chunks_sink = Arc::clone(&chunks);
    let tool = SpawnTool::new(
        Arc::new(ContentThenToolProvider {
            calls: std::sync::atomic::AtomicUsize::new(0),
        }),
        Arc::new(create_test_store().await),
        workspace.clone(),
        in_tx,
    )
    .with_task_supervisor(supervisor.clone(), "api:test-session", ledger.clone())
    .with_parent_subagent_output_router(router.clone())
    .with_child_stream_callback(move |task_id, offset, text| {
        chunks_sink
            .lock()
            .unwrap()
            .push((task_id.to_owned(), offset, text.to_owned()));
    });

    let result = tool
        .execute(&serde_json::json!({
            "task": "scan the workspace and report",
            "label": "streamer",
            "mode": "background",
            "allowed_tools": ["list_dir"]
        }))
        .await
        .unwrap();
    assert!(result.success, "dispatch failed: {}", result.output);

    let started = std::time::Instant::now();
    let task = loop {
        let tasks = supervisor.get_tasks_for_session("api:test-session");
        if let Some(task) = tasks.first() {
            if task.status == crate::task_supervisor::TaskStatus::Completed {
                break task.clone();
            }
            if task.status == crate::task_supervisor::TaskStatus::Failed {
                panic!("background child failed: {:?}", task.error);
            }
        }
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "background child did not complete in time"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    };

    // (a) live chunks arrived, keyed by the spawned task id, with
    // start-offset cursors: first window starts at 0, and each next
    // window starts where the previous ended.
    let chunks = chunks.lock().unwrap().clone();
    assert!(
        chunks.len() >= 2,
        "expected a chunk per iteration (plan + final), got {chunks:?}"
    );
    for (task_id, _, _) in &chunks {
        assert_eq!(task_id, &task.id, "chunks are keyed by the child task id");
    }
    assert_eq!(chunks[0].1, 0, "the FIRST window starts at offset 0");
    let mut expected = 0u64;
    for (_, offset, text) in &chunks {
        assert_eq!(
            *offset, expected,
            "start-offset must be the cumulative streamed bytes BEFORE the chunk: {chunks:?}"
        );
        expected += text.len() as u64;
    }

    // (b) NO duplication in the router file: streamed live text goes only
    // to the callback; the router transcript is the Response-based durable
    // record, one copy per message.
    let transcript = router
        .preview(&task.id)
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_default();
    assert_eq!(
        transcript.matches("FINAL: transcript test done").count(),
        1,
        "the final text must appear EXACTLY once (StreamChunk must not \
         double the Response append): {transcript:?}"
    );
    assert_eq!(
        transcript.matches("PLAN: scan the tree").count(),
        1,
        "iteration text must appear EXACTLY once: {transcript:?}"
    );
}

/// PR #1799 fix pin (c): `child_spawn_clone` preserves the stream callback
/// so grandchildren keep streaming to the dock (the #1679 lesson: child
/// registries silently losing parent wiring).
#[tokio::test]
async fn child_spawn_clone_preserves_stream_callback() {
    let (in_tx, _in_rx) = tokio::sync::mpsc::channel(16);
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let tool = SpawnTool::new(
        Arc::new(MockProvider),
        Arc::new(create_test_store().await),
        workspace.clone(),
        in_tx,
    )
    .with_child_stream_callback(|_, _, _| {});
    let clone = tool.child_spawn_clone(workspace, &AgentId::new("child-1"));
    assert!(
        clone.child_stream_callback.is_some(),
        "the grandchild spawn path must inherit the stream callback"
    );
}

/// Emits one `shell` call that writes a deliverable via a redirect —
/// reporting no `file_modified`, exactly like the mini4 heredoc reviews —
/// then ends the turn.
struct ShellDeliverableProvider {
    calls: std::sync::atomic::AtomicUsize,
}

#[async_trait]
impl LlmProvider for ShellDeliverableProvider {
    async fn chat(
        &self,
        _messages: &[octos_core::Message],
        _tools: &[octos_llm::ToolSpec],
        _config: &octos_llm::ChatConfig,
    ) -> Result<octos_llm::ChatResponse> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        if call == 0 {
            return Ok(octos_llm::ChatResponse {
                content: None,
                reasoning_content: None,
                tool_calls: vec![octos_core::ToolCall {
                    id: "call_shell".into(),
                    name: "shell".into(),
                    arguments: serde_json::json!({
                        "command": "printf '# Review\\n' > octos-review.md",
                    }),
                    metadata: None,
                }],
                stop_reason: octos_llm::StopReason::ToolUse,
                usage: octos_llm::TokenUsage::default(),
                provider_index: None,
            });
        }
        Ok(octos_llm::ChatResponse {
            content: Some("wrote the review".into()),
            reasoning_content: None,
            tool_calls: vec![],
            stop_reason: octos_llm::StopReason::EndTurn,
            usage: octos_llm::TokenUsage::default(),
            provider_index: None,
        })
    }

    fn model_id(&self) -> &str {
        "mock"
    }

    fn provider_name(&self) -> &str {
        "mock"
    }
}

#[tokio::test]
async fn background_deliverable_surfaces_shell_written_file_in_output_files() {
    // End-to-end reproduction of the mini4 "zero deliverable" flow: a
    // background spawn whose worker writes its deliverable with a raw
    // `shell` redirect — NO write_file, so no `file_modified` — must still
    // land in the task ledger's `output_files`, surfaced by the seeded
    // workspace contract rather than the tool-record path.
    let (in_tx, _in_rx) = tokio::sync::mpsc::channel(16);
    let temp = tempfile::tempdir().unwrap();
    let ledger = temp.path().join("tasks.jsonl");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let supervisor = Arc::new(TaskSupervisor::new());
    supervisor.enable_persistence(&ledger).unwrap();
    let tool = SpawnTool::new(
        Arc::new(ShellDeliverableProvider {
            calls: std::sync::atomic::AtomicUsize::new(0),
        }),
        Arc::new(create_test_store().await),
        workspace.clone(),
        in_tx,
    )
    .with_task_supervisor(supervisor.clone(), "api:test-session", ledger.clone())
    .with_sandbox(SandboxConfig {
        mode: crate::sandbox::SandboxMode::None,
        ..Default::default()
    });

    let result = tool
        .execute(&serde_json::json!({
            "task": "Review the repo, then write octos-review.md",
            "label": "reviewer",
            "mode": "background",
            "allowed_tools": ["shell"],
            "deliverable": "*.md"
        }))
        .await
        .unwrap();
    assert!(result.success, "spawn dispatch failed: {}", result.output);

    let started = std::time::Instant::now();
    loop {
        let tasks = supervisor.get_tasks_for_session("api:test-session");
        if let Some(task) = tasks.first() {
            match task.status {
                crate::task_supervisor::TaskStatus::Completed => {
                    assert_eq!(
                        task.output_files.len(),
                        1,
                        "the shell-written deliverable must surface in output_files: {:?}",
                        task.output_files
                    );
                    assert!(
                        task.output_files[0].ends_with("octos-review.md"),
                        "output_files[0] = {:?}",
                        task.output_files[0]
                    );
                    break;
                }
                crate::task_supervisor::TaskStatus::Failed => {
                    panic!("background deliverable spawn failed: {:?}", task.error);
                }
                _ => {}
            }
        }
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "background deliverable spawn did not complete in time"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn background_deliverable_uses_configured_root_without_touching_workspace_state() {
    let (in_tx, _in_rx) = tokio::sync::mpsc::channel(16);
    let temp = tempfile::tempdir().unwrap();
    let ledger = temp.path().join("tasks.jsonl");
    let workspace = temp.path().join("workspace");
    let deliverable_root = temp.path().join("runtime").join("spawn-deliverables");
    std::fs::create_dir_all(&workspace).unwrap();
    let supervisor = Arc::new(TaskSupervisor::new());
    supervisor.enable_persistence(&ledger).unwrap();
    let tool = SpawnTool::new(
        Arc::new(ShellDeliverableProvider {
            calls: std::sync::atomic::AtomicUsize::new(0),
        }),
        Arc::new(create_test_store().await),
        workspace.clone(),
        in_tx,
    )
    .with_task_supervisor(supervisor.clone(), "api:test-session", ledger)
    .with_deliverable_root(deliverable_root.clone())
    .with_workspace_write_access(false)
    .with_sandbox(SandboxConfig {
        mode: crate::sandbox::SandboxMode::None,
        ..Default::default()
    });

    let result = tool
        .execute(&serde_json::json!({
            "task": "Review the repo, then write octos-review.md",
            "label": "reviewer",
            "mode": "background",
            "allowed_tools": ["shell"],
            "deliverable": "*.md"
        }))
        .await
        .unwrap();
    assert!(result.success, "spawn dispatch failed: {}", result.output);

    let started = std::time::Instant::now();
    loop {
        let tasks = supervisor.get_tasks_for_session("api:test-session");
        if let Some(task) = tasks.first() {
            match task.status {
                crate::task_supervisor::TaskStatus::Completed => {
                    assert_eq!(task.output_files.len(), 1, "{:?}", task.output_files);
                    assert!(
                        std::path::Path::new(&task.output_files[0]).starts_with(&deliverable_root),
                        "deliverable must be outside the workspace: {:?}",
                        task.output_files
                    );
                    assert!(
                        !workspace.join(".octos").exists(),
                        "deliverable setup must not create workspace state"
                    );
                    break;
                }
                crate::task_supervisor::TaskStatus::Failed => {
                    panic!("background deliverable spawn failed: {:?}", task.error);
                }
                _ => {}
            }
        }
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "background deliverable spawn did not complete in time"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn read_only_spawn_refuses_deliverable_without_external_root() {
    let (in_tx, _in_rx) = tokio::sync::mpsc::channel(1);
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let tool = SpawnTool::new(
        Arc::new(MockProvider),
        Arc::new(create_test_store().await),
        workspace.clone(),
        in_tx,
    )
    .with_workspace_write_access(false);

    let result = tool
        .execute(&serde_json::json!({
            "task": "review the workspace",
            "mode": "sync",
            "deliverable": "*.md"
        }))
        .await
        .unwrap();

    assert!(!result.success);
    assert!(
        result.output.contains("external deliverable root"),
        "{}",
        result.output
    );
    assert!(
        !workspace.join(".octos").exists(),
        "read-only deliverable refusal must leave no workspace state"
    );
}

#[tokio::test]
async fn read_only_spawn_refuses_worktree_isolation_without_creating_workspace_state() {
    let (in_tx, _in_rx) = tokio::sync::mpsc::channel(1);
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let tool = SpawnTool::new(
        Arc::new(MockProvider),
        Arc::new(create_test_store().await),
        workspace.clone(),
        in_tx,
    )
    .with_workspace_write_access(false);

    let result = tool
        .execute(&serde_json::json!({
            "task": "review the workspace",
            "mode": "sync",
            "isolation": "worktree"
        }))
        .await
        .unwrap();

    assert!(!result.success);
    assert!(result.output.contains("read-only"), "{}", result.output);
    assert!(
        !workspace.join(".octos").exists(),
        "read-only worktree refusal must leave no workspace state"
    );
}

#[tokio::test]
async fn background_without_deliverable_does_not_surface_shell_write() {
    // Control for the test above: the SAME shell-written file does NOT
    // surface without `deliverable` — confirming the deliverable contract
    // (not some other path) is what closes the gap.
    let (in_tx, _in_rx) = tokio::sync::mpsc::channel(16);
    let temp = tempfile::tempdir().unwrap();
    let ledger = temp.path().join("tasks.jsonl");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let supervisor = Arc::new(TaskSupervisor::new());
    supervisor.enable_persistence(&ledger).unwrap();
    let tool = SpawnTool::new(
        Arc::new(ShellDeliverableProvider {
            calls: std::sync::atomic::AtomicUsize::new(0),
        }),
        Arc::new(create_test_store().await),
        workspace.clone(),
        in_tx,
    )
    .with_task_supervisor(supervisor.clone(), "api:test-session", ledger.clone())
    .with_sandbox(SandboxConfig {
        mode: crate::sandbox::SandboxMode::None,
        ..Default::default()
    });

    let result = tool
        .execute(&serde_json::json!({
            "task": "Review the repo, then write octos-review.md",
            "label": "reviewer",
            "mode": "background",
            "allowed_tools": ["shell"]
            // no `deliverable`
        }))
        .await
        .unwrap();
    assert!(result.success, "spawn dispatch failed: {}", result.output);

    let started = std::time::Instant::now();
    loop {
        let tasks = supervisor.get_tasks_for_session("api:test-session");
        if let Some(task) = tasks.first() {
            match task.status {
                crate::task_supervisor::TaskStatus::Completed => {
                    assert!(
                        task.output_files.is_empty(),
                        "without a deliverable contract the shell write must NOT surface \
                             (the bug being fixed): {:?}",
                        task.output_files
                    );
                    break;
                }
                crate::task_supervisor::TaskStatus::Failed => {
                    panic!("spawn failed: {:?}", task.error);
                }
                _ => {}
            }
        }
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "background spawn did not complete in time"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

/// Build a minimal `Input` from a JSON value with the defaults the
/// tests expect. Centralising this keeps the M8.2 manifest tests below
/// independent of future serde changes.
fn parse_spawn_input(value: serde_json::Value) -> Input {
    serde_json::from_value(value).expect("input parses")
}

#[test]
fn resolve_spawn_max_iterations_defaults_and_bounds() {
    // Not requested → the generous spawn default (NOT the interactive 50).
    assert_eq!(
        resolve_spawn_max_iterations(None),
        DEFAULT_SPAWN_MAX_ITERATIONS
    );
    // (That it exceeds the interactive 50 is asserted at compile time
    // beside the constant itself.)
    // Normal value passes through.
    assert_eq!(resolve_spawn_max_iterations(Some(120)), 120);
    // 0 is nonsensical → clamped up to 1.
    assert_eq!(resolve_spawn_max_iterations(Some(0)), 1);
    // Over the ceiling → clamped down (runaway-loop guard).
    assert_eq!(
        resolve_spawn_max_iterations(Some(100_000)),
        MAX_SPAWN_MAX_ITERATIONS
    );
    assert_eq!(
        resolve_spawn_max_iterations(Some(MAX_SPAWN_MAX_ITERATIONS)),
        MAX_SPAWN_MAX_ITERATIONS
    );
}

#[test]
fn input_parses_max_iterations_and_defaults_to_none() {
    let with = parse_spawn_input(serde_json::json!({
        "task": "review the repo",
        "max_iterations": 150
    }));
    assert_eq!(with.max_iterations, Some(150));

    // Absent → None → worker keeps its default 50.
    let without = parse_spawn_input(serde_json::json!({ "task": "quick task" }));
    assert_eq!(without.max_iterations, None);
}

#[tokio::test]
async fn spawn_spec_exposes_max_iterations() {
    let (in_tx, _in_rx) = tokio::sync::mpsc::channel(16);
    let tool = SpawnTool::new(
        Arc::new(MockProvider),
        Arc::new(create_test_store().await),
        PathBuf::from("/tmp"),
        in_tx,
    );
    let schema = tool.input_schema();
    let props = schema["properties"].as_object().expect("properties object");
    assert!(
        props.contains_key("max_iterations"),
        "spawn schema must expose max_iterations so the model can raise the budget"
    );
    assert_eq!(props["max_iterations"]["type"], "integer");
}

#[tokio::test]
async fn spawn_schema_warns_narrowed_allowed_tools_needs_write_file_or_deliverable() {
    // Guards the guidance that stops the "worker shell-wrote to /tmp, deliverable
    // lost" failure: if the orchestrator narrows allowed_tools, it must include
    // write_file OR set a deliverable glob. Keyed loosely so rewording is fine.
    let (in_tx, _in_rx) = tokio::sync::mpsc::channel(16);
    let tool = SpawnTool::new(
        Arc::new(MockProvider),
        Arc::new(create_test_store().await),
        PathBuf::from("/tmp"),
        in_tx,
    );
    let desc = tool.input_schema()["properties"]["allowed_tools"]["description"]
        .as_str()
        .expect("allowed_tools description present")
        .to_string();
    assert!(
        desc.contains("write_file"),
        "allowed_tools guidance must mention write_file: {desc}"
    );
    assert!(
        desc.contains("deliverable"),
        "allowed_tools guidance must point at the deliverable glob: {desc}"
    );
}

#[test]
fn apply_agent_definition_preserves_inline_max_iterations() {
    // An inline max_iterations must survive manifest layering (inline wins /
    // is untouched — the manifest carries no iteration budget).
    let registry = crate::agents::AgentDefinitions::with_builtins();
    let mut input = parse_spawn_input(serde_json::json!({
        "task": "research this topic",
        "agent_definition_id": "research-worker",
        "max_iterations": 200
    }));
    apply_agent_definition(&mut input, &registry).expect("apply");
    assert_eq!(input.max_iterations, Some(200));
}

#[test]
fn should_resolve_manifest_in_spawn_tool() {
    // Spawn args reference `research-worker`; the manifest's `tools`
    // list must flow into the resolved `Input.allowed_tools`. Inline
    // `allowed_tools` is empty so the manifest fills it in.
    let registry = crate::agents::AgentDefinitions::with_builtins();
    let mut input = parse_spawn_input(serde_json::json!({
        "task": "research this topic",
        "agent_definition_id": "research-worker"
    }));
    apply_agent_definition(&mut input, &registry).expect("apply");

    // Research-worker manifest lists deep_search + web_fetch + web_search.
    for expected in ["search", "web_fetch", "web_search"] {
        assert!(
            input.allowed_tools.contains(&expected.to_string()),
            "manifest tool {expected} did not flow into allowed_tools"
        );
    }
    // Manifest's disallowed_tools (shell/write/edit) must not appear.
    for forbidden in ["shell", "write_file", "edit_file"] {
        assert!(
            !input.allowed_tools.contains(&forbidden.to_string()),
            "manifest disallowed_tool {forbidden} leaked into allowed_tools"
        );
    }
}

#[test]
fn should_let_inline_fields_override_manifest() {
    // Inline `model` must beat the manifest's `model`. The manifest
    // sets no model on `research-worker`, so we use a local manifest
    // that has one to make the override visible.
    let mut registry = crate::agents::AgentDefinitions::new();
    registry.insert(
        "with-model",
        crate::agents::AgentDefinition::from_json_str(
            r#"{
                    "name": "with-model",
                    "version": 1,
                    "tools": ["read_file"],
                    "model": "manifest-model"
                }"#,
        )
        .expect("parse"),
    );

    let mut input = parse_spawn_input(serde_json::json!({
        "task": "do it",
        "agent_definition_id": "with-model",
        "model": "inline-model"
    }));
    apply_agent_definition(&mut input, &registry).expect("apply");

    // Inline wins for model.
    assert_eq!(input.model.as_deref(), Some("inline-model"));
}

#[test]
fn manifest_disallowed_tools_are_denied_by_policy_not_pruned_from_allow_list() {
    // inline [shell, grep] + manifest disallowed [shell]. The allow-list is
    // NOT mutated (shell stays); apply_agent_definition returns the
    // disallowed list, which build_subagent_tool_policy enforces as a
    // DENY. Deny wins over allow → shell blocked, grep allowed.
    let mut registry = crate::agents::AgentDefinitions::new();
    registry.insert(
        "example",
        crate::agents::AgentDefinition::from_json_str(
            r#"{
                    "name": "example",
                    "version": 1,
                    "tools": ["read_file", "shell"],
                    "disallowed_tools": ["shell"]
                }"#,
        )
        .expect("parse"),
    );

    let mut input = parse_spawn_input(serde_json::json!({
        "task": "do it",
        "agent_definition_id": "example",
        "allowed_tools": ["shell", "grep"]
    }));
    let disallowed = apply_agent_definition(&mut input, &registry).expect("apply");

    assert_eq!(disallowed, vec!["shell".to_string()]);
    // Inline list is kept verbatim — disallow is NOT a prune anymore.
    assert!(input.allowed_tools.contains(&"grep".to_string()));
    assert!(input.allowed_tools.contains(&"shell".to_string()));

    let policy = build_subagent_tool_policy(input.allowed_tools.clone(), disallowed, None);
    assert!(
        !policy.is_allowed("shell"),
        "manifest-disallowed shell must be denied by policy"
    );
    assert!(policy.is_allowed("grep"), "grep must remain allowed");
}

#[test]
fn manifest_forbidding_its_only_tool_grants_no_tools_not_allow_all() {
    // P1 (the original finding): manifest tools:[shell] + disallowed:[shell]
    // with no inline and no role. The OLD retain emptied the allow-list,
    // which ToolPolicy reads as "allow ALL". Now the allow-list keeps
    // [shell] (filled from def.tools, NOT pruned) and shell is denied, so
    // the effective tool set is EMPTY — never allow-all.
    let mut registry = crate::agents::AgentDefinitions::new();
    registry.insert(
        "shell-then-forbid-shell",
        crate::agents::AgentDefinition::from_json_str(
            r#"{
                    "name": "shell-then-forbid-shell",
                    "version": 1,
                    "tools": ["shell"],
                    "disallowed_tools": ["shell"]
                }"#,
        )
        .expect("parse"),
    );

    let mut input = parse_spawn_input(serde_json::json!({
        "task": "do it",
        "agent_definition_id": "shell-then-forbid-shell"
    }));
    let disallowed = apply_agent_definition(&mut input, &registry).expect("apply");
    assert_eq!(disallowed, vec!["shell".to_string()]);

    let policy = build_subagent_tool_policy(input.allowed_tools.clone(), disallowed, None);
    assert!(!policy.is_allowed("shell"), "the forbidden tool is denied");
    // The critical anti-inversion assertions: NOT allow-all.
    assert!(
        !policy.is_allowed("read_file"),
        "must NOT fall through to allow-all"
    );
    assert!(
        !policy.is_allowed("web_fetch"),
        "must NOT fall through to allow-all"
    );
}

#[test]
fn role_refill_cannot_reintroduce_a_manifest_disallowed_tool() {
    // Codex P1: a spawn that ALSO supplies a `role` used to re-fill the
    // emptied allow-list with the role's tool budget, re-introducing a
    // manifest-forbidden tool. Because disallowed_tools is now a policy
    // deny (not a one-time allow-list prune), a role-provided tool that
    // overlaps the deny-list is still blocked — deny wins over allow.
    //
    // Simulates the post-`apply_role_template` state: the allow-list has
    // been refilled with the role's tools (including the forbidden one).
    let role_refilled_allow = vec![
        "shell".to_string(),
        "read_file".to_string(),
        "edit_file".to_string(),
    ];
    let manifest_disallowed = vec!["shell".to_string()];

    let policy = build_subagent_tool_policy(role_refilled_allow, manifest_disallowed, None);
    assert!(
        !policy.is_allowed("shell"),
        "a role-provided tool must still be denied by the manifest's disallowed_tools"
    );
    assert!(policy.is_allowed("read_file"));
    assert!(policy.is_allowed("edit_file"));
}

#[test]
fn effective_allowed_tools_prunes_manifest_disallowed_from_the_list() {
    // Codex P1: the list handed to the agent_mcp dispatch payload (and the
    // preflight) must be `allowed_tools` MINUS the manifest's disallowed
    // tools — the remote agent never runs the local deny-list policy.
    //
    // Both-allowed-and-disallowed → nothing survives. (An empty list on
    // the wire means "no explicit tools", NOT "allow all" — that inversion
    // only bites the in-process ToolPolicy, which keeps the full list.)
    assert!(
        effective_allowed_tools(&["shell".to_string()], &["shell".to_string()]).is_empty(),
        "a tool that is both allowed and disallowed must not reach the remote"
    );
    // Mixed: only the non-disallowed tool survives.
    assert_eq!(
        effective_allowed_tools(
            &["read_file".to_string(), "shell".to_string()],
            &["shell".to_string()],
        ),
        vec!["read_file".to_string()],
    );
    // No disallow → identity.
    assert_eq!(
        effective_allowed_tools(&["grep".to_string()], &[]),
        vec!["grep".to_string()],
    );
}

#[test]
fn effective_allowed_tools_prunes_with_policy_semantics_not_exact_match() {
    // Codex P2 (fold 2): `disallowed_tools` entries carry the same
    // wildcard/group semantics ToolPolicy enforces locally. An exact
    // `contains` prune would let `shell` (denied via `group:runtime`) or
    // `podcast_generate` (denied via `podcast_*`) reach the agent_mcp
    // payload and the preflight — re-opening the bypass for exactly the
    // deny spellings the local policy honors.
    assert_eq!(
        effective_allowed_tools(
            &["shell".to_string(), "read_file".to_string()],
            &["group:runtime".to_string()],
        ),
        vec!["read_file".to_string()],
        "a group deny entry must prune its member tools"
    );
    assert_eq!(
        effective_allowed_tools(
            &["podcast_generate".to_string(), "grep".to_string()],
            &["podcast_*".to_string()],
        ),
        vec!["grep".to_string()],
        "a wildcard deny entry must prune matching tools"
    );
}

#[test]
fn effective_allowed_tools_prunes_a_group_entry_denied_verbatim() {
    // Codex P1 (fold 3): the allow-list may itself carry a group entry
    // (`tools: ["group:runtime"]`). `entry_matches` expands a denied group
    // only against CONCRETE member names — the group string is not a
    // member of itself — so pure policy filtering lets the denied group
    // survive into the agent_mcp payload / preflight. The exact-contains
    // check must prune identical entries too (as the pre-policy-semantics
    // code did).
    assert_eq!(
        effective_allowed_tools(
            &["group:runtime".to_string(), "read_file".to_string()],
            &["group:runtime".to_string()],
        ),
        vec!["read_file".to_string()],
        "a group entry denied verbatim must not survive the prune"
    );
    // Same belt-and-braces for a verbatim wildcard entry.
    assert_eq!(
        effective_allowed_tools(&["podcast_*".to_string()], &["podcast_*".to_string()]),
        Vec::<String>::new(),
        "a wildcard entry denied verbatim must not survive the prune"
    );
}

#[test]
fn preflight_does_not_reject_a_disallowed_but_unavailable_tool() {
    // Codex P2: once `disallowed_tools` stopped pruning `allowed_tools`,
    // the availability preflight (run on the FULL allow-list) would fail a
    // spawn for a manifest-forbidden tool that isn't installed on the host
    // — even though the policy denies it anyway. Preflighting the EFFECTIVE
    // (post-deny) set fixes that.
    let tools = ToolRegistry::with_builtins("/tmp");
    let allowed = vec!["shell".to_string(), "podcast_generate".to_string()];
    let disallowed = vec!["podcast_generate".to_string()];

    // The OLD ordering (preflight on the raw allow-list) rejects the spawn
    // because `podcast_generate` is not available on this host:
    assert!(
        ensure_subagent_tools_available(&tools, &allowed, true).is_err(),
        "sanity: the unfiltered allow-list still trips the missing-tool guard"
    );
    // The FIX: preflight the EFFECTIVE set — the forbidden-and-missing tool
    // is gone, so the otherwise-valid `shell`-only spawn is admitted.
    let effective = effective_allowed_tools(&allowed, &disallowed);
    ensure_subagent_tools_available(&tools, &effective, true)
        .expect("a disallowed-and-missing tool must not fail the preflight");
}

#[tokio::test]
async fn agent_mcp_dispatch_payload_sends_effective_allow_list_not_the_raw_one() {
    // Codex P1 (wiring guard): the `agent_mcp` dispatch payload is the ONLY
    // tool list the remote agent ever sees — it never runs the local
    // deny-list ToolPolicy. If the payload carried the RAW `allowed_tools`,
    // a manifest that both allows and forbids `shell` would still grant the
    // remote `shell`, bypassing the deny-list fix. Drive a real spawn
    // through a recording backend and assert the payload carries the
    // EFFECTIVE (post-deny) allow-list, plus the deny-list for a remote
    // that can honor it.
    use crate::tools::mcp_agent::McpAgentBackend;
    use std::sync::Mutex;

    struct RecordingBackend {
        seen: Arc<Mutex<Option<serde_json::Value>>>,
    }
    #[async_trait]
    impl McpAgentBackend for RecordingBackend {
        fn backend_label(&self) -> &'static str {
            "local"
        }
        fn endpoint_label(&self) -> String {
            "recording".to_string()
        }
        async fn dispatch(&self, request: DispatchRequest) -> DispatchResponse {
            *self.seen.lock().unwrap() = Some(request.task.clone());
            DispatchResponse {
                outcome: DispatchOutcome::Success,
                output: "recorded".to_string(),
                files_to_send: Vec::new(),
                error: None,
                context_contract: None,
            }
        }
    }

    let mut registry = crate::agents::AgentDefinitions::new();
    registry.insert(
        "example",
        crate::agents::AgentDefinition::from_json_str(
            r#"{
                    "name": "example",
                    "version": 1,
                    "tools": ["shell", "grep"],
                    "disallowed_tools": ["shell"]
                }"#,
        )
        .expect("parse manifest"),
    );
    let mut ctx = crate::tools::ToolContext::zero();
    ctx.agent_definitions = Arc::new(registry);

    let seen = Arc::new(Mutex::new(None));
    let backend: SharedBackend = Arc::new(RecordingBackend { seen: seen.clone() });
    let (in_tx, _in_rx) = tokio::sync::mpsc::channel(16);
    let tool = SpawnTool::new(
        Arc::new(MockProvider),
        Arc::new(create_test_store().await),
        PathBuf::from("/tmp"),
        in_tx,
    )
    .with_mcp_agent_backend(backend, Some("run_task".to_string()));

    // Return value is irrelevant (post-dispatch validation may FAIL the
    // artifact) — the payload is captured at dispatch time, before any of
    // that runs.
    let _ = tool
        .execute_with_context(
            &ctx,
            &serde_json::json!({
                "task": "do it",
                "agent_definition_id": "example",
                "allowed_tools": ["shell", "grep"],
                "backend": "agent_mcp",
                "mode": "sync"
            }),
        )
        .await;

    let payload = seen
        .lock()
        .unwrap()
        .clone()
        .expect("the recording backend must have been dispatched to");
    let allowed: Vec<&str> = payload
        .get("allowed_tools")
        .and_then(|v| v.as_array())
        .expect("payload carries allowed_tools")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert_eq!(
        allowed,
        vec!["grep"],
        "agent_mcp payload must send the EFFECTIVE allow-list (shell pruned), not the raw one"
    );
    // The deny-list is also forwarded so a remote that honors an explicit
    // deny-list can enforce it even if a role template there re-expands.
    let disallowed: Vec<&str> = payload
        .get("disallowed_tools")
        .and_then(|v| v.as_array())
        .expect("payload carries disallowed_tools")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert_eq!(disallowed, vec!["shell"]);
}

#[tokio::test]
async fn sync_spawn_admits_a_disallowed_but_unavailable_tool_via_effective_preflight() {
    // Codex P2 (wiring guard): the builtin sync spawn path preflights tool
    // availability. Before the fix it checked the RAW allow-list, so a
    // manifest that forbids an uninstalled tool (`podcast_generate`) failed
    // the whole spawn with "required tool(s) not available" — even though
    // the deny-list blocks that tool anyway. After the fix it preflights
    // the EFFECTIVE set, so the otherwise-valid `shell`-only spawn runs.
    let mut registry = crate::agents::AgentDefinitions::new();
    registry.insert(
        "forbids-missing",
        crate::agents::AgentDefinition::from_json_str(
            r#"{
                    "name": "forbids-missing",
                    "version": 1,
                    "tools": ["shell", "podcast_generate"],
                    "disallowed_tools": ["podcast_generate"]
                }"#,
        )
        .expect("parse manifest"),
    );
    let mut ctx = crate::tools::ToolContext::zero();
    ctx.agent_definitions = Arc::new(registry);

    let (in_tx, _in_rx) = tokio::sync::mpsc::channel(16);
    let tool = SpawnTool::new(
        Arc::new(MockProvider),
        Arc::new(create_test_store().await),
        PathBuf::from("/tmp"),
        in_tx,
    );

    let result = tool
        .execute_with_context(
            &ctx,
            &serde_json::json!({
                "task": "do it",
                "agent_definition_id": "forbids-missing",
                "allowed_tools": ["shell", "podcast_generate"],
                "mode": "sync"
            }),
        )
        .await
        .unwrap();

    assert!(
        !result.output.contains("required tool(s) not available"),
        "a disallowed-and-missing tool must not fail the sync spawn preflight; got: {}",
        result.output
    );
    assert!(
        result.success,
        "the shell-only spawn should run to completion: {}",
        result.output
    );
}

#[test]
fn should_error_when_agent_definition_id_unknown() {
    // Typos in the id are a hard error so a silent-typo cannot erase
    // the manifest's safety envelope.
    let registry = crate::agents::AgentDefinitions::with_builtins();
    let mut input = parse_spawn_input(serde_json::json!({
        "task": "do it",
        "agent_definition_id": "no-such-manifest"
    }));
    let err = apply_agent_definition(&mut input, &registry).unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("no-such-manifest"), "message: {msg}");
}

#[test]
fn should_not_mutate_input_when_agent_definition_id_missing() {
    // No id means no resolution. This preserves the fast path for
    // callers that never touch manifests.
    let registry = crate::agents::AgentDefinitions::with_builtins();
    let mut input = parse_spawn_input(serde_json::json!({
        "task": "plain spawn",
        "allowed_tools": ["shell"]
    }));
    let before = input.clone();
    apply_agent_definition(&mut input, &registry).expect("apply");

    assert_eq!(input.allowed_tools, before.allowed_tools);
    assert_eq!(input.model, before.model);
}

// ────────── M8 Runtime Parity W2.B1 wiring tests ──────────

/// A SpawnTool built without explicit parent caches must keep the
/// pre-W2 default — `None` on every parent introspection helper —
/// so unrelated callers don't pay any cost from the new optional
/// fields.
#[tokio::test]
async fn spawn_tool_default_has_no_parent_caches() {
    let (in_tx, _in_rx) = tokio::sync::mpsc::channel(16);
    let tool = SpawnTool::new(
        Arc::new(MockProvider),
        Arc::new(create_test_store().await),
        PathBuf::from("/tmp"),
        in_tx,
    );
    assert!(tool.parent_file_state_cache().is_none());
    assert!(tool.parent_subagent_output_router().is_none());
    assert!(tool.parent_subagent_summary_generator().is_none());
}

// ────────── M8 Runtime Parity W2.B2 recovery prompt helper ──────────

#[test]
fn build_spawn_recovery_prompt_includes_task_and_error_text() {
    let prompt = build_spawn_recovery_prompt(
        "Generate a 5-slide deck on AI",
        "validator rejected child artifact: deck.pptx missing",
    );
    assert!(prompt.contains("[system-internal]"));
    assert!(prompt.contains("Generate a 5-slide deck on AI"));
    assert!(
        prompt.contains("validator rejected child artifact: deck.pptx missing"),
        "recovery prompt must surface the verbatim failure: {prompt}"
    );
    assert!(
        prompt.contains("different strategy") || prompt.contains("smaller scope"),
        "recovery prompt must direct the LLM toward an alternative"
    );
}

#[test]
fn build_spawn_recovery_prompt_handles_empty_task_desc() {
    let prompt = build_spawn_recovery_prompt("", "boom");
    assert!(prompt.contains("Original task: "));
    assert!(prompt.contains("Failure: boom"));
}

/// Provider that returns a hard `Err` on the first call and a
/// successful EndTurn on every subsequent call. Used to drive the
/// M8.9 recovery wrapper.
struct FailThenSucceedProvider {
    calls: std::sync::atomic::AtomicUsize,
}

#[async_trait]
impl LlmProvider for FailThenSucceedProvider {
    async fn chat(
        &self,
        _messages: &[octos_core::Message],
        _tools: &[octos_llm::ToolSpec],
        _config: &octos_llm::ChatConfig,
    ) -> Result<octos_llm::ChatResponse> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        if n == 0 {
            return Err(eyre::eyre!("simulated provider failure"));
        }
        Ok(octos_llm::ChatResponse {
            content: Some("recovered".into()),
            reasoning_content: None,
            tool_calls: vec![],
            stop_reason: octos_llm::StopReason::EndTurn,
            usage: octos_llm::TokenUsage::default(),
            provider_index: None,
        })
    }
    fn model_id(&self) -> &str {
        "mock"
    }
    fn provider_name(&self) -> &str {
        "mock"
    }
}

#[tokio::test]
async fn run_task_with_m8_9_recovery_retries_once_after_initial_failure() {
    let provider = Arc::new(FailThenSucceedProvider {
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let calls_ref = provider.calls.load(Ordering::SeqCst);
    assert_eq!(calls_ref, 0);

    let memory = Arc::new(create_test_store().await);
    let registry = ToolRegistry::with_builtins(PathBuf::from("/tmp"));
    let worker = Agent::new(
        AgentId::new("test-worker"),
        provider.clone(),
        registry,
        memory,
    );
    let subtask = Task::new(
        TaskKind::Code {
            instruction: "Recover me".into(),
            files: vec![],
        },
        TaskContext {
            working_dir: PathBuf::from("/tmp"),
            ..Default::default()
        },
    );

    let result = run_task_with_m8_9_recovery(&worker, &subtask, "Recover me").await;
    let task_result = result.expect("recovery succeeds");
    assert!(task_result.success, "recovery turn must succeed");
    assert!(
        provider.calls.load(Ordering::SeqCst) >= 2,
        "recovery must invoke the provider at least twice (one fail + one retry); got {}",
        provider.calls.load(Ordering::SeqCst)
    );
}

/// Provider whose every call hard-fails. Drives the
/// "recovery still fails -> bubble up" branch.
struct AlwaysFailProvider;

#[async_trait]
impl LlmProvider for AlwaysFailProvider {
    async fn chat(
        &self,
        _messages: &[octos_core::Message],
        _tools: &[octos_llm::ToolSpec],
        _config: &octos_llm::ChatConfig,
    ) -> Result<octos_llm::ChatResponse> {
        Err(eyre::eyre!("simulated permanent failure"))
    }
    fn model_id(&self) -> &str {
        "mock"
    }
    fn provider_name(&self) -> &str {
        "mock"
    }
}

#[tokio::test]
async fn run_task_with_m8_9_recovery_bubbles_up_when_recovery_also_fails() {
    let provider = Arc::new(AlwaysFailProvider);
    let memory = Arc::new(create_test_store().await);
    let registry = ToolRegistry::with_builtins(PathBuf::from("/tmp"));
    let worker = Agent::new(AgentId::new("test-worker"), provider, registry, memory);
    let subtask = Task::new(
        TaskKind::Code {
            instruction: "do".into(),
            files: vec![],
        },
        TaskContext {
            working_dir: PathBuf::from("/tmp"),
            ..Default::default()
        },
    );

    let result = run_task_with_m8_9_recovery(&worker, &subtask, "do").await;
    assert!(result.is_err(), "permanent failure must bubble up");
}

/// Once wired with parent caches the SpawnTool must surface the
/// same `Arc` instances back through its introspection helpers —
/// session_actor / tests rely on identity to assert the parent
/// cache reaches the spawned child.
#[tokio::test]
async fn spawn_tool_propagates_parent_caches_via_builders() {
    let (in_tx, _in_rx) = tokio::sync::mpsc::channel(16);
    let cache = Arc::new(crate::FileStateCache::new());
    let router = Arc::new(crate::SubAgentOutputRouter::new(std::env::temp_dir().join(
        format!(
                "octos-w2-router-{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0),
            ),
    )));
    let supervisor = TaskSupervisor::new();
    let summary_gen = Arc::new(crate::AgentSummaryGenerator::new(
        Arc::new(MockProvider),
        router.clone(),
        supervisor,
    ));

    let tool = SpawnTool::new(
        Arc::new(MockProvider),
        Arc::new(create_test_store().await),
        PathBuf::from("/tmp"),
        in_tx,
    )
    .with_parent_file_state_cache(cache.clone())
    .with_parent_subagent_output_router(router.clone())
    .with_parent_subagent_summary_generator(summary_gen.clone());

    // `Arc::ptr_eq` is the cheapest identity check that proves the
    // child observed the same instance the parent wired in — not a
    // freshly-built one.
    assert!(Arc::ptr_eq(
        tool.parent_file_state_cache().expect("cache wired"),
        &cache,
    ));
    assert!(Arc::ptr_eq(
        tool.parent_subagent_output_router().expect("router wired"),
        &router,
    ));
    assert!(Arc::ptr_eq(
        tool.parent_subagent_summary_generator()
            .expect("summary generator wired"),
        &summary_gen,
    ));
}

/// Guard C regression: a spawn invocation at depth 4 must refuse
/// before any backend dispatch, surfacing a structured tool failure
/// the LLM can react to. The depth gate fires before
/// argument parsing — even invalid JSON returns the depth-limit
/// error rather than the legacy "invalid spawn tool input" path.
#[tokio::test]
async fn spawn_refuses_at_depth_4() {
    let (in_tx, _in_rx) = tokio::sync::mpsc::channel(16);
    let tool = SpawnTool::new(
        Arc::new(MockProvider),
        Arc::new(create_test_store().await),
        PathBuf::from("/tmp"),
        in_tx,
    );

    // Build a ToolContext at the depth cap. The spawn tool reads
    // `ctx.spawn_depth` and refuses before parsing args.
    let mut ctx = super::super::ToolContext::zero();
    ctx.spawn_depth = MAX_SPAWN_DEPTH;

    let result = tool
        .execute_with_context(
            &ctx,
            &serde_json::json!({
                "task": "do something deeply nested"
            }),
        )
        .await;
    let tool_result = match result {
        Ok(r) => r,
        Err(error) => panic!("depth refusal should return Ok(failed) rather than Err: {error}"),
    };
    assert!(!tool_result.success, "spawn at the cap must report failure");
    assert!(
        tool_result
            .output
            .contains(&format!("spawn depth limit ({MAX_SPAWN_DEPTH}) exceeded")),
        "structured reason missing from output: {}",
        tool_result.output
    );
    assert!(
        tool_result.output.contains("refusing further nesting"),
        "structured reason missing from output: {}",
        tool_result.output
    );

    // Sanity: at depth 0 the tool keeps working (no early refusal).
    let mut ctx0 = super::super::ToolContext::zero();
    ctx0.spawn_depth = 0;
    // We pass an empty input so the legacy validation path runs. A
    // zero-depth spawn does NOT short-circuit with the depth-limit
    // refusal — it falls through into the regular pipeline (which
    // surfaces an unrelated error for the empty input).
    let baseline = tool
        .execute_with_context(&ctx0, &serde_json::json!({}))
        .await;
    match baseline {
        Ok(r) => {
            assert!(
                !r.output.contains("spawn depth limit"),
                "below-cap spawn must not emit the depth-limit refusal: {}",
                r.output
            );
        }
        Err(error) => {
            let err_msg = format!("{error}");
            assert!(
                !err_msg.contains("spawn depth limit"),
                "below-cap spawn must not emit the depth-limit refusal: {err_msg}"
            );
        }
    }
}

/// Guard C boundary: depth 3 (one less than the cap) is still
/// allowed; the gate fires on depth 4 only.
#[tokio::test]
async fn spawn_allows_depth_below_cap() {
    let (in_tx, _in_rx) = tokio::sync::mpsc::channel(16);
    let tool = SpawnTool::new(
        Arc::new(MockProvider),
        Arc::new(create_test_store().await),
        PathBuf::from("/tmp"),
        in_tx,
    );

    let mut ctx = super::super::ToolContext::zero();
    ctx.spawn_depth = MAX_SPAWN_DEPTH - 1;

    // An empty input still trips the legacy validation path; the
    // important invariant is that depth-3 does NOT short-circuit
    // with the structured "spawn depth limit" message.
    let result = tool
        .execute_with_context(&ctx, &serde_json::json!({}))
        .await;
    match result {
        Ok(tool_result) => {
            assert!(
                !tool_result.output.contains("spawn depth limit"),
                "depth below cap must not emit the depth-limit refusal: {}",
                tool_result.output
            );
        }
        Err(error) => {
            let err_msg = format!("{error}");
            assert!(
                !err_msg.contains("spawn depth limit"),
                "depth below cap must not emit the depth-limit refusal: {err_msg}"
            );
        }
    }
}

// -----------------------------------------------------------------------
// Phase 2-D: SessionScope propagation tests for SpawnTool.
//
// The migrated spawn tool reads `ctx.session_scope` and threads it
// onto the child Agent via `Agent::with_session_scope`. The child
// Agent's execution loop then plants the same scope onto every
// child `ToolContext` (see `agent/execution.rs`). These tests
// exercise that path end-to-end by mounting a recording tool on the
// child registry and asserting on what `execute_with_context` sees.
// -----------------------------------------------------------------------

/// Test-only tool that records the `session_scope.workspace()` it
/// observes on its `ToolContext`. Used by the Phase 2-D propagation
/// tests to capture what the child Agent's execution loop hands to
/// migrated tools. Lives only inside `#[cfg(test)]`.
struct ScopeRecordingTool {
    observed: Arc<std::sync::Mutex<Option<PathBuf>>>,
}

#[async_trait]
impl Tool for ScopeRecordingTool {
    fn name(&self) -> &str {
        "scope_probe"
    }

    fn description(&self) -> &str {
        "test-only tool that records the session_scope it observes"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object", "properties": {} })
    }

    async fn execute(&self, args: &serde_json::Value) -> Result<ToolResult> {
        self.execute_with_context(&super::super::ToolContext::zero(), args)
            .await
    }

    async fn execute_with_context(
        &self,
        ctx: &super::super::ToolContext,
        _args: &serde_json::Value,
    ) -> Result<ToolResult> {
        let observed = ctx
            .session_scope
            .as_ref()
            .map(|scope| scope.workspace().to_path_buf());
        *self.observed.lock().unwrap_or_else(|e| e.into_inner()) = observed;
        Ok(ToolResult {
            output: "ok".to_string(),
            success: true,
            ..Default::default()
        })
    }
}

/// Mock provider that calls `scope_probe` once and then ends — drives
/// the child Agent through exactly one tool execution so the
/// recording tool sees the migrated `ToolContext`.
struct ScopeProbeProvider;

#[async_trait]
impl LlmProvider for ScopeProbeProvider {
    async fn chat(
        &self,
        messages: &[octos_core::Message],
        _tools: &[octos_llm::ToolSpec],
        _config: &octos_llm::ChatConfig,
    ) -> Result<octos_llm::ChatResponse> {
        // First call → invoke scope_probe; second call (after the
        // probe's tool_result lands) → end the turn.
        let probe_already_run = messages
            .iter()
            .any(|msg| matches!(msg.role, octos_core::MessageRole::Tool));
        if probe_already_run {
            Ok(octos_llm::ChatResponse {
                content: Some("done".into()),
                reasoning_content: None,
                tool_calls: vec![],
                stop_reason: octos_llm::StopReason::EndTurn,
                usage: octos_llm::TokenUsage::default(),
                provider_index: None,
            })
        } else {
            Ok(octos_llm::ChatResponse {
                content: None,
                reasoning_content: None,
                tool_calls: vec![octos_core::ToolCall {
                    id: "call_scope_probe".into(),
                    name: "scope_probe".into(),
                    arguments: serde_json::json!({}),
                    metadata: None,
                }],
                stop_reason: octos_llm::StopReason::ToolUse,
                usage: octos_llm::TokenUsage::default(),
                provider_index: None,
            })
        }
    }

    fn model_id(&self) -> &str {
        "mock"
    }

    fn provider_name(&self) -> &str {
        "mock"
    }
}

struct EditSameFileProvider;

#[async_trait]
impl LlmProvider for EditSameFileProvider {
    async fn chat(
        &self,
        messages: &[octos_core::Message],
        _tools: &[octos_llm::ToolSpec],
        _config: &octos_llm::ChatConfig,
    ) -> Result<octos_llm::ChatResponse> {
        let edit_already_run = messages
            .iter()
            .any(|msg| matches!(msg.role, octos_core::MessageRole::Tool));
        if edit_already_run {
            return Ok(octos_llm::ChatResponse {
                content: Some("done".into()),
                reasoning_content: None,
                tool_calls: vec![],
                stop_reason: octos_llm::StopReason::EndTurn,
                usage: octos_llm::TokenUsage::default(),
                provider_index: None,
            });
        }

        Ok(octos_llm::ChatResponse {
            content: None,
            reasoning_content: None,
            tool_calls: vec![octos_core::ToolCall {
                id: "call_edit_file".into(),
                name: "edit_file".into(),
                arguments: serde_json::json!({
                    "path": "shared.txt",
                    "old_string": "base\n",
                    "new_string": "worker content\n"
                }),
                metadata: None,
            }],
            stop_reason: octos_llm::StopReason::ToolUse,
            usage: octos_llm::TokenUsage::default(),
            provider_index: None,
        })
    }

    fn model_id(&self) -> &str {
        "mock"
    }

    fn provider_name(&self) -> &str {
        "mock"
    }
}

fn run_git(repo: &std::path::Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .status()
        .expect("git command starts");
    assert!(status.success(), "git {args:?} failed with {status}");
}

/// `git init` + one commit at `repo`, so worktree tests have a HEAD to
/// branch worker worktrees from.
#[test]
fn is_inside_git_work_tree_detects_repo_vs_plain_dir() {
    let git_dir = tempfile::tempdir().unwrap();
    init_worktree_test_repo(git_dir.path());
    assert!(
        is_inside_git_work_tree(git_dir.path()),
        "an initialized repo must be detected as a work tree"
    );

    let plain = tempfile::tempdir().unwrap();
    assert!(
        !is_inside_git_work_tree(plain.path()),
        "a non-git directory must not be detected as a work tree"
    );
}

#[test]
fn allocate_worker_worktree_gives_actionable_error_outside_a_git_repo() {
    // The live gap: a non-git workspace fell into `git rev-parse
    // --show-toplevel` and leaked `fatal: not a git repository`. Now it
    // returns an actionable message naming the remedy.
    let plain = tempfile::tempdir().unwrap();
    let worker = AgentId::new("subagent-0");
    let msg = match allocate_worker_worktree(plain.path(), &worker, None) {
        Ok(_) => panic!("worktree isolation must be refused outside a git repo"),
        Err(err) => err.to_string(),
    };
    assert!(
        msg.contains("git repository"),
        "error must explain the git-repo requirement: {msg}"
    );
    assert!(
        msg.contains("isolation: shared"),
        "error must name the actionable remedy: {msg}"
    );
    assert!(
        !msg.contains("show-toplevel"),
        "the raw git plumbing error must not leak: {msg}"
    );
}

fn init_worktree_test_repo(repo: &std::path::Path) {
    std::fs::create_dir_all(repo).unwrap();
    run_git(repo, &["init"]);
    std::fs::write(repo.join("shared.txt"), "base\n").unwrap();
    run_git(repo, &["add", "shared.txt"]);
    run_git(
        repo,
        &[
            "-c",
            "user.name=Octos Test",
            "-c",
            "user.email=octos@example.invalid",
            "commit",
            "-m",
            "base",
        ],
    );
}

fn git_worktree_list(repo: &std::path::Path) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["worktree", "list", "--porcelain"])
        .output()
        .expect("git worktree list runs");
    assert!(output.status.success(), "git worktree list failed");
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// The `.octos/work` root may exist (it is created before the last
/// refusal points), but a refused spawn must not leave any worker
/// worktree directory inside it.
fn assert_no_worker_worktrees(repo: &std::path::Path) {
    let work_root = repo.join(".octos/work");
    if !work_root.exists() {
        return;
    }
    let leftovers: Vec<PathBuf> = std::fs::read_dir(&work_root)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert!(
        leftovers.is_empty(),
        "refused spawn left worker worktrees behind: {leftovers:?}"
    );
}

/// PR #1250 finding 1: a sync spawn refused AFTER worktree allocation
/// (here: the subagent tool-availability preflight `?` return) must
/// prune the just-created worktree + branch. Leaking it as a live
/// registered worktree is permanent — the `octos clean` sweep only
/// removes directories absent from `git worktree list`.
#[tokio::test]
async fn worktree_isolation_prunes_allocation_when_sync_preflight_refuses() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("repo");
    init_worktree_test_repo(&repo);
    let baseline = git_worktree_list(&repo);

    let (in_tx, _in_rx) = tokio::sync::mpsc::channel(16);
    let tool = SpawnTool::new(
        Arc::new(MockProvider),
        Arc::new(create_test_store().await),
        repo.clone(),
        in_tx,
    );
    let scope = octos_core::SessionScope::solo(repo.clone(), vec![]).expect("scope construction");
    let mut ctx = super::super::ToolContext::zero();
    ctx.session_scope = Some(Arc::new(scope));

    let result = tool
        .execute_with_context(
            &ctx,
            &serde_json::json!({
                "task": "edit shared.txt",
                "mode": "sync",
                "isolation": "worktree",
                "allowed_tools": ["definitely_not_a_real_tool"]
            }),
        )
        .await;
    assert!(
        result.is_err(),
        "an unavailable allowed tool must refuse the sync spawn"
    );

    assert_eq!(
        git_worktree_list(&repo),
        baseline,
        "refused spawn must leave `git worktree list` unchanged"
    );
    assert_no_worker_worktrees(&repo);
    assert!(
        !git_ref_exists(&repo, "refs/heads/octos/worker/subagent-0").unwrap(),
        "refused spawn must not leave its worker branch behind"
    );
}

/// PR #1250 finding 1: the background fanout-cap refusal happens after
/// worktree allocation; the refused spawn must prune the worktree +
/// branch instead of leaking a live registered worktree.
#[tokio::test]
async fn worktree_isolation_prunes_allocation_when_fanout_cap_refuses() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("repo");
    init_worktree_test_repo(&repo);
    let baseline = git_worktree_list(&repo);

    let (in_tx, _in_rx) = tokio::sync::mpsc::channel(16);
    let supervisor = Arc::new(TaskSupervisor::new());
    // Unique per-test session key: shared keys are drained cross-test.
    let session_key = "api:worktree-cap-session";
    for i in 0..crate::task_supervisor::MAX_CHILDREN_PER_PARENT {
        let id = supervisor.register("busy", &format!("call-{i}"), Some(session_key));
        assert!(!id.is_empty(), "saturation register #{i} must succeed");
    }
    let tool = SpawnTool::new(
        Arc::new(MockProvider),
        Arc::new(create_test_store().await),
        repo.clone(),
        in_tx,
    )
    .with_task_supervisor(supervisor.clone(), session_key, repo.join("tasks.jsonl"));

    let result = tool
        .execute(&serde_json::json!({
            "task": "Write a short answer",
            "label": "over-cap worktree spawn",
            "mode": "background",
            "isolation": "worktree"
        }))
        .await
        .unwrap();

    assert!(!result.success, "cap-refused spawn must fail");
    assert!(
        result.output.contains("[TASK LIMIT]"),
        "refusal must surface the cap; got: {}",
        result.output
    );
    assert_eq!(
        git_worktree_list(&repo),
        baseline,
        "refused spawn must leave `git worktree list` unchanged"
    );
    assert_no_worker_worktrees(&repo);
    assert!(
        !git_ref_exists(&repo, "refs/heads/octos/worker/subagent-0").unwrap(),
        "refused spawn must not leave its worker branch behind"
    );
}

/// PR #1250 finding 2: a symlinked `.octos/work` must be rejected
/// BEFORE anything is created — even when the context carries no
/// session scope at all (the scope-based validation alone would never
/// run here).
#[cfg(unix)]
#[tokio::test]
async fn worktree_isolation_refuses_symlinked_work_root_before_creating() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("repo");
    init_worktree_test_repo(&repo);
    let outside = temp.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::create_dir_all(repo.join(".octos")).unwrap();
    // repo/.octos/work -> outside the repository root.
    std::os::unix::fs::symlink(&outside, repo.join(".octos/work")).unwrap();
    let baseline = git_worktree_list(&repo);

    let (in_tx, _in_rx) = tokio::sync::mpsc::channel(16);
    let tool = SpawnTool::new(
        Arc::new(MockProvider),
        Arc::new(create_test_store().await),
        repo.clone(),
        in_tx,
    );

    // Deliberately NO ctx.session_scope: the repo-root containment
    // check must refuse on its own.
    let result = tool
        .execute(&serde_json::json!({
            "task": "edit shared.txt",
            "mode": "sync",
            "isolation": "worktree"
        }))
        .await
        .unwrap();

    assert!(!result.success, "symlinked work root must refuse the spawn");
    assert!(
        result.output.contains("symlink"),
        "refusal must name the symlink escape; got: {}",
        result.output
    );
    assert_eq!(
        std::fs::read_dir(&outside).unwrap().count(),
        0,
        "nothing may be created outside the repository root"
    );
    assert_eq!(git_worktree_list(&repo), baseline);
}

/// PR #1250 finding 2: session-scope validation must run BEFORE
/// `git worktree add`. With a scope rooted at a repo SUBDIR, the
/// planned worktree (`<repo-root>/.octos/work/...`) falls outside the
/// session root: the spawn must be refused with nothing created.
#[tokio::test]
async fn worktree_isolation_refuses_scope_escape_before_creating() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("repo");
    init_worktree_test_repo(&repo);
    let session_root = repo.join("session-root");
    std::fs::create_dir_all(&session_root).unwrap();
    let baseline = git_worktree_list(&repo);

    let (in_tx, _in_rx) = tokio::sync::mpsc::channel(16);
    let tool = SpawnTool::new(
        Arc::new(MockProvider),
        Arc::new(create_test_store().await),
        session_root.clone(),
        in_tx,
    );
    let scope =
        octos_core::SessionScope::solo(session_root.clone(), vec![]).expect("scope construction");
    let mut ctx = super::super::ToolContext::zero();
    ctx.session_scope = Some(Arc::new(scope));

    let result = tool
        .execute_with_context(
            &ctx,
            &serde_json::json!({
                "task": "edit shared.txt",
                "mode": "sync",
                "isolation": "worktree"
            }),
        )
        .await
        .unwrap();

    assert!(
        !result.success,
        "a worktree outside the session root must refuse the spawn"
    );
    assert!(
        result.output.contains("rejected by session scope"),
        "refusal must name the scope rejection; got: {}",
        result.output
    );
    assert_eq!(
        git_worktree_list(&repo),
        baseline,
        "scope-refused spawn must not create a worktree"
    );
    assert_no_worker_worktrees(&repo);
    assert!(
        !repo.join(".octos/work").exists(),
        "scope-refused spawn must not create the work root either"
    );
}

#[test]
fn worker_worktree_slug_validation_rejects_traversal() {
    for valid in ["subagent-0", "abc.DEF_123", "parent/child-1"] {
        validate_worker_worktree_slug(valid).expect("valid slug accepted");
    }

    for invalid in [
        "",
        ".",
        "..",
        "../escape",
        "escape/..",
        "/absolute",
        "\\absolute",
        "bad\\slash",
        "bad space",
    ] {
        assert!(
            validate_worker_worktree_slug(invalid).is_err(),
            "invalid slug {invalid:?} was accepted"
        );
    }
}

#[tokio::test]
async fn worktree_isolation_runs_concurrent_writers_on_separate_branches() {
    let repo = tempfile::tempdir().unwrap();
    run_git(repo.path(), &["init"]);
    std::fs::write(repo.path().join("shared.txt"), "base\n").unwrap();
    run_git(repo.path(), &["add", "shared.txt"]);
    run_git(
        repo.path(),
        &[
            "-c",
            "user.name=Octos Test",
            "-c",
            "user.email=octos@example.invalid",
            "commit",
            "-m",
            "base",
        ],
    );

    let scope = octos_core::SessionScope::solo(repo.path().to_path_buf(), vec![])
        .expect("scope construction");
    let (in_tx, _in_rx) = tokio::sync::mpsc::channel(16);
    let tool = SpawnTool::new(
        Arc::new(EditSameFileProvider),
        Arc::new(create_test_store().await),
        repo.path().to_path_buf(),
        in_tx,
    );
    let mut ctx = super::super::ToolContext::zero();
    ctx.session_scope = Some(Arc::new(scope));

    let args = serde_json::json!({
        "task": "edit shared.txt",
        "mode": "sync",
        "isolation": "worktree",
        "allowed_tools": ["edit_file"]
    });
    let (first, second) = tokio::join!(
        tool.execute_with_context(&ctx, &args),
        tool.execute_with_context(&ctx, &args)
    );
    let first = first.expect("first spawn returns");
    let second = second.expect("second spawn returns");
    assert!(first.success, "first spawn failed: {}", first.output);
    assert!(second.success, "second spawn failed: {}", second.output);

    assert_eq!(
        std::fs::read_to_string(repo.path().join("shared.txt")).unwrap(),
        "base\n",
        "parent workspace must not be mutated by isolated workers"
    );
    for slug in ["subagent-0", "subagent-1"] {
        let worker_root = repo.path().join(".octos/work").join(slug);
        assert_eq!(
            std::fs::read_to_string(worker_root.join("shared.txt")).unwrap(),
            "worker content\n"
        );
        let status =
            std::fs::read_to_string(worker_root.join(".octos/worker-worktree.json")).unwrap();
        assert!(status.contains("\"status\": \"completed\""));
    }
}

#[tokio::test]
async fn spawn_propagates_scope_to_sub_agent() {
    // When the parent `ToolContext` carries a `SessionScope`, the
    // sync-mode sub-agent's tools must see the same scope on their
    // own `ToolContext`. Without this, a session's filesystem
    // contract is forgotten the moment work is delegated to a
    // sub-agent.
    let scope_dir = tempfile::tempdir().unwrap();
    let scope = octos_core::SessionScope::solo(scope_dir.path().to_path_buf(), vec![])
        .expect("scope construction");

    let observed = Arc::new(std::sync::Mutex::new(None::<PathBuf>));
    let observed_for_factory = observed.clone();

    let (in_tx, _in_rx) = tokio::sync::mpsc::channel(16);
    let tool = SpawnTool::new(
        Arc::new(ScopeProbeProvider),
        Arc::new(create_test_store().await),
        scope_dir.path().to_path_buf(),
        in_tx,
    )
    .with_child_tool_factory(Arc::new(move || {
        Arc::new(ScopeRecordingTool {
            observed: observed_for_factory.clone(),
        })
    }));

    let mut ctx = super::super::ToolContext::zero();
    ctx.session_scope = Some(Arc::new(scope));

    let result = tool
        .execute_with_context(
            &ctx,
            &serde_json::json!({
                "task": "probe the scope",
                "mode": "sync",
                "allowed_tools": ["scope_probe"]
            }),
        )
        .await
        .expect("spawn returns Ok");
    assert!(
        result.success,
        "expected sync spawn success: {}",
        result.output
    );

    let captured = observed.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let captured = captured.expect(
        "scope_probe must observe a SessionScope on its ToolContext — \
             without Phase 2-D propagation the child Agent runs scope-less",
    );
    let canonical_expected =
        std::fs::canonicalize(scope_dir.path()).expect("canonicalise scope dir");
    let canonical_observed =
        std::fs::canonicalize(&captured).expect("canonicalise observed workspace");
    assert_eq!(
        canonical_observed, canonical_expected,
        "child Agent's session_scope.workspace() must match the parent's"
    );
}

#[tokio::test]
async fn spawn_sub_agent_inherits_workspace_cwd() {
    // The sync-mode child Agent's `working_dir` (passed via
    // `TaskContext`) and the scope's workspace agree when the
    // SpawnTool was constructed with `working_dir == scope.workspace()`.
    // This is the production wiring (`runtime/session.rs` builds
    // `SpawnTool::new(... working_dir == scope.workspace())`), and
    // it's the property the Phase 2-D contract relies on: the child
    // workspace CWD == the parent's scoped workspace, so any shell
    // tool the child invokes runs with the right CWD even without
    // the scope plumb.
    let scope_dir = tempfile::tempdir().unwrap();
    let scope = octos_core::SessionScope::solo(scope_dir.path().to_path_buf(), vec![])
        .expect("scope construction");

    let observed = Arc::new(std::sync::Mutex::new(None::<PathBuf>));
    let observed_for_factory = observed.clone();

    let (in_tx, _in_rx) = tokio::sync::mpsc::channel(16);
    // SpawnTool's working_dir matches scope.workspace() — this is
    // the production case.
    let tool = SpawnTool::new(
        Arc::new(ScopeProbeProvider),
        Arc::new(create_test_store().await),
        scope_dir.path().to_path_buf(),
        in_tx,
    )
    .with_child_tool_factory(Arc::new(move || {
        Arc::new(ScopeRecordingTool {
            observed: observed_for_factory.clone(),
        })
    }));

    let mut ctx = super::super::ToolContext::zero();
    ctx.session_scope = Some(Arc::new(scope));

    let result = tool
        .execute_with_context(
            &ctx,
            &serde_json::json!({
                "task": "probe",
                "mode": "sync",
                "allowed_tools": ["scope_probe"]
            }),
        )
        .await
        .expect("spawn ok");
    assert!(result.success);

    let captured = observed
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .expect("scope_probe ran");
    let canonical_workspace = std::fs::canonicalize(&captured).expect("canonicalise");
    let canonical_spawn_cwd =
        std::fs::canonicalize(scope_dir.path()).expect("canonicalise spawn cwd");
    assert_eq!(
        canonical_workspace, canonical_spawn_cwd,
        "child workspace must equal the spawn cwd when SpawnTool::new \
             was given `scope.workspace()` as its `working_dir`"
    );
}

#[tokio::test]
async fn spawn_falls_back_to_legacy_cwd_when_no_scope() {
    // No scope on the parent context — the child Agent must keep
    // its pre-Phase-2D behaviour byte-for-byte: `session_scope ==
    // None`. The recording tool sees no scope. The legacy
    // `working_dir` continues to drive every other code path
    // (`ToolRegistry::with_builtins`, `TaskContext`).
    let working = tempfile::tempdir().unwrap();
    let observed = Arc::new(std::sync::Mutex::new(None::<PathBuf>));
    let observed_for_factory = observed.clone();

    let (in_tx, _in_rx) = tokio::sync::mpsc::channel(16);
    let tool = SpawnTool::new(
        Arc::new(ScopeProbeProvider),
        Arc::new(create_test_store().await),
        working.path().to_path_buf(),
        in_tx,
    )
    .with_child_tool_factory(Arc::new(move || {
        Arc::new(ScopeRecordingTool {
            observed: observed_for_factory.clone(),
        })
    }));

    let ctx = super::super::ToolContext::zero();
    assert!(
        ctx.session_scope.is_none(),
        "precondition: parent ctx has no scope"
    );

    let result = tool
        .execute_with_context(
            &ctx,
            &serde_json::json!({
                "task": "probe",
                "mode": "sync",
                "allowed_tools": ["scope_probe"]
            }),
        )
        .await
        .expect("spawn ok");
    assert!(result.success);

    let captured = observed.lock().unwrap_or_else(|e| e.into_inner()).clone();
    assert!(
        captured.is_none(),
        "without a parent scope the child Agent must NOT synthesise one; observed={:?}",
        captured
    );
}
