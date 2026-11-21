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
    // These fixtures model profiles that existed at server startup, so they
    // keep the pre-upgrade per-profile chat root.
    crate::runtime::chat_workspace::register_existing_profile(data_dir)
        .expect("existing-profile chat root");
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

/// Bootstrap `session_id` in `cwd` with write access, as a turn would.
pub(crate) async fn writable_runtime(
    profile: &Arc<crate::runtime::ProfileRuntime>,
    session_id: &SessionKey,
    cwd: &Path,
) -> eyre::Result<Arc<crate::runtime::SessionRuntime>> {
    sandboxed_runtime(profile, session_id, cwd, None).await
}

/// What `runtime`'s shell says to each of `commands` in turn, whether it ran
/// or refused.
pub(crate) async fn shell_says(
    runtime: &crate::runtime::SessionRuntime,
    commands: &[String],
) -> String {
    let mut said = String::new();
    for command in commands {
        let ran = run_tool(runtime, "shell", json!({ "command": command })).await;
        said += &ran.unwrap_or_else(|refused| refused);
    }
    said
}

/// A workspace snapshot manager for `workspace`, kept in `profile`'s data
/// directory.
pub(crate) fn snapshot_manager(
    profile: &crate::runtime::ProfileRuntime,
    workspace: &Path,
) -> octos_agent::SnapshotManager {
    let root = profile.data_dir.join("snapshots");
    octos_agent::SnapshotManager::new(root, workspace, 10).expect("a git binary")
}

/// The files of snapshot `id`, one per line.
pub(crate) fn snapshot_files(
    manager: &octos_agent::SnapshotManager,
    id: &octos_agent::SnapshotId,
) -> String {
    let listed = std::process::Command::new("git")
        .arg("--git-dir")
        .arg(manager.git_dir())
        .args(["ls-tree", "-r", "--name-only", id.as_str()])
        .output()
        .expect("git");
    String::from_utf8_lossy(&listed.stdout).into_owned()
}

/// [`writable_runtime`] with no sandbox at all.
pub(crate) async fn unconfined_runtime(
    profile: &Arc<crate::runtime::ProfileRuntime>,
    session_id: &SessionKey,
    cwd: &Path,
) -> eyre::Result<Arc<crate::runtime::SessionRuntime>> {
    let none = octos_agent::SandboxConfig {
        enabled: false,
        ..Default::default()
    };
    sandboxed_runtime(profile, session_id, cwd, Some(none)).await
}

/// [`writable_runtime`], in `sandbox` instead of the profile's when given.
async fn sandboxed_runtime(
    profile: &Arc<crate::runtime::ProfileRuntime>,
    session_id: &SessionKey,
    cwd: &Path,
    sandbox: Option<octos_agent::SandboxConfig>,
) -> eyre::Result<Arc<crate::runtime::SessionRuntime>> {
    // Earlier tests may have dropped TempDirs after registering their chats
    // roots in this process-global list. Drop those stale fixture roots so a
    // parallel test's cleanup cannot race this runtime's fail-closed check;
    // production registers each live root before use.
    let reserved = crate::runtime::chat_workspace::reserved_folders();
    for root in reserved.folders().into_iter().filter(|root| !root.exists()) {
        reserved.remove(&root);
    }
    let write = octos_agent::EffectivePermissions::workspace_write();
    let cwd = Some(cwd.to_path_buf());
    crate::runtime::SessionRuntime::bootstrap_with_permissions_and_sandbox(
        profile,
        session_id.clone(),
        cwd,
        write,
        sandbox,
        true,
    )
    .await
}

/// Run a tool as a turn of `runtime` would: its output, or why it failed.
pub(crate) async fn run_tool(
    runtime: &crate::runtime::SessionRuntime,
    name: &str,
    args: Value,
) -> Result<String, String> {
    let ctx = octos_agent::tools::ToolContext {
        session_scope: runtime.agent.session_scope().cloned(),
        ..octos_agent::tools::ToolContext::zero()
    };
    match runtime.tools.execute_with_context(&ctx, name, &args).await {
        Ok(result) if result.success => Ok(result.output),
        Ok(result) => Err(result.output),
        Err(error) => Err(error.to_string()),
    }
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

/// `session/files/read` of the workspace-relative `path` in `session`'s
/// workspace, as `identity`, with the `/` separators the protocol takes on
/// every platform.
pub(crate) async fn read_session_file(
    state: &Arc<AppState>,
    identity: &crate::api::router::AuthIdentity,
    session: &SessionKey,
    path: &Path,
) -> Result<octos_core::ui_protocol::SessionFilesReadResult, RpcError> {
    let params = SessionFilesReadParams {
        session_id: session.0.clone(),
        path: path
            .iter()
            .map(|part| part.to_string_lossy())
            .collect::<Vec<_>>()
            .join("/"),
        offset: None,
        length: None,
    };
    crate::api::handlers::session_file_read(state, &HeaderMap::new(), Some(identity), params).await
}

/// `session/files.list` of `session`'s whole workspace tree, as `identity`:
/// the listing's JSON text.
pub(crate) async fn list_session_files(
    state: &Arc<AppState>,
    identity: &crate::api::router::AuthIdentity,
    session: &SessionKey,
) -> String {
    let params = SessionFilesListParams {
        session_id: session.0.clone(),
        prefix: None,
        depth: Some(8),
        limit: None,
        cursor: None,
    };
    let service = crate::api::handlers::SessionQueryService::new(Arc::clone(state));
    let listed = service
        .files(&HeaderMap::new(), Some(identity), &params)
        .await;
    listed.expect("session/files.list").to_string()
}

/// `GET /api/files?path=…`, naming `session` when given, as `identity`: the
/// body, or the status when the request fails.
pub(crate) async fn download_file(
    state: &Arc<AppState>,
    identity: &crate::api::router::AuthIdentity,
    session: Option<&SessionKey>,
    path: &str,
) -> Result<Vec<u8>, axum::http::StatusCode> {
    let mut query = HashMap::from([("path".to_owned(), path.to_owned())]);
    query.extend(session.map(|key| ("session".to_owned(), key.0.clone())));
    let response = crate::api::handlers::serve_file_by_query(
        State(Arc::clone(state)),
        HeaderMap::new(),
        Some(axum::Extension(identity.clone())),
        axum::extract::Query(query),
    )
    .await;
    if !response.status().is_success() {
        return Err(response.status());
    }
    let body = axum::body::to_bytes(response.into_body(), usize::MAX).await;
    Ok(body.expect("response body").to_vec())
}

/// `GET /api/files/list?dirs=…` as `identity`: the listing's JSON text.
pub(crate) async fn list_profile_files(
    state: &Arc<AppState>,
    identity: &crate::api::router::AuthIdentity,
    dirs: &str,
) -> String {
    let query = HashMap::from([("dirs".to_owned(), dirs.to_owned())]);
    let response = crate::api::handlers::list_content_files(
        State(Arc::clone(state)),
        HeaderMap::new(),
        Some(axum::Extension(identity.clone())),
        axum::extract::Query(query),
    )
    .await;
    let body = axum::body::to_bytes(response.into_body(), usize::MAX).await;
    String::from_utf8(body.expect("/api/files/list").to_vec()).expect("JSON text")
}

/// A provider no test here calls: nothing listens on its port.
pub(crate) fn unused_llm() -> Arc<dyn octos_llm::LlmProvider> {
    Arc::new(
        octos_llm::openai::OpenAIProvider::new("", "unused").with_base_url("http://127.0.0.1:9"),
    )
}

/// The profile the chat workspace tests run as.
pub(crate) const CHAT_PROFILE: &str = "t0596";

pub(crate) fn chat_key(chat: &str) -> SessionKey {
    SessionKey::with_profile(CHAT_PROFILE, "api", chat)
}

pub(crate) async fn create_chat(
    state: &Arc<AppState>,
    session_id: SessionKey,
) -> Result<PathBuf, Value> {
    let params = ChatCreateParams {
        session_id,
        profile_id: CHAT_PROFILE.into(),
    };
    session_chat_create_result(state, None, params)
        .await
        .map(|created| PathBuf::from(created.workspace_root))
        .map_err(|error| error.data.unwrap()["kind"].clone())
}

/// `secret.txt` ("theirs") in a chat of a second profile, `other`, started
/// with its data directory under `root` beside the chat tests' profile,
/// spelled with `..` as a custom `data_dir` may be.
pub(crate) async fn other_profile_chat_file(root: &Path, other: &str) -> PathBuf {
    let data_dir = root.join("profiles/../profiles").join(other).join("data");
    let sandbox = octos_agent::SandboxConfig::default();
    make_m11e_profile_with_llm_and_sandbox(other, &data_dir, unused_llm(), sandbox).await;
    let chat = SessionKey::with_profile(other, "api", "theirs");
    let chat = crate::runtime::chat_workspace::create(&data_dir, other, &chat).expect("chat");
    let file = chat.root.join("secret.txt");
    std::fs::write(&file, "theirs").expect("the other profile's chat file");
    file
}

/// Record `key` in its profile's session store, as its first open does.
pub(crate) async fn record_session(state: &Arc<AppState>, key: SessionKey) -> SessionKey {
    let sessions = resolve_sessions_for_lookup(state, None, None, &key).await;
    let sessions = sessions.expect("the session's store");
    let mut sessions = sessions.lock().await;
    sessions
        .ensure_session_record_in_mode(&key, None)
        .await
        .expect("a session record");
    key
}

/// The session delete `session/delete` runs, of `id` as the admin: its status.
pub(crate) async fn delete_session_as_admin(
    state: &Arc<AppState>,
    id: &SessionKey,
) -> axum::http::StatusCode {
    let response = crate::api::handlers::delete_session(
        State(Arc::clone(state)),
        HeaderMap::new(),
        Some(axum::Extension(crate::api::router::AuthIdentity::Admin)),
        axum::extract::Path(id.to_string()),
    )
    .await;
    response.status()
}

/// The `pf/` handle of `file` in `profile`'s data directory.
pub(crate) fn profile_handle(profile: &crate::runtime::ProfileRuntime, file: &Path) -> String {
    octos_core::file_handle::encode_profile_file_handle(&profile.data_dir, file).expect("handle")
}

/// The `data.kind` of an RPC error.
pub(crate) fn error_kind(error: RpcError) -> Value {
    error.data.unwrap()["kind"].clone()
}

/// Register `profile` in a profile store under `root` on `state`, as a
/// multi-profile server has it, so profile-scoped file access resolves the
/// profile's data folder. `state` must not be shared yet.
pub(crate) fn with_profile_store(
    state: &mut Arc<AppState>,
    root: &Path,
    profile: &crate::runtime::ProfileRuntime,
) {
    let store = crate::profiles::ProfileStore::open_unified(root).expect("profile store");
    let now = chrono::Utc::now();
    let record = crate::profiles::UserProfile {
        id: profile.profile_id.clone(),
        name: profile.profile_id.clone(),
        enabled: true,
        data_dir: Some(profile.data_dir.display().to_string()),
        parent_id: None,
        public_subdomain: None,
        config: Default::default(),
        created_at: now,
        updated_at: now,
    };
    store.save(&record).expect("profile record");
    Arc::get_mut(state).expect("unshared state").profile_store = Some(Arc::new(store));
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
    ws: &(impl TurnConnection + Clone + 'static),
    ledger: &Arc<UiProtocolLedger>,
    contracts: &Arc<UiProtocolContractStores>,
    state: &Arc<AppState>,
    session_id: &SessionKey,
) -> (tokio::task::JoinHandle<ToolApprovalDecision>, ApprovalId) {
    let turn_id = TurnId::new();
    let requester = UiProtocolApprovalRequester {
        ws: Arc::new(ws.clone()),
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
        ws: Arc::new(ws.clone()),
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
