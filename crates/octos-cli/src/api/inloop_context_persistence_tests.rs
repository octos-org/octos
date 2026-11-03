use super::*;
use crate::context_manager::{
    ContextLedgerLoadStatus, context_ledger_path, load_or_rebuild_context_manager,
};

fn request(phase: PromptContextPhase, iteration: u32) -> PromptContextRequest {
    PromptContextRequest {
        phase,
        iteration,
        provider_name: "fixture".into(),
        model_id: "model".into(),
        context_window: 200_000,
    }
}

fn normalized_prompt(manager: &ContextManager) -> Value {
    json!(
        manager
            .for_prompt(&PromptBuildPolicy::default())
            .messages
            .iter()
            .map(|row| {
                json!({"role": row.role, "content": row.content, "tool_calls": row.tool_calls,
            "tool_call_id": row.tool_call_id, "media": row.media})
            })
            .collect::<Vec<_>>()
    )
}

#[test]
fn should_append_inloop_context_without_rewriting_baseline_and_resume_committed_tail() {
    let dir = tempfile::tempdir().unwrap();
    let session = SessionKey::new("api", "inloop-incremental");
    let mut history = vec![Message::user("Inspect the deck")];
    let initial = ContextManager::from_session_history(session.to_string(), None, &history);
    let manager = Arc::new(StdMutex::new(initial));
    let bridge = AppUiPromptContextBridge::new(
        session.clone(),
        dir.path().to_owned(),
        manager.clone(),
        false,
    );
    let mut prompt = vec![Message::system("stable runtime")];
    prompt.extend(history.clone());
    bridge
        .prepare_prompt(request(PromptContextPhase::TurnStart, 1), &mut prompt)
        .unwrap();
    let root = context_ledger_path(dir.path(), &session.to_string());
    let baseline = std::fs::read(&root).unwrap();
    for iteration in 2..=5 {
        let id = format!("inspect-{iteration}");
        let mut call = Message::assistant("");
        call.tool_calls = Some(vec![octos_core::ToolCall {
            id: id.clone(),
            name: "view_image".into(),
            arguments: json!({"path": format!("page-{iteration}.png")}),
            metadata: None,
        }]);
        let mut output =
            Message::tool_with_thread("page inspected", id, octos_core::ThreadId::new("deck"));
        output.media = vec![format!("page-{iteration}.png")];
        history.extend([call.clone(), output.clone()]);
        prompt.extend([call, output]);
        bridge
            .prepare_prompt(
                request(PromptContextPhase::Iteration, iteration),
                &mut prompt,
            )
            .unwrap();
        assert!(
            std::fs::read(&root).unwrap() == baseline,
            "ordinary prompt preparation must append a transition, not rewrite the baseline"
        );
    }

    // An interrupted turn must not turn source-less loop rows into committed history.
    let (recovered, status) =
        load_or_rebuild_context_manager(dir.path(), session.to_string(), None, &history[..1]);
    assert_eq!(status, ContextLedgerLoadStatus::Loaded);
    assert_eq!(
        normalized_prompt(&recovered),
        normalized_prompt(&ContextManager::from_session_history(
            session.to_string(),
            None,
            &history[..1]
        ))
    );

    // Once transcript rows are committed, stamp their in-flight twins and resume
    // the exact call/result pairs and media from baseline + transitions.
    let mut batch =
        postturn_context_batch::AppUiContextCommitBatch::new(dir.path(), &manager, &session);
    for (seq, row) in history.iter().enumerate().skip(1) {
        batch.record(row, seq);
    }
    batch.finish().unwrap();
    let (loaded, status) =
        load_or_rebuild_context_manager(dir.path(), session.to_string(), None, &history);
    assert_eq!(status, ContextLedgerLoadStatus::Loaded);
    assert_eq!(
        normalized_prompt(&loaded),
        normalized_prompt(&manager.lock().unwrap())
    );
    assert!(std::fs::read(root).unwrap() == baseline);
}

#[test]
fn should_checkpoint_inloop_compaction_then_return_to_incremental_writes() {
    let dir = tempfile::tempdir().unwrap();
    let session = SessionKey::new("api", "inloop-compaction-checkpoint");
    let mut history = Vec::new();
    for i in 0..20 {
        history.push(Message::user(format!("old task {i}: {}", "x".repeat(400))));
        history.push(Message::assistant(format!(
            "old answer {i}: {}",
            "y".repeat(400)
        )));
    }
    history.push(Message::user("Finish the current task"));
    let mut initial = ContextManager::from_session_history(session.to_string(), None, &history);
    initial.reconcile_prompt_cache_epoch("fixture", "model", "stable runtime", &[]);
    let manager = Arc::new(StdMutex::new(initial));
    let bridge = AppUiPromptContextBridge::new(
        session.clone(),
        dir.path().to_owned(),
        manager.clone(),
        false,
    );
    let mut prompt = vec![Message::system("stable runtime")];
    prompt.extend(history.clone());
    bridge
        .prepare_prompt(request(PromptContextPhase::TurnStart, 1), &mut prompt)
        .unwrap();
    let root = context_ledger_path(dir.path(), &session.to_string());
    let baseline = std::fs::read(&root).unwrap();
    let mut compact = request(PromptContextPhase::Iteration, 2);
    compact.context_window = 300;
    let report = bridge.prepare_prompt(compact, &mut prompt).unwrap();
    assert!(report.compaction_performed);
    let compacted = std::fs::read(&root).unwrap();
    assert!(
        compacted != baseline,
        "compaction must install a new baseline"
    );
    let (loaded, status) =
        load_or_rebuild_context_manager(dir.path(), session.to_string(), None, &history);
    assert_eq!(status, ContextLedgerLoadStatus::Loaded);
    assert_eq!(loaded.compactions(), manager.lock().unwrap().compactions());
    assert_eq!(loaded.cache_epoch(), manager.lock().unwrap().cache_epoch());
    assert_eq!(
        normalized_prompt(&loaded),
        normalized_prompt(&manager.lock().unwrap())
    );

    bridge
        .prepare_prompt(request(PromptContextPhase::Iteration, 3), &mut prompt)
        .unwrap();
    assert!(
        std::fs::read(root).unwrap() == compacted,
        "the iteration after compaction must return to incremental persistence"
    );
    let (loaded, status) =
        load_or_rebuild_context_manager(dir.path(), session.to_string(), None, &history);
    assert_eq!(status, ContextLedgerLoadStatus::Loaded);
    assert_eq!(
        normalized_prompt(&loaded),
        normalized_prompt(&manager.lock().unwrap())
    );
}

#[test]
fn should_retry_failed_inloop_append_without_losing_pending_rows() {
    let dir = tempfile::tempdir().unwrap();
    let session = SessionKey::new("api", "inloop-append-retry");
    let mut history = vec![Message::user("finish deck")];
    let manager = Arc::new(StdMutex::new(ContextManager::from_session_history(
        session.to_string(),
        None,
        &history,
    )));
    let bridge = AppUiPromptContextBridge::new(
        session.clone(),
        dir.path().to_owned(),
        manager.clone(),
        false,
    );
    let mut prompt = history.clone();
    bridge
        .prepare_prompt(request(PromptContextPhase::TurnStart, 1), &mut prompt)
        .unwrap();
    let root = context_ledger_path(dir.path(), &session.to_string());
    let baseline = std::fs::read(&root).unwrap();
    let journal = std::fs::read_dir(root.parent().unwrap())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.to_string_lossy().ends_with(".transitions.jsonl"))
        .unwrap();
    std::fs::remove_file(&journal).unwrap();
    std::fs::create_dir(&journal).unwrap();
    let first = Message::assistant("first pending result");
    history.push(first.clone());
    prompt.push(first);
    // Rebuildable-context persistence is best effort; failure must not clear
    // the dirty cursor or abort the model turn.
    bridge
        .prepare_prompt(request(PromptContextPhase::Iteration, 2), &mut prompt)
        .unwrap();
    assert!(std::fs::read(&root).unwrap() == baseline);
    std::fs::remove_dir(&journal).unwrap();
    std::fs::write(&journal, []).unwrap();
    let second = Message::assistant("second pending result");
    history.push(second.clone());
    prompt.push(second);
    bridge
        .prepare_prompt(request(PromptContextPhase::Iteration, 3), &mut prompt)
        .unwrap();
    assert!(std::fs::metadata(&journal).unwrap().len() > 0);
    let mut batch =
        postturn_context_batch::AppUiContextCommitBatch::new(dir.path(), &manager, &session);
    for (seq, row) in history.iter().enumerate().skip(1) {
        batch.record(row, seq);
    }
    batch.finish().unwrap();
    let (loaded, status) =
        load_or_rebuild_context_manager(dir.path(), session.to_string(), None, &history);
    assert_eq!(status, ContextLedgerLoadStatus::Loaded);
    assert_eq!(
        normalized_prompt(&loaded),
        normalized_prompt(&manager.lock().unwrap())
    );
    assert!(
        normalized_prompt(&loaded)
            .to_string()
            .contains("first pending result")
    );
    assert!(std::fs::read(root).unwrap() == baseline);
}

struct BoundedToolProjectionProvider {
    call: octos_core::ToolCall,
    requests: Arc<StdMutex<Vec<Vec<Message>>>>,
}

#[async_trait::async_trait]
impl octos_llm::LlmProvider for BoundedToolProjectionProvider {
    async fn chat(
        &self,
        messages: &[Message],
        _tools: &[octos_llm::ToolSpec],
        _config: &octos_llm::ChatConfig,
    ) -> eyre::Result<octos_llm::ChatResponse> {
        let mut requests = self.requests.lock().unwrap();
        requests.push(messages.to_vec());
        let first = requests.len() == 1;
        Ok(octos_llm::ChatResponse {
            content: (!first).then(|| "done".to_string()),
            reasoning_content: None,
            tool_calls: if first {
                vec![self.call.clone()]
            } else {
                vec![]
            },
            stop_reason: if first {
                octos_llm::StopReason::ToolUse
            } else {
                octos_llm::StopReason::EndTurn
            },
            usage: Default::default(),
            provider_index: None,
        })
    }

    fn model_id(&self) -> &str {
        "bounded-tool-fixture"
    }
    fn provider_name(&self) -> &str {
        "fixture"
    }
    fn context_window(&self) -> u32 {
        200_000
    }
}

/// Exercise the native task loop and real AppUI bridge, observing the prompt
/// at the provider boundary after real filesystem/command tool execution.
async fn bounded_tool_projection(
    dir: &std::path::Path,
    tool_name: &str,
    arguments: Value,
    policy: Option<crate::context_manager::ToolOutputPolicy>,
) -> (String, String) {
    bounded_tool_projection_with_hooks(dir, tool_name, arguments, policy, None).await
}

async fn bounded_tool_projection_with_hooks(
    dir: &std::path::Path,
    tool_name: &str,
    arguments: Value,
    policy: Option<crate::context_manager::ToolOutputPolicy>,
    hooks: Option<Arc<octos_agent::hooks::HookExecutor>>,
) -> (String, String) {
    let session = SessionKey::new("api", "bounded-tool-projection");
    let mut manager = ContextManager::new(session.to_string(), None);
    if let Some(policy) = policy {
        manager = manager.with_tool_output_policy(policy);
    }
    let manager = Arc::new(StdMutex::new(manager));
    let bridge = AppUiPromptContextBridge::new(session, dir.join("data"), manager.clone(), false);
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let provider = Arc::new(BoundedToolProjectionProvider {
        call: octos_core::ToolCall {
            id: "call_bounded_tool".into(),
            name: tool_name.into(),
            arguments,
            metadata: None,
        },
        requests: requests.clone(),
    });
    let mut tools = octos_agent::ToolRegistry::with_builtins(dir);
    // The actual child roster has no recall tool: recovery must remain usable
    // through its normal filesystem/command tools.
    tools.retain(|name| matches!(name, "read_file" | "write_file" | "bash" | "shell"));
    assert!(!tools.specs().iter().any(|tool| tool.name == "recall"));
    let memory = Arc::new(
        octos_memory::EpisodeStore::open(dir.join("memory"))
            .await
            .unwrap(),
    );
    let mut agent = octos_agent::Agent::new(
        octos_core::AgentId::new("projection-worker"),
        provider,
        tools,
        memory,
    )
    .with_system_prompt("Inspect the requested file.".into())
    .with_config(octos_agent::AgentConfig {
        save_episodes: false,
        ..Default::default()
    })
    .with_prompt_context_manager(Arc::new(bridge));
    if let Some(hooks) = hooks {
        agent = agent.with_hooks(hooks);
    }
    let task = octos_core::Task::new(
        octos_core::TaskKind::Code {
            instruction: "Inspect the file".into(),
            files: vec![],
        },
        octos_core::TaskContext {
            working_dir: dir.into(),
            ..Default::default()
        },
    );
    assert!(agent.run_task(&task).await.unwrap().success);
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    let delivered = requests[1]
        .iter()
        .find(|row| row.role == MessageRole::Tool)
        .unwrap()
        .content
        .clone();
    let raw = manager
        .lock()
        .unwrap()
        .tool_output_by_call_id("call_bounded_tool")
        .unwrap();
    (delivered, raw)
}

#[tokio::test]
async fn bounded_tool_projection_retains_native_read_file_requested_range() {
    for text in ["context-boundary ", "中文技能说明，保留完整指令。"] {
        let dir = tempfile::tempdir().unwrap();
        let source = (1..=350)
            .map(|line| format!("record_{line:03} {}\n", text.repeat(5)))
            .collect::<String>();
        std::fs::write(dir.path().join("reference.txt"), source).unwrap();
        let (delivered, raw) = bounded_tool_projection(
            dir.path(),
            "read_file",
            json!({"filePath": "reference.txt", "start_line": 70, "limit": 200}),
            None,
        )
        .await;
        assert!(raw.len() > 4096 && raw.len() <= octos_core::tool_output_limit("read_file"));
        assert!(raw.contains("record_269") && !raw.contains("record_270"));
        if !text.is_ascii() {
            assert!(raw.len() > raw.chars().count());
        }
        assert_eq!(
            delivered, raw,
            "the model must receive the entire byte-bounded requested range, including CJK"
        );
    }
}

#[tokio::test]
async fn bounded_tool_projection_preserves_native_read_file_continuation_footer() {
    for (line_count, text) in [
        (10_000, "bounded file content".to_owned()),
        (10, "宽".repeat(2_000)),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let source = (1..=line_count)
            .map(|line| format!("record_{line:05}: {text}\n"))
            .collect::<String>();
        std::fs::write(dir.path().join("large.txt"), source).unwrap();
        let (delivered, raw) = bounded_tool_projection(
            dir.path(),
            "read_file",
            json!({"path": "large.txt", "start_line": 1, "limit": 10_000}),
            None,
        )
        .await;
        assert!(
            raw.contains("Continue with offset:"),
            "the tool must have emitted real paging advice"
        );
        assert!(raw.len() <= octos_core::tool_output_limit("read_file"));
        assert_eq!(
            delivered, raw,
            "projection must retain exact coverage and continuation for many lines and CJK wide lines"
        );
    }
}

#[tokio::test]
async fn bounded_tool_projection_preserves_bash_file_body_and_full_log_pointer() {
    for line_count in [180, 1_000] {
        let dir = tempfile::tempdir().unwrap();
        let source = (1..=line_count)
            .map(|line| format!("record_{line:04} {}\n", "ordinary tool output ".repeat(5)))
            .collect::<String>();
        std::fs::write(dir.path().join("command-output.txt"), &source).unwrap();
        let (delivered, raw) = bounded_tool_projection(
            dir.path(),
            "bash",
            json!({"cmd": "cat command-output.txt"}),
            None,
        )
        .await;
        assert!(raw.len() > 4096 && raw.len() <= octos_core::tool_output_limit("bash"));
        if line_count == 180 {
            assert!(raw.contains("record_0180"));
        } else {
            assert!(
                raw.contains("Full output:"),
                "the bounded command output must retain its full-log pointer"
            );
            assert!(raw.contains("truncated"));
        }
        assert_eq!(
            delivered, raw,
            "the provider must receive the bounded Bash body, exit status, and log reference"
        );
    }
}

#[tokio::test]
async fn bounded_tool_projection_tighter_policy_keeps_usable_native_read_recovery() {
    let dir = tempfile::tempdir().unwrap();
    let source = (1..=100)
        .map(|line| format!("record_{line:03} {}\n", "ordinary text ".repeat(5)))
        .collect::<String>();
    std::fs::write(dir.path().join("reference.txt"), source).unwrap();
    let policy = crate::context_manager::ToolOutputPolicy {
        policy_id: "explicit-small-policy".into(),
        inline_raw_threshold_bytes: 512,
        model_visible_max_bytes: Some(1024),
    };
    let (delivered, raw) = bounded_tool_projection(
        dir.path(),
        "read_file",
        json!({
            "path": "reference.txt", "start_line": 20, "limit": 60
        }),
        Some(policy.clone()),
    )
    .await;
    assert!(raw.len() > 1024);
    assert!(delivered.starts_with(&raw[..1024]));
    assert!(delivered.ends_with("[truncated]"));
    assert!(
        delivered.contains("start_line") && delivered.contains("limit"),
        "recovery must name callable native read parameters: {delivered}"
    );
    assert!(
        delivered.contains("If available"),
        "recall is optional, not an advertised child capability"
    );
    assert!(
        !delivered.contains("record_079"),
        "the explicit smaller body budget must still apply"
    );
    let next_line: usize = delivered
        .split("start_line: ")
        .nth(1)
        .unwrap()
        .split(',')
        .next()
        .unwrap()
        .parse()
        .unwrap();
    assert!(next_line > 20 && next_line < 79);
    let (continued, continued_raw) = bounded_tool_projection(
        dir.path(),
        "read_file",
        json!({
            "path": "reference.txt", "start_line": next_line, "limit": 1
        }),
        Some(policy),
    )
    .await;
    assert_eq!(
        continued, continued_raw,
        "the emitted recovery call must itself reach the provider intact"
    );
    assert!(continued.contains(&format!("record_{next_line:03}")));
}

#[tokio::test]
async fn bounded_tool_projection_preserves_post_budget_hook_diagnostics() {
    use octos_agent::hooks::{HookConfig, HookEvent, HookExecutor};
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("output.txt"),
        "ordinary command output\n".repeat(5_000),
    )
    .unwrap();
    let diagnostics = dir.path().join("diagnostic.txt");
    std::fs::write(
        &diagnostics,
        format!(
            "{}\nFINAL_CHECKER_DIAGNOSTIC",
            "checker detail ".repeat(125)
        ),
    )
    .unwrap();
    let hook = HookConfig {
        event: HookEvent::AfterToolCall,
        command: vec![
            "sh".into(),
            "-c".into(),
            "cat \"$1\"; exit 1".into(),
            "hook".into(),
            diagnostics.to_string_lossy().into_owned(),
        ],
        timeout_ms: 5_000,
        tool_filter: vec!["bash".into()],
        path_filter: vec![],
        requires_bin: None,
    };
    let (delivered, raw) = bounded_tool_projection_with_hooks(
        dir.path(),
        "bash",
        json!({"cmd": "cat output.txt"}),
        None,
        Some(Arc::new(HookExecutor::new(vec![hook.clone(), hook]))),
    )
    .await;
    assert!(raw.len() > octos_core::tool_output_limit("bash"));
    assert!(raw.contains("Full output:") && raw.contains("FINAL_CHECKER_DIAGNOSTIC"));
    assert!(
        delivered.contains("Full output:"),
        "the native full-log reference must remain visible"
    );
    assert!(
        delivered.contains("FINAL_CHECKER_DIAGNOSTIC"),
        "post-budget diagnostics must survive projection"
    );
    assert_eq!(
        delivered.matches("FINAL_CHECKER_DIAGNOSTIC").count(),
        raw.matches("FINAL_CHECKER_DIAGNOSTIC").count(),
        "every appended checker diagnostic must reach the model"
    );
    assert!(
        delivered.contains("bytes omitted"),
        "any projection loss must be explicitly reported"
    );
}
