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

/// T-0636: a `TurnCaller` with no socket. A durable event is appended to the
/// ledger the way the socket path appends it, and recorded; a turn is
/// recorded by its prompt instead of run.
pub(crate) struct RecordingCaller {
    id: ConnectionId,
    external: bool,
    ingress_generation: Option<u64>,
    pub(crate) emitted: StdMutex<Vec<UiNotification>>,
    pub(crate) runs: Arc<StdMutex<Vec<String>>>,
}

pub(crate) struct RecordingRunner(Arc<StdMutex<Vec<String>>>);

impl RecordingCaller {
    pub(crate) fn new(external: bool, ingress_generation: Option<u64>) -> Self {
        Self {
            id: ConnectionId::next(),
            external,
            ingress_generation,
            emitted: StdMutex::default(),
            runs: Arc::default(),
        }
    }
}

impl TurnCaller for RecordingCaller {
    type Runner = RecordingRunner;

    fn connection_id(&self) -> ConnectionId {
        self.id
    }

    fn is_external(&self) -> bool {
        self.external
    }

    fn ingress_generation(&self) -> Option<u64> {
        self.ingress_generation
    }

    fn emit_durable(
        &self,
        ledger: &UiProtocolLedger,
        notification: UiNotification,
    ) -> Result<(), SendError> {
        ledger
            .try_append_notification_from(notification.clone(), self.id)
            .map_err(|error| SendError::LifecycleFailure(error.to_string()))?;
        self.emitted.lock().unwrap().push(notification);
        Ok(())
    }

    fn turn_runner(&self) -> RecordingRunner {
        RecordingRunner(self.runs.clone())
    }
}

impl session_turn_service::TurnRunner for RecordingRunner {
    async fn run(self, run: session_turn_service::TurnRun) {
        self.0.lock().unwrap().push(run.prompt);
    }
}

/// The socket twin of a [`RecordingCaller`] with the same flags (a
/// work-secret ingress when it has a grant generation); its frames go to the
/// returned receiver.
pub(crate) fn ws_caller(
    external: bool,
    ingress_generation: Option<u64>,
) -> (WsConnection, mpsc::Receiver<WsMessage>) {
    let (tx, rx) = mpsc::channel(64);
    let ws = WsConnection {
        session_ingress: ingress_generation.is_some(),
        ingress_generation,
        ..WsConnection::new(tx)
    };
    ws.set_external(external);
    (ws, rx)
}

/// The socket's reply to `id` so far, as `{"result": ..}` or `{"error": ..}`.
pub(crate) fn ws_reply(rx: &mut mpsc::Receiver<WsMessage>, id: &str) -> Value {
    let mut reply = None;
    while let Ok(message) = rx.try_recv() {
        let WsMessage::Text(text) = message else {
            continue;
        };
        let frame: Value = serde_json::from_str(&text).expect("json frame");
        if frame["id"] == id {
            reply = Some(frame);
        }
    }
    let mut reply = reply.unwrap_or_else(|| panic!("no reply to {id}"));
    let reply_fields = reply.as_object_mut().expect("reply object");
    reply_fields.retain(|key, _| key == "result" || key == "error");
    reply
}

/// The same `{"result": ..}` / `{"error": ..}` shape for a service outcome.
pub(crate) fn service_reply<T: Serialize>(outcome: Result<T, RpcError>) -> Value {
    match outcome {
        Ok(result) => json!({ "result": result }),
        Err(error) => json!({ "error": error }),
    }
}

pub(crate) fn refused_error(refused: session_turn_service::Refused) -> RpcError {
    match refused {
        session_turn_service::Refused::Scope(error)
        | session_turn_service::Refused::Error(error) => error,
    }
}

/// The session's ledger rows.
pub(crate) fn ledger_rows(ledger: &UiProtocolLedger, session: &SessionKey) -> Value {
    let start = UiCursor {
        stream: session.0.clone(),
        seq: 0,
    };
    let rows = ledger
        .replay_after(session, Some(&start))
        .expect("ledger replay");
    Value::Array(rows.into_iter().map(|row| json!(row.event)).collect())
}

/// `value` as two fixtures can compare it: every `(from, to)` substituted
/// in its JSON text (their own paths and ids), digests of them (64 hex
/// digits) hidden, and the fields that differ by construction dropped
/// (timestamps; the transcript hash, which covers the session id).
pub(crate) fn same_as(value: &Value, substitutions: &[(&str, &str)]) -> Value {
    fn drop_volatile(value: &mut Value) {
        match value {
            Value::Object(fields) => {
                fields.retain(|key, _| !key.ends_with("_at") && key != "transcript_hash");
                fields.values_mut().for_each(drop_volatile);
            }
            Value::Array(items) => items.iter_mut().for_each(drop_volatile),
            _ => {}
        }
    }
    let mut text = value.to_string();
    for (from, to) in substitutions {
        text = text.replace(from, to);
    }
    let mut hidden = String::with_capacity(text.len());
    let mut hex = String::new();
    for c in text.chars().chain(std::iter::once(' ')) {
        if c.is_ascii_hexdigit() {
            hex.push(c);
            continue;
        }
        hidden.push_str(if hex.len() == 64 { "<digest>" } else { &hex });
        hex.clear();
        hidden.push(c);
    }
    hidden.pop();
    let mut value = serde_json::from_str(&hidden).expect("json");
    drop_volatile(&mut value);
    value
}

/// The files under `root`, relative and sorted.
pub(crate) fn files_under(root: &std::path::Path) -> Vec<String> {
    let mut files = Vec::new();
    let mut dirs = vec![root.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                dirs.push(path);
            } else {
                files.push(
                    path.strip_prefix(root)
                        .expect("under root")
                        .display()
                        .to_string(),
                );
            }
        }
    }
    files.sort();
    files
}

/// A registry entry for a running turn whose task acknowledges an interrupt
/// the way `run_standalone_turn` does: terminal `interrupted`, then the ack.
pub(crate) fn acking_turn(turn_id: TurnId, owner: Option<ConnectionId>) -> ActiveTurn {
    let (interrupt_tx, mut interrupt_rx) = mpsc::channel::<()>(1);
    let state = Arc::new(TokioMutex::new(TurnState::Active));
    let task_state = state.clone();
    let task = tokio::spawn(async move {
        if interrupt_rx.recv().await.is_some() {
            let mut state = task_state.lock().await;
            let interrupted = TurnState::Terminal(TerminalReason::Interrupted);
            if let TurnState::Interrupting { ack, .. } = std::mem::replace(&mut *state, interrupted)
            {
                let _ = ack.send(());
            }
        }
    });
    ActiveTurn {
        dispatched_prompt: None,
        turn_cancel: tokio_util::sync::CancellationToken::new(),
        owner,
        turn_id,
        profile_id: MAIN_PROFILE_ID.to_owned(),
        state,
        interrupt_tx: Arc::new(TokioMutex::new(Some(interrupt_tx))),
        steer: None,
        abort: task.abort_handle(),
    }
}

/// `turn/start` of `prompt` (no input item when it is empty).
pub(crate) fn turn_start_params(
    session: &SessionKey,
    turn: &TurnId,
    prompt: &str,
) -> TurnStartParams {
    TurnStartParams {
        session_id: session.clone(),
        turn_id: turn.clone(),
        input: (!prompt.is_empty())
            .then(|| InputItem::Text {
                text: prompt.into(),
            })
            .into_iter()
            .collect(),
        media: Vec::new(),
        topic: None,
        rewrite_for: None,
        reasoning_effort: None,
        tool_context: None,
        live_video: false,
        origin: None,
    }
}

/// What a turn registry holds for `session`: `none`, `active`,
/// `interrupting`, or the terminal reason.
pub(crate) async fn turn_state_label(
    active_turns: &SharedActiveTurns,
    session: &SessionKey,
) -> String {
    let registry = active_turns.lock().await;
    let Some(active) = registry.get(session) else {
        return "none".into();
    };
    match &*active.state.lock().await {
        TurnState::Active => "active".into(),
        TurnState::Interrupting { .. } => "interrupting".into(),
        TurnState::Terminal(reason) => reason.as_str().into(),
    }
}

/// A model that ends every turn at once with an empty reply.
pub(crate) struct NoopLlm;
#[async_trait::async_trait]
impl octos_llm::LlmProvider for NoopLlm {
    async fn chat(
        &self,
        _messages: &[Message],
        _tools: &[octos_llm::ToolSpec],
        _config: &octos_llm::ChatConfig,
    ) -> eyre::Result<octos_llm::ChatResponse> {
        Ok(octos_llm::ChatResponse {
            content: Some(String::new()),
            tool_calls: Vec::new(),
            stop_reason: octos_llm::StopReason::EndTurn,
            usage: Default::default(),
            reasoning_content: None,
            provider_index: None,
        })
    }
    fn model_id(&self) -> &str {
        "noop"
    }
    fn provider_name(&self) -> &str {
        "noop"
    }
}

// T-0636: the two runs of a `SessionTurnService` twin test, one through the
// WebSocket handler and one through the service with no socket.

/// What one fixture did with its requests, its own ids and paths read alike.
#[derive(Debug, PartialEq)]
pub(crate) struct Seen {
    pub(crate) replies: Vec<Value>,
    pub(crate) ledger: Value,
    pub(crate) state: Value,
}

impl Seen {
    /// The first request was answered with a result.
    pub(crate) fn answered(&self) -> bool {
        self.replies[0].get("result").is_some()
    }

    /// How many ledger rows the session has.
    pub(crate) fn rows(&self) -> Option<usize> {
        self.ledger.as_array().map(Vec::len)
    }
}

/// Process-wide side tables are keyed by session, so every run gets its own
/// chat (`topic` empty: none).
pub(crate) fn twin_session(topic: &str) -> SessionKey {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    SessionKey::with_profile_topic(MAIN_PROFILE_ID, "api", &format!("twin{n}"), topic)
}

pub(crate) async fn approval_once(
    (external, own, topic, profile, _): (bool, bool, &str, &str, Option<&str>),
    via_ws: bool,
    turn: &TurnId,
) -> Seen {
    let temp = tempfile::tempdir().expect("tempdir");
    let state = state_with_sessions(temp.path());
    let ledger = Arc::new(UiProtocolLedger::new(32));
    let contracts = Arc::new(UiProtocolContractStores::default());
    let session = twin_session(topic);
    let (ws, mut rx) = ws_caller(external, None);
    let caller = RecordingCaller::new(external, None);
    let me = if via_ws {
        ws.connection_id()
    } else {
        TurnCaller::connection_id(&caller)
    };
    let approval = ApprovalId::new();
    let raised_by = if own { me } else { ConnectionId::next() };
    let mut decision = contracts.approvals.request_runtime_owned(
        ApprovalRequestedEvent::generic(
            session.clone(),
            approval.clone(),
            turn.clone(),
            "shell",
            "Run command",
            "cargo test",
        ),
        Some(raised_by.0),
    );
    let external_owner = external.then_some(me);
    let mut replies = Vec::new();
    for id in ["respond", "again"] {
        let mut params = ApprovalRespondParams::new(
            session.clone(),
            approval.clone(),
            ApprovalDecision::Approve,
        );
        params.approval_scope = Some("approve_for_session".into());
        replies.push(if via_ws {
            handle_approval_respond(
                &ws,
                &state,
                &ledger,
                &contracts,
                Some(profile),
                external_owner,
                id.into(),
                params,
            )
            .await;
            ws_reply(&mut rx, id)
        } else {
            let service = SessionTurnService::new(&state, &ledger);
            match service
                .respond_approval(&caller, &contracts, Some(profile), external_owner, params)
                .await
            {
                Ok(responded) => {
                    let reply = json!({ "result": responded.result });
                    responded.audit(&state, &contracts).await;
                    reply
                }
                Err(refused) => service_reply::<Value>(Err(refused_error(refused))),
            }
        });
    }
    let ids = [
        (session.0.as_str(), "<session>"),
        (&approval.0.to_string(), "<approval>"),
    ];
    Seen {
        replies: replies.iter().map(|reply| same_as(reply, &ids)).collect(),
        ledger: same_as(&ledger_rows(&ledger, &session), &ids),
        state: json!({
            "decision": format!("{:?}", decision.try_recv().ok()),
            "scopes": contracts.scopes.list_for_session(&session).len(),
            // The audit log, named by the time it was written.
            "files": files_under(temp.path()).iter().map(|file| file.replace(|c: char| c.is_ascii_digit(), "")).collect::<Vec<_>>(),
        }),
    }
}

/// What a session's registry entry is when `turn/interrupt` arrives.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Entry {
    Missing,
    OtherTurn,
    Running { own: bool },
    Finished,
}

pub(crate) async fn interrupt_once(
    (entry, external, _): (Entry, bool, &str),
    via_ws: bool,
    turn: &TurnId,
) -> Seen {
    let temp = tempfile::tempdir().expect("tempdir");
    let state = state_with_sessions(temp.path());
    let ledger = Arc::new(UiProtocolLedger::new(32));
    let contracts = Arc::new(UiProtocolContractStores::default());
    let active_turns: SharedActiveTurns = Arc::default();
    let session = twin_session("");
    let (ws, mut rx) = ws_caller(external, None);
    let caller = RecordingCaller::new(external, None);
    let me = if via_ws {
        ws.connection_id()
    } else {
        TurnCaller::connection_id(&caller)
    };
    let entry = match entry {
        Entry::Missing => None,
        Entry::OtherTurn => Some(acking_turn(TurnId::new(), Some(me))),
        Entry::Running { own } => Some(acking_turn(
            turn.clone(),
            Some(if own { me } else { ConnectionId::next() }),
        )),
        Entry::Finished => Some(ActiveTurn {
            state: Arc::new(TokioMutex::new(TurnState::Terminal(
                TerminalReason::Completed,
            ))),
            ..acking_turn(turn.clone(), Some(me))
        }),
    };
    if let Some(entry) = entry {
        active_turns.lock().await.insert(session.clone(), entry);
    }
    let mut replies = Vec::new();
    for id in ["stop", "again"] {
        let params = TurnInterruptParams {
            session_id: session.clone(),
            turn_id: turn.clone(),
        };
        replies.push(if via_ws {
            handle_turn_interrupt(
                &ws,
                &state,
                &ledger,
                &active_turns,
                &contracts,
                Some(MAIN_PROFILE_ID),
                id.into(),
                params,
            )
            .await;
            ws_reply(&mut rx, id)
        } else {
            let service = SessionTurnService::new(&state, &ledger);
            service_reply(
                service
                    .interrupt(&caller, &active_turns, Some(MAIN_PROFILE_ID), params)
                    .await,
            )
        });
    }
    let ids = [(session.0.as_str(), "<session>")];
    Seen {
        replies: replies.iter().map(|reply| same_as(reply, &ids)).collect(),
        ledger: ledger_rows(&ledger, &session),
        state: json!(turn_state_label(&active_turns, &session).await),
    }
}

pub(crate) async fn start_once(
    (prompt, runtime, occupied, external, grant, profile, admitted): (
        &str,
        bool,
        bool,
        bool,
        Option<u64>,
        &str,
        bool,
    ),
    via_ws: bool,
) -> (Seen, Vec<String>) {
    // Process-wide admission tables (the build cache) are keyed by turn.
    let turn = &TurnId::new();
    let temp = tempfile::tempdir().expect("tempdir");
    let state = if runtime {
        state_with_profile_llm(temp.path(), MAIN_PROFILE_ID, Arc::new(NoopLlm))
            .await
            .0
    } else {
        state_with_sessions(temp.path())
    };
    let ledger = Arc::new(UiProtocolLedger::new(64));
    let contracts = Arc::new(UiProtocolContractStores::default());
    let active_turns: SharedActiveTurns = Arc::default();
    let connection_turns: SharedConnectionTurns = Arc::default();
    let session = twin_session("");
    let (ws, mut rx) = ws_caller(external, grant);
    let caller = RecordingCaller::new(external, grant);
    let me = if via_ws {
        ws.connection_id()
    } else {
        TurnCaller::connection_id(&caller)
    };
    let running = TurnId::new();
    if occupied {
        active_turns
            .lock()
            .await
            .insert(session.clone(), acking_turn(running.clone(), None));
    }
    let params = turn_start_params(&session, turn, prompt);
    let features = ConnectionUiFeatures::default();
    let reply = if via_ws {
        handle_turn_start(
            &ws,
            &state,
            &ledger,
            &contracts,
            &active_turns,
            &connection_turns,
            Some(profile),
            None,
            features,
            "start".into(),
            params,
        )
        .await;
        ws_reply(&mut rx, "start")
    } else {
        let start = SessionTurnService::new(&state, &ledger)
            .start_turn(
                &caller,
                &contracts,
                &active_turns,
                &connection_turns,
                Some(profile),
                None,
                features,
                params,
                None,
            )
            .await;
        match start.outcome {
            Ok(admitted) => {
                admitted.start();
                json!({ "result": { "accepted": true } })
            }
            Err(refused) => service_reply::<Value>(Err(refused_error(refused))),
        }
    };
    // The service's turn runs once, through the caller's runner, after
    // `start()`; wait for it before reading what ran.
    let ran = || caller.runs.lock().unwrap().clone();
    for _ in 0..200 {
        if via_ws || !ran().is_empty() || !admitted {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    let registry = active_turns.lock().await;
    let registered = registry
        .get(&session)
        .filter(|active| active.turn_id == *turn);
    let seen = json!({
        "turn": registered.map(|active| (active.profile_id.clone(), active.owner == Some(me))),
        "connection": connection_turns.lock().await.get(&session).map(|entry| entry.turn_id == *turn),
    });
    registry.values().for_each(|active| active.abort.abort());
    let ids = [
        (session.0.as_str(), "<session>"),
        (&turn.0.to_string(), "<turn>"),
        (&running.0.to_string(), "<running>"),
    ];
    (
        Seen {
            replies: vec![same_as(&reply, &ids)],
            ledger: Value::Null,
            state: seen,
        },
        ran(),
    )
}

pub(crate) async fn open_once(
    (profile, ingress, _): (Option<&str>, bool, bool),
    via_ws: bool,
) -> Seen {
    let temp = tempfile::tempdir().expect("tempdir");
    let state = state_with_sessions(temp.path());
    let ledger = Arc::new(UiProtocolLedger::new(32));
    let (approvals, questions) = (
        PendingApprovalStore::default(),
        PendingQuestionStore::default(),
    );
    let session = twin_session("");
    let params = test_open_params(session.clone(), None);
    let features = ConnectionUiFeatures::default();
    let reply = if via_ws {
        let (ws, mut rx) = ws_caller(false, None);
        let forwarders: SharedLiveForwarders = Arc::default();
        handle_session_open(
            &ws,
            &state,
            &ledger,
            &approvals,
            &questions,
            &forwarders,
            profile,
            profile,
            features,
            "open".into(),
            params,
            ingress,
        )
        .await;
        ws_reply(&mut rx, "open")
    } else {
        let service = SessionTurnService::new(&state, &ledger);
        let target = service.open_target(profile, &params);
        let caller = RecordingCaller::new(false, None);
        let opened = service
            .open(
                &caller, &approvals, &questions, profile, profile, features, params, ingress,
                false, &target,
            )
            .await;
        match opened {
            Ok(outcome) => json!({ "result": outcome.result }),
            Err(refused) => service_reply::<Value>(Err(refused_error(refused))),
        }
    };
    let encoded = session.0.replace(':', "%3A");
    let ids = [
        (temp.path().to_str().expect("utf-8 path"), "<tmp>"),
        (session.0.as_str(), "<session>"),
        (encoded.as_str(), "<session>"),
    ];
    Seen {
        replies: vec![same_as(&reply, &ids)],
        ledger: same_as(&ledger_rows(&ledger, &session), &ids),
        state: same_as(&json!(files_under(temp.path())), &ids),
    }
}
