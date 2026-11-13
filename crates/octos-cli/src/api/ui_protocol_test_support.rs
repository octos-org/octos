use super::*;
use axum::extract::ws::Message as WsMessage;
use serde_json::Value;
use tokio::sync::mpsc;

pub(crate) async fn recv_rpc_json(rx: &mut mpsc::Receiver<WsMessage>) -> Value {
    let text = match rx.recv().await.expect("rpc frame") {
        WsMessage::Text(text) => Ok(text),
        other => Err(other),
    }
    .expect("expected text frame");
    serde_json::from_str(text.as_str()).expect("json frame")
}

/// One connection's raw App UI RPC path, with its own ledger, contract
/// stores and turn maps. The replies go to the connection's receiver.
pub(crate) struct RawRpc {
    state: Arc<AppState>,
    ws: WsConnection,
    ledger: Arc<UiProtocolLedger>,
    contracts: Arc<UiProtocolContractStores>,
    active_turns: SharedActiveTurns,
    connection_turns: SharedConnectionTurns,
}

impl RawRpc {
    pub(crate) fn new(state: &Arc<AppState>, ws: &WsConnection) -> Self {
        Self {
            state: Arc::clone(state),
            ws: ws.clone(),
            ledger: Arc::new(UiProtocolLedger::new(16)),
            contracts: Arc::new(UiProtocolContractStores::default()),
            active_turns: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            connection_turns: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        }
    }

    /// Whether the raw path handled `method`.
    pub(crate) async fn call(
        &self,
        method: &str,
        features: ConnectionUiFeatures,
        id: &str,
        params: Value,
    ) -> bool {
        let request = RpcRequest::new(id.to_owned(), method, params);
        let (ledger, contracts) = (&self.ledger, &self.contracts);
        let (active_turns, connection_turns) = (&self.active_turns, &self.connection_turns);
        handle_raw_appui_rpc(
            &self.ws,
            &self.state,
            ledger,
            contracts,
            active_turns,
            connection_turns,
            features,
            None,
            id.to_owned(),
            &request,
        )
        .await
    }
}

/// Builds a `WsConnection` whose writer side feeds the retained tests' mpsc.
pub(crate) fn ws_connection_for_test(capacity: usize) -> (WsConnection, mpsc::Receiver<WsMessage>) {
    let (tx, rx) = mpsc::channel(capacity);
    (WsConnection::new(tx), rx)
}

pub(crate) fn test_open_params(
    session_id: SessionKey,
    after: Option<UiCursor>,
) -> SessionOpenParams {
    SessionOpenParams {
        session_id,
        after,
        ..SessionOpenParams::default()
    }
}

pub(crate) fn state_with_sessions(data_dir: &std::path::Path) -> Arc<AppState> {
    Arc::new(AppState {
        sessions: Some(Arc::new(tokio::sync::Mutex::new(
            octos_bus::SessionManager::open(data_dir).expect("session manager"),
        ))),
        ..AppState::empty_for_tests()
    })
}

pub(crate) async fn make_m11e_profile_with_llm_and_sandbox(
    profile_id: &str,
    data_dir: &std::path::Path,
    llm: Arc<dyn octos_llm::LlmProvider>,
    sandbox: octos_agent::SandboxConfig,
) -> Arc<crate::runtime::ProfileRuntime> {
    std::fs::create_dir_all(data_dir).expect("profile data dir");
    let memory = Arc::new(
        octos_memory::EpisodeStore::open(data_dir)
            .await
            .expect("episode store"),
    );
    let memory_store = Arc::new(
        octos_memory::MemoryStore::open(data_dir)
            .await
            .expect("memory store"),
    );
    let tool_config = Arc::new(
        octos_agent::ToolConfigStore::open(data_dir)
            .await
            .expect("tool config store"),
    );
    let base_tools = octos_agent::ToolRegistry::with_builtins_and_sandbox(
        data_dir,
        octos_agent::create_sandbox(&sandbox),
    );
    let recall = Arc::new(
        octos_memory::RecallStore::open(data_dir, octos_memory::RecallConfig::default())
            .expect("recall store"),
    );
    Arc::new(crate::runtime::ProfileRuntime {
        profile_id: profile_id.to_string(),
        data_dir: data_dir.to_path_buf(),
        session_store_root: None,
        config: crate::config::Config::default(),
        artifact_retention: Default::default(),
        llm,
        goal_verifier_llm: None,
        adaptive_router: None,
        runtime_qos_catalog: None,
        credential_pool: None,
        content_classifier: None,
        primary_model_id: "m11e-stub".to_string(),
        provider_name: "stub".to_string(),
        credentials: HashMap::new(),
        skills_dir: None,
        plugin_env_template: Vec::new(),
        tool_policy: None,
        default_sandbox: sandbox,
        max_iterations: None,
        session_defaults: None,
        agent_profile: None,
        format_after_edit: false,
        snapshots: None,
        tool_specs: Arc::new(base_tools),
        plugin_tool_names: Vec::new(),
        skill_actions: Vec::new(),
        mcp_client: None,
        plugin_reload: None,
        plugin_dirs: Vec::new(),
        plugin_prompt_fragments: Vec::new(),
        plugin_hooks: Vec::new(),
        review_config: None,
        human_approval_rules: None,
        system_prompt: "test-system-prompt".to_string(),
        prompt_parts: crate::commands::gateway::prompt::GatewayPromptParts {
            uses_bundled_base: false,
            pre_memory: "test-system-prompt".to_string(),
            post_memory: String::new(),
        },
        memory,
        memory_store,
        recall,
        embedder: None,
        memory_inject_tokens: 2500,
        memory_refresh_enabled: false,
        memory_refresh: None,
        tool_config,
        cron_service: None,
        runtime_lifecycle: None,
        pipeline_factory: None,
        hook_executor: None,
        lane_routing: None,
        voice: crate::config::VoiceConfig::default(),
    })
}

pub(crate) async fn state_with_profile_llm(
    data_dir: &std::path::Path,
    profile_id: &str,
    llm: Arc<dyn octos_llm::LlmProvider>,
) -> (Arc<AppState>, Arc<crate::runtime::ProfileRuntime>) {
    state_with_profile_llm_and_sandbox(
        data_dir,
        profile_id,
        llm,
        octos_agent::SandboxConfig::default(),
    )
    .await
}

pub(crate) async fn state_with_profile_llm_and_sandbox(
    data_dir: &std::path::Path,
    profile_id: &str,
    llm: Arc<dyn octos_llm::LlmProvider>,
    sandbox: octos_agent::SandboxConfig,
) -> (Arc<AppState>, Arc<crate::runtime::ProfileRuntime>) {
    std::fs::create_dir_all(data_dir).expect("data dir");

    let sessions = Arc::new(tokio::sync::Mutex::new(
        octos_bus::SessionManager::open(data_dir).expect("session manager"),
    ));

    let profile_data_dir = data_dir.join("profiles").join(profile_id).join("data");
    let profile_runtime =
        make_m11e_profile_with_llm_and_sandbox(profile_id, &profile_data_dir, llm, sandbox).await;

    let mut profiles = HashMap::new();
    profiles.insert(profile_id.to_string(), profile_runtime.clone());

    let state = Arc::new(AppState {
        profiles,
        sessions: Some(sessions),
        ..AppState::empty_for_tests()
    });

    (state, profile_runtime)
}

// ---------------------------------------------------------------------------
// OctoSense ADR 0004 G1 (kernel half): an external client's approvals go only
// to that client (UPCR-2026-036).
// ---------------------------------------------------------------------------

pub(crate) fn test_active_turn(turn_id: TurnId, abort: AbortHandle) -> ActiveTurn {
    let (tx, _rx) = mpsc::channel::<()>(1);
    ActiveTurn {
        dispatched_prompt: None,
        turn_cancel: tokio_util::sync::CancellationToken::new(),
        owner: None,
        turn_id,
        profile_id: MAIN_PROFILE_ID.to_owned(),
        state: Arc::new(TokioMutex::new(TurnState::Active)),
        interrupt_tx: Arc::new(TokioMutex::new(Some(tx))),
        steer: None,
        abort,
    }
}

pub(crate) fn sample_pending_question(
    session_id: SessionKey,
    question_id: QuestionId,
    turn_id: TurnId,
) -> UserQuestionRequestedEvent {
    use octos_core::ui_protocol::{UserQuestion, UserQuestionOption};
    UserQuestionRequestedEvent::new(
        session_id,
        question_id,
        turn_id,
        "Pick a framework",
        "Which framework should I scaffold?",
        vec![UserQuestion {
            header: "Framework".into(),
            question: "Which framework?".into(),
            options: vec![
                UserQuestionOption {
                    label: "axum".into(),
                    description: "tower-based".into(),
                },
                UserQuestionOption {
                    label: "actix".into(),
                    description: "actor-based".into(),
                },
            ],
            multi_select: false,
            allow_free_text: true,
        }],
    )
}

/// Await the RPC response frame carrying `id`, skipping interleaved
/// notification frames (turn/started, projection envelopes, ...).
pub(crate) async fn recv_rpc_response_with_id(
    rx: &mut mpsc::Receiver<axum::extract::ws::Message>,
    id: &str,
) -> Value {
    for _ in 0..256 {
        let frame = recv_rpc_json(rx).await;
        if frame.get("id").and_then(Value::as_str) == Some(id) {
            return frame;
        }
    }
    panic!("no rpc response with id {id} within 256 frames");
}

/// A fresh system-like session per call: the approval side tables are
/// process-wide, so tests never share a session key.
pub(crate) fn g1_system_session() -> SessionKey {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    SessionKey(format!(
        "local:g1-octosense-{}#system",
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))
}

/// Run one approval request of a turn on `ws` in the background; return the
/// task and the approval once it is pending.
pub(crate) async fn g1_raise_approval(
    ws: &WsConnection,
    ledger: &Arc<UiProtocolLedger>,
    contracts: &Arc<UiProtocolContractStores>,
    state: &Arc<AppState>,
    session_id: &SessionKey,
) -> (tokio::task::JoinHandle<ToolApprovalDecision>, ApprovalId) {
    let turn_id = TurnId::new();
    let requester = UiProtocolApprovalRequester {
        ws: ws.clone(),
        ledger: Arc::clone(ledger),
        contracts: Arc::clone(contracts),
        state: Arc::clone(state),
        // Not a `peer-` topic, so the #1842 park gate never resolves.
        peers_root: std::path::PathBuf::from("/nonexistent/peers"),
        session_id: session_id.clone(),
        turn_id: turn_id.clone(),
        features: ConnectionUiFeatures::default(),
    };
    let task = tokio::spawn(async move {
        <UiProtocolApprovalRequester as octos_agent::ToolApprovalRequester>::request_approval(
            &requester,
            ToolApprovalRequest {
                tool_id: "shell-1".into(),
                tool_name: "shell".into(),
                title: "Run command".into(),
                body: "ls".into(),
                command: Some("ls".into()),
                cwd: None,
                once_only: false,
                host_tool: None,
            },
        )
        .await
    });
    for _ in 0..500 {
        if let Some(pending) = contracts
            .approvals
            .pending_for_session(session_id)
            .into_iter()
            .find(|approval| approval.turn_id == turn_id)
        {
            return (task, pending.approval_id);
        }
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }
    panic!("the approval never became pending");
}

/// The approval ids `session/open` lists as pending for `connection`.
pub(crate) async fn g1_opened_pending(
    state: &Arc<AppState>,
    ledger: &UiProtocolLedger,
    contracts: &UiProtocolContractStores,
    connection: ConnectionId,
    session_id: &SessionKey,
) -> Vec<ApprovalId> {
    open_session_result(
        state,
        ledger,
        &contracts.approvals,
        &contracts.user_questions,
        connection,
        None,
        None,
        ConnectionUiFeatures::default(),
        test_open_params(session_id.clone(), None),
    )
    .await
    .expect("session/open")
    .pending_approvals
    .into_iter()
    .map(|approval| approval.approval_id)
    .collect()
}

pub(crate) async fn g1_state(temp: &std::path::Path, session_id: &SessionKey) -> Arc<AppState> {
    let state = state_with_sessions(temp);
    let sessions = state.sessions.as_ref().expect("sessions");
    sessions.lock().await.get_or_create(session_id).await;
    state
}

pub(crate) fn g1_question_features() -> ConnectionUiFeatures {
    ConnectionUiFeatures {
        workspace_session_refs_v1: false,
        user_question_v1: true,
        ..ConnectionUiFeatures::stdio_defaults()
    }
}

/// Run one `ask_user_question` of a turn on `ws` in the background; return
/// the task and the question once it is pending.
pub(crate) async fn g1_ask_question(
    ws: &WsConnection,
    ledger: &Arc<UiProtocolLedger>,
    contracts: &Arc<UiProtocolContractStores>,
    state: &Arc<AppState>,
    session_id: &SessionKey,
) -> (
    tokio::task::JoinHandle<octos_agent::UserQuestionOutcome>,
    QuestionId,
) {
    use octos_agent::UserQuestionRequester as _;
    let turn_id = TurnId::new();
    let requester = SessionUserQuestionRequester {
        ws: ws.clone(),
        ledger: Arc::clone(ledger),
        contracts: Arc::clone(contracts),
        state: Arc::clone(state),
        peers_root: std::path::PathBuf::from("/nonexistent/peers"),
        session_id: session_id.clone(),
        turn_id: turn_id.clone(),
    };
    let task = tokio::spawn(async move {
        requester
            .request_user_question(octos_agent::UserQuestionRequest {
                questions: sample_pending_question(
                    SessionKey("unused".into()),
                    QuestionId::new(),
                    TurnId::new(),
                )
                .questions,
                title: "Pick a framework".to_owned(),
                body: "Which framework?".to_owned(),
            })
            .await
    });
    for _ in 0..500 {
        if let Some(pending) = contracts
            .user_questions
            .pending_for_session(session_id)
            .into_iter()
            .find(|question| question.turn_id == turn_id)
        {
            return (task, pending.question_id);
        }
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }
    panic!("the question never became pending");
}

/// The question ids `session/open` lists as pending for `connection`.
pub(crate) async fn g1_opened_questions(
    state: &Arc<AppState>,
    ledger: &UiProtocolLedger,
    contracts: &UiProtocolContractStores,
    connection: ConnectionId,
    session_id: &SessionKey,
) -> Vec<QuestionId> {
    open_session_result(
        state,
        ledger,
        &contracts.approvals,
        &contracts.user_questions,
        connection,
        None,
        None,
        g1_question_features(),
        test_open_params(session_id.clone(), None),
    )
    .await
    .expect("session/open")
    .pending_questions
    .into_iter()
    .map(|question| question.question_id)
    .collect()
}

pub(crate) fn g1_answer() -> Vec<UserQuestionAnswer> {
    vec![UserQuestionAnswer {
        selected_labels: vec!["axum".into()],
        free_text: None,
    }]
}

/// A stub model's reply: run `tool_calls`, or else end the turn with
/// `content`.
pub(crate) fn stub_reply(
    content: Option<&str>,
    tool_calls: Vec<octos_core::ToolCall>,
) -> octos_llm::ChatResponse {
    octos_llm::ChatResponse {
        content: content.map(str::to_owned),
        reasoning_content: None,
        stop_reason: if tool_calls.is_empty() {
            octos_llm::StopReason::EndTurn
        } else {
            octos_llm::StopReason::ToolUse
        },
        tool_calls,
        usage: octos_llm::TokenUsage {
            input_tokens: 1,
            output_tokens: 1,
            ..Default::default()
        },
        provider_index: None,
    }
}

/// A stub model's call `id` of the tool `name`.
pub(crate) fn stub_tool_call(id: &str, name: &str, arguments: Value) -> octos_core::ToolCall {
    octos_core::ToolCall {
        id: id.into(),
        name: name.into(),
        arguments,
        metadata: None,
    }
}
