//! UPCR-2026-035 — host-registered tools of a host-owned app peer:
//! `peer/tools/register` authorization and replacement, exact tool
//! visibility per turn, routing to the host (`peer/tool/call` →
//! `peer/tool/result`), timeouts, and risk gating through the existing
//! approval path.
use super::tests::test_support::{stub_reply, stub_tool_call};
use super::*;

use crate::peers::host_tools::{apply_session_host_tools, resolve_session_host_tools};
use octos_core::ui_protocol::ApprovalRespondParams;

/// Per-test host chat id shared by the retained host-tool scenarios.
fn host_chat() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    thread_local! {
        static CHAT: String = format!("host-hp{}", NEXT.fetch_add(1, Ordering::Relaxed));
    }
    CHAT.with(Clone::clone)
}

/// How long a wait may budget for a turn to reach its next observable step
/// (the model, a `peer/tool/call` frame, an approval event, the peer's
/// `result.md`). These are arrival assertions, not latency contracts, and the
/// Windows CI shard runs this suite noticeably slower than Linux (suite level
/// ~1.5-1.7x, with individual peer-lane waits observed exceeding 60s) —
/// arrival waits of 20-60s popped there repeatedly while the identical tests
/// stayed green on Linux. Budget minutes, not milliseconds.
const TURN_ARRIVAL_BUDGET: std::time::Duration = std::time::Duration::from_secs(180);

struct Fx {
    _tmp: tempfile::TempDir,
    state: Arc<AppState>,
    runtime: Arc<crate::runtime::ProfileRuntime>,
    data_dir: PathBuf,
    apps: PathBuf,
    system: SessionKey,
}

async fn fixture() -> Fx {
    fixture_with(|_| {}).await
}

/// The "dev" profile used by both the unit fixture and the scripted e2e
/// fixture: same openai/gpt-4o-mini shape, only the env credential pair
/// differs (test key vs scripted-e2e key).
fn dev_profile(env_key: &str, env_value: &str) -> crate::profiles::UserProfile {
    crate::profiles::UserProfile {
        id: "dev".to_string(),
        name: "Dev".to_string(),
        enabled: true,
        data_dir: None,
        parent_id: None,
        public_subdomain: None,
        config: crate::profiles::ProfileConfig {
            llm: Some(crate::profiles::LlmProfileConfig {
                primary: Some(crate::profiles::LlmModelSelectionConfig {
                    family_id: Some("openai".to_string()),
                    model_id: Some("gpt-4o-mini".to_string()),
                    route: Some(crate::profiles::LlmRouteConfig {
                        route_id: None,
                        label: None,
                        base_url: None,
                        api_key_env: Some(env_key.to_string()),
                        api_type: None,
                    }),
                    ..Default::default()
                }),
                fallbacks: Vec::new(),
            }),
            env_vars: [(env_key.to_string(), env_value.to_string())]
                .into_iter()
                .collect(),
            ..Default::default()
        },
        created_at: Utc::now(),
        updated_at: Utc::now(),
    }
}

/// [`fixture`], with `setup` applied to the server state first.
async fn fixture_with(setup: impl FnOnce(&mut AppState)) -> Fx {
    let tmp = tempfile::tempdir().unwrap();
    let profile = dev_profile("HOST_TOOLS_TEST_KEY", "test-key");
    let data_dir = tmp.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();
    let runtime = crate::runtime::ProfileRuntime::bootstrap(
        &profile,
        &data_dir,
        None,
        crate::runtime::BootstrapRole::Serve,
    )
    .await
    .expect("bootstrap dev runtime");
    let mut state = AppState::empty_for_tests();
    state.profiles.insert("dev".to_string(), runtime.clone());
    setup(&mut state);
    let apps = tmp.path().join("apps");
    std::fs::create_dir_all(apps.join("news")).unwrap();
    Fx {
        state: Arc::new(state),
        runtime,
        data_dir,
        apps,
        system: SessionKey::with_profile_topic("dev", "api", &host_chat(), "system"),
        _tmp: tmp,
    }
}

fn ws_connection_for_test(capacity: usize) -> (WsConnection, mpsc::Receiver<WsMessage>) {
    let (tx, rx) = mpsc::channel(capacity);
    (WsConnection::new(tx), rx)
}

fn rpc(method: &str, params: Value) -> RpcRequest<Value> {
    RpcRequest::new("host-1".to_string(), method, params)
}

/// Stage the News app peer; returns its host token.
async fn prepare_news(fx: &Fx) -> String {
    let result = raw_peer_prepare(
        &fx.state,
        &rpc(
            APPUI_METHOD_PEER_PREPARE,
            json!({
                "brief": "You are the News app's assistant.",
                "names": ["News"],
                "cwd": fx.apps.join("news").to_string_lossy(),
                "session_id": fx.system,
                "memory_namespace": "app/news/acct-1",
                "resume": true,
            }),
        ),
        None,
    )
    .await
    .expect("stage the news peer");
    result["host_token"].as_str().unwrap().to_owned()
}

fn peer_key(fx: &Fx) -> SessionKey {
    SessionKey(format!("{}#peer-news", fx.system.base_key()))
}

fn peers_root(fx: &Fx) -> PathBuf {
    fx.data_dir.join("peers")
}

fn register(fx: &Fx, ws: &WsConnection, token: &str, extra: Value) -> Result<Value, RpcError> {
    let mut params = json!({
        "session_id": fx.system,
        "peer": "news",
        "host_token": token,
    });
    for (key, value) in extra.as_object().unwrap() {
        params[key] = value.clone();
    }
    raw_peer_tools_register(
        ws,
        &fx.state,
        &rpc(APPUI_METHOD_PEER_TOOLS_REGISTER, params),
        None,
    )
}

fn news_list() -> Value {
    json!({
        "name": "news.list",
        "description": "List the latest news items.",
        "input_schema": {"type": "object", "properties": {"topic": {"type": "string"}}},
        "output_schema": {"type": "object"},
        "risk": "read",
        "background": true,
    })
}

fn mail_send() -> Value {
    json!({
        "name": "mail.send",
        "description": "Send a drafted mail.",
        "input_schema": {"type": "object", "required": ["draft_id"]},
        "risk": "destructive",
        "background": true,
    })
}

/// The tool registry one turn of `key` would offer the model.
async fn turn_registry(fx: &Fx, key: &SessionKey, turn: &str) -> octos_agent::ToolRegistry {
    let runtime = crate::runtime::SessionRuntime::bootstrap(&fx.runtime, key.clone(), None)
        .await
        .expect("bound session");
    let mut registry = runtime.tools.snapshot_excluding(&[]);
    let resolved = resolve_session_host_tools(&peers_root(fx), key);
    // The host's own turn: driven by the connection that registered the set.
    let host = crate::peers::host_tools::host_route_connection(&peers_root(fx), "news");
    apply_session_host_tools(&mut registry, &resolved, &peers_root(fx), key, turn, host);
    registry
}

fn sorted_names(registry: &octos_agent::ToolRegistry) -> Vec<String> {
    let mut names = registry.tool_names();
    names.sort();
    names
}

fn call_ctx(id: &str) -> octos_agent::tools::ToolContext {
    octos_agent::tools::ToolContext {
        tool_id: id.to_owned(),
        ..octos_agent::tools::ToolContext::zero()
    }
}

fn frame_json(message: WsMessage) -> Value {
    let WsMessage::Text(text) = message else {
        panic!("expected a text frame");
    };
    serde_json::from_str(text.as_str()).expect("json frame")
}

/// Next notification with `method` (skips any other frame), within
/// [`TURN_ARRIVAL_BUDGET`].
async fn next_frame(rx: &mut mpsc::Receiver<WsMessage>, method: &str) -> Value {
    let deadline = std::time::Instant::now() + TURN_ARRIVAL_BUDGET;
    loop {
        let message = tokio::time::timeout(
            deadline.saturating_duration_since(std::time::Instant::now()),
            rx.recv(),
        )
        .await
        .expect("a frame in time")
        .expect("connection open");
        let frame = frame_json(message);
        if frame["method"] == method {
            return frame["params"].clone();
        }
    }
}

fn answer(fx: &Fx, token: &str, call_id: &str, reply: Value) -> Result<Value, RpcError> {
    let mut params = json!({
        "session_id": fx.system,
        "peer": "news",
        "host_token": token,
        "call_id": call_id,
    });
    for (key, value) in reply.as_object().unwrap() {
        params[key] = value.clone();
    }
    // Answered on the connection that holds the route (the one the call
    // was sent to).
    let host = crate::peers::host_tools::host_route_connection(&peers_root(fx), "news")
        .unwrap_or_default();
    raw_peer_tool_result(
        host,
        &fx.state,
        &rpc(APPUI_METHOD_PEER_TOOL_RESULT, params),
        None,
    )
}

/// A fake host: answers every `peer/tool/call` with `reply(params)`.
fn spawn_fake_host(
    fx: &Fx,
    token: String,
    mut rx: mpsc::Receiver<WsMessage>,
    reply: impl Fn(&Value) -> Value + Send + 'static,
) -> tokio::task::JoinHandle<Vec<Value>> {
    let state = fx.state.clone();
    let system = fx.system.clone();
    let peers_root = peers_root(fx);
    tokio::spawn(async move {
        let mut calls = Vec::new();
        while let Some(message) = rx.recv().await {
            let frame = frame_json(message);
            if frame["method"] != "peer/tool/call" {
                continue;
            }
            let params = frame["params"].clone();
            let mut body = json!({
                "session_id": system,
                "peer": "news",
                "host_token": token,
                "call_id": params["call_id"],
            });
            for (key, value) in reply(&params).as_object().unwrap() {
                body[key] = value.clone();
            }
            let host = crate::peers::host_tools::host_route_connection(&peers_root, "news")
                .unwrap_or_default();
            raw_peer_tool_result(
                host,
                &state,
                &rpc(APPUI_METHOD_PEER_TOOL_RESULT, body),
                None,
            )
            .expect("the kernel accepts the result");
            calls.push(params);
        }
        calls
    })
}

fn audit_rows(fx: &Fx) -> Vec<Value> {
    std::fs::read_to_string(peers_root(fx).join("news/tool_audit.jsonl"))
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[tokio::test]
async fn should_time_out_and_cancel_a_call_the_host_never_answers() {
    let fx = fixture().await;
    let token = prepare_news(&fx).await;
    let key = peer_key(&fx);
    let (ws, mut rx) = ws_connection_for_test(32);
    register(
        &fx,
        &ws,
        &token,
        json!({ "tools": [news_list()], "call_timeout_ms": 100 }),
    )
    .unwrap();

    let registry = turn_registry(&fx, &key, "turn-1").await;
    let result = registry
        .execute_with_context(&call_ctx("c1"), "news_list", &json!({}))
        .await
        .unwrap();
    assert!(
        !result.success && result.output.contains("timeout"),
        "{}",
        result.output
    );

    let call = next_frame(&mut rx, "peer/tool/call").await;
    let cancel = next_frame(&mut rx, "peer/tool/cancel").await;
    assert_eq!(cancel["call_id"], call["call_id"]);
    assert_eq!(cancel["reason"], "timeout");

    // A late answer is refused and audited.
    let late = answer(
        &fx,
        &token,
        call["call_id"].as_str().unwrap(),
        json!({"ok": true, "data": {}}),
    )
    .expect_err("late result");
    assert_eq!(late.data.unwrap()["kind"], "peer_tool_call_not_found");
    let rows = audit_rows(&fx);
    assert_eq!(rows.last().unwrap()["decision"], "late_result");
    assert_eq!(rows.last().unwrap()["call_id"], call["call_id"]);
    assert!(crate::peers::host_tools::pending_calls_for(&peers_root(&fx), "news").is_empty());

    // With no live host at all the call fails without waiting.
    crate::peers::host_tools::set_host_route(&peers_root(&fx), "news", 0, Arc::new(|_, _| false));
    let result = registry
        .execute_with_context(&call_ctx("c2"), "news_list", &json!({}))
        .await
        .unwrap();
    assert!(
        result.output.contains("host_unavailable"),
        "{}",
        result.output
    );
}

/// The approval bridge the serve turn installs for a `turn/start` on `key`:
/// approvals park on that session (a request context's is the app's own
/// conversation; the peer's own session is where the host surfaces the
/// person-absent approvals).
fn app_approver(
    fx: &Fx,
    key: &SessionKey,
    contracts: &Arc<UiProtocolContractStores>,
    turn: &TurnId,
) -> Arc<dyn octos_agent::ToolApprovalRequester> {
    let (ws, rx) = ws_connection_for_test(64);
    std::mem::forget(rx);
    Arc::new(UiProtocolApprovalRequester {
        ws,
        ledger: Arc::new(UiProtocolLedger::new(64)),
        contracts: contracts.clone(),
        state: fx.state.clone(),
        peers_root: peers_root(fx),
        session_id: key.clone(),
        turn_id: turn.clone(),
        features: ConnectionUiFeatures::default(),
    })
}

async fn wait_for_pending(
    contracts: &UiProtocolContractStores,
    key: &SessionKey,
) -> Vec<ApprovalRequestedEvent> {
    let deadline = std::time::Instant::now() + TURN_ARRIVAL_BUDGET;
    while std::time::Instant::now() < deadline {
        let pending = contracts.approvals.pending_for_session(key);
        if !pending.is_empty() {
            return pending;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("no approval was raised");
}

#[tokio::test]
async fn should_run_a_destructive_tool_only_after_the_persons_approval() {
    let fx = fixture().await;
    let token = prepare_news(&fx).await;
    let key = peer_key(&fx);
    let (ws, rx) = ws_connection_for_test(32);
    register(&fx, &ws, &token, json!({ "tools": [mail_send()] })).unwrap();
    let host = spawn_fake_host(
        &fx,
        token.clone(),
        rx,
        |_| json!({ "ok": true, "data": {"sent": true} }),
    );
    let registry = Arc::new(turn_registry(&fx, &key, "turn-1").await);
    let args = json!({"draft_id": "d-42"});

    // No interactive client (no approval bridge): refused, the host is not asked.
    let unattended = registry
        .execute_with_context(&call_ctx("c0"), "mail_send", &args)
        .await
        .unwrap();
    assert!(!unattended.success && unattended.output.contains("approval"));

    // With the app's conversation attached: an approval with the exact args.
    let contracts = Arc::new(UiProtocolContractStores::default());
    let turn = TurnId::new();
    let approver = app_approver(&fx, &key, &contracts, &turn);
    let run = {
        let registry = registry.clone();
        let args = args.clone();
        tokio::spawn(
            octos_agent::tools::TOOL_APPROVAL_CTX.scope(approver, async move {
                registry
                    .execute_with_context(&call_ctx("c1"), "mail_send", &args)
                    .await
                    .unwrap()
            }),
        )
    };
    let pending = wait_for_pending(&contracts, &key).await;
    assert_eq!(pending.len(), 1);
    let approval_id = pending[0].approval_id.clone();

    // The system agent cannot answer it (the person answers in the app),
    // even with the peer's session open on the wire.
    crate::peers::peer_wire_registry()
        .register(crate::peers::peer_wire_key("dev", "news"), key.clone());
    let refused = crate::peers::peer_respond_resolve(
        &peers_root(&fx),
        &fx.system.0,
        "dev",
        &contracts,
        &|_, _| panic!("nothing is decided"),
        octos_agent::PeerRespondRequest {
            slug: "news".into(),
            id: Some(approval_id.0.to_string()),
            decision: Some("approve".into()),
            answers: None,
        },
    )
    .expect_err("the system agent may not approve");
    assert!(refused.contains("person in the app"), "{refused}");
    assert_eq!(contracts.approvals.pending_for_session(&key).len(), 1);

    // The person approves on the peer's session: the call runs.
    contracts
        .approvals
        .respond_with_context(ApprovalRespondParams::new(
            key.clone(),
            approval_id,
            ApprovalDecision::Approve,
        ))
        .expect("the person approves");
    let result = run.await.unwrap();
    assert!(result.success, "{}", result.output);

    drop(ws);
    crate::peers::host_tools::set_host_route(&peers_root(&fx), "news", 0, Arc::new(|_, _| false));
    let calls = host.await.unwrap();
    assert_eq!(calls.len(), 1, "only the approved call reached the host");
    assert_eq!(calls[0]["args"], args);
    let decisions: Vec<Value> = audit_rows(&fx)
        .iter()
        .map(|r| r["decision"].clone())
        .collect();
    assert_eq!(
        decisions,
        [json!("approval_unavailable"), json!("approved")]
    );
}

fn news_topics_set() -> Value {
    json!({
        "name": "news.topics_set",
        "description": "Replace the followed topics.",
        "input_schema": {"type": "object", "required": ["topics"]},
        "risk": "act",
        "background": true,
    })
}

fn rpc_error_kind(message: WsMessage) -> Value {
    frame_json(message)["error"]["data"]["kind"].clone()
}

#[tokio::test]
async fn should_never_answer_or_remember_a_host_tool_approval_by_scope() {
    let fx = fixture().await;
    let token = prepare_news(&fx).await;
    let key = peer_key(&fx);
    let (ws, rx) = ws_connection_for_test(32);
    register(&fx, &ws, &token, json!({ "tools": [mail_send()] })).unwrap();
    let host = spawn_fake_host(
        &fx,
        token.clone(),
        rx,
        |_| json!({ "ok": true, "data": {} }),
    );
    let registry = Arc::new(turn_registry(&fx, &key, "turn-1").await);
    let contracts = Arc::new(UiProtocolContractStores::default());
    let turn = TurnId::new();

    // A remembered session-wide scope for this tool exists already...
    contracts.scopes.record(
        &key,
        ApprovalScopeKind::from_scope_str("approve_for_session"),
        match_key_for(
            ApprovalScopeKind::from_scope_str("approve_for_session"),
            "mail_send",
            &turn,
        ),
        ApprovalDecision::Approve,
    );
    // ...yet the call still asks the person.
    let run = {
        let registry = registry.clone();
        let approver = app_approver(&fx, &key, &contracts, &turn);
        tokio::spawn(
            octos_agent::tools::TOOL_APPROVAL_CTX.scope(approver, async move {
                registry
                    .execute_with_context(&call_ctx("c1"), "mail_send", &json!({"draft_id": "d-1"}))
                    .await
                    .unwrap()
            }),
        )
    };
    let pending = wait_for_pending(&contracts, &key).await;

    // Answering with a scope decides this call only and records no scope.
    let mut params = ApprovalRespondParams::new(
        key.clone(),
        pending[0].approval_id.clone(),
        ApprovalDecision::Approve,
    );
    params.approval_scope = Some("approve_for_tool".into());
    let fresh = UiProtocolContractStores::default();
    let outcome = contracts
        .approvals
        .respond_with_context(params.clone())
        .unwrap();
    assert!(outcome.context.as_ref().unwrap().once_only);
    assert!(!record_approval_scope(
        &fresh,
        &key,
        params.approval_scope.as_deref(),
        outcome.context.as_ref(),
        ApprovalDecision::Approve,
    ));
    assert!(
        fresh
            .scopes
            .lookup(&key, "mail_send", &TurnId::new())
            .is_none()
    );
    assert!(run.await.unwrap().success);

    // An ordinary approval still records its scope.
    let ordinary = crate::contracts::approvals::RespondedApprovalContext {
        tool_name: "shell".into(),
        turn_id: TurnId::new(),
        once_only: false,
    };
    assert!(record_approval_scope(
        &fresh,
        &key,
        Some("approve_for_tool"),
        Some(&ordinary),
        ApprovalDecision::Approve,
    ));

    drop(ws);
    crate::peers::host_tools::set_host_route(&peers_root(&fx), "news", 0, Arc::new(|_, _| false));
    assert_eq!(host.await.unwrap().len(), 1);
}

// ---------------------------------------------------------------------------
// End to end through `turn/start` → `run_standalone_turn`
// ---------------------------------------------------------------------------

/// A scripted model: on its first call it asks for `tool` with `args`, then
/// ends the turn. Records the tool roster it was offered on every call and
/// the tool result it was given.
struct ScriptedHostToolLlm {
    tool: String,
    args: Value,
    calls: std::sync::atomic::AtomicUsize,
    rosters: std::sync::Mutex<Vec<Vec<String>>>,
    tool_results: std::sync::Mutex<Vec<String>>,
}

impl ScriptedHostToolLlm {
    fn new(tool: &str, args: Value) -> Arc<Self> {
        Arc::new(Self {
            tool: tool.to_owned(),
            args,
            calls: Default::default(),
            rosters: Default::default(),
            tool_results: Default::default(),
        })
    }
}

#[async_trait::async_trait]
impl octos_llm::LlmProvider for ScriptedHostToolLlm {
    async fn chat(
        &self,
        messages: &[Message],
        tools: &[octos_llm::ToolSpec],
        _config: &octos_llm::ChatConfig,
    ) -> eyre::Result<octos_llm::ChatResponse> {
        let call = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let mut roster: Vec<String> = tools.iter().map(|t| t.name.clone()).collect();
        roster.sort();
        self.rosters.lock().unwrap().push(roster);
        for message in messages {
            if message.role == MessageRole::Tool {
                self.tool_results
                    .lock()
                    .unwrap()
                    .push(message.content.clone());
            }
        }
        if call == 0 {
            let call = stub_tool_call("call_1", &self.tool, self.args.clone());
            Ok(stub_reply(None, vec![call]))
        } else {
            Ok(stub_reply(Some("done"), Vec::new()))
        }
    }

    fn model_id(&self) -> &str {
        "scripted-host-tool"
    }

    fn provider_name(&self) -> &str {
        "stub"
    }
}

/// The e2e fixture: a profile runtime whose model is `llm`, a staged News
/// peer, and the host's connection with the tool set registered on it.
async fn e2e_fixture(llm: Arc<dyn octos_llm::LlmProvider>, tools: Value) -> E2e {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = tmp.path().join("data");
    std::fs::create_dir_all(data_dir.join("memory")).unwrap();
    // The profile's (the system agent's) private memory: no app turn, and no
    // foreign turn on an app session, may see it.
    std::fs::write(
        data_dir.join("memory/MEMORY.md"),
        "- PROFILE-PRIVATE-FACT: the owner's bank PIN hint\n",
    )
    .unwrap();
    let runtime = {
        let base = crate::runtime::ProfileRuntime::bootstrap(
            &dev_profile("HOST_TOOLS_E2E_KEY", "k"),
            &data_dir,
            None,
            crate::runtime::BootstrapRole::Serve,
        )
        .await
        .expect("bootstrap");
        // Swap in the scripted model.
        let Ok(mut runtime) = Arc::try_unwrap(base) else {
            panic!("the freshly bootstrapped runtime has one owner");
        };
        runtime.llm = llm.clone();
        Arc::new(runtime)
    };
    let mut state = AppState::empty_for_tests();
    state.profiles.insert("dev".to_string(), runtime.clone());
    state.sessions = Some(Arc::new(tokio::sync::Mutex::new(
        octos_bus::SessionManager::open(&data_dir).unwrap(),
    )));
    let state = Arc::new(state);
    let apps = tmp.path().join("apps");
    std::fs::create_dir_all(apps.join("news")).unwrap();
    let system = SessionKey::with_profile_topic("dev", "api", &host_chat(), "system");
    let prepared = raw_peer_prepare(
        &state,
        &rpc(
            APPUI_METHOD_PEER_PREPARE,
            json!({
                "brief": "You are the News app's assistant.",
                "names": ["News"],
                "cwd": apps.join("news").to_string_lossy(),
                "session_id": system,
                "memory_namespace": "app/news/acct-1",
                "resume": true,
            }),
        ),
        None,
    )
    .await
    .expect("prepare");
    let token = prepared["host_token"].as_str().unwrap().to_owned();
    let (ws, rx) = ws_connection_for_test(256);
    let mut params = json!({"session_id": system, "peer": "news", "host_token": token});
    for (key, value) in tools.as_object().unwrap() {
        params[key] = value.clone();
    }
    raw_peer_tools_register(
        &ws,
        &state,
        &rpc(APPUI_METHOD_PEER_TOOLS_REGISTER, params),
        None,
    )
    .expect("register");
    E2e {
        _tmp: tmp,
        state,
        data_dir,
        system,
        token,
        ws,
        rx: Some(rx),
    }
}

struct E2e {
    _tmp: tempfile::TempDir,
    state: Arc<AppState>,
    data_dir: PathBuf,
    system: SessionKey,
    token: String,
    ws: WsConnection,
    rx: Option<mpsc::Receiver<WsMessage>>,
}

/// Records every request's full text (system prompt and messages) and ends
/// the turn at once.
#[derive(Default)]
struct RecordingLlm {
    requests: std::sync::Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl octos_llm::LlmProvider for RecordingLlm {
    async fn chat(
        &self,
        messages: &[Message],
        _tools: &[octos_llm::ToolSpec],
        _config: &octos_llm::ChatConfig,
    ) -> eyre::Result<octos_llm::ChatResponse> {
        let text = messages
            .iter()
            .map(|m| m.content.clone())
            .collect::<Vec<_>>()
            .join("\n---\n");
        self.requests.lock().unwrap().push(text);
        Ok(stub_reply(Some("ok"), Vec::new()))
    }

    fn model_id(&self) -> &str {
        "recording"
    }

    fn provider_name(&self) -> &str {
        "stub"
    }
}

// ---------------------------------------------------------------------------
// `octos serve --host-managed` external clients (UPCR-2026-036)
// ---------------------------------------------------------------------------

fn external_ws(capacity: usize) -> (WsConnection, mpsc::Receiver<WsMessage>) {
    let (ws, rx) = ws_connection_for_test(capacity);
    ws.set_external(true);
    (ws, rx)
}

#[tokio::test]
async fn should_refuse_host_tool_registration_and_results_when_the_connection_is_external() {
    let fx = fixture().await;
    let token = prepare_news(&fx).await;
    let params = json!({
        "session_id": fx.system,
        "peer": "news",
        "host_token": token,
        "tools": [news_list()],
    });
    // The gate in front of every external call refuses both methods...
    for method in [
        APPUI_METHOD_PEER_TOOLS_REGISTER,
        APPUI_METHOD_PEER_TOOL_RESULT,
        APPUI_METHOD_PEER_INPUT_REJECT,
    ] {
        let error = super::super::host_managed::external_gate(method, &params, &HashSet::new())
            .unwrap_err();
        assert_eq!(
            error.data.unwrap()["kind"],
            super::super::host_managed::EXTERNAL_METHOD_DENIED
        );
    }
    // ...and so does the handler behind it, even with the peer's host token.
    let (ws, mut rx) = external_ws(8);
    let ledger = Arc::new(UiProtocolLedger::new(16));
    let contracts = Arc::new(UiProtocolContractStores::default());
    let active_turns: SharedActiveTurns = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
    let connection_turns: SharedConnectionTurns = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
    for (id, method) in [
        ("ext-register", APPUI_METHOD_PEER_TOOLS_REGISTER),
        ("ext-result", APPUI_METHOD_PEER_TOOL_RESULT),
        ("ext-reject", APPUI_METHOD_PEER_INPUT_REJECT),
    ] {
        let mut call_params = params.clone();
        call_params["call_id"] = json!("call-1");
        let handled = handle_raw_appui_rpc(
            &ws,
            &fx.state,
            &ledger,
            &contracts,
            &active_turns,
            &connection_turns,
            ConnectionUiFeatures::stdio_defaults(),
            None,
            id.into(),
            &RpcRequest::new(id.to_string(), method, call_params),
        )
        .await;
        assert!(handled);
        assert_eq!(
            rpc_error_kind(rx.recv().await.unwrap()),
            super::super::host_managed::EXTERNAL_METHOD_DENIED
        );
    }
    assert!(matches!(
        crate::peers::host_tools::read_tool_set(&peers_root(&fx), "news"),
        crate::peers::host_tools::StoredToolSet::None
    ));
    assert_eq!(
        crate::peers::host_tools::host_route_connection(&peers_root(&fx), "news"),
        None,
        "an external connection never becomes the peer's host"
    );
}

#[tokio::test]
async fn should_never_give_an_external_turn_a_host_routed_tool_when_one_is_registered() {
    let fx = fixture().await;
    let token = prepare_news(&fx).await;
    let key = peer_key(&fx);
    let (host_ws, _host_rx) = ws_connection_for_test(8);
    register(
        &fx,
        &host_ws,
        &token,
        json!({ "tools": [news_list()], "generic_tools": ["grep"] }),
    )
    .unwrap();

    // External clients cannot even name the peer's session...
    for method in ["turn/start", "session/open"] {
        assert!(
            super::super::host_managed::external_gate(
                method,
                &json!({ "session_id": key }),
                &HashSet::new(),
            )
            .is_err()
        );
    }
    // ...an external connection's turn is never the host's...
    let (ext_ws, _ext_rx) = external_ws(8);
    assert_eq!(host_tools_turn_connection(&ext_ws, false), None);
    assert_eq!(
        host_tools_turn_connection(&host_ws, false),
        Some(host_ws.connection_id.0)
    );
    let runtime = crate::runtime::SessionRuntime::bootstrap(&fx.runtime, key.clone(), None)
        .await
        .unwrap();
    let mut external = runtime.tools.snapshot_excluding(&[]);
    apply_session_host_tools(
        &mut external,
        &resolve_session_host_tools(&peers_root(&fx), &key),
        &peers_root(&fx),
        &key,
        "turn-ext",
        host_tools_turn_connection(&ext_ws, false),
    );
    assert!(external.tool_names().is_empty());

    // ...and the external filter drops host-routed tools by origin, even
    // from a registry that holds them next to allowlisted built-ins.
    let mut registry = turn_registry(&fx, &key, "turn-host").await;
    assert_eq!(sorted_names(&registry), vec!["grep", "news_list"]);
    assert_eq!(
        registry.origin("news_list"),
        Some(octos_agent::ToolOrigin::HostRouted)
    );
    super::super::host_managed::confine_external_turn_tools(&mut registry);
    assert_eq!(sorted_names(&registry), vec!["grep"]);
}

#[tokio::test]
async fn should_answer_a_host_tool_approval_only_on_its_connection_when_turn_ids_collide() {
    let fx = fixture().await;
    let token = prepare_news(&fx).await;
    let key = peer_key(&fx);
    let (host_ws, _host_rx) = ws_connection_for_test(64);
    register(&fx, &host_ws, &token, json!({ "tools": [mail_send()] })).unwrap();
    let ledger = Arc::new(UiProtocolLedger::new(64));
    let contracts = Arc::new(UiProtocolContractStores::default());
    let respond = |session: &SessionKey, approval_id: &ApprovalId| {
        ApprovalRespondParams::new(
            session.clone(),
            approval_id.clone(),
            ApprovalDecision::Approve,
        )
    };
    // A client-chosen turn id another connection also claims.
    let turn = TurnId::new();

    // A host-routed approval whose side-table entry is gone (evicted): the
    // ownership recorded on the approval itself still decides.
    let approval_id = ApprovalId::new();
    let _rx = contracts.approvals.request_runtime_entry(
        ApprovalRequestedEvent::generic(
            key.clone(),
            approval_id.clone(),
            turn.clone(),
            "mail_send",
            "Send mail",
            "draft d",
        ),
        Some(host_ws.connection_id.0),
        true,
        Some(crate::peers::host_tools::route_key(
            &peers_root(&fx),
            "news",
        )),
    );
    assert!(crate::peers::host_tools::host_approval_visible(
        &approval_id.0.to_string(),
        u64::MAX
    ));

    // An external client that used the same turn id: refused.
    let (ext_ws, mut ext_rx) = external_ws(8);
    handle_approval_respond(
        &ext_ws,
        &fx.state,
        &ledger,
        &contracts,
        None,
        Some(ext_ws.connection_id()),
        "e1".into(),
        respond(&key, &approval_id),
    )
    .await;
    assert_eq!(
        rpc_error_kind(ext_rx.recv().await.unwrap()),
        super::super::host_managed::HOST_OWNED_PEER_ANSWER_DENIED
    );
    // Another ordinary connection: refused.
    let (other_ws, mut other_rx) = ws_connection_for_test(8);
    handle_approval_respond(
        &other_ws,
        &fx.state,
        &ledger,
        &contracts,
        None,
        None,
        "o1".into(),
        respond(&key, &approval_id),
    )
    .await;
    assert_eq!(
        rpc_error_kind(other_rx.recv().await.unwrap()),
        "peer_host_connection_only"
    );
    assert_eq!(contracts.approvals.pending_for_session(&key).len(), 1);

    // An ordinary approval on a shared session, raised by the host's turn
    // with the same turn id: an external client cannot answer it either,
    // because it was raised on another connection.
    let shared = SessionKey("dev:api:host#shared".into());
    let shared_id = ApprovalId::new();
    let _shared_rx = contracts.approvals.request_runtime_owned(
        ApprovalRequestedEvent::generic(
            shared.clone(),
            shared_id.clone(),
            turn.clone(),
            "shell",
            "Run",
            "ls",
        ),
        Some(host_ws.connection_id.0),
    );
    handle_approval_respond(
        &ext_ws,
        &fx.state,
        &ledger,
        &contracts,
        None,
        Some(ext_ws.connection_id()),
        "e2".into(),
        respond(&shared, &shared_id),
    )
    .await;
    assert_eq!(
        rpc_error_kind(ext_rx.recv().await.unwrap()),
        super::super::host_managed::EXTERNAL_TURN_DENIED
    );
    assert_eq!(contracts.approvals.pending_for_session(&shared).len(), 1);

    // The host connection answers the host-routed one.
    handle_approval_respond(
        &host_ws,
        &fx.state,
        &ledger,
        &contracts,
        None,
        None,
        "h1".into(),
        respond(&key, &approval_id),
    )
    .await;
    assert!(contracts.approvals.pending_for_session(&key).is_empty());
}

// ---------------------------------------------------------------------------
// Registration adds app tools; the system agent's input goes to the host
// ---------------------------------------------------------------------------

fn send_input_request(message: &str, occurrence: &str) -> octos_agent::PeerSendInputRequest {
    octos_agent::PeerSendInputRequest {
        slug: "news".into(),
        message: message.into(),
        occurrence_id: occurrence.into(),
    }
}

// ---------------------------------------------------------------------------
// Cross-app tools: each tool names its owning app, each call its caller
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Round 11: interrupt, host SESSION tool sets, visibility, attended input,
// host drop, write-side confinement
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn should_cancel_an_in_flight_host_call_when_the_turn_is_interrupted() {
    let llm = ScriptedHostToolLlm::new("news_topics_set", json!({"topics": ["rust"]}));
    let mut e = e2e_fixture(llm.clone(), json!({ "tools": [news_topics_set()] })).await;
    let session = SessionKey(format!("{}#peer-news", e.system.base_key()));
    let mut rx = e.rx.take().unwrap();
    let ledger = Arc::new(UiProtocolLedger::new(256));
    let contracts = Arc::new(UiProtocolContractStores::default());
    let active_turns: SharedActiveTurns = Arc::new(TokioMutex::new(HashMap::new()));
    let connection_turns: SharedConnectionTurns = Arc::new(TokioMutex::new(HashMap::new()));
    let turn_id = TurnId::new();
    handle_turn_start(
        &e.ws,
        &e.state,
        &ledger,
        &contracts,
        &active_turns,
        &connection_turns,
        None,
        None,
        ConnectionUiFeatures::stdio_defaults(),
        "start-1".into(),
        TurnStartParams {
            session_id: session.clone(),
            turn_id: turn_id.clone(),
            input: vec![InputItem::Text {
                text: "follow rust".into(),
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
    // The host receives the call and is working on it (never answers).
    let call = next_frame(&mut rx, "peer/tool/call").await;
    // The person interrupts the turn.
    handle_turn_interrupt(
        &e.ws,
        &e.state,
        &ledger,
        &active_turns,
        &contracts,
        None,
        "int-1".into(),
        TurnInterruptParams {
            session_id: session.clone(),
            turn_id: turn_id.clone(),
        },
    )
    .await;
    // The host is told to stop that very call. The frame is written by the
    // call's own task when it observes the cancellation; nothing here joins
    // that task, so on a loaded runner the frame can land after the turn's
    // terminal and ack are already out (#2638). The pending set empties in
    // the same synchronous block that writes the frame, so wait for that
    // first — up to 20 s, well inside the 30 s call timeout, so a timeout
    // is not what can have emptied it; if that race were ever lost anyway,
    // the `reason` assert below is the backstop.
    let peers_root = e.data_dir.join("peers");
    let mut cancelled = false;
    for _ in 0..1000 {
        if crate::peers::host_tools::pending_calls_for(&peers_root, "news").is_empty() {
            cancelled = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(
        cancelled,
        "the interrupted call is still pending after the 20 s drain wait"
    );
    let cancel = next_frame(&mut rx, "peer/tool/cancel").await;
    assert_eq!(cancel["call_id"], call["call_id"]);
    assert_eq!(cancel["reason"], "cancelled");
    // ...and the call ends as an unknown outcome (it is not resent later).
    // The row is written by the call task when it observes the cancellation,
    // so like the cancel frame it is an arrival the loaded shard can delay.
    let mut unknown = false;
    let deadline = std::time::Instant::now() + TURN_ARRIVAL_BUDGET;
    while !unknown && std::time::Instant::now() < deadline {
        let audit =
            std::fs::read_to_string(peers_root.join("news/tool_audit.jsonl")).unwrap_or_default();
        unknown = audit.contains("\"outcome\":\"unknown\"");
        if !unknown {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }
    assert!(unknown, "the interrupted call is audited as unknown");
    assert!(crate::peers::host_tools::pending_calls_for(&peers_root, "news").is_empty());
}

// ---------------------------------------------------------------------------
// #2567 post-approval follow-ups (N1–N4)
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// #2618 — `peer/input/reject`: the host refuses a `peer/input`
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// The shared peer conversation: the person and the system agent both drive
// the host-owned peer's own session, and each turn says who is speaking.
// ---------------------------------------------------------------------------

/// Every frame a connection receives, kept for inspection.
type Frames = Arc<std::sync::Mutex<Vec<Value>>>;

fn collect_frames(mut rx: mpsc::Receiver<WsMessage>) -> Frames {
    let frames: Frames = Default::default();
    let sink = frames.clone();
    tokio::spawn(async move {
        while let Some(message) = rx.recv().await {
            if let WsMessage::Text(text) = message {
                if let Ok(frame) = serde_json::from_str::<Value>(text.as_str()) {
                    sink.lock().unwrap().push(frame);
                }
            }
        }
    });
    frames
}

/// The first frame matching `pick`, within [`TURN_ARRIVAL_BUDGET`].
async fn wait_frame(frames: &Frames, pick: impl Fn(&Value) -> bool) -> Value {
    let deadline = std::time::Instant::now() + TURN_ARRIVAL_BUDGET;
    while std::time::Instant::now() < deadline {
        if let Some(frame) = frames.lock().unwrap().iter().find(|f| pick(f)) {
            return frame.clone();
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("no matching frame in {:?}", frames.lock().unwrap());
}

/// One `turn/start` on `session` from `ws`; whether it was admitted.
#[allow(clippy::too_many_arguments)]
async fn start_turn(
    e: &E2e,
    ws: &WsConnection,
    active_turns: &SharedActiveTurns,
    request_id: &str,
    session: &SessionKey,
    turn_id: &TurnId,
    text: &str,
    origin: Option<octos_core::ui_protocol::TurnOrigin>,
) -> bool {
    start_turn_in(
        e,
        ws,
        active_turns,
        &Arc::new(UiProtocolContractStores::default()),
        request_id,
        session,
        turn_id,
        text,
        origin,
    )
    .await
}

/// [`start_turn`] with the contract stores (approvals, questions) the turn's
/// requesters park on.
#[allow(clippy::too_many_arguments)]
async fn start_turn_in(
    e: &E2e,
    ws: &WsConnection,
    active_turns: &SharedActiveTurns,
    contracts: &Arc<UiProtocolContractStores>,
    request_id: &str,
    session: &SessionKey,
    turn_id: &TurnId,
    text: &str,
    origin: Option<octos_core::ui_protocol::TurnOrigin>,
) -> bool {
    let connection_turns: SharedConnectionTurns = Arc::new(TokioMutex::new(HashMap::new()));
    handle_turn_start(
        ws,
        &e.state,
        &Arc::new(UiProtocolLedger::new(256)),
        contracts,
        active_turns,
        &connection_turns,
        None,
        None,
        ConnectionUiFeatures::stdio_defaults(),
        request_id.into(),
        TurnStartParams {
            session_id: session.clone(),
            turn_id: turn_id.clone(),
            input: vec![InputItem::Text { text: text.into() }],
            media: Vec::new(),
            topic: None,
            rewrite_for: None,
            reasoning_effort: None,
            tool_context: None,
            live_video: false,
            origin,
        },
    )
    .await
}

/// The peer's `result.md` once turn `turn_id` wrote it.
async fn wait_result(e: &E2e, turn_id: &TurnId) -> String {
    let path = e.data_dir.join("peers/news/result.md");
    let needle = format!("turn_id: {}", turn_id.0);
    let deadline = std::time::Instant::now() + TURN_ARRIVAL_BUDGET;
    while std::time::Instant::now() < deadline {
        if let Ok(body) = std::fs::read_to_string(&path) {
            if body.contains(&needle) {
                return body;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("turn {turn_id:?} never wrote the peer's result");
}

fn shared_peer(e: &E2e) -> SessionKey {
    SessionKey(format!("{}#peer-news", e.system.base_key()))
}

/// A model whose first call waits until the test releases it.
#[derive(Default)]
struct GatedLlm {
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
    calls: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl octos_llm::LlmProvider for GatedLlm {
    async fn chat(
        &self,
        _messages: &[Message],
        _tools: &[octos_llm::ToolSpec],
        _config: &octos_llm::ChatConfig,
    ) -> eyre::Result<octos_llm::ChatResponse> {
        if self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
            self.entered.notify_one();
            self.release.notified().await;
        }
        Ok(stub_reply(Some("ok"), Vec::new()))
    }

    fn model_id(&self) -> &str {
        "gated"
    }

    fn provider_name(&self) -> &str {
        "stub"
    }
}

// ---------------------------------------------------------------------------
// UPCR-2026-034, the parallel person context with shared history: the
// person's lane (a request context opened with `share_history`) runs in
// parallel with the peer's own session (the system agent's lane), and each
// lane's turns see the other's recent rows, read-only.
// ---------------------------------------------------------------------------

/// Open request context `context_id` of the News peer from `caller`, with
/// `extra` params (e.g. `share_history`).
fn open_context_from(
    e: &E2e,
    caller: Option<&WsConnection>,
    context_id: &str,
    extra: Value,
) -> Result<Value, RpcError> {
    let mut params = json!({"session_id": e.system, "peer": "news", "context_id": context_id,
                            "host_token": e.token});
    for (key, value) in extra.as_object().unwrap() {
        params[key] = value.clone();
    }
    raw_peer_context_open_from(
        &e.state,
        &rpc(APPUI_METHOD_PEER_CONTEXT_OPEN, params),
        None,
        caller,
    )
}

fn sharing_context(e: &E2e, context_id: &str) -> SessionKey {
    let opened = open_context_from(
        e,
        Some(&e.ws),
        context_id,
        json!({"share_history": {"last_n": 10}}),
    )
    .expect("the host opens a sharing context");
    assert_eq!(
        opened["share_history"],
        json!({"last_n": 10, "max_bytes": crate::peers::shared_history::SHARE_HISTORY_DEFAULT_MAX_BYTES})
    );
    serde_json::from_value(opened["session_id"].clone()).unwrap()
}

/// Every file under `root` whose bytes contain `needle`.
fn files_containing(root: &Path, needle: &str) -> Vec<PathBuf> {
    let mut hits = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if std::fs::read(&path)
                .is_ok_and(|bytes| String::from_utf8_lossy(&bytes).contains(needle))
            {
                hits.push(path);
            }
        }
    }
    hits
}

/// The system agent's `peer/input` turn id for `text`.
async fn peer_input_turn_id(e: &E2e, frames: &Frames, text: &str, occurrence: &str) -> TurnId {
    deliver_peer_send_input(
        "dev",
        &e.data_dir.join("peers"),
        &e.system.0,
        &TurnId::new(),
        send_input_request(text, occurrence),
    )
    .expect("delivered to the host");
    let input = wait_frame(frames, |f| {
        f["method"] == "peer/input" && f["params"]["text"].as_str() == Some(text)
    })
    .await;
    serde_json::from_value(input["params"]["turn_id"].clone()).unwrap()
}

/// Start a turn, retrying while the session is still finishing its last one.
#[allow(clippy::too_many_arguments)]
async fn start_turn_when_free(
    e: &E2e,
    active: &SharedActiveTurns,
    request_id: &str,
    session: &SessionKey,
    turn_id: &TurnId,
    text: &str,
    origin: Option<octos_core::ui_protocol::TurnOrigin>,
) {
    let deadline = std::time::Instant::now() + TURN_ARRIVAL_BUDGET;
    let mut attempt = 0usize;
    while std::time::Instant::now() < deadline {
        if start_turn(
            e,
            &e.ws,
            active,
            &format!("{request_id}-{attempt}"),
            session,
            turn_id,
            text,
            origin.clone(),
        )
        .await
        {
            return;
        }
        attempt += 1;
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("turn {request_id} was never admitted");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn should_show_each_lane_the_others_recent_turns_without_persisting_them() {
    let llm = Arc::new(RecordingLlm::default());
    let mut e = e2e_fixture(llm.clone(), json!({ "tools": [] })).await;
    let frames = collect_frames(e.rx.take().unwrap());
    let peer = shared_peer(&e);
    let context = sharing_context(&e, "ui-1");
    let active: SharedActiveTurns = Arc::new(TokioMutex::new(HashMap::new()));

    // The system agent's lane: a peer/input turn on the peer's own session.
    let input_turn = peer_input_turn_id(&e, &frames, "SUMMARIZE_TODAY", "call_1").await;
    start_turn_when_free(
        &e,
        &active,
        "s1",
        &peer,
        &input_turn,
        "SUMMARIZE_TODAY",
        None,
    )
    .await;
    let result = wait_result(&e, &input_turn).await;
    assert!(result.contains("\norigin: system_agent\n"), "{result}");

    // The person's lane: no origin needed, the kernel labels it the person's,
    // and the model sees the system agent's conversation read-only.
    let person_turn = TurnId::new();
    start_turn_when_free(
        &e,
        &active,
        "p1",
        &context,
        &person_turn,
        "what's new?",
        None,
    )
    .await;
    let result = wait_result(&e, &person_turn).await;
    assert!(
        result.contains("\norigin: person\ncontext: ui-1\n"),
        "{result}"
    );
    assert!(
        gathered_peer_result("news", &result).is_some(),
        "peer_gather accepts the person context's round: {result}"
    );
    let request = llm.requests.lock().unwrap()[1].clone();
    assert!(
        request.contains("<shared_history lane=\"system_agent\" read_only=\"true\">"),
        "{request}"
    );
    assert!(
        request.contains("Recent turns in the system agent's conversation with this app"),
        "{request}"
    );
    assert!(
        request.contains("[from the system agent] SUMMARIZE_TODAY"),
        "{request}"
    );
    assert!(request.contains("[the app agent] ok"), "{request}");
    assert!(
        request.contains("[from the person] what's new?"),
        "{request}"
    );

    // The reverse: the system agent's next turn sees the person's lane.
    let second_input = peer_input_turn_id(&e, &frames, "CHECK_MAIL", "call_2").await;
    start_turn_when_free(&e, &active, "s2", &peer, &second_input, "CHECK_MAIL", None).await;
    wait_result(&e, &second_input).await;
    let request = llm.requests.lock().unwrap()[2].clone();
    assert!(
        request.contains("<shared_history lane=\"person\" read_only=\"true\">"),
        "{request}"
    );
    assert!(
        request.contains("Recent turns in the person's conversation with this app"),
        "{request}"
    );
    assert!(
        request.contains("[from the person] what's new?"),
        "{request}"
    );
    // Its own lane's rows come from its own transcript, not the block.
    assert_eq!(
        request.matches("SUMMARIZE_TODAY").count(),
        1,
        "the peer's own row is not repeated in the block: {request}"
    );

    // The block rides on the prompt only: neither lane's transcript nor
    // context ledger (nor any other file) holds it.
    let root = e.data_dir.parent().unwrap().to_path_buf();
    assert_eq!(
        files_containing(&root, "<shared_history"),
        Vec::<PathBuf>::new()
    );
    // Each lane keeps its own transcript.
    let ctx_rows = files_containing(&root, "[from the person] what's new?");
    assert!(!ctx_rows.is_empty());
    assert!(
        files_containing(&root, "CHECK_MAIL")
            .iter()
            .all(|path| !ctx_rows.contains(path)
                || path.extension().is_none_or(|ext| ext != "jsonl")),
        "the system agent's rows are not in the person's transcript"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn should_run_the_person_lane_while_the_system_agent_lane_is_busy() {
    let llm = Arc::new(GatedLlm::default());
    let mut e = e2e_fixture(llm.clone(), json!({ "tools": [] })).await;
    let frames = collect_frames(e.rx.take().unwrap());
    let peer = shared_peer(&e);
    let context = sharing_context(&e, "ui-1");
    let active: SharedActiveTurns = Arc::new(TokioMutex::new(HashMap::new()));

    // The system agent's turn holds the model.
    let input_turn = peer_input_turn_id(&e, &frames, "SUMMARIZE_TODAY", "call_1").await;
    assert!(
        start_turn(
            &e,
            &e.ws,
            &active,
            "s1",
            &peer,
            &input_turn,
            "SUMMARIZE_TODAY",
            None
        )
        .await
    );
    tokio::time::timeout(TURN_ARRIVAL_BUDGET, llm.entered.notified())
        .await
        .expect("the system agent's turn reached the model");

    // The person's turn is admitted at once (no turn_in_progress) and ends
    // while the system agent's turn is still running.
    let person_turn = TurnId::new();
    assert!(
        start_turn(
            &e,
            &e.ws,
            &active,
            "p1",
            &context,
            &person_turn,
            "hello",
            None
        )
        .await,
        "the person's lane is not blocked by the system agent's"
    );
    let result = wait_result(&e, &person_turn).await;
    assert!(result.contains("\nturn: 1\n"), "{result}");
    assert!(
        active
            .lock()
            .await
            .get(&peer)
            .is_some_and(|turn| turn.turn_id == input_turn),
        "the system agent's turn is still in flight"
    );
    // And a second person turn only waits for the first person turn.
    let next_person = TurnId::new();
    start_turn_when_free(&e, &active, "p2", &context, &next_person, "more", None).await;
    wait_result(&e, &next_person).await;
    // The person's rounds alone queue no synthesis on the system agent: they
    // are recorded as covered.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert!(matches!(
        read_peer_fleet_synthesis_marks(&e.data_dir.join("peers"), &e.system.0),
        FleetSynthesisMarks::Rounds(ref rounds) if rounds.get("news") == Some(&2)
    ));
    assert_eq!(
        crate::autonomy::agent_orchestrator::default_agent_orchestrator()
            .pending_continuation_count_for_session_for_test(&e.system, "dev"),
        0,
        "no synthesis turn queued on the system agent"
    );

    llm.release.notify_one();
    let result = wait_result(&e, &input_turn).await;
    assert!(result.contains("\nturn: 3\n"), "{result}");
    assert!(result.contains("\norigin: system_agent\n"), "{result}");
    let dir = e.data_dir.join("peers/news");
    // (`turns.txt` is appended just after `result.md`.)
    let mut turns = String::new();
    let deadline = std::time::Instant::now() + TURN_ARRIVAL_BUDGET;
    while std::time::Instant::now() < deadline {
        turns = std::fs::read_to_string(dir.join("turns.txt")).unwrap();
        if turns.lines().count() >= 3 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(turns.lines().count(), 3, "{turns}");
    for (round, turn) in [(1, &person_turn), (2, &next_person), (3, &input_turn)] {
        let body = std::fs::read_to_string(dir.join(format!("result-{round}.md"))).unwrap();
        assert!(body.contains(&format!("turn_id: {}", turn.0)), "{body}");
    }
}

/// The model of the running-turn tests. Records each request's text. Its
/// turn's own message decides: "SEND_IT" says a sentence and calls
/// `mail_send` (then "done" after the tool result); "HOLD" waits until the
/// test releases it; anything else answers "ok".
#[derive(Default)]
struct LaneLlm {
    requests: std::sync::Mutex<Vec<String>>,
    held: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

impl LaneLlm {
    /// The recorded request whose text contains `needle`.
    fn request_with(&self, needle: &str) -> String {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .find(|request| request.contains(needle))
            .cloned()
            .unwrap_or_else(|| panic!("no request contains {needle:?}"))
    }
}

#[async_trait::async_trait]
impl octos_llm::LlmProvider for LaneLlm {
    async fn chat(
        &self,
        messages: &[Message],
        _tools: &[octos_llm::ToolSpec],
        _config: &octos_llm::ChatConfig,
    ) -> eyre::Result<octos_llm::ChatResponse> {
        let text = messages
            .iter()
            .map(|m| m.content.clone())
            .collect::<Vec<_>>()
            .join("\n---\n");
        self.requests.lock().unwrap().push(text);
        let own = messages
            .iter()
            .rev()
            .find(|m| m.role == MessageRole::User && m.content.starts_with("[from "))
            .map(|m| m.content.clone())
            .unwrap_or_default();
        let after_tool = messages.last().is_some_and(|m| m.role == MessageRole::Tool);
        let reply = |content: &str| stub_reply(Some(content), Vec::new());
        if own.contains("SEND_IT") && !after_tool {
            let send = stub_tool_call(
                "call_send",
                "mail_send",
                json!({"draft_id": "d-secret-args"}),
            );
            return Ok(stub_reply(Some("Sending the mail now."), vec![send]));
        }
        if own.contains("SEND_IT") {
            return Ok(reply("done"));
        }
        if own.contains("HOLD") {
            self.held.notify_one();
            self.release.notified().await;
        }
        Ok(reply("ok"))
    }

    fn model_id(&self) -> &str {
        "lane"
    }

    fn provider_name(&self) -> &str {
        "stub"
    }
}

/// Wait (bounded) until `session` has a pending approval in `contracts`.
async fn wait_for_pending_approval(
    contracts: &UiProtocolContractStores,
    session: &SessionKey,
) -> Vec<ApprovalRequestedEvent> {
    let deadline = std::time::Instant::now() + TURN_ARRIVAL_BUDGET;
    while std::time::Instant::now() < deadline {
        let pending = contracts.approvals.pending_for_session(session);
        if !pending.is_empty() {
            return pending;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("the turn never asked for an approval");
}

/// Wait (bounded) until the turn's live draft holds `needle`.
///
/// The draft is appended by the turn-loop task as it consumes the turn's
/// progress events, while the pending approval is inserted by the agent
/// task itself. On a loaded runner (merge-group run 41804229471) the
/// approval can become visible BEFORE the consumer has folded the streamed
/// text into the draft, so a test that synchronizes only on the approval
/// can read an empty draft and lose the `(in progress)` row. The draft is
/// the actual precondition of the shared-history assertion, so wait for
/// the draft directly — a bounded condition poll, never a fixed sleep.
async fn wait_for_live_draft(session: &SessionKey, turn_id: &TurnId, needle: &str) {
    let deadline = std::time::Instant::now() + TURN_ARRIVAL_BUDGET;
    while std::time::Instant::now() < deadline {
        if btw_live_draft_tail(session, turn_id).contains(needle) {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("the turn's live draft never showed {needle:?}");
}

/// The shared-history markers of a running turn, which no file may hold.
fn assert_no_block_persisted(e: &E2e) {
    let root = e.data_dir.parent().unwrap().to_path_buf();
    for needle in ["<shared_history", "[turn status]", "(in progress)"] {
        assert_eq!(
            files_containing(&root, needle),
            Vec::<PathBuf>::new(),
            "{needle} was persisted"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn should_show_the_person_lane_a_system_agent_turn_waiting_for_approval() {
    let llm = Arc::new(LaneLlm::default());
    let mut e = e2e_fixture(llm.clone(), json!({ "tools": [mail_send()] })).await;
    let frames = collect_frames(e.rx.take().unwrap());
    let peer = shared_peer(&e);
    let context = sharing_context(&e, "ui-1");
    let active: SharedActiveTurns = Arc::new(TokioMutex::new(HashMap::new()));
    let contracts = Arc::new(UiProtocolContractStores::default());

    // The system agent's turn parks on the approval of a destructive tool.
    let input_turn = peer_input_turn_id(&e, &frames, "SEND_IT", "call_1").await;
    assert!(
        start_turn_in(
            &e,
            &e.ws,
            &active,
            &contracts,
            "s1",
            &peer,
            &input_turn,
            "SEND_IT",
            None
        )
        .await
    );
    let pending = wait_for_pending_approval(&contracts, &peer).await;
    // The approval being pending does not imply the streamed text has been
    // folded into the live draft (different tasks, see `wait_for_live_draft`).
    wait_for_live_draft(&peer, &input_turn, "Sending the mail now.").await;

    // Meanwhile the person's lane sees the running turn: its request, what
    // it said so far, and what it waits on (the tool's name, no arguments).
    let person_turn = TurnId::new();
    start_turn_when_free(
        &e,
        &active,
        "p1",
        &context,
        &person_turn,
        "what is the agent doing?",
        None,
    )
    .await;
    wait_result(&e, &person_turn).await;
    let request = llm.request_with("[from the person] what is the agent doing?");
    let block = request
        .split("\n---\n")
        .find(|part| part.starts_with("<shared_history lane=\"system_agent\""))
        .unwrap_or_else(|| panic!("no block: {request}"))
        .to_owned();
    let rows: Vec<&str> = block
        .lines()
        .filter(|line| line.starts_with("- "))
        .map(|line| line.split_once("Z ").unwrap().1)
        .collect();
    assert_eq!(
        rows,
        [
            "[from the system agent] SEND_IT",
            "[the app agent] (in progress) Sending the mail now.",
            "[turn status] still running, waiting for approval: mail_send",
        ],
        "{block}"
    );
    assert!(!block.contains("d-secret-args"), "{block}");

    // Declined: the turn ends, and the next block shows it as finished rows.
    contracts
        .approvals
        .respond_with_context(ApprovalRespondParams::new(
            peer.clone(),
            pending[0].approval_id.clone(),
            ApprovalDecision::Deny,
        ))
        .expect("the person declines");
    wait_result(&e, &input_turn).await;
    let next_turn = TurnId::new();
    start_turn_when_free(&e, &active, "p2", &context, &next_turn, "and now?", None).await;
    wait_result(&e, &next_turn).await;
    let request = llm.request_with("[from the person] and now?");
    assert!(
        request.contains("[from the system agent] SEND_IT\n"),
        "{request}"
    );
    assert!(request.contains("[the app agent] done\n"), "{request}");
    assert!(!request.contains("[turn status]"), "{request}");
    assert!(!request.contains("(in progress)"), "{request}");
    assert_no_block_persisted(&e);
}

// ── #2605: the durable host-only kernel tool list of a host session ─────

// ---------------------------------------------------------------------------
// UPCR-2026-034 `peer/purge` (#2604)
// ---------------------------------------------------------------------------

/// `peer/purge` of the News peer from `ws`, on the process-wide turn registry.
async fn purge(
    state: &Arc<AppState>,
    ws: &WsConnection,
    system: &SessionKey,
    peer: &str,
    token: &str,
) -> Result<Value, RpcError> {
    let ledger = Arc::new(UiProtocolLedger::new(64));
    let contracts = Arc::new(UiProtocolContractStores::default());
    peer_purge::raw_peer_purge(
        ws,
        state,
        &ledger,
        &contracts,
        &active_turns_registry(),
        &rpc(
            APPUI_METHOD_PEER_PURGE,
            json!({"session_id": system, "peer": peer, "host_token": token}),
        ),
        None,
    )
    .await
}

/// The same for a peer whose workspace the kernel provisioned (what a peer
/// staged without `cwd` gets): attempt one erases the workspace before the
/// purge stops, so the retry must treat the gone workspace as already
/// erased instead of refusing it forever. The workspace is staged under the
/// kernel's provisioned root directly, so the branch is taken on every
/// platform (a macOS tempdir's `/var` symlink would otherwise misroute a
/// data-dir-relative path to the kept branch).
#[cfg(unix)]
#[tokio::test]
async fn should_finish_a_retry_when_an_earlier_attempt_erased_the_provisioned_workspace() {
    use std::os::unix::fs::PermissionsExt;

    let fx = fixture().await;
    let provisioned = dunce::canonicalize(&fx.data_dir)
        .unwrap()
        .join(crate::runtime::memory_namespace::APP_WORKSPACES_DIR)
        .join("app/mail/acct-1");
    std::fs::create_dir_all(&provisioned).unwrap();
    let prepared = raw_peer_prepare(
        &fx.state,
        &rpc(
            APPUI_METHOD_PEER_PREPARE,
            json!({
                "brief": "You are the Mail app's assistant.",
                "names": ["Mail"],
                "session_id": fx.system,
                "memory_namespace": "app/mail/acct-1",
                "cwd": provisioned.to_string_lossy(),
            }),
        ),
        None,
    )
    .await
    .expect("stage the mail peer");
    let token = prepared["host_token"].as_str().unwrap().to_owned();
    assert_eq!(
        prepared["cwd"].as_str().unwrap(),
        provisioned.to_string_lossy()
    );
    // The peer's memory namespace holds a file, made unremovable.
    let ns_root =
        crate::runtime::memory_namespace::memory_namespace_root(&fx.data_dir, "app/mail/acct-1");
    std::fs::create_dir_all(&ns_root).unwrap();
    std::fs::write(ns_root.join("MEMORY.md"), "- the account's fact\n").unwrap();
    std::fs::set_permissions(&ns_root, std::fs::Permissions::from_mode(0o555)).unwrap();

    let (ws, _rx) = ws_connection_for_test(8);
    let error = purge(&fx.state, &ws, &fx.system, "mail", &token)
        .await
        .unwrap_err();
    let data = error.data.unwrap();
    assert_eq!(data["kind"], "peer_purge_incomplete");
    assert!(!provisioned.exists(), "the attempt erased the workspace");

    // The retry: the gone workspace must not fail it again.
    std::fs::set_permissions(&ns_root, std::fs::Permissions::from_mode(0o755)).unwrap();
    let result = purge(&fx.state, &ws, &fx.system, "mail", &token)
        .await
        .expect("the retry finishes the purge");
    assert_eq!(result["erased"]["workspace"], "erased", "{result}");
    assert_eq!(result["errors"], json!([]), "{result}");
    assert!(!ns_root.exists());
    assert!(!peers_root(&fx).join("mail").exists());
}

// ---------------------------------------------------------------------------
// UPCR-2026-034 `read_parent`: a request context's read-only view of its
// peer's folder (#2603).
// ---------------------------------------------------------------------------
