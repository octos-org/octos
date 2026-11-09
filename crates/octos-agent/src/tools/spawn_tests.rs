use super::*;

/// Runaway guard for the "poll until the background task reaches state X"
/// loops below. It is NOT a latency assertion: on a passing run the loop
/// returns as soon as the state arrives, so a generous ceiling costs nothing,
/// and on a genuinely broken run the test still fails — just later.
///
/// It used to be 5s, which sits under the noise floor of a loaded CI runner:
/// `test_background_spawn_fails_when_contract_owned_workflow_is_not_ready`
/// failed with "did not fail in time" on ubuntu-latest in run 30387949499
/// while the only change under test was one line of `.github/workflows/ci.yml`.
/// These loops spawn real subprocesses, so their wall-clock cost tracks host
/// load rather than anything the test controls.
///
/// #2053: the Windows runners miss fixed-duration waits that pass everywhere
/// else — `test_background_spawn_persists_workflow_phase_transitions` failed
/// this ceiling on `check-windows` while an in-job retry of the SAME job
/// produced a different failure set, and a plain re-run went green. These
/// loops break on content the instant it appears, so a larger Windows ceiling
/// costs a passing run nothing.
#[cfg(not(windows))]
const BACKGROUND_DEADLINE: std::time::Duration = std::time::Duration::from_secs(60);
#[cfg(windows)]
const BACKGROUND_DEADLINE: std::time::Duration = std::time::Duration::from_secs(240);

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
            started.elapsed() < BACKGROUND_DEADLINE,
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

// Minimal mock provider for testing
struct MockProvider;

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

async fn create_test_store() -> EpisodeStore {
    let dir = tempfile::tempdir().unwrap();
    // Leak the dir so it stays alive for the test
    let dir = Box::leak(Box::new(dir));
    EpisodeStore::open(dir.path()).await.unwrap()
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

/// PR #1250 finding 2: a symlinked `.octos/work` must be rejected
/// BEFORE anything is created — even when the context carries no
/// session scope at all (the scope-based validation alone would never
/// run here).
#[cfg(unix)]
struct NativeFeedbackProvider {
    prompts: std::sync::Mutex<Vec<Vec<octos_core::Message>>>,
    entered: tokio::sync::Notify,
    release: tokio::sync::Semaphore,
}

impl Default for NativeFeedbackProvider {
    fn default() -> Self {
        Self {
            prompts: Default::default(),
            entered: Default::default(),
            release: tokio::sync::Semaphore::new(0),
        }
    }
}

#[async_trait]
impl LlmProvider for NativeFeedbackProvider {
    async fn chat(
        &self,
        messages: &[octos_core::Message],
        tools: &[octos_llm::ToolSpec],
        config: &octos_llm::ChatConfig,
    ) -> Result<octos_llm::ChatResponse> {
        let call = {
            let mut prompts = self.prompts.lock().unwrap();
            prompts.push(messages.to_vec());
            prompts.len()
        };
        self.entered.notify_one();
        self.release.acquire().await.unwrap().forget();
        let mut response = MockProvider.chat(messages, tools, config).await?;
        response.content = Some(format!("child answer {call}"));
        Ok(response)
    }
    fn model_id(&self) -> &str {
        "native-feedback-test"
    }
    fn provider_name(&self) -> &str {
        "mock"
    }
}

async fn native_feedback_fixture() -> (
    tempfile::TempDir,
    Arc<NativeFeedbackProvider>,
    ToolRegistry,
    super::super::ToolContext,
    String,
) {
    native_feedback_fixture_with_sender(Arc::new(|_| Box::pin(async { true }))).await
}

async fn native_feedback_fixture_with_sender(
    sender: BackgroundResultSender,
) -> (
    tempfile::TempDir,
    Arc<NativeFeedbackProvider>,
    ToolRegistry,
    super::super::ToolContext,
    String,
) {
    let temp = tempfile::tempdir().unwrap();
    let provider = Arc::new(NativeFeedbackProvider::default());
    let mut registry = ToolRegistry::with_builtins(temp.path());
    let supervisor = registry.supervisor();
    supervisor
        .enable_persistence(temp.path().join("tasks.jsonl"))
        .unwrap();
    let (tx, _rx) = tokio::sync::mpsc::channel(16);
    let tool = SpawnTool::new(
        provider.clone(),
        Arc::new(create_test_store().await),
        temp.path().into(),
        tx,
    )
    .with_task_supervisor(
        supervisor.clone(),
        "api:native-feedback",
        temp.path().join("tasks.jsonl"),
    )
    .with_background_result_sender(sender);
    registry.register(tool);
    let ctx = super::super::ToolContext {
        task_supervisor: Some(supervisor.clone()),
        parent_session_key: Some("api:native-feedback".into()),
        session_scope: Some(Arc::new(
            SessionScope::solo(temp.path().into(), Vec::new()).unwrap(),
        )),
        ..super::super::ToolContext::zero()
    };
    let started = registry.execute_with_context(&ctx, "spawn_agent", &serde_json::json!({
        "task": "original child assignment", "allowed_tools": ["read_file"], "owned_paths": ["out/item"]
    })).await.unwrap();
    assert!(started.success, "{}", started.output);
    let id = supervisor.get_all_tasks()[0].id.clone();
    tokio::time::timeout(BACKGROUND_DEADLINE, provider.entered.notified())
        .await
        .unwrap();
    (temp, provider, registry, ctx, id)
}

async fn native_feedback_finished(id: &str) {
    tokio::time::timeout(BACKGROUND_DEADLINE, async {
        while crate::task_supervisor::task_is_live(id) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn should_deliver_native_feedback_to_running_child_prompt() {
    let (_temp, provider, registry, ctx, id) = native_feedback_fixture().await;
    let sent = registry
        .execute_with_context(
            &ctx,
            "send_input",
            &serde_json::json!({
                "agent_id": id, "message": "reviewer: repair the missing check"
            }),
        )
        .await
        .unwrap();
    assert!(sent.success, "{}", sent.output);
    provider.release.add_permits(2);
    native_feedback_finished(&id).await;
    let prompts = provider.prompts.lock().unwrap();
    assert_eq!(prompts.len(), 2);
    assert!(
        prompts[1]
            .iter()
            .any(|m| m.role == octos_core::MessageRole::User
                && m.content == "reviewer: repair the missing check")
    );
    assert_eq!(
        ctx.task_supervisor.as_ref().unwrap().get_all_tasks().len(),
        1
    );
}

#[tokio::test]
async fn should_reject_cross_session_and_cancelled_native_feedback() {
    let (_temp, provider, registry, ctx, id) = native_feedback_fixture().await;
    let foreign = super::super::ToolContext {
        parent_session_key: Some("api:foreign".into()),
        ..ctx.clone()
    };
    let rejected = registry
        .execute_with_context(
            &foreign,
            "send_input",
            &serde_json::json!({
                "agent_id": id, "message": "foreign input"
            }),
        )
        .await
        .unwrap();
    assert!(!rejected.success, "{}", rejected.output);
    ctx.task_supervisor.as_ref().unwrap().cancel(&id).unwrap();
    provider.release.add_permits(1);
    native_feedback_finished(&id).await;
    let rejected = registry
        .execute_with_context(&ctx, "resume_agent", &serde_json::json!({ "agent_id": id }))
        .await
        .unwrap();
    assert!(!rejected.success, "{}", rejected.output);
    assert_eq!(provider.prompts.lock().unwrap().len(), 1);
    assert_eq!(
        ctx.task_supervisor.as_ref().unwrap().get_all_tasks().len(),
        1
    );
}

#[tokio::test]
async fn should_continue_feedback_queued_during_native_terminal_delivery() {
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let sender: BackgroundResultSender = {
        let entered = entered.clone();
        let release = release.clone();
        Arc::new(move |_| {
            let entered = entered.clone();
            let release = release.clone();
            Box::pin(async move {
                entered.notify_one();
                release.notified().await;
                true
            })
        })
    };
    let (_temp, provider, registry, ctx, id) = native_feedback_fixture_with_sender(sender).await;
    provider.release.add_permits(1);
    tokio::time::timeout(BACKGROUND_DEADLINE, entered.notified())
        .await
        .unwrap();
    let old = ctx.task_supervisor.as_ref().unwrap().get_task(&id).unwrap();
    assert!(old.status.is_terminal());
    assert!(old.final_output.unwrap().contains("child answer 1"));
    let sent = registry
        .execute_with_context(
            &ctx,
            "send_input",
            &serde_json::json!({
                "agent_id": id, "message": "late review repair"
            }),
        )
        .await
        .unwrap();
    assert!(sent.success, "{}", sent.output);
    assert_eq!(
        ctx.task_supervisor.as_ref().unwrap().get_all_tasks().len(),
        1
    );
    let queued = registry
        .execute_with_context(
            &ctx,
            "wait_agent",
            &serde_json::json!({"agent_id":id,"timeout_ms":0,"include_output":true}),
        )
        .await
        .unwrap();
    let queued: serde_json::Value = serde_json::from_str(&queued.output).unwrap();
    assert_eq!(queued["agents"][0]["terminal"], false);
    assert!(queued["agents"][0].get("output").is_none());
    release.notify_one();
    tokio::time::timeout(BACKGROUND_DEADLINE, provider.entered.notified())
        .await
        .unwrap();
    let tasks = ctx.task_supervisor.as_ref().unwrap().get_all_tasks();
    assert_eq!(tasks.len(), 2);
    let next = tasks.iter().find(|t| t.id != id).unwrap().id.clone();
    assert_eq!(
        tasks
            .iter()
            .find(|t| t.id == next)
            .unwrap()
            .relaunched_from
            .as_deref(),
        Some(id.as_str())
    );
    provider.release.add_permits(1);
    tokio::time::timeout(BACKGROUND_DEADLINE, entered.notified())
        .await
        .unwrap();
    release.notify_one();
    native_feedback_finished(&next).await;
    let waited = registry
        .execute_with_context(
            &ctx,
            "wait_agent",
            &serde_json::json!({"agent_id":id,"timeout_ms":0,"include_output":true}),
        )
        .await
        .unwrap();
    let waited: serde_json::Value = serde_json::from_str(&waited.output).unwrap();
    assert_eq!(waited["agents"][0]["agent_id"], next);
    assert!(
        waited["agents"][0]["output"]
            .as_str()
            .unwrap()
            .contains("child answer 2")
    );
    let prompts = provider.prompts.lock().unwrap();
    assert!(
        prompts
            .last()
            .unwrap()
            .iter()
            .any(|m| m.content == "late review repair")
    );
}

struct NestedNativeFeedbackProvider;
#[async_trait]
impl LlmProvider for NestedNativeFeedbackProvider {
    async fn chat(
        &self,
        messages: &[octos_core::Message],
        tools: &[octos_llm::ToolSpec],
        config: &octos_llm::ChatConfig,
    ) -> Result<octos_llm::ChatResponse> {
        if messages.iter().any(|m| {
            m.role == octos_core::MessageRole::User && m.content.contains("grandchild assignment")
        }) {
            return MockProvider.chat(messages, tools, config).await;
        }
        let results: Vec<_> = messages
            .iter()
            .filter(|m| m.role == octos_core::MessageRole::Tool)
            .collect();
        if results.len() >= 2 {
            assert!(
                results.last().unwrap().content.contains("native_executor"),
                "nested feedback failed: {}",
                results.last().unwrap().content
            );
            return MockProvider.chat(messages, tools, config).await;
        }
        let (name, arguments) = if let Some(result) = results.first() {
            let pattern =
                Regex::new(r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}")
                    .unwrap();
            let id = pattern
                .find(&result.content)
                .expect("spawn returned a native task ID")
                .as_str();
            (
                "send_input",
                serde_json::json!({"agent_id":id,"message":"nested reviewer feedback"}),
            )
        } else {
            (
                "spawn_agent",
                serde_json::json!({"task":"grandchild assignment", "allowed_tools":["read_file"]}),
            )
        };
        Ok(octos_llm::ChatResponse {
            content: None,
            reasoning_content: None,
            tool_calls: vec![octos_core::ToolCall {
                id: format!("nested-{}", results.len()),
                name: name.into(),
                arguments,
                metadata: None,
            }],
            stop_reason: octos_llm::StopReason::ToolUse,
            usage: Default::default(),
            provider_index: None,
        })
    }
    fn model_id(&self) -> &str {
        "nested-feedback"
    }
    fn provider_name(&self) -> &str {
        "mock"
    }
}

#[tokio::test]
async fn should_allow_nested_native_worker_to_send_feedback_to_its_child() {
    let temp = tempfile::tempdir().unwrap();
    let supervisor = Arc::new(TaskSupervisor::new());
    let (tx, _rx) = tokio::sync::mpsc::channel(16);
    let tool = SpawnTool::new(
        Arc::new(NestedNativeFeedbackProvider),
        Arc::new(create_test_store().await),
        temp.path().into(),
        tx,
    )
    .with_agent_config(AgentConfig {
        max_iterations: 5,
        ..Default::default()
    })
    .with_task_supervisor(
        supervisor.clone(),
        "api:nested-feedback",
        temp.path().join("tasks.jsonl"),
    )
    .with_background_result_sender(Arc::new(|_| Box::pin(async { true })));
    let started = tool.execute(&serde_json::json!({"task":"drive nested worker feedback", "allowed_tools":["spawn_agent","send_input"]})).await.unwrap();
    assert!(started.success, "{}", started.output);
    let id = supervisor.get_all_tasks()[0].id.clone();
    native_feedback_finished(&id).await;
    let task = supervisor.get_task(&id).unwrap();
    assert_eq!(
        task.status,
        crate::TaskStatus::Completed,
        "{:?}",
        task.error
    );
}
