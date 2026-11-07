// T-0346: serve-path turn undo. The per-turn agent rebuild used to drop the
// session agent's snapshot manager, so `snapshots.enabled` produced zero
// undo points on WS turns. These tests prove the pre-turn snapshot exists,
// covers everything the round changed, and that restore → restore-again
// round-trips (undo, then undo the undo) while the conversation survives.
struct TurnUndoStubLlm {
    calls: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl octos_llm::LlmProvider for TurnUndoStubLlm {
    async fn chat(
        &self,
        _messages: &[Message],
        _tools: &[octos_llm::ToolSpec],
        _config: &octos_llm::ChatConfig,
    ) -> eyre::Result<octos_llm::ChatResponse> {
        let call = self.calls.fetch_add(1, Ordering::Relaxed);
        let tool_calls = match call {
            0 => vec![octos_core::ToolCall {
                id: "call-write".into(),
                name: "write_file".into(),
                arguments: json!({"path": "undo-target.txt", "content": "written by the turn\n"}),
                metadata: None,
            }],
            1 => vec![octos_core::ToolCall {
                id: "call-overwrite".into(),
                name: "write_file".into(),
                arguments: json!({"path": "seed.txt", "content": "overwritten by the turn\n"}),
                metadata: None,
            }],
            _ => Vec::new(),
        };
        let done = tool_calls.is_empty();
        Ok(octos_llm::ChatResponse {
            content: if done { Some("turn done".into()) } else { None },
            reasoning_content: None,
            tool_calls,
            stop_reason: if done {
                octos_llm::StopReason::EndTurn
            } else {
                octos_llm::StopReason::ToolUse
            },
            usage: octos_llm::TokenUsage::default(),
            provider_index: None,
        })
    }

    fn model_id(&self) -> &str {
        "turn-undo-stub"
    }

    fn provider_name(&self) -> &str {
        "stub"
    }
}

async fn undo_probe_state(
    temp: &std::path::Path,
    workspace: &std::path::Path,
) -> (Arc<AppState>, Arc<crate::runtime::ProfileRuntime>) {
    let provider = Arc::new(TurnUndoStubLlm {
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let profile_data = temp.join("profile-data");
    let mut runtime = make_m11e_profile_with_llm_and_sandbox(
        "undoprofile",
        &profile_data,
        provider,
        octos_agent::SandboxConfig::default(),
    )
    .await;
    Arc::get_mut(&mut runtime)
        .expect("fresh runtime is uniquely owned")
        .snapshots = Some(octos_agent::SnapshotConfig {
        enabled: true,
        keep_last: 20,
    });

    let sessions = Arc::new(tokio::sync::Mutex::new(
        octos_bus::SessionManager::open(temp).expect("session manager"),
    ));
    let mut profiles = HashMap::new();
    profiles.insert("undoprofile".to_string(), runtime.clone());
    let mut state = AppState {
        profiles,
        sessions: Some(sessions),
        ..AppState::empty_for_tests()
    };
    state.session_cache = Arc::new(
        crate::runtime::SessionRuntimeCache::new(4, Duration::from_secs(60))
            .with_sessions_in_cwd(true),
    );
    let _ = workspace;
    (Arc::new(state), runtime)
}

async fn snapshot_list(
    state: &Arc<AppState>,
    session_id: &SessionKey,
    profile: &str,
) -> Vec<Value> {
    raw_snapshot_list(
        state,
        &RpcRequest::<Value>::new(
            format!("snap-list-{}", uuid::Uuid::now_v7()),
            APPUI_METHOD_SNAPSHOT_LIST,
            json!({ "session_id": session_id }),
        ),
        Some(profile),
    )
    .expect("snapshot/list")
    .get("snapshots")
    .and_then(Value::as_array)
    .cloned()
    .unwrap_or_default()
}

async fn snapshot_restore(state: &Arc<AppState>, session_id: &SessionKey, profile: &str, id: &str) {
    raw_snapshot_restore(
        state,
        &RpcRequest::<Value>::new(
            format!("snap-restore-{}", uuid::Uuid::now_v7()),
            APPUI_METHOD_SNAPSHOT_RESTORE,
            json!({ "session_id": session_id, "snapshot_id": id }),
        ),
        Some(profile),
    )
    .await
    .expect("snapshot/restore");
}

#[tokio::test(flavor = "current_thread")]
async fn turn_undo_restores_round_changes_and_undo_is_itself_undoable() {
    let temp = tempfile::TempDir::new().expect("temp dir");
    let workspace = temp.path().join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(workspace.join("seed.txt"), "original seed\n").unwrap();

    let (state, _runtime) = undo_probe_state(temp.path(), &workspace).await;
    let session_id = SessionKey::with_profile("undoprofile", "api", "undo-round");
    let active_turns: SharedActiveTurns = Arc::new(TokioMutex::new(HashMap::new()));
    let connection_turns: SharedConnectionTurns = Arc::new(TokioMutex::new(HashMap::new()));
    let ledger = Arc::new(UiProtocolLedger::new(32));
    let contracts = Arc::new(UiProtocolContractStores::default());
    let forwarders: SharedLiveForwarders = Arc::new(TokioMutex::new(HashMap::new()));
    let (ws, mut rx) = ws_connection_for_test(64);
    let features = ConnectionUiFeatures::stdio_defaults();

    let opened = handle_session_open(
        &ws,
        &state,
        &ledger,
        &contracts.approvals,
        &contracts.user_questions,
        &forwarders,
        Some("undoprofile"),
        None,
        features,
        "open".into(),
        SessionOpenParams {
            session_ref: None,
            client_commands: None,
            session_id: session_id.clone(),
            topic: None,
            profile_id: Some("undoprofile".into()),
            cwd: Some(workspace.to_string_lossy().into_owned()),
            sandbox: None,
            after: None,
        },
        false,
    )
    .await;
    assert!(opened, "session/open must succeed");

    assert!(
        snapshot_list(&state, &session_id, "undoprofile").await.is_empty(),
        "no snapshots before the first turn"
    );

    let turn_id = TurnId::new();
    handle_turn_start(
        &ws,
        &state,
        &ledger,
        &contracts,
        &active_turns,
        &connection_turns,
        None,
        None,
        features,
        "turn".into(),
        TurnStartParams {
            session_id: session_id.clone(),
            turn_id: turn_id.clone(),
            input: vec![InputItem::Text {
                text: "write and overwrite".into(),
            }],
            media: Vec::new(),
            topic: None,
            rewrite_for: None,
            reasoning_effort: None,
            tool_context: None,
            live_video: false,
            origin: None,
        },
    )
    .await;
    // Wait for the turn to settle, auto-approving any tool approval the way
    // the web client would.
    let mut seen: Vec<String> = Vec::new();
    let settled = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let frame = recv_rpc_json(&mut rx).await;
            seen.push(frame.to_string().chars().take(300).collect());
            let m = frame.get("method").and_then(Value::as_str);
            if m == Some("approval/requested") {
                let approval_id = frame["params"]["approval_id"].as_str().unwrap().to_string();
                contracts
                    .approvals
                    .respond(ApprovalRespondParams::new(
                        session_id.clone(),
                        {
                            let raw = approval_id.parse::<uuid::Uuid>().expect("approval id is a uuid");
                            ApprovalId(raw)
                        },
                        ApprovalDecision::Approve,
                    ))
                    .expect("pending approval accepts");
                continue;
            }
            let is_v2_terminal = m == Some("projection/envelope")
                && frame["params"]["payload"]["type"].as_str() == Some("turn_terminal");
            if m == Some("turn/completed") || m == Some("turn/error") || is_v2_terminal {
                if let Some(m) = m {
                    if m == "turn/error" {
                        panic!("turn failed: {frame}");
                    }
                }
                if is_v2_terminal {
                    assert_eq!(
                        frame["params"]["payload"]["data"]["outcome"].as_str(),
                        Some("completed"),
                        "turn failed: {frame}"
                    );
                }
                break;
            }
        }
    })
    .await;
    assert!(settled.is_ok(), "turn must settle; frames: {seen:#?}");

    // The round changed two files.
    assert_eq!(
        std::fs::read_to_string(workspace.join("undo-target.txt")).unwrap(),
        "written by the turn\n"
    );
    assert_eq!(
        std::fs::read_to_string(workspace.join("seed.txt")).unwrap(),
        "overwritten by the turn\n"
    );

    let snaps = snapshot_list(&state, &session_id, "undoprofile").await;
    let pre_turn = snaps
        .iter()
        .find(|row| row["turn_id"].as_str() == Some(turn_id.0.to_string().as_str()))
        .unwrap_or_else(|| panic!("pre-turn snapshot for {turn_id:?} missing: {snaps:?}"));
    let pre_turn_id = pre_turn["id"].as_str().unwrap().to_string();

    // Undo the round: both files return to their pre-turn state.
    snapshot_restore(&state, &session_id, "undoprofile", &pre_turn_id).await;
    assert!(
        !workspace.join("undo-target.txt").exists(),
        "undo must remove the file the turn created"
    );
    assert_eq!(
        std::fs::read_to_string(workspace.join("seed.txt")).unwrap(),
        "original seed\n",
        "undo must restore the overwritten file"
    );
    // The conversation survives the file rollback.
    let runtime = state
        .session_cache
        .get_or_init(&_runtime, session_id.clone(), Some(workspace.clone()))
        .await
        .expect("cached session runtime");
    let messages = runtime
        .sessions
        .lock()
        .await
        .get_or_create(&session_id)
        .await
        .messages
        .clone();
    assert!(
        messages
            .iter()
            .any(|m| m.role == MessageRole::User && m.content == "write and overwrite"),
        "undo must not touch the conversation: {messages:?}"
    );

    // Undo the undo: the restore's own pre-restore snapshot returns the
    // workspace to the post-turn state.
    let snaps = snapshot_list(&state, &session_id, "undoprofile").await;
    let pre_restore = snaps
        .iter()
        .find(|row| row["label"].as_str() == Some("pre-restore"))
        .unwrap_or_else(|| panic!("pre-restore snapshot missing: {snaps:?}"));
    snapshot_restore(
        &state,
        &session_id,
        "undoprofile",
        pre_restore["id"].as_str().unwrap(),
    )
    .await;
    assert_eq!(
        std::fs::read_to_string(workspace.join("undo-target.txt")).unwrap(),
        "written by the turn\n"
    );
    assert_eq!(
        std::fs::read_to_string(workspace.join("seed.txt")).unwrap(),
        "overwritten by the turn\n"
    );
}

/// T-0434 #1: a refused restore must surface the conflicting FILE NAME to the
/// UI — `raw_snapshot_restore` must keep the whole error chain (`{err:#}`),
/// not just the outermost context line.
#[tokio::test(flavor = "current_thread")]
async fn restore_conflict_error_names_the_file_to_the_client() {
    let temp = tempfile::TempDir::new().expect("temp dir");
    let workspace = temp.path().join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(workspace.join("notes.txt"), "one\n").unwrap();

    let (state, _runtime) = undo_probe_state(temp.path(), &workspace).await;
    let session_id = SessionKey::with_profile("undoprofile", "api", "undo-conflict");
    let forwarders: SharedLiveForwarders = Arc::new(TokioMutex::new(HashMap::new()));
    let contracts = UiProtocolContractStores::default();
    let ledger = Arc::new(UiProtocolLedger::new(32));
    let (ws, _rx) = ws_connection_for_test(64);
    assert!(
        handle_session_open(
            &ws,
            &state,
            &ledger,
            &contracts.approvals,
            &contracts.user_questions,
            &forwarders,
            Some("undoprofile"),
            None,
            ConnectionUiFeatures::stdio_defaults(),
            "open".into(),
            SessionOpenParams {
                session_ref: None,
                client_commands: None,
                session_id: session_id.clone(),
                topic: None,
                profile_id: Some("undoprofile".into()),
                cwd: Some(workspace.to_string_lossy().into_owned()),
                sandbox: None,
                after: None,
            },
            false,
        )
        .await
    );

    let (_enabled, manager) = snapshot_context_for_session(&state, Some("undoprofile"), &session_id)
        .expect("snapshot context");
    let manager = std::sync::Arc::new(manager.expect("snapshots enabled"));
    let first = manager
        .take_snapshot_async("pre-turn turn-1")
        .await
        .expect("pre-turn snapshot");
    std::fs::write(workspace.join("notes.txt"), "two\n").unwrap();
    snapshot_restore(&state, &session_id, "undoprofile", first.as_str()).await;
    assert_eq!(std::fs::read_to_string(workspace.join("notes.txt")).unwrap(), "one\n");

    // The user now edits by hand (no new turn): the next restore must refuse
    // AND name the file in the error the client receives.
    std::fs::write(workspace.join("notes.txt"), "user-edit\n").unwrap();
    let error = raw_snapshot_restore(
        &state,
        &RpcRequest::<Value>::new(
            "snap-restore-conflict",
            APPUI_METHOD_SNAPSHOT_RESTORE,
            json!({ "session_id": session_id, "snapshot_id": first.as_str() }),
        ),
        Some("undoprofile"),
    )
    .await
    .expect_err("restore must refuse to clobber the user edit");
    assert!(
        error.message.contains("notes.txt"),
        "the client-visible error must name the conflicting file: {}",
        error.message
    );
    // T-0467: a structured reason + workspace-relative paths, so clients stop
    // matching on the message text.
    let data = error.data.as_ref().expect("conflict carries structured data");
    assert_eq!(data["reason"], "files_edited");
    assert_eq!(data["paths"], json!(["notes.txt"]));
    assert_eq!(
        std::fs::read_to_string(workspace.join("notes.txt")).unwrap(),
        "user-edit\n"
    );
}

/// T-0467: restoring while a turn is running is refused with a structured
/// `turn_running` reason, distinct from an edit conflict.
#[tokio::test(flavor = "current_thread")]
async fn restore_while_turn_running_reports_turn_running_reason() {
    let temp = tempfile::TempDir::new().expect("temp dir");
    let workspace = temp.path().join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(workspace.join("notes.txt"), "one\n").unwrap();
    let (state, _runtime) = undo_probe_state(temp.path(), &workspace).await;
    let session_id = SessionKey::with_profile("undoprofile", "api", "undo-running");
    let forwarders: SharedLiveForwarders = Arc::new(TokioMutex::new(HashMap::new()));
    let contracts = UiProtocolContractStores::default();
    let ledger = Arc::new(UiProtocolLedger::new(32));
    let (ws, _rx) = ws_connection_for_test(64);
    assert!(
        handle_session_open(
            &ws,
            &state,
            &ledger,
            &contracts.approvals,
            &contracts.user_questions,
            &forwarders,
            Some("undoprofile"),
            None,
            ConnectionUiFeatures::stdio_defaults(),
            "open".into(),
            SessionOpenParams {
                session_ref: None,
                client_commands: None,
                session_id: session_id.clone(),
                topic: None,
                profile_id: Some("undoprofile".into()),
                cwd: Some(workspace.to_string_lossy().into_owned()),
                sandbox: None,
                after: None,
            },
            false,
        )
        .await
    );
    // Register a RUNNING turn for this session in the process-global registry.
    let active = active_turns_registry();
    let turn_id = TurnId::new();
    active.lock().await.insert(
        session_id.clone(),
        ActiveTurn {
            dispatched_prompt: None,
            turn_cancel: tokio_util::sync::CancellationToken::new(),
            owner: None,
            profile_id: "undoprofile".to_owned(),
            turn_id: turn_id.clone(),
            state: Arc::new(TokioMutex::new(TurnState::Active)),
            interrupt_tx: Arc::new(TokioMutex::new(None)),
            steer: None,
            abort: tokio::spawn(async {}).abort_handle(),
        },
    );
    let error = raw_snapshot_restore(
        &state,
        &RpcRequest::<Value>::new(
            "snap-restore-running",
            APPUI_METHOD_SNAPSHOT_RESTORE,
            json!({ "session_id": session_id.clone(), "snapshot_id": "any" }),
        ),
        Some("undoprofile"),
    )
    .await
    .expect_err("restore during a running turn must be refused");
    let data = error.data.as_ref().expect("turn-running carries structured data");
    assert_eq!(data["reason"], "turn_running");
    assert!(error.message.contains("turn is running"), "{}", error.message);
    active.lock().await.remove(&session_id);
}
