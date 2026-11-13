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
